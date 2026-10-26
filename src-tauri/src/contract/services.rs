//! The service traits between build tasks (contract §15.2 T01 row, §6.0).
//!
//! Every wave-1 task implements the trait it *provides* and consumes the others
//! only through these traits, so the tasks build in parallel against stubs.
//! The method sets are derived from §15.2 "Provides → consumers" / "Consumes"
//! and the command semantics of §4.2; a task that needs another method reports
//! it (a contract change, §15.1).
//!
//! | Trait | Provided by | Consumed by |
//! |---|---|---|
//! | [`WorldApi`], [`EvidenceSink`], [`StorePatchSink`] | T02 | T03–T12, T19, commands |
//! | [`CompatApi`], [`CompatSink`] | T21 | T02, T05, T07, T09, T10 |
//! | [`SupervisorApi`] | T09 | T05, T06, T08, T10, T11, T21 |
//! | [`PtyApi`] | T06 | T07, T10 |
//! | [`PaneHostApi`] | T07 | T10 |
//! | [`SidecarApi`], [`TurnEventSink`] | T08 | T10 |
//! | [`GitApi`], [`IdeApi`] | T11 | T10, T12 |
//! | [`NotifyApi`] | T12 | T02, commands |
//! | [`SessionActorApi`] | T10 | T08 (sign-in), T09 (`continue_all`, quit) |
//! | [`ContentHubApi`] | T04 | T05, T07, T08 |
//! | [`SessionDriver`] (§6.0) | T05, T06, T08 | T10 |

use std::sync::Arc;

use async_trait::async_trait;

use crate::contract::account::{Account, Quota};
use crate::contract::activity::{
    ExitInfo, GildeFacts, ProcessFacts, PtyFacts, RegistryFacts, ScreenPromptFacts, StopCause,
    StreamFacts, TeamFileFacts, TranscriptFacts,
};
use crate::contract::attention::AttentionQueue;
use crate::contract::common::{AbsPath, EpochMs, Notice, PermissionMode, Vendor};
use crate::contract::compat::{ClaudeCompat, CompatCapability};
use crate::contract::content::{Block, BlockPatch, ConvoSourceKind, ResetReason};
use crate::contract::error::IpcError;
use crate::contract::ids::{
    AccountId, ClientMsgId, LocationId, MemberKey, ProcessKey, ProjectId, PtyId, QuitRecordId,
    RequestId, SessionUid, TeamKey,
};
use crate::contract::interaction::Decision;
use crate::contract::ipc::{
    AccountSignInRes, AppliesFrom, ChangesGetReq, ChangesView, ContinueAllRes, DismissWhat,
    EditorKind, GitCommitPreviewReq, GitCommitPreviewRes, GitCommitReq, GitCommitRes,
    GitMergePreviewReq, GitMergePreviewRes, GitMergeReq, GitMergeRes, IdeEditorsRes, IdeOpenReq,
    IdeRevealReq, InteractionAnswerReq, InteractionAnswerRes, InteractionDetailReq,
    InteractionDetailRes, InterruptOutcome, NewSessionOptions, NewsessionOptionsReq, PauseUntil,
    ScreenKeyOutcome, ScreenPromptAction, ScreenPromptKeyReq, ScreenPromptKeyRes, SendDelivery,
    OutgoingImage, SendPriority, SessionContinueReq, SessionContinueRes, SessionModelsRes, SessionRetryAsTerminalReq,
    SessionRetryAsTerminalRes, SessionStartReq, SessionStartRes, SessionStopPreviewRes,
    SessionStopReq, SessionStopRes, SessionTechnicalRes, TurnCancelQueuedReq, TurnCancelQueuedRes,
    TurnInterruptReq, TurnInterruptRes, TurnRetryReq, TurnRetryRes, TurnSendReq, TurnSendRes,
    ViewState, WorktreeDiscardPreviewReq, WorktreeDiscardPreviewRes, WorktreeDiscardReq,
};
use crate::contract::process::{ProcKind, ProcessRecord};
use crate::contract::session::{BinaryRef, ControlMode, Origin, OwnerApp, UsageToday};
use crate::contract::sidecar::{AuthStatus, ModelInfo, ProbeParams, TurnEvent, TurnStartParams};
use crate::contract::store::{
    BootStatus, IntegrationsState, QuitPhase, SourceStatus, StorePatch, StoreSnapshot,
    VisibilityEvent,
};
use crate::contract::team::TeamAdapter;

