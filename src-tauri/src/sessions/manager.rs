//! The sessions manager (contract §6.0, §6.5): one actor per Gilde-owned
//! session, the start / continue / stop / Continue-all paths, the identity
//! lock and the liveness re-check, and the [`SessionActorApi`] that the
//! commands, T08 (sign-in) and T09 (`continue_all`, quit) call.
//!
//! Owner: T10.
//!
//! Everything the manager talks to is a trait ("port") so that the session
//! logic is testable with fakes; the production adapters live next to their
//! consumers (`start.rs`: transports, binaries, env; `options.rs`: accounts,
//! prefs; `continuation.rs`: liveness, quit items) and the composition root
//! (T09b, A10.6) builds them — see the module docs of [`crate::sessions`].

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use parking_lot::Mutex;

use crate::contract::account::{Account, Quota};
use crate::contract::activity::{EvidenceLine, OpenToolUse, StopCause};
use crate::contract::common::{AbsPath, EpochMs, PermissionMode, Vendor};
use crate::contract::compat::ClaudeCompat;
use crate::contract::error::{ErrorCode, IpcError};
use crate::contract::ids::{AccountId, ProjectId, PtyId, QuitRecordId, SessionUid};
use crate::contract::ipc::{
    AccountSignInRes, AppliesFrom, ContinueAllRes, InteractionAnswerReq, InteractionAnswerRes,
    InteractionDetailReq, InteractionDetailRes, NewSessionOptions, NewsessionOptionsReq,
    ScreenPromptKeyReq, ScreenPromptKeyRes, SessionContinueReq, SessionContinueRes,
    SessionModelsRes, SessionRetryAsTerminalReq, SessionRetryAsTerminalRes, SessionStartReq,
    SessionStartRes, SessionStopPreviewRes, SessionStopReq, SessionStopRes, SessionTechnicalRes,
    TurnCancelQueuedReq, TurnCancelQueuedRes, TurnInterruptReq, TurnInterruptRes, TurnRetryReq,
    TurnRetryRes, TurnSendReq, TurnSendRes, VendorOption,
};
use crate::contract::services::{
    CompatApi, EvidenceSink, GitApi, PaneHostApi, PtyExitSink, PtyHandle, PtyOpenSpec,
    SessionActorApi, SidecarApi, StopOutcome, SupervisorApi, WorldApi,
};
use crate::contract::session::{BinaryRef, ControlMode, QueuedMessage, Transport};
use crate::contract::sidecar::AccountEnv;
use crate::convo::driver::{ConvoHooks, ConvoSpec};
use crate::pty::ReadyOutcome;
use crate::store::models::QuitItemRow;
use crate::turns::driver::TurnSessionSpec;

use super::actor::{ActorDriver, ActorHandle};

// ---------------------------------------------------------------------------
// bounds
// ---------------------------------------------------------------------------

/// `clientRequestId` is remembered this long (§4.1 rule 4), on the
/// monotonic clock (A48.4).
pub const IDEMPOTENCY_TTL_MS: u64 = 60_000;
/// A `confirmToken` is valid this long (§4.1 rule 4), on the monotonic clock
/// (A48.4).
pub const CONFIRM_TTL_MS: u64 = 60_000;
/// An identical `screen_prompt_key` joins a running one for at most this long
/// (A34.2, A38.6), on the monotonic clock (A48.4); a finished one is
/// remembered by the PTY, per appearance of the prompt
/// (`pty::manager::TypedAnswers`).
pub const SCREEN_KEY_JOIN_MS: u64 = crate::pty::manager::SCREEN_REPEAT_MS as u64;
/// Ended actors kept for late answers (`withdrawn`) before the oldest go.
pub const ENDED_ACTORS_KEPT: usize = 256;

/// The bounds of §6.2.2, §6.5 and §4.2.2. Tests shorten them; no functional
/// test asserts a wall-clock budget (A10.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTiming {
    /// §6.2.2 Starting timeout (the readiness wait for a typed first prompt).
    pub ready_timeout: Duration,
    /// §6.5 rule 1: how long Continue waits for its own crash leftover.
    pub own_leftover_wait: Duration,
    /// How often a liveness or readiness wait re-checks.
    pub liveness_poll: Duration,
    /// §4.2.2: `newsession_options` answers with what is known within 1 s.
    pub options_wait: Duration,
    /// How long a start waits for agent detection before `NOT_READY{agentsNotDetected}`.
    pub detect_wait: Duration,
    /// `claude stop <id>` for a watched background job.
    pub claude_stop_wait: Duration,
    /// Fenced SIGTERM grace for a watched session (§9.8).
    pub watched_term_grace_ms: u32,
    /// §6.5 rule 4: Continue all keeps at most this many spawns in flight.
    pub continue_all_parallel: usize,
}

impl Default for SessionTiming {
    fn default() -> SessionTiming {
        SessionTiming {
            ready_timeout: Duration::from_secs(30),
            own_leftover_wait: Duration::from_secs(12),
            liveness_poll: Duration::from_millis(250),
            options_wait: Duration::from_secs(1),
            detect_wait: Duration::from_secs(10),
            claude_stop_wait: Duration::from_secs(10),
            watched_term_grace_ms: 2_000,
            continue_all_parallel: 3,
        }
    }
}

// ---------------------------------------------------------------------------
// ports
// ---------------------------------------------------------------------------

/// The PTY id, transport and hosted flag of a Gilde session (A18.1).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ControlFacts {
    pub pty: Option<PtyId>,
    pub transport: Option<Transport>,
    pub hosted_team: bool,
}

/// Driver-only Details facts (A19.1): each call fully replaces the previous.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DriverDetails {
    pub approval_channel_unconfirmed: Option<String>,
    pub version_note: Option<String>,
    pub lines: Vec<EvidenceLine>,
}

