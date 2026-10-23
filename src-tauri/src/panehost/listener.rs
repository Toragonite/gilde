//! The Unix socket listener and self-heal (§4.6, §10.4).
//!
//! Owner: T07.
//!
//! * **Path:** `<app_data>/run/ph.sock` when that path is ≤ 100 bytes (macOS
//!   `sun_path` holds 104), else `/tmp/gilde-<uid>/ph-<8 hex of
//!   sha256(app_data)>.sock`.
//! * **Directory:** mode 0700, owned by the user, a real directory (not a
//!   symlink) — checked at bind and at every (re)start; a mismatch refuses to
//!   host (`lead_env` then answers `UNSUPPORTED{paneHosting}` and teams run
//!   in-process). The directory is created 0700 when absent; an existing one
//!   is never re-permissioned.
//! * **Socket:** mode 0600. A stale socket file of ours is replaced; any other
//!   file at the path is left alone and hosting is refused.
//! * **Accept loop:** one thread blocked in `poll(2)` on the listener and a
//!   stop pipe (no periodic wake-ups). Each connection is served on the
//!   runtime's blocking pool, at most [`MAX_INFLIGHT`] at once (more are
//!   closed unanswered and counted), with a 2 s read and write timeout.
//!   **Requires a multi-thread Tokio runtime** (the service's timers run on
//!   its workers).
//! * **Self-heal:** [`PaneHost::reconcile`] (every 60 s) re-creates the
//!   directory and re-binds when the socket vanished (e.g. `/tmp` cleaned) or
//!   the accept thread died; a panic inside the loop restarts it within 1 s.

use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::panehost::auth::{self, PeerCred};
use crate::panehost::proto::{self, LineError, ShimResponse};
use crate::panehost::server::PaneHost;

pub const SOCKET_NAME: &str = "ph.sock";
/// The longest primary path used (`sun_path` is 104 bytes on macOS).
pub const MAX_PRIMARY_PATH: usize = 100;
pub const READ_TIMEOUT: Duration = Duration::from_secs(2);
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
/// Connections served at once; more are closed unanswered.
pub const MAX_INFLIGHT: usize = 16;
/// How often the composition root runs [`PaneHost::reconcile`].
pub const RECONCILE_EVERY: Duration = Duration::from_secs(60);

/// The socket path for an app data dir (§4.6 rule).
pub fn socket_path(app_data: &Path, uid: u32) -> PathBuf {
    let primary = app_data.join("run").join(SOCKET_NAME);
    if primary.as_os_str().len() <= MAX_PRIMARY_PATH {
        return primary;
    }
    let digest = hex::encode(Sha256::digest(app_data.as_os_str().as_encoded_bytes()));
    PathBuf::from(format!("/tmp/gilde-{uid}")).join(format!("ph-{}.sock", &digest[..8]))
}

/// Why the socket cannot be hosted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostProblem {
    NotADirectory,
    Symlink,
    WrongOwner,
    TooOpen,
    /// A file that is not our socket sits at the socket path.
    Occupied,
    Io(io::ErrorKind),
}

impl HostProblem {
    pub fn as_str(&self) -> &'static str {
        match self {
            HostProblem::NotADirectory => "notADirectory",
            HostProblem::Symlink => "symlink",
            HostProblem::WrongOwner => "wrongOwner",
            HostProblem::TooOpen => "tooOpen",
            HostProblem::Occupied => "occupied",
            HostProblem::Io(_) => "io",
        }
    }
}

fn io_problem(e: io::Error) -> HostProblem {
    HostProblem::Io(e.kind())
}

/// The directory is ours alone: a real directory, owned by `uid`, no group
/// or other permission bits.
pub fn check_private_dir(dir: &Path, uid: u32) -> Result<(), HostProblem> {
    let m = std::fs::symlink_metadata(dir).map_err(io_problem)?;
    if m.file_type().is_symlink() {
        return Err(HostProblem::Symlink);
    }
    if !m.is_dir() {
        return Err(HostProblem::NotADirectory);
    }
    if m.uid() != uid {
        return Err(HostProblem::WrongOwner);
    }
    if m.mode() & 0o077 != 0 {
        return Err(HostProblem::TooOpen);
    }
    Ok(())
}

