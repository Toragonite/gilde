//! The session actor (contract §6.0, §4.1 rule 5): one Tokio task per
//! Gilde-owned session that owns its driver and runs every command and every
//! transport event for that session in arrival order.
//!
//! Owner: T10.
//!
//! * [`ActorDriver`] — the driver as the actor uses it: the §6.0
//!   [`SessionDriver`] plus the read-only accessors of the three real drivers
//!   (`ConvoDriver`, `PtyDriver`, `TurnDriver`), implemented here for each.
//! * [`ActorHandle`] — the command side (one `mpsc` channel, one reply per
//!   command); `turn_send`/`turn_retry` are idempotent by `clientMsgId` for
//!   the session's life (§4.1 rule 4).
//! * [`SessionSink`] — the session's `EvidenceSink`: it binds every spawn id
//!   it sees to the session uid **before** forwarding (A18.1, A20.1), so a
//!   driver's own evidence can never mint a second row; it notices the end of
//!   a process and a turn-based session that started a new conversation
//!   (§6.3, T-turns-newConversation).
//! * [`SessionHooks`] — T05's `ConvoHooks` (A19.1): queued → `set_queued`,
//!   Details facts → `set_details_extra` (full replacement, latest of each),
//!   quota / signed-out → the accounts service.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, Weak};

use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::sync::{mpsc, oneshot};

use crate::contract::account::Quota;
use crate::contract::activity::{EvidenceLine, ExitInfo, GildeFacts, ProcessFacts, StopCause};
use crate::contract::common::{EpochMs, Notice, PermissionMode, Vendor};
use crate::contract::error::{ErrorCode, IpcError};
use crate::contract::ids::{AccountId, ClientMsgId, ProcessKey, PtyId, RequestId, SessionUid};
use crate::contract::interaction::{Decision, DecisionKind, PendingInteraction};
use crate::contract::ipc::{
    AnswerOutcome, AnswerVia, AppliesFrom, InteractionDetailRes, InterruptOutcome, ModelOption,
    ScreenKeyOutcome, ScreenPromptAction, SendDelivery, SendPriority, SessionModelsRes,
    SessionTechnicalRes, TurnSendRes,
};
use crate::contract::services::{
    ContentHubApi, ContentUpdate, EvidenceSink, EvidenceUpdate, IdentityHint, LocationFacts,
    OutgoingMessage, PaneHostApi, PtyExitSink, SessionDriver, SidecarApi, SourceFacts,
    StopOutcome, LifecycleOp,
};
use crate::contract::session::{ControlMode, QueuedMessage, Transport};
use crate::contract::sidecar::ProbeParams;
use crate::contract::store::{IntegrationsState, SourceStatus};
use crate::convo::driver::{ConvoDriver, ConvoHooks};
use crate::pty::{PtyDriver, PtyManager};
use crate::turns::driver::TurnDriver;

use super::manager::{AccountsPort, ControlFacts, DriverDetails, SessionBinder, SessionsManager};

/// How many `clientMsgId` outcomes an actor remembers.
pub const SENT_REMEMBERED: usize = 512;

// ---------------------------------------------------------------------------
// the driver as the actor uses it
// ---------------------------------------------------------------------------

/// The §6.0 driver plus the read-only accessors the commands need. Defaults
/// answer "not offered" so a driver implements only what it has.
#[async_trait]
pub trait ActorDriver: SessionDriver + Sync {
    /// `interaction_answer` with the previous decision (`alreadyAnswered`).
    async fn answer_with_previous(&mut self, req: &RequestId, d: Decision) -> Result<(AnswerOutcome, Option<DecisionKind>), IpcError> {
        self.answer(req, d).await.map(|o| (o, None))
    }
    /// The pending card for `req`, when the driver keeps one (the
    /// `REFUSED{defaultToNo}` check, A19.6).
    async fn pending(&self, req: &RequestId) -> Option<PendingInteraction> {
        let _ = req;
        None
    }
    /// `session_models` (F-23).
    async fn models(&self) -> Result<(Vec<ModelOption>, Option<String>), IpcError> {
        Err(IpcError::unsupported("setModel"))
    }
    /// `interaction_detail`.
    async fn detail(&self, req: &RequestId) -> Result<InteractionDetailRes, IpcError> {
        let _ = req;
        Err(IpcError::not_found("request"))
    }
    /// `session_technical` (never in the store).
    async fn technical(&self) -> Result<SessionTechnicalRes, IpcError> {
        Err(IpcError::unsupported("notLive"))
    }
    /// The queue the actor publishes through `World::set_queued` (turn-based:
    /// the held message, A18.3). A driver that publishes its own queue
    /// (Conversation, through `ConvoHooks::queued`) returns `None`.
    async fn queued(&self) -> Option<Vec<QueuedMessage>> {
        None
    }
    /// Messages waiting behind a running turn (the `position` of a queued send).
    async fn waiting(&self) -> Vec<QueuedMessage> {
        self.queued().await.unwrap_or_default()
    }
}

