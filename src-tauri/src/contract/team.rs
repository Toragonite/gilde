//! Teams (contract §3.4.6–§3.4.8 **[meta]**) and the vendor-neutral team
//! protocol (§7.1, M14): one [`TeamAdapter`] trait + the capability descriptor.
//!
//! The UI talks only to this abstraction (§7.1.3): a control appears iff the
//! capability allows it; a fact the adapter marks `Partial`/`Unknown` is shown
//! with "≈" or "unknown", never invented.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::contract::activity::{ActivityState, TeamFileFacts};
use crate::contract::common::{EpochMs, IdentityColour, Vendor};
use crate::contract::ids::{MemberKey, MessageKey, PaneId, ProjectId, PtyId, RequestId, SessionUid, TaskKey, TeamKey};
use crate::contract::interaction::Decision;
use crate::contract::session::MemberRole;

// ---------------------------------------------------------------------------
// Team, Member (§3.4.6)
// ---------------------------------------------------------------------------

/// Invariants: a Team row is shown only with ≥ 1 member besides the lead;
/// members never disappear while the team lives; members: lead first, then
/// `joinedAt` order; ≤ 200 members per team.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct Team {
    pub key: TeamKey,
    pub source: TeamSource,
    /// Display name (team name; raw id only in Details).
    pub name: String,
    /// e.g. "session-25ae1157"
    pub raw_id: String,
    pub project_id: Option<ProjectId>,
    pub lead: Option<MemberKey>,
    pub lead_binding: LeadBinding,
    /// Gilde hosts its panes (§7.4).
    pub hosted: bool,
    /// For the ownership bar.
    pub external_host: Option<ExternalHost>,
    pub caps: TeamCapabilities,
    /// Most urgent member state, band order [ux-ia §4.9].
    pub glyph_state: ActivityState,
    pub progress: TeamProgress,
    #[ts(type = "number | null")]
    pub started_at: Option<EpochMs>,
    #[ts(type = "number | null")]
    pub ended_at: Option<EpochMs>,
    pub pane_hosting: PaneHostingState,
    /// The last config.json parse failed; last good roster kept.
    pub roster_stale: bool,
    /// A30.1: panes of a hosted team that are not a member's session —
    /// starting placeholders and unknown panes (§7.4.5); creation order, at
    /// most 16; empty for teams Gilde does not host.
    pub pane_tiles: Vec<PaneTile>,
}

/// A30.1: one non-member pane of a hosted team (from PaneHost's pane table,
/// A21.1). A pane bound to a member session is never a tile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct PaneTile {
    pub pane_id: PaneId,
    pub state: PaneTileState,
    /// Sanitized display text (the pane title), if any.
    pub title: Option<String>,
    /// Only for a running `Unknown` pane: its terminal, shown read-only.
    pub pty: Option<PtyId>,
    #[ts(type = "number")]
    pub created_at: EpochMs,
}