/// Create `dir` (one level, mode 0700) when absent, then check it.
pub fn ensure_private_dir(dir: &Path, uid: u32) -> Result<(), HostProblem> {
    match std::fs::symlink_metadata(dir) {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            std::fs::DirBuilder::new().mode(0o700).create(dir).map_err(io_problem)?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(io_problem)?;
        }
        Err(e) => return Err(io_problem(e)),
    }
    check_private_dir(dir, uid)
}

fn bind_socket(path: &Path, uid: u32) -> Result<(UnixListener, u64), HostProblem> {
    let parent = path.parent().ok_or(HostProblem::NotADirectory)?;
    ensure_private_dir(parent, uid)?;
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_socket() && m.uid() == uid => {
            std::fs::remove_file(path).map_err(io_problem)?;
        }
        Ok(_) => return Err(HostProblem::Occupied),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_problem(e)),
    }
    let listener = UnixListener::bind(path).map_err(io_problem)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(io_problem)?;
    let ino = std::fs::symlink_metadata(path).map_err(io_problem)?.ino();
    listener.set_nonblocking(true).map_err(io_problem)?;
    Ok((listener, ino))
}

/// A close-on-exec pipe (read end, write end).
fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` is a writable array of two ints.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    for fd in fds {
        // SAFETY: `fd` was just returned by pipe(2).
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    // SAFETY: both fds are open and owned by nobody else.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// A bound listener and its accept thread.
pub(crate) struct Running {
    path: PathBuf,
    ino: u64,
    stop: Option<std::fs::File>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Running {
    fn alive(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }

    fn socket_intact(&self) -> bool {
        std::fs::symlink_metadata(&self.path).is_ok_and(|m| m.file_type().is_socket() && m.ino() == self.ino)
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(mut stop) = self.stop.take() {
            let _ = stop.write_all(&[1]);
        }
        if let Some(t) = self.thread.take() {
            if t.thread().id() != std::thread::current().id() {
                let _ = t.join();
            }
        }
        // Remove the socket file only if it is still the one we bound.
        if self.socket_intact() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// What the accept loop needs.
struct LoopCtx {
    listener: UnixListener,
    stop: OwnedFd,
    host: Weak<PaneHost>,
    rt: tokio::runtime::Handle,
    inflight: Arc<AtomicUsize>,
}

enum LoopEnd {
    Stop,
    Retry,
}

fn accept_loop(ctx: LoopCtx) {
    loop {
        let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| accept_until_stop(&ctx)));
        match run {
            Ok(LoopEnd::Stop) => return,
            Ok(LoopEnd::Retry) | Err(_) => {
                tracing::warn!(target: "panehost", "the accept loop restarts");
                std::thread::sleep(Duration::from_millis(500));
            }
        }
        if ctx.host.strong_count() == 0 {
            return;
        }
    }
}

fn accept_until_stop(ctx: &LoopCtx) -> LoopEnd {
    loop {
        let mut fds = [
            libc::pollfd { fd: ctx.listener.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: ctx.stop.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        ];
        // SAFETY: `fds` is a valid array of two pollfds for the call.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if rc < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return LoopEnd::Retry;
        }
        if fds[1].revents != 0 {
            return LoopEnd::Stop;
        }
        if fds[0].revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
            return LoopEnd::Retry;
        }
        if fds[0].revents & libc::POLLIN == 0 {
            continue;
        }
        loop {
            match ctx.listener.accept() {
                Ok((stream, _)) => dispatch(stream, ctx),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    tracing::warn!(target: "panehost", kind = ?e.kind(), "accept failed");
                    break;
                }
            }
        }
    }
}

/// Decrements the in-flight count even if serving panics.
struct Slot(Arc<AtomicUsize>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn dispatch(stream: UnixStream, ctx: &LoopCtx) {
    let Some(host) = ctx.host.upgrade() else { return };
    if ctx.inflight.fetch_add(1, Ordering::SeqCst) >= MAX_INFLIGHT {
        ctx.inflight.fetch_sub(1, Ordering::SeqCst);
        host.counters.dropped_busy();
        return;
    }
    let slot = Slot(ctx.inflight.clone());
    let rt = ctx.rt.clone();
    ctx.rt.spawn_blocking(move || {
        let _slot = slot;
        serve(stream, &host, &rt);
    });
}

/// One connection: one request line in, one response line out.
fn serve(mut stream: UnixStream, host: &PaneHost, rt: &tokio::runtime::Handle) {
    // Accepted sockets inherit O_NONBLOCK on macOS.
    if stream.set_nonblocking(false).is_err() {
        return;
    }
    // Read first: a peer that connected and closed (an abandoned connect)
    // has no credentials left to read, but is still counted. On macOS a
    // socket whose peer already closed refuses `SO_RCVTIMEO` (EINVAL).
    if stream.set_read_timeout(Some(READ_TIMEOUT)).is_err() {
        host.counters.empty_connection();
        return;
    }
    let deadline = Instant::now() + READ_TIMEOUT;
    let line = proto::read_line_until(&mut stream, proto::MAX_REQUEST_LINE, deadline);
    if line == Err(LineError::Empty) {
        host.counters.empty_connection();
        return;
    }
    let Ok(peer) = auth::peer_cred(&stream) else { return };
    let resp = match line {
        Ok(line) => rt.block_on(host.handle_line(&line, peer)),
        Err(LineError::TooLong) => host.refuse_oversized(&peer),
        Err(LineError::Empty) | Err(LineError::Io) => return,
    };
    let Ok(mut out) = serde_json::to_vec(&resp) else { return };
    out.push(b'\n');
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let _ = stream.write_all(&out);
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

/// What a reconcile found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reconcile {
    Healthy,
    Rebound,
    Unavailable(HostProblem),
}

impl PaneHost {
    /// Bind the socket and start the accept loop (replacing a running one).
    pub fn start(&self, rt: tokio::runtime::Handle) -> Result<(), HostProblem> {
        let mut slot = self.listener.lock();
        if let Some(old) = slot.take() {
            drop(old);
        }
        let bound = bind_socket(&self.cfg.socket_path, self.cfg.app_uid).and_then(|(listener, ino)| {
            let (stop_r, stop_w) = pipe().map_err(io_problem)?;
            let ctx = LoopCtx {
                listener,
                stop: stop_r,
                host: self.me.clone(),
                rt,
                inflight: Arc::new(AtomicUsize::new(0)),
            };
            let thread = std::thread::Builder::new()
                .name("panehost-accept".into())
                .spawn(move || accept_loop(ctx))
                .map_err(io_problem)?;
            Ok(Running {
                path: self.cfg.socket_path.clone(),
                ino,
                stop: Some(std::fs::File::from(stop_w)),
                thread: Some(thread),
            })
        });
        match bound {
            Ok(running) => {
                *slot = Some(running);
                self.hosting.store(true, Ordering::SeqCst);
                tracing::info!(target: "panehost", "pane host listening");
                Ok(())
            }
            Err(problem) => {
                self.hosting.store(false, Ordering::SeqCst);
                tracing::warn!(target: "panehost", problem = problem.as_str(), "pane hosting unavailable");
                Err(problem)
            }
        }
    }

    /// Stop listening (quit, tests).
    pub fn stop(&self) {
        self.hosting.store(false, Ordering::SeqCst);
        let old = self.listener.lock().take();
        drop(old);
    }

    /// The 60 s self-heal: re-bind when the socket vanished or the accept
    /// thread died; refuse to host when the directory stopped being private.
    pub fn reconcile(&self, rt: tokio::runtime::Handle) -> Reconcile {
        let parent = self.cfg.socket_path.parent().map(Path::to_path_buf);
        let dir_ok = match parent.as_deref() {
            Some(dir) => match check_private_dir(dir, self.cfg.app_uid) {
                Ok(()) => Ok(()),
                // A vanished directory is re-created by the restart.
                Err(HostProblem::Io(io::ErrorKind::NotFound)) => Err(None),
                Err(p) => Err(Some(p)),
            },
            None => Err(Some(HostProblem::NotADirectory)),
        };
        if let Err(Some(problem)) = dir_ok {
            self.stop();
            tracing::warn!(target: "panehost", problem = problem.as_str(), "the socket directory is no longer private");
            return Reconcile::Unavailable(problem);
        }
        let healthy = dir_ok.is_ok() && self.listener.lock().as_ref().is_some_and(|r| r.alive() && r.socket_intact());
        if healthy {
            return Reconcile::Healthy;
        }
        match self.start(rt) {
            Ok(()) => {
                tracing::info!(target: "panehost", "pane host socket re-created");
                Reconcile::Rebound
            }
            Err(p) => Reconcile::Unavailable(p),
        }
    }

    /// Run [`PaneHost::reconcile`] every `every` until the host is dropped.
    pub fn spawn_reconcile_loop(self: &Arc<Self>, rt: tokio::runtime::Handle, every: Duration) -> tokio::task::JoinHandle<()> {
        let host = Arc::downgrade(self);
        let rt2 = rt.clone();
        rt.spawn(async move {
            let mut tick = tokio::time::interval(every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await;
            loop {
                tick.tick().await;
                let Some(h) = host.upgrade() else { return };
                let rt3 = rt2.clone();
                let _ = tokio::task::spawn_blocking(move || h.reconcile(rt3)).await;
            }
        })
    }

    /// An over-long request line: refused like a bad frame (after the uid).
    pub(crate) fn refuse_oversized(&self, peer: &PeerCred) -> ShimResponse {
        if peer.uid != self.cfg.app_uid {
            return self.refuse(auth::Refusal::ForeignUid, peer, "");
        }
        self.refuse(auth::Refusal::BadFrame, peer, "")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_socket_lives_in_run_unless_the_path_is_too_long() {
        assert_eq!(socket_path(Path::new("/Users/t/Library/Application Support/gilde"), 501), PathBuf::from("/Users/t/Library/Application Support/gilde/run/ph.sock"));
        let long = PathBuf::from(format!("/Users/t/{}", "x".repeat(120)));
        let p = socket_path(&long, 501);
        assert!(p.starts_with("/tmp/gilde-501"), "{}", p.display());
        let name = p.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("ph-") && name.ends_with(".sock") && name.len() == "ph-".len() + 8 + ".sock".len());
        assert_eq!(socket_path(&long, 501), p, "stable per app data dir");
        let edge = PathBuf::from(format!("/{}", "y".repeat(MAX_PRIMARY_PATH - "/run/ph.sock".len() - 1)));
        assert_eq!(socket_path(&edge, 501).as_os_str().len(), MAX_PRIMARY_PATH);
        assert!(socket_path(&edge, 501).starts_with(&edge), "exactly 100 bytes stays primary");
    }

    #[test]
    fn the_directory_must_be_private_and_ours() {
        let uid = auth::app_uid();
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("run");
        ensure_private_dir(&run, uid).unwrap();
        assert_eq!(std::fs::metadata(&run).unwrap().permissions().mode() & 0o777, 0o700);
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(check_private_dir(&run, uid), Err(HostProblem::TooOpen));
        assert_eq!(check_private_dir(&run, uid + 1), Err(HostProblem::WrongOwner));
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&run, &link).unwrap();
        assert_eq!(check_private_dir(&link, uid), Err(HostProblem::Symlink));
        let file = dir.path().join("file");
        std::fs::write(&file, b"").unwrap();
        assert_eq!(check_private_dir(&file, uid), Err(HostProblem::NotADirectory));
    }

    #[test]
    fn a_foreign_file_at_the_socket_path_is_never_removed() {
        let uid = auth::app_uid();
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("run");
        ensure_private_dir(&run, uid).unwrap();
        let path = run.join(SOCKET_NAME);
        std::fs::write(&path, b"not a socket").unwrap();
        assert_eq!(bind_socket(&path, uid).err(), Some(HostProblem::Occupied));
        assert!(path.is_file());
        std::fs::remove_file(&path).unwrap();
        let (l, _) = bind_socket(&path, uid).unwrap();
        assert_eq!(std::fs::symlink_metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        drop(l);
        // Our stale socket is replaced.
        assert!(bind_socket(&path, uid).is_ok());
    }
}
