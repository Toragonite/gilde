//! `TurnDriver: SessionDriver` — the turn-based mode (contract §6.0, §6.3):
//! codex, cursor and agy conversations, one agent-mesh turn at a time through
//! the sidecar ([`SidecarApi`]).
//!
//! The session actor (T10) owns one driver per held session and calls it in
//! arrival order. The driver:
//!
//!   * **starts a turn** per message — `turn.start` with every field (cwd, model,
//!     permission mode, account, wall clock, and `resumeId` = the conversation
//!     the vendor reported, from turn 2 on, or the one being continued);
//!   * **holds one message** while a turn runs ([`super::held`]) and starts it as
//!     the next turn when the running one ends with `Done` (never after `Error`);
//!   * **feeds World** (S1 facts through [`EvidenceSink`]): the Gilde facts
//!     (`mode: TurnBased`), the turn phase, each turn's end and its outcome —
//!     `Done{ok|incomplete}` → Ok, `Done{stopped}` → Interrupted, `Error{auth}` →
//!     Sign-in needed, `{quota}` → Usage limit (this account), `{exec|unknown}` →
//!     Agent error, or Didn't start when no `Session` event ever arrived — and
//!     the vendor conversation id on the identity hint (a `Session` id that
//!     differs from the turn's `resumeId` is a NEW conversation: World forks it,
//!     §6.3, PM-23);
//!   * **feeds the ContentHub** through a [`TurnRendererFactory`] (T04's mesh
//!     renderer in the app; the prompt and the turn's blocks, memory only);
//!   * **stops**: `turn_interrupt` → `turn.cancel` (SIGTERM of the turn's group,
//!     outcome `stopped`, the session stays held and resumable); `stop` cancels,
//!     drops the held message and releases the session (Stopped).
//!
//! Nothing here persists text or logs content: prompts, deltas and errors stay
//! in memory (§3.1 rule 5, §6.3 "Nothing is persisted to work around it").

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::contract::activity::{
    ErrorSub, GildeFacts, ProcessFacts, StopCause, StreamFacts, TurnEnd, TurnOutcome, TurnPhase,
};
use crate::contract::common::{AbsPath, EpochMs, PermissionMode, Vendor};
use crate::contract::error::{IpcError, VendorErrorClass};
use crate::contract::ids::{AccountId, ClientMsgId, RequestId, SessionUid};
use crate::contract::interaction::Decision;
use crate::contract::ipc::{AppliesFrom, InterruptOutcome, ScreenKeyOutcome, ScreenPromptAction, SendDelivery};
use crate::contract::services::{
    AnswerOutcome, ContentHubApi, ContentUpdate, EvidenceSink, EvidenceUpdate, IdentityHint,
    OutgoingMessage, SessionDriver, SessionMetaFacts, SidecarApi, SourceFacts, StopOutcome,
    TurnEventSink, UsageFacts, UsageSource,
};
use crate::contract::session::{ControlMode, QueuedMessage, UsageToday};
use crate::contract::sidecar::{ProbeParams, TurnDoneStatus, TurnEvent, TurnStartParams, TurnUsage};
use crate::sidecar::HELPER_RESTARTED;
use crate::turns::held::{HeldMessage, HeldSlot};

/// Stream facts are re-published at most this often while a turn streams.
pub const FRAME_FACTS_EVERY_MS: EpochMs = 1_000;
/// How many `clientMsgId`s are remembered for `turn_send` idempotency.
pub const SENT_IDS_KEPT: usize = 256;
/// Usage reports: at most one per session per second (A10.4).
pub const USAGE_REPORT_EVERY_MS: EpochMs = 1_000;

// ---------------------------------------------------------------------------
// spend (A10.4): the `Turns` usage producer
// ---------------------------------------------------------------------------

/// The local calendar day `YYYY-MM-DD` of `at` (the machine's time zone).
pub fn local_day(at: EpochMs) -> String {
    let secs = at.div_euclid(1_000) as libc::time_t;
    // SAFETY: `tm` is a plain C struct written by `localtime_r`/`gmtime_r`, which
    // are thread-safe and only read `secs`.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { !libc::localtime_r(&secs, &mut tm).is_null() }
        || unsafe { !libc::gmtime_r(&secs, &mut tm).is_null() };
    if !ok {
        return "1970-01-01".to_string();
    }
    format!("{:04}-{:02}-{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday)
}

/// One turn's spend as last reported (each field the largest value seen, so a
/// repeated or replayed report never counts twice).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct TurnSpend {
    input: Option<u64>,
    output: Option<u64>,
    cached: Option<u64>,
    cost_micros: Option<u64>,
}

fn max_opt(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (x, None) => x,
        (None, y) => y,
    }
}

fn sum_opt(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.saturating_add(y)),
        (x, None) => x,
        (None, y) => y,
    }
}

