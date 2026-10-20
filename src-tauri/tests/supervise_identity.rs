//! The three identity fences against **real processes** (DESIGN §8-4, M1-PLAN
//! T08 acceptance 5–6).
//!
//! The unit tests in `supervise::identity` prove the *policy* against a fake
//! probe. These prove the *plumbing*: that `libproc`, `sysinfo` and `ps` on this
//! machine actually answer the questions the policy asks, and — the one that
//! matters most — that a recycled pid does **not** verify and the innocent
//! process holding it is left completely alone.

mod fixtures;

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use gilde_code_lib::config::ConfigStore;
use gilde_code_lib::store::models::{ProcKind, ProcRow};
use gilde_code_lib::store::{dao, Db};
use gilde_code_lib::supervise::identity::{
    verify, verify_with, Fence, LiveProbe, MismatchReason, ProcProbe,
};
use gilde_code_lib::supervise::procscan::{ps_snapshot, Snapshot};
use gilde_code_lib::supervise::registry::{SpawnGuard, SpawnPlan, Supervisor};
use gilde_code_lib::supervise::spawn::is_alive;
use std::os::unix::process::CommandExt as _;
use std::sync::Arc;

const SETTLE: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

struct Harness {
    supervisor: Supervisor,
    db: Db,
    _dir: tempfile::TempDir,
}

async fn harness() -> Harness {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::open_in_memory().expect("db");
    let config =
        Arc::new(ConfigStore::open_at(&dir.path().join("config.json")).expect("config store"));
    let supervisor = Supervisor::open_instance(&db, config, "0.0.0-test")
        .await
        .expect("open_instance");
    Harness {
        supervisor,
        db,
        _dir: dir,
    }
}

impl Harness {
    async fn row(&self, spawn_id: &str) -> ProcRow {
        self.supervisor
            .row(spawn_id)
            .await
            .expect("read row")
            .expect("row exists")
    }

    /// Overwrite pid / start time on a row without going through the guard —
    /// the only way to stage a pid-reuse or a clock mismatch on purpose.
    async fn force(&self, spawn_id: &str, pid: Option<i64>, proc_start_ts: Option<i64>) {
        let spawn_id = spawn_id.to_string();
        self.db
            .write(move |conn| {
                conn.execute(
                    "UPDATE proc_registry SET pid = ?2, pgid = ?2, proc_start_ts = ?3
                      WHERE spawn_id = ?1",
                    rusqlite::params![spawn_id, pid, proc_start_ts],
                )?;
                Ok(())
            })
            .await
            .expect("force row");
    }
}

/// A child that is always killed and reaped, even when a test panics.
struct Kid {
    child: Child,
    reaped: bool,
}

impl Kid {
    fn pid(&self) -> i32 {
        self.child.id() as i32
    }

    fn kill_and_reap(&mut self) {
        if !self.reaped {
            let _ = self.child.kill();
            let _ = self.child.wait();
            self.reaped = true;
        }
    }
}

impl Drop for Kid {
    fn drop(&mut self) {
        self.kill_and_reap();
    }
}

fn fake_agent() -> PathBuf {
    fixtures::fake_agent_path()
}