#[async_trait]
impl ActorDriver for ConvoDriver {
    async fn answer_with_previous(&mut self, req: &RequestId, d: Decision) -> Result<(AnswerOutcome, Option<DecisionKind>), IpcError> {
        ConvoDriver::answer_with_previous(self, req, d).await
    }
    async fn pending(&self, req: &RequestId) -> Option<PendingInteraction> {
        let snap = self.snapshot().await.ok()?;
        snap.pending.into_iter().find(|p| &p.request_id == req)
    }
    async fn models(&self) -> Result<(Vec<ModelOption>, Option<String>), IpcError> {
        ConvoDriver::models(self).await
    }
    async fn detail(&self, req: &RequestId) -> Result<InteractionDetailRes, IpcError> {
        self.interaction_detail(req).await
    }
    async fn technical(&self) -> Result<SessionTechnicalRes, IpcError> {
        Ok(ConvoDriver::technical(self))
    }
    async fn waiting(&self) -> Vec<QueuedMessage> {
        self.snapshot().await.map(|s| s.queued).unwrap_or_default()
    }
}

/// A Terminal-mode session: T06's `PtyDriver` plus what `session_technical`
/// shows (pid, start, the redacted argv, §9.7).
pub struct TerminalDriver {
    inner: PtyDriver,
    pty: Arc<PtyManager>,
    technical: SessionTechnicalRes,
}

impl TerminalDriver {
    pub fn new(pty: Arc<PtyManager>, id: PtyId, technical: SessionTechnicalRes) -> TerminalDriver {
        TerminalDriver { inner: PtyDriver::new(pty.clone(), id), pty, technical }
    }
}

#[async_trait]
impl SessionDriver for TerminalDriver {
    fn mode(&self) -> ControlMode {
        self.inner.mode()
    }
    async fn send(&mut self, msg: OutgoingMessage) -> Result<SendDelivery, IpcError> {
        self.inner.send(msg).await
    }
    async fn cancel_queued(&mut self, id: &ClientMsgId) -> Result<bool, IpcError> {
        self.inner.cancel_queued(id).await
    }
    async fn answer(&mut self, req: &RequestId, d: Decision) -> Result<AnswerOutcome, IpcError> {
        self.inner.answer(req, d).await
    }
    async fn screen_key(&mut self, req: &RequestId, rev: &str, key: ScreenPromptAction) -> Result<ScreenKeyOutcome, IpcError> {
        self.inner.screen_key(req, rev, key).await
    }
    async fn interrupt(&mut self, discard_queued: bool) -> Result<InterruptOutcome, IpcError> {
        self.inner.interrupt(discard_queued).await
    }
    async fn force_stop(&mut self) -> Result<bool, IpcError> {
        self.inner.force_stop().await
    }
    async fn set_model(&mut self, m: Option<String>) -> Result<AppliesFrom, IpcError> {
        self.inner.set_model(m).await
    }
    async fn set_permission_mode(&mut self, m: PermissionMode) -> Result<(), IpcError> {
        self.inner.set_permission_mode(m).await
    }
    async fn stop(&mut self, why: StopCause) -> Result<StopOutcome, IpcError> {
        self.inner.stop(why).await
    }
}

#[async_trait]
impl ActorDriver for TerminalDriver {
    async fn technical(&self) -> Result<SessionTechnicalRes, IpcError> {
        match self.pty.get(self.inner.pty_id()) {
            Ok(s) if !s.has_exited() => Ok(self.technical.clone()),
            _ => Err(IpcError::unsupported("notLive")),
        }
    }
}

/// A turn-based session: T08's `TurnDriver` plus the model list of its vendor.
pub struct TurnBasedDriver {
    inner: TurnDriver,
    sidecar: Option<Arc<dyn SidecarApi>>,
    probe: ProbeParams,
}

impl TurnBasedDriver {
    pub fn new(inner: TurnDriver, sidecar: Option<Arc<dyn SidecarApi>>, probe: ProbeParams) -> TurnBasedDriver {
        TurnBasedDriver { inner, sidecar, probe }
    }
}

#[async_trait]
impl SessionDriver for TurnBasedDriver {
    fn mode(&self) -> ControlMode {
        self.inner.mode()
    }
    async fn send(&mut self, msg: OutgoingMessage) -> Result<SendDelivery, IpcError> {
        self.inner.send(msg).await
    }
    async fn cancel_queued(&mut self, id: &ClientMsgId) -> Result<bool, IpcError> {
        self.inner.cancel_queued(id).await
    }
    async fn answer(&mut self, req: &RequestId, d: Decision) -> Result<AnswerOutcome, IpcError> {
        self.inner.answer(req, d).await
    }
    async fn screen_key(&mut self, req: &RequestId, rev: &str, key: ScreenPromptAction) -> Result<ScreenKeyOutcome, IpcError> {
        self.inner.screen_key(req, rev, key).await
    }
    async fn interrupt(&mut self, discard_queued: bool) -> Result<InterruptOutcome, IpcError> {
        self.inner.interrupt(discard_queued).await
    }
    async fn force_stop(&mut self) -> Result<bool, IpcError> {
        self.inner.force_stop().await
    }
    async fn set_model(&mut self, m: Option<String>) -> Result<AppliesFrom, IpcError> {
        self.inner.set_model(m).await
    }
    async fn set_permission_mode(&mut self, m: PermissionMode) -> Result<(), IpcError> {
        self.inner.set_permission_mode(m).await
    }
    async fn stop(&mut self, why: StopCause) -> Result<StopOutcome, IpcError> {
        self.inner.stop(why).await
    }
}

#[async_trait]
impl ActorDriver for TurnBasedDriver {
    async fn models(&self) -> Result<(Vec<ModelOption>, Option<String>), IpcError> {
        let current = self.inner.model();
        let Some(sidecar) = &self.sidecar else {
            return Ok((Vec::new(), current));
        };
        let models = sidecar
            .models(self.probe.clone())
            .await
            .map(|list| {
                list.into_iter()
                    .map(|m| ModelOption { id: m.id, label: m.label, is_default: m.is_default })
                    .collect()
            })
            .unwrap_or_default();
        Ok((models, current))
    }
    async fn queued(&self) -> Option<Vec<QueuedMessage>> {
        Some(self.inner.held_message().into_iter().collect())
    }
}

