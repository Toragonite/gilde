//! The Gilde 2 boot (contract §2.4) and what shutdown takes back down (§10.2).
//!
//! Owner: T09.
//!
//!   * **T-boot-1** — `setup()` (`lifecycle::boot::setup_core`) returns in
//!     ≤ 150 ms with a cold `cache/path.json` and a sleeping login shell, and
//!     starts no process at all;
//!   * **T-boot-2** — no `proc_registry` row is planned before
//!     `app_instance.window_visible_at` (a spawn waits for the gate the boot
//!     sweep opens after the window), and no sidecar row exists before the
//!     first command that needs it;
//!   * **T-boot-memoryOnly** — a read-only `GILDE_HOME` boots in memory-only
//!     mode: no crash, the notice, `DISK_FULL` on spawn;
//!   * shutdown — every child of this instance and the sidecar end, the
//!     `app_instance` row closes, a second shutdown is refused;
//!   * A38 (test hermeticity, G7-12/13/14) — the core this file boots runs no
//!     login shell and no vendor CLI: `Booted` sits on a [`Hermetic`] tree,
//!     pins detection to it and pins `SHELL`; the reviewer's probes are kept
//!     as regression tests (the shutdown test re-run under `env -i` with a
//!     marker `SHELL` and marker vendor scripts runs none of them);
//!   * `full_path_smoke` — the Rust half of `scripts/smoke.mjs` (ignored).

mod fixtures;

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gilde_code_lib::config::ConfigStore;
use gilde_code_lib::contract::activity::ExitInfo;
use gilde_code_lib::contract::common::{AbsPath, EpochMs};
use gilde_code_lib::contract::ids::{PtyId, SessionUid};
use gilde_code_lib::contract::process::ProcKind as CKind;
use gilde_code_lib::contract::services::{PtyApi, PtyExitSink, PtyOpenSpec, SpawnRequest, SupervisorApi};
use gilde_code_lib::contract::ErrorCode;
use gilde_code_lib::lifecycle::boot::{self, Lifecycle, SetupOutcome, SetupOutput};
use gilde_code_lib::lifecycle::wiring;
use gilde_code_lib::paths::AppPaths;
use gilde_code_lib::pty::PtyManager;
use gilde_code_lib::sidecar::SidecarRuntime;
use gilde_code_lib::state::{self, AppState, Services};
use gilde_code_lib::store::models::ProcKind;
use gilde_code_lib::store::{dao, Db};
use gilde_code_lib::supervise::health::HealthState;
use gilde_code_lib::supervise::registry::Supervisor;
use gilde_code_lib::supervise::spawn::is_alive;

use serde_json::json;

use fixtures::*;

/// How long a process may take to die before the test calls it a survivor.
const SETTLE: Duration = Duration::from_secs(10);

async fn setup(paths: AppPaths) -> Box<SetupOutput> {
    match boot::setup_core(paths, "0.0.0-test", "build-test").await.expect("setup") {
        SetupOutcome::Ready(out) => out,
        SetupOutcome::SecondInstance { .. } => panic!("no other instance holds the lock"),
    }
}

// ---------------------------------------------------------------------------
// T-boot-1
// ---------------------------------------------------------------------------

/// T-boot-1, functional (A10.7): `setup()` spawns nothing and never runs the
/// login shell. Its time is checked against a 10× ceiling here; the 150 ms
/// budget itself is the G5 bench below (CI's first public run measured
/// 173 ms on a cold runner disk, all of it in B2.db).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_boot_1_setup_returns_fast_and_spawns_nothing() {
    t_boot_1(Duration::from_millis(1500)).await;
}

/// T-boot-1's budget (§2.4: `setup()` ≤ 150 ms), a G5 benchmark (A10.7).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "G5 benchmark (A10.7): T-boot-1's 150 ms budget, run on a quiet machine"]
async fn bench_t_boot_1_setup_within_150_ms() {
    t_boot_1(Duration::from_millis(150)).await;
}

async fn t_boot_1(budget: Duration) {
    let h = Hermetic::new();
    // A login shell that would sleep for half a minute and leave a mark.
    let mark = h.tmp_dir().join("shell-ran");
    let shell = h.bin_dir().join("slow-shell");
    std::fs::write(
        &shell,
        format!("#!/bin/sh\ntouch {}\nsleep 30\n", mark.display()),
    )
    .expect("fake shell");
    std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let _env = h.enter();
    // The sleeping shell stands in for `SHELL` only while `setup()` runs; the
    // hermetic pin comes back afterwards, also on a panic (A38.2: never a
    // process-wide removal). Still under `_env`'s lock.
    let shell_swap = RestoreVar::swap("SHELL", &shell);

    let paths = AppPaths::for_home(&h.home());
    // A previous launch created and migrated `state.db` (first-run migration
    // has its own budget, §2.4 B2); the PATH cache stays cold.
    drop(setup(paths.clone()).await);
    assert!(!paths.path_cache().exists(), "a cold path cache");
    let started = Instant::now();
    let out = setup(paths.clone()).await;
    let took = started.elapsed();
    drop(shell_swap);
    assert_eq!(std::env::var("SHELL").as_deref(), Ok(NO_SHELL), "the hermetic SHELL is back");
    let (_, phases) = out.lifecycle.timer_line();
    assert!(
        took <= budget,
        "setup() took {took:?} (contract §2.4, T-boot-1: ≤ 150 ms; this run's bound {budget:?}): {phases}"
    );
    assert!(!mark.exists(), "setup() must not run the login shell");
    assert!(!paths.path_cache().exists(), "the PATH probe is not on the boot path");
    let me = out.supervisor.app_instance_id().to_string();
    let rows = out
        .db
        .read(move |conn| dao::proc::rows_of_instance(conn, &me))
        .await
        .expect("rows");
    assert!(rows.is_empty(), "setup() planned a process: {rows:?}");
    assert!(!out.lifecycle.gate.is_open(), "the gate opens only after the boot sweep");
}

