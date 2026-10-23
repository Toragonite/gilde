//! The store mirror and the global events (contract §4.3, D2).
//!
//! One authoritative in-memory graph mirrored to the WebView by
//! `snapshot + rev + patches` over one ordered `Channel<StorePatch>`. Patches
//! are coalesced to ≤ 10 per second; ops for the same entity within a window
//! collapse to the last; a patch carries ≤ 2,000 ops, otherwise World sends
//! `Resync` and the WebView calls `store_subscribe` again. A patch is ≤ 1 MiB;
//! the snapshot for 500 sessions ≤ 2 MiB.
//!
//! Only four Tauri events exist; everything else is a Channel.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::contract::account::Account;
use crate::contract::attention::AttentionQueue;
use crate::contract::common::{EpochMs, Notice, Vendor};
use crate::contract::compat::ClaudeCompat;
use crate::contract::ids::{SessionUid, TeamKey, UndoToken};
use crate::contract::ipc::VendorOption;
use crate::contract::process::ProcessRecord;
use crate::contract::project::{Location, Project};
use crate::contract::session::Session;
use crate::contract::team::{Member, Task, Team, TeamMessage};

/// The most ops one patch may carry before World sends `Resync` instead.
pub const MAX_OPS_PER_PATCH: usize = 2_000;

/// `store_subscribe` response (§4.2.1). Atomic: taken under the World lock;
/// every later patch with `fromRev ≥ rev` goes to the subscribing channel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct StoreSnapshot {
    #[ts(type = "number")]
    pub rev: u64,
    pub projects: Vec<Project>,
    pub locations: Vec<Location>,
    pub sessions: Vec<Session>,
    pub teams: Vec<Team>,
    pub members: Vec<Member>,
    pub tasks: Vec<Task>,
    pub messages: Vec<TeamMessage>,
    pub queue: AttentionQueue,
    pub accounts: Vec<Account>,
    pub leftovers: Vec<ProcessRecord>,
    pub compat: Vec<ClaudeCompat>,
    pub notices: Vec<Notice>,
    pub boot: BootStatus,
    pub sources: Vec<SourceStatus>,
    pub integrations: IntegrationsState,
}

/// One message on the store channel. The WebView MUST resubscribe on any `rev`
/// gap or `Resync` [R1 §1d graphSlice].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct StorePatch {
    #[ts(type = "number")]
    pub from_rev: u64,
    #[ts(type = "number")]
    pub to_rev: u64,
    pub ops: Vec<StoreOp>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum StoreOp {
    Upsert { entity: Box<Entity> },
    Remove { entity_kind: EntityKind, id: String },
    Queue { queue: AttentionQueue },
    Boot { status: BootStatus },
    Notices { notices: Vec<Notice> },
    Compat { list: Vec<ClaudeCompat> },
    Sources { statuses: Vec<SourceStatus> },
    Integrations { state: IntegrationsState },
    /// The quit sweep's progress (§10.2).
    Quit { stopped: u32, total: u32, phase: QuitPhase },
    Resync,
}

/// An upserted entity. Struct variants with `value` because some entities have
/// a `kind` field of their own (e.g. `Project.kind`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum Entity {
    Project { value: Project },
    Location { value: Location },
    Session { value: Box<Session> },
    Team { value: Team },
    Member { value: Member },
    Task { value: Task },
    Message { value: TeamMessage },
    Account { value: Account },
    Leftover { value: ProcessRecord },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum EntityKind {
    Project,
    Location,
    Session,
    Team,
    Member,
    Task,
    Message,
    Account,
    Leftover,
}

