//! One live PTY: the reader, the coalescer, the exit watcher, the one attached
//! view, and the write lock (contract §6.2.3, §6.2.6, §6.2.7).
//!
//! ```text
//!   reader (OS thread)   ring.push → status.on_output → view? try_send : replies
//!        │                                     (DA1/DSR answered only without a view)
//!        ▼
//!   coalescer (task)  ── 8 ms / 32 KiB ──► the attached view's Channel<PtyChunk>
//!
//!   exit watcher (OS thread) ── waitpid → drain → close the held slave → EOF
//!                               ──► status, registry, PtyExitSink
//! ```
//!
//! **A child's last output is never lost (A52.1).** On macOS the last close
//! of a pty's slave waits at most ~0.6 s for the master to read what is still
//! queued, then discards it (measured on Darwin 23.6, T49). The core
//! therefore holds its own slave descriptor ([`open_held_slave`], from
//! before the fork) for the whole life of the child, so no close by the
//! child, and no close of the spawn-time slave, is ever that last one. A
//! child that leads its terminal's session (every PTY child does) cannot
//! even finish exiting until its output has been read, and its exit then
//! revokes the terminal. After `waitpid` the exit watcher waits until the
//! master holds nothing its reader has not taken, one check after each of
//! the reader's reads, and then closes the held slave. Both waits sleep on
//! [`Shared::progress`]; nothing polls on a timer. Exit is reported after
//! EOF, or when [`EXIT_EOF_GRACE`] runs out, whichever comes first. The
//! reader always reads (it never waits on a view), so a child's exit is
//! never stuck waiting for a drain that cannot happen.
//!
//! **A slow or wedged view never applies back-pressure to the agent.** The
//! reader pushes into the ring (which evicts, never waits) and then *tries* to
//! hand the frame on. A full queue drops it and marks `resync`; the view gets
//! `Resync{offset}` and re-snapshots — a visual hiccup, never a stalled agent.
//!
//! **No byte is decoded here.** Bytes go ring → base64 → `PtyChunk::Data`;
//! xterm.js decodes them exactly once.
//!
//! **No periodic wakeups.** The coalescer sleeps on its queue and only arms a
//! timer while it holds bytes to flush (§11.2 rule 1); status evaluation is
//! the status engine's ticker's job.

use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU16, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use parking_lot::{Condvar, Mutex};
use portable_pty::{Child, MasterPty, PtySize};

use crate::contract::activity::ExitInfo;
use crate::contract::content::{DetachReason, PtyChunk};
use crate::contract::ids::{PtyId, ViewId};
use crate::contract::ipc::PtyInfo;
use crate::contract::process::ProcKind;
use crate::contract::services::SpawnTicket;
use crate::contract::IpcError;
use crate::status::StatusEngine;
use crate::store::now_ms;

use super::dump;
use super::ring::Ring;

// ---------------------------------------------------------------------------
// tuning constants (R1 §3, kept)
// ---------------------------------------------------------------------------

/// One `read()` at a time.
pub const READ_CHUNK: usize = 64 * 1024;
/// Coalescer flush cadence.
pub const FLUSH_INTERVAL: Duration = Duration::from_millis(8);
/// Coalescer flush size.
pub const FLUSH_BYTES: usize = 32 * 1024;
/// Frames the reader may queue before it starts dropping them.
pub const QUEUE_FRAMES: usize = 64;

/// Terminal size bounds (§4.2.4 `pty_resize` clamp).
pub const MIN_COLS: u16 = 20;
pub const MIN_ROWS: u16 = 5;
pub const MAX_COLS: u16 = 1000;
pub const MAX_ROWS: u16 = 500;
/// What a size of `0` becomes (§6.2.1: "else 120×40").
pub const FALLBACK_COLS: u16 = 120;
pub const FALLBACK_ROWS: u16 = 40;

/// How long after `waitpid` the exit watcher waits for the master to drain
/// and the reader's EOF before reporting an exit anyway (a grandchild may
/// hold the slave open; A52.1).
pub const EXIT_EOF_GRACE: Duration = Duration::from_secs(2);

/// Bracketed paste markers (§6.2.3).
pub const PASTE_START: &[u8] = b"\x1b[200~";
pub const PASTE_END: &[u8] = b"\x1b[201~";

/// Clamp a requested size (never rejected, §4.2.4).
pub fn sanitize_size(cols: u16, rows: u16) -> (u16, u16) {
    let cols = if cols == 0 { FALLBACK_COLS } else { cols.clamp(MIN_COLS, MAX_COLS) };
    let rows = if rows == 0 { FALLBACK_ROWS } else { rows.clamp(MIN_ROWS, MAX_ROWS) };
    (cols, rows)
}