// ---------------------------------------------------------------------------
// T-boot-2
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t_boot_2_no_row_before_the_window_and_no_early_sidecar() {
    let h = Hermetic::new();
    let paths = AppPaths::for_home(&h.home());
    let out = setup(paths).await;
    let sup = out.supervisor.clone();
    gilde_code_lib::supervise::spawn::allow_executable(
        std::path::Path::new("/bin/sh"),
        &[CKind::Probe],
    );

    // A spawn attempted right after setup waits for the gate.
    let early = {
        let sup = sup.clone();
        tokio::spawn(async move {
            let t = Instant::now();
            let ticket = SupervisorApi::plan_spawn(
                sup.as_ref(),
                SpawnRequest {
                    kind: CKind::Probe,
                    argv0: AbsPath::parse("/bin/sh").unwrap(),
                    args: vec!["-c".into(), "true".into()],
                    cwd: None,
                    env_keys: Vec::new(),
                    parent_uid: None,
                    generation: None,
                    turn_id: None,
                },
            )
            .await;
            (ticket, t.elapsed())
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!early.is_finished(), "a spawn must wait for the spawn gate");

    // B5 then B6.
    let visible = gilde_code_lib::store::now_ms();
    out.lifecycle.window_visible(visible);
    sup.mark_window_visible(visible).await.expect("window visible");
    boot::run_boot_sweep(&out.lifecycle, &sup).await;
    let (ticket, waited) = early.await.expect("join");
    let ticket = ticket.expect("planned after the gate opened");
    assert!(waited >= Duration::from_millis(300));
    SupervisorApi::mark_exited(
        sup.as_ref(),
        &ticket,
        gilde_code_lib::contract::activity::ExitInfo { code: Some(0), signal: None },
    )
    .await;

    let me = sup.app_instance_id().to_string();
    let (before, at, rows) = out
        .db
        .read(move |conn| {
            let at = dao::app_instance::window_visible_at(conn, &me)?.expect("window_visible_at");
            Ok((dao::proc::planned_before(conn, &me, at)?, at, dao::proc::rows_of_instance(conn, &me)?))
        })
        .await
        .expect("read");
    assert_eq!(at, visible);
    assert!(before.is_empty(), "rows planned before the window: {before:?}");
    assert!(!rows.is_empty(), "the gated spawn wrote its row");
    assert!(
        !rows.iter().any(|r| r.kind == ProcKind::Sidecar),
        "no sidecar row before the first command that needs it (B12)"
    );
}

// ---------------------------------------------------------------------------
// T-boot-memoryOnly
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_boot_memory_only_on_a_read_only_gilde_home() {
    let h = Hermetic::new();
    let paths = AppPaths::for_home(&h.home());
    // The folder `GILDE_HOME`'s app data lives in cannot be written.
    let parent = paths.app_data_dir().parent().expect("parent").to_path_buf();
    std::fs::create_dir_all(&parent).expect("parent");
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o500)).expect("ro");

    let out = setup(paths.clone()).await;
    let restore = || {
        let _ = std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700));
    };
    let lc: &Lifecycle = &out.lifecycle;
    assert!(lc.memory_only.is_active(), "memory-only mode on a read-only GILDE_HOME");
    assert_eq!(lc.memory_only.reason_str().as_deref(), Some("readOnly"));
    assert!(out.db.is_memory_only());
    assert_eq!(lc.feed.boot().memory_only.as_deref(), Some("readOnly"));
    assert!(
        lc.feed.notices().iter().any(|n| n.key == "notice.memoryOnly"),
        "one notice says so"
    );

    lc.window_visible(gilde_code_lib::store::now_ms());
    boot::run_boot_sweep(lc, &out.supervisor).await;
    gilde_code_lib::supervise::spawn::allow_executable(std::path::Path::new("/bin/sh"), &[CKind::Probe]);
    let err = SupervisorApi::plan_spawn(
        out.supervisor.as_ref(),
        SpawnRequest {
            kind: CKind::Probe,
            argv0: AbsPath::parse("/bin/sh").unwrap(),
            args: vec!["-c".into(), "true".into()],
            cwd: None,
            env_keys: Vec::new(),
            parent_uid: None,
            generation: None,
            turn_id: None,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::DiskFull, "a spawn needs a durable row");
    assert_eq!(SupervisorApi::memory_only(out.supervisor.as_ref()).as_deref(), Some("readOnly"));
    restore();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_only_resumes_on_disk_when_writes_work_again() {
    let h = Hermetic::new();
    let paths = AppPaths::for_home(&h.home());
    let parent = paths.app_data_dir().parent().expect("parent").to_path_buf();
    std::fs::create_dir_all(&parent).expect("parent");
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o500)).expect("ro");
    let out = setup(paths.clone()).await;
    assert!(out.db.is_memory_only());
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).expect("rw");
    // What the 60 s re-check does when the probe succeeds.
    assert_eq!(out.lifecycle.memory_only.check(paths.app_data_dir()), None);
    out.db.resume_on_disk().expect("resume");
    out.lifecycle.sync_memory_only();
    assert!(!out.db.is_memory_only());
    assert!(paths.state_db().exists(), "state.db is written again");
    assert!(out.lifecycle.feed.notices().is_empty());
}

