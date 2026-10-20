//! `AppConfig` — the serde shape of `config.json` (DESIGN §10-3).
//!
//! Three rules govern this file:
//!
//! 1. **Every field carries `#[serde(default)]`.** A config written by an older
//!    build, or hand-edited down to three keys, must still load.
//! 2. **Every struct carries `#[serde(flatten)] extra`.** A config written by a
//!    *newer* build round-trips unknown keys untouched instead of silently
//!    dropping the user's settings the first time an old build saves.
//! 3. **Small string enums are lenient.** An unrecognized `theme` falls back to
//!    the default rather than failing the whole file — losing one knob beats
//!    quarantining the config (which is reserved for genuinely malformed JSON).
//!
//! The TypeScript mirror is `src/lib/ipc.ts` (`AppConfig` and friends), owned by
//! T04. Field names here must serialize to exactly the names declared there.

use std::collections::BTreeMap;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

/// Bag of keys this build does not know about, preserved across a load/save.
pub type Extra = Map<String, Value>;

/// Current `config.json` schema version.
pub const SCHEMA_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// leniency helper
// ---------------------------------------------------------------------------

/// Deserialize `T`, falling back to `T::default()` when the value does not fit.
///
/// Only used for closed string enums. JSON is self-describing, so buffering the
/// value first is always possible; the cost is one `Value` per field.
fn lenient<'de, D, T>(de: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned + Default,
{
    let value = Value::deserialize(de)?;
    Ok(serde_json::from_value(value).unwrap_or_default())
}

