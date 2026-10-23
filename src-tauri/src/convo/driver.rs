//! `ConvoDriver: SessionDriver` (contract §6.0, §6.1) — the L1 transport of
//! a Conversation session.
//!
//! Owner: T05. Consumed by T10 (the session actor owns one driver per
//! session and calls it in arrival order).
//!
//! **Spawn** ([`ConvoDriver::spawn`]): the spawn gate, the compat record of
//! the binary actually spawned (`CompatApi::for_binary` with the spawn-time
//! `BinaryRef`, A1), the provisional flags of that version (read once, never
//! re-derived for this process, §3.10), the exact argv of §6.1.1, a durable
//! `planned` row through `SupervisorApi::plan_spawn` (kind `Convo`,
//! `generation`/`turn_id` = `None`, A12.8/A13) before the process exists, the
//! composed env (T10's composer, stamped with the ticket's `GILDE_SPAWN_ID`)
//! plus `CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS=1` and
//! `CLAUDE_CODE_ENTRYPOINT=gilde` — never `DISABLE_AUTOUPDATER` (§6.0) — its
//! own process group, piped stdio, `mark_running`.
//!
//! **Pipes**: stdout through a line splitter (64 MiB cap, skip without
//! buffering); stderr into a 64 KiB ring (diagnostics only, never parsed as
//! frames, never persisted); stdin through a bounded queue (64 frames /
//! 8 MiB) whose overflow is `BUSY{stdinFull}`. Stdin stays open for the
//! session's whole life; only the stop sequence closes it.
//!
//! **The pump** (one task per session) owns the [`Core`] and serializes every
//! stdout line, command, exit and deadline. Effects go to World
//! (`EvidenceSink`, with the identity hint), the ContentHub, `CompatSink` and
//! the [`ConvoHooks`].
//!
//! **Stop** (§6.1.7) [C5]: `interrupt` → that turn's `result` (≤ 5 s; ≤ 3 s at
//! quit) → close stdin → ≤ 1.5 s for exit → fenced SIGTERM to the group via
//! `SupervisorApi::signal` → ≤ 2 s (1.5 s at quit); survivors are reported,
//! never SIGKILLed. `turn_force_stop` = fenced SIGTERM only after 5 s without a
//! `result`. Gilde never signals anything itself: every signal goes through the
//! supervisor's identity fence.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStderr, ChildStdin, ChildStdout};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::contract::account::Quota;
use crate::contract::activity::{ExitInfo, ProcessFacts, StopCause};
use crate::contract::common::{AbsPath, CompatState, EpochMs, Notice, PermissionMode, Vendor};
use crate::contract::error::{IpcError, VendorErrorClass};
use crate::contract::ids::{AccountId, ClientMsgId, ProcessKey, RequestId, SessionUid};
use crate::contract::interaction::{Decision, DecisionKind};
use crate::contract::ipc::{
    AnswerOutcome, AppliesFrom, InteractionDetailRes, InterruptOutcome, ModelOption, ScreenKeyOutcome, ScreenPromptAction,
    SendDelivery, SessionTechnicalRes,
};
use crate::contract::process::ProcKind;
use crate::contract::services::{
    CompatApi, CompatSink, ContentHubApi, EvidenceSink, EvidenceUpdate, IdentityHint, OutgoingMessage, SessionDriver,
    SignalOutcome, SignalPlan, SourceFacts, SpawnRequest, SpawnTicket, StopOutcome, SupervisorApi,
};
use crate::contract::session::{BinaryRef, ControlMode, QueuedMessage};
use crate::convo::frames::{
    AbandonedAsk, CancelPlan, CompatEvent, Core, CoreConfig, Counters, Effect, FrameClass, HookEvent, Now, Outbox,
    PatternSource, WriteError,
};
use crate::convo::handshake::{self, ArgvSpec, LaunchKind};
use crate::convo::ledger::GildeReply;
use crate::convo::wire::{LineEvent, LineSplitter, QUEUE_BYTES, QUEUE_FRAMES};
use crate::convo::{guard, Clock, Flags, SystemClock};
use crate::render::sanitize::sanitize_display_bytes;

/// The stderr ring (§6.1.1).
pub const STDERR_RING: usize = 64 * 1024;
/// Stdout lines buffered between the reader and the pump.
const LINE_QUEUE: usize = 256;

// ---------------------------------------------------------------------------
// what T10 passes in
// ---------------------------------------------------------------------------

/// Composes the child's environment (§6.0 *Composed environment*) for a
/// `GILDE_SPAWN_ID` marker — in the app `Supervisor::compose_env(base,
/// account, marker)`. The driver adds only the Conversation variables.
pub type EnvComposer = Arc<dyn Fn(&str) -> BTreeMap<String, String> + Send + Sync>;

/// One Conversation process to start (new, continue, copy, other account).
#[derive(Clone)]
pub struct ConvoSpec {
    pub uid: SessionUid,
    /// How World addresses this session (T10's hint); the driver adds the
    /// vendor, the vendor session id once known, the process key and the
    /// spawn id.
    pub hint: IdentityHint,
    pub account_id: Option<AccountId>,
    /// The account's config dir (the settings-file guard `stat`s
    /// `<cfg>/settings.json`, never opens it); `None` = `<home>/.claude`.
    pub config_dir: Option<AbsPath>,
    /// Resolved at spawn by T10 (§6.0 *Binary resolution*); argv[0] = its realpath.
    pub binary: BinaryRef,
    pub cwd: AbsPath,
    pub home: Option<AbsPath>,
    pub launch: LaunchKind,
    pub permission_mode: PermissionMode,
    pub model: Option<String>,
    /// The user chose `bypassPermissions` at start (New-session sheet).
    pub allow_bypass: bool,
    /// Written once `initialize` succeeded.
    pub first_prompt: Option<OutgoingMessage>,
    /// A resume of a session that was waiting [F-05].
    pub abandoned: Option<AbandonedAsk>,
    pub env: EnvComposer,
}

