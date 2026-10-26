//! Session **[meta]** (except `title`, `pending`, `queued`: **[content]**)
//! (contract §3.4.4).
//!
//! | Field | Invariant |
//! |---|---|
//! | `control` | `InGilde` ⇔ a live row of Gilde's PID registry for this instance, or a held turn-based session (§6.3). `Watching` ⇔ alive, not ours. `NotRunning` ⇔ no live process and not held. `activity.state ∈ {Ended, Stopped, Crashed}` ⇒ `NotRunning`; `NotRunning` ⇒ `activity.state ∈ {Ended, Stopped, Crashed, Error·Didn't start, Error·Sign-in needed, Error·Usage limit}` [F-14, F-21] |
//! | `control.InGilde.pty` | present ⇔ `mode == Terminal` |
//! | `control.InGilde.hosted_team` | true only for a Claude lead started with Team on (§7.4) |
//! | `title` | never persisted; recomputed on demand (§5.9) |
//! | `pending` | non-empty only while live; distinct `request_id`s; cleared on exit |
//! | `caps` | recomputed on every change of control, activity, pending, compat |
//! | `memory_bytes` | quantized to 10 MB steps (< 1 GB) / 100 MB steps (≥ 1 GB) |

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::contract::activity::Activity;
use crate::contract::common::{AbsPath, CompatState, EpochMs, IdentityColour, PermissionMode, Vendor};
use crate::contract::ids::{
    AccountId, ClientMsgId, LocationId, MemberKey, ProjectId, PtyId, SessionUid, TeamKey,
};
use crate::contract::interaction::PendingInteraction;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct Session {
    pub uid: SessionUid,
    pub vendor: Vendor,
    pub account_id: Option<AccountId>,
    /// Validated per §3.2.
    pub vendor_session_id: Option<String>,
    pub project_id: Option<ProjectId>,
    pub location_id: Option<LocationId>,
    pub cwd: Option<AbsPath>,
    pub control: Control,
    pub activity: Activity,
    /// **[content]** ≤ 120 chars; agent-name → custom-title → ai-title → first
    /// prompt (60) → None.
    pub title: Option<String>,
    pub team: Option<TeamRole>,
    #[ts(type = "number | null")]
    pub started_at: Option<EpochMs>,
    #[ts(type = "number | null")]
    pub ended_at: Option<EpochMs>,
    /// Last timestamped row/frame, never file mtime [R2 E30].
    #[ts(type = "number | null")]
    pub last_activity_at: Option<EpochMs>,
    /// **[content]** oldest first (§3.4.5).
    pub pending: Vec<PendingInteraction>,
    /// **[content]** Conversation: sent while a turn runs; TurnBased: ≤ 1 held.
    pub queued: Vec<QueuedMessage>,
    pub model: Option<String>,
    pub permission_mode: Option<PermissionMode>,
    /// Resolved at spawn for Gilde's sessions (§6.0), from the exec path for
    /// watched ones [A1].
    pub binary: Option<BinaryRef>,
    /// = `binary.version` at spawn (Gilde) or the registry `version` (watched);
    /// selects the ClaudeCompat record for the session's whole life (§3.10).
    pub claude_version: Option<String>,
    /// pgid-subtree RSS, quantized (§11.2).
    #[ts(type = "number | null")]
    pub memory_bytes: Option<u64>,
    pub usage_today: Option<UsageToday>,
    /// What the UI may offer; computed in Rust.
    pub caps: SessionCaps,
    /// ⋯ Details popover facts.
    pub details: SessionDetails,
    /// Tombstone (§3.3.3): the WebView remaps every reference and never shows it.
    pub merged_into: Option<SessionUid>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum Control {
    InGilde {
        mode: ControlMode,
        pty: Option<PtyId>,
        hosted_team: bool,
        transport: Transport,
    },
    Watching {
        origin: Origin,
        owner: Option<OwnerApp>,
    },
    NotRunning {
        last_origin: Option<Origin>,
        ended_how: EndedHow,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum ControlMode {
    Conversation,
    Terminal,
    TurnBased,
}

impl crate::contract::validate::Validate for ControlMode {}

/// How Gilde talks to an In Gilde session: `L1` Conversation via the core,
/// `L2` only when §6.1.10 switched that version ("Connection: helper").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum Transport {
    L1,
    L2,
    Pty,
    Turns,
}

/// Origin precedence: §3.4.4 `origin` row; `OtherGilde` from §5.2.2 [F-42, N-05].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum Origin {
    Terminal,
    VsCode,
    /// The ChatGPT (Codex) desktop app's own codex server.
    ChatGptApp,
    ItermTeam,
    TmuxTeam,
    TerminalTeam,
    Background,
    Headless,
    Orchestrator,
    OtherGilde,
    Unknown,
}

impl crate::contract::validate::Validate for Origin {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum EndedHow {
    Ended,
    Stopped,
    Crashed,
    HistoryOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct OwnerApp {
    pub app_pid: i32,
    /// "Terminal", "iTerm2", "VS Code"… display only.
    pub app_name: String,
    pub kind: OwnerKind,
    pub bundle_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum OwnerKind {
    Terminal,
    VsCode,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum MemberRole {
    Lead,
    Member,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct TeamRole {
    pub team_key: TeamKey,
    pub member_key: MemberKey,
    pub role: MemberRole,
    pub colour: Option<IdentityColour>,
}

/// **[content]**
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct QueuedMessage {
    pub client_msg_id: ClientMsgId,
    /// ≤ 256 KiB.
    pub text: String,
    #[ts(type = "number")]
    pub queued_at: EpochMs,
}

/// The binary a session runs: resolved at spawn (§6.0 *Binary resolution*) [A1, N-07].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct BinaryRef {
    pub realpath: AbsPath,
    pub fingerprint: BinaryFingerprint,
    /// `^\d+\.\d+\.\d+$` from `--version`; `None` when it could not be read.
    pub version: Option<String>,
    /// The kernel's exec path (`proc_pidpath`) once the child runs [N-07].
    pub pidpath: Option<AbsPath>,
}

/// `(size, mtime, inode)` of a binary (§3.10, `claude_compat_observed`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct BinaryFingerprint {
    #[ts(type = "number")]
    pub size: u64,
    #[ts(type = "number")]
    pub mtime_ms: i64,
    #[ts(type = "number")]
    pub inode: u64,
}

impl BinaryFingerprint {
    /// `"<size>:<mtime_ms>:<inode>"` (the `claude_compat_observed` key, §3.9).
    pub fn key(&self) -> String {
        format!("{}:{}:{}", self.size, self.mtime_ms, self.inode)
    }
}

/// Today's usage; "unknown" (absent) is a legitimate value, never 0 [ux-ia §3.15].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct UsageToday {
    /// Local calendar day `YYYY-MM-DD`.
    pub day: String,
    #[ts(type = "number | null")]
    pub input_tokens: Option<u64>,
    #[ts(type = "number | null")]
    pub output_tokens: Option<u64>,
    #[ts(type = "number | null")]
    pub cached_tokens: Option<u64>,
    /// Integer micro-USD, only when the vendor reports cost (§3.9).
    #[ts(type = "number | null")]
    pub cost_micros: Option<u64>,
}

/// The single source of what is clickable [ux-ia P4].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct SessionCaps {
    pub send: SendCap,
    pub interrupt: bool,
    pub force_stop: bool,
    pub approve: ApproveCap,
    pub set_model: bool,
    pub set_permission_mode: Vec<PermissionMode>,
    pub continue_options: Vec<ContinueOption>,
    pub stop_session: StopCap,
    pub terminal_view: bool,
    pub log_view: bool,
    pub changes: ChangesCap,
    pub owner_focus: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum SendCap {
    None,
    Conversation,
    PtyPaste,
    Turn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum ApproveCap {
    None,
    Structured,
    ScreenKeys,
    GoToOwner,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum ContinueOption {
    Resume,
    Fork,
    Attach,
    OtherAccount,
    RetryAsTerminal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum StopCap {
    None,
    Fenced,
    ClaudeStop,
    TermAfterIdentity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum ChangesCap {
    None,
    ReadOnly,
    /// commit/merge/discard
    Full,
}

/// ⋯ Details popover facts (§3.4.4, all **[meta]** except
/// `orchestrator_task_title`). Paths are display strings: under the home
/// directory as `~/…` (§4.8, §9.7). The redacted command line is **not** here
/// (fetched on demand by `session_technical`) [F-03].
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct SessionDetails {
    pub vendor_session_id: Option<String>,
    pub account_label: Option<String>,
    pub config_dir: Option<String>,
    pub cwd: Option<String>,
    pub location: Option<String>,
    pub model: Option<String>,
    pub control_mode: Option<ControlMode>,
    /// L1 / L2 ("Connection: helper") / PTY / turns.
    pub connection: Option<Transport>,
    pub claude_version: Option<String>,
    /// "Not yet verified with Gilde" when Unverified (§9.12).
    pub compat_state: Option<CompatState>,
    pub binary_realpath: Option<String>,
    /// Spawn-time vs `system/init` version note [A1].
    pub version_note: Option<String>,
    #[ts(type = "number | null")]
    pub started_at: Option<EpochMs>,
    #[ts(type = "number | null")]
    pub memory_bytes: Option<u64>,
    /// With "Reveal in Finder" via `ide_reveal`.
    pub transcript_path: Option<String>,
    pub team_raw_id: Option<String>,
    /// **[content]**
    pub orchestrator_task_title: Option<String>,
    pub usage_today: Option<UsageToday>,
    /// §6.1.11: the tool that ran without a seen permission request.
    pub approval_channel_unconfirmed: Option<String>,
    /// "Updates arrive when each step completes · last update {time}" (§6.4).
    #[ts(type = "number | null")]
    pub follow_last_row_at: Option<EpochMs>,
    /// §3.10 `details.teammateVersionDiffers`.
    pub teammate_version_differs: bool,
    /// §5.2.4 `details.teamFormatUnverified`.
    pub team_format_unverified: bool,
}