// ---------------------------------------------------------------------------
// identity and evidence (§3.3, §3.5.2, §5.1)
// ---------------------------------------------------------------------------

/// The dedupe key `(vendor, accountId, vendorSessionId)` [R2 Q7.8]: unique
/// among live and persisted sessions; at most one **live** uid per key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct IdentityKey {
    pub vendor: Vendor,
    pub account_id: Option<AccountId>,
    pub vendor_session_id: String,
}

/// What a producer knows about whose evidence this is (§3.3.2); World resolves
/// it (rules 1–5, first match wins).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IdentityHint {
    pub vendor: Option<Vendor>,
    pub account_id: Option<AccountId>,
    pub vendor_session_id: Option<String>,
    pub process: Option<ProcessKey>,
    /// Set by discovery on every hint whose `(pid, start)` is a live row of this
    /// instance's PID registry or matches a `planned` row (PM-22).
    pub gilde_spawn_id: Option<String>,
}

/// Session metadata a source learned (all optional; absent = unknown). Titles
/// and first prompts are **[content]**: memory only.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SessionMetaFacts {
    pub cwd: Option<AbsPath>,
    pub started_at: Option<EpochMs>,
    pub model: Option<String>,
    pub permission_mode: Option<PermissionMode>,
    pub origin: Option<Origin>,
    pub owner: Option<OwnerApp>,
    pub binary: Option<BinaryRef>,
    pub claude_version: Option<String>,
    pub transcript_path: Option<AbsPath>,
    /// **[content]**
    pub title: Option<String>,
    pub teammate: Option<TeammateLabels>,
    pub memory_bytes: Option<u64>,
}

/// The allowlisted teammate label flags read from argv (§5.2.2) [F-03].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TeammateLabels {
    pub agent_id: Option<String>,
    pub agent_name: Option<String>,
    pub team_name: Option<String>,
    pub agent_color: Option<String>,
    pub parent_session_id: Option<String>,
}

/// The facts of one source (S1–S6 of §3.5.1, plus Gilde's own and metadata).
#[derive(Debug, Clone, PartialEq)]
pub enum SourceFacts {
    Gilde(GildeFacts),
    Process(ProcessFacts),
    Stream(StreamFacts),
    Registry(RegistryFacts),
    Transcript(TranscriptFacts),
    Team(TeamFileFacts),
    Pty(PtyFacts),
    Meta(Box<SessionMetaFacts>),
    /// The source became unreadable (`permissionDenied`, §5.8): keep the last
    /// facts, mark them `Likely`.
    SourceDenied(bool),
    /// Spend for the local day (§6.4) [A10.4].
    Usage(UsageFacts),
}

/// Which producer measured a [`UsageFacts`] report; World shows the
/// highest-priority source that has reported: `Stream` > `Turns` >
/// `Transcript` [A10.4].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum UsageSource {
    /// S1 stream-json of a session Gilde runs (T05).
    Stream,
    /// A turn-based session's turn results (T08).
    Turns,
    /// Transcript rows of a watched session (T04).
    Transcript,
}

/// A producer's **running total** for the local calendar day `today.day`
/// (never a delta), each usage object counted once per `message.id` (P32);
/// `cost_micros` only when the vendor reports cost [A10.4].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageFacts {
    pub source: UsageSource,
    pub today: UsageToday,
}

/// One piece of evidence; producers never mutate World, they send these.
#[derive(Debug, Clone, PartialEq)]
pub struct EvidenceUpdate {
    pub hint: IdentityHint,
    pub facts: SourceFacts,
    /// Wall clock of the observation.
    pub at: EpochMs,
}