/// World's binding methods (A18.1, A18.3, A19.1, A20.1). T09b passes
/// `Arc<World>` (its `impl` lands with T02's merge).
pub trait SessionBinder: Send + Sync {
    /// `bind_spawn(spawn_id, Some(uid))` always returns `uid` (A20.1).
    fn bind_spawn(&self, spawn_id: &str, uid: Option<&SessionUid>, vendor: Vendor, account: Option<AccountId>) -> SessionUid;
    fn set_control_detail(&self, uid: &SessionUid, detail: ControlFacts);
    /// The full current list, oldest first; **[content]**, memory only.
    fn set_queued(&self, uid: &SessionUid, list: Vec<QueuedMessage>);
    fn set_details_extra(&self, uid: &SessionUid, extra: DriverDetails);
    /// A24.2: a copy's parent (World persists it as `session_meta.parent_uid`).
    /// The default records nothing.
    fn set_parent(&self, uid: &SessionUid, parent: &SessionUid) {
        let _ = (uid, parent);
    }
    /// A24.2: the parent World recorded.
    fn parent_of(&self, uid: &SessionUid) -> Option<SessionUid> {
        let _ = uid;
        None
    }
    /// A24.3 (F-05): the tool call the session's transcript ends in.
    fn open_tool_use(&self, uid: &SessionUid) -> Option<OpenToolUse> {
        let _ = uid;
        None
    }
    /// A35.1: the Conversation driver's report of the permission mode.
    fn set_permission_mode(&self, uid: &SessionUid, mode: Option<crate::contract::common::PermissionMode>) {
        let _ = (uid, mode);
    }
}

impl SessionBinder for crate::world::World {
    fn set_permission_mode(&self, uid: &SessionUid, mode: Option<crate::contract::common::PermissionMode>) {
        crate::world::World::set_permission_mode(self, uid, mode);
    }
    fn bind_spawn(&self, spawn_id: &str, uid: Option<&SessionUid>, vendor: Vendor, account: Option<AccountId>) -> SessionUid {
        crate::world::World::bind_spawn(self, spawn_id, uid, vendor, account)
    }

    fn set_control_detail(&self, uid: &SessionUid, detail: ControlFacts) {
        crate::world::World::set_control_detail(
            self,
            uid,
            crate::world::ControlDetail { pty: detail.pty, transport: detail.transport, hosted_team: detail.hosted_team },
        );
    }

    fn set_queued(&self, uid: &SessionUid, list: Vec<QueuedMessage>) {
        crate::world::World::set_queued(self, uid, list);
    }

    fn set_details_extra(&self, uid: &SessionUid, extra: DriverDetails) {
        crate::world::World::set_details_extra(
            self,
            uid,
            crate::world::DetailsExtra {
                approval_channel_unconfirmed: extra.approval_channel_unconfirmed,
                version_note: extra.version_note,
                lines: extra.lines,
            },
        );
    }

    fn set_parent(&self, uid: &SessionUid, parent: &SessionUid) {
        crate::world::World::set_parent(self, uid, parent.clone());
    }

    fn parent_of(&self, uid: &SessionUid) -> Option<SessionUid> {
        crate::world::World::parent_of(self, uid)
    }

    fn open_tool_use(&self, uid: &SessionUid) -> Option<OpenToolUse> {
        crate::world::World::open_tool_use(self, uid)
    }
}

/// What the Conversation transport needs per session besides its spec.
#[derive(Clone)]
pub struct ConvoWiring {
    /// The session's evidence sink (binds the spawn id on first sight).
    pub evidence: Arc<dyn EvidenceSink>,
    pub hooks: Arc<dyn ConvoHooks>,
}

/// A Terminal session that opened.
pub struct TerminalStarted {
    pub handle: PtyHandle,
    pub driver: Box<dyn ActorDriver>,
}

/// The three transports (§6.1–§6.3). Production: [`super::start::LiveTransports`].
#[async_trait]
pub trait Transports: Send + Sync {
    /// `ConvoDriver::spawn` (T05).
    async fn conversation(&self, spec: ConvoSpec, wiring: ConvoWiring) -> Result<Box<dyn ActorDriver>, IpcError>;
    /// `PtyApi::open` + `PtyDriver::new` (T06).
    async fn terminal(&self, spec: PtyOpenSpec, on_exit: Arc<dyn PtyExitSink>) -> Result<TerminalStarted, IpcError>;
    /// `PtyDriver::wait_ready` (§6.2.2).
    async fn terminal_ready(&self, pty: &PtyId, timeout: Duration) -> ReadyOutcome;
    /// `TurnDriver::new` (T08): a held session, no process until its first
    /// message. `content_uid` is where its content goes (it moves when a turn
    /// starts a new conversation, §6.3).
    fn turn_based(
        &self,
        spec: TurnSessionSpec,
        evidence: Arc<dyn EvidenceSink>,
        content_uid: Arc<Mutex<SessionUid>>,
    ) -> Box<dyn ActorDriver>;
}

/// A vendor binary resolved at spawn (§6.0 *Binary resolution*) [A1].
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedVendor {
    pub binary: BinaryRef,
    /// claude only: the record of **this** binary's version.
    pub compat: Option<ClaudeCompat>,
}

/// Detected and resolved vendor binaries. Production: [`super::start::AgentBinaries`].
#[async_trait]
pub trait Binaries: Send + Sync {
    /// What is installed (`newsession_options.vendors`); `NOT_READY{agentsNotDetected}`
    /// while detection has not answered.
    async fn installed(&self) -> Result<Vec<VendorOption>, IpcError>;
    /// Resolve at spawn: realpath, fingerprint, `--version`, compat;
    /// `UNSUPPORTED{vendorNotInstalled}` when absent.
    async fn resolve(&self, vendor: Vendor) -> Result<ResolvedVendor, IpcError>;
}

/// The accounts service (T08) as the manager uses it. Production:
/// [`super::options::ServiceAccounts`].
#[async_trait]
pub trait AccountsPort: Send + Sync {
    async fn all(&self) -> Vec<Account>;
    /// The env rule of an account (C8): `env_key: None` = the default login.
    async fn env_for(&self, id: &AccountId) -> Option<AccountEnv>;
    /// `rate_limit_event` / `get_usage` of a live Conversation session.
    fn observed_quota(&self, id: &AccountId, quota: Quota);
    /// `initialize.account.tokenSource == "none"` [F-29].
    fn signed_out(&self, id: &AccountId);
}