// ---------------------------------------------------------------------------
// the session's evidence sink
// ---------------------------------------------------------------------------

struct SinkState {
    uid: SessionUid,
    bound: HashSet<String>,
    /// The spawn id (or turn-based token) evidence of this session carries.
    spawn_id: Option<String>,
    /// Replaces the driver's spawn id after a new conversation (§6.3).
    token_override: Option<String>,
    vendor_session_id: Option<String>,
    /// A resumed turn-based session: a `Session` id other than this one is a
    /// new conversation (§6.3).
    expected_vsid: Option<String>,
    process: Option<ProcessKey>,
    /// The Gilde facts this manager publishes itself (Terminal sessions; the
    /// Conversation and turn-based drivers publish their own).
    gilde: Option<GildeFacts>,
}

/// One session's `EvidenceSink` (see the module docs).
pub struct SessionSink {
    world: Arc<dyn EvidenceSink>,
    binder: Arc<dyn SessionBinder>,
    vendor: Vendor,
    account: Option<AccountId>,
    /// A process exit ends the session (not for turn-based: turn processes come and go).
    ends_on_exit: bool,
    state: Mutex<SinkState>,
    live: AtomicBool,
    events: OnceLock<mpsc::Sender<Cmd>>,
    /// Where a turn-based driver's content goes (moves with a new conversation).
    content_uid: Arc<Mutex<SessionUid>>,
    clock: Arc<dyn Fn() -> EpochMs + Send + Sync>,
}

impl SessionSink {
    pub fn new(
        uid: SessionUid,
        vendor: Vendor,
        account: Option<AccountId>,
        mode: ControlMode,
        world: Arc<dyn EvidenceSink>,
        binder: Arc<dyn SessionBinder>,
        clock: Arc<dyn Fn() -> EpochMs + Send + Sync>,
    ) -> Arc<SessionSink> {
        Arc::new(SessionSink {
            world,
            binder,
            vendor,
            account,
            ends_on_exit: mode != ControlMode::TurnBased,
            content_uid: Arc::new(Mutex::new(uid.clone())),
            state: Mutex::new(SinkState {
                uid,
                bound: HashSet::new(),
                spawn_id: None,
                token_override: None,
                vendor_session_id: None,
                expected_vsid: None,
                process: None,
                gilde: None,
            }),
            live: AtomicBool::new(true),
            events: OnceLock::new(),
            clock,
        })
    }

    pub fn uid(&self) -> SessionUid {
        self.state.lock().uid.clone()
    }

    pub fn content_uid(&self) -> Arc<Mutex<SessionUid>> {
        self.content_uid.clone()
    }

    pub fn is_live(&self) -> bool {
        self.live.load(Ordering::SeqCst)
    }

    pub(crate) fn set_live(&self, live: bool) {
        self.live.store(live, Ordering::SeqCst);
    }

    fn connect(&self, tx: mpsc::Sender<Cmd>) {
        let _ = self.events.set(tx);
    }

    /// Bind a spawn id to this session (A18.1, A20.1); idempotent.
    pub fn bind(&self, spawn_id: &str) {
        let (first, uid) = {
            let mut st = self.state.lock();
            let first = st.bound.insert(spawn_id.to_string());
            if st.spawn_id.is_none() {
                st.spawn_id = Some(spawn_id.to_string());
            }
            (first, st.uid.clone())
        };
        if first {
            let _ = self.binder.bind_spawn(spawn_id, Some(&uid), self.vendor, self.account.clone());
        }
    }

    pub fn set_process(&self, key: ProcessKey) {
        self.state.lock().process = Some(key);
    }

    pub fn set_vendor_session_id(&self, id: Option<String>) {
        self.state.lock().vendor_session_id = id;
    }

    /// A resumed turn-based session: another `Session` id is a new conversation.
    pub fn expect_vendor_session_id(&self, id: Option<String>) {
        self.state.lock().expected_vsid = id;
    }

    /// The hint of the evidence this manager publishes for the session.
    pub fn hint(&self) -> IdentityHint {
        let st = self.state.lock();
        IdentityHint {
            vendor: Some(self.vendor),
            account_id: self.account.clone(),
            vendor_session_id: st.vendor_session_id.clone(),
            process: st.process.clone(),
            gilde_spawn_id: st.token_override.clone().or_else(|| st.spawn_id.clone()),
        }
    }

    /// Publish facts this manager knows (Terminal sessions: Gilde, process, meta).
    pub fn publish(&self, facts: SourceFacts) {
        let at = (self.clock)();
        self.evidence(EvidenceUpdate { hint: self.hint(), facts, at });
    }

    /// Set and publish the Gilde facts of a session this manager drives itself.
    pub fn update_gilde(&self, f: impl FnOnce(&mut GildeFacts)) {
        let facts = {
            let mut st = self.state.lock();
            let Some(g) = st.gilde.as_mut() else { return };
            f(g);
            g.clone()
        };
        self.publish(SourceFacts::Gilde(facts));
    }

    pub fn init_gilde(&self, facts: GildeFacts) {
        self.state.lock().gilde = Some(facts.clone());
        self.publish(SourceFacts::Gilde(facts));
    }

