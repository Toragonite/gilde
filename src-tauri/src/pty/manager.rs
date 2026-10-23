//! The owner of every live PTY (contract §6.2) — the [`PtyApi`] T07 and T10
//! consume.
//!
//! Every PTY starts the same way:
//!
//! ```text
//! SupervisorApi::plan_spawn (durable row; §9.3 allowlist)   ← before a process exists
//!   → openpty(real winsize)
//!   → open the held slave (O_CLOEXEC | O_NOCTTY)              ← A52.1, before the fork
//!   → CommandBuilder(env_clear + the composed env + GILDE_SPAWN_ID)
//!   → slave.spawn_command
//!   → drop(pair.slave)                                       ← never the last close now
//!   → SupervisorApi::mark_running(pid)
//! ```
//!
//! The held slave is why a quickly exiting child's output survives (A52.1).
//! On macOS the last close of a pty slave waits at most ~0.6 s for the
//! master to be read, then discards what is unread (a child that opened
//! `/dev/tty` is spared: its session keeps the terminal until the drain).
//! Before A52.1 that last close was the core's `drop(pair.slave)` whenever
//! the child printed and exited before the core got there (no reader existed
//! yet, and the drop stalled an async worker for those 0.6 s), or the
//! child's own exit when the reader was starved. The session closes the held
//! slave once the child is reaped and the master drained (`session.rs`).
//!
//! A refused plan launches nothing (A12.8). A PTY whose spawn fails after the
//! plan closes its row as exited (and its held slave). Nothing here sends
//! SIGKILL; `close` goes through the supervisor's identity-fenced signal
//! (§9.8, C5).
//!
//! Titles (OSC 0/2) live only in memory (A12.4); nothing here writes
//! `state.db` except through the supervisor's registry.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dashmap::DashMap;
use parking_lot::RwLock;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};

use crate::accounts::AccountSlot;
use crate::agents::AgentRegistry;
use crate::config::ConfigStore;
use crate::contract::activity::{ExitInfo, PtyFacts, ScreenPromptFacts};
use crate::contract::common::{EpochMs, Vendor};
use crate::contract::ids::{ProcessKey, PtyId, ViewId};
use crate::contract::ipc::{PtyInfo, ScreenKeyOutcome, ScreenPromptAction};
use crate::contract::process::ProcKind;
use crate::contract::services::{
    EvidenceSink, EvidenceUpdate, IdentityHint, PtyApi, PtyExitSink, PtyHandle, PtyOpenSpec,
    SignalOutcome, SignalPlan, SourceFacts, SpawnRequest, SupervisorApi,
};
use crate::contract::IpcError;
use crate::status::engine::StatusListener;
use crate::status::evidence::trust_positions;
use crate::status::{ForegroundSource, StatusEngine};
use crate::store::now_ms;
use crate::supervise::registry::Supervisor;

use super::ring::{clamp_ring_bytes, DEFAULT_RING_BYTES};
use super::session::{paste_bytes, sanitize_size, ChunkSink, PtySession, SessionParts};

/// `GILDE_SPAWN_ID` — stamped from the plan's ticket, overriding whatever the
/// caller composed.
const ENV_SPAWN_ID: &str = "GILDE_SPAWN_ID";

/// How long `DenyWithReply` waits for the prompt to leave the grid (§6.2.4).
pub const DENY_REPLY_WAIT: Duration = Duration::from_secs(2);
/// How long `AllowWithNote` waits for option 1 to become the note field.
pub const AMEND_WAIT: Duration = Duration::from_secs(1);
/// How long the trust action waits for one arrow key's repaint.
pub const TRUST_KEY_WAIT: Duration = Duration::from_secs(1);
/// The repaint is over after this much silence.
pub const REPAINT_QUIET: Duration = Duration::from_millis(60);
/// At most this many single arrow keys toward the trust option (A6).
pub const TRUST_MAX_KEYS: usize = 3;
/// An identical screen-prompt answer within this long of the first, on the
/// same appearance of the prompt, is a double press (A34.2, A38.6).
pub const SCREEN_REPEAT_MS: EpochMs = 2_000;

/// The canonical form of a screen-prompt action (A34.2): `key:<k>`,
/// `trustFolder`, `denyWithReply:<sha256>` / `allowWithNote:<sha256>` of the
/// text (hex) — the text itself is never kept.
pub fn screen_action_key(action: &ScreenPromptAction) -> String {
    use sha2::Digest as _;
    let digest = |text: &str| hex::encode(sha2::Sha256::digest(text.as_bytes()));
    match action {
        ScreenPromptAction::Key { key } => format!("key:{key}"),
        ScreenPromptAction::TrustFolder => "trustFolder".to_string(),
        ScreenPromptAction::DenyWithReply { text } => format!("denyWithReply:{}", digest(text)),
        ScreenPromptAction::AllowWithNote { text } => format!("allowWithNote:{}", digest(text)),
    }
}

/// One answer already typed into a PTY's prompt (A38.6).
#[derive(Debug, Clone, PartialEq)]
pub struct TypedAnswer {
    pub rev: String,
    /// [`screen_action_key`].
    pub action: String,
    /// The held prompt's `first_seen_at` when it was typed — the appearance
    /// it answered; `None` when the engine had not adopted the prompt yet.
    pub held: Option<EpochMs>,
    pub at: EpochMs,
    pub outcome: ScreenKeyOutcome,
}