/// The bytes of a bracketed paste of `text`, then `\r` iff `submit` (§6.2.3).
///
/// The end marker can never be injected from content: the text is split at
/// every `ESC[201~` it contains and each piece goes into its own bracketed
/// segment, so the marker bytes from content are never sent and no piece can
/// close a paste early.
pub fn paste_bytes(text: &str, submit: bool) -> Vec<u8> {
    let end = std::str::from_utf8(PASTE_END).expect("ascii");
    let mut out = Vec::with_capacity(text.len() + 16);
    for piece in text.split(end) {
        if piece.is_empty() {
            continue;
        }
        out.extend_from_slice(PASTE_START);
        out.extend_from_slice(piece.as_bytes());
        out.extend_from_slice(PASTE_END);
    }
    if submit {
        out.push(b'\r');
    }
    out
}

// ---------------------------------------------------------------------------
// the view
// ---------------------------------------------------------------------------

/// Where a view's chunks go. `false` = the view is gone.
pub trait ChunkSink: Send + Sync {
    fn send(&self, chunk: PtyChunk) -> bool;
}

impl ChunkSink for tauri::ipc::Channel<PtyChunk> {
    fn send(&self, chunk: PtyChunk) -> bool {
        tauri::ipc::Channel::send(self, chunk).is_ok()
    }
}

struct View {
    id: ViewId,
    sink: Arc<dyn ChunkSink>,
}

// ---------------------------------------------------------------------------
// shared state
// ---------------------------------------------------------------------------

/// What the reader thread, the coalescer and the exit watcher share.
pub(crate) struct Shared {
    pub(crate) id: String,
    pub(crate) ring: Mutex<Ring>,
    view: Mutex<Option<View>>,
    /// Mirrors `view.is_some()` for the reader's hot path.
    delivering: AtomicBool,
    /// Set by the reader when a frame had to be dropped.
    resync: AtomicBool,
    pub(crate) eof: AtomicBool,
    pub(crate) exited: AtomicBool,
    pub(crate) exit: Mutex<Option<ExitInfo>>,
    pub(crate) last_output_at: AtomicI64,
    pub(crate) status: Arc<StatusEngine>,
    /// Woken on every chunk of output (screen actions wait on it).
    pub(crate) output: tokio::sync::Notify,
    /// Woken once when the child was reaped.
    pub(crate) exit_notify: tokio::sync::Notify,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    /// Signalled after every read the reader pushed into the ring, at its
    /// EOF and when the child was reaped. Each of those changes is made
    /// under the ring lock, so a thread that checks the ring's offset, `eof`
    /// or `reaped` under that lock and then waits misses no wakeup (A52.1).
    pub(crate) progress: Condvar,
    /// `waitpid` returned (set before the drain; `exited` is set after it).
    pub(crate) reaped: AtomicBool,
    /// A52.1: the core's own descriptor on the pty's slave (`O_CLOEXEC`,
    /// `O_NOCTTY`), held until the child is reaped and the master drained.
    /// `None` once closed ([`Shared::close_held_slave`]).
    held_slave: Mutex<Option<OwnedFd>>,
    /// Test hook (T49): hold the reader back until the child is reaped, or
    /// this long.
    #[cfg(any(test, feature = "test-fixtures"))]
    hold_reader: Option<Duration>,
    /// Test hook: what released the held-back reader (0 = not held yet,
    /// 1 = the reap, 2 = the ceiling).
    #[cfg(any(test, feature = "test-fixtures"))]
    hold_outcome: std::sync::atomic::AtomicU8,
}

impl Shared {
    /// Close the core's slave descriptor (A52.1). Idempotent. The close runs
    /// outside the lock.
    pub(crate) fn close_held_slave(&self) {
        let held = self.held_slave.lock().take();
        drop(held);
    }

    #[cfg(any(test, feature = "test-fixtures"))]
    fn held_slave_fd(&self) -> Option<RawFd> {
        self.held_slave.lock().as_ref().map(|fd| fd.as_raw_fd())
    }

    fn send_to_view(&self, chunk: PtyChunk) {
        let view = self.view.lock();
        if let Some(view) = view.as_ref() {
            if !view.sink.send(chunk) {
                tracing::debug!(target: "pty", session = %self.id, "chunk delivery failed; the view resyncs on its next attach");
            }
        }
    }

    fn announce_resync(&self) {
        self.resync.store(false, Ordering::Release);
        let offset = self.ring.lock().next_seq();
        self.send_to_view(PtyChunk::Resync { offset });
    }
}

enum Msg {
    Data { offset: u64, bytes: Vec<u8> },
    /// Terminal-query replies for a PTY with no view (§6.2.6).
    Reply(Vec<u8>),
}

/// Everything [`PtySession::start`] needs besides the pty itself.
pub(crate) struct SessionParts {
    pub(crate) pty_id: PtyId,
    pub(crate) kind: ProcKind,
    pub(crate) ticket: SpawnTicket,
    pub(crate) agent_key: String,
    pub(crate) cwd: PathBuf,
    pub(crate) pid: i32,
    pub(crate) pgid: i32,
    pub(crate) start_sec: Option<i64>,
    pub(crate) cols: u16,
    pub(crate) rows: u16,
    pub(crate) ring_bytes: usize,
    pub(crate) idle_after_ms: u64,
    pub(crate) status: Arc<StatusEngine>,
    /// Called once on exit, from the exit watcher thread.
    pub(crate) on_exit: Box<dyn FnOnce(ExitInfo, i64) + Send>,
    /// A52.1: the core's own slave descriptor ([`open_held_slave`]), opened
    /// before the fork. The session closes it; dropping these parts closes
    /// it too (a start that fails).
    pub(crate) held_slave: OwnedFd,
    /// Test hook (T49): the reader waits until the child is reaped, or this
    /// long, before its first read.
    #[cfg(any(test, feature = "test-fixtures"))]
    pub(crate) hold_reader: Option<Duration>,
}