    pub fn gilde(&self) -> Option<GildeFacts> {
        self.state.lock().gilde.clone()
    }

    /// §6.3: this turn started a NEW conversation. The session moves to a new
    /// uid (the driver keeps running under it), the old uid is released, and
    /// the new row says `turn.newConversation`.
    fn rehome(&self, at: EpochMs) {
        let (old, old_hint, new, token) = {
            let mut st = self.state.lock();
            let old = st.uid.clone();
            let old_hint = IdentityHint {
                vendor: Some(self.vendor),
                account_id: self.account.clone(),
                vendor_session_id: None,
                process: None,
                gilde_spawn_id: st.token_override.clone().or_else(|| st.spawn_id.clone()),
            };
            let new = SessionUid::mint();
            let token = format!("tb_{}", uuid::Uuid::now_v7().simple());
            st.uid = new.clone();
            st.token_override = Some(token.clone());
            st.bound.insert(token.clone());
            st.expected_vsid = None;
            (old, old_hint, new, token)
        };
        *self.content_uid.lock() = new.clone();
        let _ = self.binder.bind_spawn(&token, Some(&new), self.vendor, self.account.clone());
        // The old row is no longer held (its conversation could not be continued).
        self.world.evidence(EvidenceUpdate {
            hint: old_hint,
            facts: SourceFacts::Gilde(GildeFacts {
                spawned_at: at,
                mode: ControlMode::TurnBased,
                ready_at: Some(at),
                stop_cause: Some(StopCause::User),
                signals_sent: Vec::new(),
                start_failure: None,
            }),
            at,
        });
        self.binder.set_control_detail(&new, ControlFacts { pty: None, transport: Some(Transport::Turns), hosted_team: false });
        self.binder.set_details_extra(
            &new,
            DriverDetails { lines: vec![EvidenceLine::key("turn.newConversation")], ..DriverDetails::default() },
        );
        if let Some(tx) = self.events.get() {
            let _ = tx.try_send(Cmd::Rehomed { old, new });
        }
    }
}

impl EvidenceSink for SessionSink {
    fn evidence(&self, mut u: EvidenceUpdate) {
        // §6.3: a resumed turn reported another conversation.
        let new_conversation = {
            let st = self.state.lock();
            match (&st.expected_vsid, &u.hint.vendor_session_id) {
                (Some(want), Some(got)) => !got.is_empty() && want != got,
                _ => false,
            }
        };
        if new_conversation {
            self.rehome(u.at);
        }
        let (token, uid) = {
            let st = self.state.lock();
            (st.token_override.clone(), st.uid.clone())
        };
        if let Some(t) = token {
            u.hint.gilde_spawn_id = Some(t);
        }
        if let Some(sp) = u.hint.gilde_spawn_id.clone() {
            self.bind(&sp);
        }
        if let Some(vsid) = u.hint.vendor_session_id.clone().filter(|v| !v.is_empty()) {
            self.state.lock().vendor_session_id = Some(vsid);
        }
        let ended = self.ends_on_exit
            && matches!(u.facts, SourceFacts::Process(ProcessFacts::Exited { .. }) | SourceFacts::Process(ProcessFacts::Gone { .. }));
        let maybe_queue = matches!(u.facts, SourceFacts::Stream(_) | SourceFacts::Gilde(_));
        self.world.evidence(u);
        if ended && self.live.swap(false, Ordering::SeqCst) {
            if let Some(tx) = self.events.get() {
                let _ = tx.try_send(Cmd::Ended { uid });
            }
        } else if maybe_queue {
            if let Some(tx) = self.events.get() {
                let _ = tx.try_send(Cmd::Refresh);
            }
        }
    }

    fn location(&self, facts: LocationFacts) {
        self.world.location(facts);
    }

    fn source_status(&self, status: SourceStatus) {
        self.world.source_status(status);
    }

    fn account(&self, account: crate::contract::account::Account) {
        self.world.account(account);
    }

    fn compat(&self, list: Vec<crate::contract::compat::ClaudeCompat>) {
        self.world.compat(list);
    }

    fn integrations(&self, state: IntegrationsState) {
        self.world.integrations(state);
    }

    fn lifecycle(&self, op: LifecycleOp) {
        self.world.lifecycle(op);
    }

    fn notify_paused(&self, until: Option<EpochMs>) {
        self.world.notify_paused(until);
    }
}

/// A turn-based driver's content goes to the session's current uid (§6.3).
pub struct RehomingContent {
    pub inner: Arc<dyn ContentHubApi>,
    pub uid: Arc<Mutex<SessionUid>>,
}

impl ContentHubApi for RehomingContent {
    fn publish(&self, _uid: &SessionUid, update: ContentUpdate) {
        let uid = self.uid.lock().clone();
        self.inner.publish(&uid, update);
    }

    fn has_subscribers(&self, _uid: &SessionUid) -> bool {
        let uid = self.uid.lock().clone();
        self.inner.has_subscribers(&uid)
    }
}

// ---------------------------------------------------------------------------
// ConvoHooks (A19.1)
// ---------------------------------------------------------------------------

/// T05's hooks for one Conversation session (A18.3, A19.1).
pub struct SessionHooks {
    binder: Arc<dyn SessionBinder>,
    accounts: Arc<dyn AccountsPort>,
    details: Mutex<DriverDetails>,
}

impl SessionHooks {
    pub fn new(binder: Arc<dyn SessionBinder>, accounts: Arc<dyn AccountsPort>) -> Arc<SessionHooks> {
        Arc::new(SessionHooks { binder, accounts, details: Mutex::new(DriverDetails::default()) })
    }

