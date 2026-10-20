//! `kill_tree` against **real processes** — DESIGN §8-6, M1-PLAN T10 acceptance 4.
//!
//! The unit tests in `supervise::kill` prove the *policy* (target sets,
//! exclusions, the escalation gate) against synthetic snapshots. These prove the
//! part that cannot be faked: that signals actually reach a process tree that
//! escapes its group, that a process ignoring `SIGTERM` is escalated, and — the
//! two that matter most — that a row pointing at one of our ancestors or at a
//! recycled pid causes **no signal at all**.
//!
//! Every test that sends a signal runs in its own process under `rusty-fork`, so
//! a bug in the code under test can never reach the `cargo test` runner (which
//! is, by construction, one of our ancestors).
//!
//! `kill_tree` is `async`; `rusty_fork_test!` bodies are not, so each forked
//! test builds a current-thread runtime of its own.

mod fixtures;

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gilde_code_lib::config::ConfigStore;
use gilde_code_lib::store::models::{ProcKind, ProcRow, ProcState};
use gilde_code_lib::store::{dao, Db};
use gilde_code_lib::supervise::identity::{live_argv, verify, Fence};
use gilde_code_lib::supervise::kill::{
    is_gone, kill_bare_pid, kill_pids, kill_tree, reason, survivors_still_ours, KillOpts,
    KillReport, KillTarget,
};
use gilde_code_lib::supervise::procscan::{ps_snapshot, start_time_of, Snapshot};
use gilde_code_lib::supervise::registry::{SpawnGuard, SpawnPlan, Supervisor};
use gilde_code_lib::supervise::spawn::is_alive;

use rusty_fork::rusty_fork_test;
use std::os::unix::process::CommandExt as _;

/// How long a test waits for the kernel to catch up with reality.
const SETTLE: Duration = Duration::from_secs(5);
/// Long enough that the 200 ms "too young" exclusion cannot swallow a target we
/// just spawned.
const AGE_SETTLE: Duration = Duration::from_millis(400);

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

/// A current-thread runtime, so a `rusty_fork_test!` body can drive async code.
fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(f)
}

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

    /// Overwrite identity columns directly — the only way to stage a pid-reuse
    /// or an "this row names our parent" scenario on purpose.
    async fn force_identity(
        &self,
        spawn_id: &str,
        pid: i64,
        pgid: Option<i64>,
        proc_start_ts: i64,
        argv: Option<Vec<String>>,
    ) {
        let spawn_id = spawn_id.to_string();
        let (argv_json, argv_sig) = match argv {
            Some(argv) => (
                Some(serde_json::to_string(&argv).expect("argv json")),
                Some(gilde_code_lib::supervise::identity::argv_sig_of(&argv)),
            ),
            None => (None, None),
        };
        self.db
            .write(move |conn| {
                conn.execute(
                    "UPDATE proc_registry
                        SET pid = ?2, pgid = ?3, proc_start_ts = ?4,
                            argv = COALESCE(?5, argv), argv_sig = COALESCE(?6, argv_sig)
                      WHERE spawn_id = ?1",
                    rusqlite::params![spawn_id, pid, pgid, proc_start_ts, argv_json, argv_sig],
                )?;
                Ok(())
            })
            .await
            .expect("force identity");
    }

    async fn event_rows(&self, spawn_id: &str) -> Vec<gilde_code_lib::store::models::EventLogRow> {
        let spawn_id = spawn_id.to_string();
        self.db
            .read(move |conn| dao::event_log::by_subject(conn, &spawn_id, 32))
            .await
            .expect("read event_log")
    }
}

fn fake_agent() -> PathBuf {
    fixtures::fake_agent_path()
}

fn argv_of(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|p| (*p).to_string()).collect()
}

