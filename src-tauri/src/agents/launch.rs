//! Launch templates — DESIGN §6-6, M1-PLAN T09.6–T09.8.
//!
//! One template per agent the app can start in the **interactive** (PTY) lane.
//! The headless lane never comes through here; that argv knowledge lives inside
//! the agent-mesh adapters and is not ours to duplicate (DESIGN §0 rule 1).
//!
//! ## What a template is for
//!
//! `pty::manager` needs four things to open a session: an absolute binary, the
//! interactive argv, which environment variable carries the account directory,
//! and how a first prompt reaches the CLI. That is the whole struct.
//!
//! ## `Argv` is the default because it has no injection surface
//!
//! `CommandBuilder` takes an argv **array** and there is no shell in the path,
//! so a prompt beginning with `--model` cannot hijack parsing. `TypeAfterReady`
//! is strictly worse — it depends on recognising a ready prompt, and the text it
//! types is indistinguishable from what the user types.
//!
//! ## Measured on this machine, 2026-09-07 (M1-PLAN T09.7)
//!
//! DESIGN shipped `claude -- "<prompt>"` and `codex -- "<prompt>"` as
//! `verified: false`, because the `--` fence had only been confirmed for the
//! *headless* argv the adapters build. Both were then exercised for real, in a
//! throw-away directory, over a real pty:
//!
//! * `claude 2.1.263` — the TUI came up, the prompt appeared as the first turn
//!   (`❯ AOPROBE …`) and the model answered. **Verified.**
//! * `codex-cli 0.145.0` — same, `› AOPROBE …` followed by the answer.
//!   **Verified.**
//!
//! So both are promoted to `verified: true` here, and DECISIONS §M1 세부 기본값
//! item 4 (fall back to `TypeAfterReady` if argv delivery fails) does **not**
//! trigger. `agy`, `cursor-agent` and `gemini` stay `TypeAfterReady`; `agy`
//! deliberately carries **no `-p`** (M1-PLAN T09.6) — `-p` is its headless
//! one-shot flag and putting it in a TUI defeats the interactive lane.

use serde::{Deserialize, Serialize};

use crate::agents::{AgentBinary, AgentKind};
use crate::config::model::{AppConfig, LaunchOverride};

/// How a prompt reaches an interactive CLI (DESIGN §6-6).
///
/// Re-exported from `config::model` rather than redefined: the launch template
/// and `config.agents.launchOverrides.<agent>.promptMode` must be the same
/// closed set, or an override could name a mode the template cannot express.
pub use crate::config::model::PromptMode;

/// Placeholder substituted in [`LaunchTemplate::prompt_argv`].
pub const PROMPT_PLACEHOLDER: &str = "{PROMPT}";

/// Placeholder substituted in [`LaunchTemplate::resume_args`] (M2 slot).
pub const CONVERSATION_PLACEHOLDER: &str = "{ID}";

/// Everything `pty::manager` needs to start one agent (DESIGN §6-6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchTemplate {
    pub agent: AgentKind,
    /// Absolute path resolved by [`crate::agents::detect`]. Never a bare name —
    /// DESIGN §6-1 and identity fence 2 both depend on that.
    pub binary: String,
    /// Interactive TUI argv. Never `-p`.
    pub args: Vec<String>,
    pub account_env_key: Option<String>,
    pub prompt_mode: PromptMode,
    /// Template carrying [`PROMPT_PLACEHOLDER`].
    pub prompt_argv: Option<Vec<String>>,
    /// M2 slot, populated now so no migration is needed later.
    pub resume_args: Option<Vec<String>>,
    /// `false` ⇒ the UI marks this launch path "unverified on this machine".
    pub verified: bool,
}

impl LaunchTemplate {
    /// Full argv for an interactive session with no initial prompt.
    pub fn interactive_argv(&self) -> Vec<String> {
        let mut argv = vec![self.binary.clone()];
        argv.extend(self.args.iter().cloned());
        argv
    }

    /// Full argv for an interactive session that starts with `prompt`.
    ///
    /// `None` when this agent cannot take a prompt on argv — the caller then
    /// opens the session bare and types the prompt after the ready gate
    /// (DESIGN §6-5 `session_send_prompt`).
    pub fn argv_with_prompt(&self, prompt: &str) -> Option<Vec<String>> {
        if self.prompt_mode != PromptMode::Argv {
            return None;
        }
        let template = self.prompt_argv.as_ref()?;
        let mut argv = self.interactive_argv();
        argv.extend(
            template
                .iter()
                .map(|part| part.replace(PROMPT_PLACEHOLDER, prompt)),
        );
        Some(argv)
    }

