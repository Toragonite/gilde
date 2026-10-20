//! One live PTY session: the wire types, the three threads, and the closing
//! ceremony (DESIGN §6-1 … §6-5).
//!
//! ## The three threads
//!
//! ```text
//!   reader (OS thread)      ring.push  ─┐            never blocks, ever
//!        │ try_send                     │
//!        ▼                              ▼
//!   coalescer (tokio task) ── 8 ms / 32 KiB ──► Channel<PtyChunk>
//!
//!   exit watcher (OS thread) ── child.wait() ──► mark_exited + gilde://session/exit
//! ```
//!
//! The reader gets its own **OS thread** rather than `spawn_blocking`: the read
//! blocks for the entire life of the session, and a `spawn_blocking` task that
//! never returns permanently consumes one of tokio's blocking workers.
//!
//! ## The invariant that shapes everything here
//!
//! **A slow or wedged webview must never apply back-pressure to the agent.**
//! The reader pushes into the ring (which evicts, never waits) and then
//! *attempts* to hand the frame to the coalescer with `try_send`. A full queue
//! sets `resync` and the frame is dropped: the user loses intermediate frames
//! and gets a `gilde://pty/resync` telling the frontend to re-snapshot, which is a
//! visual hiccup rather than a stalled agent.
//!
//! ## No byte is ever decoded here
//!
//! Bytes go ring → base64 → `PtyChunk`. Decoding happens exactly once, in
//! xterm.js. `scripts/lint-forbidden.sh` enforces the absence of the two
//! conversions that would break it.

use std::io::{Read, Write};
use std::os::fd::{BorrowedFd, RawFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU16, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use parking_lot::Mutex;
use portable_pty::{Child, MasterPty, PtySize};
use serde::{Deserialize, Serialize};
use tauri::ipc::Channel;

use crate::agents::AgentKind;
use crate::error::{GildeError, Result};
use crate::events::bus::EventBus;
use crate::events::{self, PtyResyncEvent, SessionExitEvent};
use crate::model::status::{SessionStatus, SessionStatusReport};
use crate::status::evidence::PROMPT_GATE_CONFIDENCE; // T16
use crate::status::StatusEngine; // T16
use crate::store::{dao, now_ms, Db};
use crate::supervise::registry::SpawnGuard;

use super::dump; // PINNED §36
use super::ring::Ring;

// ---------------------------------------------------------------------------
// tuning constants — DESIGN §6-2
// ---------------------------------------------------------------------------

/// One `read()` at a time. Matches the boundary the `binary-flood` fixture
/// stresses on purpose.
pub const READ_CHUNK: usize = 64 * 1024;

/// Coalescer flush cadence.
pub const FLUSH_INTERVAL: Duration = Duration::from_millis(8);

/// Coalescer flush size.
pub const FLUSH_BYTES: usize = 32 * 1024;

/// Frames the reader may queue before it starts dropping them. 64 × 64 KiB is a
/// 4 MiB ceiling per session — big enough to ride out a garbage-collection
/// pause in the webview, small enough that a wedged UI cannot exhaust memory.
pub const QUEUE_FRAMES: usize = 64;

/// How often the coalescer task re-evaluates the session's status while no
/// bytes are flowing.
///
/// T16's [`StatusEngine`] also runs a 500 ms tick of its own — that one carries
/// the `ps` snapshot and is what the OS-truth signal needs. This one exists
/// because it is already awake for the flush timer: it costs a comparison, it
/// keeps a session evaluating even in a test with no ticker running, and
/// evaluating more often than DESIGN's 500 ms is only ever an improvement (the
/// 300 ms debounce, not the tick, is what bounds transitions).
pub const STATUS_TICK: Duration = Duration::from_millis(250);

/// `Standard` close: how long the TUI gets to honour `^C` / `^D` before
/// `kill_tree` takes over (DESIGN §6-5).
pub const STANDARD_CLOSE_GRACE: Duration = Duration::from_secs(2);

/// Terminal size floor. `codex` renders nothing into a 0×0 pty (PINNED §9-4),
/// so a session is never opened smaller than this even if the frontend asks
/// before its first `FitAddon.fit()`.
pub const MIN_COLS: u16 = 20;
pub const MIN_ROWS: u16 = 5;
/// What a request of `0` becomes — the classic default, and a size every CLI
/// is happy to draw into.
pub const FALLBACK_COLS: u16 = 80;
pub const FALLBACK_ROWS: u16 = 24;

/// ETX — what `^C` puts on the wire.
const CTRL_C: u8 = 0x03;
/// EOT — what `^D` puts on the wire.
const CTRL_D: u8 = 0x04;
/// Bracketed paste, DESIGN §6-5.
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

// ---------------------------------------------------------------------------
// wire types — mirrored in `src/lib/ipc.ts` §8
// ---------------------------------------------------------------------------

/// One coalesced run of pty bytes, base64-encoded (DESIGN §6-3).
///
/// `seq` is the ring sequence number of the **first** byte in `b64`, which is
/// what lets a remount drop duplicates after a snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PtyChunk {
    pub seq: u64,
    pub b64: String,
}