/// What released a reader the T49 hook held back.
#[cfg(any(test, feature = "test-fixtures"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaderHold {
    /// The child was reaped first.
    Reaped,
    /// The ceiling ran out first: the child could not be reaped while its
    /// output was unread.
    Ceiling,
}

/// A52.1: a second descriptor on the slave named `tty` (the master's
/// `tty_name`), for the core to hold. `std` opens it `O_CLOEXEC` atomically,
/// so no spawn inherits it, not even one forked by another thread meanwhile
/// (a PTY child's `pre_exec` closes every descriptor above 2 anyway).
/// `O_NOCTTY`: the core never takes the terminal as its own.
///
/// It is a fresh open of the device, not a `dup` of portable-pty's slave:
/// `SlavePty` exposes no descriptor, and that one only gets `FD_CLOEXEC`
/// set by an `fcntl` after `openpty` returns.
pub(crate) fn open_held_slave(tty: &Path) -> std::io::Result<OwnedFd> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(tty)?;
    Ok(OwnedFd::from(file))
}

/// A `FD_CLOEXEC` duplicate of `fd`.
fn dup_cloexec(fd: RawFd) -> std::io::Result<OwnedFd> {
    // SAFETY: `fcntl(F_DUPFD_CLOEXEC)` only reads `fd`; a non-negative
    // result is a new descriptor that nothing else owns.
    let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if dup < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: see above.
    Ok(unsafe { OwnedFd::from_raw_fd(dup) })
}

/// Whether a read on the master would return at once: output its reader
/// has not taken, or an EOF (hang-up) it has not seen yet.
///
/// `ioctl(FIONREAD)` cannot answer this on macOS. On a pty master it reports
/// 0 even with output queued for the master (measured on Darwin 23.6, T49),
/// so the drain asks `poll`.
fn master_readable(master: BorrowedFd<'_>) -> bool {
    let mut fds = [libc::pollfd {
        fd: master.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    }];
    loop {
        // SAFETY: one valid `pollfd`, a zero timeout; `master` is borrowed
        // for the call.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), 1, 0) };
        if ready < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        return ready > 0 && fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0;
    }
}

// ---------------------------------------------------------------------------
// the session
// ---------------------------------------------------------------------------

/// A live pty and the child attached to it.
pub struct PtySession {
    pub(crate) shared: Arc<Shared>,
    pty_id: PtyId,
    kind: ProcKind,
    ticket: SpawnTicket,
    agent_key: String,
    cwd: PathBuf,
    pid: i32,
    pgid: i32,
    start_sec: Option<i64>,
    opened_at: i64,
    cols: AtomicU16,
    rows: AtomicU16,
    /// `MasterPty` is `Send` but not `Sync`.
    master: Mutex<Box<dyn MasterPty + Send>>,
    master_fd: RawFd,
    /// The PTY write lock (§6.2.4): every write, paste and screen action holds
    /// it, so a multi-key screen action is never interleaved with typing.
    pub(crate) write_gate: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for PtySession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PtySession")
            .field("pty_id", &self.pty_id)
            .field("pid", &self.pid)
            .finish()
    }
}