/// `cost_usd` → integer micro-USD; only a finite, non-negative vendor value.
pub fn cost_micros_of(cost_usd: Option<f64>) -> Option<u64> {
    cost_usd
        .filter(|c| c.is_finite() && *c >= 0.0)
        .map(|c| (c * 1_000_000.0).round() as u64)
}

/// The `Turns` producer's running total for one session (A10.4): per local
/// day, each turn counted once (by turn id), reported when the total changes,
/// at most once per second. No usage reported = nothing to report (absent is
/// "unknown", never 0).
#[derive(Debug, Default)]
pub struct UsageMeter {
    day: Option<String>,
    turns: BTreeMap<String, TurnSpend>,
    last_sent: Option<UsageToday>,
    last_sent_at: Option<EpochMs>,
}

impl UsageMeter {
    pub fn new() -> UsageMeter {
        UsageMeter::default()
    }

    /// Record what a turn reported, observed on local day `day`. A new day
    /// starts a new total.
    pub fn observe(&mut self, day: &str, turn_id: &str, usage: Option<&TurnUsage>, cost_usd: Option<f64>) {
        if self.day.as_deref() != Some(day) {
            self.day = Some(day.to_string());
            self.turns.clear();
        }
        let cost_micros = cost_micros_of(cost_usd);
        if usage.is_none() && cost_micros.is_none() {
            return;
        }
        let spend = self.turns.entry(turn_id.to_string()).or_default();
        if let Some(u) = usage {
            spend.input = max_opt(spend.input, u.input_tokens);
            spend.output = max_opt(spend.output, u.output_tokens);
            spend.cached = max_opt(spend.cached, u.cached_input_tokens);
        }
        spend.cost_micros = max_opt(spend.cost_micros, cost_micros);
    }

    /// Today's running total; `None` until something was reported today.
    pub fn total(&self) -> Option<UsageToday> {
        let day = self.day.clone()?;
        let mut today = UsageToday {
            day,
            input_tokens: None,
            output_tokens: None,
            cached_tokens: None,
            cost_micros: None,
        };
        for spend in self.turns.values() {
            today.input_tokens = sum_opt(today.input_tokens, spend.input);
            today.output_tokens = sum_opt(today.output_tokens, spend.output);
            today.cached_tokens = sum_opt(today.cached_tokens, spend.cached);
            today.cost_micros = sum_opt(today.cost_micros, spend.cost_micros);
        }
        let reported = today.input_tokens.is_some()
            || today.output_tokens.is_some()
            || today.cached_tokens.is_some()
            || today.cost_micros.is_some();
        reported.then_some(today)
    }

    /// The report to send now: the total changed since the last report and the
    /// last one is at least a second old.
    pub fn due(&mut self, now: EpochMs) -> Option<UsageToday> {
        let total = self.total()?;
        if self.last_sent.as_ref() == Some(&total) {
            return None;
        }
        if self
            .last_sent_at
            .is_some_and(|at| now.saturating_sub(at) < USAGE_REPORT_EVERY_MS)
        {
            return None;
        }
        self.last_sent = Some(total.clone());
        self.last_sent_at = Some(now);
        Some(total)
    }

    /// Milliseconds until a held-back change may be sent; `None` when nothing waits.
    pub fn wait_ms(&self, now: EpochMs) -> Option<u64> {
        let total = self.total()?;
        if self.last_sent.as_ref() == Some(&total) {
            return None;
        }
        let at = self.last_sent_at?;
        Some(USAGE_REPORT_EVERY_MS.saturating_sub(now.saturating_sub(at)).max(0) as u64)
    }
}

// ---------------------------------------------------------------------------
// content rendering seam
// ---------------------------------------------------------------------------

/// Turns one turn's events into conversation updates. The app binds T04's
/// `render::mesh::MeshTurnRenderer` (ids `<turnId>#<k>`, sanitized text, the
/// System lines for `stopped` / errors); tests bind a recorder.
pub trait TurnRenderer: Send {
    /// The user's message that opens the turn.
    fn user_prompt(&mut self, text: &str, client_msg_id: Option<ClientMsgId>, at: EpochMs) -> Vec<ContentUpdate>;
    /// One event of the turn.
    fn on_event(&mut self, event: &TurnEvent, at: EpochMs) -> Vec<ContentUpdate>;
}

/// Makes the renderer of each turn.
pub trait TurnRendererFactory: Send + Sync {
    fn for_turn(&self, turn_id: &str) -> Box<dyn TurnRenderer>;
}

/// A factory that renders nothing (a session nobody shows, or before T04's
/// renderer is bound).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoContent;

struct NoContentRenderer;

