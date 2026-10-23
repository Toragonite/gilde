//! Application state assembly (T29; reworked for Gilde 2 by T09).
//!
//! Four things live here, and they are the four things that only exist once
//! per process:
//!
//!   * [`AppState`] — every service the boot built, in one place, for the code
//!     that needs *all* of them at once (the shutdown path, diagnostics).
//!   * [`Services`] — the late-bound service traits of the other build tasks
//!     (World, the sessions manager, compat, notifications). Each is bound
//!     once by `lib.rs` when its owner's implementation exists; a command that
//!     needs an unbound one answers `UNSUPPORTED{cap:"notBuilt"}`.
//!   * [`InstanceLock`] — the `instance.lock` `flock` guard of DESIGN §10-1.
//!   * [`shutdown`] — the counterpart of the boot: the quit sweep of contract
//!     §10.2 (never SIGKILL a vendor CLI), the sidecar, then
//!     `app_instance.ended_at`.

use std::fs::{File, OpenOptions};
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::os::unix::io::AsRawFd as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tauri::Manager as _;

use crate::config::ConfigStore;
use crate::contract::services::{CompatApi, NotifyApi, SessionActorApi, WorldApi};
use crate::lifecycle::boot::Lifecycle;
use crate::lifecycle::wiring::Wired;
use crate::paths::AppPaths;
use crate::sidecar::SidecarManager;
use crate::store::Db;
use crate::supervise::registry::Supervisor;

// ---------------------------------------------------------------------------
// Services (late-bound traits of other tasks)
// ---------------------------------------------------------------------------

/// The service traits other tasks provide (contract §15.2), bound once each.
#[derive(Default)]
pub struct Services {
    world: OnceLock<Arc<dyn WorldApi>>,
    sessions: OnceLock<Arc<dyn SessionActorApi>>,
    compat: OnceLock<Arc<dyn CompatApi>>,
    notify: OnceLock<Arc<dyn NotifyApi>>,
}

impl std::fmt::Debug for Services {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Services")
            .field("world", &self.world.get().is_some())
            .field("sessions", &self.sessions.get().is_some())
            .field("compat", &self.compat.get().is_some())
            .field("notify", &self.notify.get().is_some())
            .finish()
    }
}

impl Services {
    pub fn new() -> Arc<Services> {
        Arc::new(Services::default())
    }

    /// Bind World (T02). `false` if one was bound already.
    pub fn bind_world(&self, world: Arc<dyn WorldApi>) -> bool {
        self.world.set(world).is_ok()
    }

    /// Bind the sessions manager (T10).
    pub fn bind_sessions(&self, sessions: Arc<dyn SessionActorApi>) -> bool {
        self.sessions.set(sessions).is_ok()
    }

    /// Bind compat (T21).
    pub fn bind_compat(&self, compat: Arc<dyn CompatApi>) -> bool {
        self.compat.set(compat).is_ok()
    }

    /// Bind notifications (T12).
    pub fn bind_notify(&self, notify: Arc<dyn NotifyApi>) -> bool {
        self.notify.set(notify).is_ok()
    }

    pub fn world(&self) -> Option<Arc<dyn WorldApi>> {
        self.world.get().cloned()
    }

    pub fn sessions(&self) -> Option<Arc<dyn SessionActorApi>> {
        self.sessions.get().cloned()
    }

    pub fn compat(&self) -> Option<Arc<dyn CompatApi>> {
        self.compat.get().cloned()
    }

    pub fn notify(&self) -> Option<Arc<dyn NotifyApi>> {
        self.notify.get().cloned()
    }
}

// ---------------------------------------------------------------------------
// AppState
// ---------------------------------------------------------------------------