    /// M2 slot: full argv to resume `conversation_id`.
    pub fn argv_for_resume(&self, conversation_id: &str) -> Option<Vec<String>> {
        let template = self.resume_args.as_ref()?;
        let mut argv = self.interactive_argv();
        argv.extend(
            template
                .iter()
                .map(|part| part.replace(CONVERSATION_PLACEHOLDER, conversation_id)),
        );
        Some(argv)
    }
}

// ---------------------------------------------------------------------------
// the table
// ---------------------------------------------------------------------------

/// The immutable half of a template — everything that does not depend on where
/// the binary happens to live.
struct TemplateSpec {
    args: &'static [&'static str],
    prompt_mode: PromptMode,
    prompt_argv: Option<&'static [&'static str]>,
    resume_args: Option<&'static [&'static str]>,
    verified: bool,
}

/// DESIGN §6-6, with the two promotions measured in T09 (see the module docs).
fn spec_for(kind: AgentKind) -> TemplateSpec {
    match kind {
        AgentKind::Claude => TemplateSpec {
            args: &[],
            prompt_mode: PromptMode::Argv,
            prompt_argv: Some(&["--", PROMPT_PLACEHOLDER]),
            resume_args: Some(&["--resume", CONVERSATION_PLACEHOLDER]),
            // Measured 2026-09-07 against claude 2.1.263 over a real pty.
            verified: true,
        },
        AgentKind::Codex => TemplateSpec {
            args: &[],
            prompt_mode: PromptMode::Argv,
            prompt_argv: Some(&["--", PROMPT_PLACEHOLDER]),
            resume_args: Some(&["resume", CONVERSATION_PLACEHOLDER]),
            // Measured 2026-09-07 against codex-cli 0.145.0 over a real pty.
            verified: true,
        },
        AgentKind::Antigravity => TemplateSpec {
            // No `-p`: that is agy's headless one-shot flag (M1-PLAN T09.6).
            args: &[],
            prompt_mode: PromptMode::TypeAfterReady,
            prompt_argv: None,
            resume_args: Some(&["--conversation", CONVERSATION_PLACEHOLDER]),
            // `AGY_HOME` is an upstream guess and the ready gate is unmeasured.
            verified: false,
        },
        AgentKind::Cursor => TemplateSpec {
            args: &[],
            prompt_mode: PromptMode::TypeAfterReady,
            prompt_argv: None,
            resume_args: Some(&["--resume", CONVERSATION_PLACEHOLDER]),
            // Not installed on this machine — nothing about it is measured.
            verified: false,
        },
        AgentKind::GeminiCli => TemplateSpec {
            args: &[],
            prompt_mode: PromptMode::TypeAfterReady,
            prompt_argv: None,
            resume_args: None,
            verified: false,
        },
        AgentKind::Shell => TemplateSpec {
            args: &["-l"],
            prompt_mode: PromptMode::None,
            prompt_argv: None,
            resume_args: None,
            verified: true,
        },
    }
}

/// Build one template per **installed** agent, applying
/// `config.agents.launchOverrides.<agent>` on top (M1-PLAN T09.8).
///
/// Agents with no resolved binary are omitted rather than emitted with an empty
/// `binary`: DESIGN §6-1 requires an absolute path, and a template that cannot
/// honour that invariant is not a launchable thing. The UI learns what is
/// missing from `agents_detect`, which reports every kind including the
/// uninstalled ones.
pub fn templates(cfg: &AppConfig, bins: &[AgentBinary]) -> Vec<LaunchTemplate> {
    bins.iter()
        .filter_map(|bin| template_for(cfg, bin))
        .collect()
}

/// One template, or `None` when there is no binary to launch.
pub fn template_for(cfg: &AppConfig, bin: &AgentBinary) -> Option<LaunchTemplate> {
    let kind = bin.kind;
    let spec = spec_for(kind);
    let override_ = cfg.agents.launch_overrides.get(kind.key());

    let binary = override_
        .and_then(|o| o.binary.clone())
        .filter(|b| !b.trim().is_empty())
        .or_else(|| bin.abs_path.as_ref().map(|p| p.to_string_lossy().into_owned()))?;

    let mut template = LaunchTemplate {
        agent: kind,
        binary,
        args: spec.args.iter().map(|s| (*s).to_string()).collect(),
        account_env_key: kind.account_env_key().map(str::to_string),
        prompt_mode: spec.prompt_mode,
        prompt_argv: spec
            .prompt_argv
            .map(|v| v.iter().map(|s| (*s).to_string()).collect()),
        resume_args: spec
            .resume_args
            .map(|v| v.iter().map(|s| (*s).to_string()).collect()),
        verified: spec.verified,
    };

    if let Some(over) = override_ {
        apply_override(&mut template, over);
    }
    Some(template)
}