    fn publish(&self, uid: &SessionUid, f: impl FnOnce(&mut DriverDetails)) {
        let details = {
            let mut d = self.details.lock();
            f(&mut d);
            d.clone()
        };
        self.binder.set_details_extra(uid, details);
    }
}

impl ConvoHooks for SessionHooks {
    fn queued(&self, uid: &SessionUid, queued: Vec<QueuedMessage>) {
        self.binder.set_queued(uid, queued);
    }

    fn quota(&self, account: &AccountId, quota: Quota) {
        self.accounts.observed_quota(account, quota);
    }

    fn signed_out(&self, account: &AccountId) {
        self.accounts.signed_out(account);
    }

    fn approval_unconfirmed(&self, uid: &SessionUid, tool: &str) {
        let tool = tool.to_string();
        self.publish(uid, move |d| d.approval_channel_unconfirmed = Some(tool));
    }

    fn notice(&self, uid: &SessionUid, notice: Notice) {
        self.publish(uid, move |d| {
            let line = EvidenceLine { key: notice.key, params: notice.params };
            if !d.lines.iter().any(|l| l.key == line.key) {
                d.lines.push(line);
            }
        });
    }

    fn version_note(&self, uid: &SessionUid, note: &str) {
        let note = note.to_string();
        self.publish(uid, move |d| d.version_note = Some(note));
    }

    fn permission_mode(&self, uid: &SessionUid, mode: Option<crate::contract::common::PermissionMode>) {
        self.binder.set_permission_mode(uid, mode);
    }
}

// ---------------------------------------------------------------------------
// PTY exits → the actor
// ---------------------------------------------------------------------------

/// T06's exit sink for one Terminal session: forwards the exit to the actor
/// (or keeps it until the actor exists, when the child died at once).
#[derive(Default)]
pub struct ActorExitSink {
    tx: OnceLock<mpsc::Sender<Cmd>>,
    early: Mutex<Option<(ExitInfo, EpochMs)>>,
}

impl ActorExitSink {
    pub fn new() -> Arc<ActorExitSink> {
        Arc::new(ActorExitSink::default())
    }

    fn connect(&self, tx: mpsc::Sender<Cmd>) {
        if self.tx.set(tx.clone()).is_ok() {
            if let Some((exit, at)) = self.early.lock().take() {
                let _ = tx.try_send(Cmd::Exited { exit, at });
            }
        }
    }
}

impl PtyExitSink for ActorExitSink {
    fn exited(&self, _pty: &PtyId, exit: ExitInfo, at: EpochMs) {
        match self.tx.get() {
            Some(tx) => {
                let _ = tx.try_send(Cmd::Exited { exit, at });
            }
            None => *self.early.lock() = Some((exit, at)),
        }
    }
}

// ---------------------------------------------------------------------------
// the actor
// ---------------------------------------------------------------------------

type Reply<T> = oneshot::Sender<Result<T, IpcError>>;

/// What an actor runs, in arrival order.
pub(crate) enum Cmd {
    Send { msg: OutgoingMessage, retry: bool, reply: Reply<TurnSendRes> },
    CancelQueued(ClientMsgId, Reply<bool>),
    Answer { req: RequestId, decision: Decision, via: AnswerVia, reply: Reply<(AnswerOutcome, Option<DecisionKind>)> },
    ScreenKey { req: RequestId, rev: String, action: ScreenPromptAction, reply: Reply<ScreenKeyOutcome> },
    Interrupt(bool, Reply<InterruptOutcome>),
    ForceStop(Reply<bool>),
    SetModel(Option<String>, Reply<AppliesFrom>),
    SetMode(PermissionMode, Reply<()>),
    Stop(StopCause, Reply<StopOutcome>),
    Models(Reply<SessionModelsRes>),
    Detail(RequestId, Reply<InteractionDetailRes>),
    Technical(Reply<SessionTechnicalRes>),
    LastText(oneshot::Sender<Option<String>>),
    /// The typed-after-ready first prompt (§6.2.2).
    FirstPrompt(String),
    /// A Terminal session's process exited (T06's exit sink).
    Exited { exit: ExitInfo, at: EpochMs },
    /// The driver's process ended (seen on its evidence).
    Ended { uid: SessionUid },
    /// The driver's queue may have changed (turn-based held message, A18.3).
    Refresh,
    /// A turn-based session moved to a new uid (§6.3).
    Rehomed { old: SessionUid, new: SessionUid },
}

/// What a session was started with (kept for `session_retry_as_terminal`).
pub use super::start::LaunchRequest;

/// The mutable facts of one actor's session.
#[derive(Clone)]
pub struct ActorInfo {
    pub vendor: Vendor,
    pub account_id: Option<AccountId>,
    pub mode: ControlMode,
    pub transport: Transport,
    pub pty: Option<PtyId>,
    pub pid: Option<i32>,
    pub hosted_team: bool,
    /// Launched with `--allow-dangerously-skip-permissions` (bypass at start).
    pub launched_bypass: bool,
    /// The start request (memory only: it may hold the first prompt).
    pub launch: Option<LaunchRequest>,
    pub started_at: EpochMs,
}

impl std::fmt::Debug for ActorInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActorInfo")
            .field("vendor", &self.vendor)
            .field("mode", &self.mode)
            .field("transport", &self.transport)
            .field("pty", &self.pty)
            .field("pid", &self.pid)
            .field("hosted_team", &self.hosted_team)
            .finish_non_exhaustive()
    }
}