/// The §6.0 composed environment of a vendor child. Production:
/// [`super::start::SupervisorEnv`].
pub trait EnvSource: Send + Sync {
    /// The allowlist + passthrough + the account variable of C8 (unset for a
    /// default login) + `GILDE_SPAWN_ID = marker` + `GILDE_APP_INSTANCE`.
    fn compose(&self, vendor: Vendor, account: Option<&AccountEnv>, marker: &str) -> BTreeMap<String, String>;
    /// The `HOME` the children get.
    fn home(&self) -> Option<AbsPath>;
}

/// One live process that serves a vendor session id (§6.5 rule 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveProcess {
    pub pid: i32,
    pub start_sec: i64,
    /// A Claude registry `kind:"bg"` entry (a background job → Attach).
    pub background: bool,
}

/// The fresh liveness check of §6.5 rule 1. Production:
/// [`super::continuation::ScanLiveness`].
#[async_trait]
pub trait Liveness: Send + Sync {
    /// Every live process that serves `vendor_session_id` (registry scan of the
    /// watched config dirs + argv scan); never cached.
    async fn find(&self, vendor: Vendor, vendor_session_id: &str) -> Result<Vec<LiveProcess>, IpcError>;
    /// Whether `(pid, start_sec)` still runs.
    fn alive(&self, pid: i32, start_sec: i64) -> bool;
}

/// `project_prefs` (§3.9, §4.2.2): last vendor, account, model, mode, location kind.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProjectPrefs {
    pub last_vendor: Option<Vendor>,
    pub last_account_id: Option<AccountId>,
    pub last_model: Option<String>,
    pub last_control_mode: Option<ControlMode>,
    /// `here` | `newWorktree` | `existingWorktree`.
    pub last_location_kind: Option<String>,
}

/// Production: [`super::options::DbPrefs`].
#[async_trait]
pub trait PrefsStore: Send + Sync {
    async fn get(&self, project: &ProjectId) -> Option<ProjectPrefs>;
    async fn put(&self, project: &ProjectId, prefs: ProjectPrefs);
}

/// The items of a quit/crash record in quit order. Production:
/// [`super::continuation::DbQuitItems`].
#[async_trait]
pub trait QuitItems: Send + Sync {
    async fn items(&self, record: &QuitRecordId) -> Result<Vec<QuitItemRow>, IpcError>;
}

/// The wall clock (tests pin it): timestamps only, never a timer (A48.4).
pub type WallClock = Arc<dyn Fn() -> EpochMs + Send + Sync>;

/// The monotonic clock, in ms (A48.4): the timers that guard a user action —
/// the confirm-token TTL and the idempotency windows — run on it, so a
/// wall-clock jump neither stretches nor cuts them. Production: the uptime
/// clock (`discovery::now_up_ms`); tests pass their own
/// ([`SessionsManager::with_monotonic_clock`]).
pub type MonoClock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// Everything the manager consumes (contract §15.2 T10 row).
#[derive(Clone)]
pub struct SessionDeps {
    pub world: Arc<dyn WorldApi>,
    pub binder: Arc<dyn SessionBinder>,
    pub supervisor: Arc<dyn SupervisorApi>,
    pub transports: Arc<dyn Transports>,
    pub binaries: Arc<dyn Binaries>,
    pub accounts: Arc<dyn AccountsPort>,
    pub env: Arc<dyn EnvSource>,
    pub compat: Arc<dyn CompatApi>,
    pub liveness: Arc<dyn Liveness>,
    pub prefs: Arc<dyn PrefsStore>,
    pub quit_items: Arc<dyn QuitItems>,
    /// `None` = no worktree creation (a start with `NewWorktree` is refused).
    pub git: Option<Arc<dyn GitApi>>,
    /// `None` = no pane hosting: a team runs its teammates in-process.
    pub panehost: Option<Arc<dyn PaneHostApi>>,
    /// Model lists (`newsession_options`, `session_models` for turn-based).
    pub sidecar: Option<Arc<dyn SidecarApi>>,
    pub clock: WallClock,
    pub timing: SessionTiming,
}

// ---------------------------------------------------------------------------
// idempotency (§4.1 rule 4)
// ---------------------------------------------------------------------------

type Cell<T> = Arc<tokio::sync::OnceCell<Result<T, IpcError>>>;

/// A `clientRequestId`-keyed memory: a repeat within the TTL — also one that
/// arrives while the first is still running — gets the first call's outcome.
/// An [`Idem::in_flight`] memory only joins calls that are still running and
/// forgets each one as soon as it finished.
///
/// Times are monotonic-clock readings ([`MonoClock`], A48.4).
pub(crate) struct Idem<T: Clone + Send + Sync + 'static> {
    map: Mutex<HashMap<String, (u64, Cell<T>)>>,
    ttl: u64,
    in_flight_only: bool,
}