impl PtySession {
    pub(crate) fn start(
        parts: SessionParts,
        master: Box<dyn MasterPty + Send>,
        child: Box<dyn Child + Send + Sync>,
    ) -> Result<PtySession, IpcError> {
        let now = now_ms();
        let master_fd = master
            .as_raw_fd()
            .ok_or_else(|| IpcError::spawn_failed().with_detail("the pty master has no descriptor"))?;
        let reader = master
            .try_clone_reader()
            .map_err(|e| IpcError::spawn_failed().with_detail(format!("pty reader: {e}")))?;
        let writer = master
            .take_writer()
            .map_err(|e| IpcError::spawn_failed().with_detail(format!("pty writer: {e}")))?;
        // The exit watcher's own handle on the master, for the drain check:
        // the session's may close first.
        let probe = dup_cloexec(master_fd)
            .map_err(|e| IpcError::spawn_failed().with_detail(format!("pty probe: {e}")))?;
        let id = parts.pty_id.as_str().to_string();

        dump::marker(&id, format_args!("open {}x{} {}", parts.cols, parts.rows, parts.agent_key));

        // Registered before the reader exists, so no byte arrives first.
        parts.status.register_sized(
            &id,
            &parts.agent_key,
            parts.idle_after_ms,
            parts.cols,
            parts.rows,
            now,
        );

        let shared = Arc::new(Shared {
            id: id.clone(),
            ring: Mutex::new(Ring::new(parts.ring_bytes)),
            view: Mutex::new(None),
            delivering: AtomicBool::new(false),
            resync: AtomicBool::new(false),
            eof: AtomicBool::new(false),
            exited: AtomicBool::new(false),
            exit: Mutex::new(None),
            last_output_at: AtomicI64::new(0),
            status: parts.status.clone(),
            output: tokio::sync::Notify::new(),
            exit_notify: tokio::sync::Notify::new(),
            writer: Arc::new(Mutex::new(writer)),
            progress: Condvar::new(),
            reaped: AtomicBool::new(false),
            held_slave: Mutex::new(Some(parts.held_slave)),
            #[cfg(any(test, feature = "test-fixtures"))]
            hold_reader: parts.hold_reader,
            #[cfg(any(test, feature = "test-fixtures"))]
            hold_outcome: std::sync::atomic::AtomicU8::new(0),
        });

        let (tx, rx) = tokio::sync::mpsc::channel::<Msg>(QUEUE_FRAMES);
        if !spawn_reader(shared.clone(), reader, tx) {
            // Nobody will read the master. A child that leads its session
            // could then never finish exiting while the core holds a slave,
            // so let go of it: the old, lossy behaviour, but no stuck exit.
            shared.close_held_slave();
        }
        spawn_coalescer(shared.clone(), rx);
        spawn_exit_watcher(shared.clone(), child, parts.pid, probe, parts.on_exit);

        Ok(PtySession {
            shared,
            pty_id: parts.pty_id,
            kind: parts.kind,
            ticket: parts.ticket,
            agent_key: parts.agent_key,
            cwd: parts.cwd,
            pid: parts.pid,
            pgid: parts.pgid,
            start_sec: parts.start_sec,
            opened_at: now,
            cols: AtomicU16::new(parts.cols),
            rows: AtomicU16::new(parts.rows),
            master: Mutex::new(master),
            master_fd,
            write_gate: tokio::sync::Mutex::new(()),
        })
    }

    // -- identity ------------------------------------------------------------

    pub fn id(&self) -> &str {
        &self.shared.id
    }

    pub fn pty_id(&self) -> &PtyId {
        &self.pty_id
    }

    pub fn kind(&self) -> ProcKind {
        self.kind
    }

    pub fn ticket(&self) -> &SpawnTicket {
        &self.ticket
    }

    pub fn agent_key(&self) -> &str {
        &self.agent_key
    }

    pub fn pid(&self) -> i32 {
        self.pid
    }

    pub fn pgid(&self) -> i32 {
        self.pgid
    }

    pub fn start_sec(&self) -> Option<i64> {
        self.start_sec
    }

    pub fn cwd(&self) -> &std::path::Path {
        &self.cwd
    }

    pub fn opened_at(&self) -> i64 {
        self.opened_at
    }

    pub fn has_exited(&self) -> bool {
        self.shared.exited.load(Ordering::Acquire)
    }

    pub fn saw_eof(&self) -> bool {
        self.shared.eof.load(Ordering::Acquire)
    }

    pub fn exit_info(&self) -> Option<ExitInfo> {
        *self.shared.exit.lock()
    }

    /// The core's own slave descriptor while it holds one (A52.1; tests
    /// check that it is close-on-exec and that it is closed after the exit).
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn held_slave_fd(&self) -> Option<RawFd> {
        self.shared.held_slave_fd()
    }

    /// What released the reader the T49 hook held back, once it was released.
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn reader_hold(&self) -> Option<ReaderHold> {
        match self.shared.hold_outcome.load(Ordering::Acquire) {
            1 => Some(ReaderHold::Reaped),
            2 => Some(ReaderHold::Ceiling),
            _ => None,
        }
    }

    pub fn last_output_at(&self) -> i64 {
        self.shared.last_output_at.load(Ordering::Relaxed)
    }

    pub fn size(&self) -> (u16, u16) {
        (self.cols.load(Ordering::Relaxed), self.rows.load(Ordering::Relaxed))
    }

    /// The winsize the kernel holds for this tty (`TIOCGWINSZ`).
    pub fn tty_size(&self) -> Option<(u16, u16)> {
        self.master.lock().get_size().ok().map(|s| (s.cols, s.rows))
    }

    /// The pty's foreground process group (`tcgetpgrp` on the master).
    pub fn foreground_pgid(&self) -> Option<i32> {
        if let Some(pgid) = self.master.lock().process_group_leader() {
            if pgid > 0 {
                return Some(pgid);
            }
        }
        // SAFETY: `self.master` owns the descriptor for the life of `self`.
        let fd = unsafe { BorrowedFd::borrow_raw(self.master_fd) };
        nix::unistd::tcgetpgrp(fd)
            .ok()
            .map(|pid| pid.as_raw())
            .filter(|pgid| *pgid > 0)
    }

    // -- the view ------------------------------------------------------------