/// How a session ends (DESIGN §6-5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CloseMode {
    /// `SIGHUP` to the pty's foreground group — "the terminal window closed".
    Gentle,
    /// `^C`, `^D`, two seconds of patience, then `kill_tree`.
    #[default]
    Standard,
    /// Deliberate orphaning: `keep_alive = 1`, delivery stops, process lives.
    Detach,
}

/// Signals `session_signal` may deliver to the foreground process group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PtySignal {
    Int,
    Hup,
    Term,
}

impl PtySignal {
    fn as_nix(self) -> nix::sys::signal::Signal {
        use nix::sys::signal::Signal;
        match self {
            PtySignal::Int => Signal::SIGINT,
            PtySignal::Hup => Signal::SIGHUP,
            PtySignal::Term => Signal::SIGTERM,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            PtySignal::Int => "int",
            PtySignal::Hup => "hup",
            PtySignal::Term => "term",
        }
    }
}

/// What the UI needs to render one terminal tab.
///
/// `project_id` / `worktree_id` are empty strings for a session that is not
/// attached to one (the fixture sessions the tests open). Every session the
/// product opens goes through `session_open`, which requires both.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PtySessionInfo {
    pub id: String,
    pub spawn_id: String,
    pub project_id: String,
    pub worktree_id: String,
    pub agent: AgentKind,
    pub account_id: Option<String>,
    pub title: String,
    pub cwd: String,
    pub cols: u16,
    pub rows: u16,
    pub pid: i32,
    pub pgid: i32,
    pub opened_at: i64,
    pub detached: bool,
    pub first_retained_seq: u64,
    pub next_seq: u64,
}

/// The answer to `session_send_prompt` — never a bare boolean, because a
/// refusal has to be explainable (DESIGN §6-5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendPromptOutcome {
    pub sent: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused_reason: Option<String>,
    pub bracketed_paste: bool,
}

/// How the child ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExitInfo {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

// ---------------------------------------------------------------------------
// shared state
// ---------------------------------------------------------------------------

/// Everything the reader thread, the coalescer task and the exit watcher touch.
/// Split out of [`PtySession`] so the threads hold an `Arc` of exactly what they
/// need and nothing that would make the session un-droppable.
pub(crate) struct Shared {
    pub(crate) id: String,
    pub(crate) ring: Mutex<Ring>,
    pub(crate) sink: Mutex<Option<Channel<PtyChunk>>>,
    /// Set by the reader when a frame had to be dropped; cleared by the
    /// coalescer when it emits `gilde://pty/resync`.
    pub(crate) resync: AtomicBool,
    /// False once the session is detached: the reader keeps draining the pty
    /// (so the child never blocks on write) but nothing is delivered.
    pub(crate) delivering: AtomicBool,
    pub(crate) eof: AtomicBool,
    pub(crate) exited: AtomicBool,
    pub(crate) exit: Mutex<Option<ExitInfo>>,
    pub(crate) last_io_at: AtomicI64,
    /// T16: the three-source inference engine. Shared by every session in the
    /// app, keyed by [`Shared::id`], so the 500 ms tick and the `ps` snapshot
    /// are paid for once rather than once per terminal tab.
    pub(crate) status: Arc<StatusEngine>,
    pub(crate) events: Option<EventBus>,
}

impl Shared {
    fn emit_resync(&self) {
        let next_seq = self.ring.lock().next_seq();
        tracing::debug!(
            target: "pty",
            session = %self.id,
            next_seq,
            "output outran the ipc channel; asking the view to re-snapshot"
        );
        if let Some(bus) = &self.events {
            bus.emit(
                events::PTY_RESYNC,
                PtyResyncEvent {
                    session_id: self.id.clone(),
                    next_seq,
                },
            );
        }
    }
}

/// One frame as it travels from the reader thread to the coalescer.
struct Frame {
    seq: u64,
    bytes: Vec<u8>,
}

// ---------------------------------------------------------------------------
// the session
// ---------------------------------------------------------------------------

/// A live pty and the child attached to it.
pub struct PtySession {
    pub(crate) shared: Arc<Shared>,
    spawn_id: String,
    project_id: String,
    worktree_id: String,
    agent: AgentKind,
    account_id: Option<String>,
    title: String,
    cwd: PathBuf,
    pid: i32,
    pgid: i32,
    opened_at: i64,
    cols: AtomicU16,
    rows: AtomicU16,
    detached: AtomicBool,
    /// `MasterPty` is `Send` but not `Sync`; the mutex is what makes a session
    /// shareable across the command surface.
    master: Mutex<Box<dyn MasterPty + Send>>,
    /// Kept for `tcgetpgrp` and for the record; valid as long as `master` is.
    master_fd: RawFd,
    writer: Mutex<Box<dyn Write + Send>>,
    /// Held so that `Drop` cannot mark a running row `lost`, and so the exit
    /// watcher can close the row with a code.
    #[allow(dead_code)]
    guard: Arc<SpawnGuard>,
}