// ---------------------------------------------------------------------------
// shutdown
// ---------------------------------------------------------------------------

fn repo_sidecar_bundle() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("sidecar").join("sidecar.cjs")
}

/// The vendored `gilde-node` when the tree has one, else the developer's `node`.
fn node_binary() -> PathBuf {
    let vendored = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("binaries")
        .join("gilde-node-aarch64-apple-darwin");
    if vendored.is_file() {
        return vendored;
    }
    which::which("node").expect("a node runtime is required for the boot tests")
}

/// One process variable swapped under a lock the caller already holds (a
/// `Hermetic::enter` guard) and put back when dropped, also on a panic.
struct RestoreVar {
    key: &'static str,
    saved: Option<std::ffi::OsString>,
}

impl RestoreVar {
    fn swap(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> RestoreVar {
        let saved = std::env::var_os(key);
        std::env::set_var(key, value);
        RestoreVar { key, saved }
    }
}

impl Drop for RestoreVar {
    fn drop(&mut self) {
        match self.saved.take() {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}

/// The services the background boot builds — the real wiring
/// (`lifecycle::wiring`, A22) with the OS boundary faked: no window, no
/// notification, no Dock, discovery over a fixture process table.
///
/// Hermetic (A38.2): it sits on a [`Hermetic`] tree (whose guard refuses to
/// start when a vendor CLI is reachable from this process), pins detection to
/// a fixture dir ([`fixtures::detection_pins_at`]: `pathOverride`, the vendor
/// binaries, `terminal.shell`) and pins the process `SHELL` to [`NO_SHELL`]
/// for its whole life, so the sidecar's binary resolution
/// (`configure_params` → `resolve_vendor` → `resolve_path`) can reach neither
/// a login shell nor a real CLI.
struct Booted {
    state: AppState,
    pty: Arc<PtyManager>,
    _shell: EnvPin,
    h: Hermetic,
}

/// Exits of the PTYs a test opened (nothing to do with them here).
struct NoExits;

impl PtyExitSink for NoExits {
    fn exited(&self, _pty: &PtyId, _exit: ExitInfo, _at: EpochMs) {}
}

impl Booted {
    async fn new() -> Booted {
        Booted::boot(None).await
    }

    /// `fake_bin`: the dir detection is pinned to (the smoke run's
    /// `--fake-bin`); `None` = this tree's own `bin/`.
    async fn boot(fake_bin: Option<PathBuf>) -> Booted {
        let h = Hermetic::new();
        let shell = EnvPin::set(&[("SHELL", NO_SHELL)]);
        let home = std::fs::canonicalize(h.home()).expect("canonicalize");
        let paths = Arc::new(AppPaths::for_home(&home));
        paths.ensure_all().expect("ensure");
        let db = Db::open(&paths.state_db()).expect("db");
        let config = Arc::new(ConfigStore::open_at(&paths.config_file()).expect("config"));
        config
            .patch(json!({"agents": {"envPassthrough": ["GILDE_TEST_MODE"]}}))
            .expect("patch");
        config
            .patch(fixtures::detection_pins_at(&fake_bin.unwrap_or_else(|| h.bin_dir())))
            .expect("detection pins");
        config.flush().expect("flush the pins");
        let supervisor = Arc::new(
            Supervisor::open_instance(&db, config.clone(), "0.0.0-test")
                .await
                .expect("open_instance"),
        );
        let lifecycle = Lifecycle::new("0.0.0-test", "build-test");
        lifecycle.set_supervisor(supervisor.clone());
        // The resolved node is allowlisted for the sidecar and its self-test,
        // as the sidecar manager does once it resolved it (A13.3).
        gilde_code_lib::supervise::spawn::allow_executable(
            std::path::Path::new(&node_binary()),
            &[CKind::Sidecar, CKind::Probe],
        );
        let services = Services::new();
        let core = wiring::Core {
            paths: paths.clone(),
            db: db.clone(),
            config: config.clone(),
            lifecycle: lifecycle.clone(),
            supervisor: supervisor.clone(),
            services: services.clone(),
            health: HealthState::new(),
        };
        let (platform, _recorded) =
            wiring::fakes::platform(SidecarRuntime::explicit(node_binary(), repo_sidecar_bundle()));
        let (discovery, procs) = wiring::fakes::hermetic_discovery(&home);
        let wired = wiring::wire(
            &core,
            platform,
            wiring::Options {
                discovery: Some(discovery),
                procs: Some(procs),
                follow_watch: false,
                background: false,
                runtime: tokio::runtime::Handle::current(),
            },
        )
        .await
        .expect("wired");
        let wired = Arc::new(wired);
        let pty = wired.pty.clone();
        let state = AppState::new(
            paths,
            db,
            config,
            supervisor,
            lifecycle,
            services,
            wired,
            gilde_code_lib::store::now_ms(),
        );
        Booted { state, pty, _shell: shell, h }
    }

    /// A pty session running the `fake_agent` fixture in `hang` mode, opened
    /// through `PtyApi::open` (the shim of the pre-Gilde-2 call shape is gone,
    /// A22.2); its row carries a session uid, so the quit sweep counts it.
    async fn open_hanging_session(&self) -> i32 {
        // The fixture stands in for a resolved agent binary: allowlisted for
        // the `pty` kind before it is planned (§9.3, A12.8).
        gilde_code_lib::supervise::spawn::allow_executable(&fake_agent_path(), &[CKind::Pty]);
        // The child sees this tree only: its fixture dir and the system floor, its home.
        let env = vec![
            ("PATH".to_string(), format!("{}:/usr/bin:/bin", self.h.bin_dir().display())),
            ("HOME".to_string(), self.h.home().to_string_lossy().into_owned()),
            ("SHELL".to_string(), NO_SHELL.to_string()),
        ];
        let spec = PtyOpenSpec {
            kind: CKind::Pty,
            argv0: AbsPath::parse(fake_agent_path().to_string_lossy().into_owned()).expect("abs"),
            args: vec!["hang".to_string()],
            cwd: AbsPath::parse(std::fs::canonicalize(self.h.home()).expect("home").to_string_lossy().into_owned())
                .expect("abs"),
            env,
            cols: 80,
            rows: 24,
            parent_uid: Some(SessionUid::mint()),
        };
        let info = PtyApi::open(self.pty.as_ref(), spec, Arc::new(NoExits)).await.expect("session opens");
        assert!(is_alive(info.pid), "the child is running before shutdown");
        info.pid
    }

    /// A40.2: a session that outlives the quit — the fixture with SIGHUP and
    /// SIGTERM ignored (`trap ''` survives `exec`), in a PTY a Gilde session
    /// owns, so it is one of Gilde's vendor rows (A39.1(b)).
    async fn open_stubborn_session(&self) -> i32 {
        gilde_code_lib::supervise::spawn::allow_executable(Path::new("/bin/sh"), &[CKind::Pty]);
        let env = vec![
            ("PATH".to_string(), format!("{}:/usr/bin:/bin", self.h.bin_dir().display())),
            ("HOME".to_string(), self.h.home().to_string_lossy().into_owned()),
            ("SHELL".to_string(), NO_SHELL.to_string()),
        ];
        let script = format!("trap '' HUP TERM; exec '{}' ignore-term", fake_agent_path().display());
        let spec = PtyOpenSpec {
            kind: CKind::Pty,
            argv0: AbsPath::parse("/bin/sh".to_string()).expect("abs"),
            args: vec!["-c".to_string(), script],
            cwd: AbsPath::parse(std::fs::canonicalize(self.h.home()).expect("home").to_string_lossy().into_owned())
                .expect("abs"),
            env,
            cols: 80,
            rows: 24,
            parent_uid: Some(SessionUid::mint()),
        };
        let info = PtyApi::open(self.pty.as_ref(), spec, Arc::new(NoExits)).await.expect("session opens");
        assert!(is_alive(info.pid), "the child is running before shutdown");
        info.pid
    }
}

async fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    predicate()
}

/// A40.1/A40.2 (G7c-02 budget variant): four sessions survive the quit. The
/// shutdown stays inside `SHUTDOWN_BUDGET`, never SIGKILLs them, leaves their
/// rows to the next boot as quit survivors, and closes the instance row (a
/// clean quit is never read as a crash).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shutdown_with_four_survivors_stays_in_budget_and_closes_the_instance() {
    let booted = Booted::new().await;
    let state = booted.state.clone();
    let mut pids = Vec::new();
    for _ in 0..4 {
        pids.push(booted.open_stubborn_session().await);
    }
    // Let the shell reach its `exec`.
    tokio::time::sleep(Duration::from_millis(400)).await;