/// Overlay one `launchOverrides` entry.
///
/// Anything the user re-specifies about *prompt delivery* clears `verified`:
/// our measurement was of the shipped argv, and silently keeping the badge on a
/// path nobody has run would be exactly the dishonesty DESIGN §0 rule 7 exists
/// to prevent. Overriding only `args` or `binary` leaves it alone.
fn apply_override(template: &mut LaunchTemplate, over: &LaunchOverride) {
    if let Some(args) = &over.args {
        template.args = args.clone();
    }
    if let Some(mode) = over.prompt_mode {
        if mode != template.prompt_mode {
            template.prompt_mode = mode;
            template.verified = false;
        }
    }
    if let Some(prompt_argv) = &over.prompt_argv {
        let replacement = if prompt_argv.is_empty() {
            None
        } else {
            Some(prompt_argv.clone())
        };
        if replacement != template.prompt_argv {
            template.prompt_argv = replacement;
            template.verified = false;
        }
    }
    if let Some(resume_args) = &over.resume_args {
        template.resume_args = if resume_args.is_empty() {
            None
        } else {
            Some(resume_args.clone())
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{AgentBinary, BinarySource};
    use std::path::PathBuf;

    fn installed(kind: AgentKind, path: &str) -> AgentBinary {
        AgentBinary {
            kind,
            abs_path: Some(PathBuf::from(path)),
            version: Some("1.0.0".into()),
            installed: true,
            ok: true,
            verified: false,
            verified_range: None,
            warning: None,
            source: BinarySource::Detected,
            detected_at: Some(0),
        }
    }

    fn missing(kind: AgentKind) -> AgentBinary {
        AgentBinary {
            kind,
            abs_path: None,
            version: None,
            installed: false,
            ok: false,
            verified: false,
            verified_range: None,
            warning: None,
            source: BinarySource::NotFound,
            detected_at: Some(0),
        }
    }

    fn all_installed() -> Vec<AgentBinary> {
        vec![
            installed(AgentKind::Claude, "/nvm/bin/claude"),
            installed(AgentKind::Codex, "/nvm/bin/codex"),
            installed(AgentKind::Cursor, "/usr/local/bin/cursor-agent"),
            installed(AgentKind::Antigravity, "/home/.local/bin/agy"),
            installed(AgentKind::GeminiCli, "/nvm/bin/gemini"),
            installed(AgentKind::Shell, "/bin/zsh"),
        ]
    }

    fn template(kind: AgentKind, cfg: &AppConfig) -> LaunchTemplate {
        templates(cfg, &all_installed())
            .into_iter()
            .find(|t| t.agent == kind)
            .unwrap_or_else(|| panic!("no template for {kind:?}"))
    }

    #[test]
    fn the_design_table_is_implemented_verbatim() {
        let cfg = AppConfig::default();

        let claude = template(AgentKind::Claude, &cfg);
        assert_eq!(claude.binary, "/nvm/bin/claude");
        assert!(claude.args.is_empty());
        assert_eq!(claude.account_env_key.as_deref(), Some("CLAUDE_CONFIG_DIR"));
        assert_eq!(claude.prompt_mode, PromptMode::Argv);
        assert_eq!(
            claude.prompt_argv,
            Some(vec!["--".to_string(), "{PROMPT}".to_string()])
        );

        let codex = template(AgentKind::Codex, &cfg);
        assert_eq!(codex.account_env_key.as_deref(), Some("CODEX_HOME"));
        assert_eq!(codex.resume_args, Some(vec!["resume".into(), "{ID}".into()]));

        let agy = template(AgentKind::Antigravity, &cfg);
        assert_eq!(agy.account_env_key.as_deref(), Some("AGY_HOME"));
        assert_eq!(agy.prompt_mode, PromptMode::TypeAfterReady);

        let cursor = template(AgentKind::Cursor, &cfg);
        assert_eq!(
            cursor.account_env_key, None,
            "cursor-agent has no config-dir env — ambient login only"
        );

        let shell = template(AgentKind::Shell, &cfg);
        assert_eq!(shell.args, vec!["-l".to_string()]);
        assert_eq!(shell.prompt_mode, PromptMode::None);
    }

    /// M1-PLAN T09.6, called out as 필수.
    #[test]
    fn agy_never_carries_dash_p_in_the_interactive_template() {
        let cfg = AppConfig::default();
        let agy = template(AgentKind::Antigravity, &cfg);

        assert!(!agy.args.iter().any(|a| a == "-p" || a == "--print"));
        assert_eq!(agy.prompt_mode, PromptMode::TypeAfterReady);
        assert!(agy.prompt_argv.is_none());
        assert!(
            agy.argv_with_prompt("hello").is_none(),
            "a TypeAfterReady agent must not produce a prompt argv"
        );
    }

    /// M1-PLAN T09.7. Both promotions are backed by a real pty run; the three
    /// unmeasured CLIs stay honest about it.
    #[test]
    fn measured_agents_are_verified_and_unmeasured_ones_are_not() {
        let cfg = AppConfig::default();
        assert!(template(AgentKind::Claude, &cfg).verified);
        assert!(template(AgentKind::Codex, &cfg).verified);
        assert!(!template(AgentKind::Antigravity, &cfg).verified);
        assert!(!template(AgentKind::Cursor, &cfg).verified);
        assert!(!template(AgentKind::GeminiCli, &cfg).verified);
    }

    #[test]
    fn prompt_argv_renders_the_placeholder_without_a_shell_in_the_way() {
        let cfg = AppConfig::default();
        let claude = template(AgentKind::Claude, &cfg);

        assert_eq!(
            claude.argv_with_prompt("--model opus; rm -rf /"),
            Some(vec![
                "/nvm/bin/claude".to_string(),
                "--".to_string(),
                "--model opus; rm -rf /".to_string(),
            ]),
            "the prompt stays one argv element, fenced by --"
        );
        assert_eq!(
            claude.argv_for_resume("abc-123"),
            Some(vec![
                "/nvm/bin/claude".to_string(),
                "--resume".to_string(),
                "abc-123".to_string(),
            ])
        );
    }

    #[test]
    fn an_uninstalled_agent_gets_no_template() {
        let cfg = AppConfig::default();
        let bins = vec![
            installed(AgentKind::Claude, "/nvm/bin/claude"),
            missing(AgentKind::Cursor),
            missing(AgentKind::GeminiCli),
        ];
        let got = templates(&cfg, &bins);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].agent, AgentKind::Claude);
    }

    /// M1-PLAN T09.8 — every field is overridable.
    #[test]
    fn every_template_field_is_overridable_from_config() {
        let mut cfg = AppConfig::default();
        cfg.agents.launch_overrides.insert(
            "claude".into(),
            LaunchOverride {
                prompt_mode: Some(PromptMode::TypeAfterReady),
                args: Some(vec!["--dangerously-skip-permissions".into()]),
                prompt_argv: Some(vec!["--prompt".into(), "{PROMPT}".into()]),
                resume_args: Some(vec!["-c".into()]),
                binary: Some("/opt/pinned/claude".into()),
                ..LaunchOverride::default()
            },
        );

        let claude = template(AgentKind::Claude, &cfg);
        assert_eq!(claude.binary, "/opt/pinned/claude");
        assert_eq!(claude.args, vec!["--dangerously-skip-permissions".to_string()]);
        assert_eq!(claude.prompt_mode, PromptMode::TypeAfterReady);
        assert_eq!(
            claude.prompt_argv,
            Some(vec!["--prompt".into(), "{PROMPT}".into()])
        );
        assert_eq!(claude.resume_args, Some(vec!["-c".to_string()]));
        assert!(
            !claude.verified,
            "an overridden prompt path is no longer the one we measured"
        );
    }

    #[test]
    fn the_default_config_override_is_a_no_op_and_keeps_the_measured_badge() {
        // `AgentsConfig::default()` ships `{"claude": {"promptMode": "argv",
        // "args": []}}` (DESIGN §10-3). Restating the shipped values must not
        // cost the verified badge.
        let cfg = AppConfig::default();
        let claude = template(AgentKind::Claude, &cfg);
        assert_eq!(claude.prompt_mode, PromptMode::Argv);
        assert!(claude.verified);
    }

    #[test]
    fn an_empty_prompt_argv_override_means_no_argv_delivery() {
        let mut cfg = AppConfig::default();
        cfg.agents.launch_overrides.insert(
            "codex".into(),
            LaunchOverride {
                prompt_argv: Some(Vec::new()),
                ..LaunchOverride::default()
            },
        );
        let codex = template(AgentKind::Codex, &cfg);
        assert_eq!(codex.prompt_argv, None);
        assert!(codex.argv_with_prompt("x").is_none());
        assert!(!codex.verified);
    }

    #[test]
    fn the_wire_shape_matches_ipc_ts() {
        let cfg = AppConfig::default();
        let value = serde_json::to_value(template(AgentKind::Cursor, &cfg)).expect("json");
        assert_eq!(value["agent"], serde_json::json!("cursorAgent"));
        assert_eq!(value["accountEnvKey"], serde_json::Value::Null);
        assert_eq!(value["promptMode"], serde_json::json!("typeAfterReady"));
        assert_eq!(value["promptArgv"], serde_json::Value::Null);
        assert_eq!(value["verified"], serde_json::json!(false));
    }
}