impl TurnRenderer for NoContentRenderer {
    fn user_prompt(&mut self, _text: &str, _id: Option<ClientMsgId>, _at: EpochMs) -> Vec<ContentUpdate> {
        Vec::new()
    }
    fn on_event(&mut self, _event: &TurnEvent, _at: EpochMs) -> Vec<ContentUpdate> {
        Vec::new()
    }
}

impl TurnRendererFactory for NoContent {
    fn for_turn(&self, _turn_id: &str) -> Box<dyn TurnRenderer> {
        Box::new(NoContentRenderer)
    }
}

// ---------------------------------------------------------------------------
// spec and dependencies
// ---------------------------------------------------------------------------

/// What a turn-based session is (fixed for its life except the model).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnSessionSpec {
    pub uid: SessionUid,
    /// codex, cursor or agy (claude is Conversation or Terminal, §4.2.2).
    pub vendor: Vendor,
    pub account_id: AccountId,
    pub cwd: AbsPath,
    /// The model of the next turn (`session_set_model` → next turn).
    pub model: Option<String>,
    /// Fixed at start (ux-ia §2.2); the sidecar maps it to the vendor's flag.
    pub permission_mode: Option<PermissionMode>,
    /// The vendor conversation being continued (Continue / Resume / Continue
    /// all). The caller has verified a cursor/agy id on disk (§3.2).
    pub resume_id: Option<String>,
    /// How World addresses this session (T10 sets it; the driver adds the
    /// vendor conversation id once the vendor reports one).
    pub hint: IdentityHint,
    /// Each turn's wall clock; `None` = the sidecar's default (45 min).
    pub timeout_ms: Option<u64>,
}

/// What the driver talks to.
#[derive(Clone)]
pub struct TurnDeps {
    pub sidecar: Arc<dyn SidecarApi>,
    pub evidence: Arc<dyn EvidenceSink>,
    pub content: Arc<dyn ContentHubApi>,
    pub renderer: Arc<dyn TurnRendererFactory>,
    /// Wall clock, ms (tests pin it).
    pub clock: Arc<dyn Fn() -> EpochMs + Send + Sync>,
    /// The local calendar day of a wall-clock time (tests pin it).
    pub day: Arc<dyn Fn(EpochMs) -> String + Send + Sync>,
}

impl TurnDeps {
    /// The dependencies with the real clock and the machine's time zone.
    pub fn new(
        sidecar: Arc<dyn SidecarApi>,
        evidence: Arc<dyn EvidenceSink>,
        content: Arc<dyn ContentHubApi>,
        renderer: Arc<dyn TurnRendererFactory>,
    ) -> TurnDeps {
        TurnDeps {
            sidecar,
            evidence,
            content,
            renderer,
            clock: TurnDeps::system_clock(),
            day: TurnDeps::local_day(),
        }
    }

    /// The real wall clock.
    pub fn system_clock() -> Arc<dyn Fn() -> EpochMs + Send + Sync> {
        Arc::new(crate::store::now_ms)
    }

    /// The machine's local calendar day.
    pub fn local_day() -> Arc<dyn Fn(EpochMs) -> String + Send + Sync> {
        Arc::new(local_day)
    }
}

// ---------------------------------------------------------------------------
// state
// ---------------------------------------------------------------------------

struct RunningTurn {
    turn_id: String,
    /// The conversation this turn was asked to continue.
    resume_id: Option<String>,
    saw_session: bool,
    renderer: Box<dyn TurnRenderer>,
    /// The user's message, until it is published (it always precedes the
    /// turn's own blocks).
    opening: Option<Vec<ContentUpdate>>,
}

/// A reserved turn, ready for `turn.start`.
struct Launch {
    needs_model: bool,
    params: TurnStartParams,
}

struct TurnState {
    model: Option<String>,
    /// The vendor conversation of this session, once reported.
    conversation_id: Option<String>,
    /// `false` once a turn ended with no resumable id (`""`, §6.3).
    resumable: bool,
    current: Option<RunningTurn>,
    held: HeldSlot,
    spawned_at: EpochMs,
    ready_at: Option<EpochMs>,
    phase_since: EpochMs,
    last_turn_start_at: Option<EpochMs>,
    last_turn_end: Option<TurnEnd>,
    last_frame_at: EpochMs,
    last_frame_published: EpochMs,
    stopped: Option<StopCause>,
    sent: VecDeque<(ClientMsgId, SendDelivery)>,
    turns_started: u64,
    usage: UsageMeter,
    usage_flush_scheduled: bool,
}

enum Msg {
    Event { turn_id: String, seq: u64, event: TurnEvent },
}

struct Inner {
    spec: TurnSessionSpec,
    deps: TurnDeps,
    state: Mutex<TurnState>,
    events: mpsc::UnboundedSender<Msg>,
}

/// The sink a turn's events arrive through (`SidecarApi::turn_start`).
struct RouteSink {
    turn_id: String,
    tx: mpsc::UnboundedSender<Msg>,
}