/// Project/location identity from §5.6 (no git spawn in discovery).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocationFacts {
    pub project_id: ProjectId,
    pub project_root: AbsPath,
    pub git: bool,
    pub location_id: LocationId,
    pub location_path: AbsPath,
    pub worktree: bool,
    pub branch: Option<String>,
}

/// The input side of World (bounded, non-blocking; never fails visibly).
pub trait EvidenceSink: Send + Sync {
    fn evidence(&self, update: EvidenceUpdate);
    fn location(&self, facts: LocationFacts);
    fn source_status(&self, status: SourceStatus);
    fn account(&self, account: Account);
    /// The full compat list after any change (the `Compat{list}` store op).
    /// The default is a no-op only so that fakes keep compiling; World (T02)
    /// overrides it (T-world-compatList) [A10.1].
    fn compat(&self, list: Vec<ClaudeCompat>) {
        let _ = list;
    }
    /// The optional integrations after a change, e.g. orchestrator presence
    /// (§5.10; the `Integrations{state}` store op). The default is a no-op only
    /// so that fakes keep compiling; World (T02) overrides it
    /// (T-world-integrations) [A11.6].
    fn integrations(&self, state: IntegrationsState) {
        let _ = state;
    }
    /// Lifecycle facts that become store ops (§2.4, §9.8, §10): World emits
    /// `Boot`, `Notices`, `Quit`, and `Upsert`/`Remove` of `Leftover`
    /// entities from them. The default is a no-op only so that fakes keep
    /// compiling; World (T02) overrides it (T-world-lifecycleOps) [A12.1].
    fn lifecycle(&self, op: LifecycleOp) {
        let _ = op;
    }
    /// The notification pause changed (`None` = resumed; see
    /// `attention::PAUSED_UNTIL_RESUME`): World sets `AttentionQueue.paused_until`
    /// and emits the `Queue` op. The default is a no-op only so that fakes keep
    /// compiling; World (T02) overrides it (T-world-notifyPaused) [A16.1].
    fn notify_paused(&self, until: Option<EpochMs>) {
        let _ = until;
    }
}

/// The lifecycle store inputs of T09 (boot, notices, quit progress, leftover
/// processes) [A12.1]. `Notices` is the full current list (not a delta);
/// `LeftoverRemove` names the record by its `ProcessKey`.
#[derive(Debug, Clone, PartialEq)]
pub enum LifecycleOp {
    Boot(BootStatus),
    Notices(Vec<Notice>),
    Quit { stopped: u32, total: u32, phase: QuitPhase },
    LeftoverUpsert(Box<ProcessRecord>),
    LeftoverRemove(ProcessKey),
}

/// Where World pushes store patches for one subscriber (the `store_subscribe`
/// command wraps the Tauri channel). `false` = the subscriber is gone.
pub trait StorePatchSink: Send + Sync {
    fn send(&self, patch: StorePatch) -> bool;
}

/// Held from the liveness re-check through spawn until the process is
/// registered (§6.0 *Identity lock*).
pub type IdentityLock = tokio::sync::OwnedMutexGuard<()>;

/// The World facade (T02): `apply`, `subscribe`, `snapshot`, `identity_lock`, queue.
#[async_trait]
pub trait WorldApi: Send + Sync {
    /// The evidence input (the "apply" side).
    fn sink(&self) -> Arc<dyn EvidenceSink>;
    /// Atomic under the World lock: the snapshot, then every patch with
    /// `fromRev ≥ rev` to `sink`; a second call replaces the first subscriber.
    fn subscribe(&self, sink: Box<dyn StorePatchSink>) -> StoreSnapshot;
    fn snapshot(&self) -> StoreSnapshot;
    fn session(&self, uid: &SessionUid) -> Option<crate::contract::session::Session>;
    fn queue(&self) -> AttentionQueue;
    async fn identity_lock(&self, key: IdentityKey) -> IdentityLock;
    /// `session_mark_seen` (marks; never persisted content).
    fn mark_seen(&self, uid: &SessionUid) -> Result<(), IpcError>;
    /// `session_dismiss`: `REFUSED{notInState}`.
    fn dismiss(&self, uid: &SessionUid, what: DismissWhat) -> Result<(), IpcError>;
    /// `queue_later`: 30 min on the uptime clock; `NOT_FOUND{queueItem}`.
    fn queue_later(&self, uid: &SessionUid) -> Result<EpochMs, IpcError>;
    fn register_team_adapter(&self, adapter: Arc<dyn TeamAdapter>);
    /// The absolute transcript path and vendor of a session, from its
    /// `SessionMetaFacts.transcript_path`; `None` when unknown. Never derive it
    /// from the display string of `SessionDetails.transcript_path`. The
    /// default exists only so that fakes keep compiling; World (T02)
    /// overrides it (T-world-transcriptPath) [A10.3].
    fn transcript(&self, uid: &SessionUid) -> Option<(AbsPath, Vendor)> {
        let _ = uid;
        None
    }
}