    /// Attach `view` (§6.2.6): one view per PTY; a different view takes it
    /// over and the previous one hears `Detached{otherView}`. Attach first,
    /// then snapshot; the view dedupes by offset.
    pub fn attach(&self, view: ViewId, sink: Arc<dyn ChunkSink>) -> PtyInfo {
        {
            let mut current = self.shared.view.lock();
            if let Some(old) = current.as_ref() {
                if old.id != view {
                    old.sink.send(PtyChunk::Detached {
                        reason: DetachReason::OtherView,
                    });
                }
            }
            *current = Some(View { id: view, sink });
            // A fresh view's snapshot is authoritative.
            self.shared.resync.store(false, Ordering::Release);
            self.shared.delivering.store(true, Ordering::Release);
        }
        let (cols, rows) = self.size();
        PtyInfo {
            offset: self.shared.ring.lock().next_seq(),
            cols,
            rows,
            title: self.shared.status.title(&self.shared.id),
        }
    }

    /// Idempotent; only the attached view can detach itself.
    pub fn detach(&self, view: &ViewId) -> bool {
        let mut current = self.shared.view.lock();
        if current.as_ref().is_some_and(|v| &v.id == view) {
            *current = None;
            self.shared.delivering.store(false, Ordering::Release);
            return true;
        }
        false
    }

    pub fn has_view(&self) -> bool {
        self.shared.delivering.load(Ordering::Acquire)
    }

    /// Stop delivering to whatever view is attached (the session is ending).
    pub(crate) fn drop_view(&self) {
        *self.shared.view.lock() = None;
        self.shared.delivering.store(false, Ordering::Release);
    }

    /// 16-byte header + the retained ring bytes.
    pub fn snapshot(&self) -> Vec<u8> {
        self.shared.ring.lock().snapshot()
    }

    pub fn next_offset(&self) -> u64 {
        self.shared.ring.lock().next_seq()
    }

    // -- writing ---------------------------------------------------------------

    /// Write raw bytes **without** taking the write lock (the caller holds it).
    /// The write itself runs off the async runtime: a child that is not
    /// reading can make a large write block.
    pub(crate) async fn write_unlocked(&self, bytes: Vec<u8>) -> Result<(), IpcError> {
        dump::input(&self.shared.id, &bytes);
        let writer = self.shared.writer.clone();
        tokio::task::spawn_blocking(move || {
            let mut w = writer.lock();
            w.write_all(&bytes).and_then(|()| w.flush())
        })
        .await
        .map_err(|_| IpcError::internal("pty-write-join"))?
        .map_err(|e| {
            if self.has_exited() {
                IpcError::unsupported("noPty")
            } else {
                IpcError::spawn_failed().with_detail(format!("pty write failed: {e}"))
            }
        })
    }

    /// Write raw bytes under the write lock.
    pub async fn write(&self, bytes: &[u8]) -> Result<(), IpcError> {
        let _gate = self.write_gate.lock().await;
        self.write_unlocked(bytes.to_vec()).await
    }

    /// Bracketed paste + `\r` iff `submit`, under the write lock (§6.2.3).
    pub async fn paste(&self, text: &str, submit: bool) -> Result<(), IpcError> {
        let _gate = self.write_gate.lock().await;
        self.write_unlocked(paste_bytes(text, submit)).await
    }

    /// `TIOCSWINSZ` (the kernel sends `SIGWINCH`). A no-op when unchanged.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<bool, IpcError> {
        let (cols, rows) = sanitize_size(cols, rows);
        if self.size() == (cols, rows) {
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
            .map_err(|e| IpcError::spawn_failed().with_detail(format!("resize failed: {e}")))?;
        self.cols.store(cols, Ordering::Relaxed);
        self.rows.store(rows, Ordering::Relaxed);
        dump::marker(&self.shared.id, format_args!("resize {cols}x{rows}"));
        self.shared.status.on_resize(&self.shared.id, cols, rows);
        Ok(true)
    }

    // -- waiting ---------------------------------------------------------------