macro_rules! string_enum {
    (
        $(#[$meta:meta])*
        $name:ident, default = $default:ident {
            $( $variant:ident => $text:literal ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum $name {
            $( #[serde(rename = $text)] $variant ),+
        }

        impl $name {
            pub const fn as_str(self) -> &'static str {
                match self {
                    $( $name::$variant => $text ),+
                }
            }
        }

        impl Default for $name {
            fn default() -> Self {
                $name::$default
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

string_enum! {
    /// UI language. `System` (the default) means "follow the OS language": the
    /// frontend resolves it from `navigator.language` (ko* → ko, else en). An
    /// explicit `ko` / `en` saved by an older build reads back unchanged.
    Locale, default = System {
        System => "system",
        Ko => "ko",
        En => "en",
    }
}

string_enum! {
    /// Theme preference; `System` follows `prefers-color-scheme`.
    ThemePreference, default = System {
        System => "system",
        Light => "light",
        Dark => "dark",
    }
}

string_enum! {
    /// How a prompt reaches an interactive CLI (DESIGN §6-6).
    /// DECISIONS: if argv delivery fails in T09's measurement, the default
    /// becomes `TypeAfterReady` — that switch happens in the launch templates,
    /// not here.
    PromptMode, default = Argv {
        Argv => "argv",
        TypeAfterReady => "typeAfterReady",
        None => "none",
    }
}

string_enum! {
    /// Provider path alias layer (DESIGN §7-6).
    AliasProviderKey, default = ClaudeW {
        ClaudeW => "claude-w",
        CursorWorktree => "cursor-worktree",
        Generic => "generic",
        Custom => "custom",
    }
}

string_enum! {
    /// `accounts.registryMode` (DESIGN §10-5).
    RegistryMode, default = ImportOnce {
        ImportOnce => "import-once",
        Follow => "follow",
    }
}

// ---------------------------------------------------------------------------
// sections
// ---------------------------------------------------------------------------

/// A project the user has added. The authoritative row lives in `state.db`;
/// this is the list the shell restores before the database is queried.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ProjectConfig {
    pub id: String,
    pub path: String,
    pub name: String,
    pub color: String,
    pub default_base_ref: String,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Default for ProjectConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            path: String::new(),
            name: String::new(),
            color: "#7aa2f7".to_string(),
            default_base_ref: "HEAD".to_string(),
            extra: Extra::new(),
        }
    }
}

/// Per-agent launch overrides. Every field is optional: an override that only
/// pins `promptMode` must not erase the template's argv.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LaunchOverride {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_mode: Option<PromptMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_argv: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_args: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Agent kind keys as they appear in `config.json` (`AgentKindKey` in ipc.ts).
pub const AGENT_KIND_KEYS: [&str; 5] = ["claude", "codex", "cursorAgent", "agy", "gemini"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AgentsConfig {
    /// Replaces the login-shell PATH probe entirely when set.
    pub path_override: Option<String>,
    /// Environment variables copied into every spawned child (DESIGN §6-1).
    pub env_passthrough: Vec<String>,
    /// Absolute binary paths, per agent kind. `None` ⇒ detect it.
    pub binaries: BTreeMap<String, Option<String>>,
    pub launch_overrides: BTreeMap<String, LaunchOverride>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Default for AgentsConfig {
    fn default() -> Self {
        let mut binaries = BTreeMap::new();
        for kind in AGENT_KIND_KEYS {
            binaries.insert(kind.to_string(), None);
        }

        let mut launch_overrides = BTreeMap::new();
        launch_overrides.insert(
            "claude".to_string(),
            LaunchOverride {
                prompt_mode: Some(PromptMode::Argv),
                args: Some(Vec::new()),
                ..LaunchOverride::default()
            },
        );

        Self {
            path_override: None,
            env_passthrough: vec![
                "SSH_AUTH_SOCK".to_string(),
                "HTTPS_PROXY".to_string(),
                "HTTP_PROXY".to_string(),
                "NO_PROXY".to_string(),
            ],
            binaries,
            launch_overrides,
            extra: Extra::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SidecarConfig {
    pub enabled: bool,
    /// `None` ⇒ the vendored Node binary (DECISIONS: vendored is the default).
    pub node_path: Option<String>,
    pub ping_interval_seconds: u32,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Default for SidecarConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            node_path: None,
            ping_interval_seconds: 10,
            extra: Extra::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DispatchConfig {
    pub default_timeout_minutes: u32,
    /// Free-form on purpose: vendors keep adding modes (`ipc.ts` types this as
    /// a union **plus** `string`).
    pub permission_mode: String,
    pub max_concurrent_runs: u32,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Default for DispatchConfig {
    fn default() -> Self {
        Self {
            default_timeout_minutes: 45,
            permission_mode: "acceptEdits".to_string(),
            max_concurrent_runs: 4,
            extra: Extra::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WorktreeConfig {
    pub root: String,
    pub archive_root: String,
    pub branch_prefix: String,
    pub alias_providers: Vec<AliasProviderKey>,
    pub lock_while_live: bool,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Default for WorktreeConfig {
    fn default() -> Self {
        Self {
            root: "~/.gilde/worktrees".to_string(),
            archive_root: "~/.gilde/archive".to_string(),
            branch_prefix: "gilde/".to_string(),
            alias_providers: vec![AliasProviderKey::ClaudeW],
            lock_while_live: true,
            extra: Extra::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SupervisionConfig {
    pub kill_grace_ms: u64,
    pub max_headless_lifetime_minutes: u32,
    /// DECISIONS: 0 = disabled. Idle auto-kill ships off.
    pub idle_timeout_minutes: u32,
    /// DECISIONS: warn only. macOS cannot hard-cap a process's memory anyway.
    pub auto_kill_over_hard_ceiling: bool,
    pub soft_rss_mi_b: u64,
    pub hard_rss_mi_b: u64,
    pub global_warn_pct: u8,
    pub global_critical_pct: u8,
    pub scan_interval_seconds_active: u32,
    pub scan_interval_seconds_background: u32,
    pub min_age_seconds_for_bulk: u32,
    /// DECISIONS: verbatim PTY byte log ships off.
    pub raw_pty_log: bool,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Default for SupervisionConfig {
    fn default() -> Self {
        Self {
            kill_grace_ms: 5_000,
            max_headless_lifetime_minutes: 90,
            idle_timeout_minutes: 0,
            auto_kill_over_hard_ceiling: false,
            soft_rss_mi_b: 2_048,
            hard_rss_mi_b: 6_144,
            global_warn_pct: 70,
            global_critical_pct: 85,
            scan_interval_seconds_active: 3,
            scan_interval_seconds_background: 15,
            min_age_seconds_for_bulk: 120,
            raw_pty_log: false,
            extra: Extra::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StatusConfig {
    pub idle_after_ms: u64,
    pub stall_after_ms: u64,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Default for StatusConfig {
    fn default() -> Self {
        Self {
            idle_after_ms: 1_200,
            stall_after_ms: 120_000,
            extra: Extra::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TerminalConfig {
    pub font_size: f64,
    pub font_family: String,
    pub scrollback_bytes: u64,
    pub webgl: bool,
    /// `None` ⇒ the user's login shell.
    pub shell: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            font_size: 12.5,
            font_family: "SF Mono, Menlo, monospace".to_string(),
            scrollback_bytes: 2_097_152,
            webgl: true,
            shell: None,
            extra: Extra::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LayoutConfig {
    pub sidebar_width: u32,
    pub inspector_open: bool,
    pub inspector_width: u32,
    /// Opaque to Rust: the pane tree is the frontend's shape (DESIGN §9-1).
    pub panes: Map<String, Value>,
    pub tabs: Vec<Value>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            sidebar_width: 280,
            inspector_open: true,
            inspector_width: 360,
            panes: Map::new(),
            tabs: Vec::new(),
            extra: Extra::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AccountsConfig {
    #[serde(deserialize_with = "lenient")]
    pub registry_mode: RegistryMode,
    #[serde(flatten)]
    pub extra: Extra,
}

fn default_true() -> bool {
    true
}

/// What the app may tell the user outside its own window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NotificationsConfig {
    /// A health alert that needs the user is also posted as a macOS
    /// notification while the window is in the background. Default on; a
    /// config written before this field existed reads as on.
    #[serde(default = "default_true")]
    pub system: bool,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            system: default_true(),
            extra: Extra::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// root
// ---------------------------------------------------------------------------

/// `config.json` in full (DESIGN §10-3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AppConfig {
    pub schema_version: u32,
    #[serde(deserialize_with = "lenient")]
    pub locale: Locale,
    #[serde(deserialize_with = "lenient")]
    pub theme: ThemePreference,
    pub projects: Vec<ProjectConfig>,
    pub agents: AgentsConfig,
    pub sidecar: SidecarConfig,
    pub dispatch: DispatchConfig,
    pub worktree: WorktreeConfig,
    pub supervision: SupervisionConfig,
    pub status: StatusConfig,
    pub terminal: TerminalConfig,
    pub layout: LayoutConfig,
    pub accounts: AccountsConfig,
    pub notifications: NotificationsConfig,
    /// Keys written by a newer build. Never interpreted, never dropped.
    #[serde(flatten)]
    pub extra: Extra,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            locale: Locale::default(),
            theme: ThemePreference::default(),
            projects: Vec::new(),
            agents: AgentsConfig::default(),
            sidecar: SidecarConfig::default(),
            dispatch: DispatchConfig::default(),
            worktree: WorktreeConfig::default(),
            supervision: SupervisionConfig::default(),
            status: StatusConfig::default(),
            terminal: TerminalConfig::default(),
            layout: LayoutConfig::default(),
            accounts: AccountsConfig::default(),
            notifications: NotificationsConfig::default(),
            extra: Extra::new(),
        }
    }
}

impl AppConfig {
    /// The configured absolute path for one agent kind, if the user pinned one.
    pub fn agent_binary(&self, kind: &str) -> Option<&str> {
        self.agents.binaries.get(kind).and_then(|v| v.as_deref())
    }
}

// ---------------------------------------------------------------------------
// deep merge
// ---------------------------------------------------------------------------

/// Recursively merge `patch` into `base` (`app_settings_set` semantics).
///
/// - object + object ⇒ merged key by key
/// - anything else ⇒ `patch` replaces `base` (arrays are replaced wholesale;
///   a half-merged `projects` list would be nonsense)
/// - `null` ⇒ **removes** the key, so the serde default takes over on the next
///   deserialize. This is the only way a JSON patch can say "reset this knob",
///   and it keeps `Option` fields reachable: `{"agents":{"pathOverride":null}}`
///   drops the key, and `pathOverride` deserializes back to `None`.
pub fn deep_merge(base: &mut Value, patch: Value) {
    match (base, patch) {
        (Value::Object(base_map), Value::Object(patch_map)) => {
            for (key, value) in patch_map {
                match value {
                    Value::Null => {
                        base_map.remove(&key);
                    }
                    value => match base_map.get_mut(&key) {
                        Some(slot) => deep_merge(slot, value),
                        None => {
                            base_map.insert(key, value);
                        }
                    },
                }
            }
        }
        (slot, patch) => *slot = patch,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_match_design_10_3() {
        let value = serde_json::to_value(AppConfig::default()).expect("serialize");

        assert_eq!(value["schemaVersion"], 1);
        assert_eq!(value["locale"], "system");
        assert_eq!(value["theme"], "system");
        assert_eq!(value["projects"], json!([]));

        assert_eq!(value["agents"]["pathOverride"], Value::Null);
        assert_eq!(
            value["agents"]["envPassthrough"],
            json!(["SSH_AUTH_SOCK", "HTTPS_PROXY", "HTTP_PROXY", "NO_PROXY"])
        );
        assert_eq!(value["agents"]["binaries"]["claude"], Value::Null);
        assert_eq!(value["agents"]["binaries"]["cursorAgent"], Value::Null);
        assert_eq!(
            value["agents"]["launchOverrides"]["claude"],
            json!({ "promptMode": "argv", "args": [] })
        );

        assert_eq!(value["sidecar"], json!({ "enabled": true, "nodePath": null, "pingIntervalSeconds": 10 }));
        assert_eq!(
            value["dispatch"],
            json!({ "defaultTimeoutMinutes": 45, "permissionMode": "acceptEdits", "maxConcurrentRuns": 4 })
        );
        assert_eq!(
            value["worktree"],
            json!({
                "root": "~/.gilde/worktrees",
                "archiveRoot": "~/.gilde/archive",
                "branchPrefix": "gilde/",
                "aliasProviders": ["claude-w"],
                "lockWhileLive": true
            })
        );
        assert_eq!(
            value["supervision"],
            json!({
                "killGraceMs": 5000,
                "maxHeadlessLifetimeMinutes": 90,
                "idleTimeoutMinutes": 0,
                "autoKillOverHardCeiling": false,
                "softRssMiB": 2048,
                "hardRssMiB": 6144,
                "globalWarnPct": 70,
                "globalCriticalPct": 85,
                "scanIntervalSecondsActive": 3,
                "scanIntervalSecondsBackground": 15,
                "minAgeSecondsForBulk": 120,
                "rawPtyLog": false
            })
        );
        assert_eq!(value["status"], json!({ "idleAfterMs": 1200, "stallAfterMs": 120000 }));
        assert_eq!(
            value["terminal"],
            json!({
                "fontSize": 12.5,
                "fontFamily": "SF Mono, Menlo, monospace",
                "scrollbackBytes": 2097152,
                "webgl": true,
                "shell": null
            })
        );
        assert_eq!(
            value["layout"],
            json!({
                "sidebarWidth": 280,
                "inspectorOpen": true,
                "inspectorWidth": 360,
                "panes": {},
                "tabs": []
            })
        );
        assert_eq!(value["accounts"], json!({ "registryMode": "import-once" }));
        assert_eq!(value["notifications"], json!({ "system": true }));

        // Exactly the sections DESIGN §10-3 lists, plus `notifications` — no more, no less.
        assert_eq!(
            value.as_object().expect("object").len(),
            14,
            "unexpected top-level keys: {value:#?}"
        );
    }

    #[test]
    fn a_config_missing_half_its_fields_still_parses() {
        let cfg: AppConfig = serde_json::from_str(r#"{ "locale": "en", "terminal": { "fontSize": 18 } }"#)
            .expect("parse");

        assert_eq!(cfg.locale, Locale::En);
        assert_eq!(cfg.terminal.font_size, 18.0);
        // Everything else fell back to the documented default.
        assert_eq!(cfg.terminal.font_family, "SF Mono, Menlo, monospace");
        assert_eq!(cfg.dispatch.default_timeout_minutes, 45);
        assert_eq!(cfg.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn an_empty_object_parses_to_the_defaults() {
        let cfg: AppConfig = serde_json::from_str("{}").expect("parse");
        assert_eq!(cfg, AppConfig::default());
    }

    #[test]
    fn unknown_keys_round_trip_at_every_level() {
        let raw = json!({
            "locale": "en",
            "futureSection": { "deep": [1, 2, 3] },
            "terminal": { "fontSize": 11.0, "futureKnob": true },
            "supervision": { "futureCeiling": 9 }
        });

        let cfg: AppConfig = serde_json::from_value(raw).expect("parse");
        let back = serde_json::to_value(&cfg).expect("serialize");

        assert_eq!(back["futureSection"], json!({ "deep": [1, 2, 3] }));
        assert_eq!(back["terminal"]["futureKnob"], json!(true));
        assert_eq!(back["supervision"]["futureCeiling"], json!(9));
        assert_eq!(back["locale"], "en");
    }

    #[test]
    fn the_default_locale_is_system_and_serializes_as_system() {
        assert_eq!(Locale::default(), Locale::System);
        assert_eq!(AppConfig::default().locale, Locale::System);
        let value = serde_json::to_value(AppConfig::default()).expect("serialize");
        assert_eq!(value["locale"], "system");
        let back: AppConfig = serde_json::from_value(value).expect("parse");
        assert_eq!(back.locale, Locale::System);
    }

    #[test]
    fn an_old_config_with_an_explicit_locale_keeps_it() {
        for (text, want) in [("ko", Locale::Ko), ("en", Locale::En)] {
            let raw = format!(r#"{{ "schemaVersion": 1, "locale": "{text}", "theme": "dark" }}"#);
            let cfg: AppConfig = serde_json::from_str(&raw).expect("parse");
            assert_eq!(cfg.locale, want, "{text} reads back as itself");
            let value = serde_json::to_value(&cfg).expect("serialize");
            assert_eq!(value["locale"], text, "{text} round-trips");
            let again: AppConfig = serde_json::from_value(value).expect("parse");
            assert_eq!(again.locale, want);
        }
    }

    #[test]
    fn a_config_without_a_locale_key_follows_the_system() {
        let cfg: AppConfig =
            serde_json::from_str(r#"{ "schemaVersion": 1, "theme": "light" }"#).expect("parse");
        assert_eq!(cfg.locale, Locale::System);
        let cfg: AppConfig = serde_json::from_str(r#"{ "locale": "system" }"#).expect("parse");
        assert_eq!(cfg.locale, Locale::System);
    }

    #[test]
    fn an_unknown_enum_value_falls_back_instead_of_failing_the_file() {
        let cfg: AppConfig =
            serde_json::from_str(r#"{ "theme": "solarized", "locale": "fr", "accounts": { "registryMode": "gossip" } }"#)
                .expect("parse");

        assert_eq!(cfg.theme, ThemePreference::System);
        assert_eq!(cfg.locale, Locale::System);
        assert_eq!(cfg.accounts.registry_mode, RegistryMode::ImportOnce);
    }

    #[test]
    fn deep_merge_merges_objects_and_replaces_scalars_and_arrays() {
        let mut base = json!({
            "a": { "b": 1, "c": { "d": 2 } },
            "list": [1, 2, 3],
            "scalar": "old"
        });

        deep_merge(
            &mut base,
            json!({ "a": { "c": { "e": 3 } }, "list": [9], "scalar": "new", "fresh": true }),
        );

        assert_eq!(
            base,
            json!({
                "a": { "b": 1, "c": { "d": 2, "e": 3 } },
                "list": [9],
                "scalar": "new",
                "fresh": true
            })
        );
    }

    #[test]
    fn deep_merge_null_removes_the_key() {
        let mut base = json!({ "keep": 1, "drop": { "nested": true } });
        deep_merge(&mut base, json!({ "drop": null }));
        assert_eq!(base, json!({ "keep": 1 }));
    }

    #[test]
    fn an_old_config_without_notifications_reads_system_notifications_as_on() {
        // A config.json written before the field existed: no section at all.
        let cfg: AppConfig =
            serde_json::from_str(r#"{ "schemaVersion": 1, "locale": "ko", "theme": "dark" }"#).expect("parse");
        assert!(cfg.notifications.system, "no section → on");
        // A section without the field (e.g. only a key from a newer build).
        let cfg: AppConfig =
            serde_json::from_str(r#"{ "notifications": { "futureKnob": 1 } }"#).expect("parse");
        assert!(cfg.notifications.system, "no field → on");
        assert_eq!(cfg.notifications.extra.get("futureKnob"), Some(&json!(1)), "unknown keys kept");
        // An explicit choice is kept and round-trips.
        let cfg: AppConfig = serde_json::from_str(r#"{ "notifications": { "system": false } }"#).expect("parse");
        assert!(!cfg.notifications.system, "an explicit off stays off");
        let value = serde_json::to_value(&cfg).expect("serialize");
        assert_eq!(value["notifications"], json!({ "system": false }));
        // `{"notifications":{"system":null}}` (the reset patch) removes the key → back to on.
        let mut raw = serde_json::to_value(&cfg).expect("serialize");
        deep_merge(&mut raw, json!({ "notifications": { "system": null } }));
        let back: AppConfig = serde_json::from_value(raw).expect("parse");
        assert!(back.notifications.system, "reset → the default, on");
    }

    #[test]
    fn agent_binary_reads_the_pinned_path() {
        let mut cfg = AppConfig::default();
        assert_eq!(cfg.agent_binary("claude"), None);
        cfg.agents
            .binaries
            .insert("claude".to_string(), Some("/opt/claude".to_string()));
        assert_eq!(cfg.agent_binary("claude"), Some("/opt/claude"));
        assert_eq!(cfg.agent_binary("nope"), None);
    }
}