// ---------------------------------------------------------------------------
// compat (§3.10) — T21
// ---------------------------------------------------------------------------

#[async_trait]
pub trait CompatApi: Send + Sync {
    /// Every record (the `Compat{list}` store op, `app_bootstrap.compat`).
    fn list(&self) -> Vec<ClaudeCompat>;
    fn lookup(&self, version: &str) -> Option<ClaudeCompat>;
    /// The record for a binary resolved at spawn: creates it for a new version
    /// and waits up to 1 s for its static canary (timeout = `Unknown`).
    async fn for_binary(&self, binary: &BinaryRef) -> ClaudeCompat;
    /// A provisional-clause flag with forward inheritance (§3.10 *Features*).
    fn feature(&self, version: &str, flag: &str) -> bool;
    /// A per-build app feature (`app-features.json`).
    fn app_feature(&self, flag: &str) -> bool;
    /// The New-session / team notices for a version.
    fn notices(&self, version: &str) -> Vec<Notice>;
    /// Schedule a background canary for a changed binary (B10).
    fn schedule_canary(&self, binary: &BinaryRef);
}

/// Runtime compat evidence (§3.10 inputs 3–4).
pub trait CompatSink: Send + Sync {
    fn handshake(&self, version: &str, ok: bool, note: Option<String>);
    fn deviation(&self, version: &str, capability: CompatCapability, note: String);
    fn note(&self, version: &str, note: String);
}

// ---------------------------------------------------------------------------
// supervision (§2.1, §3.8, §9.3, §9.8) — T09
// ---------------------------------------------------------------------------

/// A process to plan (write-before-spawn).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnRequest {
    pub kind: ProcKind,
    /// The checked realpath; refused unless in the live allowlist (§9.3).
    pub argv0: AbsPath,
    pub args: Vec<String>,
    pub cwd: Option<AbsPath>,
    /// Env key names only.
    pub env_keys: Vec<String>,
    pub parent_uid: Option<SessionUid>,
    /// The sidecar generation (≥ 1) the process belongs to: required for
    /// `Sidecar` and `Turn` kinds, `None` otherwise. Stored in the registry
    /// row so the dead-generation sweep finds orphans [A13.1].
    pub generation: Option<i64>,
    /// The sidecar turn id a `Turn` child serves (≤ 128 bytes, no control
    /// characters): required for `Turn`, `None` otherwise. Stored as the
    /// row's `run_id` [A13.1].
    pub turn_id: Option<String>,
}

/// A planned registry row: the id and the `GILDE_SPAWN_ID` value to stamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnTicket {
    pub spawn_id: String,
    pub env_marker: String,
}