impl TurnEventSink for RouteSink {
    fn event(&self, seq: u64, event: TurnEvent) {
        let _ = self.tx.send(Msg::Event {
            turn_id: self.turn_id.clone(),
            seq,
            event,
        });
    }
}

/// The turn-based driver. Cheap to hold; its event task lives as long as it.
pub struct TurnDriver {
    inner: Arc<Inner>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for TurnDriver {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn new_turn_id() -> String {
    format!("tn_{}", uuid::Uuid::now_v7().simple())
}

impl TurnDriver {
    /// A held session (§6.3): no process until its first message. Publishes the
    /// Gilde, metadata and idle stream facts. Must run inside a tokio runtime.
    pub fn new(spec: TurnSessionSpec, deps: TurnDeps) -> TurnDriver {
        let now = (deps.clock)();
        let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
        let inner = Arc::new(Inner {
            state: Mutex::new(TurnState {
                model: spec.model.clone(),
                conversation_id: spec.resume_id.clone(),
                resumable: true,
                current: None,
                held: HeldSlot::new(),
                spawned_at: now,
                ready_at: None,
                phase_since: now,
                last_turn_start_at: None,
                last_turn_end: None,
                last_frame_at: now,
                last_frame_published: 0,
                stopped: None,
                sent: VecDeque::new(),
                turns_started: 0,
                usage: UsageMeter::new(),
                usage_flush_scheduled: false,
            }),
            spec,
            deps,
            events: tx,
        });
        inner.publish_gilde();
        inner.publish_meta();
        inner.publish_stream();
        let weak = Arc::downgrade(&inner);
        let task = tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                let Some(inner) = weak.upgrade() else { return };
                match msg {
                    Msg::Event { turn_id, seq, event } => inner.on_event(&turn_id, seq, event).await,
                }
            }
        });
        TurnDriver { inner, task }
    }

    /// The held message (`Session.queued` for a turn-based session).
    pub fn held_message(&self) -> Option<QueuedMessage> {
        self.inner.state.lock().held.peek()
    }

    /// The vendor conversation id, once known.
    pub fn conversation_id(&self) -> Option<String> {
        self.inner.state.lock().conversation_id.clone()
    }

    /// Whether a turn is running.
    pub fn is_running(&self) -> bool {
        self.inner.state.lock().current.is_some()
    }

    /// Whether the session can be resumed later (a turn ended with an id).
    pub fn resumable(&self) -> bool {
        let state = self.inner.state.lock();
        state.resumable && state.conversation_id.as_deref().is_some_and(|c| !c.is_empty())
    }

    /// The model of the next turn.
    pub fn model(&self) -> Option<String> {
        self.inner.state.lock().model.clone()
    }
}

impl Inner {
    fn now(&self) -> EpochMs {
        (self.deps.clock)()
    }

    fn hint(&self) -> IdentityHint {
        let mut hint = self.spec.hint.clone();
        hint.vendor = Some(self.spec.vendor);
        if hint.account_id.is_none() {
            hint.account_id = Some(self.spec.account_id.clone());
        }
        if let Some(id) = self.state.lock().conversation_id.clone().filter(|c| !c.is_empty()) {
            hint.vendor_session_id = Some(id);
        }
        hint
    }

    fn send_facts(&self, facts: SourceFacts) {
        let update = EvidenceUpdate {
            hint: self.hint(),
            facts,
            at: self.now(),
        };
        self.deps.evidence.evidence(update);
    }

    fn publish_gilde(&self) {
        let facts = {
            let state = self.state.lock();
            GildeFacts {
                spawned_at: state.spawned_at,
                mode: ControlMode::TurnBased,
                ready_at: state.ready_at,
                stop_cause: state.stopped,
                signals_sent: Vec::new(),
                start_failure: None,
            }
        };
        self.send_facts(SourceFacts::Gilde(facts));
    }

    fn publish_meta(&self) {
        let model = self.state.lock().model.clone();
        let meta = SessionMetaFacts {
            cwd: Some(self.spec.cwd.clone()),
            model,
            permission_mode: self.spec.permission_mode,
            ..SessionMetaFacts::default()
        };
        self.send_facts(SourceFacts::Meta(Box::new(meta)));
    }

    fn stream_facts(state: &TurnState) -> StreamFacts {
        StreamFacts {
            phase: if state.current.is_some() {
                TurnPhase::Running
            } else {
                TurnPhase::Idle
            },
            phase_since: state.phase_since,
            pending: Vec::new(),
            last_turn_start_at: state.last_turn_start_at,
            last_turn_end: state.last_turn_end.clone(),
            last_frame_at: state.last_frame_at,
        }
    }

    fn publish_stream(&self) {
        let facts = {
            let mut state = self.state.lock();
            state.last_frame_published = state.last_frame_at;
            Self::stream_facts(&state)
        };
        self.send_facts(SourceFacts::Stream(facts));
    }

