//! §4.2.8 History, attention, notifications, settings.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use super::app::Theme;
use super::changes::EditorKind;
use super::empty_structs;
use super::sessions::uid_req;
use crate::contract::common::{epoch_ms, AbsPath, EpochMs, Vendor};
use crate::contract::ids::{AccountId, HistoryCursor, ProjectId, QueryId, SessionUid};
use crate::contract::session::{ContinueOption, Origin};
use crate::contract::validate::{self as v, Checked, Invalid, Validate};

empty_structs!(req:
    /// `notify_resume` request.
    NotifyResumeReq,
    /// `notify_permission_request`: Rust asks macOS; first use only.
    NotifyPermissionRequestReq,
    /// `notify_test`: Settings only; class `probe`.
    NotifyTestReq,
    /// `settings_get` request.
    SettingsGetReq,
);

empty_structs!(res: NotifyResumeRes);

uid_req!(
    /// `history_preview`: **[content]** read on demand, memory only;
    /// `UNSUPPORTED{titleOnly}` for cursor/agy.
    HistoryPreviewReq,
    /// `history_row` (A29.2): the one `HistoryRow` for a uid (the Session
    /// screen's read-only view of a session World does not hold);
    /// `NOT_FOUND{session}` when the history index does not know the uid.
    HistoryRowReq,
    /// `queue_later`: 30 min (uptime clock); `NOT_FOUND{queueItem}`.
    QueueLaterReq,
);

// -- history_query ---------------------------------------------------------------

/// `text` 0..=256 chars, NFC-normalized, matched with the Korean-safe matcher;
/// `limit` 1..=100; a newer `queryId` supersedes older ones.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct HistoryQueryReq {
    pub query_id: QueryId,
    pub text: String,
    pub filters: HistoryFilters,
    pub cursor: Option<HistoryCursor>,
    pub limit: u16,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct HistoryFilters {
    pub project_id: Option<ProjectId>,
    pub vendor: Option<Vendor>,
    #[ts(type = "number | null")]
    pub from: Option<EpochMs>,
    #[ts(type = "number | null")]
    pub to: Option<EpochMs>,
    pub account_id: Option<AccountId>,
    pub origin: Option<Origin>,
}

impl Validate for HistoryFilters {
    fn validate(&self) -> Checked {
        v::nested("projectId", &self.project_id)?;
        v::nested("accountId", &self.account_id)?;
        if let Some(f) = self.from {
            epoch_ms("from", f)?;
        }
        if let Some(t) = self.to {
            epoch_ms("to", t)?;
        }
        if let (Some(f), Some(t)) = (self.from, self.to) {
            if f > t {
                return Err(Invalid::new("from", "range"));
            }
        }
        Ok(())
    }
}

impl Validate for HistoryQueryReq {
    fn validate(&self) -> Checked {
        v::nested("queryId", &self.query_id)?;
        v::chars("text", &self.text, 0, 256)?;
        v::nested("filters", &self.filters)?;
        v::nested("cursor", &self.cursor)?;
        v::range("limit", self.limit, 1, 100)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct HistoryQueryRes {
    pub query_id: QueryId,
    pub rows: Vec<HistoryRow>,
    pub next_cursor: Option<HistoryCursor>,
    pub coverage: HistoryCoverage,
    pub superseded: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct HistoryCoverage {
    pub titles_loaded: u32,
    pub total: u32,
}

/// Every row carries a uid (minted and persisted in `session_meta` on first
/// appearance), so Read = `convo_subscribe{uid}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct HistoryRow {
    pub uid: SessionUid,
    pub vendor: Vendor,
    pub project_id: Option<ProjectId>,
    pub account_id: Option<AccountId>,
    pub origin: Origin,
    #[ts(type = "number | null")]
    pub started_at: Option<EpochMs>,
    #[ts(type = "number | null")]
    pub ended_at: Option<EpochMs>,
    pub running: bool,
    pub title_only: bool,
    pub label: HistoryLabel,
    /// **[content]**
    pub title: Option<String>,
    pub continue_options: Vec<ContinueOption>,
    pub continued_in: Option<SessionUid>,
}

/// `vendorDate` = "{vendor} 대화 · {date}" (cursor rows have no title [C6]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum HistoryLabel {
    Title,
    VendorDate,
}

/// **[content]** (the History preview pane).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct HistoryPreviewRes {
    /// ≤ 400 chars
    pub first_prompt: Option<String>,
    /// ≤ 400
    pub last_reply: Option<String>,
    pub turns: Option<u32>,
    #[ts(type = "number | null")]
    pub started_at: Option<EpochMs>,
    #[ts(type = "number | null")]
    pub ended_at: Option<EpochMs>,
    pub continued_in: Option<SessionUid>,
}