impl std::fmt::Debug for ConvoSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConvoSpec").field("uid", &self.uid).field("launch", &self.launch).field("mode", &self.permission_mode).finish_non_exhaustive()
    }
}

/// What the session manager and the accounts service learn from a
/// Conversation session that has no `SourceFacts` path [A19.1]: **T10**
/// implements it — `queued` → `World::set_queued` (A18.3);
/// `approval_unconfirmed`, `version_note` and `notice` (the
/// `convo.notReading` / `convo.unanswerable` lines, as `EvidenceLine
/// {key, params}`) → `World::set_details_extra(uid, DetailsExtra{…})`, a full
/// replacement each call, so T10 keeps the latest of each per session;
/// `quota` / `signed_out` → the accounts service (T08).
pub trait ConvoHooks: Send + Sync {
    /// `Session.queued` changed (messages sent while a turn runs).
    fn queued(&self, uid: &SessionUid, queued: Vec<QueuedMessage>) {
        let _ = (uid, queued);
    }
    /// The account's quota from `rate_limit_event` / `get_usage` (no probe).
    fn quota(&self, account: &AccountId, quota: Quota) {
        let _ = (account, quota);
    }
    /// `initialize.account.tokenSource == "none"`: `Account.sign_in = SignedOut` [F-29].
    fn signed_out(&self, account: &AccountId) {
        let _ = account;
    }
    /// §6.1.11 `approval_channel_unconfirmed` for the session's Details.
    fn approval_unconfirmed(&self, uid: &SessionUid, tool: &str) {
        let _ = (uid, tool);
    }
    /// A session line (`convo.notReading`, `convo.unanswerable`).
    fn notice(&self, uid: &SessionUid, notice: Notice) {
        let _ = (uid, notice);
    }
    /// `SessionDetails.version_note`: the CLI reports another version than
    /// the one resolved at spawn [A1] (the session keeps the spawn-time flags).
    fn version_note(&self, uid: &SessionUid, note: &str) {
        let _ = (uid, note);
    }
    /// A35.1: the CLI reported a permission mode (`None` = one Gilde does
    /// not know, A33.3): the session's mode, never the previous one.
    fn permission_mode(&self, uid: &SessionUid, mode: Option<PermissionMode>) {
        let _ = (uid, mode);
    }
}

/// Hooks that do nothing.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoHooks;
impl ConvoHooks for NoHooks {}

/// The bounds of §6.1.2, §6.1.7 and §10.2 (tests shorten them; no wall-clock
/// budget is asserted, A10.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvoTiming {
    pub interrupt_wait_ms: u64,
    pub quit_interrupt_wait_ms: u64,
    pub eof_exit_wait_ms: u64,
    pub term_grace_ms: u32,
    pub quit_term_grace_ms: u32,
    /// After an `initialize` compat failure: close stdin, then SIGTERM after this.
    pub compat_stop_wait_ms: u64,
    /// `set_model` / `set_permission_mode` / `get_usage` reply wait.
    pub reply_wait_ms: u64,
    pub cancel_wait_ms: u64,
    /// After exit, stdout lines still buffered are read for at most this long.
    pub drain_ms: u64,
    pub line_cap: usize,
    /// §6.1.2 step 6 (30 s; ~13 s observed with the sandbox on).
    pub initialize_timeout_ms: u64,
}

impl Default for ConvoTiming {
    fn default() -> ConvoTiming {
        ConvoTiming {
            interrupt_wait_ms: 5_000,
            quit_interrupt_wait_ms: 3_000,
            eof_exit_wait_ms: 1_500,
            term_grace_ms: 2_000,
            quit_term_grace_ms: 1_500,
            compat_stop_wait_ms: 1_500,
            reply_wait_ms: 5_000,
            cancel_wait_ms: 2_000,
            drain_ms: 500,
            line_cap: crate::convo::wire::LINE_CAP,
            initialize_timeout_ms: handshake::INITIALIZE_TIMEOUT_MS,
        }
    }
}

/// The services a driver talks to.
#[derive(Clone)]
pub struct ConvoDeps {
    pub supervisor: Arc<dyn SupervisorApi>,
    pub compat: Arc<dyn CompatApi>,
    pub compat_sink: Arc<dyn CompatSink>,
    pub evidence: Arc<dyn EvidenceSink>,
    pub content: Arc<dyn ContentHubApi>,
    pub hooks: Arc<dyn ConvoHooks>,
    pub clock: Arc<dyn Clock>,
    pub patterns: Arc<PatternSource>,
    pub timing: ConvoTiming,
}

impl ConvoDeps {
    /// The real clock, the bundled error patterns, default bounds, no hooks.
    pub fn new(
        supervisor: Arc<dyn SupervisorApi>,
        compat: Arc<dyn CompatApi>,
        compat_sink: Arc<dyn CompatSink>,
        evidence: Arc<dyn EvidenceSink>,
        content: Arc<dyn ContentHubApi>,
    ) -> ConvoDeps {
        ConvoDeps {
            supervisor,
            compat,
            compat_sink,
            evidence,
            content,
            hooks: Arc::new(NoHooks),
            clock: Arc::new(SystemClock),
            patterns: PatternSource::bundled(),
            timing: ConvoTiming::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// stdin queue
// ---------------------------------------------------------------------------

/// One queued stdin frame; a tagged frame is a decision for that card.
struct QueuedFrame {
    tag: Option<String>,
    bytes: Vec<u8>,
}

/// Card ids whose queued answer must not be written (withdrawn meanwhile).
type Withdrawn = Arc<Mutex<std::collections::HashSet<String>>>;

struct StdinQueue {
    tx: mpsc::Sender<QueuedFrame>,
    bytes: Arc<AtomicUsize>,
    closed: Arc<AtomicBool>,
    close: Option<oneshot::Sender<()>>,
    withdrawn: Withdrawn,
}

impl StdinQueue {
    fn close(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
        if let Some(c) = self.close.take() {
            let _ = c.send(());
        }
    }
}

impl StdinQueue {
    fn push(&mut self, line: String, class: FrameClass, tag: Option<&str>) -> Result<(), WriteError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(WriteError::Closed);
        }
        if line.len() > class.cap() {
            return Err(WriteError::TooLarge);
        }
        let mut buf = line.into_bytes();
        buf.push(b'\n');
        let n = buf.len();
        let queued = self.bytes.load(Ordering::SeqCst);
        if queued > 0 && queued + n > QUEUE_BYTES {
            return Err(WriteError::Full);
        }
        self.bytes.fetch_add(n, Ordering::SeqCst);
        if let Some(t) = tag {
            self.withdrawn.lock().remove(t);
        }
        match self.tx.try_send(QueuedFrame { tag: tag.map(str::to_owned), bytes: buf }) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.bytes.fetch_sub(n, Ordering::SeqCst);
                Err(WriteError::Full)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.bytes.fetch_sub(n, Ordering::SeqCst);
                self.closed.store(true, Ordering::SeqCst);
                Err(WriteError::Closed)
            }
        }
    }
}