/// Spawn `argv` in its own process group, optionally with our spawn marker and
/// optionally with **no environment at all** (the "the child wiped its env"
/// case that fence 2 has to cover on its own).
fn spawn_child(argv: &[String], marker: Option<&str>, clear_env: bool) -> Kid {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    if clear_env {
        cmd.env_clear();
    }
    if let Some(marker) = marker {
        cmd.env("GILDE_SPAWN_ID", marker);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    let child = cmd
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {argv:?}: {e}"));
    let kid = Kid {
        child,
        reaped: false,
    };
    wait_until_visible(kid.pid());
    kid
}

/// Wait until `ps` can see the pid, so a test never races the kernel.
fn wait_until_visible(pid: i32) {
    let deadline = Instant::now() + SETTLE;
    loop {
        if ps_snapshot().map(|s| s.contains(pid)).unwrap_or(false) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "pid {pid} never appeared in a ps snapshot"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_until_gone(pid: i32) {
    let deadline = Instant::now() + SETTLE;
    while is_alive(pid) {
        assert!(Instant::now() < deadline, "pid {pid} never exited");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn argv_of(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|p| (*p).to_string()).collect()
}

/// Plan a row, start the process it describes, and mark it running — the exact
/// sequence DESIGN §8-3 prescribes.
async fn register(
    h: &Harness,
    argv: &[String],
    clear_env: bool,
) -> (SpawnGuard, Kid) {
    let guard = h
        .supervisor
        .plan_spawn(SpawnPlan::new(ProcKind::Pty, argv.to_vec(), "/tmp"))
        .await
        .expect("plan_spawn");
    let marker = if clear_env {
        None
    } else {
        Some(guard.env_marker().to_string())
    };
    let kid = spawn_child(argv, marker.as_deref(), clear_env);
    let pid = kid.pid();
    guard.mark_running(pid, pid).expect("mark_running");
    (guard, kid)
}

fn snapshot() -> Snapshot {
    ps_snapshot().expect("ps snapshot")
}

// ---------------------------------------------------------------------------
// (a) a registered, live process verifies
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_registered_live_fake_agent_verifies() {
    let h = harness().await;
    let argv = argv_of(&[&fake_agent().to_string_lossy(), "hang"]);
    let (guard, _kid) = register(&h, &argv, false).await;

    let row = h.row(guard.spawn_id()).await;
    assert_eq!(
        verify(&row, &snapshot()),
        Fence::Verified,
        "a process we started, recorded and can still see must verify"
    );
}

// ---------------------------------------------------------------------------
// (b) a tampered start time is a mismatch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_start_time_ten_seconds_off_is_a_mismatch() {
    let h = harness().await;
    let argv = argv_of(&[&fake_agent().to_string_lossy(), "hang"]);
    let (guard, kid) = register(&h, &argv, false).await;

    let row = h.row(guard.spawn_id()).await;
    let recorded = row.proc_start_ts.expect("start time recorded");
    h.force(guard.spawn_id(), row.pid, Some(recorded + 10)).await;

    let row = h.row(guard.spawn_id()).await;
    assert_eq!(
        verify(&row, &snapshot()),
        Fence::Mismatch(MismatchReason::StartTime),
        "fence 1 is mandatory and nothing else can substitute for it"
    );
    assert!(is_alive(kid.pid()), "a mismatch sends no signal");
}

// ---------------------------------------------------------------------------
// (c) pid reuse — the case that would otherwise kill a stranger's process
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_recycled_pid_never_verifies_and_the_innocent_process_is_untouched() {
    let h = harness().await;
    let argv = argv_of(&[&fake_agent().to_string_lossy(), "hang"]);
    let (guard, mut kid) = register(&h, &argv, false).await;
    let row = h.row(guard.spawn_id()).await;
    let original_start = row.proc_start_ts.expect("start time recorded");

    // The registered process dies …
    kid.kill_and_reap();
    wait_until_gone(kid.pid());

    // … and something completely unrelated takes its place in the row.
    let mut innocent = spawn_child(&argv_of(&["/bin/sleep", "30"]), None, false);
    h.force(
        guard.spawn_id(),
        Some(innocent.pid() as i64),
        Some(original_start),
    )
    .await;

    let row = h.row(guard.spawn_id()).await;
    let verdict = verify(&row, &snapshot());
    assert!(
        matches!(verdict, Fence::Mismatch(_)),
        "a recycled pid must never verify, got {verdict:?}"
    );
    assert!(
        is_alive(innocent.pid()),
        "the process holding the recycled pid must be left alone"
    );

    innocent.kill_and_reap();
}

// ---------------------------------------------------------------------------
// (d) a process with no environment is still verified by argv
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_child_with_a_wiped_environment_is_verified_by_its_argv() {
    let h = harness().await;
    let argv = argv_of(&[&fake_agent().to_string_lossy(), "hang"]);
    let (guard, kid) = register(&h, &argv, true).await;

    let row = h.row(guard.spawn_id()).await;
    assert_eq!(
        gilde_code_lib::supervise::procscan::environ_of(kid.pid())
            .map(|e| e.iter().any(|v| v.starts_with("GILDE_SPAWN_ID=")))
            .unwrap_or(false),
        false,
        "this child deliberately carries no marker"
    );
    assert_eq!(
        verify(&row, &snapshot()),
        Fence::Verified,
        "fence 2 must carry a child that fence 3 cannot see"
    );
}

// ---------------------------------------------------------------------------
// (e) another user's process
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_process_owned_by_another_user_is_a_uid_mismatch() {
    let h = harness().await;
    let argv = argv_of(&[&fake_agent().to_string_lossy(), "hang"]);
    let guard = h
        .supervisor
        .plan_spawn(SpawnPlan::new(ProcKind::Pty, argv, "/tmp"))
        .await
        .expect("plan_spawn");

    // pid 1 is `launchd`: alive, root-owned, and emphatically not ours.
    let snap = snapshot();
    let start = snap
        .start_time(1)
        .or_else(|| Some(snap.at() / 1000))
        .expect("some start time for pid 1");
    h.force(guard.spawn_id(), Some(1), Some(start)).await;

    let row = h.row(guard.spawn_id()).await;
    assert_eq!(
        verify(&row, &snap),
        Fence::Mismatch(MismatchReason::Uid),
        "a foreign uid is refused before anything else is even considered"
    );

    guard.abandon("test");
}

// ---------------------------------------------------------------------------
// (f) the environment block is unreadable — the documented macOS limitation
// ---------------------------------------------------------------------------

/// A live probe with fence 3 switched off, standing in for the signed /
/// hardened binaries whose environment macOS will not let us read.
struct NoEnvProbe<'a>(LiveProbe<'a>);

impl ProcProbe for NoEnvProbe<'_> {
    fn is_alive(&self, pid: i32) -> bool {
        self.0.is_alive(pid)
    }
    fn start_time(&self, pid: i32) -> Option<i64> {
        self.0.start_time(pid)
    }
    fn start_time_degraded(&self, pid: i32) -> Option<i64> {
        self.0.start_time_degraded(pid)
    }
    fn uid(&self, pid: i32) -> Option<u32> {
        self.0.uid(pid)
    }
    fn user_name(&self, pid: i32) -> Option<String> {
        self.0.user_name(pid)
    }
    fn argv(&self, pid: i32) -> Option<Vec<String>> {
        self.0.argv(pid)
    }
    fn args_string(&self, pid: i32) -> Option<String> {
        self.0.args_string(pid)
    }
    fn environ(&self, _pid: i32) -> Option<Vec<String>> {
        None
    }
    fn our_uid(&self) -> Option<u32> {
        self.0.our_uid()
    }
}

#[tokio::test]
async fn an_unreadable_environment_block_does_not_break_verification() {
    let h = harness().await;
    let argv = argv_of(&[&fake_agent().to_string_lossy(), "hang"]);
    let (guard, _kid) = register(&h, &argv, false).await;

    let row = h.row(guard.spawn_id()).await;
    let snap = snapshot();
    assert_eq!(
        verify_with(&row, &NoEnvProbe(LiveProbe::new(&snap))),
        Fence::Verified,
        "the argv fence must stand on its own when macOS hides the environment"
    );
}

// ---------------------------------------------------------------------------
// a `planned` row that never started
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_planned_row_with_no_pid_is_a_mismatch_not_a_pass() {
    let h = harness().await;
    let argv = argv_of(&[&fake_agent().to_string_lossy(), "hang"]);
    let guard = h
        .supervisor
        .plan_spawn(SpawnPlan::new(ProcKind::Pty, argv, "/tmp"))
        .await
        .expect("plan_spawn");

    let row = h.row(guard.spawn_id()).await;
    assert_eq!(
        verify(&row, &snapshot()),
        Fence::Mismatch(MismatchReason::NoPid)
    );

    guard.abandon("test");
}

// ---------------------------------------------------------------------------
// a registered process that exited
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_process_that_exited_reads_as_gone() {
    let h = harness().await;
    let argv = argv_of(&[&fake_agent().to_string_lossy(), "hang"]);
    let (guard, mut kid) = register(&h, &argv, false).await;
    let pid = kid.pid();
    kid.kill_and_reap();
    wait_until_gone(pid);

    let row = h.row(guard.spawn_id()).await;
    assert_eq!(
        verify(&row, &snapshot()),
        Fence::Gone,
        "a finished process needs no signal and gets none"
    );

    // The row is still ours to close; nothing above touched the database.
    let spawn_id = guard.spawn_id().to_string();
    let live = h
        .db
        .read(move |conn| dao::proc::get(conn, &spawn_id))
        .await
        .expect("read")
        .expect("row");
    assert_eq!(live.pid, Some(pid as i64));
}
