//! Activity: the derived state of a session (contract §3.5) — the wire types
//! (§3.4.13) and the in-process evidence types `derive()` consumes (§3.5.2).
//!
//! The evidence types are Rust-only: they flow from the producers (T03–T08)
//! into World (T02) through `EvidenceSink` and never cross IPC.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::contract::common::{Confidence, EpochMs};
use crate::contract::ids::{AccountId, EpisodeId};
use crate::contract::interaction::PendingInteraction;
use crate::contract::session::ControlMode;
use crate::contract::team::IdleReason;

/// `Activity` (§3.4.13): what the session is doing, since when, how sure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct Activity {
    pub state: ActivityState,
    /// §3.5.8: earliest `received_at` of the pending interactions, the
    /// registry `status_updated_at`, the prompt's first-seen time, or the
    /// episode start.
    #[ts(type = "number")]
    pub since: EpochMs,
    pub confidence: Confidence,
    pub evidence: Option<EvidenceLine>,
    pub episode: EpisodeId,
}

/// The M4 state list (§3.5.6) as a discriminated union on `kind`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum ActivityState {
    Starting,
    Working,
    NeedsApproval,
    NeedsAnswer,
    Error { sub: ErrorSub },
    Crashed { exit: ExitInfo },
    NoProgress { for_secs: u32 },
    NewReply,
    Ready,
    Ended,
    Stopped,
    StatusUnknown { why: UnknownWhy },
}

impl ActivityState {
    /// The `kind` wire name (episode keys, §3.5.8).
    pub fn kind_str(&self) -> &'static str {
        match self {
            ActivityState::Starting => "starting",
            ActivityState::Working => "working",
            ActivityState::NeedsApproval => "needsApproval",
            ActivityState::NeedsAnswer => "needsAnswer",
            ActivityState::Error { .. } => "error",
            ActivityState::Crashed { .. } => "crashed",
            ActivityState::NoProgress { .. } => "noProgress",
            ActivityState::NewReply => "newReply",
            ActivityState::Ready => "ready",
            ActivityState::Ended => "ended",
            ActivityState::Stopped => "stopped",
            ActivityState::StatusUnknown { .. } => "statusUnknown",
        }
    }
}

/// `ErrorSub` (§3.4.13).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum ErrorSub {
    /// `account_id` is `None` when the session's account is unknown; `resets_at`
    /// only when the vendor text carries one (§6.1.7).
    UsageLimit {
        account_id: Option<AccountId>,
        #[ts(type = "number | null")]
        resets_at: Option<EpochMs>,
    },
    SignInNeeded,
    DidntStart { reason: DidntStartReason },
    AgentError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum DidntStartReason {
    Timeout,
    Exited,
    CompatFailed,
    MinVersion,
}

/// How a process ended (own children; watched exits are not observable).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct ExitInfo {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

/// `UnknownWhy` (§3.4.13, R11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum UnknownWhy {
    ProcessOnly,
    UnknownPeerProtocol,
    NoThreadMap,
    Unregistered,
}

/// One evidence line under a session state ("Can't read updates right now.",
/// §9.12 `evidence.cantRead`): an i18n key plus data parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct EvidenceLine {
    pub key: String,
    #[ts(type = "Record<string, string | number | boolean>")]
    pub params: serde_json::Map<String, serde_json::Value>,
}

impl EvidenceLine {
    /// A line without parameters.
    pub fn key(key: &str) -> EvidenceLine {
        EvidenceLine { key: key.to_string(), params: serde_json::Map::new() }
    }

    /// A card detail line carrying (already sanitized) vendor text:
    /// `card.detailText` with `params.text` [A19.7].
    pub fn detail_text(text: impl Into<String>) -> EvidenceLine {
        let mut params = serde_json::Map::new();
        params.insert("text".to_string(), serde_json::Value::String(text.into()));
        EvidenceLine { key: "card.detailText".to_string(), params }
    }
}

/// Primary actions (§3.5.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum PrimaryAction {
    Stop,
    Open,
    Answer,
    GoToOwner,
    /// Ships as "Continue a copy on {account}" until LP-25 (§6.5).
    ContinueOnOtherAccount,
    SignIn,
    ShowLog,
    StartAsTerminal,
    Retry,
    ContinueInGilde,
    Review,
    Message,
    Continue,
    ContinueCopy,
    CleanUp,
}

// ---------------------------------------------------------------------------
// evidence (§3.5.2) — in-process only
// ---------------------------------------------------------------------------