impl Outbox for StdinQueue {
    fn try_write(&mut self, line: String, class: FrameClass) -> Result<(), WriteError> {
        self.push(line, class, None)
    }

    fn try_write_tagged(&mut self, line: String, class: FrameClass, tag: &str) -> Result<(), WriteError> {
        self.push(line, class, Some(tag))
    }

    fn withdraw(&mut self, tag: &str) {
        let mut w = self.withdrawn.lock();
        if w.len() < 4_096 {
            w.insert(tag.to_owned());
        }
    }
}

async fn write_stdin(
    mut stdin: ChildStdin,
    mut rx: mpsc::Receiver<QueuedFrame>,
    bytes: Arc<AtomicUsize>,
    mut close: oneshot::Receiver<()>,
    broken: mpsc::UnboundedSender<()>,
    withdrawn: Withdrawn,
) {
    loop {
        tokio::select! {
            biased;
            _ = &mut close => return,
            f = rx.recv() => {
                let Some(QueuedFrame { tag, bytes: buf }) = f else { return };
                let n = buf.len();
                // A decision whose card was withdrawn while it waited here is
                // dropped (§6.1.4 Withdrawal).
                if tag.is_some_and(|t| withdrawn.lock().remove(&t)) {
                    bytes.fetch_sub(n, Ordering::SeqCst);
                    continue;
                }
                let r = tokio::select! {
                    biased;
                    _ = &mut close => return,
                    r = stdin.write_all(&buf) => r,
                };
                bytes.fetch_sub(n, Ordering::SeqCst);
                if r.is_err() {
                    let _ = broken.send(());
                    return;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// stdout / stderr readers
// ---------------------------------------------------------------------------

enum ReadEvent {
    /// A line and its share of the byte budget (released when processed).
    Line(Vec<u8>, tokio::sync::OwnedSemaphorePermit),
    TooLarge,
    Eof,
}

/// Bytes of stdout lines waiting for the pump at most (one maximal line plus
/// slack): the reader waits beyond it, so a flood of huge lines never
/// accumulates in memory.
fn line_budget(cap: usize) -> usize {
    cap.saturating_add(1024 * 1024).min(u32::MAX as usize)
}

async fn read_stdout(mut out: ChildStdout, tx: mpsc::Sender<ReadEvent>, cap: usize) {
    let mut splitter = LineSplitter::new(cap);
    let budget = Arc::new(tokio::sync::Semaphore::new(line_budget(cap)));
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let (events, eof) = match out.read(&mut buf).await {
            Ok(0) | Err(_) => (splitter.finish().into_iter().collect::<Vec<_>>(), true),
            Ok(n) => (splitter.push(&buf[..n]), false),
        };
        for ev in events {
            let msg = match ev {
                LineEvent::Line(l) => {
                    let want = l.len().clamp(1, line_budget(cap)) as u32;
                    let Ok(permit) = budget.clone().acquire_many_owned(want).await else { return };
                    ReadEvent::Line(l, permit)
                }
                LineEvent::TooLarge => ReadEvent::TooLarge,
            };
            if tx.send(msg).await.is_err() {
                return;
            }
        }
        if eof {
            let _ = tx.send(ReadEvent::Eof).await;
            return;
        }
    }
}

/// The last 64 KiB of stderr (diagnostics only).
#[derive(Debug, Default)]
pub struct StderrRing {
    buf: VecDeque<u8>,
}

impl StderrRing {
    fn push(&mut self, bytes: &[u8]) {
        self.buf.extend(bytes);
        while self.buf.len() > STDERR_RING {
            self.buf.pop_front();
        }
    }

    /// The tail as display text (sanitized).
    pub fn text(&self) -> String {
        let (a, b) = self.buf.as_slices();
        let mut v = Vec::with_capacity(self.buf.len());
        v.extend_from_slice(a);
        v.extend_from_slice(b);
        sanitize_display_bytes(&v, crate::contract::display::TextCap::Bytes(STDERR_RING)).text
    }
}

async fn read_stderr(mut err: ChildStderr, ring: Arc<Mutex<StderrRing>>) {
    let mut buf = vec![0u8; 8 * 1024];
    loop {
        match err.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => ring.lock().push(&buf[..n]),
        }
    }
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

type Reply<T> = oneshot::Sender<Result<T, IpcError>>;

enum Cmd {
    Send(OutgoingMessage, Reply<SendDelivery>),
    CancelQueued(ClientMsgId, Reply<CancelPlan>),
    Answer(RequestId, Decision, Reply<(AnswerOutcome, Option<DecisionKind>)>),
    Interrupt(bool, Reply<InterruptOutcome>),
    ForceStop(Reply<bool>),
    SetModel(Option<String>, Reply<(oneshot::Receiver<GildeReply>, AppliesFrom)>),
    SetMode(PermissionMode, Reply<oneshot::Receiver<GildeReply>>),
    GetUsage(Reply<oneshot::Receiver<GildeReply>>),
    Stop(StopCause, Reply<StopOutcome>),
    Detail(RequestId, Reply<InteractionDetailRes>),
    Snapshot(oneshot::Sender<Snapshot>),
}

/// A read-only view for the inherent accessors.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub queued: Vec<QueuedMessage>,
    pub models: Vec<ModelOption>,
    pub model: Option<String>,
    pub ready: bool,
    pub exited: bool,
    pub vendor_session_id: Option<String>,
    pub allow_session: bool,
    pub counters: Counters,
    pub pending: Vec<crate::contract::interaction::PendingInteraction>,
}

// ---------------------------------------------------------------------------
// the pump
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopPhase {
    None,
    AwaitResult { until: Instant },
    AwaitExit { until: Instant },
    AwaitTerm,
}

struct Pump {
    core: Core,
    uid: SessionUid,
    hint: IdentityHint,
    account_id: Option<AccountId>,
    spawn_version: String,
    ticket: SpawnTicket,
    key: ProcessKey,
    argv: Vec<String>,
    deps: ConvoDeps,
    stdin: StdinQueue,
    stderr: Arc<Mutex<StderrRing>>,
    phase: StopPhase,
    quit: bool,
    force: bool,
    stop_waiters: Vec<Reply<StopOutcome>>,
    force_waiters: Vec<Reply<bool>>,
    survivor_reported: bool,
    exit: Option<ExitInfo>,
    exit_pending: Option<(ExitInfo, Instant)>,
    stdout_done: bool,
    sig_tx: mpsc::UnboundedSender<Result<SignalOutcome, IpcError>>,
}

impl Pump {
    fn now(&self) -> Now {
        Now { wall: self.deps.clock.wall(), mono: self.deps.clock.mono_ms() }
    }

    fn hint(&self) -> IdentityHint {
        let mut h = self.hint.clone();
        h.vendor = Some(Vendor::Claude);
        if h.account_id.is_none() {
            h.account_id = self.account_id.clone();
        }
        if let Some(sid) = self.core.vendor_session_id() {
            h.vendor_session_id = Some(sid.to_owned());
        }
        h.process = Some(self.key.clone());
        h.gilde_spawn_id = Some(self.ticket.spawn_id.clone());
        h
    }

    fn evidence(&self, facts: SourceFacts, at: EpochMs) {
        self.deps.evidence.evidence(EvidenceUpdate { hint: self.hint(), facts, at });
    }

    fn apply(&mut self, fx: Vec<Effect>) {
        let at = self.deps.clock.wall();
        for e in fx {
            match e {
                Effect::Evidence(f) => self.evidence(f, at),
                Effect::Content(u) => self.deps.content.publish(&self.uid, u),
                Effect::Compat(c) => match c {
                    CompatEvent::Handshake { ok, note } => self.deps.compat_sink.handshake(&self.spawn_version, ok, note),
                    CompatEvent::Deviation { capability, note } => {
                        self.deps.compat_sink.deviation(&self.core.effective_version(), capability, note)
                    }
                    CompatEvent::Note(n) => self.deps.compat_sink.note(&self.core.effective_version(), n),
                },
                Effect::Hook(h) => match h {
                    HookEvent::Quota(q) => {
                        if let Some(a) = &self.account_id {
                            self.deps.hooks.quota(a, q);
                        }
                    }
                    HookEvent::SignedOut => {
                        if let Some(a) = &self.account_id {
                            self.deps.hooks.signed_out(a);
                        }
                    }
                    HookEvent::Queued(q) => self.deps.hooks.queued(&self.uid, q),
                    HookEvent::ApprovalUnconfirmed(t) => self.deps.hooks.approval_unconfirmed(&self.uid, &t),
                    HookEvent::Notice(n) => self.deps.hooks.notice(&self.uid, n),
                    HookEvent::VersionNote(v) => self.deps.hooks.version_note(&self.uid, &v),
                    HookEvent::PermissionMode(m) => self.deps.hooks.permission_mode(&self.uid, m),
                },
                Effect::StopForCompat => {
                    // §6.1.2 step 3: close stdin (no turn runs yet), then fenced
                    // SIGTERM after 1.5 s. No stop cause: it stays Didn't start.
                    if self.exit.is_none() && self.phase == StopPhase::None {
                        self.stdin.close();
                        self.phase = StopPhase::AwaitExit { until: Instant::now() + Duration::from_millis(self.deps.timing.compat_stop_wait_ms) };
                    }
                }
                Effect::TurnEnded => {
                    if matches!(self.phase, StopPhase::AwaitResult { .. }) {
                        self.close_stdin_phase();
                    }
                }
            }
        }
    }

    fn close_stdin_phase(&mut self) {
        self.stdin.close();
        self.phase = StopPhase::AwaitExit { until: Instant::now() + Duration::from_millis(self.deps.timing.eof_exit_wait_ms) };
    }

    fn term_phase(&mut self) {
        if self.phase == StopPhase::AwaitTerm || self.exit.is_some() || self.exit_pending.is_some() {
            return;
        }
        self.phase = StopPhase::AwaitTerm;
        let grace = if self.quit { self.deps.timing.quit_term_grace_ms } else { self.deps.timing.term_grace_ms };
        let sup = self.deps.supervisor.clone();
        let key = self.key.clone();
        let tx = self.sig_tx.clone();
        tokio::spawn(async move {
            let r = sup.signal(&key, SignalPlan::Term { grace_ms: grace }).await;
            let _ = tx.send(r);
        });
        let now = self.now();
        let mut fx = Vec::new();
        self.core.signal_sent(libc::SIGTERM, now, &mut fx);
        self.apply(fx);
    }

    fn begin_stop(&mut self, cause: StopCause, reply: Reply<StopOutcome>) {
        if self.exit.is_some() {
            let _ = reply.send(Ok(StopOutcome { stopped: true, survivors: 0 }));
            return;
        }
        if self.survivor_reported {
            // The fenced SIGTERM already ran and the process survived it; it
            // is never escalated [C5]: report the survivor again at once.
            let _ = reply.send(Ok(StopOutcome { stopped: false, survivors: 1 }));
            return;
        }
        self.stop_waiters.push(reply);
        if cause == StopCause::Quit {
            self.quit = true;
        }
        if self.phase != StopPhase::None {
            return;
        }
        let now = self.now();
        let mut fx = Vec::new();
        let wait = self.core.stop_begin(cause, &mut self.stdin, now, &mut fx);
        self.apply(fx);
        if wait {
            let ms = if self.quit { self.deps.timing.quit_interrupt_wait_ms } else { self.deps.timing.interrupt_wait_ms };
            self.phase = StopPhase::AwaitResult { until: Instant::now() + Duration::from_millis(ms) };
        } else {
            self.close_stdin_phase();
        }
    }

    fn finish_stop(&mut self, outcome: StopOutcome) {
        for w in self.stop_waiters.drain(..) {
            let _ = w.send(Ok(outcome));
        }
        for w in self.force_waiters.drain(..) {
            let _ = w.send(Ok(outcome.stopped));
        }
    }

    fn deadline(&self) -> Option<Instant> {
        let base_mono = self.deps.clock.mono_ms();
        let core = self.core.next_deadline().map(|m| Instant::now() + Duration::from_millis(m.saturating_sub(base_mono)));
        let stop = match self.phase {
            StopPhase::AwaitResult { until } | StopPhase::AwaitExit { until } if self.exit.is_none() && self.exit_pending.is_none() => Some(until),
            _ => None,
        };
        let drain = self.exit_pending.map(|(_, t)| t);
        [core, stop, drain].into_iter().flatten().min()
    }

    fn on_deadline(&mut self) {
        let now = self.now();
        let mut fx = Vec::new();
        self.core.tick(&mut self.stdin, now, &mut fx);
        self.apply(fx);
        let t = Instant::now();
        if self.exit.is_none() && self.exit_pending.is_none() {
            match self.phase {
                StopPhase::AwaitResult { until } if t >= until => self.close_stdin_phase(),
                StopPhase::AwaitExit { until } if t >= until => self.term_phase(),
                _ => {}
            }
        }
        if let Some((exit, until)) = self.exit_pending {
            if t >= until {
                self.finalize_exit(exit);
            }
        }
    }

    fn finalize_exit(&mut self, exit: ExitInfo) {
        self.exit_pending = None;
        if self.exit.is_some() {
            return;
        }
        self.exit = Some(exit);
        self.phase = StopPhase::None;
        self.stdin.close();
        let now = self.now();
        let stderr = self.stderr.lock().text();
        let mut fx = Vec::new();
        self.core.on_exit(&self.argv, &stderr, now, &mut fx);
        self.apply(fx);
        self.evidence(ProcessFacts::Exited { at: now.wall, code: exit.code, signal: exit.signal }.into_source(), now.wall);
        let sup = self.deps.supervisor.clone();
        let ticket = self.ticket.clone();
        tokio::spawn(async move { sup.mark_exited(&ticket, exit).await });
        self.finish_stop(StopOutcome { stopped: true, survivors: 0 });
    }

    fn snapshot(&self) -> Snapshot {
        let (models, model) = self.core.models();
        Snapshot {
            queued: self.core.queued(),
            models,
            model,
            ready: self.core.is_ready(),
            exited: self.exit.is_some(),
            vendor_session_id: self.core.vendor_session_id().map(str::to_owned),
            allow_session: self.core.allow_session(),
            counters: self.core.counters(),
            pending: self.core.pending_cards(),
        }
    }

    fn on_cmd(&mut self, cmd: Cmd) {
        let now = self.now();
        let mut fx = Vec::new();
        match cmd {
            Cmd::Send(m, r) => {
                let res = self.core.send(m, &mut self.stdin, now, &mut fx);
                let _ = r.send(res);
            }
            Cmd::CancelQueued(id, r) => {
                let res = self.core.cancel_queued(&id, &mut self.stdin, &mut fx);
                let _ = r.send(res);
            }
            Cmd::Answer(req, d, r) => {
                let res = self.core.answer(&req, &d, &mut self.stdin, now, &mut fx);
                let _ = r.send(res);
            }
            Cmd::Interrupt(discard, r) => {
                let res = self.core.interrupt(discard, &mut self.stdin, now, &mut fx);
                let _ = r.send(res);
            }
            Cmd::ForceStop(r) => {
                if self.exit.is_some() || self.exit_pending.is_some() {
                    let _ = r.send(Ok(true));
                } else if self.survivor_reported {
                    let _ = r.send(Ok(false));
                } else if let Err(e) = self.core.force_stop_allowed(now) {
                    let _ = r.send(Err(e));
                } else {
                    self.force = true;
                    self.force_waiters.push(r);
                    self.core.stop_begin(StopCause::ForceStop, &mut self.stdin, now, &mut fx);
                    self.apply(std::mem::take(&mut fx));
                    self.term_phase();
                }
            }
            Cmd::SetModel(m, r) => {
                let _ = r.send(self.core.set_model(m, &mut self.stdin));
            }
            Cmd::SetMode(m, r) => {
                let _ = r.send(self.core.set_permission_mode(m, &mut self.stdin, now));
            }
            Cmd::GetUsage(r) => {
                let _ = r.send(self.core.get_usage(&mut self.stdin));
            }
            Cmd::Stop(cause, r) => self.begin_stop(cause, r),
            Cmd::Detail(req, r) => {
                let _ = r.send(self.core.interaction_detail(&req));
            }
            Cmd::Snapshot(r) => {
                let _ = r.send(self.snapshot());
            }
        }
        self.apply(fx);
    }

    fn on_signal_outcome(&mut self, r: Result<SignalOutcome, IpcError>) {
        let survived = match r {
            Ok(o) => o.survivor && !o.exited,
            Err(e) => {
                tracing::warn!(target: "convo", code = e.code.as_str(), "fenced SIGTERM refused");
                true
            }
        };
        if survived && self.exit.is_none() && self.exit_pending.is_none() {
            // Never SIGKILL a vendor CLI [C5]: report the survivor.
            self.survivor_reported = true;
            self.finish_stop(StopOutcome { stopped: false, survivors: 1 });
        }
    }

    async fn run(
        mut self,
        mut lines: mpsc::Receiver<ReadEvent>,
        mut cmds: mpsc::Receiver<Cmd>,
        mut exit_rx: oneshot::Receiver<ExitInfo>,
        mut sig_rx: mpsc::UnboundedReceiver<Result<SignalOutcome, IpcError>>,
        mut broken_rx: mpsc::UnboundedReceiver<()>,
    ) {
        let mut cmds_open = true;
        let mut exit_open = true;
        let mut broken_open = true;
        loop {
            if self.exit.is_some() && !cmds_open {
                return;
            }
            let deadline = self.deadline();
            tokio::select! {
                ev = lines.recv(), if !self.stdout_done => {
                    let now = self.now();
                    let mut fx = Vec::new();
                    match ev {
                        Some(ReadEvent::Line(l, permit)) => {
                            self.core.on_line(&l, &mut self.stdin, now, &mut fx);
                            drop(permit);
                        }
                        Some(ReadEvent::TooLarge) => self.core.on_too_large(now, &mut fx),
                        Some(ReadEvent::Eof) | None => {
                            self.stdout_done = true;
                        }
                    }
                    self.apply(fx);
                    if self.stdout_done {
                        if let Some((exit, _)) = self.exit_pending {
                            self.finalize_exit(exit);
                        }
                    }
                }
                c = cmds.recv(), if cmds_open => match c {
                    Some(c) => self.on_cmd(c),
                    None => {
                        cmds_open = false;
                        if self.exit.is_none() {
                            // The driver was dropped without a stop: stop the
                            // process rather than leave it unowned.
                            let (tx, _rx) = oneshot::channel();
                            self.begin_stop(StopCause::User, tx);
                        }
                    }
                },
                e = &mut exit_rx, if exit_open => {
                    exit_open = false;
                    let exit = e.unwrap_or(ExitInfo { code: None, signal: None });
                    // Nothing more can reach the process: answers and sends
                    // from now on are `withdrawn` / unsupported, never "sent".
                    self.stdin.close();
                    if self.stdout_done {
                        self.finalize_exit(exit);
                    } else {
                        self.exit_pending = Some((exit, Instant::now() + Duration::from_millis(self.deps.timing.drain_ms)));
                    }
                }
                s = sig_rx.recv() => {
                    if let Some(s) = s {
                        self.on_signal_outcome(s);
                    }
                }
                b = broken_rx.recv(), if broken_open => {
                    // The writer ended: stdin is gone either way.
                    broken_open = false;
                    let _ = b;
                    self.stdin.close();
                }
                _ = tokio::time::sleep_until(deadline.unwrap_or_else(|| Instant::now() + Duration::from_secs(3600))), if deadline.is_some() => {
                    self.on_deadline();
                }
            }
        }
    }
}

trait IntoSource {
    fn into_source(self) -> SourceFacts;
}

impl IntoSource for ProcessFacts {
    fn into_source(self) -> SourceFacts {
        SourceFacts::Process(self)
    }
}

// ---------------------------------------------------------------------------
// the driver
// ---------------------------------------------------------------------------

/// The Conversation driver of one session. Dropping it stops the process
/// (the stop sequence with cause `User`) if the session actor did not.
pub struct ConvoDriver {
    tx: mpsc::Sender<Cmd>,
    pid: i32,
    started_at: EpochMs,
    argv_redacted: Vec<String>,
    stderr: Arc<Mutex<StderrRing>>,
    timing: ConvoTiming,
    flags: Flags,
}

fn spawn_failed(detail: &str) -> IpcError {
    IpcError::spawn_failed().with_detail(crate::render::sanitize::display(detail, crate::contract::display::TextCap::Bytes(4 * 1024)))
}

impl ConvoDriver {
    /// Start a Conversation process (§6.1.1–§6.1.2). Must run inside a tokio
    /// runtime. Refuses a version below 2.1.268 or with `conversation: Failed`
    /// (`UNSUPPORTED{conversationMinVersion|conversation}`): T10 starts those in
    /// Terminal mode.
    pub async fn spawn(spec: ConvoSpec, deps: ConvoDeps) -> Result<ConvoDriver, IpcError> {
        deps.supervisor.spawn_gate().await?;
        let compat = deps.compat.for_binary(&spec.binary).await;
        if compat.below_minimum {
            return Err(IpcError::unsupported("conversationMinVersion"));
        }
        if compat.conversation == CompatState::Failed {
            return Err(IpcError::unsupported("conversation"));
        }
        let version = compat.version.clone();
        let flags = Flags::from_compat(&*deps.compat, &version);
        let args = handshake::argv(
            &ArgvSpec {
                launch: spec.launch.clone(),
                permission_mode: spec.permission_mode,
                model: spec.model.clone(),
                allow_bypass: spec.allow_bypass,
            },
            &flags,
        )?;
        let additions = handshake::env_additions(&flags);
        let mut env_keys: Vec<String> = (spec.env)("").into_keys().collect();
        env_keys.extend(additions.iter().map(|(k, _)| (*k).to_owned()));
        env_keys.sort();
        env_keys.dedup();
        let argv0 = spec.binary.realpath.clone();
        let ticket = deps
            .supervisor
            .plan_spawn(SpawnRequest {
                kind: ProcKind::Convo,
                argv0: argv0.clone(),
                args: args.clone(),
                cwd: Some(spec.cwd.clone()),
                env_keys,
                parent_uid: Some(spec.uid.clone()),
                generation: None,
                turn_id: None,
            })
            .await?;
        let mut env = (spec.env)(&ticket.env_marker);
        for (k, v) in additions {
            env.insert(k.to_owned(), v.to_owned());
        }
        let mut cmd = crate::supervise::spawn::tokio_command_grouped(Path::new(argv0.as_str()));
        cmd.args(&args)
            .current_dir(spec.cwd.as_str())
            .env_clear()
            .envs(&env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                deps.supervisor.mark_exited(&ticket, ExitInfo { code: None, signal: None }).await;
                return Err(spawn_failed(&e.to_string()));
            }
        };
        let pid = child.id().map(|p| p as i32).unwrap_or(-1);
        if let Err(e) = deps.supervisor.mark_running(&ticket, pid).await {
            tracing::warn!(target: "convo", code = e.code.as_str(), "could not mark the convo row running");
        }
        let now = Now { wall: deps.clock.wall(), mono: deps.clock.mono_ms() };
        let start_sec = crate::supervise::procscan::start_time_of(pid).unwrap_or(now.wall / 1000);
        let key = ProcessKey::new(pid.max(0) as u32, start_sec.max(0) as u64);
        let mut binary = spec.binary.clone();
        binary.pidpath = libproc::proc_pid::pidpath(pid).ok().and_then(|p| AbsPath::parse(p).ok());

        let (Some(stdin), Some(stdout), Some(stderr)) = (child.stdin.take(), child.stdout.take(), child.stderr.take()) else {
            return Err(spawn_failed("no pipes"));
        };
        let stderr_ring = Arc::new(Mutex::new(StderrRing::default()));
        let (in_tx, in_rx) = mpsc::channel::<QueuedFrame>(QUEUE_FRAMES);
        let bytes = Arc::new(AtomicUsize::new(0));
        let (close_tx, close_rx) = oneshot::channel();
        let (broken_tx, broken_rx) = mpsc::unbounded_channel();
        let withdrawn: Withdrawn = Arc::new(Mutex::new(std::collections::HashSet::new()));
        tokio::spawn(write_stdin(stdin, in_rx, bytes.clone(), close_rx, broken_tx, withdrawn.clone()));
        let (line_tx, line_rx) = mpsc::channel(LINE_QUEUE);
        tokio::spawn(read_stdout(stdout, line_tx, deps.timing.line_cap));
        tokio::spawn(read_stderr(stderr, stderr_ring.clone()));
        let (exit_tx, exit_rx) = oneshot::channel();
        tokio::spawn(async move {
            let exit = match child.wait().await {
                Ok(st) => ExitInfo { code: st.code(), signal: crate::supervise::spawn::exit_signal_of(&st) },
                Err(_) => ExitInfo { code: None, signal: None },
            };
            let _ = exit_tx.send(exit);
        });

        let config_dir: Option<PathBuf> = spec
            .config_dir
            .as_ref()
            .map(|c| PathBuf::from(c.as_str()))
            .or_else(|| spec.home.as_ref().map(|h| Path::new(h.as_str()).join(".claude")));
        let cfg = CoreConfig {
            uid: spec.uid.clone(),
            account_id: spec.account_id.clone(),
            launch: spec.launch.clone(),
            spawn_version: version.clone(),
            handshake_unchecked: compat.handshake_ok.is_none(),
            flags: flags.clone(),
            permission_mode: spec.permission_mode,
            // "Launched with it" [R4 §1.9]: only a start in bypass mode passes
            // `--allow-dangerously-skip-permissions` (§6.1.1).
            allow_bypass: spec.allow_bypass && spec.permission_mode == PermissionMode::BypassPermissions,
            model: spec.model.clone(),
            cwd: spec.cwd.clone(),
            home: spec.home.clone(),
            binary,
            settings_paths: guard::settings_paths(Path::new(spec.cwd.as_str()), config_dir.as_deref()),
            child_pid: pid,
            abandoned: spec.abandoned.clone(),
            initialize_timeout_ms: deps.timing.initialize_timeout_ms,
        };
        let mut fx = Vec::new();
        let core = Core::new(cfg, deps.patterns.clone(), now, &mut fx);
        let (sig_tx, sig_rx) = mpsc::unbounded_channel();
        let mut full_argv = vec![argv0.as_str().to_owned()];
        full_argv.extend(args.iter().cloned());
        let mut pump = Pump {
            core,
            uid: spec.uid.clone(),
            hint: spec.hint.clone(),
            account_id: spec.account_id.clone(),
            spawn_version: version,
            ticket,
            key,
            argv: full_argv.clone(),
            deps: deps.clone(),
            stdin: StdinQueue { tx: in_tx, bytes, closed: Arc::new(AtomicBool::new(false)), close: Some(close_tx), withdrawn },
            stderr: stderr_ring.clone(),
            phase: StopPhase::None,
            quit: false,
            force: false,
            stop_waiters: Vec::new(),
            force_waiters: Vec::new(),
            survivor_reported: false,
            exit: None,
            exit_pending: None,
            stdout_done: false,
            sig_tx,
        };
        pump.evidence(ProcessFacts::Alive { pid, start_sec, cpu_active_at: None }.into_source(), now.wall);
        pump.apply(fx);
        if pump.core.start(&mut pump.stdin).is_err() {
            tracing::warn!(target: "convo", "could not queue initialize");
        }
        if let Some(first) = spec.first_prompt.clone() {
            let mut fx = Vec::new();
            if let Err(e) = pump.core.send(first, &mut pump.stdin, now, &mut fx) {
                tracing::warn!(target: "convo", code = e.code.as_str(), "the first prompt was not queued");
            }
            pump.apply(fx);
        }
        let (tx, cmds) = mpsc::channel(64);
        tokio::spawn(pump.run(line_rx, cmds, exit_rx, sig_rx, broken_rx));
        Ok(ConvoDriver {
            tx,
            pid,
            started_at: now.wall,
            argv_redacted: deps.supervisor.redact_argv(&full_argv),
            stderr: stderr_ring,
            timing: deps.timing.clone(),
            flags,
        })
    }

    async fn call<T>(&self, make: impl FnOnce(Reply<T>) -> Cmd) -> Result<T, IpcError> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(make(tx)).await.map_err(|_| IpcError::unsupported("sessionEnded"))?;
        rx.await.map_err(|_| IpcError::unsupported("sessionEnded"))?
    }

    async fn reply_wait(&self, rx: oneshot::Receiver<GildeReply>, ms: u64, cap: &str) -> Result<GildeReply, IpcError> {
        match tokio::time::timeout(Duration::from_millis(ms), rx).await {
            Ok(Ok(GildeReply::Gone)) | Ok(Err(_)) => Err(IpcError::unsupported(cap)),
            Ok(Ok(r)) => Ok(r),
            Err(_) => Err(IpcError::timeout()),
        }
    }

    /// The child's pid.
    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// The flags this process runs with.
    pub fn flags(&self) -> &Flags {
        &self.flags
    }

    /// `interaction_answer` with the `previous` decision for `alreadyAnswered`.
    pub async fn answer_with_previous(&mut self, req: &RequestId, d: Decision) -> Result<(AnswerOutcome, Option<DecisionKind>), IpcError> {
        let req = req.clone();
        self.call(move |r| Cmd::Answer(req, d, r)).await
    }

    /// `Session.queued` (memory only).
    pub async fn snapshot(&self) -> Result<Snapshot, IpcError> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Cmd::Snapshot(tx)).await.map_err(|_| IpcError::unsupported("sessionEnded"))?;
        rx.await.map_err(|_| IpcError::unsupported("sessionEnded"))
    }

    /// `session_models` from the `initialize` response.
    pub async fn models(&self) -> Result<(Vec<ModelOption>, Option<String>), IpcError> {
        let s = self.snapshot().await?;
        Ok((s.models, s.model))
    }

    /// `interaction_detail` (the full input, ≤ 64 KiB, sanitized).
    pub async fn interaction_detail(&self, req: &RequestId) -> Result<InteractionDetailRes, IpcError> {
        let req = req.clone();
        self.call(move |r| Cmd::Detail(req, r)).await
    }

    /// The account's quota over the live session (`get_usage`, P12).
    pub async fn quota(&self) -> Result<Option<Quota>, IpcError> {
        let rx = self.call(Cmd::GetUsage).await?;
        match self.reply_wait(rx, self.timing.reply_wait_ms, "getUsage").await? {
            GildeReply::Usage(q) => Ok(q),
            _ => Err(IpcError::vendor_error(VendorErrorClass::Unknown, true)),
        }
    }

    /// `session_technical`: the redacted command line (§9.7).
    pub fn technical(&self) -> SessionTechnicalRes {
        SessionTechnicalRes { pid: self.pid, started_at: self.started_at, command_redacted: self.argv_redacted.clone(), parent_chain: Vec::new() }
    }

    /// The stderr tail for Log / "Show log" (sanitized; never persisted).
    pub fn stderr_tail(&self) -> String {
        self.stderr.lock().text()
    }
}