/// How to signal (identity fence first, ±1 s start time + argv signature).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalPlan {
    /// Fenced SIGTERM to the group, wait `grace_ms`.
    Term { grace_ms: u32 },
    /// SIGHUP first (PTYs, panes), then fenced SIGTERM after `grace_ms`.
    HupThenTerm { grace_ms: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignalOutcome {
    pub exited: bool,
    /// Still alive after the plan; never SIGKILLed if a vendor CLI (§9.8).
    pub survivor: bool,
}

/// One process of a libproc scan (§5.2.2): never raw argv or env values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcInfo {
    pub key: ProcessKey,
    pub pid: i32,
    pub ppid: i32,
    pub uid: u32,
    pub start_sec: i64,
    pub exec_path: Option<String>,
    /// §9.7 redacted form only.
    pub argv_redacted: Vec<String>,
    pub labels: TeammateLabels,
    pub cwd: Option<String>,
    pub rss_bytes: u64,
    pub cpu_ns: u64,
    /// Env-name markers (`CCO_TEAM_MEMBER=`, `GILDE_APP_INSTANCE=`); `None` = unreadable.
    pub cco_team_member: Option<bool>,
    pub gilde_app_instance: Option<bool>,
    /// The vendor-CLI classifier of §5.2.2 (also the §9.8 signal policy).
    pub vendor: Option<Vendor>,
}

/// The in-process scan T03 produces and T09 (procscan, watchdog, health) consumes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProcSnapshot {
    pub taken_at: EpochMs,
    pub procs: Vec<ProcInfo>,
}

#[async_trait]
pub trait SupervisorApi: Send + Sync {
    /// Waits up to 5 s for the boot sweep (B6), else `NOT_READY{bootSweep}`.
    async fn spawn_gate(&self) -> Result<(), IpcError>;
    /// A durable `planned` row before the process exists; `DISK_FULL` in
    /// memory-only mode; `REFUSED{notAllowlisted}`.
    async fn plan_spawn(&self, req: SpawnRequest) -> Result<SpawnTicket, IpcError>;
    async fn mark_running(&self, ticket: &SpawnTicket, pid: i32) -> Result<(), IpcError>;
    async fn mark_exited(&self, ticket: &SpawnTicket, exit: ExitInfo);
    /// Identity-fenced signal; never SIGKILL a vendor CLI (§9.8).
    async fn signal(&self, target: &ProcessKey, plan: SignalPlan) -> Result<SignalOutcome, IpcError>;
    /// The §5.2.2 classifier.
    fn is_vendor_cli(&self, exec_path: &str) -> bool;
    /// The one redaction helper (§9.7, `supervise/redact.rs`).
    fn redact_argv(&self, argv: &[String]) -> Vec<String>;
    /// `Some(reason)` while in memory-only mode (§2.4).
    fn memory_only(&self) -> Option<String>;
    /// The latest scan (T03's snapshot as the supervisor sees it).
    fn last_scan(&self) -> Option<ProcSnapshot>;
}

// ---------------------------------------------------------------------------
// PTY (§6.2) — T06
// ---------------------------------------------------------------------------

/// A PTY to open: a vendor TUI, an attach, or (PaneHost only) `/bin/sh -c`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtyOpenSpec {
    /// `Pty`, `Attach` or `Pane`.
    pub kind: ProcKind,
    pub argv0: AbsPath,
    pub args: Vec<String>,
    pub cwd: AbsPath,
    /// The composed env (§6.0); values never logged.
    pub env: Vec<(String, String)>,
    pub cols: u16,
    pub rows: u16,
    pub parent_uid: Option<SessionUid>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtyHandle {
    pub pty_id: PtyId,
    pub pid: i32,
    pub ticket: SpawnTicket,
}

/// Notified once when a PTY's process exits.
pub trait PtyExitSink: Send + Sync {
    fn exited(&self, pty: &PtyId, exit: ExitInfo, at: EpochMs);
}

#[async_trait]
pub trait PtyApi: Send + Sync {
    async fn open(&self, spec: PtyOpenSpec, on_exit: Arc<dyn PtyExitSink>) -> Result<PtyHandle, IpcError>;
    async fn write(&self, pty: &PtyId, bytes: &[u8]) -> Result<(), IpcError>;
    /// Bracketed paste (the end marker split out of content, §6.2.3), CR iff `submit`.
    async fn paste(&self, pty: &PtyId, text: &str, submit: bool) -> Result<(), IpcError>;
    async fn resize(&self, pty: &PtyId, cols: u16, rows: u16) -> Result<(), IpcError>;
    async fn close(&self, pty: &PtyId, plan: SignalPlan) -> Result<SignalOutcome, IpcError>;
    /// The parsed screen prompt, if any (§6.2.4).
    fn screen_prompt(&self, pty: &PtyId) -> Option<ScreenPromptFacts>;
    /// Under the PTY write lock, re-parse and act only if `prompt_rev` matches
    /// (else `STALE_PROMPT`).
    async fn screen_action(&self, pty: &PtyId, prompt_rev: &str, action: &ScreenPromptAction) -> Result<ScreenKeyOutcome, IpcError>;
    fn facts(&self, pty: &PtyId) -> Option<PtyFacts>;
}