fn snapshot() -> Snapshot {
    ps_snapshot().expect("ps snapshot")
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

fn spawn_child(argv: &[String], marker: Option<&str>) -> Kid {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
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

/// Waits for a pid to be *finished*, zombie included.
///
/// `kill(pid, 0)` succeeds on a zombie, so a killed direct child of the test
/// process reads as alive until somebody reaps it — and `kill_tree` deliberately
/// leaves that reaping to whoever owns the `Child` handle. `is_gone` is the
/// zombie-aware predicate the kill path itself uses.
fn wait_until_gone(pid: i32) {
    let deadline = Instant::now() + SETTLE;
    while !is_gone(pid) {
        assert!(Instant::now() < deadline, "pid {pid} never exited");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Plan a row, start the process it describes, mark it running — DESIGN §8-3.
async fn register(h: &Harness, argv: &[String]) -> (SpawnGuard, Kid) {
    let guard = h
        .supervisor
        .plan_spawn(SpawnPlan::new(ProcKind::Pty, argv.to_vec(), "/tmp"))
        .await
        .expect("plan_spawn");
    let kid = spawn_child(argv, Some(guard.env_marker()));
    let pid = kid.pid();
    guard.mark_running(pid, pid).expect("mark_running");
    (guard, kid)
}

/// Register an already-running fixture handle (one started through
/// `fixtures::Running`, which owns its own stdio pipes).
async fn register_pid(h: &Harness, argv: &[String], pid: i32, pgid: i32) -> SpawnGuard {
    let guard = h
        .supervisor
        .plan_spawn(SpawnPlan::new(ProcKind::Pty, argv.to_vec(), "/tmp"))
        .await
        .expect("plan_spawn");
    guard.mark_running(pid, pgid).expect("mark_running");
    guard
}

fn fast_opts() -> KillOpts {
    KillOpts::default().grace(Duration::from_millis(700))
}

/// Assert a report never claims a bare success.
fn assert_report_is_honest(report: &KillReport) {
    assert!(
        report.refused_reason.is_some()
            || !report.requested.is_empty()
            || !report.excluded.is_empty(),
        "a report must always explain itself: {report:?}"
    );
}

// ===========================================================================
// 1. a process that escapes its group is still killed
// ===========================================================================

rusty_fork_test! {
    #[test]
    fn escape_group_killed_via_descendant_walk() {
        block_on(async {
            let h = harness().await;
            let mut escape = fixtures::start_escape_group();
            let parent_pid = escape.parent.pid();
            let escaped_pid = escape.escaped_pid;

            // The escapee called setsid(): it is in neither our group nor the
            // fixture's, so a group signal alone cannot reach it.
            let snap = snapshot();
            let group = snap.pids_in_group(parent_pid);
            assert!(
                !group.contains(&escaped_pid),
                "the escapee must have left the fixture's process group"
            );
            assert!(
                snap.descendants_of(parent_pid).contains(&escaped_pid),
                "the descendant walk is the path that finds it"
            );

            std::thread::sleep(AGE_SETTLE);
            let argv = argv_of(&[&fake_agent().to_string_lossy(), "escape-group"]);
            let guard = register_pid(&h, &argv, parent_pid, escape.parent.pgid()).await;

            let report = kill_tree(&h.supervisor, guard.spawn_id(), fast_opts()).await;
            assert_report_is_honest(&report);
            assert_eq!(report.refused_reason, None, "{report:?}");
            assert!(
                report.requested.contains(&escaped_pid),
                "the escapee must be in the target set: {report:?}"
            );
            assert!(report.survivors.is_empty(), "{report:?}");

            wait_until_gone(escaped_pid);
            escape.parent.kill_and_reap();
            assert!(is_gone(escaped_pid), "the escapee outlived its kill");
        });
    }
}

// ===========================================================================
// 2. a grandchild dies with the tree
// ===========================================================================

rusty_fork_test! {
    #[test]
    fn grandchild_killed_under_7s() {
        block_on(async {
            let h = harness().await;
            let mut tree = fixtures::start_spawn_grandchild(60);
            let parent_pid = tree.parent.pid();
            let grandchild_pid = tree.grandchild_pid;
            std::thread::sleep(AGE_SETTLE);

            let argv = argv_of(&[&fake_agent().to_string_lossy(), "spawn-grandchild", "60"]);
            let guard = register_pid(&h, &argv, parent_pid, tree.parent.pgid()).await;

            let started = Instant::now();
            let report = kill_tree(&h.supervisor, guard.spawn_id(), fast_opts()).await;
            let elapsed = started.elapsed();

            assert_eq!(report.refused_reason, None, "{report:?}");
            assert!(
                report.requested.contains(&grandchild_pid),
                "the grandchild must be targeted: {report:?}"
            );
            wait_until_gone(parent_pid);
            wait_until_gone(grandchild_pid);
            assert!(
                elapsed < Duration::from_secs(7),
                "the whole tree must be gone well inside 7s, took {elapsed:?}"
            );
            tree.parent.kill_and_reap();
        });
    }
}

// ===========================================================================
// 3. SIGTERM is ignored → escalation to SIGKILL, and the report says so
// ===========================================================================

rusty_fork_test! {
    #[test]
    fn ignore_term_escalates_to_kill() {
        block_on(async {
            let h = harness().await;
            let mut stubborn = fixtures::start_ignore_term();
            let pid = stubborn.pid();
            std::thread::sleep(AGE_SETTLE);

            let argv = argv_of(&[&fake_agent().to_string_lossy(), "ignore-term"]);
            let guard = register_pid(&h, &argv, pid, stubborn.pgid()).await;

            let report = kill_tree(&h.supervisor, guard.spawn_id(), fast_opts()).await;
            assert_eq!(report.refused_reason, None, "{report:?}");
            assert!(
                report.escalated.contains(&pid),
                "the report must show the SIGTERM→SIGKILL escalation: {report:?}"
            );
            assert!(report.survivors.is_empty(), "SIGKILL always wins: {report:?}");
            wait_until_gone(pid);

            let row = h.row(guard.spawn_id()).await;
            assert_eq!(row.state, ProcState::Killed, "row must record the kill");
            assert_eq!(
                row.exit_signal,
                Some(libc::SIGKILL as i64),
                "the row records which signal ended it"
            );
            stubborn.kill_and_reap();
        });
    }
}

// ===========================================================================
// 4. a row naming one of our ancestors is refused — no signal at all
// ===========================================================================

rusty_fork_test! {
    #[test]
    fn ancestor_refused() {
        block_on(async {
            let h = harness().await;
            let snap = snapshot();
            let me = std::process::id() as i32;
            let ancestors = snap.ancestors_of(me);
            let parent = *ancestors
                .iter()
                .find(|p| **p > 1)
                .expect("this test process has a parent");

            // Build a row that describes the parent *exactly*, so the identity
            // fences pass and only the exclusion set stands between the parent
            // and a signal. That is precisely what this test is for.
            let parent_argv = live_argv(parent).expect("our parent's argv is readable");
            let parent_start = start_time_of(parent).expect("our parent's start time");
            let guard = h
                .supervisor
                .plan_spawn(SpawnPlan::new(ProcKind::Pty, parent_argv.clone(), "/tmp"))
                .await
                .expect("plan_spawn");
            h.force_identity(
                guard.spawn_id(),
                parent as i64,
                None,
                parent_start,
                Some(parent_argv),
            )
            .await;

            let row = h.row(guard.spawn_id()).await;
            assert_eq!(
                verify(&row, &snap),
                Fence::Verified,
                "the fences must pass, or this test is not exercising the exclusion set"
            );

            let report = kill_tree(&h.supervisor, guard.spawn_id(), fast_opts()).await;
            assert_eq!(
                report.refused_reason.as_deref(),
                Some(reason::ANCESTOR),
                "our ancestor chain is untouchable: {report:?}"
            );
            // The contract, not an accident of scheduling: a refused kill sends
            // no signal at all (DESIGN §8-4 — nothing may be signalled without
            // `Verified`; DESIGN §8-6 — a refusal answers with an empty
            // `requested`), so `requested` must be empty even though the
            // descendant walk discovered pids. Until `KillReport::with_reason`
            // cleared it, the field leaked the *planned* target set, which made
            // this assertion fail whenever a sibling test spawned children in
            // the same process tree.
            assert!(report.requested.is_empty(), "no pid may be signalled: {report:?}");
            assert!(report.escalated.is_empty(), "and none may be escalated: {report:?}");
            assert!(report.survivors.is_empty(), "a refusal has no survivors: {report:?}");
            assert!(
                report.excluded.iter().any(|e| e.pid == parent && e.reason == reason::ANCESTOR),
                "the refusal must name the pid and the reason: {report:?}"
            );
            assert!(is_alive(parent), "our parent must still be running");
            assert!(is_alive(me), "we must still be running");

            guard.abandon("test");
        });
    }
}

// ===========================================================================
// 5. pid reuse — the innocent process holding the recycled pid is untouched
// ===========================================================================

rusty_fork_test! {
    #[test]
    fn pid_reuse_never_signalled() {
        block_on(async {
            let h = harness().await;
            let argv = argv_of(&[&fake_agent().to_string_lossy(), "hang"]);
            let (guard, mut kid) = register(&h, &argv).await;
            let row = h.row(guard.spawn_id()).await;
            let original_start = row.proc_start_ts.expect("start time recorded");

            // The registered process dies, and something unrelated takes over
            // its slot in the row (T08 acceptance 6c).
            kid.kill_and_reap();
            wait_until_gone(kid.pid());

            let mut innocent = spawn_child(&argv_of(&["/bin/sleep", "30"]), None);
            h.force_identity(
                guard.spawn_id(),
                innocent.pid() as i64,
                Some(innocent.pid() as i64),
                original_start,
                None,
            )
            .await;

            let report = kill_tree(&h.supervisor, guard.spawn_id(), fast_opts()).await;
            assert!(
                report.refused_reason.is_some(),
                "a mismatched row must be refused: {report:?}"
            );
            assert!(report.requested.is_empty(), "no signal may be sent: {report:?}");
            assert!(
                is_alive(innocent.pid()),
                "the innocent process holding the recycled pid must survive"
            );

            let row = h.row(guard.spawn_id()).await;
            assert_eq!(row.state, ProcState::Lost, "the row is marked lost, not killed");

            innocent.kill_and_reap();
        });
    }
}

// ===========================================================================
// 6. the start-time re-check happens before SIGKILL
// ===========================================================================

rusty_fork_test! {
    #[test]
    fn start_time_recheck_before_kill() {
        // Simulates "the survivor died during the grace window and its pid was
        // recycled": the pid is alive, but its kernel start time is no longer the
        // one recorded before SIGTERM. The escalation gate must drop it, and the
        // process holding that pid must never see SIGKILL.
        let mut stubborn = fixtures::start_ignore_term();
        let pid = stubborn.pid();
        let real_start = start_time_of(pid).expect("start time");

        let mut recorded = HashMap::new();
        recorded.insert(pid, Some(real_start + 7));
        let (escalate, dropped) = survivors_still_ours(&[pid], &recorded);
        assert!(
            escalate.is_empty(),
            "a pid whose start time moved must never be escalated"
        );
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].pid, pid);
        assert_eq!(dropped[0].reason, reason::START_TIME_CHANGED);
        assert!(is_alive(pid), "and it must still be alive — no signal was sent");

        // The same pid with its true start time is still ours.
        let mut recorded = HashMap::new();
        recorded.insert(pid, Some(real_start));
        let (escalate, dropped) = survivors_still_ours(&[pid], &recorded);
        assert_eq!(escalate, vec![pid]);
        assert!(dropped.is_empty());

        stubborn.kill_and_reap();
    }
}

// ===========================================================================
// 7. a dry run computes everything and signals nothing
// ===========================================================================

rusty_fork_test! {
    #[test]
    fn dry_run_signals_nothing() {
        block_on(async {
            let h = harness().await;
            let mut tree = fixtures::start_spawn_grandchild(60);
            let parent_pid = tree.parent.pid();
            let grandchild_pid = tree.grandchild_pid;
            std::thread::sleep(AGE_SETTLE);

            let argv = argv_of(&[&fake_agent().to_string_lossy(), "spawn-grandchild", "60"]);
            let guard = register_pid(&h, &argv, parent_pid, tree.parent.pgid()).await;

            let report = kill_tree(
                &h.supervisor,
                guard.spawn_id(),
                fast_opts().dry_run(true),
            )
            .await;

            assert!(report.dry_run, "{report:?}");
            assert_eq!(report.refused_reason, None, "{report:?}");
            assert!(
                report.requested.contains(&parent_pid) && report.requested.contains(&grandchild_pid),
                "a dry run reports the full target set: {report:?}"
            );
            assert!(report.escalated.is_empty());
            assert!(report.survivors.is_empty());

            std::thread::sleep(Duration::from_millis(300));
            assert!(is_alive(parent_pid), "a dry run must not signal");
            assert!(is_alive(grandchild_pid), "a dry run must not signal");

            let row = h.row(guard.spawn_id()).await;
            assert_eq!(row.state, ProcState::Running, "a dry run does not close the row");

            tree.parent.kill_and_reap();
        });
    }
}

// ===========================================================================
// 8. every kill writes an event_log row carrying the fences and per-pid results
// ===========================================================================

rusty_fork_test! {
    #[test]
    fn every_kill_writes_event_log() {
        block_on(async {
            let h = harness().await;
            let argv = argv_of(&[&fake_agent().to_string_lossy(), "hang"]);
            let (guard, mut kid) = register(&h, &argv).await;
            let pid = kid.pid();
            std::thread::sleep(AGE_SETTLE);

            let report = kill_tree(&h.supervisor, guard.spawn_id(), fast_opts()).await;
            assert_eq!(report.refused_reason, None, "{report:?}");

            let rows = h.event_rows(guard.spawn_id()).await;
            assert_eq!(rows.len(), 1, "exactly one audit row per kill");
            let row = &rows[0];
            assert_eq!(row.category, "kill");
            let detail: serde_json::Value =
                serde_json::from_str(row.detail_json.as_deref().expect("detail_json"))
                    .expect("detail is json");
            assert_eq!(detail["fence"], "verified", "the fence result is recorded");
            assert_eq!(detail["dryRun"], false);
            assert!(
                detail["requested"].as_array().expect("requested").iter().any(|v| v == pid),
                "targets are recorded: {detail}"
            );
            let per_pid = detail["perPid"].as_array().expect("perPid");
            let mine = per_pid
                .iter()
                .find(|e| e["pid"] == pid)
                .unwrap_or_else(|| panic!("no per-pid row for {pid}: {detail}"));
            assert_eq!(mine["term"], true, "per-pid results are recorded: {detail}");
            assert_eq!(mine["gone"], true);
            assert!(detail["argv"].as_str().expect("argv").contains("fake_agent"));

            // A refused kill leaves a row too — an unexplained kill is worse
            // than a noisy log.
            let second = kill_tree(&h.supervisor, guard.spawn_id(), fast_opts()).await;
            assert!(second.refused_reason.is_some(), "{second:?}");
            let rows = h.event_rows(guard.spawn_id()).await;
            assert_eq!(rows.len(), 2, "the refusal is audited as well");

            kid.kill_and_reap();
        });
    }
}

// ===========================================================================
// 9. kill_pids: batch shape, and a bare pid with no registry row
// ===========================================================================

rusty_fork_test! {
    #[test]
    fn kill_pids_reports_one_outcome_per_target_in_order() {
        block_on(async {
            let h = harness().await;
            let argv = argv_of(&[&fake_agent().to_string_lossy(), "hang"]);
            let (guard, mut kid) = register(&h, &argv).await;
            let unmanaged = fixtures::start_hang();
            let unmanaged_pid = unmanaged.pid();
            std::thread::sleep(AGE_SETTLE);

            let targets = vec![
                KillTarget::spawn(guard.spawn_id()),
                KillTarget::pid(unmanaged_pid),
                // pid 1 is launchd: refused under every option.
                KillTarget::pid(1),
            ];
            let outcomes = kill_pids(&h.supervisor, &targets, fast_opts()).await;

            assert_eq!(outcomes.len(), 3, "one outcome per target, in order");
            assert_eq!(outcomes[0].target, KillTarget::spawn(guard.spawn_id()));
            assert_eq!(outcomes[0].report.refused_reason, None, "{:?}", outcomes[0]);
            assert_eq!(outcomes[1].target, KillTarget::pid(unmanaged_pid));
            assert_eq!(outcomes[1].report.refused_reason, None, "{:?}", outcomes[1]);
            assert_eq!(
                outcomes[2].report.refused_reason.as_deref(),
                Some(reason::INIT),
                "pid 1 is never killable: {:?}",
                outcomes[2]
            );
            assert!(is_alive(1), "launchd is still there");

            wait_until_gone(kid.pid());
            wait_until_gone(unmanaged_pid);
            kid.kill_and_reap();
            // The bare-pid path harvested this one (nothing else owned it), so
            // its handle must not be waited on again.
            drop(unmanaged);
        });
    }
}

// ===========================================================================
// 10. a row for a process that already exited is closed, not signalled
// ===========================================================================

rusty_fork_test! {
    #[test]
    fn a_gone_process_is_reported_gone_and_the_row_is_closed() {
        block_on(async {
            let h = harness().await;
            let argv = argv_of(&[&fake_agent().to_string_lossy(), "hang"]);
            let (guard, mut kid) = register(&h, &argv).await;
            let pid = kid.pid();
            kid.kill_and_reap();
            wait_until_gone(pid);

            let report = kill_tree(&h.supervisor, guard.spawn_id(), fast_opts()).await;
            assert_eq!(report.refused_reason.as_deref(), Some(reason::GONE), "{report:?}");
            assert!(report.requested.is_empty());

            let row = h.row(guard.spawn_id()).await;
            assert_eq!(row.state, ProcState::Exited);
            assert_eq!(row.note.as_deref(), Some("reaped"));
        });
    }
}

// ===========================================================================
// 11. an unknown spawn id is refused, and refusals never panic
// ===========================================================================

rusty_fork_test! {
    #[test]
    fn an_unknown_spawn_id_is_refused_not_an_error() {
        block_on(async {
            let h = harness().await;
            let report = kill_tree(&h.supervisor, "no-such-spawn", fast_opts()).await;
            assert_eq!(report.refused_reason.as_deref(), Some(reason::NOT_FOUND));
            assert_eq!(report.spawn_id.as_deref(), Some("no-such-spawn"));
            assert!(report.requested.is_empty());
            assert_report_is_honest(&report);
        });
    }
}

// ===========================================================================
// 12. the source of the kill path contains no name-matching bulk terminator
// ===========================================================================

#[test]
fn the_kill_path_never_matches_processes_by_name() {
    let source = include_str!("../src/supervise/kill.rs");
    for banned in ["p".to_string() + "kill", "kill".to_string() + "all"] {
        assert!(
            !source.contains(&banned),
            "{banned} must never appear in the kill path (DESIGN §8-6)"
        );
    }
}

// ===========================================================================
// 13. the platform fact the whole report depends on
// ===========================================================================

#[test]
fn a_killed_but_unreaped_child_still_answers_kill_zero() {
    // Measured, not assumed: a zombie answers `kill(pid, 0)`, so `is_alive`
    // alone would report every killed direct child as a SIGKILL survivor.
    let mut kid = spawn_child(&argv_of(&[&fake_agent().to_string_lossy(), "hang"]), None);
    let pid = kid.pid();
    // SAFETY: `kid` owns this pid and reaps it on drop.
    unsafe { libc::kill(pid, libc::SIGKILL) };

    wait_until_gone(pid);
    assert!(
        is_alive(pid),
        "nothing has reaped this child, so it is still a zombie"
    );
    assert!(is_gone(pid), "the kill path must see through the zombie");
    assert_eq!(
        start_time_of(pid),
        None,
        "the kernel keeps no proc_bsdinfo for a zombie — the cheap pre-filter"
    );

    kid.kill_and_reap();
    assert!(!is_alive(pid), "and once reaped it is gone for everybody");
}

// ===========================================================================
// 8. the packaged bundle's own sidecar — the defect no unit test could see
// ===========================================================================

/// Manual verification of PINNED §23-1 / M1-VERIFICATION §H-6: an orphaned
/// `gilde-node` **inside our own `.app`** is a legal kill target.
///
/// The defect existed only in the bundle layout. `externalBin` puts the pinned
/// Node runtime at `Gilde Code.app/Contents/MacOS/gilde-node`, which
/// matched `is_app_bundle_main`'s `.*\.app/Contents/MacOS/[^/]+$`, so the
/// Recovered Sessions sheet answered `Terminate` with "Identity check failed,
/// so nothing was signalled" and the Process Monitor filed the row under
/// `Protected` — leaving no UI path at all to a sidecar that was burning a
/// core. In the dev tree the same binary lives in `src-tauri/binaries/`, does
/// not match the pattern, and the whole class of bug is invisible: that is why
/// this check has to be run against a real bundle, by hand.
///
/// ```text
/// npm run tauri build -- --bundles app
/// APP="src-tauri/target/release/bundle/macos/Gilde Code.app"
/// "$APP/Contents/MacOS/gilde" &          # let the sidecar start
/// kill -9 <the host pid>                              # orphan it
/// GILDE_ORPHAN_PID=<the gilde-node pid> cargo test --test kill_safety -- \
///     --ignored --nocapture the_orphaned_bundled_sidecar
/// ```
///
/// It **kills** the pid it is given: that is the assertion. Pass it only a
/// sidecar you orphaned yourself.
#[test]
#[ignore = "needs a live orphaned sidecar from a packaged build; see the doc comment"]
fn the_orphaned_bundled_sidecar_is_a_legal_kill_target() {
    let pid: i32 = std::env::var("GILDE_ORPHAN_PID")
        .expect("set GILDE_ORPHAN_PID to the orphaned gilde-node pid")
        .trim()
        .parse()
        .expect("GILDE_ORPHAN_PID must be a pid");

    let snap = snapshot();
    let row = snap
        .get(pid)
        .unwrap_or_else(|| panic!("pid {pid} is not in the ps snapshot"));
    println!("[INT-B] target {pid}: {}", row.args);
    assert!(
        row.args.contains(".app/Contents/MacOS/gilde-node"),
        "this test is about the *bundled* sidecar: {}",
        row.args
    );
    assert!(
        !snap.is_app_bundle_main(pid),
        "our own bundled sidecar is not a foreground GUI app (DESIGN §8-4/§8-6)"
    );

    block_on(async {
        let h = harness().await;

        // The dry run first: it is the same `KillPlan` the sheet executes, and
        // it proves the decision before anything is signalled.
        let dry = kill_bare_pid(&h.supervisor, pid, KillOpts::default().dry_run(true)).await;
        println!("[INT-B] dry run: {dry:?}");
        assert_eq!(
            dry.refused_reason, None,
            "the dry run must not be refused: {dry:?}"
        );
        assert!(
            dry.requested.contains(&pid),
            "the plan must name the sidecar as a target: {dry:?}"
        );
        assert!(
            !dry.excluded.iter().any(|e| e.pid == pid),
            "and must not spare it for any reason: {dry:?}"
        );

        let report = kill_bare_pid(&h.supervisor, pid, fast_opts()).await;
        println!("[INT-B] kill: {report:?}");
        assert_eq!(report.refused_reason, None, "no refusal: {report:?}");
        assert_eq!(report.requested, vec![pid], "requested exactly the sidecar");
        assert!(report.survivors.is_empty(), "and it did not survive");
        assert!(is_gone(pid), "ps agrees the process is gone");
    });
}

// ===========================================================================
// 14. A4: protected processes are refused on every kill path, with a real
//     (non dry-run) request, against a `.app` main binary we started ourselves
// ===========================================================================

/// A copy of the fixture inside a temp `.app` bundle, run as its main binary:
/// to the scanner and the kill path it is a foreground GUI application, and it
/// is a process this test started, so a bug can only ever hurt the test.
fn spawn_fake_bundle_main(dir: &std::path::Path) -> Kid {
    let macos = dir.join("Kill Probe.app/Contents/MacOS");
    std::fs::create_dir_all(&macos).expect("bundle dir");
    let main = macos.join("Kill Probe");
    std::fs::copy(fake_agent(), &main).expect("copy the fixture into the bundle");
    spawn_child(&argv_of(&[&main.to_string_lossy(), "hang"]), None)
}

rusty_fork_test! {
    #[test]
    fn a_bundle_main_is_refused_by_the_tree_the_bare_and_the_bulk_path() {
        block_on(async {
            let h = harness().await;
            let dir = tempfile::tempdir().expect("tempdir");
            let mut app = spawn_fake_bundle_main(dir.path());
            let pid = app.pid();
            std::thread::sleep(AGE_SETTLE);
            assert!(snapshot().is_app_bundle_main(pid), "the probe must read as a bundle main");

            // 1. the bare-pid path (Process Monitor single row, health clean-up)
            let bare = kill_bare_pid(&h.supervisor, pid, fast_opts()).await;
            assert_eq!(bare.refused_reason.as_deref(), Some(reason::APP_BUNDLE_MAIN), "{bare:?}");
            assert!(bare.requested.is_empty(), "no pid may be signalled: {bare:?}");

            // 2. kill_pids routes a bare pid the same way
            let routed = kill_pids(&h.supervisor, &[KillTarget::pid(pid)], fast_opts()).await;
            assert_eq!(
                routed[0].report.refused_reason.as_deref(),
                Some(reason::APP_BUNDLE_MAIN),
                "{routed:?}"
            );

            // 3. a registry row naming it, fences passing: the tree path refuses
            let argv: Vec<String> = gilde_code_lib::supervise::identity::live_argv(pid)
                .expect("the probe's argv is readable");
            let guard = register_pid(&h, &argv, pid, pid).await;
            let row = h.row(guard.spawn_id()).await;
            assert_eq!(verify(&row, &snapshot()), Fence::Verified, "fences must pass for this to test the exclusion");
            let tree = kill_tree(&h.supervisor, guard.spawn_id(), fast_opts()).await;
            assert_eq!(tree.refused_reason.as_deref(), Some(reason::APP_BUNDLE_MAIN), "{tree:?}");
            assert!(tree.requested.is_empty(), "{tree:?}");

            // 4. the bulk gate (procmon_kill with several rows)
            let bystander = spawn_child(&argv_of(&[&fake_agent().to_string_lossy(), "hang"]), None);
            std::thread::sleep(AGE_SETTLE);
            let bulk = gilde_code_lib::commands::monitor::kill_targets(
                &h.supervisor,
                &[KillTarget::pid(pid), KillTarget::pid(bystander.pid())],
                fast_opts().dry_run(true),
                &HashMap::new(),
                0,
            )
            .await;
            assert_eq!(
                bulk[0].report.refused_reason.as_deref(),
                Some(reason::APP_BUNDLE_MAIN),
                "{bulk:?}"
            );

            assert!(is_alive(pid), "the bundle main must have received no signal");
            guard.abandon("test");
            app.kill_and_reap();
            drop(bystander);
        });
    }
}

// ===========================================================================
// 15. A4: a kill never crosses into another process group
// ===========================================================================

rusty_fork_test! {
    #[test]
    fn a_tree_kill_never_reaches_a_sibling_group_even_when_the_row_names_it() {
        block_on(async {
            let h = harness().await;
            let hang = argv_of(&[&fake_agent().to_string_lossy(), "hang"]);
            let (guard, mut target) = register(&h, &hang).await;
            // A bystander in a group of its own (ours, so a bug cannot hurt
            // anything that matters — but it must not be signalled either).
            let mut bystander = spawn_child(&hang, None);
            let bystander_pid = bystander.pid();
            std::thread::sleep(AGE_SETTLE);
            assert_ne!(
                snapshot().get(bystander_pid).map(|r| r.pgid),
                Some(target.pid()),
                "the bystander leads its own group"
            );

            // The worst case: the row's recorded pgid is a STALE value that now
            // names the bystander's group (its leader pid was recycled). The
            // group signal is fenced on "one of our verified targets is in it".
            let row = h.row(guard.spawn_id()).await;
            h.force_identity(
                guard.spawn_id(),
                i64::from(target.pid()),
                Some(i64::from(bystander_pid)),
                row.proc_start_ts.expect("start time recorded"),
                None,
            )
            .await;

            let report = kill_tree(&h.supervisor, guard.spawn_id(), fast_opts()).await;
            assert_eq!(report.refused_reason, None, "{report:?}");
            assert!(!report.requested.contains(&bystander_pid), "{report:?}");
            assert!(
                report.excluded.iter().any(|e| e.pid == bystander_pid && e.reason == reason::GROUP_UNSAFE),
                "the foreign group must be named as unsafe, not signalled: {report:?}"
            );
            wait_until_gone(target.pid());
            assert!(is_alive(bystander_pid), "a foreign group was signalled");
            target.kill_and_reap();
            bystander.kill_and_reap();
        });
    }
}

rusty_fork_test! {
    #[test]
    fn a_bare_pid_kill_never_signals_its_group_siblings() {
        block_on(async {
            let h = harness().await;
            let fake = fake_agent();
            // One group (the shell leads it), two sibling fixtures inside it.
            let script = format!("\"{0}\" hang & \"{0}\" hang & wait", fake.display());
            let mut shell = spawn_child(&argv_of(&["/bin/sh", "-c", &script]), None);
            let shell_pid = shell.pid();
            let deadline = Instant::now() + SETTLE;
            let kids = loop {
                let kids = snapshot().descendants_of(shell_pid);
                if kids.len() >= 2 {
                    break kids;
                }
                assert!(Instant::now() < deadline, "the shell never started both fixtures");
                std::thread::sleep(Duration::from_millis(20));
            };
            std::thread::sleep(AGE_SETTLE);
            let snap = snapshot();
            let (victim, sibling) = (kids[0], kids[1]);
            assert_eq!(snap.get(victim).map(|r| r.pgid), Some(shell_pid));
            assert_eq!(snap.get(sibling).map(|r| r.pgid), Some(shell_pid));

            let report = kill_bare_pid(&h.supervisor, victim, fast_opts()).await;
            assert_eq!(report.refused_reason, None, "{report:?}");
            assert_eq!(report.requested, vec![victim], "only the pid itself (it has no children): {report:?}");
            wait_until_gone(victim);
            assert!(is_alive(sibling), "a group sibling was signalled");
            assert!(is_alive(shell_pid), "the group leader was signalled");
            shell.kill_and_reap();
            // The shell is gone; its other child is ours to end.
            fixtures::kill_pid(sibling, libc::SIGKILL);
        });
    }
}