    /// Wait until the screen changed after `since` (a status-engine grid
    /// generation) and then held still for `quiet`, or `max` passed. Returns
    /// whether it changed at all.
    pub(crate) async fn wait_screen_change(&self, since: Option<u64>, quiet: Duration, max: Duration) -> bool {
        let deadline = Instant::now() + max;
        let generation = || {
            self.shared
                .status
                .screen(&self.shared.id)
                .map(|s| s.generation)
        };
        // First: anything at all.
        loop {
            let notified = self.shared.output.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if generation() != since || self.has_exited() {
                break;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            if tokio::time::timeout(left, notified).await.is_err() {
                return generation() != since;
            }
        }
        // Then: the repaint is over.
        loop {
            let notified = self.shared.output.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || self.has_exited() {
                return true;
            }
            if tokio::time::timeout(quiet.min(left), notified).await.is_err() {
                return true;
            }
        }
    }

    /// Wait up to `budget` for the child to be reaped.
    pub async fn wait_exit(&self, budget: Duration) -> bool {
        let notified = self.shared.exit_notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.has_exited() {
            return true;
        }
        tokio::time::timeout(budget, notified).await.is_ok() || self.has_exited()
    }
}

impl Drop for PtySession {
    /// The manager let go of the session: its slave descriptor goes too
    /// (A52.1). The exit watcher has usually closed it already.
    fn drop(&mut self) {
        self.shared.close_held_slave();
    }
}

// ---------------------------------------------------------------------------
// the three threads
// ---------------------------------------------------------------------------

fn short(id: &str) -> &str {
    let tail = id.len().saturating_sub(8);
    &id[tail..]
}

/// Test hook (T49): hold the reader back until the child is reaped or
/// `ceiling` passes, and record which came first.
#[cfg(any(test, feature = "test-fixtures"))]
fn hold_until_reaped(shared: &Shared, ceiling: Duration) {
    let deadline = Instant::now() + ceiling;
    let mut ring = shared.ring.lock();
    while !shared.reaped.load(Ordering::Acquire) {
        if shared.progress.wait_until(&mut ring, deadline).timed_out() {
            break;
        }
    }
    let outcome = if shared.reaped.load(Ordering::Acquire) { 1 } else { 2 };
    shared.hold_outcome.store(outcome, Ordering::Release);
}

/// Start the reader thread; `false` if it could not be started.
fn spawn_reader(shared: Arc<Shared>, mut reader: Box<dyn Read + Send>, tx: tokio::sync::mpsc::Sender<Msg>) -> bool {
    let name = format!("gilde-pty-r-{}", short(&shared.id));
    let spawned = std::thread::Builder::new().name(name).spawn(move || {
        use tokio::sync::mpsc::error::TrySendError;
        #[cfg(any(test, feature = "test-fixtures"))]
        if let Some(ceiling) = shared.hold_reader {
            hold_until_reaped(&shared, ceiling);
        }
        let mut buf = vec![0u8; READ_CHUNK];
        loop {
            match reader.read(&mut buf) {
                // `portable-pty` maps the master's EIO (all slaves closed) to EOF.
                Ok(0) => break,
                Ok(n) => {
                    shared.last_output_at.store(now_ms(), Ordering::Relaxed);
                    dump::output(&shared.id, &buf[..n]);
                    let offset = shared.ring.lock().push(&buf[..n]);
                    // The exit watcher's drain re-checks the master after it.
                    shared.progress.notify_all();
                    let queries = shared.status.on_output(&shared.id, &buf[..n]);
                    shared.output.notify_waiters();
                    if shared.delivering.load(Ordering::Acquire) {
                        match tx.try_send(Msg::Data {
                            offset,
                            bytes: buf[..n].to_vec(),
                        }) {
                            Ok(()) => {}
                            Err(TrySendError::Full(_)) => shared.resync.store(true, Ordering::Release),
                            Err(TrySendError::Closed(_)) => {}
                        }
                    } else if !queries.is_empty() {
                        // No view: the core answers DA1 and DSR itself so a
                        // hidden TUI never stalls on a reply (§6.2.6).
                        let reply: Vec<u8> = queries.iter().flat_map(|q| q.reply()).collect();
                        let _ = tx.try_send(Msg::Reply(reply));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    tracing::debug!(target: "pty", session = %shared.id, error = %e, "pty read ended");
                    break;
                }
            }
        }
        {
            // Under the ring lock: the exit watcher checks `eof` under it.
            let _ring = shared.ring.lock();
            shared.eof.store(true, Ordering::Release);
        }
        shared.progress.notify_all();
        shared.output.notify_waiters();
        dump::marker(&shared.id, format_args!("eof"));
        dump::close(&shared.id);
    });
    match spawned {
        Ok(_) => true,
        Err(err) => {
            tracing::error!(target: "pty", error = %err, "could not start the pty reader thread");
            false
        }
    }
}

fn spawn_coalescer(shared: Arc<Shared>, mut rx: tokio::sync::mpsc::Receiver<Msg>) {
    tauri::async_runtime::spawn(async move {
        let mut pending: Vec<u8> = Vec::with_capacity(FLUSH_BYTES);
        let mut pending_offset: u64 = 0;
        let mut flush_at: Option<tokio::time::Instant> = None;
        // Where the last delivered chunk ended: consecutive chunks join up
        // unless a resync was published between them.
        let mut delivered_end: Option<u64> = None;

        loop {
            let msg = match flush_at {
                // Bytes to flush: wait at most until the flush is due.
                Some(at) => match tokio::time::timeout_at(at, rx.recv()).await {
                    Ok(msg) => msg,
                    Err(_) => {
                        flush(&shared, &mut pending, pending_offset, &mut delivered_end);
                        flush_at = None;
                        // A drop that nothing arrived after must still be
                        // announced; a view that is behind dedupes by offset.
                        if shared.resync.load(Ordering::Acquire) {
                            shared.announce_resync();
                        }
                        continue;
                    }
                },
                // Nothing pending: sleep until the reader sends something.
                None => rx.recv().await,
            };
            match msg {
                Some(Msg::Data { offset, bytes }) => {
                    let joins_up = if pending.is_empty() {
                        delivered_end
                    } else {
                        Some(pending_offset + pending.len() as u64)
                    }
                    .is_none_or(|end| end == offset);
                    if !joins_up {
                        // A hole: send what joins up, then tell the view to
                        // re-snapshot before anything from the far side.
                        flush(&shared, &mut pending, pending_offset, &mut delivered_end);
                        shared.announce_resync();
                        flush_at = None;
                    }
                    if pending.is_empty() {
                        pending_offset = offset;
                        flush_at = Some(tokio::time::Instant::now() + FLUSH_INTERVAL);
                    }
                    pending.extend_from_slice(&bytes);
                    if pending.len() >= FLUSH_BYTES {
                        flush(&shared, &mut pending, pending_offset, &mut delivered_end);
                        flush_at = None;
                    }
                }
                Some(Msg::Reply(bytes)) => {
                    let writer = shared.writer.clone();
                    let id = shared.id.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        dump::input(&id, &bytes);
                        let mut w = writer.lock();
                        if let Err(e) = w.write_all(&bytes).and_then(|()| w.flush()) {
                            tracing::debug!(target: "pty", error = %e, "terminal query reply not written");
                        }
                    })
                    .await;
                }
                None => {
                    flush(&shared, &mut pending, pending_offset, &mut delivered_end);
                    if shared.resync.load(Ordering::Acquire) {
                        shared.announce_resync();
                    }
                    break;
                }
            }
        }
        shared.status.tick_session(&shared.id);
    });
}