    let started = Instant::now();
    let report = state::shutdown(&state).await;
    let took = started.elapsed();
    let alive: Vec<bool> = pids.iter().map(|p| is_alive(*p)).collect();
    let instance = state.supervisor.app_instance_id().to_string();
    let rows = state
        .db
        .read(move |conn| dao::proc::rows_of_instance(conn, &instance))
        .await
        .expect("registry rows");
    let open = state.db.read(dao::app_instance::list_open).await.expect("open instances");
    // Cleanup of the processes this test started, before asserting.
    for pid in &pids {
        unsafe {
            libc::kill(*pid, libc::SIGKILL);
        }
    }

    assert!(took < state::SHUTDOWN_BUDGET, "the shutdown took {took:?}: {report:?}");
    assert!(report.instance_closed, "{report:?}");
    let id = state.supervisor.app_instance_id().to_string();
    assert!(!open.iter().any(|i| i.id == id), "app_instance.ended_at was not filled");
    assert!(alive.iter().all(|a| *a), "a Gilde vendor session was SIGKILLed: {alive:?}");
    assert!(report.survivors >= 4, "{report:?}");
    for pid in &pids {
        let row = rows.iter().find(|r| r.pid == Some(i64::from(*pid))).expect("row");
        assert_eq!(
            (row.state, row.note.as_deref()),
            (gilde_code_lib::store::models::ProcState::Running, Some(gilde_code_lib::lifecycle::quit::NOTE_QUIT_SURVIVOR)),
            "the shutdown sweep left the quit survivor for the next boot: {row:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_ends_every_pty_child_and_the_sidecar_and_closes_the_instance_row() {
    let booted = Booted::new().await;
    let state = booted.state.clone();

    let first = booted.open_hanging_session().await;
    let second = booted.open_hanging_session().await;
    state.sidecar.start().await.expect("the sidecar starts");
    let instance = state.supervisor.app_instance_id().to_string();
    let sidecar_pid = state
        .db
        .read(move |conn| dao::proc::rows_of_instance(conn, &instance))
        .await
        .expect("registry rows")
        .into_iter()
        .find(|row| row.kind == ProcKind::Sidecar && row.state.is_live())
        .and_then(|row| row.pid)
        .expect("a started sidecar has a live registry row with a pid") as i32;
    assert!(is_alive(sidecar_pid), "the sidecar is running before shutdown");

    let started = Instant::now();
    let report = state::shutdown(&state).await;
    assert_eq!(report.survivors, 0, "no pid may outlive the shutdown: {report:?}");
    assert!(report.sidecar_stopped, "the sidecar was asked to stop");
    assert!(report.instance_closed, "the app_instance row was closed");
    assert!(report.killed >= 2, "both pty sessions were in the quit record, got {}", report.killed);
    assert!(started.elapsed() < state::SHUTDOWN_BUDGET);

    for pid in [first, second, sidecar_pid] {
        assert!(wait_until(SETTLE, || !is_alive(pid)).await, "pid {pid} survived the shutdown");
    }

    let instance = state.supervisor.app_instance_id().to_string();
    let rows = state
        .db
        .read(move |conn| dao::proc::rows_of_instance(conn, &instance))
        .await
        .expect("registry rows");
    for row in &rows {
        assert!(
            !row.state.is_live(),
            "{:?} row {} is still {:?} after shutdown",
            row.kind,
            row.spawn_id,
            row.state
        );
    }
    let open = state.db.read(dao::app_instance::list_open).await.expect("open instances");
    let id = state.supervisor.app_instance_id().to_string();
    assert!(!open.iter().any(|i| i.id == id), "app_instance.ended_at was not filled");

    // The exit wrote a quit record for the two sessions (§10.2 step 3).
    let records = state
        .db
        .read(|conn| dao::quit_record::list_open(conn, gilde_code_lib::store::now_ms()))
        .await
        .expect("records");
    assert_eq!(records.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_shutdown_is_refused_by_the_gate() {
    let booted = Booted::new().await;
    let state = booted.state.clone();
    assert!(!state.is_shutting_down());
    assert!(state.begin_shutdown(), "the first caller goes through");
    assert!(state.is_shutting_down());
    assert!(!state.begin_shutdown(), "the second caller is turned away");
    let clone = state.clone();
    assert!(!clone.begin_shutdown());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idle_instance_still_closes_its_row() {
    let booted = Booted::new().await;
    let report = state::shutdown(&booted.state).await;
    assert_eq!(report.killed, 0);
    assert_eq!(report.survivors, 0);
    assert!(report.instance_closed);
    let open = booted.state.db.read(dao::app_instance::list_open).await.expect("open instances");
    assert!(open.is_empty(), "no instance row is left open");
}

// ---------------------------------------------------------------------------
// A38 — the core this file boots runs no login shell and no vendor CLI
// (the G7 reviewer's probes G7-12/13/14, g7b `9a9ff02`, kept as regressions)
// ---------------------------------------------------------------------------

fn write_exec(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    std::fs::write(path, body).expect("write");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

/// A tree for re-running one test of this binary under `env -i`: a marker
/// login shell (it logs its argv and answers with a `PATH` holding `fakebin`)
/// and a marker script for every name of [`VENDOR_CLIS`] in `fakebin`. Every
/// marker appends one line to `markers` when it runs; nothing real can run.
struct MarkerRun {
    _t: tempfile::TempDir,
    root: PathBuf,
}

impl MarkerRun {
    fn new() -> MarkerRun {
        let t = tempfile::Builder::new().prefix("g7-boot-").tempdir().expect("tempdir");
        let root = std::fs::canonicalize(t.path()).expect("canonical");
        let run = MarkerRun { _t: t, root };
        for d in [run.home(), run.root.join("gh"), run.fakebin(), run.root.join("tmp")] {
            std::fs::create_dir_all(d).expect("dirs");
        }
        write_exec(
            &run.shell(),
            &format!(
                "#!/bin/sh\necho \"login-shell $*\" >> '{m}'\nprintf '__GILDE_B__%s__GILDE_E__' '{b}:/usr/bin:/bin'\n",
                m = run.markers_file().display(),
                b = run.fakebin().display()
            ),
        );
        run.plant(&run.fakebin());
        run
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }
    fn fakebin(&self) -> PathBuf {
        self.root.join("fakebin")
    }
    fn shell(&self) -> PathBuf {
        self.root.join("marker-shell")
    }
    fn markers_file(&self) -> PathBuf {
        self.root.join("markers")
    }

    /// A marker script for every vendor name in `dir`.
    fn plant(&self, dir: &Path) {
        for v in VENDOR_CLIS {
            write_exec(
                &dir.join(v),
                &format!("#!/bin/sh\necho \"{v} $*\" >> '{}'\necho 1.0.0\n", self.markers_file().display()),
            );
        }
    }

    /// What the markers recorded (empty = nothing ran).
    fn ran(&self) -> String {
        std::fs::read_to_string(self.markers_file()).unwrap_or_default()
    }

    /// Run `test` of this very binary, alone, under `env -i` with the marker
    /// `SHELL`, this tree's `HOME` and `path` as the process `PATH`.
    fn run(&self, test: &str, path: &str) -> (bool, String) {
        let me = std::env::current_exe().expect("this test binary");
        let out = Command::new(me)
            .args([test, "--exact", "--test-threads=1"])
            .env_clear()
            .env("HOME", self.home())
            .env("SHELL", self.shell())
            .env("PATH", path)
            .env("TMPDIR", self.root.join("tmp"))
            .env("GILDE_HOME", self.root.join("gh"))
            .env("LANG", "en_US.UTF-8")
            .env("AGENT_CLI_CREDENTIAL_STORE", "memory")
            .env("DISABLE_AUTOUPDATER", "1")
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .expect("re-run this test binary");
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        (out.status.success(), text)
    }
}

/// The last lines of a child's output, for an assertion message.
fn tail(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(25)..].join("\n")
}

/// G7-12 (BLOCKER), the reviewer's probe: the shutdown test re-run under
/// `env -i` with a marker `SHELL` whose `PATH` names marker vendor scripts.
/// Before A38 it ran the login shell and every vendor `--version` (sidecar
/// `configure_params` → `resolve_vendor` → `resolve_path`); now it runs none
/// of them — and still passes.
#[test]
fn g7_12_the_boot_test_runs_no_login_shell_and_no_vendor_cli() {
    let run = MarkerRun::new();
    let (ok, out) = run.run("shutdown_ends_every_pty_child_and_the_sidecar_and_closes_the_instance_row", "/usr/bin:/bin");
    let ran = run.ran();
    println!("PROBE G7-12: shutdown test passed={ok}; executed by the test: {:?}", ran.trim_end());
    assert!(ran.is_empty(), "the boot test ran the login shell or a vendor CLI:\n{ran}");
    assert!(ok, "the shutdown test must still pass under env -i:\n{}", tail(&out));
}

/// G7-12, the two other ways a vendor CLI can be in reach — on the process
/// `PATH`, and on the static detection ladder of the process `HOME` — make
/// the rig refuse to start (the A38.2 guard) instead of running it.
#[test]
fn g7_12_a_reachable_vendor_cli_makes_the_rig_refuse_rather_than_run_it() {
    // (a) on the process PATH
    let run = MarkerRun::new();
    let path = format!("{}:/usr/bin:/bin", run.fakebin().display());
    let (ok, out) = run.run("a_second_shutdown_is_refused_by_the_gate", &path);
    let ran = run.ran();
    println!("PROBE G7-12a: vendor scripts on PATH → passed={ok}, guard={}, ran={:?}", out.contains("hermetic guard"), ran.trim_end());
    assert!(!ok && out.contains("hermetic guard"), "the rig must refuse a PATH that reaches a vendor CLI:\n{}", tail(&out));
    assert!(ran.is_empty(), "nothing may run before the refusal:\n{ran}");

    // (b) on the home ladder (`~/.local/bin`), PATH clean
    let run = MarkerRun::new();
    run.plant(&run.home().join(".local/bin"));
    let (ok, out) = run.run("an_idle_instance_still_closes_its_row", "/usr/bin:/bin");
    let ran = run.ran();
    println!("PROBE G7-12b: vendor scripts in ~/.local/bin → passed={ok}, guard={}, ran={:?}", out.contains("hermetic guard"), ran.trim_end());
    assert!(!ok && out.contains("hermetic guard"), "the rig must refuse a home ladder that reaches a vendor CLI:\n{}", tail(&out));
    assert!(out.contains(".local/bin/claude"), "the refusal names what it found:\n{}", tail(&out));
    assert!(ran.is_empty(), "nothing may run before the refusal:\n{ran}");
}

/// G7-13 (MAJOR): the guard now looks where in-process code looks. The
/// reviewer's probe (`which` over the process `PATH` after `Hermetic::new()`
/// passed) finds nothing; and the guard itself reports a vendor-named file on
/// a `PATH` entry and on the home ladder (nvm and `~/.local/bin`), while a
/// live fixture dir — and only a live one — is exempt. Only `stat`s.
#[test]
fn g7_13_the_hermetic_guard_refuses_a_reachable_vendor_cli() {
    let h = Hermetic::new();
    let reachable: Vec<PathBuf> = VENDOR_CLIS
        .iter()
        .filter_map(|n| which::which(n).ok())
        .filter(|p| !p.starts_with(h.bin_dir()))
        .collect();
    println!("PROBE G7-13: Hermetic::new() passed; vendor CLIs reachable from the process PATH: {reachable:?}");
    assert!(reachable.is_empty(), "Hermetic::new() accepted a process that reaches {reachable:?}");

    // Every name detection may run is guarded (drift check against the product).
    for kind in gilde_code_lib::agents::AgentKind::ALL {
        if kind == gilde_code_lib::agents::AgentKind::Shell {
            continue;
        }
        for name in kind.binary_names() {
            assert!(VENDOR_CLIS.contains(name), "detection may run `{name}` ({kind}) but the guard does not look for it");
        }
    }

    let t = tempfile::tempdir().expect("tempdir");
    let on_path = t.path().join("path-dir");
    write_exec(&on_path.join("claude"), "#!/bin/sh\nexit 0\n");
    assert_eq!(vendor_clis_in(std::slice::from_ref(&on_path)), vec![on_path.join("claude")]);

    let home = t.path().join("home");
    write_exec(&home.join(".local/bin/agy"), "#!/bin/sh\nexit 0\n");
    write_exec(&home.join(".nvm/versions/node/v20.19.2/bin/codex"), "#!/bin/sh\nexit 0\n");
    let ladder = gilde_code_lib::agents::path::static_candidates(&home);
    assert!(ladder.iter().any(|d| d == Path::new("/opt/homebrew/bin")) && ladder.iter().any(|d| d == Path::new("/usr/local/bin")));
    let found = vendor_clis_in(&ladder);
    assert!(found.contains(&home.join(".local/bin/agy")), "{found:?}");
    assert!(found.contains(&home.join(".nvm/versions/node/v20.19.2/bin/codex")), "{found:?}");

    // A live fixture dir is the one place a vendor name may be.
    write_exec(&h.bin_dir().join("claude"), "#!/bin/sh\nexit 0\n");
    assert!(vendor_clis_in(&[h.bin_dir()]).is_empty(), "the fixture dir is exempt");
    h.assert_no_vendor_cli_outside_fixtures();
    let bin = h.bin_dir();
    drop(h);
    std::fs::create_dir_all(&bin).expect("recreate");
    write_exec(&bin.join("claude"), "#!/bin/sh\nexit 0\n");
    assert_eq!(vendor_clis_in(std::slice::from_ref(&bin)), vec![bin.join("claude")], "a dropped tree is no longer exempt");
    let _ = std::fs::remove_dir_all(bin.parent().expect("root"));
}

/// G7-14 (MAJOR): with `pathOverride` set, the resolved search path is the
/// override and the system floor — not `/opt/homebrew/bin`, `/usr/local/bin`
/// or the home ladder — and detection over it probes nothing outside it.
#[tokio::test]
async fn g7_14_a_path_override_pins_the_search_path() {
    use gilde_code_lib::agents::{detect, noop_recorder, AgentKind};
    let t = tempfile::tempdir().expect("tempdir");
    let home = std::fs::canonicalize(t.path()).expect("canonical");
    let bin = home.join("bin");
    std::fs::create_dir_all(&bin).expect("bin");
    // A ladder dir that exists and holds a marker the override must hide.
    let markers = home.join("markers");
    write_exec(&home.join(".local/bin/agy"), &format!("#!/bin/sh\necho agy >> '{}'\necho 1.0.0\n", markers.display()));
    let paths = AppPaths::for_home(&home);
    let mut cfg = gilde_code_lib::config::AppConfig::default();
    cfg.agents.path_override = Some(bin.display().to_string());
    cfg.terminal.shell = Some(NO_SHELL.to_string());
    let report = gilde_code_lib::agents::path::resolve_path(&cfg, &paths);
    let floor = ["/usr/bin", "/bin", "/usr/sbin", "/sbin"];
    let outside: Vec<String> = report
        .entries
        .iter()
        .filter(|e| !e.starts_with(&home) && !floor.contains(&e.to_str().unwrap_or("")))
        .map(|e| e.display().to_string())
        .collect();
    println!("PROBE G7-14: pathOverride={} resolved to {:?}; outside the override and the system floor: {outside:?}", bin.display(), report.entries);
    assert!(outside.is_empty(), "the override does not pin the search path: {outside:?}");
    assert!(!report.contains(home.join(".local/bin")), "the home ladder is not merged: {:?}", report.entries);
    assert_eq!(report.entries.first(), Some(&bin), "the override comes first");

    // Variant: detection over that report runs nothing outside it.
    let found = detect(true, &cfg, &report, None, &noop_recorder()).await.expect("detect");
    assert!(found.iter().all(|b| !b.installed), "nothing is installed in the empty override: {found:?}");
    assert!(found.iter().any(|b| b.kind == AgentKind::Antigravity && !b.installed));
    assert!(!markers.exists(), "the ladder's agy ran: {:?}", std::fs::read_to_string(&markers));
}

// ---------------------------------------------------------------------------
// full_path_smoke — the Rust half of scripts/smoke.mjs (A38.4)
// ---------------------------------------------------------------------------

/// One `SMOKE {json}` line per step (`scripts/smoke.mjs` reads them).
fn smoke(step: &str, mut fields: serde_json::Value) {
    fields["step"] = json!(step);
    println!("SMOKE {fields}");
}

/// The full path with fakes only: boot (pinned to the smoke run's
/// `GILDE_SMOKE_FAKE_BIN`, else to this tree's `bin/` with the fixtures linked
/// in), detection over that dir and nothing else, two PTY sessions, the
/// sidecar resolving the detected binaries, the process registry, and the
/// shutdown. Run by `node scripts/smoke.mjs --fake-bin <dir>`; every step it
/// reports must be ok.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "the smoke run: node scripts/smoke.mjs --fake-bin <dir>"]
async fn full_path_smoke() {
    let fake_bin = std::env::var_os("GILDE_SMOKE_FAKE_BIN").map(PathBuf::from);
    let booted = Booted::boot(fake_bin.clone()).await;
    let bin = match fake_bin {
        Some(dir) => dir,
        None => {
            let bin = booted.h.bin_dir();
            let fake_claude = option_env!("CARGO_BIN_EXE_fake_claude")
                .map(PathBuf::from)
                .unwrap_or_else(|| fake_agent_path().with_file_name("fake_claude"));
            booted.h.install_fixture("claude", &fake_claude);
            for name in ["codex", "cursor-agent", "agy"] {
                booted.h.install_fixture(name, &fake_agent_path());
            }
            bin
        }
    };
    let state = booted.state.clone();
    smoke("boot", json!({"ok": true, "appInstance": state.supervisor.app_instance_id()}));

    let report = state.wired.agents.path_report(true);
    let detected = state.wired.agents.detect(true).await.unwrap_or_default();
    let installed: Vec<_> = detected.iter().filter(|b| b.installed).collect();
    let outside: Vec<String> = installed
        .iter()
        .filter(|b| !b.abs_path.as_deref().is_some_and(|p| p.starts_with(&bin)))
        .map(|b| b.kind.to_string())
        .collect();
    smoke(
        "detect",
        json!({
            "ok": outside.is_empty() && report.entries.first() == Some(&bin),
            "source": report.source.as_str(),
            "found": installed.iter().map(|b| json!({"kind": b.kind.to_string(), "version": b.version})).collect::<Vec<_>>(),
            "outsideFakeBin": outside,
        }),
    );

    let mut pids = Vec::new();
    for _ in 0..2 {
        let pid = booted.open_hanging_session().await;
        smoke("pty", json!({"ok": true, "pid": pid}));
        pids.push(pid);
    }
    match state.sidecar.start().await {
        Ok(()) => smoke("sidecar", json!({"ok": true})),
        Err(err) => smoke("sidecar", json!({"ok": false, "error": err.to_string()})),
    }
    let instance = state.supervisor.app_instance_id().to_string();
    let rows = state
        .db
        .read(move |conn| dao::proc::rows_of_instance(conn, &instance))
        .await
        .expect("registry rows");
    smoke(
        "registry",
        json!({
            "ok": true,
            "rows": rows.iter().map(|r| json!({"pid": r.pid, "kind": format!("{:?}", r.kind), "state": format!("{:?}", r.state)})).collect::<Vec<_>>(),
        }),
    );

    let report = state::shutdown(&state).await;
    smoke(
        "shutdown",
        json!({"ok": report.survivors == 0 && report.instance_closed, "survivors": report.survivors, "killed": report.killed}),
    );
    for pid in pids {
        assert!(wait_until(SETTLE, || !is_alive(pid)).await, "pid {pid} survived the shutdown");
    }
    assert_eq!(report.survivors, 0);
    assert!(outside.is_empty(), "detection found a binary outside the fake bin: {outside:?}");
}