impl std::fmt::Debug for PtySession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PtySession")
            .field("id", &self.shared.id)
            .field("spawn_id", &self.spawn_id)
            .field("pid", &self.pid)
            .finish()
    }
}

/// Everything [`PtySession::start`] needs that is not derived from the pty.
pub(crate) struct SessionParts {
    pub(crate) id: String,
    pub(crate) spawn_id: String,
    pub(crate) project_id: String,
    pub(crate) worktree_id: String,
    pub(crate) agent: AgentKind,
    pub(crate) account_id: Option<String>,
    pub(crate) title: String,
    pub(crate) cwd: PathBuf,
    pub(crate) pid: i32,
    pub(crate) pgid: i32,
    pub(crate) cols: u16,
    pub(crate) rows: u16,
    pub(crate) ring_bytes: usize,
    pub(crate) idle_after_ms: u64,
    pub(crate) status: Arc<StatusEngine>, // T16
    pub(crate) guard: Arc<SpawnGuard>,
    pub(crate) events: Option<EventBus>,
    pub(crate) db: Option<Db>,
}

impl PtySession {
    /// Wire up the ring, the three threads and the delivery channel.
    ///
    /// Called by [`super::manager::PtyManager::open`] once the child is running
    /// and the registry row says so.
    pub(crate) fn start(
        parts: SessionParts,
        master: Box<dyn MasterPty + Send>,
        child: Box<dyn Child + Send + Sync>,
        sink: Channel<PtyChunk>,
    ) -> Result<PtySession> {
        let now = now_ms();
        let master_fd = master
            .as_raw_fd()
            .ok_or_else(|| GildeError::pty("the pty master has no file descriptor"))?;
        let reader = master
            .try_clone_reader()
            .map_err(|e| GildeError::pty(format!("cannot read the pty master: {e}")))?;
        let writer = master
            .take_writer()
            .map_err(|e| GildeError::pty(format!("cannot write to the pty master: {e}")))?;

        // PINNED §36: the first record of the dump, and the one that says what
        // size the child was told about before it drew anything.
        dump::marker(
            &parts.id,
            format_args!("open {}x{} {}", parts.cols, parts.rows, parts.agent.signature_key()),
        );

        // T16: registered *before* the reader thread exists, so no byte can
        // arrive before there is somewhere to account for it.
        parts.status.register(
            &parts.id,
            parts.agent.signature_key(),
            parts.idle_after_ms,
            now,
        );

        let shared = Arc::new(Shared {
            id: parts.id.clone(),
            ring: Mutex::new(Ring::new(parts.ring_bytes)),
            sink: Mutex::new(Some(sink)),
            resync: AtomicBool::new(false),
            delivering: AtomicBool::new(true),
            eof: AtomicBool::new(false),
            exited: AtomicBool::new(false),
            exit: Mutex::new(None),
            last_io_at: AtomicI64::new(now),
            status: parts.status.clone(),
            events: parts.events.clone(),
        });

        let (tx, rx) = tokio::sync::mpsc::channel::<Frame>(QUEUE_FRAMES);

        spawn_reader(shared.clone(), reader, tx);
        spawn_coalescer(shared.clone(), rx);
        spawn_exit_watcher(
            shared.clone(),
            child,
            parts.guard.clone(),
            parts.db.clone(),
            parts.pid,
        );

        Ok(PtySession {
            shared,
            spawn_id: parts.spawn_id,
            project_id: parts.project_id,
            worktree_id: parts.worktree_id,
            agent: parts.agent,
            account_id: parts.account_id,
            title: parts.title,
            cwd: parts.cwd,
            pid: parts.pid,
            pgid: parts.pgid,
            opened_at: now,
            cols: AtomicU16::new(parts.cols),
            rows: AtomicU16::new(parts.rows),
            detached: AtomicBool::new(false),
            master: Mutex::new(master),
            master_fd,
            writer: Mutex::new(writer),
            guard: parts.guard,
        })
    }

    // -- identity ------------------------------------------------------------

    pub fn id(&self) -> &str {
        &self.shared.id
    }

    pub fn spawn_id(&self) -> &str {
        &self.spawn_id
    }

    pub fn pid(&self) -> i32 {
        self.pid
    }

    pub fn pgid(&self) -> i32 {
        self.pgid
    }

    pub fn agent(&self) -> AgentKind {
        self.agent
    }

    pub fn worktree_id(&self) -> &str {
        &self.worktree_id
    }

    pub fn cwd(&self) -> &std::path::Path {
        &self.cwd
    }

    pub fn is_detached(&self) -> bool {
        self.detached.load(Ordering::Acquire)
    }

    /// True once the reader has seen EOF **and** the child has been reaped.
    /// Either alone is not enough — DESIGN §6-5 makes them independent.
    pub fn is_finished(&self) -> bool {
        self.shared.eof.load(Ordering::Acquire) && self.shared.exited.load(Ordering::Acquire)
    }

    pub fn saw_eof(&self) -> bool {
        self.shared.eof.load(Ordering::Acquire)
    }

    pub fn has_exited(&self) -> bool {
        self.shared.exited.load(Ordering::Acquire)
    }