    /// Send today's spend when it changed (A10.4), at most once a second; a
    /// change inside the second is sent when the second is over.
    fn publish_usage(self: &Arc<Self>) {
        let now = self.now();
        let (report, wait) = {
            let mut state = self.state.lock();
            let report = state.usage.due(now);
            let wait = if report.is_none() && !state.usage_flush_scheduled {
                state.usage.wait_ms(now)
            } else {
                None
            };
            if wait.is_some() {
                state.usage_flush_scheduled = true;
            }
            (report, wait)
        };
        if let Some(today) = report {
            self.send_facts(SourceFacts::Usage(UsageFacts {
                source: UsageSource::Turns,
                today,
            }));
        }
        if let Some(ms) = wait {
            let weak = Arc::downgrade(self);
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(ms.max(1))).await;
                if let Some(inner) = weak.upgrade() {
                    inner.state.lock().usage_flush_scheduled = false;
                    inner.publish_usage();
                }
            });
        }
    }

    fn publish_content(&self, updates: Vec<ContentUpdate>) {
        for update in updates {
            self.deps.content.publish(&self.spec.uid, update);
        }
    }

    /// Claim the next turn under the state lock: the check and the claim are one
    /// step, so a message arriving meanwhile is held, never started beside it.
    fn reserve(
        &self,
        state: &mut TurnState,
        text: &str,
        client_msg_id: Option<ClientMsgId>,
        now: EpochMs,
    ) -> Result<Launch, IpcError> {
        if state.stopped.is_some() {
            return Err(IpcError::unsupported("send"));
        }
        if state.current.is_some() {
            return Err(IpcError::conflict("turnRunning"));
        }
        let resume_id = if state.resumable {
            state.conversation_id.clone().filter(|c| !c.is_empty())
        } else {
            None
        };
        let turn_id = new_turn_id();
        let mut renderer = self.deps.renderer.for_turn(&turn_id);
        let opening = renderer.user_prompt(text, client_msg_id, now);
        let model = state.model.clone();
        // R2 C6: a codex resume needs a model — the chip's value, else the
        // account's default from the vendor's own list (looked up at launch).
        let needs_model = self.spec.vendor == Vendor::Codex && resume_id.is_some() && model.is_none();
        state.current = Some(RunningTurn {
            turn_id: turn_id.clone(),
            resume_id: resume_id.clone(),
            saw_session: false,
            renderer,
            opening: Some(opening),
        });
        state.turns_started += 1;
        state.phase_since = now;
        state.last_turn_start_at = Some(now);
        state.last_frame_at = now;
        Ok(Launch {
            needs_model,
            params: TurnStartParams {
                turn_id,
                session_uid: self.spec.uid.clone(),
                vendor: self.spec.vendor,
                account_id: self.spec.account_id.clone(),
                cwd: self.spec.cwd.clone(),
                model,
                permission_mode: self.spec.permission_mode,
                prompt: text.to_string(),
                resume_id,
                timeout_ms: self.spec.timeout_ms,
            },
        })
    }

    /// Start a reserved turn in the sidecar. On failure the reservation is
    /// released and the error returned (the caller decides about the text).
    async fn launch(self: &Arc<Self>, mut launch: Launch) -> Result<(), IpcError> {
        let turn_id = launch.params.turn_id.clone();
        let started = async {
            if launch.needs_model {
                let models = self
                    .deps
                    .sidecar
                    .models(ProbeParams {
                        vendor: self.spec.vendor,
                        account_id: self.spec.account_id.clone(),
                    })
                    .await?;
                launch.params.model = models
                    .iter()
                    .find(|m| m.is_default)
                    .or_else(|| models.first())
                    .map(|m| m.id.clone());
                if launch.params.model.is_none() {
                    return Err(IpcError::invalid_argument("model", "required"));
                }
            }
            let sink: Arc<dyn TurnEventSink> = Arc::new(RouteSink {
                turn_id: turn_id.clone(),
                tx: self.events.clone(),
            });
            self.deps.sidecar.turn_start(launch.params, sink).await
        }
        .await;
        match started {
            Ok(()) => {
                {
                    // The user's message opens the turn, before any of its events
                    // (whichever of the two gets here first publishes it).
                    let mut state = self.state.lock();
                    if let Some(current) = state.current.as_mut().filter(|c| c.turn_id == turn_id) {
                        if let Some(opening) = current.opening.take() {
                            self.publish_content(opening);
                        }
                    }
                }
                self.publish_stream();
                Ok(())
            }
            Err(err) => {
                let now = self.now();
                {
                    let mut state = self.state.lock();
                    if state.current.as_ref().is_some_and(|c| c.turn_id == turn_id) {
                        state.current = None;
                        state.phase_since = now;
                    }
                }
                self.publish_stream();
                Err(err)
            }
        }
    }

    /// Start one turn with `text`. On success the turn is running; on failure
    /// nothing runs and the error is returned (the caller decides about `text`).
    async fn start_turn(self: &Arc<Self>, text: &str, client_msg_id: Option<ClientMsgId>) -> Result<(), IpcError> {
        let now = self.now();
        let launch = {
            let mut state = self.state.lock();
            self.reserve(&mut state, text, client_msg_id, now)?
        };
        self.launch(launch).await
    }

    async fn on_event(self: &Arc<Self>, turn_id: &str, _seq: u64, event: TurnEvent) {
        let now = self.now();
        let day = (self.deps.day)(now);
        let mut identity_changed = false;
        let mut ended = false;
        let mut next: Option<(HeldMessage, Launch)> = None;
        let mut frame_due = false;
        {
            let mut state = self.state.lock();
            let state = &mut *state;
            let Some(current) = state.current.as_mut() else { return };
            if current.turn_id != turn_id {
                return; // a stale turn (already replaced): ignore
            }
            // Content is published under the lock: the opening first, then the
            // turn's blocks in event order.
            if let Some(opening) = current.opening.take() {
                self.publish_content(opening);
            }
            let content = current.renderer.on_event(&event, now);
            self.publish_content(content);
            state.last_frame_at = now;
            match &event {
                TurnEvent::Session { conversation_id } => {
                    current.saw_session = true;
                    if !conversation_id.is_empty() {
                        if let Some(asked) = &current.resume_id {
                            if asked != conversation_id {
                                // §6.3: this turn started a NEW conversation; World
                                // attaches the new id to a new uid. Later turns
                                // continue the new one.
                                tracing::info!(target: "turns", "a resumed turn reported a different conversation");
                            }
                        }
                        if state.conversation_id.as_deref() != Some(conversation_id.as_str()) {
                            state.conversation_id = Some(conversation_id.clone());
                            identity_changed = true;
                        }
                        state.resumable = true;
                    }
                    if state.ready_at.is_none() {
                        state.ready_at = Some(now);
                        identity_changed = true;
                    }
                }
                TurnEvent::Done {
                    status,
                    conversation_id,
                    usage,
                    cost_usd,
                } => {
                    state.usage.observe(&day, turn_id, usage.as_ref(), *cost_usd);
                    if conversation_id.is_empty() {
                        if !current.saw_session {
                            // `""` = no resumable id (§6.3): the next turn starts a
                            // new conversation; Continue offers no Resume.
                            state.resumable = false;
                        }
                    } else if state.conversation_id.as_deref() != Some(conversation_id.as_str()) {
                        state.conversation_id = Some(conversation_id.clone());
                        state.resumable = true;
                        identity_changed = true;
                    }
                    if state.ready_at.is_none() {
                        state.ready_at = Some(now);
                        identity_changed = true;
                    }
                    let outcome = match status {
                        TurnDoneStatus::Stopped => TurnOutcome::Interrupted,
                        TurnDoneStatus::Ok | TurnDoneStatus::Incomplete => TurnOutcome::Ok,
                    };
                    state.last_turn_end = Some(TurnEnd { at: now, outcome });
                    state.current = None;
                    state.phase_since = now;
                    ended = true;
                    // The held message starts the next turn, claimed in this same
                    // step so a message sent meanwhile is held behind it.
                    if state.stopped.is_none() {
                        if let Some(held) = state.held.take() {
                            match self.reserve(state, &held.text, Some(held.client_msg_id.clone()), now) {
                                Ok(launch) => next = Some((held, launch)),
                                Err(_) => state.held.restore(held),
                            }
                        }
                    }
                }
                TurnEvent::Error { class, message } => {
                    let sub = match class {
                        VendorErrorClass::Auth => ErrorSub::SignInNeeded,
                        VendorErrorClass::Quota => ErrorSub::UsageLimit {
                            account_id: Some(self.spec.account_id.clone()),
                            resets_at: None,
                        },
                        VendorErrorClass::Exec | VendorErrorClass::Unknown => {
                            if message != HELPER_RESTARTED && !current.saw_session && state.ready_at.is_none() {
                                ErrorSub::DidntStart {
                                    reason: crate::contract::activity::DidntStartReason::Exited,
                                }
                            } else {
                                ErrorSub::AgentError
                            }
                        }
                    };
                    state.last_turn_end = Some(TurnEnd {
                        at: now,
                        outcome: TurnOutcome::Error(sub),
                    });
                    state.current = None;
                    state.phase_since = now;
                    ended = true;
                    // On Error the held message stays held (§6.3).
                }
                TurnEvent::Usage { usage } => {
                    state.usage.observe(&day, turn_id, Some(usage), None);
                    frame_due = now - state.last_frame_published >= FRAME_FACTS_EVERY_MS;
                }
                TurnEvent::Text { .. } | TurnEvent::Tool { .. } => {
                    frame_due = now - state.last_frame_published >= FRAME_FACTS_EVERY_MS;
                }
            }
        }
        self.publish_usage();
        if identity_changed {
            self.publish_gilde();
        }
        if ended || identity_changed || frame_due {
            self.publish_stream();
        }
        if let Some((held, launch)) = next {
            if let Err(err) = self.launch(launch).await {
                tracing::warn!(target: "turns", code = err.code.as_str(), "the held message could not start its turn; it stays held");
                let now = self.now();
                {
                    let mut state = self.state.lock();
                    // A message sent while it launched took the slot: that one stays
                    // held; this one's failure is the turn's error.
                    state.held.restore(held);
                    state.last_turn_end = Some(TurnEnd {
                        at: now,
                        outcome: TurnOutcome::Error(ErrorSub::AgentError),
                    });
                }
                self.publish_stream();
            }
        }
    }

    fn remember(&self, id: &ClientMsgId, delivery: SendDelivery) {
        let mut state = self.state.lock();
        state.sent.push_back((id.clone(), delivery));
        while state.sent.len() > SENT_IDS_KEPT {
            state.sent.pop_front();
        }
    }
}

