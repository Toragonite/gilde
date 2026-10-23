//! The reaper against **real processes** — DESIGN §8-5, M1-PLAN T11 acceptance 8.
//!
//! The unit tests in `supervise::reaper` prove the shapes and the pure rules.
//! These prove the two things only a real process can prove:
//!
//!   * that the branches which must **not** signal really do not — the reboot
//!     branch with a live matching pid, and an ambiguous planned row with two
//!     equally good candidates;
//!   * that the branches which do signal reach the process — a decided
//!     *Terminate*, a generation sweep, and an automatic termination whose undo
//!     window has closed (while an undo inside the window genuinely stops it).
//!
//! Every test runs under `rusty-fork`, in its own process. Two reasons: the
//! reaper keeps a small amount of process-global state (the pending set and the
//! undo table), and a bug in the code under test must never be able to reach the
//! `cargo test` runner, which is by construction one of our ancestors.

mod fixtures;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gilde_code_lib::config::ConfigStore;
use gilde_code_lib::events::{Events, RecordingEvents};
use gilde_code_lib::store::models::{ProcKind, ProcRow, ProcState};
use gilde_code_lib::store::{dao, Db};
use gilde_code_lib::supervise::kill::{is_gone, KillOpts};
use gilde_code_lib::supervise::procscan::ps_snapshot;
use gilde_code_lib::supervise::reaper::{
    self, note, RecoveryDecision, SweepOpts, UNDO_WINDOW,
};
use gilde_code_lib::supervise::registry::{SpawnGuard, Supervisor};
use gilde_code_lib::supervise::spawn::is_alive;

use rusty_fork::rusty_fork_test;
use std::os::unix::process::CommandExt as _;
use std::os::unix::process::ExitStatusExt as _;

/// A fixture's spawn request on the one spawn path (`SupervisorApi`'s
/// checks, A12.8/A22.3; the legacy `Supervisor::plan_spawn(SpawnPlan)` is
/// gone): `headless` rows are the turn runs they became, and a sidecar or
/// turn row belongs to `generation` (1 unless given).
#[allow(dead_code)]
fn fixture_request(
    kind: ProcKind,
    argv: &[String],
    cwd: impl AsRef<std::path::Path>,
    parent: Option<&str>,
    generation: Option<i64>,
) -> gilde_code_lib::contract::services::SpawnRequest {
    use gilde_code_lib::contract::process::ProcKind as C;
    let kind = match kind {
        ProcKind::Pty => C::Pty,
        ProcKind::Convo => C::Convo,
        ProcKind::Pane => C::Pane,
        ProcKind::Attach => C::Attach,
        ProcKind::Turn | ProcKind::Headless => C::Turn,
        ProcKind::Sidecar => C::Sidecar,
        ProcKind::Probe => C::Probe,
        ProcKind::Vendorcli => C::Vendorcli,
        ProcKind::Git => C::Git,
        ProcKind::Ide => C::Ide,
        other => panic!("no fixture kind for {other:?}"),
    };
    let needs_generation = matches!(kind, C::Turn | C::Sidecar);
    gilde_code_lib::contract::services::SpawnRequest {
        kind,
        argv0: gilde_code_lib::contract::common::AbsPath::parse(argv[0].clone()).expect("absolute argv[0]"),
        args: argv[1..].to_vec(),
        cwd: gilde_code_lib::contract::common::AbsPath::parse(cwd.as_ref().to_string_lossy().into_owned()).ok(),
        env_keys: Vec::new(),
        parent_uid: parent.map(|p| gilde_code_lib::contract::ids::SessionUid::parse(p).expect("uid")),
        generation: generation.or(needs_generation.then_some(1)),
        turn_id: (kind == C::Turn).then(|| "t-fixture".to_string()),
    }
}


/// How long a test waits for the kernel to catch up with reality.
const SETTLE: Duration = Duration::from_secs(5);
/// A10.7: the ceiling of an event-driven wait, far above any budget.
const EVENT_CEILING: Duration = Duration::from_secs(60);
/// Long enough that `kill_tree`'s 200 ms "too young" exclusion cannot swallow a
/// target we just spawned.
const AGE_SETTLE: Duration = Duration::from_millis(400);

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(f)
}

/// One database, one config, one recording event sink — and as many
/// `app_instance` rows as a test needs, which is what makes "the previous run
/// crashed" expressible at all.
struct Harness {
    db: Db,
    config: Arc<ConfigStore>,
    sink: Arc<RecordingEvents>,
    bus: Events,
    _dir: tempfile::TempDir,
}

async fn harness() -> Harness {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::open_in_memory().expect("db");
    let config =
        Arc::new(ConfigStore::open_at(&dir.path().join("config.json")).expect("config store"));
    let sink = RecordingEvents::new();
    let bus = Events::new(sink.clone());
    Harness {
        db,
        config,
        sink,
        bus,
        _dir: dir,
    }
}

impl Harness {
    /// Open another `app_instance` row on the same database.
    async fn instance(&self) -> Arc<Supervisor> {
        Arc::new(
            Supervisor::open_instance(&self.db, self.config.clone(), "0.0.0-test")
                .await
                .expect("open_instance")
                .with_events(self.bus.clone()),
        )
    }

    async fn row(&self, spawn_id: &str) -> ProcRow {
        let id = spawn_id.to_string();
        self.db
            .read(move |conn| dao::proc::get(conn, &id))
            .await
            .expect("read row")
            .expect("row exists")
    }

    /// Pretend the machine rebooted between two runs of the app.
    async fn rewrite_boot_id(&self, app_instance: &str, boot_id: &str) {
        let (id, boot) = (app_instance.to_string(), boot_id.to_string());
        self.db
            .write(move |conn| {
                conn.execute(
                    "UPDATE app_instance SET boot_id = ?2 WHERE id = ?1",
                    rusqlite::params![id, boot],
                )?;
                Ok(())
            })
            .await
            .expect("rewrite boot_id");
    }

    /// Backdate a row's `started_at`, which is the only way to have a process
    /// that is "older than any legitimate deadline" inside a fast test.
    async fn backdate(&self, spawn_id: &str, minutes: i64) {
        let id = spawn_id.to_string();
        let at = gilde_code_lib::store::now_ms() - minutes * 60_000;
        self.db
            .write(move |conn| {
                conn.execute(
                    "UPDATE proc_registry SET started_at = ?2 WHERE spawn_id = ?1",
                    rusqlite::params![id, at],
                )?;
                Ok(())
            })
            .await
            .expect("backdate");
    }

    /// The first `undoToken` any `gilde://toast` carried.
    fn undo_token(&self) -> Option<String> {
        self.sink
            .events()
            .into_iter()
            .filter(|(name, _)| name == "gilde://toast")
            .find_map(|(_, v)| {
                v.get("undoToken")
                    .and_then(|t| t.as_str())
                    .map(str::to_string)
            })
    }