/// Send one coalesced chunk; `offset` is the ring offset of `pending[0]`.
fn flush(shared: &Shared, pending: &mut Vec<u8>, offset: u64, delivered_end: &mut Option<u64>) {
    if pending.is_empty() {
        return;
    }
    *delivered_end = Some(offset + pending.len() as u64);
    let b64 = base64::engine::general_purpose::STANDARD.encode(&pending[..]);
    pending.clear();
    shared.send_to_view(PtyChunk::Data { offset, b64 });
}

/// After the reap (A52.1): wait until the reader has taken everything the
/// master holds, or `deadline` passes. The check is made again after each
/// of the reader's reads: the watcher sleeps on [`Shared::progress`] until
/// the ring's offset moves or the reader sees EOF. Returns whether the
/// master drained.
fn drain_master(shared: &Shared, master: BorrowedFd<'_>, deadline: Instant) -> bool {
    loop {
        let seen = {
            let ring = shared.ring.lock();
            if shared.eof.load(Ordering::Acquire) {
                return true;
            }
            ring.next_seq()
        };
        if !master_readable(master) {
            return true;
        }
        let mut ring = shared.ring.lock();
        while ring.next_seq() == seen && !shared.eof.load(Ordering::Acquire) {
            if shared.progress.wait_until(&mut ring, deadline).timed_out() {
                return false;
            }
        }
    }
}

/// Wait for the reader's EOF until `deadline`; whether it came.
fn wait_eof(shared: &Shared, deadline: Instant) -> bool {
    let mut ring = shared.ring.lock();
    while !shared.eof.load(Ordering::Acquire) {
        if shared.progress.wait_until(&mut ring, deadline).timed_out() {
            return shared.eof.load(Ordering::Acquire);
        }
    }
    true
}

fn spawn_exit_watcher(
    shared: Arc<Shared>,
    mut child: Box<dyn Child + Send + Sync>,
    pid: i32,
    probe: OwnedFd,
    on_exit: Box<dyn FnOnce(ExitInfo, i64) + Send>,
) {
    let name = format!("gilde-pty-w-{}", short(&shared.id));
    let spawned = std::thread::Builder::new().name(name).spawn(move || {
        let exit = reap(&mut child);
        let at = now_ms();
        {
            let _ring = shared.ring.lock();
            shared.reaped.store(true, Ordering::Release);
        }
        shared.progress.notify_all();
        // Report the exit only once the child's last bytes are in the ring
        // (A52.1). First the master drains into the reader; then the core
        // lets go of its slave, which (with no grandchild holding one) is
        // the last close and gives the reader EOF. A grandchild that keeps
        // the slave open must not hold the exit back: one bound covers
        // the drain and the EOF.
        let deadline = Instant::now() + EXIT_EOF_GRACE;
        let drained = drain_master(&shared, probe.as_fd(), deadline);
        drop(probe);
        shared.close_held_slave();
        let eof = wait_eof(&shared, deadline);
        if !drained || !eof {
            tracing::debug!(target: "pty", pty = %shared.id, drained, eof, "pty exit reported at the grace bound");
        }
        *shared.exit.lock() = Some(exit);
        shared.exited.store(true, Ordering::Release);
        shared.status.on_exit(&shared.id, exit.code, exit.signal);
        tracing::info!(target: "pty", pty = %shared.id, pid, code = ?exit.code, signal = ?exit.signal, "pty child exited");
        on_exit(exit, at);
        shared.exit_notify.notify_waiters();
        shared.output.notify_waiters();
    });
    if let Err(err) = spawned {
        tracing::error!(target: "pty", error = %err, "could not start the pty exit watcher");
    }
}