/// Everything the boot assembled, kept together.
///
/// Cloning is cheap on purpose — every field is an `Arc` or a `Db` (itself a
/// handle), so the struct is a bundle of references to the one set of
/// services, never a second copy of them. The services of the other tasks
/// live in [`Wired`] (`lifecycle::wiring`, T09b).
#[derive(Clone)]
pub struct AppState {
    pub paths: Arc<AppPaths>,
    pub db: Db,
    pub config: Arc<ConfigStore>,
    pub supervisor: Arc<Supervisor>,
    pub lifecycle: Arc<Lifecycle>,
    pub services: Arc<Services>,
    pub wired: Arc<Wired>,
    /// The sidecar (also in `wired`; kept by name for the accounts command's
    /// fallback construction).
    pub sidecar: Arc<SidecarManager>,
    /// Unix millis at which the boot's service phase finished.
    pub booted_at: i64,
    /// Set by the first [`shutdown`] so a second one (the signal handler's exit
    /// arriving after `RunEvent::Exit`, or the reverse) is a cheap no-op.
    shutting_down: Arc<AtomicBool>,
}

impl AppState {
    /// True the first time it is called, false ever after. The whole point is
    /// the side effect.
    pub fn begin_shutdown(&self) -> bool {
        !self.shutting_down.swap(true, Ordering::SeqCst)
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    /// Constructor, kept explicit rather than deriving `Default`: forgetting a
    /// service here would give the shutdown path a hole to leak processes
    /// through.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        paths: Arc<AppPaths>,
        db: Db,
        config: Arc<ConfigStore>,
        supervisor: Arc<Supervisor>,
        lifecycle: Arc<Lifecycle>,
        services: Arc<Services>,
        wired: Arc<Wired>,
        booted_at: i64,
    ) -> AppState {
        AppState {
            sidecar: wired.sidecar.clone(),
            paths,
            db,
            config,
            supervisor,
            lifecycle,
            services,
            wired,
            booted_at,
            shutting_down: Arc::new(AtomicBool::new(false)),
        }
    }
}

// ---------------------------------------------------------------------------
// shutdown
// ---------------------------------------------------------------------------

static EARLY_SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// How long the whole shutdown may take before the app stops waiting: the
/// 6 s quit bound (§10.2), the sidecar's own escalation and the final sweep.
/// A quit must never turn into a beachball; whatever is left belongs to the
/// next boot's sweep.
pub const SHUTDOWN_BUDGET: Duration = Duration::from_secs(12);

/// The part of [`SHUTDOWN_BUDGET`] kept back for closing the instance row
/// (A40.2): whatever the steps before it took, the row is closed, so a clean
/// quit is never read as a crash on the next boot.
pub const CLOSE_RESERVE: Duration = Duration::from_millis(1_500);

/// What one [`shutdown`] did. Logged, and asserted on by `tests/boot.rs`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShutdownReport {
    /// Session items the quit sweep handled.
    pub killed: usize,
    /// Vendor pids still alive after the bound — recorded for the next boot
    /// sweep, never SIGKILLed (contract §10.2, C5).
    pub survivors: usize,
    pub sidecar_stopped: bool,
    pub instance_closed: bool,
}

/// Take this instance down (contract §10.2 steps 3–5).
///
/// When `app_quit` already ran the quit sweep (the record is written and the
/// sessions are stopped), this only stops the sidecar, sweeps what is left of
/// the registry and closes the instance. An exit without `app_quit` (no
/// In-Gilde session, a signal, the last window with nothing running) runs the
/// same sweep with the registry's own view of the sessions.
///
/// Nothing here may panic or return an error: every step reports and moves on.
pub async fn shutdown(state: &AppState) -> ShutdownReport {
    let mut report = ShutdownReport::default();
    // A40.2: one deadline for steps 1–3; step 4 always runs after it.
    let deadline = tokio::time::Instant::now() + SHUTDOWN_BUDGET.saturating_sub(CLOSE_RESERVE);
    let left = || deadline.saturating_duration_since(tokio::time::Instant::now());

    // 1. Sessions (unless `app_quit` did it): record first, then the bounded
    //    stop sweep. Never SIGKILL a vendor process.
    if let Some(quit) = state.lifecycle.quit() {
        if !quit.is_done() && !quit.in_progress() {
            let preview = crate::lifecycle::boot::current_quit_preview(state).await;
            let sessions = state.services.sessions();
            let r = quit.run(preview, sessions).await;
            report.killed = r.total as usize;
            report.survivors = r.survivor_pids.len();
        }
    }

    // 2. The sidecar (`mesh.shutdown`, then its own escalation for node).
    match tokio::time::timeout(left(), state.wired.sidecar.shutdown()).await {
        Ok(Ok(())) => {
            report.sidecar_stopped = true;
            tracing::info!(target: "sidecar", "sidecar shut down");
        }
        Ok(Err(err)) => tracing::warn!(target: "sidecar", error = %err, "sidecar shutdown failed"),
        Err(_) => tracing::warn!(target: "sidecar", "the sidecar did not stop within the shutdown budget"),
    }

    // The services of the other tasks: discovery, the follower, PaneHost,
    // World's publisher, the IDE diff copies (all after the sessions).
    state.wired.shutdown();

    // 3. Whatever the registry still holds of this instance (helpers): fenced
    //    TERM, KILL only for non-vendor helpers.
    //    Quit survivors are left for the next boot (A40.1); the rest is
    //    signalled concurrently inside what is left of the budget (A40.2).
    let leftovers = crate::supervise::reaper::shutdown_sweep_within(&state.supervisor, left()).await;
    report.survivors += leftovers.iter().map(|r| r.held().len()).sum::<usize>();

    // 4. The row. An `ended_at` that is set is the difference between "quit"
    //    and "crashed" for the next boot's sweep.
    match state.supervisor.close_instance().await {
        Ok(()) => report.instance_closed = true,
        Err(err) => {
            tracing::warn!(target: "supervise", error = %err, "could not close the app instance row")
        }
    }

    report
}