#[async_trait]
impl SessionDriver for TurnDriver {
    fn mode(&self) -> ControlMode {
        ControlMode::TurnBased
    }

    /// A new turn, or the one held message while a turn runs (§6.3).
    /// Idempotent by `clientMsgId`: a repeat returns the first outcome.
    async fn send(&mut self, msg: OutgoingMessage) -> Result<SendDelivery, IpcError> {
        if !msg.images.is_empty() {
            // The helper's turn has no image input yet.
            return Err(IpcError::unsupported("images"));
        }
        let inner = &self.inner;
        {
            let mut state = inner.state.lock();
            if let Some((_, first)) = state.sent.iter().find(|(id, _)| id == &msg.client_msg_id) {
                return Ok(*first);
            }
            if state.stopped.is_some() {
                return Err(IpcError::unsupported("send"));
            }
            if state.current.is_some() {
                let held = HeldMessage {
                    client_msg_id: msg.client_msg_id.clone(),
                    text: msg.text.clone(),
                    held_at: inner.now(),
                };
                state.held.hold(held)?;
                drop(state);
                inner.remember(&msg.client_msg_id, SendDelivery::Queued);
                return Ok(SendDelivery::Queued);
            }
        }
        inner.start_turn(&msg.text, Some(msg.client_msg_id.clone())).await?;
        inner.remember(&msg.client_msg_id, SendDelivery::Sent);
        Ok(SendDelivery::Sent)
    }