/// The command side of one session actor.
pub struct ActorHandle {
    tx: Mutex<Option<mpsc::Sender<Cmd>>>,
    sink: Arc<SessionSink>,
    info: Mutex<ActorInfo>,
}

impl std::fmt::Debug for ActorHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActorHandle").field("uid", &self.uid()).field("info", &*self.info.lock()).finish()
    }
}

/// What the actor task needs besides its driver.
pub(crate) struct ActorCtx {
    pub sink: Arc<SessionSink>,
    pub binder: Arc<dyn SessionBinder>,
    pub panehost: Option<Arc<dyn PaneHostApi>>,
    pub manager: Weak<SessionsManager>,
}

/// The `UNSUPPORTED{sessionEnded}` a driver answers once its process is gone.
fn ended(e: &IpcError) -> bool {
    e.code == ErrorCode::Unsupported && e.params.get("cap").and_then(|c| c.as_str()) == Some("sessionEnded")
}

/// Keep a command's errors inside its manifest list: a driver whose session
/// ended answers the command's own capability refusal.
fn ended_as(e: IpcError, cap: &str) -> IpcError {
    if ended(&e) {
        IpcError::unsupported(cap)
    } else {
        e
    }
}

impl ActorHandle {
    /// Start the actor task for a session whose driver is running.
    pub(crate) fn start(driver: Box<dyn ActorDriver>, info: ActorInfo, ctx: ActorCtx) -> Arc<ActorHandle> {
        let (tx, rx) = mpsc::channel::<Cmd>(256);
        ctx.sink.connect(tx.clone());
        let sink = ctx.sink.clone();
        let handle = Arc::new(ActorHandle { tx: Mutex::new(Some(tx)), sink, info: Mutex::new(info) });
        let weak = Arc::downgrade(&handle);
        tokio::spawn(run(driver, rx, ctx, weak));
        handle
    }

    pub fn uid(&self) -> SessionUid {
        self.sink.uid()
    }

    pub fn info(&self) -> ActorInfo {
        self.info.lock().clone()
    }

    pub fn sink(&self) -> &Arc<SessionSink> {
        &self.sink
    }

    /// The exit sink of a Terminal session reaches this actor.
    pub(crate) fn connect_exit(&self, exit: &ActorExitSink) {
        if let Some(tx) = self.tx.lock().clone() {
            exit.connect(tx);
        }
    }

    pub async fn is_live(&self) -> bool {
        self.sink.is_live() && self.tx.lock().is_some()
    }

    /// Drop the command channel: the task ends and the driver is dropped.
    pub(crate) fn retire(&self) {
        self.tx.lock().take();
    }

    async fn call<T>(&self, make: impl FnOnce(Reply<T>) -> Cmd, cap: &str) -> Result<T, IpcError> {
        let tx = self.tx.lock().clone().ok_or_else(|| IpcError::unsupported(cap))?;
        let (rtx, rrx) = oneshot::channel();
        tx.send(make(rtx)).await.map_err(|_| IpcError::unsupported(cap))?;
        rrx.await.map_err(|_| IpcError::unsupported(cap))?
    }

    fn post(&self, cmd: Cmd) {
        if let Some(tx) = self.tx.lock().clone() {
            let _ = tx.try_send(cmd);
        }
    }

    pub async fn send(&self, id: ClientMsgId, text: String, priority: SendPriority, retry: bool) -> Result<TurnSendRes, IpcError> {
        let msg = OutgoingMessage { client_msg_id: id, text, priority };
        self.call(move |reply| Cmd::Send { msg, retry, reply }, "send").await
    }

    pub async fn cancel_queued(&self, id: ClientMsgId) -> Result<bool, IpcError> {
        match self.call(move |r| Cmd::CancelQueued(id, r), "send").await {
            Err(e) if e.code == ErrorCode::Unsupported => Err(IpcError::not_found("message")),
            other => other,
        }
    }

    pub async fn answer(&self, req: RequestId, decision: Decision, via: AnswerVia) -> Result<(AnswerOutcome, Option<DecisionKind>), IpcError> {
        match self.call(move |reply| Cmd::Answer { req, decision, via, reply }, "sessionEnded").await {
            Err(e) if ended(&e) => Ok((AnswerOutcome::Withdrawn, None)),
            other => other,
        }
    }

    pub async fn screen_key(&self, req: RequestId, rev: String, action: ScreenPromptAction) -> Result<ScreenKeyOutcome, IpcError> {
        match self.call(move |reply| Cmd::ScreenKey { req, rev, action, reply }, "sessionEnded").await {
            Err(e) if ended(&e) => Ok(ScreenKeyOutcome::Withdrawn),
            other => other,
        }
    }

    pub async fn interrupt(&self, discard_queued: bool) -> Result<InterruptOutcome, IpcError> {
        self.call(move |r| Cmd::Interrupt(discard_queued, r), "interrupt").await
    }

    pub async fn force_stop(&self) -> Result<bool, IpcError> {
        self.call(Cmd::ForceStop, "forceStop").await
    }

    pub async fn set_model(&self, model: Option<String>) -> Result<AppliesFrom, IpcError> {
        self.call(move |r| Cmd::SetModel(model, r), "setModel").await
    }

    pub async fn set_permission_mode(&self, mode: PermissionMode) -> Result<(), IpcError> {
        self.call(move |r| Cmd::SetMode(mode, r), "setPermissionMode").await
    }

