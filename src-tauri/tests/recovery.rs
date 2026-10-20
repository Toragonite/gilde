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
use gilde_code_lib::events::bus::{EventBus, RecordingSink};
use gilde_code_lib::model::graph::GraphHandle;
use gilde_code_lib::store::models::{ProcKind, ProcRow, ProcState};
use gilde_code_lib::store::{dao, Db};
use gilde_code_lib::supervise::kill::{is_gone, KillOpts};
use gilde_code_lib::supervise::procscan::ps_snapshot;
use gilde_code_lib::supervise::reaper::{
    self, note, RecoveryDecision, SweepOpts, UNDO_WINDOW,
};
use gilde_code_lib::supervise::registry::{SpawnGuard, SpawnPlan, Supervisor};
use gilde_code_lib::supervise::spawn::is_alive;

use rusty_fork::rusty_fork_test;
use std::os::unix::process::CommandExt as _;

/// How long a test waits for the kernel to catch up with reality.
const SETTLE: Duration = Duration::from_secs(5);
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
    sink: Arc<RecordingSink>,
    bus: EventBus,
    _graph: GraphHandle,
    _dir: tempfile::TempDir,
}

async fn harness() -> Harness {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::open_in_memory().expect("db");
    let config =
        Arc::new(ConfigStore::open_at(&dir.path().join("config.json")).expect("config store"));
    let graph = GraphHandle::new();
    let sink = Arc::new(RecordingSink::default());
    let bus = EventBus::with_sink(sink.clone(), graph.state().clone());
    Harness {
        db,
        config,
        sink,
        bus,
        _graph: graph,
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

/// Plan a row, start the process it describes, mark it running — DESIGN §8-3.
async fn register(
    sup: &Supervisor,
    kind: ProcKind,
    argv: &[String],
    cwd: &Path,
) -> (String, Kid) {
    let guard = sup
        .plan_spawn(SpawnPlan::new(kind, argv.to_vec(), cwd))
        .await
        .expect("plan_spawn");
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
    let guard: SpawnGuard = sup
        .plan_spawn(SpawnPlan::new(ProcKind::Pty, argv.to_vec(), cwd))
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

            let doomed = sup
                .plan_spawn(SpawnPlan::new(ProcKind::Headless, argv.clone(), &cwd).generation(7))
                .await
                .expect("plan_spawn");
            let doomed_kid = spawn_child(&argv, &cwd, Some(doomed.env_marker()));
            doomed.mark_running(doomed_kid.pid(), doomed_kid.pid()).expect("mark_running");
            let doomed_id = doomed.spawn_id().to_string();
            drop(doomed);

            let kept = sup
                .plan_spawn(SpawnPlan::new(ProcKind::Headless, argv.clone(), &cwd).generation(7))
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