    /// Every `gilde://recovery/pending` count published so far, in order.
    fn recovery_counts(&self) -> Vec<u64> {
        self.sink
            .events()
            .into_iter()
            .filter(|(name, _)| name == "gilde://recovery/pending")
            .filter_map(|(_, v)| v.get("count").and_then(|c| c.as_u64()))
            .collect()
    }
}

fn fake_agent() -> PathBuf {
    fixtures::fake_agent_path()
}

fn argv_of(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|p| (*p).to_string()).collect()
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

impl Kid {
    /// Reap the child once it exits (its exit is the event waited on);
    /// `None` if it still runs at `ceiling`. Async: the scheduled kill it
    /// waits for runs on this test's current-thread runtime, which a blocking
    /// sleep would starve.
    async fn wait_exit(&mut self, ceiling: Duration) -> Option<std::process::ExitStatus> {
        let deadline = Instant::now() + ceiling;
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                self.reaped = true;
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

impl Drop for Kid {
    fn drop(&mut self) {
        self.kill_and_reap();
    }
}

fn spawn_child(argv: &[String], cwd: &Path, marker: Option<&str>) -> Kid {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.current_dir(cwd);
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
    while !is_gone(pid) {
        assert!(Instant::now() < deadline, "pid {pid} never exited");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The §9.3 allowlist admits only resolved binaries: a fixture is
/// allowlisted for the kind it is planned as before it is planned (A12.8).
fn allow(argv: &[String], kind: ProcKind) {
    gilde_code_lib::supervise::spawn::allow_executable(std::path::Path::new(&argv[0]), &[kind]);
}

/// Plan a row, start the process it describes, mark it running — DESIGN §8-3.
async fn register(
    sup: &Supervisor,
    kind: ProcKind,
    argv: &[String],
    cwd: &Path,
) -> (String, Kid) {
    allow(argv, kind);
    // A13.1: a sidecar row always belongs to a generation (the helper's 1).
    let guard = sup
        .plan_guarded(fixture_request(kind, argv, cwd, None, None))
        .await
        .expect("plan_spawn");
    if kind == ProcKind::Headless {
        // A row an older build left: no Gilde 2 path plans `headless` (it is
        // planned as the `turn` it became), so the fixture writes the legacy
        // kind onto the row the way that build did.
        let id = guard.spawn_id().to_string();
        sup.db()
            .write(move |conn| {
                conn.execute("UPDATE proc_registry SET kind = 'headless' WHERE spawn_id = ?1", [&id])?;
                Ok(())
            })
            .await
            .expect("legacy kind");
    }
    let kid = spawn_child(argv, cwd, Some(guard.env_marker()));
    let pid = kid.pid();
    guard.mark_running(pid, pid).expect("mark_running");
    let spawn_id = guard.spawn_id().to_string();
    // The guard's `Drop` marks an un-started row `lost`; this one *did* start,
    // so dropping it here is correct and changes nothing.
    drop(guard);
    (spawn_id, kid)
}

/// A `planned` row with no process yet — the window planned-row adoption exists
/// to close. The guard is deliberately leaked: dropping it would mark the row
/// `lost`, which is exactly the state the test needs it *not* to be in.
async fn plan_only(sup: &Supervisor, argv: &[String], cwd: &Path) -> (String, String) {
    allow(argv, ProcKind::Pty);
    let guard: SpawnGuard = sup
        .plan_guarded(fixture_request(ProcKind::Pty, argv, cwd, None, None))
        .await
        .expect("plan_spawn");
    let ids = (guard.spawn_id().to_string(), guard.env_marker().to_string());
    std::mem::forget(guard);
    ids
}

fn fast_opts() -> SweepOpts {
    SweepOpts {
        undo_window: Duration::from_millis(1_200),
        kill_grace: Duration::from_millis(700),
    }
}

// ===========================================================================
// 1. reboot: no fences, no signals  (acceptance 2)
// ===========================================================================

rusty_fork_test! {
    #[test]
    fn reboot_signals_nothing() {
        block_on(async {
            let h = harness().await;
            let old = h.instance().await;
            let cwd = h._dir.path().to_path_buf();
            let argv = argv_of(&[&fake_agent().to_string_lossy(), "hang"]);
            let (spawn_id, kid) = register(&old, ProcKind::Pty, &argv, &cwd).await;
            let pid = kid.pid();

            // The row names a pid that really is alive right now — and the
            // reaper must still not look at it, let alone signal it.
            assert!(is_alive(pid));
            h.rewrite_boot_id(old.app_instance_id(), "not-this-boot-session").await;

            let new = h.instance().await;
            let report = reaper::sweep_startup(&new).await;

            assert_eq!(report.reboot_lost, 1, "{report:?}");
            assert_eq!(report.adopted, 0, "the reboot branch adopts nothing");
            assert_eq!(report.exited, 0, "the reboot branch fences nothing");
            assert!(report.auto_terminated.is_empty(), "{report:?}");
            assert!(report.pending.is_empty(), "{report:?}");

            let row = h.row(&spawn_id).await;
            assert_eq!(row.state, ProcState::Lost);
            assert_eq!(row.note.as_deref(), Some(note::REBOOT));

            // The whole point: zero signals were sent.
            std::thread::sleep(Duration::from_millis(300));
            assert!(is_alive(pid), "a pid from a previous boot must never be signalled");
            assert!(reaper::pending().is_empty());
        });
    }
}

// ===========================================================================
// 2. planned-row adoption  (acceptance 3)
// ===========================================================================

rusty_fork_test! {
    #[test]
    fn planned_row_adopted() {
        block_on(async {
            let h = harness().await;
            let old = h.instance().await;
            let cwd = h._dir.path().to_path_buf();
            let argv = argv_of(&[&fake_agent().to_string_lossy(), "hang"]);

            // The process exists; the row that describes it never got a pid.
            let (spawn_id, marker) = plan_only(&old, &argv, &cwd).await;
            let kid = spawn_child(&argv, &cwd, Some(&marker));
            let pid = kid.pid();

            let new = h.instance().await;
            let report = reaper::sweep_startup(&new).await;

            assert_eq!(report.adopted, 1, "{report:?}");
            assert_eq!(report.lost, 0, "{report:?}");

            let row = h.row(&spawn_id).await;
            assert_eq!(row.state, ProcState::Adopted);
            assert_eq!(row.pid, Some(pid as i64), "adopted with the exact pid");
            assert_eq!(row.app_instance, new.app_instance_id());
            assert!(row.proc_start_ts.is_some(), "the pid-reuse fence must be filled in");

            // Adopted, then offered to the user — and still alive.
            assert_eq!(report.pending.len(), 1, "{report:?}");
            assert_eq!(report.pending[0].spawn_id, spawn_id);
            assert!(is_alive(pid));
        });
    }
}

// ===========================================================================
// 3. two equally good candidates: lost, never signalled  (acceptance 3)
// ===========================================================================

rusty_fork_test! {
    #[test]
    fn ambiguous_candidates_lost() {
        block_on(async {
            let h = harness().await;
            let old = h.instance().await;
            let cwd = h._dir.path().to_path_buf();
            let argv = argv_of(&[&fake_agent().to_string_lossy(), "hang"]);

            let (spawn_id, marker) = plan_only(&old, &argv, &cwd).await;
            // Identical argv, identical cwd, identical marker: nothing in the
            // world can tell these two apart, so nothing may be signalled.
            let first = spawn_child(&argv, &cwd, Some(&marker));
            let second = spawn_child(&argv, &cwd, Some(&marker));

            let new = h.instance().await;
            let report = reaper::sweep_startup(&new).await;

            assert_eq!(report.adopted, 0, "an ambiguous row is never adopted: {report:?}");
            assert_eq!(report.lost, 1, "{report:?}");
            assert!(report.pending.is_empty(), "{report:?}");

            let row = h.row(&spawn_id).await;
            assert_eq!(row.state, ProcState::Lost);
            assert_eq!(row.note.as_deref(), Some(note::AMBIGUOUS));

            std::thread::sleep(Duration::from_millis(300));
            assert!(is_alive(first.pid()), "candidate 1 must be untouched");
            assert!(is_alive(second.pid()), "candidate 2 must be untouched");
        });
    }
}

// ===========================================================================
// 4. an orphan of a crashed instance is offered, then terminated  (acceptance 8)
// ===========================================================================

rusty_fork_test! {
    #[test]
    fn orphan_after_hard_kill() {
        block_on(async {
            let h = harness().await;
            // The instance that started it never closed its row — exactly what a
            // `kill -9` of the host leaves behind.
            let old = h.instance().await;
            let cwd = h._dir.path().to_path_buf();
            let argv = argv_of(&[&fake_agent().to_string_lossy(), "hang"]);
            let (spawn_id, mut kid) = register(&old, ProcKind::Pty, &argv, &cwd).await;
            let pid = kid.pid();
            drop(old);
            std::thread::sleep(AGE_SETTLE);

            let new = h.instance().await;
            let report = reaper::sweep_startup(&new).await;

            assert_eq!(report.pending.len(), 1, "{report:?}");
            let pending = &report.pending[0];
            assert_eq!(pending.spawn_id, spawn_id);
            assert_eq!(pending.pid, Some(pid));
            assert!(pending.rss_bytes > 0, "the sheet shows real RSS: {pending:?}");
            assert!(pending.argv_masked.contains("fake_agent"), "{pending:?}");
            assert_eq!(
                reaper::pending().len(),
                1,
                "`recovery_pending` answers with the same row"
            );
            assert_eq!(h.recovery_counts().last().copied(), Some(1), "published as an event");

            // The user decides: terminate.
            let outcomes = reaper::resolve(
                &new,
                &[RecoveryDecision::Terminate { spawn_id: spawn_id.clone() }],
            )
            .await;
            assert_eq!(outcomes.len(), 1);
            let report = &outcomes[0].report;
            assert_eq!(report.refused_reason, None, "{report:?}");
            assert!(report.requested.contains(&pid), "{report:?}");
            assert!(report.survivors.is_empty(), "{report:?}");

            wait_until_gone(pid);
            kid.kill_and_reap();
            assert!(is_gone(pid));
            assert!(reaper::pending().is_empty(), "a resolved row leaves the sheet");
            assert_eq!(h.recovery_counts().last().copied(), Some(0));
        });
    }
}

// ===========================================================================
// 5. keep all: everything survives and is adopted  (acceptance 8)
// ===========================================================================

rusty_fork_test! {
    #[test]
    fn keep_all_leaves_everything_alive() {
        block_on(async {
            let h = harness().await;
            let old = h.instance().await;
            let cwd = h._dir.path().to_path_buf();
            let argv = argv_of(&[&fake_agent().to_string_lossy(), "hang"]);

            let mut ids = Vec::new();
            let mut kids = Vec::new();
            for _ in 0..3 {
                let (spawn_id, kid) = register(&old, ProcKind::Pty, &argv, &cwd).await;
                ids.push(spawn_id);
                kids.push(kid);
            }
            drop(old);

            let new = h.instance().await;
            let report = reaper::sweep_startup(&new).await;
            assert_eq!(report.pending.len(), 3, "{report:?}");

            let decisions: Vec<RecoveryDecision> = ids
                .iter()
                .map(|id| RecoveryDecision::Keep { spawn_id: id.clone() })
                .collect();
            let outcomes = reaper::resolve(&new, &decisions).await;

            assert_eq!(outcomes.len(), 3);
            for outcome in &outcomes {
                assert!(
                    outcome.report.requested.is_empty(),
                    "keeping a process must not signal it: {outcome:?}"
                );
                assert_eq!(outcome.report.refused_reason.as_deref(), Some(note::KEPT));
            }

            std::thread::sleep(Duration::from_millis(300));
            for (id, kid) in ids.iter().zip(kids.iter()) {
                assert!(is_alive(kid.pid()), "a kept process must stay alive");
                let row = h.row(id).await;
                assert_eq!(row.state, ProcState::Adopted, "kept rows are adopted");
                assert!(row.keep_alive, "kept rows survive the next boot too");
                assert_eq!(row.app_instance, new.app_instance_id());
            }
            assert!(reaper::pending().is_empty());
        });
    }
}

// ===========================================================================
// 5b. keepAlive at boot: adopted outright, never a question  (acceptance 4)
// ===========================================================================

rusty_fork_test! {
    #[test]
    fn a_keep_alive_orphan_is_adopted_without_asking_and_without_a_signal() {
        block_on(async {
            let h = harness().await;
            let old = h.instance().await;
            let cwd = h._dir.path().to_path_buf();
            let argv = argv_of(&[&fake_agent().to_string_lossy(), "hang"]);

            // Headless *and* well past `maxHeadlessLifetimeMinutes`, so the only
            // thing that can spare it is `keepAlive` itself.
            let (spawn_id, kid) = register(&old, ProcKind::Headless, &argv, &cwd).await;
            h.backdate(&spawn_id, 200).await;
            let id = spawn_id.clone();
            h.db.write(move |conn| {
                dao::proc::set_keep_alive(conn, &id, true)?;
                Ok(())
            })
            .await
            .expect("set keepAlive");
            drop(old);
            std::thread::sleep(AGE_SETTLE);

            let new = h.instance().await;
            let report = reaper::sweep_startup_with(&new, fast_opts()).await;

            assert_eq!(report.adopted, 1, "keepAlive is adopted, not asked about: {report:?}");
            assert!(report.pending.is_empty(), "{report:?}");
            assert!(
                report.auto_terminated.is_empty(),
                "keepAlive outranks the headless lifetime: {report:?}"
            );

            let row = h.row(&spawn_id).await;
            assert_eq!(row.state, ProcState::Adopted);
            assert_eq!(row.note.as_deref(), Some(note::DETACHED));
            assert_eq!(row.app_instance, new.app_instance_id());
            assert!(row.keep_alive, "the flag survives the adoption");

            // Well past the undo window a scheduled kill would have used.
            tokio::time::sleep(fast_opts().undo_window * 2).await;
            assert!(is_alive(kid.pid()), "a detached process is never quietly killed");
        });
    }
}

// ===========================================================================
// 6. headless past its lifetime: scheduled, and the undo really stops it
//    (acceptance 4, 7, 8)
// ===========================================================================

rusty_fork_test! {
    #[test]
    fn headless_past_lifetime_auto_terminated_with_undo() {
        block_on(async {
            let h = harness().await;
            let old = h.instance().await;
            let cwd = h._dir.path().to_path_buf();
            let argv = argv_of(&[&fake_agent().to_string_lossy(), "hang"]);
            let (spawn_id, mut kid) = register(&old, ProcKind::Headless, &argv, &cwd).await;
            let pid = kid.pid();
            // Older than `maxHeadlessLifetimeMinutes` (default 90).
            h.backdate(&spawn_id, 200).await;
            drop(old);
            std::thread::sleep(AGE_SETTLE);

            let new = h.instance().await;
            let opts = fast_opts();
            let report = reaper::sweep_startup_with(&new, opts).await;

            assert_eq!(report.auto_terminated.len(), 1, "{report:?}");
            let dry = &report.auto_terminated[0];
            assert!(dry.dry_run, "the sweep itself signals nothing: {dry:?}");
            assert!(dry.requested.contains(&pid), "{dry:?}");
            assert!(
                report.pending.is_empty(),
                "an auto-terminated row is not also a question for the user"
            );

            // The toast carries the undo token, per DESIGN §8-5.
            let token = h.undo_token().expect("a 10 s undo token was published");
            assert!(reaper::undo_is_pending(&token));
            assert_eq!(reaper::undo_target(&token).as_deref(), Some(spawn_id.as_str()));
            assert!(reaper::undo_cancel(&token), "the undo is accepted inside the window");
            assert!(!reaper::undo_cancel(&token), "and may only be spent once");

            // Well past the window: the kill must never have happened.
            tokio::time::sleep(opts.undo_window * 2).await;
            assert!(is_alive(pid), "undo must actually prevent the kill");
            assert_eq!(h.row(&spawn_id).await.state, ProcState::Running);

            // The same schedule, left alone, does end the process.
            let token = reaper::schedule_kill(
                new.clone(),
                spawn_id.clone(),
                Duration::from_millis(300),
                KillOpts::default().grace(Duration::from_millis(700)),
            );
            assert!(reaper::undo_is_pending(&token));
            tokio::time::sleep(Duration::from_secs(3)).await;
            assert!(!reaper::undo_is_pending(&token), "the window closed");
            wait_until_gone(pid);
            kid.kill_and_reap();
            assert!(is_gone(pid), "a scheduled kill nobody undid must fire");
        });
    }
}

// ===========================================================================
// 7. the generation sweep  (acceptance 5)
// ===========================================================================

rusty_fork_test! {
    #[test]
    fn generation_sweep_kills_that_generation_and_spares_keep_alive() {
        block_on(async {
            let h = harness().await;
            let sup = h.instance().await;
            let cwd = h._dir.path().to_path_buf();
            let argv = argv_of(&[&fake_agent().to_string_lossy(), "hang"]);
            allow(&argv, ProcKind::Headless);

            let doomed = sup
                .plan_guarded(fixture_request(ProcKind::Headless, &argv, &cwd, None, Some(7)))
                .await
                .expect("plan_spawn");
            let doomed_kid = spawn_child(&argv, &cwd, Some(doomed.env_marker()));
            doomed.mark_running(doomed_kid.pid(), doomed_kid.pid()).expect("mark_running");
            let doomed_id = doomed.spawn_id().to_string();
            drop(doomed);

            let kept = sup
                .plan_guarded(fixture_request(ProcKind::Headless, &argv, &cwd, None, Some(7)))
                .await
                .expect("plan_spawn");
            let kept_kid = spawn_child(&argv, &cwd, Some(kept.env_marker()));
            kept.mark_running(kept_kid.pid(), kept_kid.pid()).expect("mark_running");
            let kept_id = kept.spawn_id().to_string();
            drop(kept);
            let id = kept_id.clone();
            h.db.write(move |conn| {
                dao::proc::set_keep_alive(conn, &id, true)?;
                Ok(())
            })
            .await
            .expect("set keepAlive");

            std::thread::sleep(AGE_SETTLE);
            let reports = reaper::sweep_generation(&sup, 7).await;

            assert_eq!(reports.len(), 1, "only the keepAlive row is spared: {reports:?}");
            assert!(reports[0].requested.contains(&doomed_kid.pid()), "{reports:?}");

            wait_until_gone(doomed_kid.pid());
            assert!(is_alive(kept_kid.pid()), "keepAlive survives its generation");
            assert_eq!(h.row(&doomed_id).await.state, ProcState::Killed);
            assert_eq!(h.row(&kept_id).await.state, ProcState::Running);

            let live = h
                .db
                .read(|conn| dao::proc::live_rows_of_generation(conn, 7))
                .await
                .expect("live rows");
            assert_eq!(live.len(), 1, "only the kept row is still live in that generation");
        });
    }
}

// ===========================================================================
// 8. the periodic reconcile  (acceptance 6)
// ===========================================================================

rusty_fork_test! {
    #[test]
    fn reconcile_samples_the_living_and_closes_the_dead() {
        block_on(async {
            let h = harness().await;
            let sup = h.instance().await;
            let cwd = h._dir.path().to_path_buf();
            let argv = argv_of(&[&fake_agent().to_string_lossy(), "hang"]);
            let (spawn_id, mut kid) = register(&sup, ProcKind::Pty, &argv, &cwd).await;
            let pid = kid.pid();

            let first = reaper::reconcile(&sup).await;
            assert_eq!(first.checked, 1, "{first:?}");
            assert_eq!(first.exited, 0, "{first:?}");
            assert!(first.changed.is_empty(), "nothing changed, so nothing is published");
            assert_eq!(first.rows.len(), 1);
            assert!(first.rows[0].alive);
            assert!(first.rows[0].rss_bytes > 0, "RSS is sampled: {first:?}");
            assert!(first.rss_bytes > 0);

            // The process dies behind the reaper's back.
            kid.kill_and_reap();
            wait_until_gone(pid);

            let before = h
                .sink
                .events()
                .into_iter()
                .filter(|(name, _)| name == "gilde://proc/registry")
                .count();
            let second = reaper::reconcile(&sup).await;
            assert_eq!(second.exited, 1, "{second:?}");
            assert_eq!(second.changed, vec![spawn_id.clone()], "{second:?}");
            assert_eq!(h.row(&spawn_id).await.state, ProcState::Exited);
            assert_eq!(h.row(&spawn_id).await.note.as_deref(), Some(note::REAPED));

            let after = h
                .sink
                .events()
                .into_iter()
                .filter(|(name, _)| name == "gilde://proc/registry")
                .count();
            assert_eq!(after, before + 1, "a change invalidates gilde://proc/registry");

            // And a third pass has nothing left to do.
            let third = reaper::reconcile(&sup).await;
            assert_eq!(third.checked, 0, "{third:?}");
        });
    }
}

// ===========================================================================
// 9. the untracked-orphan net  (DESIGN §8-5 step 4, best-effort)
// ===========================================================================

rusty_fork_test! {
    #[test]
    fn an_untracked_orphan_of_a_dead_instance_is_caught_when_its_env_is_readable() {
        block_on(async {
            let h = harness().await;
            let old = h.instance().await;
            let cwd = h._dir.path().to_path_buf();
            let argv = argv_of(&[&fake_agent().to_string_lossy(), "hang"]);

            // A child of the dead instance that no registry row ever named — the
            // sidecar spawned it and `host.spawned` never arrived.
            let mut cmd = Command::new(&argv[0]);
            cmd.args(&argv[1..])
                .current_dir(&cwd)
                .env("GILDE_APP_INSTANCE", old.app_instance_id())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0);
            let child = cmd.spawn().expect("spawn orphan");
            let mut kid = Kid { child, reaped: false };
            let pid = kid.pid();
            wait_until_visible(pid);

            // Reading another process's environment is refused often enough on
            // macOS that DESIGN §8-4 calls this fence best-effort. When the read
            // does not work here, there is nothing to assert — and saying so is
            // more honest than a test that silently proves nothing.
            let snap = ps_snapshot().expect("snapshot");
            let readable = snap
                .with_env("GILDE_APP_INSTANCE", old.app_instance_id())
                .contains(&pid);

            let new = h.instance().await;
            let report = reaper::sweep_startup(&new).await;

            if readable {
                let found = report
                    .pending
                    .iter()
                    .find(|p| p.pid == Some(pid))
                    .unwrap_or_else(|| panic!("the orphan must reach the sheet: {report:?}"));
                let row = h.row(&found.spawn_id).await;
                assert_eq!(row.state, ProcState::Running);
                assert_eq!(row.note.as_deref(), Some(note::UNTRACKED));
                assert_eq!(row.app_instance, new.app_instance_id());
                assert!(
                    row.proc_start_ts.is_some(),
                    "a materialised row must carry the pid-reuse fence"
                );
            } else {
                eprintln!(
                    "GILDE_APP_INSTANCE was not readable for pid {pid}; \
                     the untracked-orphan sweep has nothing to find (DESIGN §8-4)"
                );
                assert!(report.pending.iter().all(|p| p.pid != Some(pid)));
            }

            assert!(is_alive(pid), "discovery never signals");
            kid.kill_and_reap();
        });
    }
}

// ===========================================================================
// 10. the shutdown sweep — what SIGTERM to the host now runs
// ===========================================================================

rusty_fork_test! {
    #[test]
    fn shutdown_sweep_ends_our_own_children_and_spares_keep_alive() {
        block_on(async {
            let h = harness().await;
            let sup = h.instance().await;
            let cwd = h._dir.path().to_path_buf();
            let argv = argv_of(&[&fake_agent().to_string_lossy(), "hang"]);

            // Three rows of ours: an ordinary child, a `keepAlive` one, and the
            // sidecar — which `RunEvent::Exit` owns, not this sweep.
            let (doomed_id, doomed) = register(&sup, ProcKind::Headless, &argv, &cwd).await;
            let (kept_id, kept) = register(&sup, ProcKind::Pty, &argv, &cwd).await;
            let (sidecar_id, sidecar) = register(&sup, ProcKind::Sidecar, &argv, &cwd).await;
            let id = kept_id.clone();
            h.db.write(move |conn| {
                dao::proc::set_keep_alive(conn, &id, true)?;
                Ok(())
            })
            .await
            .expect("set keepAlive");
            std::thread::sleep(AGE_SETTLE);

            let reports = reaper::shutdown_sweep(&sup).await;

            assert_eq!(
                reports.len(),
                1,
                "the keepAlive row and the sidecar are both spared: {reports:?}"
            );
            assert!(reports[0].requested.contains(&doomed.pid()), "{reports:?}");
            wait_until_gone(doomed.pid());
            assert!(is_alive(kept.pid()), "keepAlive is what surviving the host means");
            assert!(
                is_alive(sidecar.pid()),
                "signalling gilde-node from here would look like a crash to SidecarManager"
            );
            assert_eq!(h.row(&doomed_id).await.state, ProcState::Killed);
            assert_eq!(h.row(&kept_id).await.state, ProcState::Running);
            assert_eq!(h.row(&sidecar_id).await.state, ProcState::Running);
        });
    }
}

// ===========================================================================
// 11. a sweep with nothing to do still answers  (acceptance 9)
// ===========================================================================

rusty_fork_test! {
    #[test]
    fn a_clean_boot_sweeps_nothing_and_publishes_zero() {
        block_on(async {
            let h = harness().await;
            let sup = h.instance().await;

            let report = reaper::sweep_startup(&sup).await;
            assert!(!report.touched_anything(), "{report:?}");
            assert_eq!(report.pending_count(), 0);
            assert_eq!(
                h.recovery_counts(),
                vec![0],
                "the sheet is told it has nothing to show"
            );
            assert_eq!(UNDO_WINDOW, Duration::from_secs(10), "DESIGN §8-5");
        });
    }
}

// ===========================================================================
// Gilde 2 (contract §10.3): a crashed instance's session processes are never
// adopted as live; they get a fenced SIGTERM after the undo window, and a
// vendor CLI among them is never SIGKILLed.
// ===========================================================================

rusty_fork_test! {
    #[test]
    fn crash_leftover_sessions_are_terminated_after_the_undo_window_without_sigkill() {
        block_on(async {
            let h = harness().await;
            let old = h.instance().await;
            let cwd = h._dir.path().to_path_buf();
            // A Conversation child (an ordinary fixture) and a "claude" copy that
            // ignores SIGTERM, both owned by Gilde sessions.
            let quiet = argv_of(&[&fake_agent().to_string_lossy(), "hang"]);
            let bin = tempfile::tempdir().expect("bin");
            let claude = bin.path().join("claude");
            std::fs::copy(fake_agent(), &claude).expect("copy");
            let stubborn = argv_of(&[&claude.to_string_lossy(), "ignore-term"]);
            let mut kids = Vec::new();
            let mut ids = Vec::new();
            for (argv, uid) in [
                (&quiet, "s_01890a5d-ac96-774b-bcce-b302099a8057"),
                (&stubborn, "s_01890a5d-ac96-774b-bcce-b302099a8058"),
            ] {
                allow(argv, ProcKind::Convo);
                let guard = old
                    .plan_guarded(fixture_request(ProcKind::Convo, argv, &cwd, Some(uid), None))
                    .await
                    .expect("plan");
                let kid = spawn_child(argv, &cwd, Some(guard.env_marker()));
                guard.mark_running(kid.pid(), kid.pid()).expect("running");
                ids.push(guard.spawn_id().to_string());
                drop(guard);
                kids.push(kid);
            }
            drop(old);
            std::thread::sleep(AGE_SETTLE);

            let new = h.instance().await;
            let report = reaper::sweep_startup_with(&new, fast_opts()).await;
            assert!(report.pending.is_empty(), "session rows are not offered as live: {report:?}");
            assert_eq!(report.auto_terminated.len(), 2, "{report:?}");
            assert!(report.auto_terminated.iter().all(|r| r.dry_run));
            let crash = reaper::crash_leftover_spawn_ids();
            assert!(ids.iter().all(|id| crash.contains(id)));
            // Nothing is signalled inside the undo window.
            assert!(is_alive(kids[0].pid()) && is_alive(kids[1].pid()));

            // After the window: the quiet one is gone, the vendor CLI survived
            // its SIGTERM and was not escalated.
            // Waited on as events with a generous ceiling (A10.7): the quiet
            // child's own exit, then the vendor CLI's row recording it as a
            // survivor of its SIGTERM.
            let quiet_pid = kids[0].pid();
            let status = kids[0].wait_exit(EVENT_CEILING).await;
            let row0 = h.row(&ids[0]).await;
            let status = status.unwrap_or_else(|| {
                panic!("pid {quiet_pid} never exited; row {:?} note {:?}", row0.state, row0.note)
            });
            assert_eq!(status.signal(), Some(libc::SIGTERM), "a SIGTERM, never a SIGKILL");
            let deadline = Instant::now() + EVENT_CEILING;
            let row1 = loop {
                let row = h.row(&ids[1]).await;
                if row.state == ProcState::KillIncomplete || Instant::now() >= deadline {
                    break row;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            };
            assert_eq!(row1.state, ProcState::KillIncomplete, "the survivor is recorded: {row1:?}");
            assert!(is_alive(kids[1].pid()) && !is_gone(kids[1].pid()), "a vendor CLI is never SIGKILLed");
            for kid in &mut kids {
                kid.kill_and_reap();
            }
        });
    }
}

// ===========================================================================
// A29.3: the recovery window — every stop the boot sweep schedules (a
// crashed instance's session children, the previous instance's quit
// survivors) is one group under one undo token, published as
// `BootStatus.recovery`; rows it neither stops nor adopts become leftovers.
// ===========================================================================

mod a29 {
    use super::*;
    use gilde_code_lib::contract::process::LeftoverReason;
    use gilde_code_lib::contract::services::{ProcInfo, ProcSnapshot, TeammateLabels};
    use gilde_code_lib::contract::store::{BootStatus, Entity, StoreOp};
    use gilde_code_lib::events::StoreOpsSink;
    use gilde_code_lib::lifecycle::boot::{run_boot_sweep_with, Lifecycle};
    use gilde_code_lib::supervise::health::{registry_view, HealthState};

    /// Every store op the feed published, in order.
    #[derive(Default)]
    struct Ops(parking_lot::Mutex<Vec<StoreOp>>);

    impl StoreOpsSink for Ops {
        fn ops(&self, ops: Vec<StoreOp>) {
            self.0.lock().extend(ops);
        }
    }

    impl Ops {
        fn boots(&self) -> Vec<BootStatus> {
            self.0
                .lock()
                .iter()
                .filter_map(|op| match op {
                    StoreOp::Boot { status } => Some(status.clone()),
                    _ => None,
                })
                .collect()
        }
    }

    fn lifecycle() -> (Arc<Lifecycle>, Arc<Ops>) {
        let lc = Lifecycle::new("0.0.0-test", "build");
        let ops = Arc::new(Ops::default());
        lc.feed.set_local_store(ops.clone());
        (lc, ops)
    }

    /// A Gilde session's Conversation child of `sup`'s instance.
    async fn session_child(sup: &Supervisor, argv: &[String], cwd: &Path, uid: &str) -> (String, Kid) {
        allow(argv, ProcKind::Convo);
        let guard = sup
            .plan_guarded(fixture_request(ProcKind::Convo, argv, cwd, Some(uid), None))
            .await
            .expect("plan");
        let kid = spawn_child(argv, cwd, Some(guard.env_marker()));
        guard.mark_running(kid.pid(), kid.pid()).expect("running");
        let id = guard.spawn_id().to_string();
        drop(guard);
        (id, kid)
    }

    /// A vendor process the previous instance recorded as a quit survivor
    /// (§10.2 step 6): its row stays `running` under the ended instance.
    async fn quit_survivor(h: &Harness, argv: &[String], cwd: &Path) -> (String, Kid) {
        let prev = h.instance().await;
        let (id, kid) = register(&prev, ProcKind::Pty, argv, cwd).await;
        let (sid, inst) = (id.clone(), prev.app_instance_id().to_string());
        h.db.write(move |conn| {
            conn.execute(
                "UPDATE proc_registry SET note = ?2 WHERE spawn_id = ?1",
                rusqlite::params![sid, note::QUIT_SURVIVOR],
            )?;
            dao::app_instance::close(conn, &inst, gilde_code_lib::store::now_ms())?;
            Ok(())
        })
        .await
        .expect("an ended instance with a survivor");
        (id, kid)
    }

    rusty_fork_test! {
        #[test]
        fn one_token_cancels_every_grouped_stop_and_clears_the_window() {
            block_on(async {
                let h = harness().await;
                let cwd = h._dir.path().to_path_buf();
                let argv = argv_of(&[&fake_agent().to_string_lossy(), "hang"]);
                let (survivor_id, survivor) = quit_survivor(&h, &argv, &cwd).await;
                let old = h.instance().await;
                let (a_id, a) = session_child(&old, &argv, &cwd, "s_01890a5d-ac96-774b-bcce-b302099a8061").await;
                let (b_id, b) = session_child(&old, &argv, &cwd, "s_01890a5d-ac96-774b-bcce-b302099a8062").await;
                // A headless run past its lifetime keeps its own toast and token.
                let (headless_id, headless) = register(&old, ProcKind::Headless, &argv, &cwd).await;
                h.backdate(&headless_id, 200).await;
                drop(old);
                std::thread::sleep(AGE_SETTLE);

                let new = h.instance().await;
                let (lc, ops) = lifecycle();
                let before = gilde_code_lib::store::now_ms();
                let opts = fast_opts();
                let report = run_boot_sweep_with(&lc, &new, opts).await;

                let window = report.recovery.clone().expect("the sweep scheduled stops");
                assert_eq!(window.stopping, 3, "two crash leftovers and one quit survivor: {report:?}");
                assert!(window.until >= before + opts.undo_window.as_millis() as i64, "until is wall clock");
                assert_eq!(lc.feed.boot().recovery, Some(window.clone()), "published as soon as scheduled");
                let token = window.undo_token.as_str().to_string();
                let mut members = reaper::undo_targets(&token);
                members.sort();
                let mut want = vec![a_id.clone(), b_id.clone(), survivor_id.clone()];
                want.sort();
                assert_eq!(members, want, "one token names every grouped stop");
                assert_eq!(reaper::undo_target(&token), None);

                let headless_token = h.undo_token().expect("the headless toast carries its own token");
                assert_ne!(headless_token, token);
                assert!(!members.contains(&headless_id));
                assert!(reaper::undo_cancel(&headless_token));

                // [Keep them]: one undo, every stop of the group.
                assert!(reaper::undo_cancel(&token));
                assert_eq!(lc.feed.boot().recovery, None, "the window closes on the undo");
                assert!(!reaper::undo_cancel(&token), "a token is spent once");
                assert!(reaper::undo_targets(&token).is_empty());

                // Well past the window: no signal reached any of them.
                tokio::time::sleep(opts.undo_window * 2 + Duration::from_millis(500)).await;
                for (what, kid) in [("crash a", &a), ("crash b", &b), ("survivor", &survivor), ("headless", &headless)] {
                    assert!(is_alive(kid.pid()), "{what}: the undo really prevented the stop");
                }
                // The store saw the window open, then close.
                let recoveries: Vec<_> = ops.boots().into_iter().map(|b| b.recovery).collect();
                assert!(recoveries.contains(&Some(window.clone())), "{recoveries:?}");
                assert_eq!(recoveries.last(), Some(&None), "{recoveries:?}");
                // A survivor of the undo is still a leftover for Clean up.
                let crash = reaper::crash_leftover_spawn_ids();
                assert!(crash.contains(&a_id) && crash.contains(&b_id));
                assert!(reaper::quit_survivor_spawn_ids().contains(&survivor_id));
            });
        }
    }

    rusty_fork_test! {
        #[test]
        fn the_window_ends_every_grouped_stop_fires_and_the_window_clears() {
            block_on(async {
                let h = harness().await;
                let cwd = h._dir.path().to_path_buf();
                let argv = argv_of(&[&fake_agent().to_string_lossy(), "hang"]);
                let (_survivor_id, survivor) = quit_survivor(&h, &argv, &cwd).await;
                let old = h.instance().await;
                let (_a_id, a) = session_child(&old, &argv, &cwd, "s_01890a5d-ac96-774b-bcce-b302099a8063").await;
                let (_b_id, b) = session_child(&old, &argv, &cwd, "s_01890a5d-ac96-774b-bcce-b302099a8064").await;
                drop(old);
                std::thread::sleep(AGE_SETTLE);

                let new = h.instance().await;
                let (lc, ops) = lifecycle();
                let report = run_boot_sweep_with(&lc, &new, fast_opts()).await;
                let window = report.recovery.clone().expect("a window");
                assert_eq!(window.stopping, 3);
                // Nothing is signalled inside the window.
                assert!(is_alive(a.pid()) && is_alive(b.pid()) && is_alive(survivor.pid()));

                // The window ends (an event, A10.7): `recovery` is cleared …
                let deadline = Instant::now() + EVENT_CEILING;
                while lc.feed.boot().recovery.is_some() {
                    assert!(Instant::now() < deadline, "the window never ended");
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                assert!(!reaper::undo_is_pending(window.undo_token.as_str()), "the token is spent");
                assert!(!reaper::undo_cancel(window.undo_token.as_str()), "too late to undo");
                // … and every stop of the group fires: fenced, never SIGKILL.
                for (what, mut kid) in [("crash a", a), ("crash b", b), ("survivor", survivor)] {
                    let pid = kid.pid();
                    let status = kid.wait_exit(EVENT_CEILING).await.unwrap_or_else(|| panic!("{what} ({pid}) never stopped"));
                    let sig = status.signal();
                    assert!(matches!(sig, Some(libc::SIGTERM) | Some(libc::SIGHUP)), "{what}: {sig:?}, never SIGKILL");
                }
                let recoveries: Vec<_> = ops.boots().into_iter().map(|b| b.recovery).collect();
                assert_eq!(recoveries.first(), Some(&Some(window)), "{recoveries:?}");
                assert_eq!(recoveries.last(), Some(&None), "{recoveries:?}");
            });
        }
    }

    rusty_fork_test! {
        #[test]
        fn a_row_the_sweep_neither_stops_nor_adopts_becomes_a_crash_leftover() {
            block_on(async {
                let h = harness().await;
                let cwd = h._dir.path().to_path_buf();
                let argv = argv_of(&[&fake_agent().to_string_lossy(), "hang"]);
                let old = h.instance().await;
                // A crashed instance's terminal row that belongs to no session.
                let (spawn_id, kid) = register(&old, ProcKind::Pty, &argv, &cwd).await;
                drop(old);
                std::thread::sleep(AGE_SETTLE);

                let new = h.instance().await;
                let (lc, _ops) = lifecycle();
                let report = run_boot_sweep_with(&lc, &new, fast_opts()).await;
                assert_eq!(report.sweep.pending, 1, "{report:?}");
                assert_eq!(report.recovery, None, "nothing was scheduled: no window");
                assert_eq!(lc.feed.boot().recovery, None);
                assert!(reaper::crash_leftover_spawn_ids().contains(&spawn_id), "never dropped silently");

                // The shared scan names it a GildeCrashLeftover and the feed
                // publishes it (LeftoverUpsert → Upsert{Leftover}).
                let row = h.row(&spawn_id).await;
                let start = row.proc_start_ts.expect("a start time");
                let view = registry_view(&new).await;
                let fact = view.get(&(kid.pid(), start)).expect("the registry knows it");
                assert!(fact.crash_leftover && !fact.current_instance);
                let proc = ProcInfo {
                    key: gilde_code_lib::contract::ids::ProcessKey::new(kid.pid() as u32, start as u64),
                    pid: kid.pid(),
                    ppid: 1,
                    uid: 501,
                    start_sec: start,
                    exec_path: Some(argv[0].clone()),
                    argv_redacted: argv.clone(),
                    labels: TeammateLabels::default(),
                    cwd: None,
                    rss_bytes: 1 << 20,
                    cpu_ns: 0,
                    cco_team_member: None,
                    gilde_app_instance: None,
                    vendor: None,
                };
                let health = HealthState::new();
                let snap = |at| Arc::new(ProcSnapshot { taken_at: at, procs: vec![proc.clone()] });
                health.observe(snap(1), &view);
                let leftovers = health.observe(snap(2), &view);
                assert_eq!(leftovers.len(), 1);
                assert_eq!(leftovers[0].leftover.as_ref().map(|l| l.reason), Some(LeftoverReason::GildeCrashLeftover));
                let ops = Arc::new(Ops::default());
                lc.feed.set_local_store(ops.clone());
                lc.feed.set_leftovers(leftovers);
                let published = ops.0.lock().iter().any(|op| matches!(
                    op,
                    StoreOp::Upsert { entity } if matches!(
                        entity.as_ref(),
                        Entity::Leftover { value } if value.pid == kid.pid()
                            && value.leftover.as_ref().map(|l| l.reason) == Some(LeftoverReason::GildeCrashLeftover)
                    )
                ));
                assert!(published, "{:?}", ops.0.lock());
                assert!(is_alive(kid.pid()), "a leftover is shown, never signalled by the sweep");
            });
        }
    }
}

// ===========================================================================
// A41.3 (G7c-04): observing an orphan never waits on the spawn gate; every
// marker-only orphan of a crashed instance is listed, never stopped
// automatically. No signal is sent: the sweep's stop group is never armed.
// ===========================================================================

/// `n` processes stamped with `instance`'s marker that no row names.
fn spawn_marker_orphans(instance: &str, cwd: &Path, n: usize) -> (Vec<i32>, Vec<Kid>) {
    let mut pids = Vec::new();
    let mut kids = Vec::new();
    for _ in 0..n {
        let child = Command::new(fake_agent())
            .arg("hang")
            .current_dir(cwd)
            .env("GILDE_APP_INSTANCE", instance)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn orphan");
        pids.push(child.id() as i32);
        kids.push(Kid { child, reaped: false });
    }
    for pid in &pids {
        wait_until_visible(*pid);
    }
    std::thread::sleep(AGE_SETTLE);
    (pids, kids)
}

/// The next boot exactly as `boot.rs` builds it: the gate stays closed until
/// the sweep has run.
async fn next_boot_with_closed_gate(h: &Harness) -> Arc<Supervisor> {
    Arc::new(
        Supervisor::open_instance(&h.db, h.config.clone(), "0.0.0-test")
            .await
            .expect("new")
            .with_spawn_gate(gilde_code_lib::lifecycle::spawn_gate::SpawnGate::closed()),
    )
}

rusty_fork_test! {
    /// G7c-04, the reviewer's test: both untracked orphans reach the Recovered
    /// sheet within the boot budget although the gate is closed.
    #[test]
    fn g7c_04_the_boot_sweep_catches_an_untracked_orphan_with_the_boot_gate_closed() {
        block_on(async {
            let h = harness().await;
            let old = h.instance().await;
            let (pids, mut kids) = spawn_marker_orphans(old.app_instance_id(), h._dir.path(), 2);
            let readable = ps_snapshot().expect("snap").with_env("GILDE_APP_INSTANCE", old.app_instance_id());
            assert!(pids.iter().all(|p| readable.contains(p)), "precondition: our own children's env is readable");
            let new = next_boot_with_closed_gate(&h).await;
            let mut group = reaper::StopGroup::new();
            let t0 = Instant::now();
            let report = reaper::sweep_startup_into(&new, SweepOpts::default(), &mut group).await;
            let took = t0.elapsed();
            let caught: Vec<i32> = pids.iter().copied().filter(|p| report.pending.iter().any(|r| r.pid == Some(*p))).collect();
            for kid in &mut kids {
                assert!(is_alive(kid.pid()), "observing never signals");
                kid.kill_and_reap();
            }
            assert!(
                caught.len() == pids.len() && took < Duration::from_secs(3),
                "§10.3: both untracked orphans reach the Recovered sheet within the boot budget; \
                 caught {caught:?} of {pids:?} in {took:?}"
            );
        });
    }
}

rusty_fork_test! {
    /// A41.3: ten orphans cost no gate wait, every one is listed as a
    /// `GildeCrashLeftover` (pending, in the boot sweep's crash set) and none
    /// is scheduled for an automatic stop.
    #[test]
    fn a41_ten_orphans_are_all_listed_without_a_gate_wait_and_never_auto_stopped() {
        block_on(async {
            let h = harness().await;
            let old = h.instance().await;
            let (pids, mut kids) = spawn_marker_orphans(old.app_instance_id(), h._dir.path(), 10);
            let readable = ps_snapshot().expect("snap").with_env("GILDE_APP_INSTANCE", old.app_instance_id());
            assert!(pids.iter().all(|p| readable.contains(p)), "precondition: our own children's env is readable");
            let new = next_boot_with_closed_gate(&h).await;
            let mut group = reaper::StopGroup::new();
            let t0 = Instant::now();
            let report = reaper::sweep_startup_into(&new, SweepOpts::default(), &mut group).await;
            let took = t0.elapsed();
            let crash = reaper::crash_leftover_spawn_ids();
            let listed: Vec<&reaper::RecoveredProc> = report.pending.iter().filter(|r| r.pid.is_some_and(|p| pids.contains(&p))).collect();
            let alive: Vec<bool> = pids.iter().map(|p| is_alive(*p)).collect();
            for kid in &mut kids {
                kid.kill_and_reap();
            }
            assert_eq!(listed.len(), 10, "every orphan is listed: {report:?}");
            assert!(listed.iter().all(|r| crash.contains(&r.spawn_id)), "as GildeCrashLeftover from the first scan");
            assert!(report.auto_terminated.is_empty(), "a marker-only orphan is never stopped automatically: {report:?}");
            assert!(took < Duration::from_secs(5), "no gate wait (10 × 5 s before A41.3): {took:?}");
            assert!(alive.iter().all(|a| *a), "observing never signals");
        });
    }
}
