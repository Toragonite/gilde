//! T-sessions-* (contract §6.0, §6.5, §4.2.2, §4.2.3, §14.2) against fakes of
//! every port: World (+ its A18/A19/A20 binding methods), the three
//! transports, binaries, accounts, env, compat, liveness, supervisor,
//! PaneHost, git and the quit records. No process is started by these tests;
//! the real drivers are exercised in `sessions_live.rs`.
//!
//! Waits are event driven with generous ceilings; nothing asserts a
//! wall-clock budget (A10.7).

mod fixtures;

#[path = "fixtures/fake_tmux_client/mod.rs"]
mod fake_tmux_client;

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;

use gilde_code_lib::contract::account::{Account, AccountSource, Quota, QuotaWindow, QuotaWindowKind, SignIn};
use gilde_code_lib::contract::activity::{
    Activity, ActivityState, DidntStartReason, ErrorSub, EvidenceLine, ExitInfo,
    GildeFacts, ProcessFacts, StopCause,
};
use gilde_code_lib::contract::attention::AttentionQueue;
use gilde_code_lib::contract::common::{AbsPath, CompatState, Confidence, EpochMs, Notice, PermissionMode, Vendor};
use gilde_code_lib::contract::compat::{CanaryResult, ClaudeCompat, CompatSource};
use gilde_code_lib::contract::error::{ErrorCode, IpcError};
use gilde_code_lib::contract::ids::{
    AccountId, ClientMsgId, ClientRequestId, ConfirmToken, EpisodeId, LocationId, ProcessKey, ProjectId, PtyId,
    QuitRecordId, RequestId, SessionUid,
};
use gilde_code_lib::contract::interaction::{
    Decision, DecisionKind, InteractionKind, InteractionOption, OptionRole, PendingInteraction,
};
use gilde_code_lib::contract::ipc::{
    AnswerOutcome, AnswerVia, AppliesFrom, ChangesGetReq, ChangesView, ContinueHow, DismissWhat, GitCommitPreviewReq,
    GitCommitPreviewRes, GitCommitReq, GitCommitRes, GitMergePreviewReq, GitMergePreviewRes, GitMergeReq, GitMergeRes,
    InteractionAnswerReq, InterruptOutcome, ModelOption, NewsessionOptionsReq, ScreenKeyOutcome, ScreenPromptAction,
    ScreenPromptKeyReq, SendDelivery, SendPriority, SessionContinueReq, SessionRetryAsTerminalReq, SessionStartReq,
    SessionStopReq, StartLocation, StopMethod, TurnRetryReq, TurnSendReq, VendorOption, WorktreeDiscardPreviewReq,
    WorktreeDiscardPreviewRes, WorktreeDiscardReq,
};
use gilde_code_lib::contract::process::{Leftover, LeftoverReason, ProcRole, ProcessRecord};
use gilde_code_lib::contract::project::{Location, LocationKind, Project, ProjectKind};
use gilde_code_lib::contract::services::{
    CompatApi, EvidenceSink, EvidenceUpdate, GitApi, IdentityKey, IdentityLock, LeadEnv, LocationFacts,
    OutgoingMessage, PaneHostApi, ProcSnapshot, PtyExitSink, PtyHandle, PtyOpenSpec, SessionActorApi, SessionDriver,
    SignalOutcome, SignalPlan, SourceFacts, SpawnRequest, SpawnTicket, StorePatchSink, SupervisorApi, WorldApi,
};
use gilde_code_lib::contract::session::{
    ApproveCap, BinaryFingerprint, BinaryRef, ChangesCap, Control, ControlMode, EndedHow, Origin, QueuedMessage,
    SendCap, Session, SessionCaps, SessionDetails, StopCap, Transport,
};
use gilde_code_lib::contract::sidecar::AccountEnv;
use gilde_code_lib::contract::store::{BootStatus, IntegrationsState, OrchestratorPresence, StoreSnapshot};
use gilde_code_lib::contract::team::TeamAdapter;
use gilde_code_lib::convo::driver::ConvoSpec;
use gilde_code_lib::convo::handshake::LaunchKind;
use gilde_code_lib::pty::ReadyOutcome;
use gilde_code_lib::sessions::actor::ActorDriver;
use gilde_code_lib::sessions::manager::{
    AccountsPort, Binaries, ControlFacts, ConvoWiring, DriverDetails, EnvSource, LiveProcess, Liveness, QuitItems,
    ResolvedVendor, SessionBinder, SessionDeps, SessionTiming, SessionsManager, TerminalStarted, Transports,
};
use gilde_code_lib::sessions::options::MemoryPrefs;
use gilde_code_lib::sessions::start::apply_account_rule;
use gilde_code_lib::store::models::QuitItemRow;
use gilde_code_lib::turns::driver::TurnSessionSpec;

const CEILING: Duration = Duration::from_secs(20);
const NOW: EpochMs = 1_800_000_000_000;

// ---------------------------------------------------------------------------
// ids and builders
// ---------------------------------------------------------------------------

fn cid() -> ClientRequestId {
    ClientRequestId::parse(uuid::Uuid::new_v4().to_string()).unwrap()
}

fn mid() -> ClientMsgId {
    ClientMsgId::parse(uuid::Uuid::new_v4().to_string()).unwrap()
}

fn abs(p: &str) -> AbsPath {
    AbsPath::parse(p).unwrap()
}

fn project_id() -> ProjectId {
    ProjectId::derive("/work/repo")
}

fn main_location() -> LocationId {
    LocationId::derive("/work/repo")
}

fn account(id: &str, vendor: Vendor, label: &str, default: bool) -> Account {
    Account {
        id: AccountId::parse(id).unwrap(),
        vendor,
        config_dir: None,
        label: label.to_string(),
        is_default: default,
        source: AccountSource::Ambient,
        usable_for_new: true,
        sign_in: SignIn::SignedIn,
        quota: None,
        live_sessions: 0,
    }
}

const CLAUDE_ACCT: &str = "a_00000000000000c1";
const CLAUDE_WORK: &str = "a_00000000000000c2";
const CODEX_ACCT: &str = "a_00000000000000d1";
const CURSOR_ACCT: &str = "a_00000000000000e1";

fn claude_account() -> AccountId {
    AccountId::parse(CLAUDE_ACCT).unwrap()
}

fn caps() -> SessionCaps {
    SessionCaps {
        send: SendCap::None,
        interrupt: false,
        force_stop: false,
        approve: ApproveCap::None,
        set_model: false,
        set_permission_mode: Vec::new(),
        continue_options: Vec::new(),
        stop_session: StopCap::None,
        terminal_view: false,
        log_view: false,
        changes: ChangesCap::None,
        owner_focus: false,
    }
}

fn session(uid: &SessionUid, vendor: Vendor, account: Option<&str>, vsid: Option<&str>, control: Control, state: ActivityState) -> Session {
    Session {
        uid: uid.clone(),
        vendor,
        account_id: account.map(|a| AccountId::parse(a).unwrap()),
        vendor_session_id: vsid.map(str::to_string),
        project_id: Some(project_id()),
        location_id: Some(main_location()),
        cwd: Some(abs("/work/repo")),
        control,
        activity: Activity {
            state,
            since: NOW,
            confidence: Confidence::High,
            evidence: None,
            episode: EpisodeId::derive(uid, "x", NOW),
        },
        title: None,
        team: None,
        started_at: Some(NOW),
        ended_at: None,
        last_activity_at: None,
        pending: Vec::new(),
        queued: Vec::new(),
        model: None,
        permission_mode: None,
        binary: None,
        claude_version: None,
        memory_bytes: None,
        usage_today: None,
        caps: caps(),
        details: SessionDetails::default(),
        merged_into: None,
    }
}

fn ended() -> Control {
    Control::NotRunning { last_origin: Some(Origin::Terminal), ended_how: EndedHow::Ended }
}

fn watching(origin: Origin) -> Control {
    Control::Watching { origin, owner: None }
}

fn binary(path: &str, version: &str) -> BinaryRef {
    BinaryRef {
        realpath: abs(path),
        fingerprint: BinaryFingerprint { size: version.len() as u64, mtime_ms: 1, inode: 7 },
        version: Some(version.to_string()),
        pidpath: None,
    }
}

fn compat(version: &str, conversation: CompatState, below: bool) -> ClaudeCompat {
    ClaudeCompat {
        version: version.to_string(),
        conversation,
        pane_hosting: CompatState::Unverified,
        source: CompatSource::Shipped,
        static_canary: CanaryResult::NotRun,
        handshake_ok: None,
        below_minimum: below,
        note: None,
        features: BTreeMap::new(),
    }
}

async fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + CEILING;
    while !f() {
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ---------------------------------------------------------------------------
// World + binder
// ---------------------------------------------------------------------------

#[derive(Default)]
struct WorldState {
    sessions: Mutex<HashMap<SessionUid, Session>>,
    projects: Mutex<Vec<Project>>,
    locations: Mutex<Vec<Location>>,
    leftovers: Mutex<Vec<ProcessRecord>>,
    transcripts: Mutex<HashMap<SessionUid, AbsPath>>,
    evidence: Mutex<Vec<EvidenceUpdate>>,
    location_facts: Mutex<Vec<LocationFacts>>,
    binds: Mutex<Vec<(String, Option<SessionUid>, SessionUid)>>,
    controls: Mutex<HashMap<SessionUid, ControlFacts>>,
    queued: Mutex<HashMap<SessionUid, Vec<QueuedMessage>>>,
    details: Mutex<Vec<(SessionUid, DriverDetails)>>,
    locks: Mutex<HashMap<IdentityKey, Arc<tokio::sync::Mutex<()>>>>,
    /// A24.2: `set_parent(copy, source)` calls.
    parents: Mutex<HashMap<SessionUid, SessionUid>>,
}

struct FakeWorld(Arc<WorldState>);
struct FakeSink(Arc<WorldState>);

impl EvidenceSink for FakeSink {
    fn evidence(&self, update: EvidenceUpdate) {
        self.0.evidence.lock().push(update);
    }
    fn location(&self, facts: LocationFacts) {
        self.0.location_facts.lock().push(facts);
    }
    fn source_status(&self, _status: gilde_code_lib::contract::store::SourceStatus) {}
    fn account(&self, _account: Account) {}
}

fn snapshot_of(w: &WorldState) -> StoreSnapshot {
    StoreSnapshot {
        rev: 1,
        projects: w.projects.lock().clone(),
        locations: w.locations.lock().clone(),
        sessions: w.sessions.lock().values().cloned().collect(),
        teams: Vec::new(),
        members: Vec::new(),
        tasks: Vec::new(),
        messages: Vec::new(),
        queue: AttentionQueue { rev: 0, items: Vec::new(), need_you: 0, also: 0, paused_until: None },
        accounts: Vec::new(),
        leftovers: w.leftovers.lock().clone(),
        compat: Vec::new(),
        notices: Vec::new(),
        boot: BootStatus { spawn_gate_open: true, first_discovery_done: true, agents_detected: true, memory_only: None, agents: Vec::new(), agents_scanned_at: None, recovery: None },
        sources: Vec::new(),
        integrations: IntegrationsState { orchestrator: OrchestratorPresence::Absent },
    }
}

#[async_trait]
impl WorldApi for FakeWorld {
    fn sink(&self) -> Arc<dyn EvidenceSink> {
        Arc::new(FakeSink(self.0.clone()))
    }
    fn subscribe(&self, _sink: Box<dyn StorePatchSink>) -> StoreSnapshot {
        snapshot_of(&self.0)
    }
    fn snapshot(&self) -> StoreSnapshot {
        snapshot_of(&self.0)
    }
    fn session(&self, uid: &SessionUid) -> Option<Session> {
        self.0.sessions.lock().get(uid).cloned()
    }
    fn queue(&self) -> AttentionQueue {
        AttentionQueue { rev: 0, items: Vec::new(), need_you: 0, also: 0, paused_until: None }
    }
    async fn identity_lock(&self, key: IdentityKey) -> IdentityLock {
        let m = self.0.locks.lock().entry(key).or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))).clone();
        m.lock_owned().await
    }
    fn mark_seen(&self, _uid: &SessionUid) -> Result<(), IpcError> {
        Ok(())
    }
    fn dismiss(&self, _uid: &SessionUid, _what: DismissWhat) -> Result<(), IpcError> {
        Ok(())
    }
    fn queue_later(&self, _uid: &SessionUid) -> Result<EpochMs, IpcError> {
        Ok(NOW)
    }
    fn register_team_adapter(&self, _adapter: Arc<dyn TeamAdapter>) {}
    fn transcript(&self, uid: &SessionUid) -> Option<(AbsPath, Vendor)> {
        self.0.transcripts.lock().get(uid).cloned().map(|p| (p, Vendor::Claude))
    }
}

impl SessionBinder for FakeWorld {
    fn bind_spawn(&self, spawn_id: &str, uid: Option<&SessionUid>, _vendor: Vendor, _account: Option<AccountId>) -> SessionUid {
        let out = uid.cloned().unwrap_or_else(SessionUid::mint);
        self.0.binds.lock().push((spawn_id.to_string(), uid.cloned(), out.clone()));
        out
    }
    fn set_control_detail(&self, uid: &SessionUid, detail: ControlFacts) {
        self.0.controls.lock().insert(uid.clone(), detail);
    }
    fn set_queued(&self, uid: &SessionUid, list: Vec<QueuedMessage>) {
        self.0.queued.lock().insert(uid.clone(), list);
    }
    fn set_details_extra(&self, uid: &SessionUid, extra: DriverDetails) {
        self.0.details.lock().push((uid.clone(), extra));
    }
    fn set_parent(&self, uid: &SessionUid, parent: &SessionUid) {
        self.0.parents.lock().insert(uid.clone(), parent.clone());
    }
    fn parent_of(&self, uid: &SessionUid) -> Option<SessionUid> {
        self.0.parents.lock().get(uid).cloned()
    }
}

// ---------------------------------------------------------------------------
// drivers and transports
// ---------------------------------------------------------------------------