/// Is the prompt held now the same appearance as the one `typed` answered?
/// A held prompt that went away (`None` now) or came back (another
/// `first_seen_at`, or first seen after the answer) is a new one.
fn same_appearance(typed: &TypedAnswer, held_now: Option<EpochMs>) -> bool {
    match (typed.held, held_now) {
        (Some(then), Some(now)) => then == now,
        (Some(_), None) => false,
        (None, None) => true,
        (None, Some(now)) => now <= typed.at,
    }
}

/// The double-press memory of one PTY (A34.2, A38.6): an identical action on
/// the same rev, within [`SCREEN_REPEAT_MS`], on the same appearance of the
/// prompt gets the first outcome and types nothing. Anything else is
/// forgotten: another rev, a prompt that left the grid, an old entry.
#[derive(Debug, Default, Clone)]
pub struct TypedAnswers(Vec<TypedAnswer>);

impl TypedAnswers {
    /// The first outcome of an identical answer, if it still counts.
    pub fn recall(&mut self, rev: &str, action: &str, held_now: Option<EpochMs>, now: EpochMs) -> Option<ScreenKeyOutcome> {
        self.0.retain(|t| t.rev == rev && now.saturating_sub(t.at) < SCREEN_REPEAT_MS && same_appearance(t, held_now));
        self.0.iter().find(|t| t.action == action).map(|t| t.outcome)
    }

    pub fn record(&mut self, answer: TypedAnswer) {
        self.0.retain(|t| t.action != answer.action);
        self.0.push(answer);
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Per-PTY knobs that are not part of the contract's `PtyOpenSpec`.
#[derive(Debug, Clone, Default)]
pub struct PtyOptions {
    /// The `agent-signatures.json` key; derived from argv when absent.
    pub signature_key: Option<String>,
    pub ring_bytes: Option<usize>,
    pub idle_after_ms: Option<u64>,
    /// Turn off `OPOST`/`ONLCR` (a byte-transparent tty; fidelity tests).
    pub raw_output: bool,
    /// Test hook (T49): recreate one of the orders in which a quickly
    /// exiting child's output used to be lost.
    #[cfg(any(test, feature = "test-fixtures"))]
    pub test_hold: Option<TestHold>,
}

/// The T49 test holds (A52.1).
#[cfg(any(test, feature = "test-fixtures"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestHold {
    /// After the fork, the core waits until the child is exiting and its
    /// session has let go of the terminal, and only then closes its
    /// spawn-time slave and starts the reader. Without the held slave, that
    /// close was then the last one, with no reader yet: the order in which
    /// a child that printed and exited before the core got to its drop lost
    /// its output. The child's output must fit the tty's output queue (~640
    /// bytes on macOS); a larger write blocks for want of a reader, and the
    /// open fails at the hold's ceiling.
    SpawnSlaveUntilChildExited,
    /// The reader waits until the child is reaped, or `ceiling`, before its
    /// first read (a reader starved past the child's exit).
    ReaderUntilReaped { ceiling: Duration },
}

/// The ceiling of the [`TestHold::SpawnSlaveUntilChildExited`] wait.
#[cfg(any(test, feature = "test-fixtures"))]
const HOLD_CEILING: Duration = Duration::from_secs(10);

/// Whether `pid` is exiting and its session no longer has a terminal (or it
/// is gone), from its `kinfo_proc` (`sysctl KERN_PROC_PID`, 64-bit macOS
/// layout of `<sys/sysctl.h>`: `p_flag` at 32, `e_tdev` at 572, 648 bytes;
/// `procscan` reads the same record). A session leader reaches that stage
/// after it closed its descriptors, before it waits for its terminal to
/// drain.
#[cfg(any(test, feature = "test-fixtures"))]
fn exited_past_its_terminal(pid: i32) -> bool {
    const KINFO_SIZE: usize = 648;
    const KI_FLAG: usize = 32;
    const KI_TDEV: usize = 572;
    const P_WEXIT: i32 = 0x2000;
    const NODEV: i32 = -1;
    let mut mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PID, pid];
    let mut buf = [0u8; KINFO_SIZE];
    let mut len = buf.len();
    // SAFETY: `buf` is a writable `kinfo_proc`-sized buffer.
    let rc = unsafe { libc::sysctl(mib.as_mut_ptr(), 4, buf.as_mut_ptr().cast(), &mut len, std::ptr::null_mut(), 0) };
    if rc != 0 || len != KINFO_SIZE {
        return true;
    }
    let field = |at: usize| i32::from_ne_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]]);
    field(KI_FLAG) & P_WEXIT != 0 && field(KI_TDEV) == NODEV
}