/// `waitpid`, through whichever door gives the numeric signal.
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
                ExitInfo { code: None, signal: None }
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
            ExitInfo { code: None, signal: None }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_size_is_clamped_never_rejected() {
        assert_eq!(sanitize_size(0, 0), (FALLBACK_COLS, FALLBACK_ROWS));
        assert_eq!(sanitize_size(1, 1), (MIN_COLS, MIN_ROWS));
        assert_eq!(sanitize_size(120, 40), (120, 40));
        assert_eq!(sanitize_size(5_000, 9_000), (MAX_COLS, MAX_ROWS));
    }

    /// T-pty-pasteMarker: the end marker in content never closes the paste.
    #[test]
    fn paste_splits_the_end_marker_out_of_content() {
        assert_eq!(paste_bytes("hi", true), b"\x1b[200~hi\x1b[201~\r".to_vec());
        assert_eq!(paste_bytes("hi", false), b"\x1b[200~hi\x1b[201~".to_vec());
        let hostile = "a\x1b[201~rm -rf ~\r\x1b[201~b";
        let bytes = paste_bytes(hostile, true);
        assert_eq!(
            bytes,
            b"\x1b[200~a\x1b[201~\x1b[200~rm -rf ~\r\x1b[201~\x1b[200~b\x1b[201~\r".to_vec()
        );
        // Every end marker that is sent closes a segment the core opened.
        let count = |needle: &[u8]| bytes.windows(needle.len()).filter(|w| *w == needle).count();
        assert_eq!(count(PASTE_START), count(PASTE_END));
        assert!(paste_bytes("\x1b[201~", false).is_empty(), "nothing but a marker sends nothing");
    }

    struct Collect(Mutex<Vec<PtyChunk>>);
    impl ChunkSink for Collect {
        fn send(&self, chunk: PtyChunk) -> bool {
            self.0.lock().push(chunk);
            true
        }
    }

    fn shared_for_test() -> (Arc<Shared>, Arc<Collect>) {
        let sink = Arc::new(Collect(Mutex::new(Vec::new())));
        let writer: Box<dyn Write + Send> = Box::new(std::io::sink());
        let shared = Arc::new(Shared {
            id: "unit".to_string(),
            ring: Mutex::new(Ring::new(1024)),
            view: Mutex::new(Some(View {
                id: ViewId::parse("v1").unwrap(),
                sink: sink.clone(),
            })),
            delivering: AtomicBool::new(true),
            resync: AtomicBool::new(false),
            eof: AtomicBool::new(false),
            exited: AtomicBool::new(false),
            exit: Mutex::new(None),
            last_output_at: AtomicI64::new(0),
            status: Arc::new(StatusEngine::new()),
            output: tokio::sync::Notify::new(),
            exit_notify: tokio::sync::Notify::new(),
            writer: Arc::new(Mutex::new(writer)),
            progress: Condvar::new(),
            reaped: AtomicBool::new(false),
            held_slave: Mutex::new(None),
            hold_reader: None,
            hold_outcome: std::sync::atomic::AtomicU8::new(0),
        });
        (shared, sink)
    }

    /// Regression: a batch never straddles a dropped frame — only offsets
    /// prove contiguity.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dropped_frame_never_lets_a_chunk_lie_about_its_offset() {
        let (shared, sink) = shared_for_test();
        let (tx, rx) = tokio::sync::mpsc::channel::<Msg>(8);
        spawn_coalescer(shared.clone(), rx);
        tx.send(Msg::Data { offset: 0, bytes: b"aaaa".to_vec() }).await.unwrap();
        shared.resync.store(true, Ordering::Release);
        tx.send(Msg::Data { offset: 8, bytes: b"cccc".to_vec() }).await.unwrap();
        drop(tx);
        // Event-driven: the coalescer ends when the queue closes.
        for _ in 0..500 {
            if sink.0.lock().iter().filter(|c| matches!(c, PtyChunk::Data { .. })).count() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let chunks = sink.0.lock().clone();
        let data: Vec<(u64, Vec<u8>)> = chunks
            .iter()
            .filter_map(|c| match c {
                PtyChunk::Data { offset, b64 } => Some((
                    *offset,
                    base64::engine::general_purpose::STANDARD.decode(b64).unwrap(),
                )),
                _ => None,
            })
            .collect();
        assert_eq!(data, vec![(0, b"aaaa".to_vec()), (8, b"cccc".to_vec())]);
        let first_resync = chunks.iter().position(|c| matches!(c, PtyChunk::Resync { .. }));
        let second_data = chunks
            .iter()
            .enumerate()
            .filter(|(_, c)| matches!(c, PtyChunk::Data { .. }))
            .nth(1)
            .map(|(i, _)| i);
        assert!(
            first_resync.is_some() && first_resync < second_data,
            "the hole is announced before the far side of it: {chunks:?}"
        );
    }
}