    pub fn exit_info(&self) -> Option<ExitInfo> {
        *self.shared.exit.lock()
    }

    pub fn last_io_at(&self) -> i64 {
        self.shared.last_io_at.load(Ordering::Relaxed)
    }

    pub fn info(&self) -> PtySessionInfo {
        let ring = self.shared.ring.lock();
        PtySessionInfo {
            id: self.shared.id.clone(),
            spawn_id: self.spawn_id.clone(),
            project_id: self.project_id.clone(),
            worktree_id: self.worktree_id.clone(),
            agent: self.agent,
            account_id: self.account_id.clone(),
            title: self.title.clone(),
            cwd: self.cwd.to_string_lossy().into_owned(),
            cols: self.cols.load(Ordering::Relaxed),
            rows: self.rows.load(Ordering::Relaxed),
            pid: self.pid,
            pgid: self.pgid,
            opened_at: self.opened_at,
            detached: self.detached.load(Ordering::Acquire),
            first_retained_seq: ring.first_retained_seq(),
            next_seq: ring.next_seq(),
        }
    }

    /// T16: the engine re-evaluates before answering, so the prompt gate and
    /// the Inspector never decide on a reading from before the last tick.
    pub fn status_report(&self) -> SessionStatusReport {
        self.shared.status.report(&self.shared.id)
    }

    /// True while a BEL has not been acknowledged (DESIGN §6-4).
    pub fn attention(&self) -> bool {
        self.shared.status.attention(&self.shared.id)
    }

    // -- the io surface ------------------------------------------------------

    /// Swap in the channel of a freshly mounted view (DESIGN §6-3 step 1).
    ///
    /// The channel is replaced **before** the caller takes a snapshot, which is
    /// what closes the gap that the opposite order would open.
    pub fn attach(&self, sink: Channel<PtyChunk>) {
        *self.shared.sink.lock() = Some(sink);
        self.shared.delivering.store(
            !self.detached.load(Ordering::Acquire),
            Ordering::Release,
        );
        // A fresh view has an empty screen; whatever the old one missed is
        // irrelevant, and the snapshot it is about to take is authoritative.
        self.shared.resync.store(false, Ordering::Release);
    }

    /// 16-byte header + the retained bytes (DESIGN §6-3).
    pub fn snapshot(&self) -> Vec<u8> {
        self.shared.ring.lock().snapshot()
    }

    pub fn next_seq(&self) -> u64 {
        self.shared.ring.lock().next_seq()
    }

    /// Write raw bytes to the child. Never text: the frontend encodes with
    /// `TextEncoder` so IME composition and pasted emoji survive intact.
    pub fn write_bytes(&self, bytes: &[u8]) -> Result<()> {
        // PINNED §36: the other half of the dialogue. xterm.js answers the
        // child's terminal queries on its own, and a recording of the child's
        // side alone cannot show which answer provoked which repaint.
        dump::input(&self.shared.id, bytes);
        let mut writer = self.writer.lock();
        writer
            .write_all(bytes)
            .and_then(|()| writer.flush())
            .map_err(|e| GildeError::pty(format!("write to the pty failed: {e}")))?;
        self.shared
            .last_io_at
            .store(now_ms(), Ordering::Relaxed);
        // T16: what the *user* types is activity too — T12's idle timeout reads
        // `pty_session.last_io_at`, and a session being typed into is not idle.
        self.shared.status.on_input(&self.shared.id);
        Ok(())
    }