#[async_trait]
impl SessionDriver for ConvoDriver {
    fn mode(&self) -> ControlMode {
        ControlMode::Conversation
    }

    async fn send(&mut self, msg: OutgoingMessage) -> Result<SendDelivery, IpcError> {
        self.call(move |r| Cmd::Send(msg, r)).await
    }

    async fn cancel_queued(&mut self, id: &ClientMsgId) -> Result<bool, IpcError> {
        let id = id.clone();
        match self.call(move |r| Cmd::CancelQueued(id, r)).await? {
            CancelPlan::Done(b) => Ok(b),
            CancelPlan::Wait(rx) => match tokio::time::timeout(Duration::from_millis(self.timing.cancel_wait_ms), rx).await {
                Ok(Ok(GildeReply::Ok)) => Ok(true),
                _ => Ok(false),
            },
        }
    }

    async fn answer(&mut self, req: &RequestId, d: Decision) -> Result<AnswerOutcome, IpcError> {
        self.answer_with_previous(req, d).await.map(|(o, _)| o)
    }

    async fn screen_key(&mut self, _req: &RequestId, _rev: &str, _key: ScreenPromptAction) -> Result<ScreenKeyOutcome, IpcError> {
        Err(IpcError::unsupported("screenKey"))
    }

    async fn interrupt(&mut self, discard_queued: bool) -> Result<InterruptOutcome, IpcError> {
        self.call(move |r| Cmd::Interrupt(discard_queued, r)).await
    }