/// Boot progress (§2.4) as the WebView needs it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct BootStatus {
    /// B6: the boot sweep finished and spawns may run.
    pub spawn_gate_open: bool,
    /// B7: discovery pass 1 delivered its first rows.
    pub first_discovery_done: bool,
    /// B9: PATH + `--version` detection finished ("Finding installed agents…" until then).
    pub agents_detected: bool,
    /// §2.4 memory-only mode: the reason key, or `None` when state is persisted.
    pub memory_only: Option<String>,
    /// A29.1: the last completed detection — one row per vendor in the order
    /// claude, codex, cursor, agy (`installed: false, version: None` for a
    /// missing CLI); empty until `agents_detected`. A rerun (`agents_rescan`,
    /// an applied `*Binary` / `pathsOverride` setting) keeps the previous rows
    /// until its result replaces them; the rows are the same ones
    /// `newsession_options.vendors` answers.
    pub agents: Vec<VendorOption>,
    /// A29.1: when the last detection completed (changes on every rerun, even
    /// when the rows are identical); `None` until the first one.
    #[ts(type = "number | null")]
    pub agents_scanned_at: Option<EpochMs>,
    /// A29.3: the boot sweep's pending stops (crash leftovers and quit
    /// survivors of the previous instance) while their undo window runs;
    /// `None` when there are none, after the window ends, or after the undo.
    pub recovery: Option<RecoveryWindow>,
}

/// A29.3: the Recovered dialog's feed — the boot sweep's scheduled stops,
/// grouped under one undo token (`undo_cancel{token}` cancels every stop of
/// the group still pending).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct RecoveryWindow {
    /// Processes that will receive the fenced stop when the window ends (≥ 1).
    pub stopping: u32,
    pub undo_token: UndoToken,
    /// Wall-clock end of the undo window.
    #[ts(type = "number")]
    pub until: EpochMs,
}

/// Per-source discovery status (drives the TCC notice, §5.8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct SourceStatus {
    pub vendor: Vendor,
    pub source: SourceKind,
    pub state: SourceState,
    #[ts(type = "number | null")]
    pub last_reconcile_at: Option<EpochMs>,
    /// Stale registry entries hidden by the liveness rule.
    pub hidden_stale: u32,
    pub indexed: u32,
    /// A cap of §5.8 was hit.
    pub truncated: bool,
}

/// The discovery readers of §5.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum SourceKind {
    Registry,
    Processes,
    Teams,
    Tasks,
    Transcripts,
    Jobs,
    Rollouts,
    ThreadIndex,
    Chats,
    Summaries,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum SourceState {
    Ok,
    PermissionDenied,
    Unavailable { reason: String },
    Disabled,
}

/// Optional integrations (§5.10).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct IntegrationsState {
    pub orchestrator: OrchestratorPresence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum OrchestratorPresence {
    /// No element renders (P8); Settings shows ux-ia #119.
    Absent,
    Present,
    /// Teams keep their last state with ux-ia #120.
    PresentNotRunning,
}

/// The quit sweep phases (§10.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum QuitPhase {
    Recording,
    Stopping,
    Done,
}

// ---------------------------------------------------------------------------
// the four global events (§4.3)
// ---------------------------------------------------------------------------

/// Event names, exactly as emitted.
pub mod event_names {
    pub const FOCUS_REQUEST: &str = "gilde://focus-request";
    pub const QUIT_REQUESTED: &str = "gilde://quit-requested";
    pub const TOAST: &str = "gilde://toast";
    pub const VISIBILITY: &str = "gilde://visibility";
}

/// `gilde://focus-request`: notification click, single-instance relaunch, dock menu.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct FocusRequestEvent {
    pub target: FocusTarget,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum FocusTarget {
    Session { uid: SessionUid, focus_card: bool },
    Home,
    Team { team_key: TeamKey, mode: TeamViewMode },
    Floor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum TeamViewMode {
    Panes,
    Board,
    Floor,
}

/// `gilde://quit-requested`: ⌘Q / menu Quit while sessions run in Gilde (the
/// exit is prevented until `app_quit`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct QuitRequestedEvent {}

/// `gilde://toast`: outcomes of the user's own actions only [ux-ia §6.2].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct ToastEvent {
    pub key: String,
    #[ts(type = "Record<string, string | number | boolean>")]
    pub params: serde_json::Map<String, serde_json::Value>,
    pub undo_token: Option<UndoToken>,
}

/// `gilde://visibility`: window occlusion / app activation / sleep-wake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct VisibilityEvent {
    pub frontmost: bool,
    pub occluded: bool,
    pub asleep: bool,
}