    /// `TIOCSWINSZ`, which makes the kernel deliver `SIGWINCH` to the
    /// foreground group. A no-op when the size did not actually change.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<bool> {
        let (cols, rows) = sanitize_size(cols, rows);
        if self.cols.load(Ordering::Relaxed) == cols && self.rows.load(Ordering::Relaxed) == rows {
            return Ok(false);
        }
        self.master
            .lock()
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| GildeError::pty(format!("resize failed: {e}")))?;
        self.cols.store(cols, Ordering::Relaxed);
        self.rows.store(rows, Ordering::Relaxed);
        dump::marker(&self.shared.id, format_args!("resize {cols}x{rows}"));
        // T16 / PINNED §36: the child is about to get `SIGWINCH` and redraw
        // everything. That repaint is ours, not evidence about the agent.
        self.shared.status.on_resize(&self.shared.id, cols, rows);
        Ok(true)
    }

    pub fn size(&self) -> (u16, u16) {
        (
            self.cols.load(Ordering::Relaxed),
            self.rows.load(Ordering::Relaxed),
        )
    }

    /// The winsize the **kernel** holds for this tty (`TIOCGWINSZ`), which is
    /// what the child reads and what `SIGWINCH` announced. [`PtySession::size`]
    /// is our own record of it; when the two disagree, this one is the truth.
    pub fn tty_size(&self) -> Option<(u16, u16)> {
        self.master
            .lock()
            .get_size()
            .ok()
            .map(|size| (size.cols, size.rows))
    }

    /// The pty's current foreground process group — `tcgetpgrp(master_fd)`,
    /// which `portable-pty` exposes as `process_group_leader()` (PINNED §2).
    ///
    /// This is *not* the child's own pgid: when the agent runs `less`, the
    /// foreground group is `less`. Both `Gentle` close and `session_signal`
    /// deliberately target this group, which is what a real terminal does.
    pub fn foreground_pgid(&self) -> Option<i32> {
        if let Some(pgid) = self.master.lock().process_group_leader() {
            if pgid > 0 {
                return Some(pgid);
            }
        }
        // The version-independent path DESIGN §6-4 asks for, kept as a fallback
        // even though PINNED §2 confirms `process_group_leader()` exists in
        // portable-pty 0.9: the two are the same `tcgetpgrp` call, and having
        // ours means a future crate bump cannot silently remove the signal.
        nix::unistd::tcgetpgrp(self.master_fd())
            .ok()
            .map(|pid| pid.as_raw())
            .filter(|pgid| *pgid > 0)
    }

    /// Deliver `sig` to the foreground process group.
    ///
    /// Refuses any group that is not plausibly ours: `pgid <= 1` and this
    /// process's own group are never signalled (DESIGN §8-6's exclusion rules,
    /// applied to the one path that does not go through `kill_tree`).
    pub fn signal_foreground(&self, sig: PtySignal) -> Result<i32> {
        let pgid = self
            .foreground_pgid()
            .ok_or_else(|| GildeError::pty("the pty has no foreground process group"))?;
        self.guard_pgid(pgid)?;
        nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pgid), sig.as_nix())
            .map_err(|e| GildeError::pty(format!("killpg({pgid}, {}) failed: {e}", sig.as_str())))?;
        tracing::info!(
            target: "pty",
            session = %self.shared.id,
            pgid,
            signal = sig.as_str(),
            "signalled the foreground process group"
        );
        Ok(pgid)
    }

    fn guard_pgid(&self, pgid: i32) -> Result<()> {
        if pgid <= 1 {
            return Err(GildeError::refused("init"));
        }
        if Some(pgid) == crate::supervise::spawn::pgid_of(std::process::id() as i32) {
            return Err(GildeError::refused("self"));
        }
        Ok(())
    }

    /// Type a prompt into a live TUI (DESIGN §6-5).
    ///
    /// The gate is deliberate: typing a paragraph into a `[y/N]` dialog answers
    /// the dialog with garbage. Multi-line text is wrapped in bracketed paste so
    /// the TUI submits once, at the end, instead of on the first newline.
    pub fn send_prompt(&self, text: &str) -> SendPromptOutcome {
        let report = self.status_report();
        if !matches!(report.status, SessionStatus::Idle) {
            return SendPromptOutcome {
                sent: false,
                refused_reason: Some(format!("status:{}", report.status.kind())),
                bracketed_paste: false,
            };
        }
        if report.confidence < PROMPT_GATE_CONFIDENCE {
            return SendPromptOutcome {
                sent: false,
                refused_reason: Some("lowConfidence".to_string()),
                bracketed_paste: false,
            };
        }

        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        let trimmed = normalized.trim_end_matches('\n');
        // Whitespace is not a prompt: submitting it costs the user a turn and
        // teaches the agent nothing, so it is refused like an empty string.
        if trimmed.trim().is_empty() {
            return SendPromptOutcome {
                sent: false,
                refused_reason: Some("empty".to_string()),
                bracketed_paste: false,
            };
        }
        let bracketed = trimmed.contains('\n');

        let mut payload: Vec<u8> = Vec::with_capacity(trimmed.len() + 16);
        if bracketed {
            payload.extend_from_slice(PASTE_START);
            payload.extend_from_slice(trimmed.as_bytes());
            payload.extend_from_slice(PASTE_END);
        } else {
            payload.extend_from_slice(trimmed.as_bytes());
        }
        payload.push(b'\r');

        match self.write_bytes(&payload) {
            Ok(()) => SendPromptOutcome {
                sent: true,
                refused_reason: None,
                bracketed_paste: bracketed,
            },
            Err(err) => SendPromptOutcome {
                sent: false,
                refused_reason: Some(format!("write:{}", err.code())),
                bracketed_paste: bracketed,
            },
        }
    }

    // -- closing -------------------------------------------------------------

    /// `^C` then `^D`, as a real user would (`Standard` close, step one).
    pub(crate) fn interrupt_and_eof(&self) -> Result<()> {
        self.write_bytes(&[CTRL_C])?;
        self.write_bytes(&[CTRL_D])
    }

    /// Wait up to `budget` for the child to disappear. Returns `true` if it did.
    pub(crate) fn wait_for_exit(&self, budget: Duration) -> bool {
        let deadline = Instant::now() + budget;
        loop {
            if self.shared.exited.load(Ordering::Acquire) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Stop delivering, keep the pty open (DESIGN §6-5 `Detach`).
    ///
    /// The master fd stays open on purpose: closing it hangs up the tty and the
    /// kernel sends `SIGHUP` to the very process we promised to keep alive. The
    /// reader thread keeps draining and discarding, because a detached agent
    /// whose pty buffer fills would block on its next `write` — "detached" must
    /// mean "we stopped watching", never "we froze it".
    pub(crate) fn detach(&self) {
        self.detached.store(true, Ordering::Release);
        self.shared.delivering.store(false, Ordering::Release);
        *self.shared.sink.lock() = None;
        self.shared.status.on_detach(&self.shared.id);
    }

    pub(crate) fn mark_killed(&self) {
        self.shared.status.on_killed(&self.shared.id);
    }

    /// Stop delivering to a channel that is about to go away.
    pub(crate) fn stop_delivery(&self) {
        self.shared.delivering.store(false, Ordering::Release);
        *self.shared.sink.lock() = None;
    }

    /// The raw master descriptor, borrowed for the life of this session.
    fn master_fd(&self) -> BorrowedFd<'_> {
        // SAFETY: `self.master` owns the descriptor and outlives the borrow.
        unsafe { BorrowedFd::borrow_raw(self.master_fd) }
    }
}