/// [`TestHold::SpawnSlaveUntilChildExited`]: wait for that stage, or fail
/// at [`HOLD_CEILING`]. Another process's exit has no event to wait on here,
/// so this polls; it is test-only.
#[cfg(any(test, feature = "test-fixtures"))]
async fn wait_child_exited(pid: i32) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + HOLD_CEILING;
    while !exited_past_its_terminal(pid) {
        if tokio::time::Instant::now() >= deadline {
            return Err("test hold: the child did not exit".to_string());
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    Ok(())
}

/// The vendor and signature key of a PTY, from what it runs (§5.2.2's
/// classifier, the one shared [`crate::agents::classify`], A39.1(a)): the
/// binary itself with its argv, or — for a hosted pane (`/bin/sh -c
/// <command>`) — the first vendor CLI the command string runs.
pub fn classify(kind: ProcKind, argv0: &str, args: &[String]) -> (Option<Vendor>, String) {
    let argv: Vec<&str> = std::iter::once(argv0).chain(args.iter().map(String::as_str)).collect();
    let mut vendor = crate::agents::classify::vendor_of_process(argv0, &argv);
    if vendor.is_none() && kind == ProcKind::Pane {
        if let Some(command) = args.iter().skip_while(|a| a.as_str() != "-c").nth(1) {
            vendor = crate::agents::classify::vendor_of_command(command);
        }
    }
    let key = match vendor {
        Some(Vendor::Claude) => "claude",
        Some(Vendor::Codex) => "codex",
        Some(Vendor::Cursor) => "cursor",
        Some(Vendor::Agy) => "agy",
        None => "shell",
    };
    (vendor, key.to_string())
}

/// Pushes S5 facts to World as evidence (the producer side of §3.5).
struct EvidenceRelay {
    sink: RwLock<Option<Arc<dyn EvidenceSink>>>,
    hints: DashMap<String, IdentityHint>,
}

impl StatusListener for EvidenceRelay {
    fn facts_changed(&self, session_id: &str, facts: &PtyFacts) {
        let Some(sink) = self.sink.read().clone() else {
            return;
        };
        let Some(hint) = self.hints.get(session_id).map(|h| h.clone()) else {
            return;
        };
        sink.evidence(EvidenceUpdate {
            hint,
            facts: SourceFacts::Pty(facts.clone()),
            at: now_ms(),
        });
    }
}

/// Owner of the live PTY table.
pub struct PtyManager {
    supervisor: Arc<dyn SupervisorApi>,
    status: Arc<StatusEngine>,
    sessions: DashMap<String, Arc<PtySession>>,
    relay: Arc<EvidenceRelay>,
    ring_bytes: usize,
    idle_after_ms: u64,
    /// Answers already typed into each PTY's prompt (A38.6).
    typed: DashMap<String, TypedAnswers>,
}

impl std::fmt::Debug for PtyManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PtyManager")
            .field("sessions", &self.sessions.len())
            .finish()
    }
}

impl PtyManager {
    /// The composition root's constructor (kept for `lib.rs`; T09b may switch
    /// to [`PtyManager::with_api`]).
    pub fn new(supervisor: Arc<Supervisor>, config: Arc<ConfigStore>) -> PtyManager {
        let settings = config.get();
        let mut manager = PtyManager::with_api(supervisor as Arc<dyn SupervisorApi>);
        manager.ring_bytes = clamp_ring_bytes(settings.terminal.scrollback_bytes);
        manager.idle_after_ms = settings.status.idle_after_ms;
        manager.status.set_stall_after_ms(settings.status.stall_after_ms);
        manager
    }

    /// A manager over any [`SupervisorApi`] with a private status engine.
    pub fn with_api(supervisor: Arc<dyn SupervisorApi>) -> PtyManager {
        let relay = Arc::new(EvidenceRelay {
            sink: RwLock::new(None),
            hints: DashMap::new(),
        });
        let status = Arc::new(StatusEngine::new());
        status.set_listener(relay.clone());
        PtyManager {
            supervisor,
            status,
            sessions: DashMap::new(),
            relay,
            ring_bytes: DEFAULT_RING_BYTES,
            idle_after_ms: crate::agents::signatures::DEFAULT_IDLE_AFTER_MS,
            typed: DashMap::new(),
        }
    }

    /// Use the app-wide status engine (the one with the hot-reloading
    /// signatures and the ticker).
    pub fn with_status(mut self, status: Arc<StatusEngine>) -> PtyManager {
        status.set_listener(self.relay.clone());
        self.status = status;
        self
    }

    /// Kept for the composition root: signatures reach the engine through
    /// `StatusEngine::set_signatures`.
    pub fn with_agents(self, agents: Arc<AgentRegistry>) -> PtyManager {
        let _ = agents;
        self
    }

    /// Kept for the composition root: the caller composes the account
    /// variable into `PtyOpenSpec.env` (§6.0).
    pub fn with_accounts(self, accounts: Arc<Vec<AccountSlot>>) -> PtyManager {
        let _ = accounts;
        self
    }

    /// Where S5 facts go (World's sink; wired by T09b).
    pub fn set_evidence_sink(&self, sink: Arc<dyn EvidenceSink>) {
        *self.relay.sink.write() = Some(sink);
    }

    pub fn status_engine(&self) -> &Arc<StatusEngine> {
        &self.status
    }

    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// A live session, or `UNSUPPORTED{noPty}` (§4.2.4).
    pub fn get(&self, pty: &PtyId) -> Result<Arc<PtySession>, IpcError> {
        self.sessions
            .get(pty.as_str())
            .map(|e| e.value().clone())
            .ok_or_else(|| IpcError::unsupported("noPty"))
    }

    /// The ids of every PTY still open, oldest first.
    pub fn list(&self) -> Vec<PtyId> {
        let mut all: Vec<(i64, PtyId)> = self
            .sessions
            .iter()
            .map(|e| (e.value().opened_at(), e.value().pty_id().clone()))
            .collect();
        all.sort_by_key(|(at, _)| *at);
        all.into_iter().map(|(_, id)| id).collect()
    }