    pub async fn stop(&self, why: StopCause) -> Result<StopOutcome, IpcError> {
        match self.call(move |r| Cmd::Stop(why, r), "sessionEnded").await {
            Err(e) if ended(&e) => Ok(StopOutcome { stopped: true, survivors: 0 }),
            other => other,
        }
    }

    pub async fn models(&self) -> Result<SessionModelsRes, IpcError> {
        self.call(Cmd::Models, "setModel").await
    }

    pub async fn detail(&self, req: RequestId) -> Result<InteractionDetailRes, IpcError> {
        match self.call(move |r| Cmd::Detail(req, r), "sessionEnded").await {
            Err(e) if ended(&e) => Err(IpcError::not_found("request")),
            other => other,
        }
    }

    pub async fn technical(&self) -> Result<SessionTechnicalRes, IpcError> {
        self.call(Cmd::Technical, "notLive").await
    }

    /// The last user text sent in this session (memory only; `turn_retry`).
    pub async fn last_text(&self) -> Option<String> {
        let tx = self.tx.lock().clone()?;
        let (rtx, rrx) = oneshot::channel();
        tx.send(Cmd::LastText(rtx)).await.ok()?;
        rrx.await.ok().flatten()
    }

    /// Deliver the typed-after-ready first prompt (§6.2.2) in order with the
    /// session's other commands.
    pub(crate) fn first_prompt(&self, text: String) {
        self.post(Cmd::FirstPrompt(text));
    }
}

fn mint_msg_id() -> ClientMsgId {
    ClientMsgId::parse(uuid::Uuid::new_v4().to_string()).expect("a v4 uuid is a ClientMsgId")
}

/// Remembered `clientMsgId` outcomes (§4.1 rule 4: the first outcome for the
/// session's life; bounded).
struct Sent {
    entries: VecDeque<(ClientMsgId, Result<TurnSendRes, IpcError>)>,
}

impl Sent {
    fn get(&self, id: &ClientMsgId) -> Option<Result<TurnSendRes, IpcError>> {
        self.entries.iter().find(|(k, _)| k == id).map(|(_, v)| v.clone())
    }

    fn put(&mut self, id: ClientMsgId, res: Result<TurnSendRes, IpcError>) {
        self.entries.push_back((id, res));
        while self.entries.len() > SENT_REMEMBERED {
            self.entries.pop_front();
        }
    }
}

async fn publish_queue(driver: &dyn ActorDriver, ctx: &ActorCtx) {
    if let Some(q) = driver.queued().await {
        ctx.binder.set_queued(&ctx.sink.uid(), q);
    }
}