// -- attention ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct QueueLaterRes {
    #[ts(type = "number")]
    pub until: EpochMs,
}

// -- notifications -----------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct NotifyPauseReq {
    pub until: PauseUntil,
}

impl Validate for NotifyPauseReq {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum PauseUntil {
    OneHour,
    TomorrowNine,
    UntilResume,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct NotifyPauseRes {
    #[ts(type = "number | null")]
    pub paused_until: Option<EpochMs>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct NotifyPermissionRequestRes {
    pub granted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct NotifyTestRes {
    pub delivered: bool,
}

// -- settings (§9.6) -----------------------------------------------------------------

/// The settings the WebView sees. **Ordinary** keys change with `settings_set`;
/// **executable** keys are read-only here and change only with
/// `settings_set_executable` (a Rust-owned native confirmation).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct Settings {
    pub locale: LocaleSetting,
    pub theme: Theme,
    pub attention: AttentionSettings,
    pub notify: NotifySettings,
    pub ide: IdeSettings,
    pub executable: ExecutableSettings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum LocaleSetting {
    System,
    Ko,
    En,
}

impl Validate for LocaleSetting {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct AttentionSettings {
    /// Default 300, range 60..=3600 (§3.5.2).
    pub no_progress_after_secs: u32,
    /// "Finished turns from sessions started elsewhere" (band 5 for Watching).
    pub watched_finished_turns: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct NotifySettings {
    /// "Notify about sessions started elsewhere" (§8.4 rule 3).
    pub watched_sessions: bool,
    /// Off by default; one system sound for band 1 only (§8.3).
    pub sound: bool,
    /// "알림에 내용 표시" / "Show details in notifications", default off [F-31].
    pub show_details: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct IdeSettings {
    /// `ide.editor ∈ {vscode, cursor}`; `None` = not chosen yet.
    pub editor: Option<EditorKind>,
}

/// Executable-class values (read-only here; §9.6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct ExecutableSettings {
    pub claude_binary: Option<AbsPath>,
    pub codex_binary: Option<AbsPath>,
    pub cursor_binary: Option<AbsPath>,
    pub agy_binary: Option<AbsPath>,
    pub sidecar_node_path: Option<AbsPath>,
    pub paths_override: Option<AbsPath>,
    pub env_passthrough: Vec<String>,
}

/// Typed patch of ordinary keys only; unknown key = `INVALID_ARGUMENT`
/// (`shape`); whole-patch validation. Absent = unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct SettingsPatch {
    pub locale: Option<LocaleSetting>,
    pub theme: Option<Theme>,
    pub attention: Option<AttentionSettingsPatch>,
    pub notify: Option<NotifySettingsPatch>,
    pub ide: Option<IdeSettingsPatch>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct AttentionSettingsPatch {
    /// 60..=3600
    pub no_progress_after_secs: Option<u32>,
    pub watched_finished_turns: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct NotifySettingsPatch {
    pub watched_sessions: Option<bool>,
    pub sound: Option<bool>,
    pub show_details: Option<bool>,
}

/// `editor: null` leaves it unchanged (absent = not requested, §3.1 rule 4).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct IdeSettingsPatch {
    pub editor: Option<EditorKind>,
}

impl Validate for SettingsPatch {
    fn validate(&self) -> Checked {
        if let Some(a) = &self.attention {
            if let Some(s) = a.no_progress_after_secs {
                v::range("attention.noProgressAfterSecs", s, 60, 3600)?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct SettingsSetReq {
    pub patch: SettingsPatch,
}

impl Validate for SettingsSetReq {
    fn validate(&self) -> Checked {
        v::nested("patch", &self.patch)
    }
}

/// The executable-class keys (§9.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum ExecutableKey {
    #[serde(rename = "agents.claude.binary")]
    ClaudeBinary,
    #[serde(rename = "agents.codex.binary")]
    CodexBinary,
    #[serde(rename = "agents.cursor.binary")]
    CursorBinary,
    #[serde(rename = "agents.agy.binary")]
    AgyBinary,
    #[serde(rename = "sidecar.nodePath")]
    SidecarNodePath,
    #[serde(rename = "paths.override")]
    PathsOverride,
    #[serde(rename = "agents.envPassthrough")]
    EnvPassthrough,
}

/// `Path(AbsPath | null) | EnvNames(string[])`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum ExecutableValue {
    Path { path: Option<AbsPath> },
    EnvNames { names: Vec<String> },
}

/// Rust shows a native confirmation naming the exact value; a path must be a
/// regular, executable Mach-O or `#!` script owned by the user or root, not in
/// a world-writable directory and not inside a vendor dir (checked at
/// execution); `applied:false` if the user cancels.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct SettingsSetExecutableReq {
    pub key: ExecutableKey,
    pub value: ExecutableValue,
}

/// `^[A-Z_][A-Z0-9_]{0,63}$`.
pub fn is_env_name(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && (b[0].is_ascii_uppercase() || b[0] == b'_')
        && b.iter().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == b'_')
}

/// Names `agents.envPassthrough` may never contain (§6.0) [R3 §7c, R5 §7].
pub fn is_reserved_env_name(s: &str) -> bool {
    const EXACT: [&str; 7] = [
        "TERM_PROGRAM",
        "CLAUDECODE",
        "CLAUDE_CODE_TEAMMATE_COMMAND",
        "CLAUDE_CODE_PROCESS_WRAPPER",
        "CLAUDE_CODE_ENTRYPOINT",
        "CLAUDE_CONFIG_DIR",
        "CODEX_HOME",
    ];
    const PREFIX: [&str; 5] = ["TMUX", "ITERM_", "LC_TERMINAL", "GILDE_", "CLAUDE_CODE_MESSAGING_"];
    EXACT.contains(&s) || PREFIX.iter().any(|p| s.starts_with(p))
}

impl Validate for SettingsSetExecutableReq {
    fn validate(&self) -> Checked {
        match (&self.key, &self.value) {
            (ExecutableKey::EnvPassthrough, ExecutableValue::EnvNames { names }) => {
                v::max_items("value.names", names, 32)?;
                for (i, n) in names.iter().enumerate() {
                    let f = format!("value.names[{i}]");
                    if n.is_empty() {
                        return Err(Invalid::new(f, "required"));
                    }
                    if !is_env_name(n) {
                        return Err(Invalid::new(f, "pattern"));
                    }
                    if is_reserved_env_name(n) {
                        return Err(Invalid::new(f, "reserved"));
                    }
                }
                v::unique("value.names", names)
            }
            (ExecutableKey::EnvPassthrough, ExecutableValue::Path { .. }) => {
                Err(Invalid::new("value", "kindForKey"))
            }
            (_, ExecutableValue::EnvNames { .. }) => Err(Invalid::new("value", "kindForKey")),
            (_, ExecutableValue::Path { path }) => v::nested("value.path", path),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct SettingsSetExecutableRes {
    pub applied: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_names() {
        assert!(is_env_name("HTTPS_PROXY"));
        assert!(is_env_name("_X1"));
        assert!(!is_env_name("1X"));
        assert!(!is_env_name("lower"));
        assert!(!is_env_name(&"A".repeat(65)));
        for r in ["TMUX", "TMUX_PANE", "ITERM_PROFILE", "LC_TERMINAL_VERSION", "GILDE_HOME", "CLAUDE_CONFIG_DIR", "CLAUDE_CODE_MESSAGING_X"] {
            assert!(is_reserved_env_name(r), "{r}");
        }
        assert!(!is_reserved_env_name("HTTPS_PROXY"));
    }

    #[test]
    fn executable_key_wire_names() {
        assert_eq!(
            serde_json::to_string(&ExecutableKey::EnvPassthrough).unwrap(),
            "\"agents.envPassthrough\""
        );
        let req: SettingsSetExecutableReq = serde_json::from_str(
            r#"{"key":"agents.envPassthrough","value":{"kind":"envNames","names":["TMUX"]}}"#,
        )
        .unwrap();
        assert_eq!(req.validate().unwrap_err().rule, "reserved");
    }
}