    // -- opening -------------------------------------------------------------

    /// [`PtyApi::open`] with the knobs the contract struct does not carry.
    pub async fn open_with(
        &self,
        spec: PtyOpenSpec,
        on_exit: Arc<dyn PtyExitSink>,
        options: PtyOptions,
    ) -> Result<PtyHandle, IpcError> {
        if !matches!(spec.kind, ProcKind::Pty | ProcKind::Attach | ProcKind::Pane) {
            return Err(IpcError::invalid_argument("kind", "reserved"));
        }
        for (key, value) in &spec.env {
            if key.is_empty() || key.contains('=') || key.contains('\0') || value.contains('\0') {
                return Err(IpcError::invalid_argument("env", "pattern"));
            }
        }
        let cwd = PathBuf::from(spec.cwd.as_str());
        if !cwd.is_dir() {
            return Err(IpcError::not_found("cwd"));
        }
        let (cols, rows) = sanitize_size(spec.cols, spec.rows);
        let (vendor, derived_key) = classify(spec.kind, spec.argv0.as_str(), &spec.args);
        let agent_key = options.signature_key.clone().unwrap_or(derived_key);

        StatusEngine::ensure_ticker(&self.status);
        // 1. The durable row (and the §9.3 allowlist) before anything forks.
        self.supervisor.spawn_gate().await?;
        let ticket = self
            .supervisor
            .plan_spawn(SpawnRequest {
                kind: spec.kind,
                argv0: spec.argv0.clone(),
                args: spec.args.clone(),
                cwd: Some(spec.cwd.clone()),
                env_keys: spec.env.iter().map(|(k, _)| k.clone()).collect(),
                parent_uid: spec.parent_uid.clone(),
                generation: None,
                turn_id: None,
            })
            .await?;
        let failed = |detail: String| {
            let supervisor = self.supervisor.clone();
            let ticket = ticket.clone();
            async move {
                supervisor
                    .mark_exited(&ticket, ExitInfo { code: None, signal: None })
                    .await;
                IpcError::spawn_failed().with_detail(detail)
            }
        };

        // 2. The pty. Never 0×0.
        let pair = match native_pty_system().openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        }) {
            Ok(pair) => pair,
            Err(e) => return Err(failed(format!("openpty failed: {e}")).await),
        };
        if let Some(fd) = pair.master.as_raw_fd() {
            // SAFETY: `pair.master` owns the descriptor for the whole call.
            let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
            if let Err(e) = set_output_processing(borrowed, !options.raw_output) {
                if options.raw_output {
                    return Err(failed(e).await);
                }
                tracing::warn!(target: "pty", error = %e, "could not set the tty output flags");
            }
        }
        // 2b. The core's own slave (A52.1), before anything forks: neither
        //     the child's exit nor the spawn-time slave's close below can then
        //     be the last close of the slave. Closed with the session; any
        //     return before that closes it here.
        let Some(tty) = pair.master.tty_name() else {
            return Err(failed("the pty has no slave name".to_string()).await);
        };
        let held_slave = match super::session::open_held_slave(&tty) {
            Ok(fd) => fd,
            Err(e) => return Err(failed(format!("could not hold the pty slave: {e}")).await),
        };

        // 3. The command: the composed env, never an inherited one.
        let mut cmd = CommandBuilder::new(spec.argv0.as_str());
        for arg in &spec.args {
            cmd.arg(arg);
        }
        cmd.cwd(&cwd);
        cmd.env_clear();
        for (key, value) in &spec.env {
            if key != ENV_SPAWN_ID {
                cmd.env(key, value);
            }
        }
        cmd.env(ENV_SPAWN_ID, &ticket.env_marker);

        // 4. Fork, then drop the spawn-time slave: the core keeps the held
        //    one, and the child its own.
        let child = match pair.slave.spawn_command(cmd) {
            Ok(child) => child,
            Err(e) => return Err(failed(format!("could not start the pty child: {e}")).await),
        };
        #[cfg(any(test, feature = "test-fixtures"))]
        if options.test_hold == Some(TestHold::SpawnSlaveUntilChildExited) {
            if let Some(pid) = child.process_id() {
                if let Err(e) = wait_child_exited(pid as i32).await {
                    return Err(failed(e).await);
                }
            }
        }
        drop(pair.slave);
        let Some(pid) = child.process_id().map(|p| p as i32) else {
            return Err(failed("the pty child reported no pid".to_string()).await);
        };
        let pgid = crate::supervise::spawn::pgid_of(pid).unwrap_or(pid);
        self.supervisor.mark_running(&ticket, pid).await?;
        let start_sec = crate::supervise::procscan::start_time_of(pid);

        // 5. The session.
        let pty_id = PtyId::mint();
        let exit_supervisor = self.supervisor.clone();
        let exit_ticket = ticket.clone();
        let exit_pty = pty_id.clone();
        let on_exit_hook: Box<dyn FnOnce(ExitInfo, i64) + Send> = Box::new(move |exit, at| {
            // The registry row first, then the owner hears about it.
            tauri::async_runtime::spawn(async move {
                exit_supervisor.mark_exited(&exit_ticket, exit).await;
                on_exit.exited(&exit_pty, exit, at);
            });
        });
        let session = Arc::new(PtySession::start(
            SessionParts {
                pty_id: pty_id.clone(),
                kind: spec.kind,
                ticket: ticket.clone(),
                agent_key: agent_key.clone(),
                cwd: cwd.clone(),
                pid,
                pgid,
                start_sec,
                cols,
                rows,
                ring_bytes: options.ring_bytes.unwrap_or(self.ring_bytes),
                idle_after_ms: options.idle_after_ms.unwrap_or(self.idle_after_ms),
                status: self.status.clone(),
                on_exit: on_exit_hook,
                held_slave,
                #[cfg(any(test, feature = "test-fixtures"))]
                hold_reader: match options.test_hold {
                    Some(TestHold::ReaderUntilReaped { ceiling }) => Some(ceiling),
                    _ => None,
                },
            },
            pair.master,
            child,
        )?);
        let foreground: Arc<dyn ForegroundSource> = session.clone();
        self.status.attach_foreground(pty_id.as_str(), &foreground);
        self.relay.hints.insert(
            pty_id.as_str().to_string(),
            IdentityHint {
                vendor,
                process: start_sec
                    .and_then(|s| u64::try_from(s).ok())
                    .map(|s| ProcessKey::new(pid as u32, s)),
                gilde_spawn_id: Some(ticket.spawn_id.clone()),
                ..IdentityHint::default()
            },
        );
        self.sessions.insert(pty_id.as_str().to_string(), session);
        tracing::info!(
            target: "pty",
            pty = %pty_id.as_str(),
            spawn = %ticket.spawn_id,
            kind = spec.kind.as_str(),
            agent = %agent_key,
            pid,
            cols,
            rows,
            "pty open"
        );
        Ok(PtyHandle { pty_id, pid, ticket })
    }

    // -- the view ------------------------------------------------------------

    /// `pty_attach` (§6.2.6).
    pub fn attach(&self, pty: &PtyId, view: ViewId, sink: Arc<dyn ChunkSink>) -> Result<PtyInfo, IpcError> {
        Ok(self.get(pty)?.attach(view, sink))
    }

    /// `pty_detach`: idempotent, also for a PTY that is gone.
    pub fn detach(&self, pty: &PtyId, view: &ViewId) {
        if let Ok(session) = self.get(pty) {
            session.detach(view);
        }
    }

    /// `pty_snapshot`: the `GLDP` header + the ring.
    pub fn snapshot(&self, pty: &PtyId) -> Result<Vec<u8>, IpcError> {
        Ok(self.get(pty)?.snapshot())
    }

    /// The pane title (OSC 0/2), sanitized, memory only.
    pub fn title(&self, pty: &PtyId) -> Option<String> {
        self.status.title(pty.as_str())
    }

    /// The readiness threshold of this PTY's vendor (§6.2.2).
    pub fn ready_threshold(&self, pty: &PtyId) -> f32 {
        self.status.ready_threshold(pty.as_str())
    }

    /// S5 facts as they change.
    pub fn subscribe(&self, pty: &PtyId) -> Option<tokio::sync::watch::Receiver<Option<PtyFacts>>> {
        self.status.subscribe(pty.as_str())
    }

    // -- screen actions (§6.2.4) ---------------------------------------------

    fn key_bytes(session: &PtySession, status: &StatusEngine, up: bool) -> &'static [u8] {
        let app = status
            .screen(session.id())
            .map(|s| s.app_cursor_keys)
            .unwrap_or(false);
        match (app, up) {
            (true, true) => b"\x1bOA",
            (true, false) => b"\x1bOB",
            (false, true) => b"\x1b[A",
            (false, false) => b"\x1b[B",
        }
    }

    fn generation(&self, session: &PtySession) -> Option<u64> {
        self.status.screen(session.id()).map(|s| s.generation)
    }

    /// Wait until the grid no longer shows the prompt `rev`, up to `max`.
    async fn wait_prompt_gone(&self, session: &PtySession, rev: &str, max: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + max;
        loop {
            let still = self
                .status
                .rescan(session.id())
                .flatten()
                .is_some_and(|f| f.prompt_rev == rev);
            if !still {
                return true;
            }
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                return false;
            }
            let since = self.generation(session);
            session.wait_screen_change(since, REPAINT_QUIET, left).await;
        }
    }

    async fn trust_folder(
        &self,
        session: &PtySession,
        mut facts: ScreenPromptFacts,
    ) -> Result<ScreenKeyOutcome, IpcError> {
        let signatures = self.status.signature_set();
        let Some(trust) = signatures
            .as_deref()
            .and_then(|s| s.trust_dialog(session.agent_key()))
            .cloned()
        else {
            return Err(IpcError::refused("setupPrompt"));
        };
        let rev = facts.prompt_rev.clone();
        let mut keys = 0usize;
        loop {
            let Some((highlighted, target)) = trust_positions(&facts, &trust) else {
                return Ok(ScreenKeyOutcome::Aborted);
            };
            if highlighted == target {
                session.write_unlocked(b"\r".to_vec()).await?;
                return Ok(ScreenKeyOutcome::Sent);
            }
            if keys >= TRUST_MAX_KEYS {
                return Ok(ScreenKeyOutcome::Aborted);
            }
            let since = self.generation(session);
            let key = Self::key_bytes(session, &self.status, target < highlighted);
            session.write_unlocked(key.to_vec()).await?;
            keys += 1;
            session.wait_screen_change(since, REPAINT_QUIET, TRUST_KEY_WAIT).await;
            match self.status.rescan(session.id()).flatten() {
                // The same dialog (a moved highlight keeps the digest).
                Some(next) if next.prompt_rev == rev && next.trust_dialog => facts = next,
                // Anything else — another screen, no dialog: abort, no Enter.
                _ => return Ok(ScreenKeyOutcome::Aborted),
            }
        }
    }

    /// Wait until the grid's option 1 reads as the note field.
    async fn wait_amend_field(&self, session: &PtySession, max: Duration) -> bool {
        let signatures = self.status.signature_set();
        let Some(card) = signatures
            .as_deref()
            .and_then(|s| s.prompt_card(session.agent_key()))
            .cloned()
        else {
            return false;
        };
        let deadline = tokio::time::Instant::now() + max;
        loop {
            let ready = self
                .status
                .rescan(session.id())
                .flatten()
                .and_then(|f| f.options.first().map(|o| card.amend_label.is_match(&o.label)))
                .unwrap_or(false);
            if ready {
                return true;
            }
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                return false;
            }
            let since = self.generation(session);
            session.wait_screen_change(since, REPAINT_QUIET, left).await;
        }
    }
}