    async fn force_stop(&mut self) -> Result<bool, IpcError> {
        self.call(Cmd::ForceStop).await
    }

    async fn set_model(&mut self, m: Option<String>) -> Result<AppliesFrom, IpcError> {
        let (rx, applies) = self.call(move |r| Cmd::SetModel(m, r)).await?;
        match self.reply_wait(rx, self.timing.reply_wait_ms, "setModel").await? {
            GildeReply::Ok => Ok(applies),
            _ => Err(IpcError::vendor_error(VendorErrorClass::Unknown, false)),
        }
    }

    async fn set_permission_mode(&mut self, m: PermissionMode) -> Result<(), IpcError> {
        let rx = self.call(move |r| Cmd::SetMode(m, r)).await?;
        match self.reply_wait(rx, self.timing.reply_wait_ms, "setPermissionMode").await? {
            GildeReply::Ok => Ok(()),
            _ => Err(IpcError::vendor_error(VendorErrorClass::Unknown, false)),
        }
    }

    async fn stop(&mut self, why: StopCause) -> Result<StopOutcome, IpcError> {
        match self.call(move |r| Cmd::Stop(why, r)).await {
            Err(e) if e.params.get("cap").and_then(|c| c.as_str()) == Some("sessionEnded") => Ok(StopOutcome { stopped: true, survivors: 0 }),
            other => other,
        }
    }
}