#[derive(Default)]
struct DriverLog {
    sends: Mutex<Vec<OutgoingMessage>>,
    send_result: Mutex<Option<Result<SendDelivery, IpcError>>>,
    cancels: Mutex<Vec<ClientMsgId>>,
    answers: Mutex<Vec<(RequestId, Decision)>>,
    pending: Mutex<Vec<PendingInteraction>>,
    screen_keys: Mutex<Vec<(RequestId, String)>>,
    interrupts: Mutex<Vec<bool>>,
    stops: Mutex<Vec<StopCause>>,
    models: Mutex<Vec<ModelOption>>,
    set_models: Mutex<Vec<Option<String>>>,
    modes: Mutex<Vec<PermissionMode>>,
    queued: Mutex<Vec<QueuedMessage>>,
    ended: AtomicBool,
}

struct FakeDriver {
    mode: ControlMode,
    log: Arc<DriverLog>,
}

fn session_ended() -> IpcError {
    IpcError::unsupported("sessionEnded")
}

#[async_trait]
impl SessionDriver for FakeDriver {
    fn mode(&self) -> ControlMode {
        self.mode
    }
    async fn send(&mut self, msg: OutgoingMessage) -> Result<SendDelivery, IpcError> {
        if self.log.ended.load(Ordering::SeqCst) {
            return Err(session_ended());
        }
        let res = self.log.send_result.lock().clone().unwrap_or(Ok(SendDelivery::Sent));
        if res == Ok(SendDelivery::Queued) {
            self.log.queued.lock().push(QueuedMessage { client_msg_id: msg.client_msg_id.clone(), text: msg.text.clone(), queued_at: NOW });
        }
        self.log.sends.lock().push(msg);
        res
    }
    async fn cancel_queued(&mut self, id: &ClientMsgId) -> Result<bool, IpcError> {
        self.log.cancels.lock().push(id.clone());
        let mut q = self.log.queued.lock();
        let before = q.len();
        q.retain(|m| &m.client_msg_id != id);
        Ok(q.len() != before)
    }
    async fn answer(&mut self, req: &RequestId, d: Decision) -> Result<AnswerOutcome, IpcError> {
        self.log.answers.lock().push((req.clone(), d));
        Ok(AnswerOutcome::Answered)
    }
    async fn screen_key(&mut self, req: &RequestId, rev: &str, _key: ScreenPromptAction) -> Result<ScreenKeyOutcome, IpcError> {
        self.log.screen_keys.lock().push((req.clone(), rev.to_string()));
        Ok(ScreenKeyOutcome::Sent)
    }
    async fn interrupt(&mut self, discard_queued: bool) -> Result<InterruptOutcome, IpcError> {
        self.log.interrupts.lock().push(discard_queued);
        Ok(match self.mode {
            ControlMode::Conversation => InterruptOutcome::Interrupting,
            ControlMode::Terminal => InterruptOutcome::Sent,
            ControlMode::TurnBased => InterruptOutcome::Stopped,
        })
    }
    async fn force_stop(&mut self) -> Result<bool, IpcError> {
        Ok(true)
    }
    async fn set_model(&mut self, m: Option<String>) -> Result<AppliesFrom, IpcError> {
        self.log.set_models.lock().push(m);
        Ok(AppliesFrom::NextRequest)
    }
    async fn set_permission_mode(&mut self, m: PermissionMode) -> Result<(), IpcError> {
        self.log.modes.lock().push(m);
        Ok(())
    }
    async fn stop(&mut self, why: StopCause) -> Result<gilde_code_lib::contract::services::StopOutcome, IpcError> {
        self.log.stops.lock().push(why);
        self.log.ended.store(true, Ordering::SeqCst);
        Ok(gilde_code_lib::contract::services::StopOutcome { stopped: true, survivors: 0 })
    }
}

#[async_trait]
impl ActorDriver for FakeDriver {
    async fn answer_with_previous(&mut self, req: &RequestId, d: Decision) -> Result<(AnswerOutcome, Option<DecisionKind>), IpcError> {
        if self.log.ended.load(Ordering::SeqCst) {
            return Err(session_ended());
        }
        let previous = self.log.answers.lock().iter().find(|(r, _)| r == req).map(|(_, d)| d.kind());
        if let Some(p) = previous {
            return Ok((AnswerOutcome::AlreadyAnswered, Some(p)));
        }
        self.log.answers.lock().push((req.clone(), d));
        Ok((AnswerOutcome::Answered, None))
    }
    async fn pending(&self, req: &RequestId) -> Option<PendingInteraction> {
        self.log.pending.lock().iter().find(|p| &p.request_id == req).cloned()
    }
    async fn models(&self) -> Result<(Vec<ModelOption>, Option<String>), IpcError> {
        if self.mode == ControlMode::Terminal {
            return Err(IpcError::unsupported("setModel"));
        }
        Ok((self.log.models.lock().clone(), None))
    }
    async fn queued(&self) -> Option<Vec<QueuedMessage>> {
        (self.mode == ControlMode::TurnBased).then(|| self.log.queued.lock().clone())
    }
}

#[derive(Default)]
struct FakeTransports {
    convo: Mutex<Vec<ConvoSpec>>,
    convo_fail: Mutex<Option<IpcError>>,
    wirings: Mutex<Vec<ConvoWiring>>,
    pty: Mutex<Vec<PtyOpenSpec>>,
    exits: Mutex<Vec<Arc<dyn PtyExitSink>>>,
    turns: Mutex<Vec<TurnSessionSpec>>,
    turn_sinks: Mutex<Vec<Arc<dyn EvidenceSink>>>,
    logs: Mutex<Vec<(SessionUid, Arc<DriverLog>)>>,
    seq: AtomicU64,
    /// `None` = ready at once; `Some(false)` = never ready.
    ready: Mutex<Option<bool>>,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
    /// Each spawn holds this long (concurrency tests).
    spawn_delay: Mutex<Duration>,
}

impl FakeTransports {
    fn log_of(&self, uid: &SessionUid) -> Arc<DriverLog> {
        self.logs.lock().iter().rev().find(|(u, _)| u == uid).map(|(_, l)| l.clone()).expect("a driver for this uid")
    }

    fn new_driver(&self, uid: &SessionUid, mode: ControlMode) -> Box<dyn ActorDriver> {
        let log = Arc::new(DriverLog::default());
        self.logs.lock().push((uid.clone(), log.clone()));
        Box::new(FakeDriver { mode, log })
    }