#[async_trait]
impl PtyApi for PtyManager {
    async fn open(&self, spec: PtyOpenSpec, on_exit: Arc<dyn PtyExitSink>) -> Result<PtyHandle, IpcError> {
        self.open_with(spec, on_exit, PtyOptions::default()).await
    }

    async fn write(&self, pty: &PtyId, bytes: &[u8]) -> Result<(), IpcError> {
        self.get(pty)?.write(bytes).await
    }

    async fn paste(&self, pty: &PtyId, text: &str, submit: bool) -> Result<(), IpcError> {
        self.get(pty)?.paste(text, submit).await
    }

    async fn resize(&self, pty: &PtyId, cols: u16, rows: u16) -> Result<(), IpcError> {
        self.get(pty)?.resize(cols, rows).map(|_| ())
    }

    /// Identity-fenced signal through the supervisor (never SIGKILL for a
    /// vendor CLI, §9.8). The PTY stays in the table until the child is
    /// reaped, so a survivor can still be closed again.
    async fn close(&self, pty: &PtyId, plan: SignalPlan) -> Result<SignalOutcome, IpcError> {
        let session = self.get(pty)?;
        if session.has_exited() {
            self.retire(&session);
            return Ok(SignalOutcome {
                exited: true,
                survivor: false,
            });
        }
        let Some(start) = session.start_sec().and_then(|s| u64::try_from(s).ok()) else {
            return Err(IpcError::refused("noIdentity"));
        };
        let key = ProcessKey::new(session.pid() as u32, start);
        let outcome = self.supervisor.signal(&key, plan).await?;
        if outcome.exited {
            // The registry says it is gone; give the exit watcher a moment to
            // reap it so the exit sink fires before the PTY leaves the table.
            session.wait_exit(Duration::from_secs(2)).await;
            self.retire(&session);
        }
        Ok(outcome)
    }