/// A30.1: what a non-member pane tile shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum PaneTileState {
    /// A placeholder or spawning pane: "starting…"; the UI shows
    /// `team.didNotStart` when `spawn_failed` or at `since + 30 s`.
    Starting {
        #[ts(type = "number")]
        since: EpochMs,
        spawn_failed: bool,
    },
    /// An `Unknown`-kind pane (`team.unknownPane`): never a member, never in
    /// the queue, never auto-focused.
    Unknown { running: bool, exit_code: Option<i32> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum TeamSource {
    ClaudeNative,
    Orchestrator,
    /// v1.x (OQ2).
    Emulated,
}

impl TeamSource {
    /// The wire spelling, also the `TeamKey` derivation input (§3.2).
    pub const fn as_str(self) -> &'static str {
        match self {
            TeamSource::ClaudeNative => "claudeNative",
            TeamSource::Orchestrator => "orchestrator",
            TeamSource::Emulated => "emulated",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum LeadBinding {
    /// argv `--parent-session-id`.
    Exact,
    /// createdAt ≈ startedAt.
    Heuristic,
    Hosted,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum ExternalHost {
    ITerm2,
    Tmux,
    /// A hosted lead that moved to the background (§7.4.7) [F-19].
    Background,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct TeamProgress {
    pub done: u32,
    pub total: u32,
    pub inferred: bool,
    pub approx: bool,
    /// `None` for pane recipients (§3.6).
    pub in_flight: Option<u32>,
    pub bottlenecks: Vec<MemberKey>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum PaneHostingState {
    Hosted,
    InProcessFallback,
    OutsideGilde,
    NotApplicable,
}

/// `presence = Present` ⇔ listed in the roster ∧ (pane: a live process with
/// matching `--agent-id`; in-process: the lead is alive).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct Member {
    pub key: MemberKey,
    pub team_key: TeamKey,
    /// `None` for in-process members without a resolvable transcript yet.
    pub session_uid: Option<SessionUid>,
    /// ≤ 64; sanitized.
    pub name: String,
    /// "<name>@<team>"
    pub agent_id: Option<String>,
    pub role: MemberRole,
    pub colour: Option<IdentityColour>,
    pub vendor: Vendor,
    pub backend: MemberBackend,
    #[ts(type = "number | null")]
    pub joined_at: Option<EpochMs>,
    pub presence: Presence,
    /// Hosted pane members.
    pub pane: Option<PaneId>,
    pub plan_mode_required: bool,
    pub current_task: Option<TaskKey>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum MemberBackend {
    InProcess,
    Pane,
    Orchestrator,
    Emulated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum Presence {
    Present,
    Gone {
        reason: GoneReason,
        #[ts(type = "number")]
        at: EpochMs,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum GoneReason {
    ShutdownApproved,
    ProcessExited,
    RemovedFromConfig,
    Unknown,
}

// ---------------------------------------------------------------------------
// TeamMessage (§3.4.7) — text is [content], fetched on demand
// ---------------------------------------------------------------------------

/// The store keeps at most 500 messages per team; text and summary are never
/// in the store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct TeamMessage {
    pub key: MessageKey,
    pub team_key: TeamKey,
    pub from: Party,
    pub to: Party,
    #[ts(type = "number")]
    pub at: EpochMs,
    pub kind: MessageKind,
    pub status: DeliveryStatus,
    /// Derived per turn for Claude.
    pub in_reply_to: Option<MessageKey>,
    /// Reconstructed by order/time rather than msg_id ("≈" in the UI).
    pub derived: bool,
    /// IdleNotification only.
    pub idle_reason: Option<IdleReason>,
    /// Sender-written summary exists (read on demand).
    pub has_summary: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum Party {
    Member { member: MemberKey },
    User,
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum MessageKind {
    Plain,
    IdleNotification,
    ShutdownRequest,
    ShutdownApproved,
    ShutdownRejected,
    PlanApprovalRequest,
    PlanApprovalResponse,
    PermissionRequest,
    PermissionResponse,
    TaskAssignment,
    TeammateTerminated,
    Unknown,
}

/// [R5 §9.3 #1]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum DeliveryStatus {
    Sent,
    Queued,
    Delivered,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum IdleReason {
    Available,
    Interrupted,
    Failed,
}

// ---------------------------------------------------------------------------
// Task (§3.4.8) — subject/description are [content]
// ---------------------------------------------------------------------------

/// Invariants: `blocked_by`/`blocks` only reference tasks of the same team; a
/// `Deleted` task is removed from both lists of every task; `station` is a
/// pure function of (status, owner, blocked_by open, owner session in band
/// 1–2); inferred tasks never have `blocked_by`, never a title, and their
/// station set is {Assigned, InProgress, NeedsYou, Reported}; ≤ 1,000 per team.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct Task {
    pub key: TaskKey,
    pub team_key: TeamKey,
    /// Claude: positive integer as string.
    pub native_id: Option<String>,
    /// **[content]** subject ≤ 200 chars; inferred items: None.
    pub title: Option<String>,
    /// The k of "Request {k}" for inferred items.
    pub inferred_k: Option<u32>,
    pub status: TaskStatus,
    /// Derived (pixel-office §2.1).
    pub station: Station,
    pub owner: Option<MemberKey>,
    /// Declared only; never inferred.
    pub blocked_by: Vec<TaskKey>,
    pub blocks: Vec<TaskKey>,
    #[ts(type = "number | null")]
    pub created_at: Option<EpochMs>,
    #[ts(type = "number | null")]
    pub started_at: Option<EpochMs>,
    #[ts(type = "number | null")]
    pub completed_at: Option<EpochMs>,
    /// "≥ {t}" elapsed when Gilde started late.
    #[ts(type = "number | null")]
    pub first_seen_in_progress_at: Option<EpochMs>,
    pub source: TaskSource,
    pub inferred: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum TaskStatus {
    Open,
    InProgress,
    Done,
    Deleted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum Station {
    ToDo,
    Blocked,
    Assigned,
    InProgress,
    NeedsYou,
    Done,
    Reported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum TaskSource {
    File,
    Transcript,
    Orchestrator,
    Inferred,
}

// ---------------------------------------------------------------------------
// §7.1.2 capability descriptor
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct TeamCapabilities {
    pub observe_members: Support,
    pub observe_member_state: Support,
    pub observe_messages: Support,
    pub observe_in_flight: Support,
    pub observe_tasks: Support,
    pub observe_approvals: Support,
    pub observe_questions: Support,
    pub act_send: Vec<SendRoute>,
    pub act_broadcast: Vec<SendRoute>,
    pub act_spawn: Vec<SendRoute>,
    pub act_stop: Vec<StopRoute>,
    pub act_answer_approval: ApprovalRoute,
    pub act_task_write: TaskWrite,
    pub correlation_report: Correlation,
    pub durability_task_board: Durability,
    pub durability_message_log: Durability,
    /// i18n key shown in ⋯ Details, e.g. "Counts marked ≈ are inferred".
    pub fidelity_note: Option<String>,
}

impl TeamCapabilities {
    /// The all-`Native` descriptor of the emulated engine and the fake adapter
    /// (§7.3.3, §7.3.4).
    pub fn all_native() -> TeamCapabilities {
        TeamCapabilities {
            observe_members: Support::Yes,
            observe_member_state: Support::Yes,
            observe_messages: Support::Yes,
            observe_in_flight: Support::Yes,
            observe_tasks: Support::Yes,
            observe_approvals: Support::Yes,
            observe_questions: Support::Yes,
            act_send: vec![SendRoute::Native],
            act_broadcast: vec![SendRoute::Native],
            act_spawn: vec![SendRoute::Native],
            act_stop: vec![StopRoute::Native],
            act_answer_approval: ApprovalRoute::Native,
            act_task_write: TaskWrite::Native,
            correlation_report: Correlation::PerRequest,
            durability_task_board: Durability::Durable,
            durability_message_log: Durability::Durable,
            fidelity_note: None,
        }
    }
}

/// `Inferred` is the `observe.tasks` value of §7.2.5 (inferred items are the board).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum Support {
    Yes,
    Partial,
    No,
    Unknown,
    Inferred,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum SendRoute {
    Native,
    ViaLead,
    ViaMemberPane,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum StopRoute {
    ViaLead,
    ClosePane,
    Native,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum ApprovalRoute {
    InLeadUi,
    InMemberPane,
    Native,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum TaskWrite {
    None,
    Native,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum Correlation {
    PerTurn,
    PerRequest,
    None,
    /// Orchestrator teams (§7.1.2 table: "Unknown").
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum Durability {
    Durable,
    Transient,
    Derived,
}

// ---------------------------------------------------------------------------
// §7.1.1 operations
// ---------------------------------------------------------------------------

/// Every team source implements this. Observation is push (adapters emit
/// [`TeamEvent`]s into World); actions return an outcome that may be
/// asynchronous ("asked the lead"), observed afterwards rather than
/// acknowledged [R5 §9.3 #7].
///
/// Rules (§7.1.4, tested with the fake adapter): observation never writes;
/// asynchronous acts return `PrefillLead` (or `Done` when the effect is the
/// typing itself); missing data is `Unknown`/`None`, never a default that looks
/// like data; member keys include `joined_at`; team end is emitted from
/// process/dir facts and a team is never merged with a resumed lead's new team.
pub trait TeamAdapter: Send + Sync {
    fn source(&self) -> TeamSource;
    fn capabilities(&self, team: &TeamKey) -> TeamCapabilities;
    /// Observation → World.
    fn subscribe(&self, sink: Arc<dyn TeamEventSink>);
    // Actions: each is called only when the capability allows it; otherwise the
    // UI never offers it.
    fn send(&self, team: &TeamKey, to: &MemberKey, text: &str) -> ActOutcome;
    fn broadcast(&self, team: &TeamKey, text: &str) -> ActOutcome;
    fn spawn(&self, team: &TeamKey, spec: &MemberSpec) -> ActOutcome;
    fn stop(&self, team: &TeamKey, member: &MemberKey, how: StopHow) -> ActOutcome;
    /// Claude: `Unsupported` (the user answers where the request lives).
    fn answer_approval(&self, team: &TeamKey, req: &RequestId, d: &Decision) -> ActOutcome;
    fn task_write(&self, team: &TeamKey, op: TaskOp) -> ActOutcome;
}

/// Where adapters push observations (World implements it).
pub trait TeamEventSink: Send + Sync {
    fn emit(&self, event: TeamEvent);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActOutcome {
    /// Performed (e.g. typed into a hosted member pane).
    Done,
    /// The UI fills the lead's composer; the user's ⏎ sends it.
    PrefillLead { lead: SessionUid, action: PrefillAction },
    Unsupported { reason: String },
}

/// The `team.prefill.*` texts of §9.12.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrefillAction {
    Tell { member: MemberKey },
    TellAll,
    Spawn { name: String },
    AskStop { member: MemberKey },
    AddTask,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StopHow {
    Graceful,
    HardClosePane,
}

/// What to spawn (via the lead for Claude; natively for the v1.x engine).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberSpec {
    pub name: String,
    pub vendor: Option<Vendor>,
    pub model: Option<String>,
}

/// A task-board write (`act_task_write = Native` only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskOp {
    Create { subject: String, owner: Option<MemberKey> },
    SetStatus { task: TaskKey, status: TaskStatus },
    Assign { task: TaskKey, owner: Option<MemberKey> },
    Delete { task: TaskKey },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QuestionLocation {
    MemberPane,
    Lead,
    Unverified,
}

/// Observations an adapter pushes (§7.1.1).
#[derive(Debug, Clone, PartialEq)]
pub enum TeamEvent {
    TeamSeen {
        key: TeamKey,
        name: String,
        raw_id: String,
        project: Option<ProjectId>,
        lead: Option<MemberKey>,
        binding: LeadBinding,
    },
    /// Gilde-emitted for Claude [R5 §9.3 #5].
    TeamEnded { key: TeamKey, at: EpochMs },
    MemberUpserted(Member),
    MemberGone { key: MemberKey, reason: GoneReason, at: EpochMs },
    MessageObserved(TeamMessage),
    MessageStatus { key: MessageKey, status: DeliveryStatus },
    TaskUpserted(Task),
    TaskRemoved { key: TaskKey },
    TaskBoardReset { key: TeamKey, at: EpochMs },
    /// Feeds S4 of derive().
    MemberFacts { member: MemberKey, facts: TeamFileFacts },
    ApprovalAttributed { member: MemberKey, lead: SessionUid, tool: String, at: EpochMs },
    QuestionAttributed { member: MemberKey, location: QuestionLocation, at: EpochMs },
    PlanReview { member: MemberKey, at: EpochMs },
    InFlight { team: TeamKey, count: Option<u32> },
}