// ---------------------------------------------------------------------------
// PaneHost (§7.4) — T07
// ---------------------------------------------------------------------------

/// The env additions of a hosted lead (§7.4.1). Contains the per-lead token:
/// never logged (`Debug` redacts values).
#[derive(Clone, PartialEq, Eq)]
pub struct LeadEnv {
    /// Variables to set (incl. `PATH` with the shim dir first, `GILDE_PANE_HOST`,
    /// `GILDE_PANE_HOST_TOKEN`, `GILDE_REAL_TMUX`, `CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1`).
    pub set: Vec<(String, String)>,
    /// Variables that must be absent (`TMUX`, `TMUX_PANE`, `ITERM_*`, `LC_TERMINAL*`, …).
    pub remove: Vec<String>,
}

impl std::fmt::Debug for LeadEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let keys: Vec<&str> = self.set.iter().map(|(k, _)| k.as_str()).collect();
        f.debug_struct("LeadEnv")
            .field("set_keys", &keys)
            .field("remove", &self.remove)
            .finish()
    }
}

#[async_trait]
pub trait PaneHostApi: Send + Sync {
    /// Mint the token and the env for a lead about to be spawned.
    fn lead_env(&self, lead: &SessionUid) -> Result<LeadEnv, IpcError>;
    fn lead_started(&self, lead: &SessionUid, pid: i32);
    /// Every pane of that server is closed (§7.4.7).
    async fn lead_exited(&self, lead: &SessionUid);
    /// `team_send` to a hosted pane member.
    async fn send_to_member(&self, team: &TeamKey, member: &MemberKey, text: &str) -> Result<(), IpcError>;
    /// `team_member_close`: SIGHUP, then fenced SIGTERM after 2 s.
    async fn close_member_pane(&self, team: &TeamKey, member: &MemberKey) -> Result<(), IpcError>;
}

// ---------------------------------------------------------------------------
// sidecar (§4.5, §6.3) — T08
// ---------------------------------------------------------------------------

/// Receives one turn's events in `seq` order; exactly one `Done` or `Error`.
pub trait TurnEventSink: Send + Sync {
    fn event(&self, seq: u64, event: TurnEvent);
}

#[async_trait]
pub trait SidecarApi: Send + Sync {
    /// Lazy start (B12): `NOT_READY{sidecarStarting}` until up, `SIDECAR_DOWN`
    /// after repeated deaths, `VERSION_MISMATCH` on a build mismatch.
    async fn ensure_started(&self) -> Result<(), IpcError>;
    async fn auth_status(&self, p: ProbeParams) -> Result<AuthStatus, IpcError>;
    async fn models(&self, p: ProbeParams) -> Result<Vec<ModelInfo>, IpcError>;
    async fn quota(&self, p: ProbeParams) -> Result<Option<Quota>, IpcError>;
    async fn turn_start(&self, p: TurnStartParams, sink: Arc<dyn TurnEventSink>) -> Result<(), IpcError>;
    async fn turn_cancel(&self, turn_id: &str) -> Result<bool, IpcError>;
}

// ---------------------------------------------------------------------------
// git and IDE (§4.2.5, §9.4) — T11
// ---------------------------------------------------------------------------