    fn screen_prompt(&self, pty: &PtyId) -> Option<ScreenPromptFacts> {
        self.status.prompt(pty.as_str())
    }

    async fn screen_action(
        &self,
        pty: &PtyId,
        prompt_rev: &str,
        action: &ScreenPromptAction,
    ) -> Result<ScreenKeyOutcome, IpcError> {
        let session = self.get(pty)?;
        let _gate = session.write_gate.lock().await;
        // TOCTOU: re-read the grid under the write lock (§6.2.4). The rev
        // covers the prompt box's header, so a prompt for another command is
        // another rev (A34.1).
        let Some(current) = self.status.rescan(session.id()).flatten() else {
            // The prompt left the grid: what was typed into it is forgotten.
            self.typed.remove(session.id());
            return Ok(ScreenKeyOutcome::Withdrawn);
        };
        if current.prompt_rev != prompt_rev {
            return Err(IpcError::stale_prompt());
        }
        // A double press (A34.2, A38.6): the same action on the same
        // appearance of this prompt within 2 s gets the first outcome and
        // types nothing; another action is executed on its own.
        let action_key = screen_action_key(action);
        let held = self
            .status
            .prompt(session.id())
            .filter(|p| p.prompt_rev == current.prompt_rev)
            .map(|p| p.first_seen_at);
        let now = now_ms();
        let repeat = self
            .typed
            .entry(session.id().to_string())
            .or_default()
            .recall(&current.prompt_rev, &action_key, held, now);
        if let Some(outcome) = repeat {
            return Ok(outcome);
        }
        let rev = current.prompt_rev.clone();
        let result = self.answer_prompt(&session, current, prompt_rev, action).await;
        if let Ok(outcome @ (ScreenKeyOutcome::Sent | ScreenKeyOutcome::Aborted)) = &result {
            self.typed.entry(session.id().to_string()).or_default().record(TypedAnswer {
                rev,
                action: action_key,
                held,
                at: now,
                outcome: *outcome,
            });
        }
        result
    }

    fn facts(&self, pty: &PtyId) -> Option<PtyFacts> {
        if !self.sessions.contains_key(pty.as_str()) {
            return None;
        }
        self.status.facts(pty.as_str())
    }
}