impl<T: Clone + Send + Sync + 'static> Idem<T> {
    pub(crate) fn new(ttl: u64) -> Idem<T> {
        Idem { map: Mutex::new(HashMap::new()), ttl, in_flight_only: false }
    }

    /// Joins an identical call only while the first is still running and at
    /// most `ttl` after it began (A38.6).
    pub(crate) fn in_flight(ttl: u64) -> Idem<T> {
        Idem { map: Mutex::new(HashMap::new()), ttl, in_flight_only: true }
    }

    pub(crate) async fn run<F, Fut>(&self, key: String, now: u64, f: F) -> Result<T, IpcError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, IpcError>>,
    {
        let cell = {
            let mut map = self.map.lock();
            let ttl = self.ttl;
            map.retain(|_, (at, _)| now.saturating_sub(*at) < ttl);
            map.entry(key.clone())
                .or_insert_with(|| (now, Arc::new(tokio::sync::OnceCell::new())))
                .1
                .clone()
        };
        let out = cell.get_or_init(f).await.clone();
        if self.in_flight_only {
            let mut map = self.map.lock();
            if map.get(&key).is_some_and(|(_, c)| Arc::ptr_eq(c, &cell)) {
                map.remove(&key);
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// rate classes (§4.1 rule 6)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    last: Instant,
}

/// A token bucket: `rate` tokens per second, at most `burst`; exceeding is
/// `BUSY{retryAfterMs}`.
fn take(bucket: &mut Bucket, rate: f64, burst: f64) -> Result<(), IpcError> {
    let now = Instant::now();
    let refill = now.duration_since(bucket.last).as_secs_f64() * rate;
    bucket.tokens = (bucket.tokens + refill).min(burst);
    bucket.last = now;
    if bucket.tokens >= 1.0 {
        bucket.tokens -= 1.0;
        return Ok(());
    }
    let wait_ms = ((1.0 - bucket.tokens) / rate * 1000.0).ceil().max(1.0) as u64;
    Err(IpcError::busy(wait_ms))
}

/// The rate classes of this manager's commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Rate {
    /// 5 per 10 s: `session_start`, `session_continue`, `session_retry_as_terminal`.
    Spawn,
    /// 10/s per session: `turn_*`, `interaction_answer`, `screen_prompt_key`.
    Turn,
    /// 20/s: `interaction_detail`, `session_technical`.
    Query,
    /// 10/s per command.
    Default(&'static str),
}

pub(crate) struct Rates {
    spawn: Mutex<Bucket>,
    query: Mutex<Bucket>,
    turn: Mutex<HashMap<SessionUid, Bucket>>,
    default: Mutex<HashMap<&'static str, Bucket>>,
}

impl Rates {
    fn new() -> Rates {
        let now = Instant::now();
        Rates {
            spawn: Mutex::new(Bucket { tokens: 5.0, last: now }),
            query: Mutex::new(Bucket { tokens: 20.0, last: now }),
            turn: Mutex::new(HashMap::new()),
            default: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn check(&self, rate: Rate, uid: Option<&SessionUid>) -> Result<(), IpcError> {
        match rate {
            Rate::Spawn => take(&mut self.spawn.lock(), 0.5, 5.0),
            Rate::Query => take(&mut self.query.lock(), 20.0, 20.0),
            Rate::Turn => {
                let Some(uid) = uid else { return Ok(()) };
                let mut map = self.turn.lock();
                if map.len() > 4_096 {
                    map.clear();
                }
                let b = map.entry(uid.clone()).or_insert(Bucket { tokens: 10.0, last: Instant::now() });
                take(b, 10.0, 10.0)
            }
            Rate::Default(name) => {
                let mut map = self.default.lock();
                let b = map.entry(name).or_insert(Bucket { tokens: 10.0, last: Instant::now() });
                take(b, 10.0, 10.0)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// confirm tokens (session_stop_preview → session_stop)
// ---------------------------------------------------------------------------

/// A previewed stop: single use, 60 s, bound to a digest of the previewed state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingConfirm {
    pub uid: SessionUid,
    pub digest: String,
    /// When it was issued, on the monotonic clock (A48.4).
    pub at: u64,
}

impl PendingConfirm {
    /// Still valid at monotonic time `now` (`CONFIRM_TTL_MS`).
    pub(crate) fn fresh(&self, now: u64) -> bool {
        now.saturating_sub(self.at) < CONFIRM_TTL_MS
    }
}

// ---------------------------------------------------------------------------
// the manager
// ---------------------------------------------------------------------------

/// One per app. Cheap to share through its `Arc`.
pub struct SessionsManager {
    pub(crate) deps: SessionDeps,
    pub(crate) actors: Mutex<HashMap<SessionUid, Arc<ActorHandle>>>,
    /// Ended actors, oldest first (bounded by [`ENDED_ACTORS_KEPT`]).
    pub(crate) ended: Mutex<Vec<SessionUid>>,
    pub(crate) starts: Idem<SessionUid>,
    /// `screen_prompt_key` calls still running, by `requestId` + `promptRev` +
    /// the action (§4.1 rule 4, A34.2, A38.6).
    pub(crate) screens: Idem<crate::contract::ipc::ScreenKeyOutcome>,
    pub(crate) confirms: Mutex<HashMap<String, PendingConfirm>>,
    pub(crate) rates: Rates,
    /// The live model list last seen per vendor (`newsession_options`).
    pub(crate) models_seen: Mutex<HashMap<Vendor, Vec<String>>>,
    /// The timers' clock (A48.4).
    pub(crate) mono: MonoClock,
    pub(crate) me: Weak<SessionsManager>,
}

impl std::fmt::Debug for SessionsManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionsManager").field("actors", &self.actors.lock().len()).finish_non_exhaustive()
    }
}

impl SessionsManager {
    pub fn new(deps: SessionDeps) -> Arc<SessionsManager> {
        SessionsManager::with_monotonic_clock(deps, Arc::new(crate::discovery::now_up_ms))
    }

    /// As [`SessionsManager::new`] with the monotonic clock of the timers
    /// (tests pin it, A48.4).
    pub fn with_monotonic_clock(deps: SessionDeps, mono: MonoClock) -> Arc<SessionsManager> {
        Arc::new_cyclic(|me| SessionsManager {
            deps,
            actors: Mutex::new(HashMap::new()),
            ended: Mutex::new(Vec::new()),
            starts: Idem::new(IDEMPOTENCY_TTL_MS),
            screens: Idem::in_flight(SCREEN_KEY_JOIN_MS),
            confirms: Mutex::new(HashMap::new()),
            rates: Rates::new(),
            models_seen: Mutex::new(HashMap::new()),
            mono,
            me: me.clone(),
        })
    }

    pub fn deps(&self) -> &SessionDeps {
        &self.deps
    }

    pub(crate) fn now(&self) -> EpochMs {
        (self.deps.clock)()
    }

    /// The timers' clock reading (A48.4).
    pub(crate) fn now_mono(&self) -> u64 {
        (self.mono)()
    }

    /// The actor of a uid (live or ended in this run).
    pub fn actor(&self, uid: &SessionUid) -> Option<Arc<ActorHandle>> {
        self.actors.lock().get(uid).cloned()
    }

    /// The parent uid of a copy (§3.3.3 `parent_uid`), as World recorded and
    /// persisted it (A24.2).
    pub fn parent_of(&self, uid: &SessionUid) -> Option<SessionUid> {
        self.deps.binder.parent_of(uid)
    }

    /// The uids whose actor still holds a live session.
    pub async fn live_uids(&self) -> Vec<SessionUid> {
        let all: Vec<Arc<ActorHandle>> = self.actors.lock().values().cloned().collect();
        let mut out = Vec::new();
        for a in all {
            if a.is_live().await {
                out.push(a.uid());
            }
        }
        out
    }

    pub(crate) fn insert_actor(&self, handle: Arc<ActorHandle>) {
        let uid = handle.uid();
        let old = self.actors.lock().insert(uid.clone(), handle);
        if let Some(old) = old {
            old.retire();
        }
        self.ended.lock().retain(|u| u != &uid);
    }

    /// An actor whose session ended stays for late answers; the oldest go.
    pub(crate) fn note_ended(&self, uid: &SessionUid) {
        let gone: Vec<SessionUid> = {
            let mut ended = self.ended.lock();
            if !ended.contains(uid) {
                ended.push(uid.clone());
            }
            let over = ended.len().saturating_sub(ENDED_ACTORS_KEPT);
            ended.drain(..over).collect()
        };
        for g in gone {
            let removed = self.actors.lock().remove(&g);
            if let Some(a) = removed {
                a.retire();
            }
        }
    }

    /// A turn-based session that started a new conversation moved to `new` (§6.3).
    pub(crate) fn rehome(&self, old: &SessionUid, new: &SessionUid) {
        let mut actors = self.actors.lock();
        if let Some(a) = actors.remove(old) {
            actors.insert(new.clone(), a);
        }
    }

    /// The session's live actor, or the error for a session Gilde does not run.
    pub(crate) async fn live_actor(&self, uid: &SessionUid, cap: &'static str) -> Result<Arc<ActorHandle>, IpcError> {
        if let Some(a) = self.actor(uid) {
            if a.is_live().await {
                return Ok(a);
            }
            return Err(IpcError::unsupported(cap));
        }
        match self.deps.world.session(uid) {
            None => Err(IpcError::not_found("session")),
            Some(_) => Err(IpcError::unsupported(cap)),
        }
    }

    /// Spawns are refused before the boot sweep and in memory-only mode (§2.4).
    pub(crate) async fn spawn_gate(&self) -> Result<(), IpcError> {
        self.deps.supervisor.spawn_gate().await?;
        if self.deps.supervisor.memory_only().is_some() {
            return Err(IpcError::disk_full());
        }
        Ok(())
    }
}

#[async_trait]
impl SessionActorApi for SessionsManager {
    async fn options(&self, req: &NewsessionOptionsReq) -> Result<NewSessionOptions, IpcError> {
        self.rates.check(Rate::Default("newsession_options"), None)?;
        self.new_session_options(req).await
    }

    async fn start(&self, req: SessionStartReq) -> Result<SessionStartRes, IpcError> {
        self.rates.check(Rate::Spawn, None)?;
        let key = format!("start:{}", req.client_request_id.as_str());
        let now = self.now_mono();
        let uid = self.starts.run(key, now, || self.start_session(req)).await?;
        Ok(SessionStartRes { uid })
    }

    async fn continue_session(&self, req: SessionContinueReq) -> Result<SessionContinueRes, IpcError> {
        self.rates.check(Rate::Spawn, None)?;
        let key = format!("continue:{}", req.client_request_id.as_str());
        let now = self.now_mono();
        let uid = self.starts.run(key, now, || self.continue_one(req, None)).await?;
        Ok(SessionContinueRes { uid })
    }

    async fn retry_as_terminal(&self, req: SessionRetryAsTerminalReq) -> Result<SessionRetryAsTerminalRes, IpcError> {
        self.rates.check(Rate::Spawn, None)?;
        let key = format!("retry:{}", req.client_request_id.as_str());
        let now = self.now_mono();
        let uid = req.uid.clone();
        let uid = self.starts.run(key, now, || self.retry_terminal(uid)).await?;
        Ok(SessionRetryAsTerminalRes { uid })
    }

    async fn stop_preview(&self, uid: &SessionUid) -> Result<SessionStopPreviewRes, IpcError> {
        self.rates.check(Rate::Default("session_stop_preview"), None)?;
        self.preview_stop(uid).await
    }

    async fn stop(&self, req: SessionStopReq) -> Result<SessionStopRes, IpcError> {
        self.rates.check(Rate::Default("session_stop"), None)?;
        self.stop_confirmed(req).await
    }

    async fn send(&self, req: TurnSendReq) -> Result<TurnSendRes, IpcError> {
        self.rates.check(Rate::Turn, Some(&req.uid))?;
        self.turn_send(req).await
    }

    async fn cancel_queued(&self, req: TurnCancelQueuedReq) -> Result<TurnCancelQueuedRes, IpcError> {
        self.rates.check(Rate::Turn, Some(&req.uid))?;
        let actor = match self.live_actor(&req.uid, "send").await {
            Ok(a) => a,
            Err(e) if e.code == ErrorCode::Unsupported => return Err(IpcError::not_found("message")),
            Err(e) => return Err(e),
        };
        let cancelled = actor.cancel_queued(req.client_msg_id).await?;
        Ok(TurnCancelQueuedRes { cancelled })
    }

    async fn interrupt(&self, req: TurnInterruptReq) -> Result<TurnInterruptRes, IpcError> {
        self.rates.check(Rate::Turn, Some(&req.uid))?;
        let actor = self.live_actor(&req.uid, "interrupt").await?;
        let outcome = actor.interrupt(req.discard_queued).await?;
        Ok(TurnInterruptRes { outcome })
    }

    async fn force_stop(&self, uid: &SessionUid) -> Result<bool, IpcError> {
        self.rates.check(Rate::Turn, Some(uid))?;
        let actor = self.live_actor(uid, "forceStop").await?;
        actor.force_stop().await
    }

    async fn retry(&self, req: TurnRetryReq) -> Result<TurnRetryRes, IpcError> {
        self.rates.check(Rate::Turn, Some(&req.uid))?;
        self.retry_turn(req).await
    }

    async fn set_model(&self, uid: &SessionUid, model: Option<String>) -> Result<AppliesFrom, IpcError> {
        self.rates.check(Rate::Default("session_set_model"), None)?;
        self.set_session_model(uid, model).await
    }

    async fn set_permission_mode(&self, uid: &SessionUid, mode: PermissionMode) -> Result<(), IpcError> {
        self.rates.check(Rate::Default("session_set_permission_mode"), None)?;
        let actor = self.live_actor(uid, "setPermissionMode").await?;
        actor.set_permission_mode(mode).await
    }

    async fn answer(&self, req: InteractionAnswerReq) -> Result<InteractionAnswerRes, IpcError> {
        self.rates.check(Rate::Turn, Some(&req.uid))?;
        self.answer_interaction(req).await
    }

    async fn screen_key(&self, req: ScreenPromptKeyReq) -> Result<ScreenPromptKeyRes, IpcError> {
        self.rates.check(Rate::Turn, Some(&req.uid))?;
        self.screen_prompt_key(req).await
    }

    async fn detail(&self, req: &InteractionDetailReq) -> Result<InteractionDetailRes, IpcError> {
        self.rates.check(Rate::Query, None)?;
        self.interaction_detail(req).await
    }

    async fn models(&self, uid: &SessionUid) -> Result<SessionModelsRes, IpcError> {
        self.rates.check(Rate::Default("session_models"), None)?;
        self.session_models(uid).await
    }

    async fn technical(&self, uid: &SessionUid) -> Result<SessionTechnicalRes, IpcError> {
        self.rates.check(Rate::Query, None)?;
        let actor = self.live_actor(uid, "notLive").await?;
        actor.technical().await
    }

    async fn start_sign_in(&self, account: &AccountId) -> Result<AccountSignInRes, IpcError> {
        let uid = self.sign_in(account).await?;
        Ok(AccountSignInRes { uid })
    }

    async fn continue_all(&self, record: &QuitRecordId, uids: Option<Vec<SessionUid>>) -> Result<ContinueAllRes, IpcError> {
        self.continue_record(record, uids).await
    }

    async fn stop_for_quit(&self, uid: &SessionUid) -> Result<StopOutcome, IpcError> {
        match self.actor(uid) {
            Some(a) if a.is_live().await => a.stop(StopCause::Quit).await,
            _ => Ok(StopOutcome { stopped: true, survivors: 0 }),
        }
    }
}

// ---------------------------------------------------------------------------
// A48.4: the timers that guard a user action run on the monotonic clock
// ---------------------------------------------------------------------------

#[cfg(test)]
mod timer_tests {
    //! The confirm-token TTL and the idempotency windows, with the wall clock
    //! jumping ±8 h and the monotonic clock pinned. A manager over the real
    //! World with small fakes; nothing is spawned.
    use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

    use super::*;
    use crate::contract::account::{AccountSource, SignIn};
    use crate::contract::activity::{ExitInfo, ProcessFacts, RegStatus, RegistryFacts};
    use crate::contract::common::{CompatState, Notice};
    use crate::contract::compat::{CanaryResult, CompatSource};
    use crate::contract::ids::{ClientRequestId, ConfirmToken, ProcessKey};
    use crate::contract::ipc::ContinueHow;
    use crate::contract::services::{
        EvidenceUpdate, IdentityHint, IdentityKey, ProcSnapshot, SignalOutcome, SignalPlan, SourceFacts, SpawnRequest,
        SpawnTicket,
    };
    use crate::contract::session::{BinaryFingerprint, BinaryRef};
    use crate::sessions::options::MemoryPrefs;
    use crate::world::{ManualClock, MemoryMetaStore, World, WorldConfig, WorldSettings};

    const T0: EpochMs = 1_792_749_600_000;
    const HOUR: i64 = 3_600_000;

    struct NoTransports;
    #[async_trait]
    impl Transports for NoTransports {
        async fn conversation(&self, _spec: ConvoSpec, _wiring: ConvoWiring) -> Result<Box<dyn ActorDriver>, IpcError> {
            Err(IpcError::unsupported("test"))
        }
        async fn terminal(&self, _spec: PtyOpenSpec, _on_exit: Arc<dyn PtyExitSink>) -> Result<TerminalStarted, IpcError> {
            Err(IpcError::unsupported("test"))
        }
        async fn terminal_ready(&self, _pty: &PtyId, _timeout: Duration) -> ReadyOutcome {
            ReadyOutcome::Ready
        }
        fn turn_based(
            &self,
            _spec: TurnSessionSpec,
            _evidence: Arc<dyn EvidenceSink>,
            _content: Arc<Mutex<SessionUid>>,
        ) -> Box<dyn ActorDriver> {
            panic!("the timer tests start nothing")
        }
    }

    fn compat() -> ClaudeCompat {
        ClaudeCompat {
            version: "2.1.306".into(),
            conversation: CompatState::Verified,
            pane_hosting: CompatState::Unverified,
            source: CompatSource::Shipped,
            static_canary: CanaryResult::NotRun,
            handshake_ok: None,
            below_minimum: false,
            note: None,
            features: BTreeMap::new(),
        }
    }

    struct OneBinary;
    #[async_trait]
    impl Binaries for OneBinary {
        async fn installed(&self) -> Result<Vec<VendorOption>, IpcError> {
            Ok(Vec::new())
        }
        async fn resolve(&self, _vendor: Vendor) -> Result<ResolvedVendor, IpcError> {
            let binary = BinaryRef {
                realpath: AbsPath::parse("/opt/claude/2.1.306/claude").unwrap(),
                fingerprint: BinaryFingerprint { size: 7, mtime_ms: 1, inode: 7 },
                version: Some("2.1.306".into()),
                pidpath: None,
            };
            Ok(ResolvedVendor { binary, compat: Some(compat()) })
        }
    }

    struct OneAccount;
    #[async_trait]
    impl AccountsPort for OneAccount {
        async fn all(&self) -> Vec<Account> {
            vec![Account {
                id: account(),
                vendor: Vendor::Claude,
                config_dir: None,
                label: "Default".into(),
                is_default: true,
                source: AccountSource::Ambient,
                usable_for_new: true,
                sign_in: SignIn::SignedIn,
                quota: None,
                live_sessions: 0,
            }]
        }
        async fn env_for(&self, id: &AccountId) -> Option<AccountEnv> {
            Some(AccountEnv { account_id: id.clone(), vendor: Vendor::Claude, env_key: None, config_dir: None })
        }
        fn observed_quota(&self, _id: &AccountId, _quota: Quota) {}
        fn signed_out(&self, _id: &AccountId) {}
    }

    struct Env;
    impl EnvSource for Env {
        fn compose(&self, _vendor: Vendor, _account: Option<&AccountEnv>, _marker: &str) -> BTreeMap<String, String> {
            BTreeMap::new()
        }
        fn home(&self) -> Option<AbsPath> {
            AbsPath::parse("/Users/tester").ok()
        }
    }

    struct Compat;
    #[async_trait]
    impl CompatApi for Compat {
        fn list(&self) -> Vec<ClaudeCompat> {
            vec![compat()]
        }
        fn lookup(&self, _version: &str) -> Option<ClaudeCompat> {
            Some(compat())
        }
        async fn for_binary(&self, _binary: &BinaryRef) -> ClaudeCompat {
            compat()
        }
        fn feature(&self, _version: &str, _flag: &str) -> bool {
            false
        }
        fn app_feature(&self, _flag: &str) -> bool {
            false
        }
        fn notices(&self, _version: &str) -> Vec<Notice> {
            Vec::new()
        }
        fn schedule_canary(&self, _binary: &BinaryRef) {}
    }

    /// Nothing is live anywhere: a watched stop finds no process to signal.
    struct NoneLive;
    #[async_trait]
    impl Liveness for NoneLive {
        async fn find(&self, _vendor: Vendor, _id: &str) -> Result<Vec<LiveProcess>, IpcError> {
            Ok(Vec::new())
        }
        fn alive(&self, _pid: i32, _start_sec: i64) -> bool {
            false
        }
    }

    /// Refuses every spawn and signal.
    struct Refusing;
    #[async_trait]
    impl SupervisorApi for Refusing {
        async fn spawn_gate(&self) -> Result<(), IpcError> {
            Ok(())
        }
        async fn plan_spawn(&self, _req: SpawnRequest) -> Result<SpawnTicket, IpcError> {
            Err(IpcError::refused("notAllowlisted"))
        }
        async fn mark_running(&self, _t: &SpawnTicket, _pid: i32) -> Result<(), IpcError> {
            Ok(())
        }
        async fn mark_exited(&self, _t: &SpawnTicket, _exit: ExitInfo) {}
        async fn signal(&self, _target: &ProcessKey, _plan: SignalPlan) -> Result<SignalOutcome, IpcError> {
            Err(IpcError::refused("identityUnverified"))
        }
        fn is_vendor_cli(&self, _p: &str) -> bool {
            true
        }
        fn redact_argv(&self, argv: &[String]) -> Vec<String> {
            argv.to_vec()
        }
        fn memory_only(&self) -> Option<String> {
            None
        }
        fn last_scan(&self) -> Option<ProcSnapshot> {
            None
        }
    }

    struct NoQuitItems;
    #[async_trait]
    impl QuitItems for NoQuitItems {
        async fn items(&self, _record: &QuitRecordId) -> Result<Vec<QuitItemRow>, IpcError> {
            Err(IpcError::not_found("quitRecord"))
        }
    }

    fn account() -> AccountId {
        AccountId::derive(Vendor::Claude, "/Users/tester/.claude")
    }

    /// The manager, with a wall clock and a monotonic clock the test moves.
    struct Rig {
        world: World,
        m: Arc<SessionsManager>,
        wall: Arc<AtomicI64>,
        mono: Arc<AtomicU64>,
    }

    impl Rig {
        fn new() -> Rig {
            let world = World::new(WorldConfig {
                clock: Arc::new(ManualClock::new(T0, 50_000_000)),
                meta_store: Arc::new(MemoryMetaStore::default()),
                settings: WorldSettings::default(),
                home: Some("/Users/tester".into()),
            });
            let wall = Arc::new(AtomicI64::new(T0));
            let mono = Arc::new(AtomicU64::new(9_000_000));
            let deps = SessionDeps {
                world: Arc::new(world.clone()),
                binder: Arc::new(world.clone()),
                supervisor: Arc::new(Refusing),
                transports: Arc::new(NoTransports),
                binaries: Arc::new(OneBinary),
                accounts: Arc::new(OneAccount),
                env: Arc::new(Env),
                compat: Arc::new(Compat),
                liveness: Arc::new(NoneLive),
                prefs: Arc::new(MemoryPrefs::default()),
                quit_items: Arc::new(NoQuitItems),
                git: None,
                panehost: None,
                sidecar: None,
                clock: {
                    let wall = wall.clone();
                    Arc::new(move || wall.load(Ordering::SeqCst))
                },
                timing: SessionTiming::default(),
            };
            let m = SessionsManager::with_monotonic_clock(deps, {
                let mono = mono.clone();
                Arc::new(move || mono.load(Ordering::SeqCst))
            });
            Rig { world, m, wall, mono }
        }

        /// A claude session started elsewhere, alive and idle (Watching).
        fn watched(&self, n: u32) -> SessionUid {
            let vsid = format!("{n:08x}-0000-4000-8000-000000000001");
            let hint = IdentityHint {
                vendor: Some(Vendor::Claude),
                account_id: Some(account()),
                vendor_session_id: Some(vsid.clone()),
                process: Some(ProcessKey::new(6000 + n, 1_792_700_000)),
                gilde_spawn_id: None,
            };
            let sink = self.world.sink();
            sink.evidence(EvidenceUpdate {
                hint: hint.clone(),
                facts: SourceFacts::Process(ProcessFacts::Alive { pid: 6000 + n as i32, start_sec: 1_792_700_000, cpu_active_at: None }),
                at: T0,
            });
            sink.evidence(EvidenceUpdate {
                hint,
                facts: SourceFacts::Registry(RegistryFacts {
                    status: Some(RegStatus::Idle),
                    waiting_for: None,
                    status_updated_at: Some(T0),
                    peer_protocol_ok: true,
                }),
                at: T0,
            });
            self.world.flush_now();
            let key = IdentityKey { vendor: Vendor::Claude, account_id: Some(account()), vendor_session_id: vsid };
            let uid = self.world.uid_resolver().uid_for(&key);
            assert!(self.world.session(&uid).is_some(), "the watched session exists");
            uid
        }

        /// The wall clock jumps by `ms` (a time change, NTP, a time-zone bug).
        fn jump_wall(&self, ms: i64) {
            self.wall.fetch_add(ms, Ordering::SeqCst);
        }

        fn advance_mono(&self, ms: u64) {
            self.mono.fetch_add(ms, Ordering::SeqCst);
        }

        async fn stop(&self, uid: &SessionUid, token: ConfirmToken) -> Result<SessionStopRes, IpcError> {
            self.m.stop(SessionStopReq { uid: uid.clone(), confirm_token: token }).await
        }
    }

    /// `CONFLICT{changed}` = the token was refused; anything else = it was
    /// accepted (here the stop then finds nothing to signal).
    fn token_refused(r: &Result<SessionStopRes, IpcError>) -> bool {
        matches!(r, Err(e) if e.code == ErrorCode::Conflict)
    }

    #[tokio::test]
    async fn a48_4_confirm_token_survives_wall_jumps_and_expires_on_the_monotonic_clock() {
        let r = Rig::new();
        let uid = r.watched(1);
        for jump in [8 * HOUR, -8 * HOUR] {
            // The wall clock jumps right after the preview: the token is
            // neither cut (+8 h) nor stretched (−8 h).
            let p = r.m.stop_preview(&uid).await.unwrap();
            r.jump_wall(jump);
            r.advance_mono(1_000);
            let out = r.stop(&uid, p.confirm_token).await;
            assert!(!token_refused(&out), "wall {jump:+} ms: a fresh token was refused: {out:?}");
            assert_eq!(out.unwrap_err().code, ErrorCode::Refused, "accepted, then nothing to signal");

            // 60 s on the monotonic clock end it, whatever the wall clock did.
            let p = r.m.stop_preview(&uid).await.unwrap();
            r.jump_wall(-jump);
            r.advance_mono(CONFIRM_TTL_MS);
            let out = r.stop(&uid, p.confirm_token).await;
            assert!(token_refused(&out), "wall {:+} ms, monotonic +60 s: the token still worked: {out:?}", -jump);
        }
        // One second before the TTL it is still good, with the wall clock 8 h back.
        let p = r.m.stop_preview(&uid).await.unwrap();
        r.jump_wall(-8 * HOUR);
        r.advance_mono(CONFIRM_TTL_MS - 1_000);
        assert!(!token_refused(&r.stop(&uid, p.confirm_token).await));
    }

    #[tokio::test]
    async fn a48_4_idempotency_window_survives_wall_jumps_and_ends_on_the_monotonic_clock() {
        let r = Rig::new();
        let uid = r.watched(2);
        let cid = ClientRequestId::parse(uuid::Uuid::new_v4().to_string()).unwrap();
        let req = |uid: SessionUid| SessionContinueReq {
            client_request_id: cid.clone(),
            uid,
            how: ContinueHow::Resume,
            control_mode: None,
            model: None,
            // refused by the value rules once the session is found: nothing starts
            permission_mode: Some(PermissionMode::BypassPermissions),
        };
        // The first call (an unknown session) fixes the outcome of the request id.
        let first = r.m.continue_session(req(SessionUid::mint())).await.unwrap_err();
        assert_eq!(first.code, ErrorCode::NotFound);
        // A repeat of the same request id — here naming the real session — is
        // answered from memory within the window, whatever the wall clock does.
        for jump in [8 * HOUR, -16 * HOUR] {
            r.jump_wall(jump);
            r.advance_mono(20_000);
            let again = r.m.continue_session(req(uid.clone())).await.unwrap_err();
            assert_eq!(again.code, ErrorCode::NotFound, "wall {jump:+} ms: the window was cut");
        }
        // 60 s on the monotonic clock after the first call it runs again.
        r.advance_mono(IDEMPOTENCY_TTL_MS - 40_000);
        let fresh = r.m.continue_session(req(uid.clone())).await.unwrap_err();
        assert_eq!(fresh.code, ErrorCode::InvalidArgument, "the window did not end: {fresh:?}");
    }

    #[tokio::test]
    async fn a48_4_idem_measures_its_window_in_the_clock_it_is_given() {
        let idem: Idem<u32> = Idem::new(60_000);
        let calls = AtomicU64::new(0);
        let run = |now: u64| {
            let calls = &calls;
            let idem = &idem;
            async move {
                idem.run("k".into(), now, || async move { Ok(calls.fetch_add(1, Ordering::SeqCst) as u32) }).await.unwrap()
            }
        };
        assert_eq!(run(1_000).await, 0);
        assert_eq!(run(60_999).await, 0, "inside the window");
        assert_eq!(run(61_000).await, 1, "the window ended");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