/// [`shutdown`] from a synchronous exit hook, bounded by [`SHUTDOWN_BUDGET`].
///
/// Runs at most once per process: `RunEvent::Exit` and the signal handler can
/// both arrive, and the second one must not restart a `ps` sweep on the way
/// out.
pub fn shutdown_blocking(app: &tauri::AppHandle) -> Option<ShutdownReport> {
    let Some(state) = app.try_state::<AppState>().map(|s| s.inner().clone()) else {
        // The exit came before the background boot built the services: sweep
        // what the registry holds and close the row.
        let lifecycle = app.try_state::<Arc<Lifecycle>>()?.inner().clone();
        let sup = lifecycle.supervisor()?;
        if !EARLY_SHUTDOWN.swap(true, Ordering::SeqCst) {
            tauri::async_runtime::block_on(async move {
                let _ = tokio::time::timeout(SHUTDOWN_BUDGET.saturating_sub(CLOSE_RESERVE), async {
                    if let Some(quit) = lifecycle.quit() {
                        if !quit.is_done() && !quit.in_progress() {
                            let preview = crate::lifecycle::boot::preview_for(&Services::default(), &sup).await;
                            quit.run(preview, None).await;
                        }
                    }
                })
                .await;
                // A40.2: the row is closed whatever the quit took.
                let _ = tokio::time::timeout(CLOSE_RESERVE, sup.close_instance()).await;
            });
        }
        return None;
    };
    if !state.begin_shutdown() {
        tracing::debug!(target: "supervise", "shutdown already in progress");
        return None;
    }
    tauri::async_runtime::block_on(async move {
        match tokio::time::timeout(SHUTDOWN_BUDGET, shutdown(&state)).await {
            Ok(report) => Some(report),
            Err(_) => {
                tracing::warn!(
                    target: "supervise",
                    budget_secs = SHUTDOWN_BUDGET.as_secs(),
                    "shutdown exceeded its budget; the next boot's reaper owns the rest"
                );
                // A40.2: never leave the instance open (a clean quit must not
                // read as a crash); closing twice is harmless.
                let _ = tokio::time::timeout(CLOSE_RESERVE, state.supervisor.close_instance()).await;
                None
            }
        }
    })
}

// ---------------------------------------------------------------------------
// single instance
// ---------------------------------------------------------------------------

/// Filename of the focus channel, beside `instance.lock` in the app data dir.
const FOCUS_FILE: &str = "instance.focus";

/// How often the running instance looks for a focus request. A second launch is
/// a user action, so half a second is invisible, and one `stat` of one file is
/// far cheaper than a watcher on the whole app data directory (which the
/// signature reload and the config store already watch for their own reasons).
pub const FOCUS_POLL: Duration = Duration::from_millis(500);