impl PtyManager {
    /// Type the answer `action` into the prompt `current` (§6.2.4), under the
    /// write lock the caller holds.
    async fn answer_prompt(
        &self,
        session: &PtySession,
        current: ScreenPromptFacts,
        prompt_rev: &str,
        action: &ScreenPromptAction,
    ) -> Result<ScreenKeyOutcome, IpcError> {
        match action {
            ScreenPromptAction::TrustFolder => {
                if !current.setup {
                    return Err(IpcError::unsupported("trustFolder"));
                }
                if !current.trust_dialog {
                    return Err(IpcError::refused("setupPrompt"));
                }
                self.trust_folder(session, current).await
            }
            _ if current.setup => Err(IpcError::refused("setupPrompt")),
            ScreenPromptAction::Key { key } => {
                if key == "esc" {
                    session.write_unlocked(b"\x1b".to_vec()).await?;
                    return Ok(ScreenKeyOutcome::Sent);
                }
                if !current.options.iter().any(|o| &o.key == key) {
                    return Err(IpcError::unsupported("key"));
                }
                // The digit selects at once — no CR (frozen, LP-12).
                session.write_unlocked(key.as_bytes().to_vec()).await?;
                Ok(ScreenKeyOutcome::Sent)
            }
            ScreenPromptAction::DenyWithReply { text } => {
                // The "No" digit on a permission prompt, else Esc: both end the
                // turn and leave the TUI waiting in its composer.
                let no = if current.permission_shaped {
                    current.options.last().map(|o| o.key.clone().into_bytes())
                } else {
                    None
                }
                .unwrap_or_else(|| b"\x1b".to_vec());
                session.write_unlocked(no).await?;
                if !self.wait_prompt_gone(session, prompt_rev, DENY_REPLY_WAIT).await {
                    // Denied, but the reply has nowhere safe to go.
                    return Ok(ScreenKeyOutcome::Aborted);
                }
                session.write_unlocked(paste_bytes(text, true)).await?;
                Ok(ScreenKeyOutcome::Sent)
            }
            ScreenPromptAction::AllowWithNote { text } => {
                if !current.allows_note {
                    return Err(IpcError::unsupported("allowWithNote"));
                }
                session.write_unlocked(b"\t".to_vec()).await?;
                if !self.wait_amend_field(session, AMEND_WAIT).await {
                    return Ok(ScreenKeyOutcome::Aborted);
                }
                session.write_unlocked(paste_bytes(text, false)).await?;
                session.write_unlocked(b"\r".to_vec()).await?;
                Ok(ScreenKeyOutcome::Sent)
            }
        }
    }

    /// Drop a finished PTY: stop delivering and forget it. The master closes
    /// when the last reference goes (safe: the child is gone).
    fn retire(&self, session: &Arc<PtySession>) {
        session.drop_view();
        self.sessions.remove(session.id());
        self.relay.hints.remove(session.id());
        self.typed.remove(session.id());
    }

    /// Forget every PTY whose child was reaped (called by the owner after the
    /// exit sink fired; also safe to call at any time).
    pub fn reap_exited(&self) -> usize {
        let done: Vec<Arc<PtySession>> = self
            .sessions
            .iter()
            .filter(|e| e.value().has_exited())
            .map(|e| e.value().clone())
            .collect();
        for session in &done {
            self.retire(session);
        }
        done.len()
    }

    /// The PIDs of every live PTY child (the quit sweep, diagnostics).
    pub fn live_pids(&self) -> Vec<i32> {
        self.sessions
            .iter()
            .filter(|e| !e.value().has_exited())
            .map(|e| e.value().pid())
            .collect()
    }
}