/// The input of `derive(e, now)` (§3.5.2).
#[derive(Debug, Clone, PartialEq)]
pub struct EvidenceSet {
    /// Only for sessions Gilde spawned.
    pub gilde: Option<GildeFacts>,
    pub process: ProcessFacts,
    /// S1.
    pub stream: Option<StreamFacts>,
    /// S2.
    pub registry: Option<RegistryFacts>,
    /// S3.
    pub transcript: Option<TranscriptFacts>,
    /// S4.
    pub team: Option<TeamFileFacts>,
    /// S5.
    pub pty: Option<PtyFacts>,
    pub marks: UserMarks,
    /// ux-ia §4.2 No-progress exemption — from the members' own derived
    /// activity (Working), never from the lead's registry [R6 §5 LP-20].
    pub lead_has_working_members: bool,
    /// "Finished turns from sessions started elsewhere" [ux-ia §6.1].
    pub watched_band5_allowed: bool,
    /// Setting, default 300, range 60..=3600.
    pub no_progress_after_secs: u32,
    /// A read source of this session is `permissionDenied` (§5.8).
    pub source_denied: bool,
}

/// `marks`: last_seen_at, crash_dismissed_at, error_dismissed_at,
/// new_reply_dismissed_at (wall clock).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UserMarks {
    pub last_seen_at: Option<EpochMs>,
    pub crash_dismissed_at: Option<EpochMs>,
    pub error_dismissed_at: Option<EpochMs>,
    pub new_reply_dismissed_at: Option<EpochMs>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GildeFacts {
    pub spawned_at: EpochMs,
    pub mode: ControlMode,
    pub ready_at: Option<EpochMs>,
    pub stop_cause: Option<StopCause>,
    pub signals_sent: Vec<i32>,
    pub start_failure: Option<StartFailure>,
}

/// Why Gilde stopped a session (R1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum StopCause {
    User,
    Quit,
    ForceStop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StartFailure {
    CompatFailed,
    MinVersion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessFacts {
    Alive {
        pid: i32,
        start_sec: i64,
        cpu_active_at: Option<EpochMs>,
    },
    Exited {
        at: EpochMs,
        code: Option<i32>,
        signal: Option<i32>,
    },
    Gone {
        at: EpochMs,
    },
    HistoryOnly,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StreamFacts {
    pub phase: TurnPhase,
    pub phase_since: EpochMs,
    pub pending: Vec<PendingInteraction>,
    pub last_turn_start_at: Option<EpochMs>,
    pub last_turn_end: Option<TurnEnd>,
    pub last_frame_at: EpochMs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TurnPhase {
    Idle,
    Running,
    RequiresAction,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TurnEnd {
    pub at: EpochMs,
    pub outcome: TurnOutcome,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TurnOutcome {
    Ok,
    Interrupted,
    Error(ErrorSub),
}

/// Claude registry `status` (§5.2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RegStatus {
    Busy,
    Idle,
    Waiting,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RegistryFacts {
    pub status: Option<RegStatus>,
    pub waiting_for: Option<String>,
    pub status_updated_at: Option<EpochMs>,
    pub peer_protocol_ok: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptFacts {
    pub last_row_at: Option<EpochMs>,
    pub open_tool_use: Option<OpenToolUse>,
    pub last_turn_end_at: Option<EpochMs>,
    pub last_human_prompt_at: Option<EpochMs>,
    pub last_api_error: Option<(EpochMs, ErrorSub)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenToolUse {
    pub tool_use_id: String,
    pub tool_name: String,
    pub at: EpochMs,
    /// AskUserQuestion | ExitPlanMode.
    pub is_question: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TeamFileFacts {
    pub is_active: Option<bool>,
    pub in_roster: bool,
    /// (when, tool)
    pub permission_request: Option<(EpochMs, String)>,
    pub last_idle: Option<(EpochMs, IdleReason)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PtyFacts {
    pub status: PtyStatus,
    pub confidence: f32,
    pub last_output_at: Option<EpochMs>,
    pub prompt: Option<ScreenPromptFacts>,
}

/// The S5 engine's status (§3.5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PtyStatus {
    Idle,
    Working,
    WaitingForInput,
    Stalled,
}

/// A parsed screen prompt (§6.2.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenPromptFacts {
    /// First 16 hex of sha256(header + "\n" + question + "\n" + option labels)
    /// (A34.1): the header is the prompt box's rows above the question, so
    /// prompts that approve different commands never share a rev.
    pub prompt_rev: String,
    pub question: Option<String>,
    pub header: Option<String>,
    pub options: Vec<ScreenOptionFact>,
    pub footer: Option<String>,
    /// Option 1 starts with "Yes" and the last with "No".
    pub permission_shaped: bool,
    /// A menu without digits (folder trust, onboarding, theme…).
    pub setup: bool,
    /// The recognised folder-trust dialog (A6).
    pub trust_dialog: bool,
    /// "Tab to amend" in the footer.
    pub allows_note: bool,
    /// The run lies within the bottom 15 rows.
    pub near_bottom: bool,
    pub first_seen_at: EpochMs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenOptionFact {
    /// "1".."9" (empty for setup menus).
    pub key: String,
    pub label: String,
    pub description: Option<String>,
    pub selected: bool,
}