/// The `instance.lock` guard of DESIGN §10-1.
///
/// The lock is the open file descriptor: `flock(2)` on macOS releases when the
/// last descriptor for the open file is closed, which includes the process
/// dying for any reason — a crash therefore leaves no stale lock, only a stale
/// pid in the file, and a pid is advisory here (it is logged, never signalled).
#[derive(Debug)]
pub struct InstanceLock {
    file: File,
    path: PathBuf,
}

impl InstanceLock {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Where the *other* instance writes its "please come to the front".
    pub fn focus_path(&self) -> PathBuf {
        focus_path(&self.path)
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        // Emptying the file first means a reader that opens it between the
        // unlock and the next launch sees no pid rather than a dead one.
        let _ = self.file.set_len(0);
        // SAFETY: `self.file` is open for the whole call; `LOCK_UN` on an
        // unlocked descriptor is not an error either.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// The result of trying to become *the* instance.
#[derive(Debug)]
pub enum LockOutcome {
    /// We hold the lock. Keep the value alive for the life of the process.
    Acquired(InstanceLock),
    /// Somebody else holds it; `pid` is what they wrote, when it was readable.
    Held { pid: Option<i32> },
}

/// `<app-data>/instance.focus` for a given `<app-data>/instance.lock`.
pub fn focus_path(lock: &Path) -> PathBuf {
    lock.parent()
        .unwrap_or_else(|| Path::new("."))
        .join(FOCUS_FILE)
}

/// Try to take the single-instance lock.
///
/// An I/O error is *not* "somebody else has it" — a read-only app data
/// directory would otherwise make the app unlaunchable — so it is returned and
/// the caller boots anyway with a warning.
pub fn acquire(path: &Path) -> std::io::Result<LockOutcome> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;

    // SAFETY: `file` owns the descriptor for the duration of the call.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        return match err.raw_os_error() {
            Some(libc::EWOULDBLOCK) => Ok(LockOutcome::Held {
                pid: read_pid(&mut file),
            }),
            _ => Err(err),
        };
    }

    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    write!(file, "{}", std::process::id())?;
    file.flush()?;

    Ok(LockOutcome::Acquired(InstanceLock {
        file,
        path: path.to_path_buf(),
    }))
}

/// The pid the lock holder wrote, if the file still has a readable one.
fn read_pid(file: &mut File) -> Option<i32> {
    let mut text = String::new();
    file.seek(SeekFrom::Start(0)).ok()?;
    file.read_to_string(&mut text).ok()?;
    text.trim().parse().ok()
}

/// Ask the running instance to come to the front.
///
/// Writing a file is the whole protocol. The alternatives all cost more than
/// they are worth here: a socket needs a lifetime and a cleanup story, and
/// activating another process directly needs either an AppleScript spawn (which
/// would have to go through `SpawnGuard` for a window focus) or an Objective-C
/// bridge this build does not link.
pub fn request_focus(lock_path: &Path) -> std::io::Result<()> {
    let path = focus_path(lock_path);
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)?;
    write!(file, "{}", crate::store::now_ms())?;
    file.flush()
}