    /// Drops the held message.
    async fn cancel_queued(&mut self, id: &ClientMsgId) -> Result<bool, IpcError> {
        self.inner.state.lock().held.cancel(id)
    }

    /// Turn-based sessions have no approval cards (the permission mode was
    /// chosen at start, §6.3).
    async fn answer(&mut self, _req: &RequestId, _d: Decision) -> Result<AnswerOutcome, IpcError> {
        Err(IpcError::unsupported("answer"))
    }

    async fn screen_key(&mut self, _req: &RequestId, _rev: &str, _key: ScreenPromptAction) -> Result<ScreenKeyOutcome, IpcError> {
        Err(IpcError::unsupported("screenKeys"))
    }

    /// `turn.cancel` (SIGTERM of the turn's group); outcome `stopped`. The
    /// session stays held and resumable; `discard_queued` drops the held message.
    async fn interrupt(&mut self, discard_queued: bool) -> Result<InterruptOutcome, IpcError> {
        let running = {
            let mut state = self.inner.state.lock();
            if discard_queued {
                state.held.clear();
            }
            state.current.as_ref().map(|c| c.turn_id.clone())
        };
        if let Some(turn_id) = running {
            if let Err(err) = self.inner.deps.sidecar.turn_cancel(&turn_id).await {
                // The helper is down: its turns already ended with
                // `turn.helperRestarted` and the sweep stops their children.
                tracing::warn!(target: "turns", code = err.code.as_str(), "turn.cancel failed");
            }
        }
        Ok(InterruptOutcome::Stopped)
    }

    /// Conversation only (§4.2.3).
    async fn force_stop(&mut self) -> Result<bool, IpcError> {
        Err(IpcError::unsupported("forceStop"))
    }

    /// The next turn's `--model` (§6.3).
    async fn set_model(&mut self, m: Option<String>) -> Result<AppliesFrom, IpcError> {
        self.inner.state.lock().model = m.filter(|s| !s.is_empty());
        self.inner.publish_meta();
        Ok(AppliesFrom::NextTurn)
    }