async fn run(mut driver: Box<dyn ActorDriver>, mut rx: mpsc::Receiver<Cmd>, ctx: ActorCtx, handle: Weak<ActorHandle>) {
    let mut sent = Sent { entries: VecDeque::new() };
    let mut last_text: Option<String> = None;
    let mut last_queue: Option<Vec<QueuedMessage>> = None;
    while let Some(cmd) = rx.recv().await {
        match cmd {
            Cmd::Send { msg, retry, reply } => {
                if let Some(prev) = sent.get(&msg.client_msg_id) {
                    let _ = reply.send(prev);
                    continue;
                }
                let id = msg.client_msg_id.clone();
                let text = msg.text.clone();
                let res = match driver.send(msg).await {
                    Ok(delivery) => {
                        let position = if delivery == SendDelivery::Queued {
                            let waiting = driver.waiting().await;
                            waiting.iter().position(|m| m.client_msg_id == id).map(|i| i as u32 + 1)
                        } else {
                            None
                        };
                        Ok(TurnSendRes { delivery, position })
                    }
                    Err(e) => Err(ended_as(e, "send")),
                };
                if res.is_ok() && !retry {
                    last_text = Some(text);
                }
                sent.put(id, res.clone());
                publish_queue(driver.as_ref(), &ctx).await;
                let _ = reply.send(res);
            }
            Cmd::CancelQueued(id, reply) => {
                let res = driver.cancel_queued(&id).await;
                publish_queue(driver.as_ref(), &ctx).await;
                let _ = reply.send(res);
            }
            Cmd::Answer { req, decision, via, reply } => {
                // A19.6: an AllowOnce by shortcut on a default-to-no card.
                if matches!(decision, Decision::AllowOnce) && via == AnswerVia::Shortcut {
                    if let Some(card) = driver.pending(&req).await {
                        if card.default_to_no && decision.fits(&card).is_ok() {
                            let _ = reply.send(Err(IpcError::refused("defaultToNo")));
                            continue;
                        }
                    }
                }
                let res = match driver.answer_with_previous(&req, decision).await {
                    Err(e) if ended(&e) => Ok((AnswerOutcome::Withdrawn, None)),
                    other => other,
                };
                let _ = reply.send(res);
            }
            Cmd::ScreenKey { req, rev, action, reply } => {
                let res = match driver.screen_key(&req, &rev, action).await {
                    Err(e) if ended(&e) => Ok(ScreenKeyOutcome::Withdrawn),
                    other => other,
                };
                let _ = reply.send(res);
            }
            Cmd::Interrupt(discard, reply) => {
                let res = driver.interrupt(discard).await.map_err(|e| ended_as(e, "interrupt"));
                publish_queue(driver.as_ref(), &ctx).await;
                let _ = reply.send(res);
            }
            Cmd::ForceStop(reply) => {
                let res = driver.force_stop().await.map_err(|e| ended_as(e, "forceStop"));
                let _ = reply.send(res);
            }
            Cmd::SetModel(model, reply) => {
                let res = driver.set_model(model).await.map_err(|e| ended_as(e, "setModel"));
                let _ = reply.send(res);
            }
            Cmd::SetMode(mode, reply) => {
                let launched_bypass = handle.upgrade().map(|h| h.info().launched_bypass).unwrap_or(false);
                let res = if mode == PermissionMode::BypassPermissions && !launched_bypass {
                    Err(IpcError::refused("needsLaunchFlag"))
                } else {
                    driver.set_permission_mode(mode).await.map_err(|e| ended_as(e, "setPermissionMode"))
                };
                let _ = reply.send(res);
            }
            Cmd::Stop(why, reply) => {
                // A Terminal session: World must read the exit as Gilde's stop (R1).
                if ctx.sink.gilde().is_some() {
                    ctx.sink.update_gilde(|g| {
                        g.stop_cause = Some(why);
                        for s in [libc::SIGHUP, libc::SIGTERM] {
                            if !g.signals_sent.contains(&s) {
                                g.signals_sent.push(s);
                            }
                        }
                    });
                }
                let res = match driver.stop(why).await {
                    Err(e) if ended(&e) => Ok(StopOutcome { stopped: true, survivors: 0 }),
                    other => other,
                };
                let mode = driver.mode();
                if mode == ControlMode::TurnBased || res.as_ref().is_ok_and(|o| o.stopped) {
                    ctx.sink.set_live(false);
                    if let Some(m) = ctx.manager.upgrade() {
                        m.note_ended(&ctx.sink.uid());
                    }
                }
                publish_queue(driver.as_ref(), &ctx).await;
                let _ = reply.send(res);
            }
            Cmd::Models(reply) => {
                let res = driver
                    .models()
                    .await
                    .map(|(models, current)| SessionModelsRes { models, current })
                    .map_err(|e| ended_as(e, "setModel"));
                let _ = reply.send(res);
            }
            Cmd::Detail(req, reply) => {
                let res = match driver.detail(&req).await {
                    Err(e) if ended(&e) => Err(IpcError::not_found("request")),
                    other => other,
                };
                let _ = reply.send(res);
            }
            Cmd::Technical(reply) => {
                let res = driver.technical().await.map_err(|e| ended_as(e, "notLive"));
                let _ = reply.send(res);
            }
            Cmd::LastText(reply) => {
                let _ = reply.send(last_text.clone());
            }
            Cmd::FirstPrompt(text) => {
                let msg = OutgoingMessage { client_msg_id: mint_msg_id(), text: text.clone(), priority: SendPriority::Next };
                match driver.send(msg).await {
                    Ok(_) => last_text = Some(text),
                    Err(e) => {
                        let (code, key, _) = e.log_fields();
                        tracing::warn!(target: "sessions", code, key, "the first prompt was not delivered");
                    }
                }
            }
            Cmd::Exited { exit, at } => {
                ctx.sink.publish(SourceFacts::Process(ProcessFacts::Exited { at, code: exit.code, signal: exit.signal }));
                ctx.sink.set_live(false);
                let uid = ctx.sink.uid();
                let hosted = handle.upgrade().map(|h| h.info().hosted_team).unwrap_or(false);
                if hosted {
                    if let Some(host) = &ctx.panehost {
                        host.lead_exited(&uid).await;
                    }
                }
                if let Some(m) = ctx.manager.upgrade() {
                    m.note_ended(&uid);
                }
            }
            Cmd::Ended { uid } => {
                if let Some(m) = ctx.manager.upgrade() {
                    m.note_ended(&uid);
                }
            }
            Cmd::Refresh => {
                if let Some(q) = driver.queued().await {
                    if last_queue.as_ref() != Some(&q) {
                        last_queue = Some(q.clone());
                        ctx.binder.set_queued(&ctx.sink.uid(), q);
                    }
                }
            }
            Cmd::Rehomed { old, new } => {
                if let Some(m) = ctx.manager.upgrade() {
                    m.rehome(&old, &new);
                }
                ctx.binder.set_queued(&old, Vec::new());
                if let Some(q) = driver.queued().await {
                    ctx.binder.set_queued(&new, q);
                }
            }
        }
    }
    // The channel closed (retired): an unfinished Terminal session keeps its
    // process (a quit or stop goes through `Stop`); nothing is signalled here.
    drop(driver);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ended_errors_map_to_the_commands_own_capability() {
        let e = IpcError::unsupported("sessionEnded");
        assert!(ended(&e));
        let mapped = ended_as(e, "send");
        assert_eq!(mapped.params["cap"], "send");
        let other = IpcError::conflict("queueFull");
        assert_eq!(ended_as(other.clone(), "send"), other);
    }

    #[test]
    fn sent_outcomes_are_bounded_and_first_wins_by_lookup() {
        let mut s = Sent { entries: VecDeque::new() };
        let id = mint_msg_id();
        s.put(id.clone(), Ok(TurnSendRes { delivery: SendDelivery::Sent, position: None }));
        assert_eq!(s.get(&id).unwrap().unwrap().delivery, SendDelivery::Sent);
        for _ in 0..SENT_REMEMBERED + 10 {
            s.put(mint_msg_id(), Err(IpcError::conflict("queueFull")));
        }
        assert!(s.get(&id).is_none(), "the oldest outcome is forgotten after the bound");
        assert_eq!(s.entries.len(), SENT_REMEMBERED);
    }
}