#[async_trait]
pub trait GitApi: Send + Sync {
    async fn changes(&self, req: &ChangesGetReq) -> Result<ChangesView, IpcError>;
    async fn commit_preview(&self, req: &GitCommitPreviewReq) -> Result<GitCommitPreviewRes, IpcError>;
    async fn commit(&self, req: &GitCommitReq) -> Result<GitCommitRes, IpcError>;
    async fn merge_preview(&self, req: &GitMergePreviewReq) -> Result<GitMergePreviewRes, IpcError>;
    async fn merge(&self, req: &GitMergeReq) -> Result<GitMergeRes, IpcError>;
    async fn discard_preview(&self, req: &WorktreeDiscardPreviewReq) -> Result<WorktreeDiscardPreviewRes, IpcError>;
    async fn discard(&self, req: &WorktreeDiscardReq) -> Result<(), IpcError>;
    /// A new worktree for `session_start{NewWorktree}` (existing gates, §4.2.2).
    async fn create_worktree(&self, project: &ProjectId, slug: &str, base: Option<&str>) -> Result<LocationFacts, IpcError>;
}

#[async_trait]
pub trait IdeApi: Send + Sync {
    async fn open(&self, req: &IdeOpenReq) -> Result<(), IpcError>;
    async fn reveal(&self, req: &IdeRevealReq) -> Result<(), IpcError>;
    fn editors(&self) -> IdeEditorsRes;
    /// `link_open` after validation; `Ok(false)` if a native confirmation was declined.
    async fn link_open(&self, url: &str) -> Result<bool, IpcError>;
    /// The LP-24 candidate (bundle CLI with the project root), used by T12.
    async fn focus_editor_window(&self, editor: EditorKind, project_root: &AbsPath) -> Result<bool, IpcError>;
}

// ---------------------------------------------------------------------------
// notifications and macOS (§8) — T12
// ---------------------------------------------------------------------------

#[async_trait]
pub trait NotifyApi: Send + Sync {
    /// `app_view_state` [F-04].
    fn view_state(&self, state: ViewState);
    /// `app_input_activity` (notification rule 5).
    fn input_activity(&self);
    /// Every queue change (dock badge = `need_you`, notification policy §8.4–§8.5).
    fn queue_changed(&self, queue: &AttentionQueue);
    fn visibility(&self) -> VisibilityEvent;
    fn pause(&self, until: PauseUntil) -> Option<EpochMs>;
    fn resume(&self);
    async fn permission_request(&self) -> bool;
    async fn test(&self) -> bool;
}

// ---------------------------------------------------------------------------
// sessions (§6.0) — T10
// ---------------------------------------------------------------------------

/// The sessions manager as other tasks and the commands call it.
#[async_trait]
pub trait SessionActorApi: Send + Sync {
    async fn options(&self, req: &NewsessionOptionsReq) -> Result<NewSessionOptions, IpcError>;
    async fn start(&self, req: SessionStartReq) -> Result<SessionStartRes, IpcError>;
    async fn continue_session(&self, req: SessionContinueReq) -> Result<SessionContinueRes, IpcError>;
    async fn retry_as_terminal(&self, req: SessionRetryAsTerminalReq) -> Result<SessionRetryAsTerminalRes, IpcError>;
    async fn stop_preview(&self, uid: &SessionUid) -> Result<SessionStopPreviewRes, IpcError>;
    async fn stop(&self, req: SessionStopReq) -> Result<SessionStopRes, IpcError>;
    async fn send(&self, req: TurnSendReq) -> Result<TurnSendRes, IpcError>;
    async fn cancel_queued(&self, req: TurnCancelQueuedReq) -> Result<TurnCancelQueuedRes, IpcError>;
    async fn interrupt(&self, req: TurnInterruptReq) -> Result<TurnInterruptRes, IpcError>;
    async fn force_stop(&self, uid: &SessionUid) -> Result<bool, IpcError>;
    async fn retry(&self, req: TurnRetryReq) -> Result<TurnRetryRes, IpcError>;
    async fn set_model(&self, uid: &SessionUid, model: Option<String>) -> Result<AppliesFrom, IpcError>;
    async fn set_permission_mode(&self, uid: &SessionUid, mode: PermissionMode) -> Result<(), IpcError>;
    async fn answer(&self, req: InteractionAnswerReq) -> Result<InteractionAnswerRes, IpcError>;
    async fn screen_key(&self, req: ScreenPromptKeyReq) -> Result<ScreenPromptKeyRes, IpcError>;
    async fn detail(&self, req: &InteractionDetailReq) -> Result<InteractionDetailRes, IpcError>;
    async fn models(&self, uid: &SessionUid) -> Result<SessionModelsRes, IpcError>;
    async fn technical(&self, uid: &SessionUid) -> Result<SessionTechnicalRes, IpcError>;
    /// `account_sign_in` (T08): a Terminal-mode sign-in session.
    async fn start_sign_in(&self, account: &AccountId) -> Result<AccountSignInRes, IpcError>;
    /// `continue_all` (T09): items in quit order, ≤ 3 in flight, §6.5 rule 1.
    async fn continue_all(&self, record: &QuitRecordId, uids: Option<Vec<SessionUid>>) -> Result<ContinueAllRes, IpcError>;
    /// The quit sweep (T09, §10.2): the driver's stop sequence with quit bounds.
    async fn stop_for_quit(&self, uid: &SessionUid) -> Result<StopOutcome, IpcError>;
}