    async fn hold(&self) {
        let n = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(n, Ordering::SeqCst);
        let delay = *self.spawn_delay.lock();
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl Transports for FakeTransports {
    async fn conversation(&self, spec: ConvoSpec, wiring: ConvoWiring) -> Result<Box<dyn ActorDriver>, IpcError> {
        self.hold().await;
        if let Some(e) = self.convo_fail.lock().clone() {
            return Err(e);
        }
        let n = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        // Like T05's pump: the first evidence carries the spawn id.
        let mut hint = spec.hint.clone();
        hint.gilde_spawn_id = Some(format!("sp_c{n}"));
        wiring.evidence.evidence(EvidenceUpdate {
            hint,
            facts: SourceFacts::Process(ProcessFacts::Alive { pid: 4242, start_sec: 1, cpu_active_at: None }),
            at: NOW,
        });
        let uid = spec.uid.clone();
        self.convo.lock().push(spec);
        self.wirings.lock().push(wiring);
        Ok(self.new_driver(&uid, ControlMode::Conversation))
    }

    async fn terminal(&self, spec: PtyOpenSpec, on_exit: Arc<dyn PtyExitSink>) -> Result<TerminalStarted, IpcError> {
        self.hold().await;
        let n = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let uid = spec.parent_uid.clone().expect("parent_uid = the session uid");
        self.pty.lock().push(spec);
        self.exits.lock().push(on_exit);
        let handle = PtyHandle {
            pty_id: PtyId::mint(),
            pid: std::process::id() as i32,
            ticket: SpawnTicket { spawn_id: format!("sp_t{n}"), env_marker: format!("mk_t{n}") },
        };
        Ok(TerminalStarted { handle, driver: self.new_driver(&uid, ControlMode::Terminal) })
    }

    async fn terminal_ready(&self, _pty: &PtyId, timeout: Duration) -> ReadyOutcome {
        let ready = *self.ready.lock();
        match ready {
            None | Some(true) => ReadyOutcome::Ready,
            Some(false) => {
                tokio::time::sleep(timeout).await;
                ReadyOutcome::Timeout
            }
        }
    }

    fn turn_based(&self, spec: TurnSessionSpec, evidence: Arc<dyn EvidenceSink>, _content: Arc<Mutex<SessionUid>>) -> Box<dyn ActorDriver> {
        let uid = spec.uid.clone();
        // Like T08: Gilde facts at once, with the spec's hint.
        evidence.evidence(EvidenceUpdate {
            hint: spec.hint.clone(),
            facts: SourceFacts::Gilde(GildeFacts {
                spawned_at: NOW,
                mode: ControlMode::TurnBased,
                ready_at: None,
                stop_cause: None,
                signals_sent: Vec::new(),
                start_failure: None,
            }),
            at: NOW,
        });
        self.turns.lock().push(spec);
        self.turn_sinks.lock().push(evidence);
        self.new_driver(&uid, ControlMode::TurnBased)
    }
}

// ---------------------------------------------------------------------------
// the other ports
// ---------------------------------------------------------------------------

struct FakeBinaries {
    resolved: Mutex<HashMap<Vendor, ResolvedVendor>>,
    detected: AtomicBool,
}

#[async_trait]
impl Binaries for FakeBinaries {
    async fn installed(&self) -> Result<Vec<VendorOption>, IpcError> {
        if !self.detected.load(Ordering::SeqCst) {
            return Err(IpcError::not_ready("agentsNotDetected"));
        }
        let r = self.resolved.lock();
        Ok(Vendor::ALL
            .iter()
            .map(|v| VendorOption { vendor: *v, installed: r.contains_key(v), version: r.get(v).and_then(|b| b.binary.version.clone()) })
            .collect())
    }
    async fn resolve(&self, vendor: Vendor) -> Result<ResolvedVendor, IpcError> {
        self.resolved.lock().get(&vendor).cloned().ok_or_else(|| IpcError::unsupported("vendorNotInstalled"))
    }
}

struct FakeAccounts {
    accounts: Mutex<Vec<Account>>,
    quotas: Mutex<Vec<(AccountId, Quota)>>,
    signed_out: Mutex<Vec<AccountId>>,
}

#[async_trait]
impl AccountsPort for FakeAccounts {
    async fn all(&self) -> Vec<Account> {
        self.accounts.lock().clone()
    }
    async fn env_for(&self, id: &AccountId) -> Option<AccountEnv> {
        let a = self.accounts.lock().iter().find(|a| &a.id == id).cloned()?;
        let key = match (a.vendor, a.config_dir.is_some()) {
            (Vendor::Claude, true) => Some("CLAUDE_CONFIG_DIR".to_string()),
            (Vendor::Codex, true) => Some("CODEX_HOME".to_string()),
            _ => None,
        };
        Some(AccountEnv { account_id: a.id, vendor: a.vendor, env_key: key, config_dir: a.config_dir })
    }
    fn observed_quota(&self, id: &AccountId, quota: Quota) {
        self.quotas.lock().push((id.clone(), quota));
    }
    fn signed_out(&self, id: &AccountId) {
        self.signed_out.lock().push(id.clone());
    }
}

struct FakeEnv;

impl EnvSource for FakeEnv {
    fn compose(&self, vendor: Vendor, account: Option<&AccountEnv>, marker: &str) -> BTreeMap<String, String> {
        let mut env = BTreeMap::from([
            ("PATH".to_string(), "/usr/bin:/bin".to_string()),
            ("HOME".to_string(), "/Users/tester".to_string()),
            ("TERM_PROGRAM".to_string(), "gilde-code".to_string()),
            ("CLAUDE_CONFIG_DIR".to_string(), "/leaked/from/gilde".to_string()),
            ("GILDE_SPAWN_ID".to_string(), marker.to_string()),
            ("GILDE_APP_INSTANCE".to_string(), "inst".to_string()),
        ]);
        apply_account_rule(&mut env, vendor, account);
        env
    }
    fn home(&self) -> Option<AbsPath> {
        Some(abs("/Users/tester"))
    }
}

#[derive(Default)]
struct FakeCompat {
    records: Mutex<HashMap<String, ClaudeCompat>>,
}

#[async_trait]
impl CompatApi for FakeCompat {
    fn list(&self) -> Vec<ClaudeCompat> {
        self.records.lock().values().cloned().collect()
    }
    fn lookup(&self, version: &str) -> Option<ClaudeCompat> {
        self.records.lock().get(version).cloned()
    }
    async fn for_binary(&self, binary: &BinaryRef) -> ClaudeCompat {
        let v = binary.version.clone().unwrap_or_default();
        self.lookup(&v).unwrap_or_else(|| compat(&v, CompatState::Unverified, false))
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

#[derive(Default)]
struct FakeLiveness {
    live: Mutex<HashMap<String, Vec<LiveProcess>>>,
    /// `(pid, start)` → still alive.
    alive: Mutex<HashMap<(i32, i64), bool>>,
    finds: AtomicUsize,
}

#[async_trait]
impl Liveness for FakeLiveness {
    async fn find(&self, _vendor: Vendor, id: &str) -> Result<Vec<LiveProcess>, IpcError> {
        self.finds.fetch_add(1, Ordering::SeqCst);
        let all = self.live.lock().get(id).cloned().unwrap_or_default();
        let alive = self.alive.lock();
        Ok(all.into_iter().filter(|p| alive.get(&(p.pid, p.start_sec)).copied().unwrap_or(true)).collect())
    }
    fn alive(&self, pid: i32, start_sec: i64) -> bool {
        self.alive.lock().get(&(pid, start_sec)).copied().unwrap_or(true)
    }
}

#[derive(Default)]
struct FakeSupervisor {
    gate_closed: AtomicBool,
    memory_only: Mutex<Option<String>>,
    signals: Mutex<Vec<(ProcessKey, SignalPlan)>>,
    refuse_signal: AtomicBool,
}

#[async_trait]
impl SupervisorApi for FakeSupervisor {
    async fn spawn_gate(&self) -> Result<(), IpcError> {
        if self.gate_closed.load(Ordering::SeqCst) {
            return Err(IpcError::not_ready("bootSweep"));
        }
        Ok(())
    }
    async fn plan_spawn(&self, _req: SpawnRequest) -> Result<SpawnTicket, IpcError> {
        Err(IpcError::refused("notAllowlisted"))
    }
    async fn mark_running(&self, _t: &SpawnTicket, _pid: i32) -> Result<(), IpcError> {
        Ok(())
    }
    async fn mark_exited(&self, _t: &SpawnTicket, _exit: ExitInfo) {}
    async fn signal(&self, target: &ProcessKey, plan: SignalPlan) -> Result<SignalOutcome, IpcError> {
        if self.refuse_signal.load(Ordering::SeqCst) {
            return Err(IpcError::refused("identityFence"));
        }
        self.signals.lock().push((target.clone(), plan));
        Ok(SignalOutcome { exited: true, survivor: false })
    }
    fn is_vendor_cli(&self, _p: &str) -> bool {
        true
    }
    fn redact_argv(&self, argv: &[String]) -> Vec<String> {
        argv.to_vec()
    }
    fn memory_only(&self) -> Option<String> {
        self.memory_only.lock().clone()
    }
    fn last_scan(&self) -> Option<ProcSnapshot> {
        None
    }
}

#[derive(Default)]
struct FakePaneHost {
    disabled: AtomicBool,
    started: Mutex<Vec<(SessionUid, i32)>>,
    exited: Mutex<Vec<SessionUid>>,
}

#[async_trait]
impl PaneHostApi for FakePaneHost {
    fn lead_env(&self, _lead: &SessionUid) -> Result<LeadEnv, IpcError> {
        if self.disabled.load(Ordering::SeqCst) {
            return Err(IpcError::unsupported("paneHosting"));
        }
        Ok(LeadEnv {
            set: vec![
                ("PATH".to_string(), "/shim:/usr/bin:/bin".to_string()),
                ("GILDE_PANE_HOST".to_string(), "/sock".to_string()),
                ("GILDE_PANE_HOST_TOKEN".to_string(), "secret".to_string()),
                ("CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS".to_string(), "1".to_string()),
            ],
            remove: vec!["TERM_PROGRAM".to_string()],
        })
    }
    fn lead_started(&self, lead: &SessionUid, pid: i32) {
        self.started.lock().push((lead.clone(), pid));
    }
    async fn lead_exited(&self, lead: &SessionUid) {
        self.exited.lock().push(lead.clone());
    }
    async fn send_to_member(&self, _t: &gilde_code_lib::contract::ids::TeamKey, _m: &gilde_code_lib::contract::ids::MemberKey, _x: &str) -> Result<(), IpcError> {
        Ok(())
    }
    async fn close_member_pane(&self, _t: &gilde_code_lib::contract::ids::TeamKey, _m: &gilde_code_lib::contract::ids::MemberKey) -> Result<(), IpcError> {
        Ok(())
    }
}

struct FakeGit {
    fail: Mutex<Option<IpcError>>,
    created: Mutex<Vec<(ProjectId, String, Option<String>)>>,
}

#[async_trait]
impl GitApi for FakeGit {
    async fn changes(&self, _req: &ChangesGetReq) -> Result<ChangesView, IpcError> {
        Err(IpcError::not_built())
    }
    async fn commit_preview(&self, _req: &GitCommitPreviewReq) -> Result<GitCommitPreviewRes, IpcError> {
        Err(IpcError::not_built())
    }
    async fn commit(&self, _req: &GitCommitReq) -> Result<GitCommitRes, IpcError> {
        Err(IpcError::not_built())
    }
    async fn merge_preview(&self, _req: &GitMergePreviewReq) -> Result<GitMergePreviewRes, IpcError> {
        Err(IpcError::not_built())
    }
    async fn merge(&self, _req: &GitMergeReq) -> Result<GitMergeRes, IpcError> {
        Err(IpcError::not_built())
    }
    async fn discard_preview(&self, _req: &WorktreeDiscardPreviewReq) -> Result<WorktreeDiscardPreviewRes, IpcError> {
        Err(IpcError::not_built())
    }
    async fn discard(&self, _req: &WorktreeDiscardReq) -> Result<(), IpcError> {
        Err(IpcError::not_built())
    }
    async fn create_worktree(&self, project: &ProjectId, slug: &str, base: Option<&str>) -> Result<LocationFacts, IpcError> {
        if let Some(e) = self.fail.lock().clone() {
            return Err(e);
        }
        self.created.lock().push((project.clone(), slug.to_string(), base.map(str::to_string)));
        let path = format!("/work/repo-wt/{slug}");
        Ok(LocationFacts {
            project_id: project.clone(),
            project_root: abs("/work/repo"),
            git: true,
            location_id: LocationId::derive(&path),
            location_path: abs(&path),
            worktree: true,
            branch: Some(format!("gilde/{slug}")),
        })
    }
}

#[derive(Default)]
struct FakeQuitItems(Mutex<HashMap<String, Vec<QuitItemRow>>>);

#[async_trait]
impl QuitItems for FakeQuitItems {
    async fn items(&self, record: &QuitRecordId) -> Result<Vec<QuitItemRow>, IpcError> {
        self.0.lock().get(record.as_str()).cloned().ok_or_else(|| IpcError::not_found("quitRecord"))
    }
}

// ---------------------------------------------------------------------------
// the rig
// ---------------------------------------------------------------------------

struct Rig {
    m: Arc<SessionsManager>,
    world: Arc<WorldState>,
    transports: Arc<FakeTransports>,
    binaries: Arc<FakeBinaries>,
    accounts: Arc<FakeAccounts>,
    compat: Arc<FakeCompat>,
    liveness: Arc<FakeLiveness>,
    supervisor: Arc<FakeSupervisor>,
    host: Arc<FakePaneHost>,
    git: Arc<FakeGit>,
    quit: Arc<FakeQuitItems>,
    _env: fixtures::Hermetic,
}

fn timing() -> SessionTiming {
    SessionTiming {
        ready_timeout: Duration::from_secs(5),
        own_leftover_wait: Duration::from_millis(600),
        liveness_poll: Duration::from_millis(20),
        options_wait: Duration::from_millis(500),
        detect_wait: Duration::from_secs(2),
        claude_stop_wait: Duration::from_secs(2),
        watched_term_grace_ms: 100,
        continue_all_parallel: 3,
    }
}

impl Rig {
    fn new() -> Rig {
        Rig::with(true)
    }

    fn with(panehost: bool) -> Rig {
        let env = fixtures::Hermetic::new();
        let world = Arc::new(WorldState::default());
        world.projects.lock().push(Project {
            id: project_id(),
            name: "repo".into(),
            root: abs("/work/repo"),
            kind: ProjectKind::Git,
            order: 0,
            folded: false,
            earlier: false,
            last_activity_at: None,
            live_sessions: 0,
            is_temp: false,
        });
        world.locations.lock().push(Location {
            id: main_location(),
            project_id: project_id(),
            path: abs("/work/repo"),
            kind: LocationKind::Main,
            branch: Some("main".into()),
            worktree_name: None,
            gilde_created: false,
            exists: true,
        });
        let fw = Arc::new(FakeWorld(world.clone()));
        let transports = Arc::new(FakeTransports::default());
        let compat = Arc::new(FakeCompat::default());
        compat.records.lock().insert("2.1.306".into(), compat_rec_verified());
        let binaries = Arc::new(FakeBinaries {
            resolved: Mutex::new(HashMap::from([
                (Vendor::Claude, ResolvedVendor { binary: binary("/opt/claude/2.1.306/claude", "2.1.306"), compat: Some(compat_rec_verified()) }),
                (Vendor::Codex, ResolvedVendor { binary: binary("/opt/codex/codex", "0.145.0"), compat: None }),
                (Vendor::Cursor, ResolvedVendor { binary: binary("/opt/cursor/cursor-agent", "1.0.0"), compat: None }),
            ])),
            detected: AtomicBool::new(true),
        });
        let mut work = account(CLAUDE_WORK, Vendor::Claude, "Work", false);
        work.config_dir = Some(abs("/Users/tester/.claude-work"));
        work.source = AccountSource::User;
        let accounts = Arc::new(FakeAccounts {
            accounts: Mutex::new(vec![
                account(CLAUDE_ACCT, Vendor::Claude, "Default", true),
                work,
                account(CODEX_ACCT, Vendor::Codex, "Default", true),
                account(CURSOR_ACCT, Vendor::Cursor, "Default", true),
            ]),
            quotas: Mutex::new(Vec::new()),
            signed_out: Mutex::new(Vec::new()),
        });
        let liveness = Arc::new(FakeLiveness::default());
        let supervisor = Arc::new(FakeSupervisor::default());
        let host = Arc::new(FakePaneHost::default());
        let git = Arc::new(FakeGit { fail: Mutex::new(None), created: Mutex::new(Vec::new()) });
        let quit = Arc::new(FakeQuitItems::default());
        let deps = SessionDeps {
            world: fw.clone(),
            binder: fw,
            supervisor: supervisor.clone(),
            transports: transports.clone(),
            binaries: binaries.clone(),
            accounts: accounts.clone(),
            env: Arc::new(FakeEnv),
            compat: compat.clone(),
            liveness: liveness.clone(),
            prefs: Arc::new(MemoryPrefs::default()),
            quit_items: quit.clone(),
            git: Some(git.clone()),
            panehost: panehost.then(|| host.clone() as Arc<dyn PaneHostApi>),
            sidecar: None,
            clock: Arc::new(|| NOW),
            timing: timing(),
        };
        Rig {
            m: SessionsManager::new(deps),
            world,
            transports,
            binaries,
            accounts,
            compat,
            liveness,
            supervisor,
            host,
            git,
            quit,
            _env: env,
        }
    }

    fn start_req(&self, vendor: Vendor, account: &str, mode: ControlMode) -> SessionStartReq {
        SessionStartReq {
            client_request_id: cid(),
            project_id: project_id(),
            vendor,
            account_id: AccountId::parse(account).unwrap(),
            model: None,
            location: StartLocation::Here { location_id: None },
            permission_mode: None,
            control_mode: mode,
            team: false,
            prompt: None,
        }
    }

    fn put(&self, s: Session) {
        self.world.sessions.lock().insert(s.uid.clone(), s);
    }

    fn set_state(&self, uid: &SessionUid, state: ActivityState) {
        if let Some(s) = self.world.sessions.lock().get_mut(uid) {
            s.activity.state = state;
        }
    }

    fn binds_for(&self, uid: &SessionUid) -> Vec<String> {
        self.world.binds.lock().iter().filter(|(_, _, u)| u == uid).map(|(s, _, _)| s.clone()).collect()
    }

    fn gilde_facts(&self, spawn_id: &str) -> Vec<GildeFacts> {
        self.world
            .evidence
            .lock()
            .iter()
            .filter(|e| e.hint.gilde_spawn_id.as_deref() == Some(spawn_id))
            .filter_map(|e| match &e.facts {
                SourceFacts::Gilde(g) => Some(g.clone()),
                _ => None,
            })
            .collect()
    }

    async fn start(&self, req: SessionStartReq) -> Result<SessionUid, IpcError> {
        self.m.start(req).await.map(|r| r.uid)
    }

    fn resume(&self, uid: &SessionUid, how: ContinueHow) -> SessionContinueReq {
        SessionContinueReq { client_request_id: cid(), uid: uid.clone(), how, control_mode: None, model: None, permission_mode: None }
    }
}

fn compat_rec_verified() -> ClaudeCompat {
    compat("2.1.306", CompatState::Verified, false)
}

fn err_param(e: &IpcError, name: &str) -> String {
    e.params.get(name).and_then(|v| v.as_str()).unwrap_or_default().to_string()
}

fn assert_err(r: Result<impl std::fmt::Debug, IpcError>, code: ErrorCode, param: Option<(&str, &str)>) {
    let e = r.expect_err("an error");
    assert_eq!(e.code, code, "{e:?}");
    if let Some((k, v)) = param {
        assert_eq!(err_param(&e, k), v, "{e:?}");
    }
}

fn card(id: &str, default_to_no: bool) -> PendingInteraction {
    PendingInteraction {
        request_id: RequestId::parse(id).unwrap(),
        kind: InteractionKind::ToolApproval,
        received_at: NOW,
        on_behalf_of: None,
        answer_in: None,
        title: "Run a command".into(),
        tool: None,
        detail_lines: vec![EvidenceLine::key("card.detailText")],
        options: vec![
            InteractionOption { key: "1".into(), label: "Allow once".into(), role: OptionRole::AllowOnce },
            InteractionOption { key: "3".into(), label: "Deny".into(), role: OptionRole::Deny },
        ],
        always_label: None,
        default_to_no,
        degraded: false,
        prompt_rev: None,
        confidence: Confidence::High,
        multi_select: false,
        questions: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// session_start
// ---------------------------------------------------------------------------

/// Conversation start: the exact launch, the first prompt, the spawn binding
/// on the driver's first evidence (A18.1/A20.1) and the transport (L1).
#[tokio::test]
async fn t_sessions_start_conversation_binds_the_spawn_and_records_l1() {
    let r = Rig::new();
    let mut req = r.start_req(Vendor::Claude, CLAUDE_ACCT, ControlMode::Conversation);
    req.prompt = Some("Add rate limiting".into());
    req.model = Some("opus".into());
    req.permission_mode = Some(PermissionMode::AcceptEdits);
    let uid = r.start(req).await.unwrap();

    let specs = r.transports.convo.lock().clone();
    assert_eq!(specs.len(), 1);
    let spec = &specs[0];
    assert_eq!(spec.uid, uid);
    let LaunchKind::New { session_id } = &spec.launch else { panic!("a new launch: {:?}", spec.launch) };
    assert!(uuid::Uuid::parse_str(session_id).is_ok_and(|u| u.get_version_num() == 4), "--session-id is a Gilde-minted v4");
    assert_eq!(spec.hint.vendor_session_id.as_deref(), Some(session_id.as_str()));
    assert_eq!(spec.permission_mode, PermissionMode::AcceptEdits);
    assert_eq!(spec.model.as_deref(), Some("opus"));
    assert!(!spec.allow_bypass);
    assert_eq!(spec.first_prompt.as_ref().map(|m| m.text.as_str()), Some("Add rate limiting"));
    assert_eq!(spec.binary.version.as_deref(), Some("2.1.306"), "the spawn-time BinaryRef [A1]");
    assert_eq!(spec.cwd.as_str(), "/work/repo");
    // The env composer: the default login's CLAUDE_CONFIG_DIR stays unset (C8).
    let env = (spec.env)("mk_1");
    assert!(!env.contains_key("CLAUDE_CONFIG_DIR"));
    assert_eq!(env["GILDE_SPAWN_ID"], "mk_1");

    assert_eq!(r.binds_for(&uid), vec!["sp_c1".to_string()], "bound on the driver's first evidence");
    let ctrl = r.world.controls.lock().get(&uid).cloned().unwrap();
    assert_eq!(ctrl.transport, Some(Transport::L1));
    assert!(r.m.actor(&uid).unwrap().is_live().await);
}

/// T-env-defaultAccount through the start path: another account's env names
/// its realpath, the default's does not (C8, PM-30).
#[tokio::test]
async fn t_env_default_account_on_start() {
    let r = Rig::new();
    let uid = r.start(r.start_req(Vendor::Claude, CLAUDE_WORK, ControlMode::Conversation)).await.unwrap();
    let spec = r.transports.convo.lock().iter().find(|s| s.uid == uid).cloned().unwrap();
    assert_eq!((spec.env)("m")["CLAUDE_CONFIG_DIR"], "/Users/tester/.claude-work");
    let uid2 = r.start(r.start_req(Vendor::Claude, CLAUDE_ACCT, ControlMode::Terminal)).await.unwrap();
    let pty = r.transports.pty.lock().iter().find(|s| s.parent_uid.as_ref() == Some(&uid2)).cloned().unwrap();
    assert!(pty.env.iter().all(|(k, _)| k != "CLAUDE_CONFIG_DIR"), "Terminal too: unset for the default login");
}

/// Terminal start: argv per §6.2.1 (always `--permission-mode`), parent_uid,
/// the bind right after the open (A20.1), Gilde facts, the Pty transport.
#[tokio::test]
async fn t_sessions_start_terminal_argv_bind_and_facts() {
    let r = Rig::new();
    let mut req = r.start_req(Vendor::Claude, CLAUDE_ACCT, ControlMode::Terminal);
    req.prompt = Some("fix the build".into());
    let uid = r.start(req).await.unwrap();
    let spec = r.transports.pty.lock()[0].clone();
    assert_eq!(spec.parent_uid.as_ref(), Some(&uid));
    assert_eq!(spec.argv0.as_str(), "/opt/claude/2.1.306/claude", "argv[0] = the checked realpath");
    assert_eq!(spec.args[0], "--session-id");
    assert!(uuid::Uuid::parse_str(&spec.args[1]).is_ok());
    assert_eq!(&spec.args[2..4], &["--permission-mode".to_string(), "default".to_string()]);
    assert_eq!(spec.args[spec.args.len() - 2..], ["--".to_string(), "fix the build".to_string()], "new claude: prompt on argv");
    assert_eq!(r.binds_for(&uid), vec!["sp_t1".to_string()]);
    let g = r.gilde_facts("sp_t1");
    assert_eq!(g[0].mode, ControlMode::Terminal);
    let ctrl = r.world.controls.lock().get(&uid).cloned().unwrap();
    assert_eq!(ctrl.transport, Some(Transport::Pty));
    assert!(ctrl.pty.is_some());
    // Ready (the fake PTY is idle at once): ready_at is published.
    eventually("ready_at", || r.gilde_facts("sp_t1").iter().any(|g| g.ready_at.is_some())).await;
    // The binary recorded on the session's metadata [A1].
    let meta = r
        .world
        .evidence
        .lock()
        .iter()
        .find_map(|e| match &e.facts {
            SourceFacts::Meta(m) if e.hint.gilde_spawn_id.as_deref() == Some("sp_t1") => Some(m.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(meta.binary.as_ref().and_then(|b| b.version.clone()).as_deref(), Some("2.1.306"));
    assert_eq!(meta.claude_version.as_deref(), Some("2.1.306"));
}

/// Typed after ready (§6.2.2): a resumed codex Terminal session gets its
/// first prompt only once ready, through the actor.
#[tokio::test]
async fn t_sessions_typed_first_prompt_waits_for_ready() {
    let r = Rig::new();
    *r.transports.ready.lock() = Some(false);
    let uid = SessionUid::mint();
    r.put(session(&uid, Vendor::Codex, Some(CODEX_ACCT), Some("0ff2360d-9ebf-4152-ad44-5fb0754c7823"), ended(), ActivityState::Ended));
    let mut req = r.resume(&uid, ContinueHow::Resume);
    req.control_mode = Some(ControlMode::Terminal);
    let started = r.m.continue_session(req).await.unwrap().uid;
    assert_eq!(started, uid, "Resume keeps the uid");
    let spec = r.transports.pty.lock()[0].clone();
    assert_eq!(&spec.args[..2], &["resume".to_string(), "0ff2360d-9ebf-4152-ad44-5fb0754c7823".to_string()]);
    // Not ready: nothing typed. (A resumed session has no first prompt here,
    // so check a codex new start with a prompt that must be typed: `-x`.)
    let mut req = r.start_req(Vendor::Codex, CODEX_ACCT, ControlMode::Terminal);
    req.prompt = Some("-x is a flag-like prompt".into());
    *r.transports.ready.lock() = None;
    let uid2 = r.start(req).await.unwrap();
    let log = r.transports.log_of(&uid2);
    eventually("the typed first prompt", || log.sends.lock().len() == 1).await;
    assert_eq!(log.sends.lock()[0].text, "-x is a flag-like prompt");
    assert!(r.transports.pty.lock()[1].args.iter().all(|a| a != "-x is a flag-like prompt"), "never on argv");
}

/// Hosted lead (§6.2.1, §7.4.1): `--teammate-mode tmux`, `--permission-mode`
/// and the inline `--settings`, PaneHost's env, `lead_started`/`lead_exited`.
#[tokio::test]
async fn t_sessions_hosted_lead_env_and_argv() {
    let r = Rig::new();
    let mut req = r.start_req(Vendor::Claude, CLAUDE_ACCT, ControlMode::Terminal);
    req.team = true;
    req.permission_mode = Some(PermissionMode::Plan);
    let uid = r.start(req).await.unwrap();
    let spec = r.transports.pty.lock()[0].clone();
    let a = &spec.args;
    let pos = |f: &str| a.iter().position(|x| x == f).unwrap_or_else(|| panic!("{f} in {a:?}"));
    assert_eq!(a[pos("--teammate-mode") + 1], "tmux");
    assert_eq!(a[pos("--permission-mode") + 1], "plan");
    assert_eq!(a[pos("--settings") + 1], r#"{"permissions":{"defaultMode":"plan"}}"#);
    let env: HashMap<_, _> = spec.env.iter().cloned().collect();
    assert_eq!(env["PATH"], "/shim:/usr/bin:/bin");
    assert_eq!(env["GILDE_PANE_HOST_TOKEN"], "secret");
    assert!(!env.contains_key("TERM_PROGRAM"), "LeadEnv.remove applied");
    assert_eq!(r.host.started.lock()[0].0, uid);
    assert!(r.world.controls.lock()[&uid].hosted_team);
    // The lead exits → PaneHost hears it.
    r.transports.exits.lock()[0].exited(&PtyId::mint(), ExitInfo { code: Some(0), signal: None }, NOW);
    eventually("lead_exited", || r.host.exited.lock().contains(&uid)).await;
    assert!(!r.m.actor(&uid).unwrap().is_live().await);
}

/// No pane hosting → the lead runs its teammates in-process (§7.4.9).
#[tokio::test]
async fn t_sessions_team_without_hosting_runs_in_process() {
    let r = Rig::with(false);
    let mut req = r.start_req(Vendor::Claude, CLAUDE_ACCT, ControlMode::Terminal);
    req.team = true;
    let uid = r.start(req).await.unwrap();
    let spec = r.transports.pty.lock()[0].clone();
    let i = spec.args.iter().position(|x| x == "--teammate-mode").unwrap();
    assert_eq!(spec.args[i + 1], "in-process");
    assert!(spec.env.iter().any(|(k, v)| k == "CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS" && v == "1"));
    assert!(!r.world.controls.lock()[&uid].hosted_team);
}

/// The §4.1 order of checks and every refusal of `session_start`.
#[tokio::test]
async fn t_sessions_start_refusals() {
    let r = Rig::new();
    // value rules
    let mut q = r.start_req(Vendor::Claude, CLAUDE_ACCT, ControlMode::Conversation);
    q.model = Some("gpt-9".into());
    assert_err(r.start(q).await, ErrorCode::InvalidArgument, Some(("field", "model")));
    let mut q = r.start_req(Vendor::Codex, CODEX_ACCT, ControlMode::TurnBased);
    q.permission_mode = Some(PermissionMode::BypassPermissions);
    assert_err(r.start(q).await, ErrorCode::Refused, Some(("reason", "bypassNotAllowed")));
    let mut q = r.start_req(Vendor::Codex, CLAUDE_ACCT, ControlMode::TurnBased);
    q.account_id = claude_account();
    assert_err(r.start(q).await, ErrorCode::InvalidArgument, Some(("field", "accountId")));
    // existence
    let mut q = r.start_req(Vendor::Claude, CLAUDE_ACCT, ControlMode::Conversation);
    q.project_id = ProjectId::derive("/nowhere");
    assert_err(r.start(q).await, ErrorCode::NotFound, Some(("entity", "project")));
    let mut q = r.start_req(Vendor::Claude, CLAUDE_ACCT, ControlMode::Conversation);
    q.location = StartLocation::ExistingWorktree { location_id: LocationId::derive("/x") };
    assert_err(r.start(q).await, ErrorCode::NotFound, Some(("entity", "location")));
}

#[tokio::test]
async fn t_sessions_start_capability_refusals() {
    let r = Rig::new();
    assert_err(
        r.start(r.start_req(Vendor::Claude, CLAUDE_ACCT, ControlMode::TurnBased)).await,
        ErrorCode::Unsupported,
        Some(("cap", "modeNotAllowed")),
    );
    let mut q = r.start_req(Vendor::Claude, CLAUDE_ACCT, ControlMode::Conversation);
    q.team = true;
    assert_err(r.start(q).await, ErrorCode::Unsupported, Some(("cap", "teamNotAllowed")));
    // A discovered, unconfirmed account (PM-44) and a signed-out one.
    r.accounts.accounts.lock()[1].usable_for_new = false;
    assert_err(
        r.start(r.start_req(Vendor::Claude, CLAUDE_WORK, ControlMode::Conversation)).await,
        ErrorCode::Refused,
        Some(("reason", "accountNotConfirmed")),
    );
    r.accounts.accounts.lock()[0].sign_in = SignIn::SignedOut;
    assert_err(
        r.start(r.start_req(Vendor::Claude, CLAUDE_ACCT, ControlMode::Conversation)).await,
        ErrorCode::Refused,
        Some(("reason", "signInNeeded")),
    );
    let r = Rig::new();
    r.binaries.resolved.lock().remove(&Vendor::Codex);
    assert_err(
        r.start(r.start_req(Vendor::Codex, CODEX_ACCT, ControlMode::TurnBased)).await,
        ErrorCode::Unsupported,
        Some(("cap", "vendorNotInstalled")),
    );
}

/// §3.10: a below-minimum or Failed version refuses Conversation at start; a
/// raced driver refusal starts Terminal mode instead.
#[tokio::test]
async fn t_sessions_conversation_compat_at_start() {
    let r = Rig::new();
    r.binaries.resolved.lock().get_mut(&Vendor::Claude).unwrap().compat = Some(compat("2.1.200", CompatState::Unverified, true));
    assert_err(
        r.start(r.start_req(Vendor::Claude, CLAUDE_ACCT, ControlMode::Conversation)).await,
        ErrorCode::Unsupported,
        Some(("cap", "conversationMinVersion")),
    );
    r.binaries.resolved.lock().get_mut(&Vendor::Claude).unwrap().compat = Some(compat("2.1.300", CompatState::Failed, false));
    assert_err(
        r.start(r.start_req(Vendor::Claude, CLAUDE_ACCT, ControlMode::Conversation)).await,
        ErrorCode::Unsupported,
        Some(("cap", "conversationUnavailable")),
    );
    let r = Rig::new();
    *r.transports.convo_fail.lock() = Some(IpcError::unsupported("conversation"));
    let uid = r.start(r.start_req(Vendor::Claude, CLAUDE_ACCT, ControlMode::Conversation)).await.unwrap();
    assert_eq!(r.transports.pty.lock()[0].parent_uid.as_ref(), Some(&uid), "fell back to Terminal mode");
    assert_eq!(r.m.actor(&uid).unwrap().info().mode, ControlMode::Terminal);
}

/// Spawn gate and memory-only mode (§2.4).
#[tokio::test]
async fn t_sessions_gate_and_memory_only() {
    let r = Rig::new();
    r.supervisor.gate_closed.store(true, Ordering::SeqCst);
    assert_err(
        r.start(r.start_req(Vendor::Claude, CLAUDE_ACCT, ControlMode::Conversation)).await,
        ErrorCode::NotReady,
        Some(("reason", "bootSweep")),
    );
    r.supervisor.gate_closed.store(false, Ordering::SeqCst);
    *r.supervisor.memory_only.lock() = Some("diskFull".into());
    assert_err(r.start(r.start_req(Vendor::Claude, CLAUDE_ACCT, ControlMode::Conversation)).await, ErrorCode::DiskFull, None);
    assert!(r.transports.convo.lock().is_empty(), "nothing spawned");
}

/// NewWorktree: the worktree through GitApi, World hears the location, the
/// session runs there; a git refusal is passed through (A17.4).
#[tokio::test]
async fn t_sessions_new_worktree() {
    let r = Rig::new();
    let mut q = r.start_req(Vendor::Claude, CLAUDE_ACCT, ControlMode::Conversation);
    q.location = StartLocation::NewWorktree { slug: "rate-limit".into(), base: Some("main".into()) };
    let uid = r.start(q).await.unwrap();
    assert_eq!(r.git.created.lock()[0].1, "rate-limit");
    assert_eq!(r.world.location_facts.lock()[0].location_path.as_str(), "/work/repo-wt/rate-limit");
    let spec = r.transports.convo.lock().iter().find(|s| s.uid == uid).cloned().unwrap();
    assert_eq!(spec.cwd.as_str(), "/work/repo-wt/rate-limit");
    *r.git.fail.lock() = Some(IpcError::refused("worktreeGate").with_detail("fatal: dirty"));
    let mut q = r.start_req(Vendor::Claude, CLAUDE_ACCT, ControlMode::Conversation);
    q.location = StartLocation::NewWorktree { slug: "other".into(), base: None };
    assert_err(r.start(q).await, ErrorCode::Refused, Some(("reason", "worktreeGate")));
}

/// T-sessions-idempotent: a double click (two concurrent calls with one
/// `clientRequestId`) spawns once and both get the same uid; a later repeat
/// within 60 s too.
#[tokio::test]
async fn t_sessions_idempotent_double_click() {
    let r = Rig::new();
    *r.transports.spawn_delay.lock() = Duration::from_millis(100);
    let req = r.start_req(Vendor::Claude, CLAUDE_ACCT, ControlMode::Conversation);
    let (a, b) = tokio::join!(r.m.start(req.clone()), r.m.start(req.clone()));
    let (a, b) = (a.unwrap().uid, b.unwrap().uid);
    assert_eq!(a, b);
    assert_eq!(r.transports.convo.lock().len(), 1, "one spawn");
    assert_eq!(r.m.start(req).await.unwrap().uid, a);
    assert_eq!(r.transports.convo.lock().len(), 1);
}

/// §4.1 rule 6: the spawn class (5 per 10 s) answers `BUSY{retryAfterMs}`.
#[tokio::test]
async fn t_sessions_spawn_rate_class() {
    let r = Rig::new();
    for _ in 0..5 {
        r.start(r.start_req(Vendor::Claude, CLAUDE_ACCT, ControlMode::Conversation)).await.unwrap();
    }
    let e = r.start(r.start_req(Vendor::Claude, CLAUDE_ACCT, ControlMode::Conversation)).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Busy);
    assert!(e.params["retryAfterMs"].as_u64().unwrap() > 0);
}

// ---------------------------------------------------------------------------
// session_continue (§6.5)
// ---------------------------------------------------------------------------

const VSID: &str = "6a1e2b0c-1111-4222-8333-444455556666";

#[tokio::test]
async fn t_sessions_resume_keeps_the_uid_in_its_last_mode() {
    let r = Rig::new();
    let uid = SessionUid::mint();
    let mut s = session(&uid, Vendor::Claude, Some(CLAUDE_ACCT), Some(VSID), ended(), ActivityState::Ended);
    s.details.control_mode = Some(ControlMode::Conversation);
    s.model = Some("sonnet".into());
    r.put(s);
    let got = r.m.continue_session(r.resume(&uid, ContinueHow::Resume)).await.unwrap().uid;
    assert_eq!(got, uid);
    let spec = r.transports.convo.lock()[0].clone();
    assert_eq!(spec.launch, LaunchKind::Continue { session_id: VSID.into() });
    assert_eq!(spec.model.as_deref(), Some("sonnet"));
    assert_eq!(r.binds_for(&uid), vec!["sp_c1".to_string()], "bind_spawn(spawn, Some(uid)) keeps the uid (A20.1)");
}

/// T-sessions-liveElsewhere: the re-check inside the command refuses a resume
/// of a conversation that is live anywhere; a copy (Fork) is allowed.
#[tokio::test]
async fn t_sessions_live_elsewhere() {
    let r = Rig::new();
    let uid = SessionUid::mint();
    r.put(session(&uid, Vendor::Claude, Some(CLAUDE_ACCT), Some(VSID), ended(), ActivityState::Ended));
    r.liveness.live.lock().insert(VSID.into(), vec![LiveProcess { pid: 777, start_sec: 5, background: false }]);
    assert_err(r.m.continue_session(r.resume(&uid, ContinueHow::Resume)).await, ErrorCode::SessionLiveElsewhere, None);
    assert!(r.transports.convo.lock().is_empty(), "never resumed live in place");
    let copy = r.m.continue_session(r.resume(&uid, ContinueHow::Fork)).await.unwrap().uid;
    assert_ne!(copy, uid, "Fork creates a new uid");
    assert_eq!(r.world.parents.lock().get(&copy), Some(&uid), "the manager records the parent in World (A24.2)");
    assert_eq!(r.m.parent_of(&copy), Some(uid.clone()));
    assert_eq!(r.transports.convo.lock()[0].launch, LaunchKind::Copy { session_id: VSID.into() });
}

/// A background job: Resume → `REFUSED{heldByBackground}`, Attach → `claude
/// attach <id>` in a PTY (same uid).
#[tokio::test]
async fn t_sessions_background_attach() {
    let r = Rig::new();
    let uid = SessionUid::mint();
    r.put(session(&uid, Vendor::Claude, Some(CLAUDE_ACCT), Some(VSID), watching(Origin::Background), ActivityState::Working));
    r.liveness.live.lock().insert(VSID.into(), vec![LiveProcess { pid: 778, start_sec: 6, background: true }]);
    assert_err(r.m.continue_session(r.resume(&uid, ContinueHow::Resume)).await, ErrorCode::Refused, Some(("reason", "heldByBackground")));
    let got = r.m.continue_session(r.resume(&uid, ContinueHow::Attach)).await.unwrap().uid;
    assert_eq!(got, uid);
    assert_eq!(r.transports.pty.lock()[0].args, vec!["attach".to_string(), VSID.to_string()]);
}

/// The race: two Continues of one conversation (different clientRequestIds)
/// — the identity lock lets one spawn; the other answers `CONFLICT{alreadyOwned}`.
#[tokio::test]
async fn t_sessions_continue_race_spawns_once() {
    let r = Rig::new();
    *r.transports.spawn_delay.lock() = Duration::from_millis(150);
    let uid = SessionUid::mint();
    r.put(session(&uid, Vendor::Claude, Some(CLAUDE_ACCT), Some(VSID), ended(), ActivityState::Ended));
    let (a, b) = tokio::join!(
        r.m.continue_session(r.resume(&uid, ContinueHow::Resume)),
        r.m.continue_session(r.resume(&uid, ContinueHow::Resume))
    );
    let oks = [a.is_ok(), b.is_ok()].iter().filter(|x| **x).count();
    assert_eq!(oks, 1, "exactly one Continue spawns: {a:?} {b:?}");
    let e = a.err().or(b.err()).unwrap();
    assert_eq!((e.code, err_param(&e, "changed")), (ErrorCode::Conflict, "alreadyOwned".to_string()));
    assert_eq!(r.transports.convo.lock().len(), 1, "never two live processes for one session");
}

/// T-sessions-crashLeftover [F-12]: this uid's own crash leftover is waited
/// for (the row says "Stopping the previous copy…"), then the session
/// resumes; it is never "elsewhere". A leftover that does not exit in time
/// → `NOT_READY` (retry), never a copy.
#[tokio::test]
async fn t_sessions_crash_leftover() {
    let r = Rig::new();
    let uid = SessionUid::mint();
    r.put(session(&uid, Vendor::Claude, Some(CLAUDE_ACCT), Some(VSID), ended(), ActivityState::Ended));
    let leftover = |pid: i32| ProcessRecord {
        key: ProcessKey::new(pid as u32, 9),
        pid,
        start_sec: 9,
        vendor: Some(Vendor::Claude),
        role: ProcRole::Session,
        session_uid: Some(uid.clone()),
        gilde_owned: true,
        rss_bytes: 0,
        leftover: Some(Leftover { reason: LeftoverReason::GildeCrashLeftover, since: NOW, held_scans: 2 }),
    };
    r.world.leftovers.lock().push(leftover(901));
    r.liveness.live.lock().insert(VSID.into(), vec![LiveProcess { pid: 901, start_sec: 9, background: false }]);
    // It exits shortly after the wait began.
    let lv = r.liveness.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        lv.alive.lock().insert((901, 9), false);
    });
    let got = r.m.continue_session(r.resume(&uid, ContinueHow::Resume)).await.unwrap().uid;
    assert_eq!(got, uid);
    let lines: Vec<String> = r.world.details.lock().iter().flat_map(|(_, d)| d.lines.iter().map(|l| l.key.clone())).collect();
    assert!(lines.contains(&"home.stoppingPrevious".to_string()));

    // A leftover that stays: NOT_READY, nothing spawned, no copy.
    let r = Rig::new();
    let uid2 = SessionUid::mint();
    r.put(session(&uid2, Vendor::Claude, Some(CLAUDE_ACCT), Some(VSID), ended(), ActivityState::Ended));
    r.world.leftovers.lock().push(ProcessRecord { session_uid: Some(uid2.clone()), ..leftover(902) });
    r.liveness.live.lock().insert(VSID.into(), vec![LiveProcess { pid: 902, start_sec: 9, background: false }]);
    assert_err(r.m.continue_session(r.resume(&uid2, ContinueHow::Resume)).await, ErrorCode::NotReady, Some(("reason", "stoppingPrevious")));
    assert!(r.transports.convo.lock().is_empty());
}

#[tokio::test]
async fn t_sessions_continue_refusals() {
    let r = Rig::new();
    // no vendor id
    let uid = SessionUid::mint();
    r.put(session(&uid, Vendor::Claude, Some(CLAUDE_ACCT), None, ended(), ActivityState::Ended));
    assert_err(r.m.continue_session(r.resume(&uid, ContinueHow::Resume)).await, ErrorCode::Refused, Some(("reason", "notResumable")));
    // Fork of a codex session: not a continue option
    let codex = SessionUid::mint();
    r.put(session(&codex, Vendor::Codex, Some(CODEX_ACCT), Some(VSID), ended(), ActivityState::Ended));
    assert_err(r.m.continue_session(r.resume(&codex, ContinueHow::Fork)).await, ErrorCode::Unsupported, Some(("cap", "continueOption")));
    // a cursor id that is not on disk (PM-23)
    let cur = SessionUid::mint();
    r.put(session(&cur, Vendor::Cursor, Some(CURSOR_ACCT), Some(VSID), ended(), ActivityState::Ended));
    assert_err(r.m.continue_session(r.resume(&cur, ContinueHow::Resume)).await, ErrorCode::Refused, Some(("reason", "idNotOnDisk")));
    // bypass is never chosen on a continue (PM-03)
    let mut q = r.resume(&uid, ContinueHow::Resume);
    q.permission_mode = Some(PermissionMode::BypassPermissions);
    assert_err(r.m.continue_session(q).await, ErrorCode::InvalidArgument, Some(("field", "permissionMode")));
    // unknown session
    assert_err(r.m.continue_session(r.resume(&SessionUid::mint(), ContinueHow::Resume)).await, ErrorCode::NotFound, None);
}

/// Live In Gilde → `CONFLICT{alreadyOwned}`.
#[tokio::test]
async fn t_sessions_continue_of_an_own_live_session_conflicts() {
    let r = Rig::new();
    let uid = r.start(r.start_req(Vendor::Claude, CLAUDE_ACCT, ControlMode::Conversation)).await.unwrap();
    let vsid = r.transports.convo.lock()[0].hint.vendor_session_id.clone().unwrap();
    r.put(session(&uid, Vendor::Claude, Some(CLAUDE_ACCT), Some(&vsid), ended(), ActivityState::Ready));
    assert_err(r.m.continue_session(r.resume(&uid, ContinueHow::Resume)).await, ErrorCode::Conflict, Some(("changed", "alreadyOwned")));
}

/// OtherAccount (§6.5): Gilde stops its own live process first, then starts
/// a copy by transcript path under the other account (new uid).
#[tokio::test]
async fn t_sessions_continue_on_another_account() {
    let r = Rig::new();
    let uid = r.start(r.start_req(Vendor::Claude, CLAUDE_ACCT, ControlMode::Conversation)).await.unwrap();
    let vsid = r.transports.convo.lock()[0].hint.vendor_session_id.clone().unwrap();
    r.put(session(&uid, Vendor::Claude, Some(CLAUDE_ACCT), Some(&vsid), ended(), ActivityState::Error {
        sub: ErrorSub::UsageLimit { account_id: Some(claude_account()), resets_at: None },
    }));
    r.world.transcripts.lock().insert(uid.clone(), abs("/Users/tester/.claude/projects/p/x.jsonl"));
    let mut q = r.resume(&uid, ContinueHow::OtherAccount { account_id: AccountId::parse(CLAUDE_WORK).unwrap() });
    q.control_mode = Some(ControlMode::Conversation);
    let copy = r.m.continue_session(q).await.unwrap().uid;
    assert_ne!(copy, uid);
    assert_eq!(r.transports.log_of(&uid).stops.lock().as_slice(), &[StopCause::User], "own process stopped first");
    let spec = r.transports.convo.lock().iter().find(|s| s.uid == copy).cloned().unwrap();
    assert_eq!(spec.launch, LaunchKind::OtherAccount { transcript: abs("/Users/tester/.claude/projects/p/x.jsonl") });
    assert_eq!(spec.account_id.as_ref().map(|a| a.as_str()), Some(CLAUDE_WORK));
}

/// Continue all (§6.5 rule 4): quit order, ≤ 3 in flight, a failing item is
/// listed and never aborts the rest.
#[tokio::test]
async fn t_sessions_continue_all_partial_failure() {
    let r = Rig::new();
    *r.transports.spawn_delay.lock() = Duration::from_millis(80);
    let mut items = Vec::new();
    let mut uids = Vec::new();
    for i in 0..6 {
        let uid = SessionUid::mint();
        let vsid = format!("6a1e2b0c-1111-4222-8333-44445555666{i}");
        r.put(session(&uid, Vendor::Claude, Some(CLAUDE_ACCT), Some(&vsid), ended(), ActivityState::Stopped));
        if i == 2 {
            r.liveness.live.lock().insert(vsid.clone(), vec![LiveProcess { pid: 99, start_sec: 1, background: false }]);
        }
        items.push(QuitItemRow {
            uid: uid.as_str().to_string(),
            control_mode: if i == 4 { "terminal".into() } else { "conversation".into() },
            permission_mode: Some("plan".into()),
            model: None,
            was_waiting: None,
            team_lead: false,
            survivor: false,
        });
        uids.push(uid);
    }
    // An item whose session World does not know.
    let ghost = SessionUid::mint();
    items.push(QuitItemRow {
        uid: ghost.as_str().to_string(),
        control_mode: "conversation".into(),
        permission_mode: None,
        model: None,
        was_waiting: None,
        team_lead: false,
        survivor: false,
    });
    let record = QuitRecordId::mint();
    r.quit.0.lock().insert(record.as_str().to_string(), items);
    let res = r.m.continue_all(&record, None).await.unwrap();
    assert_eq!(res.started.len(), 5);
    assert_eq!(res.failed.len(), 2);
    assert_eq!(res.failed[0].uid, uids[2]);
    assert_eq!(res.failed[0].error.code, ErrorCode::SessionLiveElsewhere);
    assert_eq!(res.failed[1].uid, ghost);
    assert!(r.transports.max_in_flight.load(Ordering::SeqCst) <= 3, "≤ 3 spawns in flight");
    assert!(r.transports.max_in_flight.load(Ordering::SeqCst) >= 2, "they do run in parallel");
    // The item's mode and permission mode are used.
    assert!(r.transports.pty.lock().iter().any(|s| s.parent_uid.as_ref() == Some(&uids[4])));
    assert!(r.transports.convo.lock().iter().all(|s| s.permission_mode == PermissionMode::Plan));
    // A subset by uid.
    let res = r.m.continue_all(&record, Some(vec![uids[2].clone()])).await.unwrap();
    assert_eq!(res.started.len() + res.failed.len(), 1);
}

// ---------------------------------------------------------------------------
// stop
// ---------------------------------------------------------------------------

#[tokio::test]
async fn t_sessions_stop_own_uses_the_driver_stop_sequence() {
    let r = Rig::new();
    let uid = r.start(r.start_req(Vendor::Claude, CLAUDE_ACCT, ControlMode::Conversation)).await.unwrap();
    r.put(session(&uid, Vendor::Claude, Some(CLAUDE_ACCT), None, ended(), ActivityState::Ready));
    let p = r.m.stop_preview(&uid).await.unwrap();
    assert_eq!(p.how, StopMethod::Fenced);
    // A25.3: the own-session sheet's sentence, with no count param.
    assert_eq!(p.owner_text.key, "sheet.stopSession");
    assert!(p.owner_text.params.is_empty(), "{:?}", p.owner_text.params);
    let res = r.m.stop(SessionStopReq { uid: uid.clone(), confirm_token: p.confirm_token.clone() }).await.unwrap();
    assert!(res.stopped);
    assert_eq!(r.transports.log_of(&uid).stops.lock().as_slice(), &[StopCause::User]);
    // single use
    assert_err(r.m.stop(SessionStopReq { uid: uid.clone(), confirm_token: p.confirm_token }).await, ErrorCode::Conflict, Some(("changed", "changed")));
    // a made-up token
    assert_err(
        r.m.stop(SessionStopReq { uid: uid.clone(), confirm_token: ConfirmToken::mint() }).await,
        ErrorCode::Conflict,
        None,
    );
    // now it is not live
    assert_err(r.m.stop_preview(&uid).await, ErrorCode::Unsupported, Some(("cap", "notLive")));
}

/// Terminal stop: Gilde facts carry the stop cause and the signals before the
/// exit, so World reads the exit as Stopped (R1), never Crashed.
#[tokio::test]
async fn t_sessions_terminal_stop_publishes_the_cause_first() {
    let r = Rig::new();
    let uid = r.start(r.start_req(Vendor::Claude, CLAUDE_ACCT, ControlMode::Terminal)).await.unwrap();
    r.put(session(&uid, Vendor::Claude, Some(CLAUDE_ACCT), None, ended(), ActivityState::Ready));
    let p = r.m.stop_preview(&uid).await.unwrap();
    r.m.stop(SessionStopReq { uid: uid.clone(), confirm_token: p.confirm_token }).await.unwrap();
    let g = r.gilde_facts("sp_t1");
    let last = g.last().unwrap();
    assert_eq!(last.stop_cause, Some(StopCause::User));
    assert!(last.signals_sent.contains(&libc::SIGHUP) && last.signals_sent.contains(&libc::SIGTERM));
}

/// A watched session: identity-fenced SIGTERM to the processes the preview
/// named; a changed set → `CONFLICT{changed}`; nothing identified →
/// `REFUSED{identityUnverified}`; never SIGKILL.
#[tokio::test]
async fn t_sessions_stop_watched() {
    let r = Rig::new();
    let uid = SessionUid::mint();
    r.put(session(&uid, Vendor::Claude, Some(CLAUDE_ACCT), Some(VSID), watching(Origin::Terminal), ActivityState::Working));
    r.liveness.live.lock().insert(VSID.into(), vec![LiveProcess { pid: 4321, start_sec: 77, background: false }]);
    let p = r.m.stop_preview(&uid).await.unwrap();
    assert_eq!(p.how, StopMethod::TermAfterIdentity);
    assert_eq!(p.owner_text.key, "sheet.stopWatched");
    assert_eq!(p.owner_text.params["owner"], "Terminal");
    let res = r.m.stop(SessionStopReq { uid: uid.clone(), confirm_token: p.confirm_token }).await.unwrap();
    assert!(res.stopped);
    let sig = r.supervisor.signals.lock()[0].clone();
    assert_eq!(sig.0, ProcessKey::new(4321, 77));
    assert!(matches!(sig.1, SignalPlan::Term { .. }));
    // The process set changed between preview and stop.
    let p = r.m.stop_preview(&uid).await.unwrap();
    r.liveness.live.lock().insert(VSID.into(), vec![LiveProcess { pid: 4322, start_sec: 78, background: false }]);
    assert_err(r.m.stop(SessionStopReq { uid: uid.clone(), confirm_token: p.confirm_token }).await, ErrorCode::Conflict, None);
    // Nothing identifiable.
    r.liveness.live.lock().clear();
    let p = r.m.stop_preview(&uid).await.unwrap();
    assert_err(
        r.m.stop(SessionStopReq { uid: uid.clone(), confirm_token: p.confirm_token }).await,
        ErrorCode::Refused,
        Some(("reason", "identityUnverified")),
    );
    // A refused fence.
    r.liveness.live.lock().insert(VSID.into(), vec![LiveProcess { pid: 4323, start_sec: 79, background: false }]);
    r.supervisor.refuse_signal.store(true, Ordering::SeqCst);
    let p = r.m.stop_preview(&uid).await.unwrap();
    assert_err(
        r.m.stop(SessionStopReq { uid: uid.clone(), confirm_token: p.confirm_token }).await,
        ErrorCode::Refused,
        Some(("reason", "identityUnverified")),
    );
    // A Claude background job is stopped with `claude stop <id>` (§9.8).
    let bg = SessionUid::mint();
    r.put(session(&bg, Vendor::Claude, Some(CLAUDE_ACCT), Some(VSID), watching(Origin::Background), ActivityState::Working));
    assert_eq!(r.m.stop_preview(&bg).await.unwrap().how, StopMethod::ClaudeStop);
    // A session that runs nowhere.
    let gone = SessionUid::mint();
    r.put(session(&gone, Vendor::Claude, Some(CLAUDE_ACCT), Some(VSID), ended(), ActivityState::Ended));
    assert_err(r.m.stop_preview(&gone).await, ErrorCode::Unsupported, Some(("cap", "notLive")));
}

/// The quit sweep (T09): the driver's stop with the Quit cause.
#[tokio::test]
async fn t_sessions_stop_for_quit() {
    let r = Rig::new();
    let uid = r.start(r.start_req(Vendor::Claude, CLAUDE_ACCT, ControlMode::Conversation)).await.unwrap();
    let out = r.m.stop_for_quit(&uid).await.unwrap();
    assert!(out.stopped);
    assert_eq!(r.transports.log_of(&uid).stops.lock().as_slice(), &[StopCause::Quit]);
    // A session Gilde does not run: nothing to stop.
    assert!(r.m.stop_for_quit(&SessionUid::mint()).await.unwrap().stopped);
}

// ---------------------------------------------------------------------------
// turns and interactions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn t_sessions_turn_send_idempotent_and_watched_refused() {
    let r = Rig::new();
    let uid = r.start(r.start_req(Vendor::Claude, CLAUDE_ACCT, ControlMode::Conversation)).await.unwrap();
    let id = mid();
    let send = |text: &str| TurnSendReq { uid: uid.clone(), client_msg_id: id.clone(), text: text.into(), priority: SendPriority::Next };
    let a = r.m.send(send("hello")).await.unwrap();
    let b = r.m.send(send("hello")).await.unwrap();
    assert_eq!(a, b);
    assert_eq!(r.transports.log_of(&uid).sends.lock().len(), 1, "the first outcome, sent once");
    // watched / unknown
    let w = SessionUid::mint();
    r.put(session(&w, Vendor::Claude, Some(CLAUDE_ACCT), Some(VSID), watching(Origin::VsCode), ActivityState::Working));
    let q = TurnSendReq { uid: w, client_msg_id: mid(), text: "x".into(), priority: SendPriority::Next };
    assert_err(r.m.send(q).await, ErrorCode::Unsupported, Some(("cap", "send")));
    let q = TurnSendReq { uid: SessionUid::mint(), client_msg_id: mid(), text: "x".into(), priority: SendPriority::Next };
    assert_err(r.m.send(q).await, ErrorCode::NotFound, None);
}

/// Held turn-based sessions (§6.3, A18.3): the first prompt starts a turn, a
/// message sent while it runs is held and published as `Session.queued`,
/// `turn_cancel_queued` drops it.
#[tokio::test]
async fn t_sessions_held_turn_based_queue() {
    let r = Rig::new();
    let mut q = r.start_req(Vendor::Codex, CODEX_ACCT, ControlMode::TurnBased);
    q.prompt = Some("first".into());
    q.permission_mode = Some(PermissionMode::Plan);
    let uid = r.start(q).await.unwrap();
    let spec = r.transports.turns.lock()[0].clone();
    assert_eq!(spec.permission_mode, Some(PermissionMode::Plan));
    assert!(spec.hint.gilde_spawn_id.as_deref().is_some_and(|t| t.starts_with("tb_")));
    assert_eq!(r.binds_for(&uid), vec![spec.hint.gilde_spawn_id.clone().unwrap()], "the held session's token is bound");
    let log = r.transports.log_of(&uid);
    assert_eq!(log.sends.lock()[0].text, "first");
    assert_eq!(r.world.controls.lock()[&uid].transport, Some(Transport::Turns));
    // A turn runs: the next message is held.
    *log.send_result.lock() = Some(Ok(SendDelivery::Queued));
    let held = mid();
    let res = r.m.send(TurnSendReq { uid: uid.clone(), client_msg_id: held.clone(), text: "second".into(), priority: SendPriority::Next }).await.unwrap();
    assert_eq!(res.delivery, SendDelivery::Queued);
    assert_eq!(res.position, Some(1));
    assert_eq!(r.world.queued.lock()[&uid][0].text, "second");
    let c = r.m.cancel_queued(gilde_code_lib::contract::ipc::TurnCancelQueuedReq { uid: uid.clone(), client_msg_id: held }).await.unwrap();
    assert!(c.cancelled);
    assert!(r.world.queued.lock()[&uid].is_empty());
    // Interrupt = turn.cancel → stopped; the session stays held.
    let i = r.m.interrupt(gilde_code_lib::contract::ipc::TurnInterruptReq { uid: uid.clone(), discard_queued: false }).await.unwrap();
    assert_eq!(i.outcome, InterruptOutcome::Stopped);
    assert!(r.m.actor(&uid).unwrap().is_live().await);
}

/// T-turns-newConversation (§6.3, PM-23): a resumed turn whose `Session` id
/// differs from its `resumeId` moves to a new uid with the line; the old uid
/// is released; later sends go to the new conversation.
#[tokio::test]
async fn t_turns_new_conversation() {
    let r = Rig::new();
    let uid = SessionUid::mint();
    r.put(session(&uid, Vendor::Codex, Some(CODEX_ACCT), Some(VSID), ended(), ActivityState::Ended));
    let got = r.m.continue_session(r.resume(&uid, ContinueHow::Resume)).await.unwrap().uid;
    assert_eq!(got, uid);
    let spec = r.transports.turns.lock()[0].clone();
    assert_eq!(spec.resume_id.as_deref(), Some(VSID));
    let sink = r.transports.turn_sinks.lock()[0].clone();
    // The driver reports another conversation.
    let mut hint = spec.hint.clone();
    hint.vendor_session_id = Some("99999999-8888-4777-8666-555555555555".into());
    sink.evidence(EvidenceUpdate { hint, facts: SourceFacts::Process(ProcessFacts::HistoryOnly), at: NOW });
    eventually("rehomed", || r.m.actor(&uid).is_none()).await;
    let new_uid = r.world.binds.lock().last().unwrap().2.clone();
    assert_ne!(new_uid, uid);
    assert!(r.m.actor(&new_uid).is_some(), "the actor moved to the new uid");
    let line = r.world.details.lock().iter().find(|(u, _)| u == &new_uid).map(|(_, d)| d.lines[0].key.clone());
    assert_eq!(line.as_deref(), Some("turn.newConversation"));
    // The old row was released (a Gilde stop cause under the old token).
    let old_token = spec.hint.gilde_spawn_id.clone().unwrap();
    assert!(r.gilde_facts(&old_token).iter().any(|g| g.stop_cause.is_some()));
    // Later evidence carries the new token.
    let last = r.world.evidence.lock().last().cloned().unwrap();
    assert_ne!(last.hint.gilde_spawn_id.as_deref(), Some(old_token.as_str()));
}

/// A19.6: `REFUSED{defaultToNo}` for an AllowOnce by shortcut on a
/// default-to-no card (by button it goes through); `REFUSED{watching}`.
#[tokio::test]
async fn t_sessions_answer_refusals_and_outcomes() {
    let r = Rig::new();
    let uid = r.start(r.start_req(Vendor::Claude, CLAUDE_ACCT, ControlMode::Conversation)).await.unwrap();
    let log = r.transports.log_of(&uid);
    log.pending.lock().push(card("r1", true));
    let ans = |via| InteractionAnswerReq { uid: uid.clone(), request_id: RequestId::parse("r1").unwrap(), decision: Decision::AllowOnce, via };
    assert_err(r.m.answer(ans(AnswerVia::Shortcut)).await, ErrorCode::Refused, Some(("reason", "defaultToNo")));
    assert!(log.answers.lock().is_empty(), "nothing sent");
    let ok = r.m.answer(ans(AnswerVia::Button)).await.unwrap();
    assert_eq!(ok.outcome, AnswerOutcome::Answered);
    let again = r.m.answer(ans(AnswerVia::Digit)).await.unwrap();
    assert_eq!((again.outcome, again.previous), (AnswerOutcome::AlreadyAnswered, Some(DecisionKind::AllowOnce)));
    // Deny by shortcut on the same kind of card is fine.
    log.pending.lock().push(card("r2", true));
    let deny = InteractionAnswerReq { uid: uid.clone(), request_id: RequestId::parse("r2").unwrap(), decision: Decision::Deny, via: AnswerVia::Shortcut };
    assert_eq!(r.m.answer(deny).await.unwrap().outcome, AnswerOutcome::Answered);
    // The session ended: a late answer is `withdrawn`, not an error.
    r.m.stop_for_quit(&uid).await.unwrap();
    let late = InteractionAnswerReq { uid: uid.clone(), request_id: RequestId::parse("r3").unwrap(), decision: Decision::Deny, via: AnswerVia::Button };
    assert_eq!(r.m.answer(late).await.unwrap().outcome, AnswerOutcome::Withdrawn);
    // Watching.
    let w = SessionUid::mint();
    r.put(session(&w, Vendor::Claude, Some(CLAUDE_ACCT), Some(VSID), watching(Origin::Terminal), ActivityState::NeedsApproval));
    let q = InteractionAnswerReq { uid: w, request_id: RequestId::parse("wa_1").unwrap(), decision: Decision::AllowOnce, via: AnswerVia::Button };
    assert_err(r.m.answer(q).await, ErrorCode::Refused, Some(("reason", "watching")));
}

/// `screen_prompt_key` (§4.1 rule 4 as amended by A34.2/A38.6): identical
/// calls — `requestId` + `promptRev` + **the action** — that overlap are
/// joined (one key sent, both get its outcome); another action, or another
/// text, is sent on its own (G7-02); a finished call is not remembered here
/// — the PTY remembers typed answers per appearance of the prompt
/// (`pty_prompts`, `sessions_live`). Only for Terminal sessions.
#[tokio::test]
async fn t_sessions_screen_prompt_key() {
    let r = Rig::new();
    let uid = r.start(r.start_req(Vendor::Claude, CLAUDE_ACCT, ControlMode::Terminal)).await.unwrap();
    let rev = "0123456789abcdef".to_string();
    let req = ScreenPromptKeyReq {
        uid: uid.clone(),
        request_id: RequestId::parse(format!("sp_{rev}")).unwrap(),
        prompt_rev: rev.clone(),
        action: ScreenPromptAction::Key { key: "1".into() },
    };
    let sent = || r.transports.log_of(&uid).screen_keys.lock().len();
    let (a, b) = tokio::join!(r.m.screen_key(req.clone()), r.m.screen_key(req.clone()));
    assert_eq!((a.unwrap().outcome, b.unwrap().outcome), (ScreenKeyOutcome::Sent, ScreenKeyOutcome::Sent));
    assert_eq!(sent(), 1, "an overlapping identical press sends nothing");
    let with = |action: ScreenPromptAction| ScreenPromptKeyReq { action, ..req.clone() };
    assert_eq!(r.m.screen_key(with(ScreenPromptAction::Key { key: "3".into() })).await.unwrap().outcome, ScreenKeyOutcome::Sent);
    assert_eq!(sent(), 2, "Deny after Allow is executed on its own");
    let reply = |text: &str| with(ScreenPromptAction::DenyWithReply { text: text.into() });
    let (x, y) = tokio::join!(r.m.screen_key(reply("keep it")), r.m.screen_key(reply("delete it")));
    assert!(x.is_ok() && y.is_ok());
    assert_eq!(sent(), 4, "another text is another action");
    assert_eq!(r.m.screen_key(req.clone()).await.unwrap().outcome, ScreenKeyOutcome::Sent);
    assert_eq!(sent(), 5, "a finished call is the PTY's to remember, per appearance of the prompt");
    let convo = r.start(r.start_req(Vendor::Claude, CLAUDE_ACCT, ControlMode::Conversation)).await.unwrap();
    assert_err(r.m.screen_key(ScreenPromptKeyReq { uid: convo, ..req }).await, ErrorCode::Unsupported, Some(("cap", "key")));
}

/// `turn_retry` resends the last user text only in Error·Agent error.
#[tokio::test]
async fn t_sessions_turn_retry() {
    let r = Rig::new();
    let uid = r.start(r.start_req(Vendor::Claude, CLAUDE_ACCT, ControlMode::Conversation)).await.unwrap();
    r.put(session(&uid, Vendor::Claude, Some(CLAUDE_ACCT), None, ended(), ActivityState::Ready));
    r.m.send(TurnSendReq { uid: uid.clone(), client_msg_id: mid(), text: "do it".into(), priority: SendPriority::Next }).await.unwrap();
    assert_err(r.m.retry(TurnRetryReq { uid: uid.clone(), client_msg_id: mid() }).await, ErrorCode::Refused, Some(("reason", "notInError")));
    r.set_state(&uid, ActivityState::Error { sub: ErrorSub::AgentError });
    let id = mid();
    r.m.retry(TurnRetryReq { uid: uid.clone(), client_msg_id: id.clone() }).await.unwrap();
    r.m.retry(TurnRetryReq { uid: uid.clone(), client_msg_id: id }).await.unwrap();
    let sends = r.transports.log_of(&uid).sends.lock().clone();
    assert_eq!(sends.len(), 2, "the retry is idempotent by clientMsgId");
    assert_eq!(sends[1].text, "do it");
}

/// `session_models` / `session_set_model` [F-23]; bypass needs the launch flag.
#[tokio::test]
async fn t_sessions_models_and_modes() {
    let r = Rig::new();
    let uid = r.start(r.start_req(Vendor::Claude, CLAUDE_ACCT, ControlMode::Conversation)).await.unwrap();
    let log = r.transports.log_of(&uid);
    *log.models.lock() = vec![ModelOption { id: "opus".into(), label: None, is_default: true }];
    assert_eq!(r.m.models(&uid).await.unwrap().models.len(), 1);
    assert_err(r.m.set_model(&uid, Some("sonnet".into())).await, ErrorCode::InvalidArgument, Some(("field", "model")));
    assert_eq!(r.m.set_model(&uid, Some("opus".into())).await.unwrap(), AppliesFrom::NextRequest);
    assert_err(r.m.set_permission_mode(&uid, PermissionMode::BypassPermissions).await, ErrorCode::Refused, Some(("reason", "needsLaunchFlag")));
    r.m.set_permission_mode(&uid, PermissionMode::Plan).await.unwrap();
    assert_eq!(log.modes.lock().as_slice(), &[PermissionMode::Plan]);
    let t = r.start(r.start_req(Vendor::Claude, CLAUDE_ACCT, ControlMode::Terminal)).await.unwrap();
    assert_err(r.m.models(&t).await, ErrorCode::Unsupported, Some(("cap", "setModel")));
}

/// `session_retry_as_terminal`: only Error·Didn't start{CompatFailed} with no
/// transcript; the same request in Terminal mode, same uid, fresh id.
#[tokio::test]
async fn t_sessions_retry_as_terminal() {
    let r = Rig::new();
    let mut q = r.start_req(Vendor::Claude, CLAUDE_ACCT, ControlMode::Conversation);
    q.prompt = Some("the original task".into());
    let uid = r.start(q).await.unwrap();
    let first_id = r.transports.convo.lock()[0].hint.vendor_session_id.clone().unwrap();
    r.put(session(&uid, Vendor::Claude, Some(CLAUDE_ACCT), None, ended(), ActivityState::Ended));
    let retry = || SessionRetryAsTerminalReq { client_request_id: cid(), uid: uid.clone() };
    assert_err(r.m.retry_as_terminal(retry()).await, ErrorCode::Refused, Some(("reason", "notEligible")));
    r.set_state(&uid, ActivityState::Error { sub: ErrorSub::DidntStart { reason: DidntStartReason::CompatFailed } });
    let got = r.m.retry_as_terminal(retry()).await.unwrap().uid;
    assert_eq!(got, uid);
    let spec = r.transports.pty.lock()[0].clone();
    assert_eq!(spec.parent_uid.as_ref(), Some(&uid));
    assert_ne!(spec.args[1], first_id, "a fresh --session-id");
    assert_eq!(spec.args.last().map(String::as_str), Some("the original task"));
    // With a transcript it is not eligible.
    r.world.transcripts.lock().insert(uid.clone(), abs("/t.jsonl"));
    assert_err(r.m.retry_as_terminal(retry()).await, ErrorCode::Refused, Some(("reason", "notEligible")));
}

/// `account_sign_in` → a Terminal session running the CLI's own sign-in
/// (`--permission-mode default`, `/login` typed after ready) [F-44].
#[tokio::test]
async fn t_sessions_sign_in() {
    let r = Rig::new();
    let res = r.m.start_sign_in(&claude_account()).await.unwrap();
    let spec = r.transports.pty.lock()[0].clone();
    assert_eq!(spec.parent_uid.as_ref(), Some(&res.uid));
    assert_eq!(spec.args, vec!["--permission-mode".to_string(), "default".to_string()]);
    assert_eq!(spec.cwd.as_str(), "/Users/tester");
    let log = r.transports.log_of(&res.uid);
    eventually("/login typed after ready", || log.sends.lock().iter().any(|m| m.text == "/login")).await;
}

/// T-compat-binarySwap at the session level [A1]: each spawn records the
/// binary resolved at that spawn; a running session keeps its record.
#[tokio::test]
async fn t_compat_binary_swap_records_the_spawn_time_binary() {
    let r = Rig::new();
    let a = r.start(r.start_req(Vendor::Claude, CLAUDE_ACCT, ControlMode::Conversation)).await.unwrap();
    // The installed binary is replaced (2.1.306 → 2.1.307, same path).
    r.binaries.resolved.lock().insert(
        Vendor::Claude,
        ResolvedVendor { binary: binary("/opt/claude/2.1.306/claude", "2.1.307"), compat: Some(compat("2.1.307", CompatState::Unverified, false)) },
    );
    let b = r.start(r.start_req(Vendor::Claude, CLAUDE_ACCT, ControlMode::Conversation)).await.unwrap();
    let specs = r.transports.convo.lock().clone();
    let va = specs.iter().find(|s| s.uid == a).unwrap().binary.version.clone();
    let vb = specs.iter().find(|s| s.uid == b).unwrap().binary.version.clone();
    assert_eq!(va.as_deref(), Some("2.1.306"));
    assert_eq!(vb.as_deref(), Some("2.1.307"));
    let metas: Vec<(Option<String>, Option<String>)> = r
        .world
        .evidence
        .lock()
        .iter()
        .filter_map(|e| match &e.facts {
            SourceFacts::Meta(m) => Some((e.hint.vendor_session_id.clone(), m.claude_version.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(metas.len(), 2, "each session's metadata was published once, at its spawn");
    assert!(r.m.actor(&a).unwrap().is_live().await, "the running session is untouched");
    // A newer version that is below the minimum would refuse a new Conversation,
    // never the running one.
    r.binaries.resolved.lock().get_mut(&Vendor::Claude).unwrap().compat = Some(compat("2.1.100", CompatState::Unverified, true));
    assert!(r.start(r.start_req(Vendor::Claude, CLAUDE_ACCT, ControlMode::Conversation)).await.is_err());
    assert!(r.m.actor(&a).unwrap().is_live().await);
}

/// ConvoHooks (A19.1): queued → set_queued, Details facts → one full
/// replacement each time, quota / signed-out → the accounts service.
#[tokio::test]
async fn t_sessions_convo_hooks_reach_world_and_accounts() {
    let r = Rig::new();
    let uid = r.start(r.start_req(Vendor::Claude, CLAUDE_ACCT, ControlMode::Conversation)).await.unwrap();
    let hooks = r.transports.wirings.lock()[0].hooks.clone();
    let q = QueuedMessage { client_msg_id: mid(), text: "later".into(), queued_at: NOW };
    hooks.queued(&uid, vec![q.clone()]);
    assert_eq!(r.world.queued.lock()[&uid], vec![q]);
    hooks.approval_unconfirmed(&uid, "Bash");
    hooks.version_note(&uid, "2.1.307");
    hooks.notice(&uid, Notice::new("convo.notReading"));
    hooks.notice(&uid, Notice::new("convo.notReading"));
    let last = r.world.details.lock().last().cloned().unwrap().1;
    assert_eq!(last.approval_channel_unconfirmed.as_deref(), Some("Bash"));
    assert_eq!(last.version_note.as_deref(), Some("2.1.307"));
    assert_eq!(last.lines.len(), 1, "one line per key");
    let quota = Quota {
        windows: vec![QuotaWindow { kind: QuotaWindowKind::FiveHour, label: None, used_pct: Some(40), resets_at: None }],
        fetched_at: NOW,
        stale: false,
        unavailable: None,
    };
    hooks.quota(&claude_account(), quota);
    hooks.signed_out(&claude_account());
    assert_eq!(r.accounts.quotas.lock().len(), 1);
    assert_eq!(r.accounts.signed_out.lock().as_slice(), &[claude_account()]);
}

// ---------------------------------------------------------------------------
// newsession_options
// ---------------------------------------------------------------------------

#[tokio::test]
async fn t_sessions_newsession_options() {
    let r = Rig::new();
    let o = r.m.options(&NewsessionOptionsReq { project_id: project_id(), vendor: Some(Vendor::Claude) }).await.unwrap();
    assert!(o.vendors.iter().any(|v| v.vendor == Vendor::Claude && v.installed));
    assert!(!o.vendors.iter().any(|v| v.vendor == Vendor::Agy && v.installed));
    assert_eq!(o.modes, vec![ControlMode::Conversation, ControlMode::Terminal]);
    assert!(o.permission_modes.contains(&PermissionMode::BypassPermissions));
    assert!(o.accounts.iter().all(|a| a.vendor == Vendor::Claude));
    assert_eq!(o.defaults.account_id, Some(claude_account()));
    assert_eq!(o.defaults.control_mode, Some(ControlMode::Conversation));
    assert!(o.team_allowed);
    assert_eq!(o.team_mode, gilde_code_lib::contract::ipc::TeamMode::Panes);
    assert_eq!(o.locations.len(), 1);
    // A failed version: Conversation is omitted with the notice.
    r.compat.records.lock().insert("2.1.306".into(), compat("2.1.306", CompatState::Failed, false));
    let o = r.m.options(&NewsessionOptionsReq { project_id: project_id(), vendor: Some(Vendor::Claude) }).await.unwrap();
    assert_eq!(o.modes, vec![ControlMode::Terminal]);
    assert!(o.notices.iter().any(|n| n.key == "notice.conversationUnavailable"));
    // The project's last start is remembered (project_prefs).
    r.compat.records.lock().insert("2.1.306".into(), compat_rec_verified());
    let mut q = r.start_req(Vendor::Codex, CODEX_ACCT, ControlMode::Terminal);
    q.model = Some("o4-mini".into());
    r.start(q).await.unwrap();
    let o = r.m.options(&NewsessionOptionsReq { project_id: project_id(), vendor: None }).await.unwrap();
    assert_eq!(o.defaults.vendor, Some(Vendor::Codex));
    assert_eq!(o.defaults.control_mode, Some(ControlMode::Terminal));
    assert_eq!(o.defaults.account_id.as_ref().map(|a| a.as_str()), Some(CODEX_ACCT));
    // Detection still running: no vendors, the notice.
    r.binaries.detected.store(false, Ordering::SeqCst);
    let o = r.m.options(&NewsessionOptionsReq { project_id: project_id(), vendor: None }).await.unwrap();
    assert!(o.vendors.is_empty());
    assert!(o.notices.iter().any(|n| n.key == "newsession.findingAgents"));
    assert_err(r.m.options(&NewsessionOptionsReq { project_id: ProjectId::derive("/x"), vendor: None }).await, ErrorCode::NotFound, None);
}

/// The §5.8 account rule inside the options: a near-limit last account
/// switches with one line.
#[tokio::test]
async fn t_sessions_newsession_account_rule() {
    let r = Rig::new();
    r.accounts.accounts.lock()[0].quota = Some(Quota {
        windows: vec![QuotaWindow { kind: QuotaWindowKind::FiveHour, label: None, used_pct: Some(93), resets_at: Some(NOW + 1) }],
        fetched_at: NOW,
        stale: false,
        unavailable: None,
    });
    let o = r.m.options(&NewsessionOptionsReq { project_id: project_id(), vendor: Some(Vendor::Claude) }).await.unwrap();
    assert_eq!(o.defaults.account_id.as_ref().map(|a| a.as_str()), Some(CLAUDE_WORK));
    let n = o.notices.iter().find(|n| n.key == "accounts.switched").unwrap();
    assert_eq!(n.params["a"], "Default");
    assert_eq!(n.params["b"], "Work");
}

// ---------------------------------------------------------------------------
// T02's real World as WorldApi + SessionBinder (A18.1, A18.3, A19.1, A20.1)
// ---------------------------------------------------------------------------

fn real_world() -> Arc<gilde_code_lib::world::World> {
    let world = Arc::new(gilde_code_lib::world::World::new(gilde_code_lib::world::WorldConfig::system(
        Some("/Users/tester".into()),
        Arc::new(gilde_code_lib::world::MemoryMetaStore::default()),
    )));
    world.sink().location(LocationFacts {
        project_id: project_id(),
        project_root: abs("/work/repo"),
        git: true,
        location_id: main_location(),
        location_path: abs("/work/repo"),
        worktree: false,
        branch: Some("main".into()),
    });
    // World publishes a project on a session change or the sidebar order.
    world.set_project_order(vec![project_id()]);
    world.flush_now();
    world
}

/// A Terminal session started through the manager appears in the real World
/// under the uid T10 chose, In Gilde · Terminal with its PTY and the
/// spawn-time binary; a resume keeps the uid even when evidence for the new
/// spawn reached World before the bind (A20.1); queued messages and the
/// driver's Details facts land on the row (A18.3, A19.1).
#[tokio::test]
async fn t_sessions_real_world_binding() {
    let r = Rig::new();
    let world = real_world();
    let mut deps = r.m.deps().clone();
    deps.world = world.clone();
    deps.binder = world.clone();
    let m = SessionsManager::new(deps);
    let uid = m.start(r.start_req(Vendor::Claude, CLAUDE_ACCT, ControlMode::Terminal)).await.unwrap().uid;
    world.flush_now();
    let s = world.session(&uid).expect("World shows the session under T10's uid");
    match &s.control {
        Control::InGilde { mode, pty, transport, hosted_team } => {
            assert_eq!(*mode, ControlMode::Terminal);
            assert!(pty.is_some());
            assert_eq!(*transport, Transport::Pty);
            assert!(!hosted_team);
        }
        other => panic!("expected In Gilde: {other:?}"),
    }
    assert_eq!(s.binary.as_ref().and_then(|b| b.version.clone()).as_deref(), Some("2.1.306"));
    assert_eq!(s.claude_version.as_deref(), Some("2.1.306"));

    // set_queued / set_details_extra through the binder.
    let q = QueuedMessage { client_msg_id: mid(), text: "later".into(), queued_at: NOW };
    gilde_code_lib::sessions::manager::SessionBinder::set_queued(world.as_ref(), &uid, vec![q.clone()]);
    gilde_code_lib::sessions::manager::SessionBinder::set_details_extra(
        world.as_ref(),
        &uid,
        DriverDetails { approval_channel_unconfirmed: Some("Bash".into()), ..DriverDetails::default() },
    );
    world.flush_now();
    let s = world.session(&uid).unwrap();
    assert_eq!(s.queued, vec![q]);
    assert_eq!(s.details.approval_channel_unconfirmed.as_deref(), Some("Bash"));

    // A20.1: evidence of a spawn reaches World before T10 binds it; the bind
    // with Some(uid) still lands on the resumed uid.
    let resumed = SessionUid::mint();
    let early = gilde_code_lib::contract::services::IdentityHint {
        vendor: Some(Vendor::Claude),
        account_id: Some(claude_account()),
        vendor_session_id: None,
        process: None,
        gilde_spawn_id: Some("sp_early".into()),
    };
    world.sink().evidence(EvidenceUpdate {
        hint: early,
        facts: SourceFacts::Process(ProcessFacts::Alive { pid: std::process::id() as i32, start_sec: 1, cpu_active_at: None }),
        at: NOW,
    });
    let bound = gilde_code_lib::sessions::manager::SessionBinder::bind_spawn(
        world.as_ref(),
        "sp_early",
        Some(&resumed),
        Vendor::Claude,
        Some(claude_account()),
    );
    assert_eq!(bound, resumed, "bind_spawn(spawn, Some(uid)) always returns uid");
}

/// The hosted lead against T07's **real** PaneHost (§7.4.1, T07 rig: the real
/// `gilde-tmux` shim installed in a temp home, a bound socket, nothing
/// spawned): the lead's PTY env carries the shim dir first on `PATH`, the
/// pane-host socket and its token, teams on, and none of the absent names
/// (`TMUX*`, `ITERM_*`, `TERM_PROGRAM` …, wildcards included); the argv has
/// `--teammate-mode tmux`, `--permission-mode` and the inline `--settings`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_sessions_hosted_lead_real_panehost() {
    let ph = fake_tmux_client::host::Rig::new(fake_tmux_client::host::parent_pid());
    ph.host.start(tokio::runtime::Handle::current()).expect("the listener binds");
    assert!(ph.host.is_hosting());
    let r = Rig::new();
    let mut deps = r.m.deps().clone();
    deps.panehost = Some(ph.host.clone() as Arc<dyn PaneHostApi>);
    let m = SessionsManager::new(deps);
    let mut req = r.start_req(Vendor::Claude, CLAUDE_ACCT, ControlMode::Terminal);
    req.team = true;
    req.permission_mode = Some(PermissionMode::AcceptEdits);
    let uid = m.start(req).await.unwrap().uid;
    let spec = r.transports.pty.lock()[0].clone();
    let env: HashMap<String, String> = spec.env.iter().cloned().collect();
    let shim_dir = ph.shim().parent().unwrap().to_string_lossy().into_owned();
    assert!(env["PATH"].starts_with(&shim_dir), "the shim dir first: {}", env["PATH"]);
    assert!(env.contains_key("GILDE_PANE_HOST"));
    assert!(env.get("GILDE_PANE_HOST_TOKEN").is_some_and(|t| !t.is_empty()), "the lead gets its token");
    assert_eq!(env.get("CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS").map(String::as_str), Some("1"));
    for k in env.keys() {
        assert!(!(k.starts_with("TMUX") || k.starts_with("ITERM_") || k.starts_with("LC_TERMINAL") || k == "CLAUDECODE"), "{k} must be absent");
    }
    let a = &spec.args;
    let pos = |f: &str| a.iter().position(|x| x == f).unwrap_or_else(|| panic!("{f} in {a:?}"));
    assert_eq!(a[pos("--teammate-mode") + 1], "tmux");
    assert_eq!(a[pos("--permission-mode") + 1], "acceptEdits");
    assert_eq!(a[pos("--settings") + 1], r#"{"permissions":{"defaultMode":"acceptEdits"}}"#);
    assert!(r.world.controls.lock()[&uid].hosted_team);
    // The lead exits: PaneHost hears it (all panes closed, §7.4.7) and the
    // session is no longer live.
    r.transports.exits.lock()[0].exited(&PtyId::mint(), ExitInfo { code: Some(0), signal: None }, NOW);
    eventually("the lead ended", || r.world.evidence.lock().iter().any(|e| matches!(e.facts, SourceFacts::Process(ProcessFacts::Exited { .. })))).await;
    eventually("not live", || !m.actor(&uid).unwrap().sink().is_live()).await;
}

// ===========================================================================
// A43.1 (G7c-07): Continue right after a crash relaunch waits for its own
// leftover — the boot sweep's record decides at once, before any health scan
// published the leftover.
// ===========================================================================

/// The boot sweep ran (before the spawn gate opened) and proved `pid` this
/// uid's crash leftover.
fn boot_sweep_proved(uid: &SessionUid, pid: i32) {
    gilde_code_lib::supervise::reaper::record_crash_leftover(gilde_code_lib::supervise::reaper::CrashLeftover {
        spawn_id: format!("crash-{}", uid.as_str()),
        pid,
        start_sec: Some(9),
        session_uid: Some(uid.as_str().to_string()),
    });
}

/// G7c-07b, the reviewer's test: World holds the first scan's record
/// (`leftover: None`) or no record yet; either way Continue waits for the own
/// leftover and resumes. Without the boot sweep's record a live process is
/// elsewhere, as before.
#[tokio::test]
async fn g7c_07b_continue_right_after_a_crash_relaunch_never_calls_its_own_leftover_elsewhere() {
    for first_scan_published in [true, false] {
        let r = Rig::new();
        let uid = SessionUid::mint();
        r.put(session(&uid, Vendor::Claude, Some(CLAUDE_ACCT), Some(VSID), ended(), ActivityState::Ended));
        boot_sweep_proved(&uid, 901);
        if first_scan_published {
            r.world.leftovers.lock().push(ProcessRecord {
                key: ProcessKey::new(901, 9),
                pid: 901,
                start_sec: 9,
                vendor: Some(Vendor::Claude),
                role: ProcRole::Session,
                session_uid: Some(uid.clone()),
                gilde_owned: true,
                rss_bytes: 0,
                leftover: None,
            });
        }
        r.liveness.live.lock().insert(VSID.into(), vec![LiveProcess { pid: 901, start_sec: 9, background: false }]);
        // The leftover exits shortly after (the undo window's SIGTERM).
        let lv = r.liveness.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            lv.alive.lock().insert((901, 9), false);
        });
        let res = r.m.continue_session(r.resume(&uid, ContinueHow::Resume)).await;
        assert!(
            res.is_ok(),
            "F-12: own crash leftover counted as elsewhere (first_scan_published={first_scan_published}): {:?}",
            res.err().map(|e| (e.code, e.params))
        );
    }
    // Control: a live process no record names is elsewhere.
    let r = Rig::new();
    let uid = SessionUid::mint();
    r.put(session(&uid, Vendor::Claude, Some(CLAUDE_ACCT), Some(VSID), ended(), ActivityState::Ended));
    r.liveness.live.lock().insert(VSID.into(), vec![LiveProcess { pid: 903, start_sec: 9, background: false }]);
    assert_err(r.m.continue_session(r.resume(&uid, ContinueHow::Resume)).await, ErrorCode::SessionLiveElsewhere, None);
}

/// A43.1: Continue all right after a crash relaunch waits for every item's own
/// leftover and starts them all — none is answered "live elsewhere".
#[tokio::test]
async fn a43_continue_all_right_after_a_crash_relaunch_waits_for_own_leftovers() {
    let r = Rig::new();
    let mut items = Vec::new();
    let mut uids = Vec::new();
    for i in 0..3 {
        let uid = SessionUid::mint();
        let vsid = format!("6a1e2b0c-1111-4222-8333-44445555667{i}");
        let pid = 930 + i;
        r.put(session(&uid, Vendor::Claude, Some(CLAUDE_ACCT), Some(&vsid), ended(), ActivityState::Stopped));
        boot_sweep_proved(&uid, pid);
        r.liveness.live.lock().insert(vsid, vec![LiveProcess { pid, start_sec: 9, background: false }]);
        items.push(QuitItemRow {
            uid: uid.as_str().to_string(),
            control_mode: "conversation".into(),
            permission_mode: Some("plan".into()),
            model: None,
            was_waiting: None,
            team_lead: false,
            survivor: false,
        });
        uids.push(uid);
    }
    let lv = r.liveness.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        for i in 0..3 {
            lv.alive.lock().insert((930 + i, 9), false);
        }
    });
    let record = QuitRecordId::mint();
    r.quit.0.lock().insert(record.as_str().to_string(), items);
    let res = r.m.continue_all(&record, None).await.unwrap();
    assert!(res.failed.is_empty(), "{:?}", res.failed.iter().map(|f| (f.uid.clone(), f.error.code)).collect::<Vec<_>>());
    assert_eq!(res.started.len(), 3);
}