/// Clamp a requested terminal size into something a TUI can draw into.
pub fn sanitize_size(cols: u16, rows: u16) -> (u16, u16) {
    let cols = if cols == 0 { FALLBACK_COLS } else { cols.max(MIN_COLS) };
    let rows = if rows == 0 { FALLBACK_ROWS } else { rows.max(MIN_ROWS) };
    (cols, rows)
}

// ---------------------------------------------------------------------------
// the three threads
// ---------------------------------------------------------------------------

fn spawn_reader(
    shared: Arc<Shared>,
    mut reader: Box<dyn Read + Send>,
    tx: tokio::sync::mpsc::Sender<Frame>,
) {
    let name = format!("gilde-pty-read-{}", short(&shared.id));
    let spawned = std::thread::Builder::new().name(name).spawn(move || {
        let mut buf = vec![0u8; READ_CHUNK];
        loop {
            match reader.read(&mut buf) {
                // `portable-pty` maps the master's `EIO` (all slaves closed) to
                // a clean EOF, so this is the one and only end condition.
                Ok(0) => break,
                Ok(n) => {
                    let now = now_ms();
                    shared.last_io_at.store(now, Ordering::Relaxed);
                    // PINNED §36: before anything interprets them.
                    dump::output(&shared.id, &buf[..n]);
                    let seq = shared.ring.lock().push(&buf[..n]);
                    // T16: the tail parser and the state machine, on the bytes
                    // themselves. Output just arrived, so this returns on the
                    // first branch of DESIGN §6-4's table — the regexes are not
                    // on the hot path.
                    shared.status.on_output(&shared.id, &buf[..n]);

                    if shared.delivering.load(Ordering::Acquire) {
                        use tokio::sync::mpsc::error::TrySendError;
                        match tx.try_send(Frame {
                            seq,
                            bytes: buf[..n].to_vec(),
                        }) {
                            Ok(()) => {}
                            // The view is behind. Drop the frame, remember that
                            // we did, and keep reading — the child must never
                            // wait for a webview.
                            Err(TrySendError::Full(_)) => {
                                shared.resync.store(true, Ordering::Release);
                            }
                            // No coalescer any more; the ring is still the truth.
                            Err(TrySendError::Closed(_)) => {}
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    tracing::debug!(
                        target: "pty",
                        session = %shared.id,
                        error = %e,
                        "pty read ended"
                    );
                    break;
                }
            }
        }
        shared.eof.store(true, Ordering::Release);
        dump::marker(&shared.id, format_args!("eof"));
        dump::close(&shared.id);
        tracing::debug!(target: "pty", session = %shared.id, "reader saw eof");
    });
    if let Err(err) = spawned {
        tracing::error!(target: "pty", error = %err, "could not start the pty reader thread");
    }
}

fn spawn_coalescer(shared: Arc<Shared>, mut rx: tokio::sync::mpsc::Receiver<Frame>) {
    tauri::async_runtime::spawn(async move {
        let mut pending: Vec<u8> = Vec::with_capacity(FLUSH_BYTES);
        let mut pending_seq: u64 = 0;
        let mut flush_at: Option<Instant> = None;
        let mut tick_at = Instant::now() + STATUS_TICK;
        // Where the last chunk we actually delivered ended. The contract with
        // the view is that consecutive chunks join up unless a resync was
        // published between them, and this is what proves it.
        let mut delivered_end: Option<u64> = None;

        loop {
            let now = Instant::now();
            let wait = flush_at
                .unwrap_or(tick_at)
                .min(tick_at)
                .saturating_duration_since(now)
                .max(Duration::from_millis(1));

            match tokio::time::timeout(wait, rx.recv()).await {
                Ok(Some(frame)) => {
                    // A hole is discovered *here*, by comparing sequence
                    // numbers, and never by reading the reader's `resync` flag:
                    // that flag goes up the instant a frame is dropped, which is
                    // up to `QUEUE_FRAMES` frames before the hole itself reaches
                    // this loop. Trusting it would let a batch straddle the hole
                    // and go out stamped with the seq of its first frame — a
                    // chunk whose bytes are not the bytes at that offset, which
                    // is the one thing the seq protocol exists to prevent.
                    let joins_up = if pending.is_empty() {
                        delivered_end
                    } else {
                        Some(pending_seq + pending.len() as u64)
                    }
                    .is_none_or(|end| end == frame.seq);
                    if !joins_up {
                        // What we hold is still valid on its own and still joins
                        // up with the chunk before it, so send it — then tell the
                        // view to re-snapshot, before anything from the far side
                        // of the hole reaches it.
                        flush(&shared, &mut pending, pending_seq, &mut delivered_end);
                        announce_resync(&shared);
                        flush_at = None;
                    }
                    if pending.is_empty() {
                        pending_seq = frame.seq;
                        flush_at = Some(Instant::now() + FLUSH_INTERVAL);
                    }
                    pending.extend_from_slice(&frame.bytes);
                    if pending.len() >= FLUSH_BYTES {
                        flush(&shared, &mut pending, pending_seq, &mut delivered_end);
                        flush_at = None;
                    }
                }
                // Every sender is gone: the reader thread ended.
                Ok(None) => {
                    flush(&shared, &mut pending, pending_seq, &mut delivered_end);
                    break;
                }
                Err(_elapsed) => {
                    let now = Instant::now();
                    if flush_at.map(|at| at <= now).unwrap_or(false) {
                        flush(&shared, &mut pending, pending_seq, &mut delivered_end);
                        flush_at = None;
                    }
                    if now >= tick_at {
                        tick_at = now + STATUS_TICK;
                        // A drop that nothing has arrived after — the flood
                        // ended on the far side of the hole — would otherwise
                        // never be announced.
                        if pending.is_empty() && shared.resync.load(Ordering::Acquire) {
                            announce_resync(&shared);
                        }
                        shared.status.tick_session(&shared.id);
                    }
                }
            }
        }

        // The stream ended with a hole in it: the last thing the view hears has
        // to be "re-snapshot", or it keeps a screen that is missing bytes.
        if shared.resync.load(Ordering::Acquire) {
            announce_resync(&shared);
        }

        // One last evaluation so the pill does not freeze on "working" after the
        // stream ends but before `child.wait()` returns.
        shared.status.tick_session(&shared.id);
    });
}

/// Publish `gilde://pty/resync` and lower the reader's flag.
///
/// Publishing twice for one hole is harmless — a re-snapshot is idempotent —
/// but publishing *late* is not, so this is called the moment a hole is proven
/// rather than on a timer.
fn announce_resync(shared: &Shared) {
    shared.resync.store(false, Ordering::Release);
    shared.emit_resync();
}

/// Send one coalesced chunk. `seq` is the ring offset of `pending[0]`, and the
/// caller guarantees `pending` holds one unbroken run of bytes from there.
/// `delivered_end` is advanced to the offset just past the chunk.
fn flush(shared: &Shared, pending: &mut Vec<u8>, seq: u64, delivered_end: &mut Option<u64>) {
    if pending.is_empty() {
        return;
    }
    *delivered_end = Some(seq + pending.len() as u64);
    let b64 = base64::engine::general_purpose::STANDARD.encode(&pending[..]);
    pending.clear();
    let sink = shared.sink.lock();
    if let Some(channel) = sink.as_ref() {
        if let Err(err) = channel.send(PtyChunk { seq, b64 }) {
            tracing::debug!(
                target: "pty",
                session = %shared.id,
                error = %err,
                "chunk delivery failed; the view will resync on its next mount"
            );
        }
    }
}

fn spawn_exit_watcher(
    shared: Arc<Shared>,
    mut child: Box<dyn Child + Send + Sync>,
    guard: Arc<SpawnGuard>,
    db: Option<Db>,
    pid: i32,
) {
    let name = format!("gilde-pty-wait-{}", short(&shared.id));
    let spawned = std::thread::Builder::new().name(name).spawn(move || {
        let exit = reap(&mut child);
        let at = now_ms();

        *shared.exit.lock() = Some(exit);
        shared.exited.store(true, Ordering::Release);
        // T16: terminal, immediately — an exit never waits out the debounce.
        shared.status.on_exit(&shared.id, exit.code, exit.signal);

        if let Err(err) = guard.mark_exited(exit.code, exit.signal) {
            tracing::warn!(target: "pty", error = %err, "could not close the registry row");
        }
        if let Some(db) = &db {
            let id = shared.id.clone();
            if let Err(err) = db.blocking(|conn| {
                dao::pty_session::mark_closed(
                    conn,
                    &id,
                    at,
                    exit.code.map(i64::from),
                    exit.signal.map(i64::from),
                )
                .map(|_| ())
            }) {
                tracing::warn!(target: "pty", error = %err, "could not close the pty_session row");
            }
        }

        tracing::info!(
            target: "pty",
            session = %shared.id,
            pid,
            code = ?exit.code,
            signal = ?exit.signal,
            "pty child exited"
        );

        if let Some(bus) = &shared.events {
            bus.emit(
                events::SESSION_EXIT,
                SessionExitEvent {
                    session_id: shared.id.clone(),
                    pid,
                    exit_code: exit.code,
                    signal: exit.signal,
                    at,
                },
            );
        }
    });
    if let Err(err) = spawned {
        tracing::error!(target: "pty", error = %err, "could not start the pty exit watcher");
    }
}

/// `waitpid`, through whichever door gives us the **numeric** signal.
///
/// `portable_pty::ExitStatus` reports a signal as `strsignal()` prose
/// ("Hangup"), which is useless to the UI and unstable across platforms. On unix
/// the boxed child *is* a `std::process::Child`, so we downcast and use
/// `ExitStatusExt::signal()`; the fall-back keeps the code honest on any future
/// backend rather than pretending a signal was an exit code.
fn reap(child: &mut Box<dyn Child + Send + Sync>) -> ExitInfo {
    let generic: &mut dyn Child = &mut **child;
    if let Some(std_child) = generic.downcast_mut::<std::process::Child>() {
        return match std_child.wait() {
            Ok(status) => ExitInfo {
                code: status.code(),
                signal: crate::supervise::spawn::exit_signal_of(&status),
            },
            Err(err) => {
                tracing::warn!(target: "pty", error = %err, "waitpid failed");
                ExitInfo::default()
            }
        };
    }
    match child.wait() {
        Ok(status) => ExitInfo {
            code: Some(status.exit_code() as i32),
            signal: None,
        },
        Err(err) => {
            tracing::warn!(target: "pty", error = %err, "child wait failed");
            ExitInfo::default()
        }
    }
}

/// Thread names are capped at 15 bytes on macOS; a uuid is longer than that.
fn short(id: &str) -> &str {
    &id[..id.len().min(8)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zero_size_becomes_a_size_a_tui_can_draw_into() {
        assert_eq!(sanitize_size(0, 0), (FALLBACK_COLS, FALLBACK_ROWS));
        assert_eq!(sanitize_size(1, 1), (MIN_COLS, MIN_ROWS));
        assert_eq!(sanitize_size(120, 40), (120, 40));
    }

    /// The status stub T15 carried is gone: T16's `StatusEngine` owns every
    /// transition now, and its state machine is tested where it lives
    /// (`status::engine`). What is still this module's business is that the
    /// session hands the engine the right things — proven end-to-end against a
    /// real pty in `tests/status_streams.rs` and `tests/pty_fidelity.rs`.

    /// Regression: a batch must never straddle a dropped frame.
    ///
    /// The reader raises `resync` the instant it drops a frame, which can be up
    /// to `QUEUE_FRAMES` frames before the hole reaches the coalescer. An
    /// earlier version consumed that flag in `flush`, so the flag was already
    /// spent by the time the hole arrived and the two sides of it were
    /// concatenated into one chunk stamped with the seq of the first — a chunk
    /// whose bytes are not the bytes at that offset. Only the sequence numbers
    /// can prove contiguity, so only they are trusted.
    #[tokio::test]
    async fn a_dropped_frame_never_lets_a_chunk_lie_about_its_offset() {
        let received: Arc<std::sync::Mutex<Vec<(u64, Vec<u8>)>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = received.clone();
        let channel = Channel::new(move |body| {
            let json = match body {
                tauri::ipc::InvokeResponseBody::Json(json) => json,
                tauri::ipc::InvokeResponseBody::Raw(_) => unreachable!("chunks are json"),
            };
            let chunk: PtyChunk = serde_json::from_str(&json).expect("chunk");
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(chunk.b64.as_bytes())
                .expect("b64");
            sink.lock().expect("sink").push((chunk.seq, bytes));
            Ok(())
        });

        let shared = Arc::new(Shared {
            id: "unit".to_string(),
            ring: Mutex::new(Ring::new(1024)),
            sink: Mutex::new(Some(channel)),
            resync: AtomicBool::new(false),
            delivering: AtomicBool::new(true),
            eof: AtomicBool::new(false),
            exited: AtomicBool::new(false),
            exit: Mutex::new(None),
            last_io_at: AtomicI64::new(0),
            status: Arc::new(StatusEngine::new()),
            events: None,
        });

        let (tx, rx) = tokio::sync::mpsc::channel::<Frame>(8);
        spawn_coalescer(shared.clone(), rx);

        tx.send(Frame {
            seq: 0,
            bytes: b"aaaa".to_vec(),
        })
        .await
        .expect("send");
        // Frames covering 4..8 were dropped by the reader, which is the only
        // party that knows it happened.
        shared.resync.store(true, Ordering::Release);
        tokio::time::sleep(Duration::from_millis(40)).await;
        tx.send(Frame {
            seq: 8,
            bytes: b"cccc".to_vec(),
        })
        .await
        .expect("send");
        tokio::time::sleep(Duration::from_millis(80)).await;

        let chunks = received.lock().expect("sink").clone();
        assert_eq!(
            chunks,
            vec![(0, b"aaaa".to_vec()), (8, b"cccc".to_vec())],
            "each chunk carries the offset its own bytes came from"
        );
        assert!(
            !shared.resync.load(Ordering::Acquire),
            "the hole was announced, so the flag is spent"
        );
    }

}