// ---------------------------------------------------------------------------
// content hub (§4.2.4, §6.4) — T04
// ---------------------------------------------------------------------------

/// A content change for one session's conversation views.
#[derive(Debug, Clone, PartialEq)]
pub enum ContentUpdate {
    Reset { blocks: Vec<Block>, has_older: bool, reason: ResetReason },
    Appended(Block),
    Updated { block_id: crate::contract::ids::BlockId, patch: BlockPatch },
    Source { source: ConvoSourceKind, note: Option<Notice> },
    Ended,
}

/// The ContentHub (≤ 8 subscriptions, eviction, load older) as producers see it.
pub trait ContentHubApi: Send + Sync {
    fn publish(&self, uid: &SessionUid, update: ContentUpdate);
    /// With no subscriber, deltas are folded into block state without events.
    fn has_subscribers(&self, uid: &SessionUid) -> bool;
}

// ---------------------------------------------------------------------------
// the per-session driver (§6.0)
// ---------------------------------------------------------------------------

/// A message for `turn_send`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingMessage {
    pub client_msg_id: ClientMsgId,
    pub text: String,
    pub priority: SendPriority,
    /// Conversation only; another driver refuses a message that has any
    /// (`UNSUPPORTED{images}`).
    pub images: Vec<OutgoingImage>,
}

/// The answer outcome of a structured card (§4.2.3) [C12].
pub use crate::contract::ipc::AnswerOutcome;

/// The result of a stop (`session_stop`, quit).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopOutcome {
    pub stopped: bool,
    pub survivors: u32,
}

/// One driver per Gilde-owned session, owned by its session actor (§6.0).
/// Unsupported operations return `UNSUPPORTED{cap}`.
#[async_trait]
pub trait SessionDriver: Send {
    fn mode(&self) -> ControlMode;
    /// `turn_send`.
    async fn send(&mut self, msg: OutgoingMessage) -> Result<SendDelivery, IpcError>;
    /// `turn_cancel_queued`.
    async fn cancel_queued(&mut self, id: &ClientMsgId) -> Result<bool, IpcError>;
    /// Structured cards (C12 outcomes).
    async fn answer(&mut self, req: &RequestId, d: Decision) -> Result<AnswerOutcome, IpcError>;
    /// Screen prompts (§6.2.4); `aborted` for a TrustFolder that did not complete.
    async fn screen_key(&mut self, req: &RequestId, rev: &str, key: ScreenPromptAction) -> Result<ScreenKeyOutcome, IpcError>;
    async fn interrupt(&mut self, discard_queued: bool) -> Result<InterruptOutcome, IpcError>;
    async fn force_stop(&mut self) -> Result<bool, IpcError>;
    async fn set_model(&mut self, m: Option<String>) -> Result<AppliesFrom, IpcError>;
    async fn set_permission_mode(&mut self, m: PermissionMode) -> Result<(), IpcError>;
    /// Session stop / quit.
    async fn stop(&mut self, why: StopCause) -> Result<StopOutcome, IpcError>;
}