/// The token a focus listener compares against: the request file's contents,
/// or `None` when there is no request.
fn focus_token(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/// Watch the focus channel and bring the window forward when a second launch
/// asks for it.
///
/// The token at start-up is the baseline, so a request left behind by an
/// earlier run never steals focus at boot.
pub fn spawn_focus_listener(
    app: &tauri::AppHandle,
    lock_path: &Path,
) -> tauri::async_runtime::JoinHandle<()> {
    let app = app.clone();
    let path = focus_path(lock_path);
    let mut last = focus_token(&path);
    tauri::async_runtime::spawn(async move {
        let mut ticker = tokio::time::interval(FOCUS_POLL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let token = focus_token(&path);
            if token.is_some() && token != last {
                tracing::info!(
                    target: "app",
                    "a second launch asked for the window; focusing"
                );
                present_main_window(&app);
            }
            last = token;
        }
    })
}

/// The window `tauri.conf.json` declares. Its label defaults to `main`; the
/// fallback is there so a renamed window in a later build cannot leave the app
/// running with nothing on screen — an invisible window is the one failure the
/// user cannot diagnose.
fn main_window(app: &tauri::AppHandle) -> Option<tauri::WebviewWindow> {
    app.get_webview_window("main")
        .or_else(|| app.webview_windows().into_values().next())
}

/// Show the main window and put it in front. Also the last step of `boot()` —
/// the window stays hidden until the first graph projection is on the wire, so
/// the user never sees an empty canvas fill in.
pub fn present_main_window(app: &tauri::AppHandle) {
    let Some(window) = main_window(app) else {
        tracing::warn!(target: "app", "no window to present");
        return;
    };
    let _ = window.unminimize();
    if let Err(err) = window.show() {
        tracing::warn!(target: "app", error = %err, "could not show the main window");
    }
    if let Err(err) = window.set_focus() {
        tracing::warn!(target: "app", error = %err, "could not focus the main window");
    }
}

/// Hide the main window for the duration of `boot()`.
///
/// `tauri.conf.json` is owned by T00 and does not set `visible: false`; the
/// window is created by `Builder::build`, but nothing is painted until the
/// event loop runs, which is after `setup()` returns. Hiding here and showing
/// in [`present_main_window`] therefore costs no flash and keeps the window
/// definition in one place.
pub fn hide_main_window(app: &tauri::AppHandle) {
    if let Some(window) = main_window(app) {
        let _ = window.hide();
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gilde-instance-{tag}-{}-{}",
            std::process::id(),
            crate::store::now_ms()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn the_first_caller_takes_the_lock_and_writes_its_pid() {
        let dir = temp_dir("first");
        let path = dir.join("instance.lock");

        let held = match acquire(&path).expect("acquire") {
            LockOutcome::Acquired(lock) => lock,
            LockOutcome::Held { .. } => panic!("an unlocked file must be acquirable"),
        };
        assert_eq!(held.path(), path);

        let text = std::fs::read_to_string(&path).expect("read");
        assert_eq!(
            text.trim().parse::<u32>().expect("pid"),
            std::process::id(),
            "the lock file names the process that holds it"
        );

        drop(held);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `flock` is per open file description, so a second `acquire` **in this
    /// same process** is refused exactly the way a second app launch is. That
    /// is the property the guard rests on, and it is worth pinning: an
    /// implementation using a pid file plus `kill(pid, 0)` would pass a
    /// two-process test and fail this one.
    #[test]
    fn a_second_acquire_is_refused_and_reports_the_holders_pid() {
        let dir = temp_dir("second");
        let path = dir.join("instance.lock");

        let first = match acquire(&path).expect("acquire") {
            LockOutcome::Acquired(lock) => lock,
            LockOutcome::Held { .. } => panic!("first acquire"),
        };

        match acquire(&path).expect("second acquire") {
            LockOutcome::Held { pid } => {
                assert_eq!(pid, Some(std::process::id() as i32));
            }
            LockOutcome::Acquired(_) => panic!("the lock is not exclusive"),
        }

        // Releasing hands it to the next launch, and leaves no stale pid.
        drop(first);
        assert_eq!(std::fs::read_to_string(&path).expect("read").trim(), "");
        assert!(matches!(
            acquire(&path).expect("third acquire"),
            LockOutcome::Acquired(_)
        ));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_focus_request_is_a_new_token_beside_the_lock() {
        let dir = temp_dir("focus");
        let lock = dir.join("instance.lock");
        let focus = focus_path(&lock);
        assert_eq!(focus, dir.join("instance.focus"));

        // No request yet: nothing for the listener to compare against.
        assert_eq!(focus_token(&focus), None);

        request_focus(&lock).expect("request");
        let first = focus_token(&focus).expect("token");
        assert!(!first.is_empty());

        // A second request must produce a *different* token, or the listener
        // would ignore it. `now_ms` can repeat inside a millisecond, so the
        // write is what is asserted, not the clock.
        std::thread::sleep(Duration::from_millis(2));
        request_focus(&lock).expect("second request");
        assert_ne!(focus_token(&focus).as_deref(), Some(first.as_str()));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unwritable_directory_is_an_error_not_a_false_conflict() {
        // A path whose parent cannot be created: booting must not mistake this
        // for "another instance is running" and quit.
        let path = Path::new("/dev/null/nope/instance.lock");
        assert!(acquire(path).is_err());
    }
}