    /// Fixed at start (ux-ia §2.2).
    async fn set_permission_mode(&mut self, _m: PermissionMode) -> Result<(), IpcError> {
        Err(IpcError::unsupported("setPermissionMode"))
    }

    /// Session stop / quit: cancel the running turn, drop the held message and
    /// release the session (Stopped; Continue re-holds it).
    async fn stop(&mut self, why: StopCause) -> Result<StopOutcome, IpcError> {
        let running = {
            let mut state = self.inner.state.lock();
            state.held.clear();
            state.stopped = Some(why);
            state.current.as_ref().map(|c| c.turn_id.clone())
        };
        if let Some(turn_id) = running {
            if let Err(err) = self.inner.deps.sidecar.turn_cancel(&turn_id).await {
                tracing::warn!(target: "turns", code = err.code.as_str(), "turn.cancel failed during stop");
            }
        }
        let now = self.inner.now();
        self.inner.publish_gilde();
        self.inner.send_facts(SourceFacts::Process(ProcessFacts::Gone { at: now }));
        Ok(StopOutcome {
            stopped: true,
            survivors: 0,
        })
    }
}

#[cfg(test)]
mod usage_tests {
    use super::*;

    fn u(input: u64, output: u64) -> TurnUsage {
        TurnUsage {
            input_tokens: Some(input),
            output_tokens: Some(output),
            cached_input_tokens: None,
        }
    }

    #[test]
    fn nothing_reported_is_absent_never_zero() {
        let mut meter = UsageMeter::new();
        assert_eq!(meter.total(), None);
        meter.observe("2026-10-22", "tn_1", None, None);
        assert_eq!(meter.total(), None, "a turn without usage reports nothing");
        assert_eq!(meter.due(1_000), None);
        meter.observe("2026-10-22", "tn_1", None, Some(f64::NAN));
        assert_eq!(meter.total(), None, "a meaningless cost is not a cost");
    }

    #[test]
    fn each_turn_counts_once_and_the_report_is_a_running_total() {
        let mut meter = UsageMeter::new();
        meter.observe("2026-10-22", "tn_1", Some(&u(100, 10)), None);
        // The same turn again (a progress report, then its Done, or a replay).
        meter.observe("2026-10-22", "tn_1", Some(&u(100, 10)), None);
        meter.observe("2026-10-22", "tn_1", Some(&u(120, 15)), Some(0.0125));
        let total = meter.total().expect("reported");
        assert_eq!((total.input_tokens, total.output_tokens), (Some(120), Some(15)));
        assert_eq!(total.cost_micros, Some(12_500));
        assert_eq!(total.cached_tokens, None, "never reported: unknown");

        meter.observe("2026-10-22", "tn_2", Some(&u(30, 3)), None);
        let total = meter.total().expect("reported");
        assert_eq!((total.input_tokens, total.output_tokens), (Some(150), Some(18)));
        assert_eq!(total.cost_micros, Some(12_500), "only the vendor's own cost");
        assert_eq!(total.day, "2026-10-22");
    }

    #[test]
    fn a_new_local_day_starts_a_new_total() {
        let mut meter = UsageMeter::new();
        meter.observe("2026-10-22", "tn_1", Some(&u(500, 50)), Some(1.0));
        meter.observe("2026-10-23", "tn_2", Some(&u(7, 1)), None);
        let total = meter.total().expect("reported");
        assert_eq!(total.day, "2026-10-23");
        assert_eq!((total.input_tokens, total.output_tokens, total.cost_micros), (Some(7), Some(1), None));
        // A day without usage yet reports nothing.
        meter.observe("2026-10-24", "tn_3", None, None);
        assert_eq!(meter.total(), None);
    }

    #[test]
    fn reports_are_sent_on_change_at_most_once_a_second() {
        let mut meter = UsageMeter::new();
        meter.observe("2026-10-22", "tn_1", Some(&u(1, 1)), None);
        assert!(meter.due(10_000).is_some(), "the first report goes at once");
        assert_eq!(meter.due(10_100), None, "unchanged: nothing to send");
        assert_eq!(meter.wait_ms(10_100), None);

        meter.observe("2026-10-22", "tn_1", Some(&u(2, 2)), None);
        assert_eq!(meter.due(10_400), None, "changed inside the second: held back");
        assert_eq!(meter.wait_ms(10_400), Some(600));
        let later = meter.due(11_000).expect("sent when the second is over");
        assert_eq!(later.input_tokens, Some(2));
        assert_eq!(meter.wait_ms(11_000), None);
    }

    #[test]
    fn the_local_day_has_the_calendar_shape() {
        let day = local_day(1_800_000_000_000);
        assert_eq!(day.len(), 10);
        assert!(day.starts_with("2027-01-1"), "{day}");
        assert_eq!(&day[4..5], "-");
    }
}