/// Turn the tty's output post-processing (`OPOST | ONLCR`) on or off.
fn set_output_processing(fd: std::os::fd::BorrowedFd<'_>, enabled: bool) -> Result<(), String> {
    use nix::sys::termios::{tcgetattr, tcsetattr, OutputFlags, SetArg};
    let mut termios = tcgetattr(fd).map_err(|e| format!("tcgetattr on the pty master: {e}"))?;
    let flags = OutputFlags::OPOST | OutputFlags::ONLCR;
    if enabled {
        termios.output_flags.insert(flags);
    } else {
        termios.output_flags.remove(flags);
    }
    tcsetattr(fd, SetArg::TCSANOW, &termios).map_err(|e| format!("tcsetattr on the pty master: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn the_vendor_comes_from_the_binary_or_the_pane_command() {
        assert_eq!(
            classify(ProcKind::Pty, "/opt/bin/claude", &[]),
            (Some(Vendor::Claude), "claude".to_string())
        );
        assert_eq!(
            classify(ProcKind::Pty, "/usr/local/bin/cursor-agent", &[]),
            (Some(Vendor::Cursor), "cursor".to_string())
        );
        assert_eq!(
            classify(
                ProcKind::Pane,
                "/bin/sh",
                &s(&["-c", "cd /tmp/p && env CLAUDECODE=1 /opt/bin/claude --agent-id a@t"])
            ),
            (Some(Vendor::Claude), "claude".to_string())
        );
        assert_eq!(
            classify(ProcKind::Pane, "/bin/sh", &s(&["-c", "cat"])),
            (None, "shell".to_string())
        );
        assert_eq!(classify(ProcKind::Pty, "/tmp/fake_agent", &[]).1, "shell");
        // A39.1(a): the install layouts Gilde spawns by realpath.
        assert_eq!(
            classify(ProcKind::Pty, "/Users/u/.local/share/claude/versions/2.1.307", &s(&["--resume"])),
            (Some(Vendor::Claude), "claude".to_string())
        );
        assert_eq!(
            classify(
                ProcKind::Pty,
                "/Users/u/.local/share/cursor-agent/versions/1/node",
                &s(&["--use-system-ca", "/Users/u/.local/share/cursor-agent/versions/1/index.js"])
            ),
            (Some(Vendor::Cursor), "cursor".to_string())
        );
        assert_eq!(
            classify(
                ProcKind::Pane,
                "/bin/sh",
                &s(&["-c", "cd /tmp/p && /Users/u/.local/share/claude/versions/2.1.307 --agent-id a@t"])
            ),
            (Some(Vendor::Claude), "claude".to_string())
        );
        assert_eq!(classify(ProcKind::Pty, "/opt/homebrew/bin/node", &s(&["/srv/server.js"])).1, "shell");
    }

    fn typed(action: &str, held: Option<EpochMs>, at: EpochMs) -> TypedAnswer {
        TypedAnswer { rev: "aaaaaaaaaaaaaaaa".into(), action: action.into(), held, at, outcome: ScreenKeyOutcome::Sent }
    }

    /// A34.2/A38.6: only an identical action on the same rev, within 2 s,
    /// on the same appearance of the prompt is a double press.
    #[test]
    fn a_double_press_is_the_same_action_rev_and_appearance_within_two_seconds() {
        let rev = "aaaaaaaaaaaaaaaa";
        let mut memo = TypedAnswers::default();
        memo.record(typed("key:1", Some(500), 1_000));
        assert_eq!(memo.recall(rev, "key:1", Some(500), 1_100), Some(ScreenKeyOutcome::Sent), "a double press");
        assert_eq!(memo.recall(rev, "key:3", Some(500), 1_100), None, "another action is executed on its own");
        assert_eq!(memo.recall(rev, "key:1", Some(500), 1_000 + SCREEN_REPEAT_MS), None, "2 s later it is a new press");

        let mut memo = TypedAnswers::default();
        memo.record(typed("key:1", Some(500), 1_000));
        assert_eq!(memo.recall("bbbbbbbbbbbbbbbb", "key:1", Some(500), 1_100), None, "another rev");
        assert!(memo.is_empty(), "the rev changed: forgotten");

        let mut memo = TypedAnswers::default();
        memo.record(typed("key:1", Some(500), 1_000));
        assert_eq!(memo.recall(rev, "key:1", None, 1_100), None, "the prompt left the grid");
        assert!(memo.is_empty());

        let mut memo = TypedAnswers::default();
        memo.record(typed("key:1", Some(500), 1_000));
        assert_eq!(memo.recall(rev, "key:1", Some(1_400), 1_500), None, "it came back: a new appearance");

        // Typed before the engine adopted the prompt: the same appearance is
        // the one first seen no later than the answer.
        let mut memo = TypedAnswers::default();
        memo.record(typed("key:1", None, 1_000));
        assert_eq!(memo.recall(rev, "key:1", None, 1_100), Some(ScreenKeyOutcome::Sent));
        assert_eq!(memo.recall(rev, "key:1", Some(990), 1_200), Some(ScreenKeyOutcome::Sent));
        assert_eq!(memo.recall(rev, "key:1", Some(1_300), 1_400), None);
    }

    #[test]
    fn several_actions_are_remembered_and_a_repeat_replaces_its_own_entry() {
        let rev = "aaaaaaaaaaaaaaaa";
        let mut memo = TypedAnswers::default();
        memo.record(typed("key:1", Some(5), 100));
        memo.record(typed("key:3", Some(5), 200));
        assert_eq!(memo.recall(rev, "key:1", Some(5), 300), Some(ScreenKeyOutcome::Sent));
        assert_eq!(memo.recall(rev, "key:3", Some(5), 300), Some(ScreenKeyOutcome::Sent));
        memo.record(TypedAnswer { outcome: ScreenKeyOutcome::Aborted, ..typed("key:3", Some(5), 400) });
        assert_eq!(memo.recall(rev, "key:3", Some(5), 500), Some(ScreenKeyOutcome::Aborted));
    }

    /// The action's canonical form keys texts by their sha256 and never holds
    /// the text itself (no content in memory beyond the call).
    #[test]
    fn the_action_key_names_the_action_and_hashes_its_text() {
        let deny = |t: &str| screen_action_key(&ScreenPromptAction::DenyWithReply { text: t.into() });
        let note = |t: &str| screen_action_key(&ScreenPromptAction::AllowWithNote { text: t.into() });
        assert_eq!(screen_action_key(&ScreenPromptAction::Key { key: "1".into() }), "key:1");
        assert_eq!(screen_action_key(&ScreenPromptAction::Key { key: "esc".into() }), "key:esc");
        assert_eq!(screen_action_key(&ScreenPromptAction::TrustFolder), "trustFolder");
        assert_eq!(deny("keep my documents"), deny("keep my documents"));
        assert_ne!(deny("keep my documents"), deny("keep my files"));
        assert_ne!(deny("same"), note("same"), "the action is part of the key");
        assert!(!deny("keep my documents").contains("documents"), "only a digest of the text");
        assert_eq!(deny("x").len(), "denyWithReply:".len() + 64);
    }
}
