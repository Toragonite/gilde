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
use gilde_code_lib::supervise::registry::{SpawnGuard, Supervisor};
use gilde_code_lib::supervise::spawn::is_alive;

use rusty_fork::rusty_fork_test;
use std::os::unix::process::CommandExt as _;

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


/// The §9.3 allowlist admits only resolved binaries: a fixture is
/// allowlisted for the kind it is planned as before it is planned (A12.8).
fn allow(argv: &[String], kind: ProcKind) {
    gilde_code_lib::supervise::spawn::allow_executable(std::path::Path::new(&argv[0]), &[kind]);
}

/// Plan a row, start the process it describes, mark it running — DESIGN §8-3.
async fn register(h: &Harness, argv: &[String]) -> (SpawnGuard, Kid) {
    allow(argv, ProcKind::Pty);
    let guard = h
        .supervisor
        .plan_guarded(fixture_request(ProcKind::Pty, argv, "/tmp", None, None))
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
    allow(argv, ProcKind::Pty);
    let guard = h
        .supervisor
        .plan_guarded(fixture_request(ProcKind::Pty, argv, "/tmp", None, None))
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
            allow(&parent_argv, ProcKind::Pty);
            let guard = h
                .supervisor
                .plan_guarded(fixture_request(ProcKind::Pty, &parent_argv, "/tmp", None, None))
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
    fn a_bundle_main_is_refused_by_the_tree_and_the_bare_path() {
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

            // (4. the Process Monitor bulk gate was retired with
            // `commands::monitor`, contract §2.2.1; the Clean-up stop goes
            // through the bare-pid path of step 1.)

            assert!(is_alive(pid), "the bundle main must have received no signal");
            guard.abandon("test");
            app.kill_and_reap();
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

// ===========================================================================
// Gilde 2: never SIGKILL a vendor CLI (contract §9.8, C5) [F-32]
// ===========================================================================

/// A copy of the fixture named `claude`: the kernel's exec path then names a
/// vendor CLI, so the classifier (§5.2.2) treats it as one. Never a real CLI.
fn vendor_copy(dir: &std::path::Path) -> PathBuf {
    let dest = dir.join("claude");
    std::fs::copy(fake_agent(), &dest).expect("copy fixture as claude");
    dest
}

/// Register an arbitrary argv under `kind` and start it with the env marker.
async fn register_kind(h: &Harness, kind: ProcKind, argv: &[String]) -> (SpawnGuard, Kid) {
    allow(argv, kind);
    let guard = h
        .supervisor
        .plan_guarded(fixture_request(kind, argv, "/tmp", None, None))
        .await
        .expect("plan_spawn");
    let kid = spawn_child(argv, Some(guard.env_marker()));
    let pid = kid.pid();
    guard.mark_running(pid, pid).expect("mark_running");
    (guard, kid)
}

rusty_fork_test! {
    #[test]
    fn a_vendor_cli_that_ignores_sigterm_is_never_sigkilled() {
        block_on(async {
            let h = harness().await;
            let bin = tempfile::tempdir().expect("bin");
            let claude = vendor_copy(bin.path());
            assert_eq!(
                gilde_code_lib::supervise::procscan::vendor_of_exec(&claude.to_string_lossy(), None),
                Some("claude")
            );
            let argv = argv_of(&[&claude.to_string_lossy(), "ignore-term"]);
            let (guard, mut kid) = register_kind(&h, ProcKind::Convo, &argv).await;
            std::thread::sleep(AGE_SETTLE);
            let pid = kid.pid();

            let report = kill_tree(&h.supervisor, guard.spawn_id(), fast_opts()).await;
            assert_report_is_honest(&report);
            assert_eq!(report.refused_reason, None, "{report:?}");
            assert!(report.requested.contains(&pid), "SIGTERM was sent: {report:?}");
            assert!(report.escalated.is_empty(), "no pid may be escalated: {report:?}");
            assert!(report.survivors.contains(&pid), "the survivor is reported: {report:?}");
            assert!(
                report.held().iter().any(|e| e.pid == pid && e.reason == reason::VENDOR_CLI),
                "held as a vendor CLI: {report:?}"
            );
            // Still alive well after the kill-verify budget: nothing SIGKILLed it.
            std::thread::sleep(Duration::from_millis(500));
            assert!(is_alive(pid) && !is_gone(pid), "a vendor CLI must never be SIGKILLed");
            let row = h.row(guard.spawn_id()).await;
            assert_eq!(row.state, ProcState::KillIncomplete);
            kid.kill_and_reap();
        });
    }
}

rusty_fork_test! {
    #[test]
    fn a_pane_sh_wrapper_with_a_live_vendor_child_is_never_sigkilled() {
        block_on(async {
            let h = harness().await;
            let bin = tempfile::tempdir().expect("bin");
            let claude = vendor_copy(bin.path());
            // `sh` stays the parent (no exec: a command follows) and ignores
            // HUP and TERM; its claude child inherits the ignored HUP and
            // ignores TERM itself.
            let script = format!("trap '' HUP TERM; {} ignore-term; true", claude.display());
            let argv = argv_of(&["/bin/sh", "-c", &script]);
            let (guard, mut kid) = register_kind(&h, ProcKind::Pane, &argv).await;
            std::thread::sleep(Duration::from_millis(800));
            let sh = kid.pid();
            let snap = snapshot();
            let child = snap
                .descendants_of(sh)
                .into_iter()
                .find(|p| gilde_code_lib::supervise::procscan::vendor_of_pid(*p) == Some("claude"))
                .expect("the vendor child runs under the wrapper");

            let report = kill_tree(&h.supervisor, guard.spawn_id(), fast_opts().hup_first(true)).await;
            assert_report_is_honest(&report);
            assert!(report.escalated.is_empty(), "nothing may be escalated: {report:?}");
            let held = report.held();
            assert!(
                held.iter().any(|e| e.pid == sh && e.reason == reason::VENDOR_DESCENDANT),
                "the wrapper is held while its vendor child lives: {report:?}"
            );
            assert!(
                held.iter().any(|e| e.pid == child && e.reason == reason::VENDOR_CLI),
                "the child is held as a vendor CLI: {report:?}"
            );
            std::thread::sleep(Duration::from_millis(500));
            assert!(is_alive(sh), "the sh wrapper was SIGKILLed");
            assert!(is_alive(child), "the vendor child was SIGKILLed");
            // Test cleanup of processes this test started.
            unsafe {
                libc::kill(child, libc::SIGKILL);
            }
            kid.kill_and_reap();
        });
    }
}

rusty_fork_test! {
    #[test]
    fn a_non_vendor_helper_is_still_escalated() {
        block_on(async {
            let h = harness().await;
            let argv = argv_of(&[&fake_agent().to_string_lossy(), "ignore-term"]);
            let (guard, kid) = register_kind(&h, ProcKind::Probe, &argv).await;
            std::thread::sleep(AGE_SETTLE);
            let pid = kid.pid();
            let report = kill_tree(&h.supervisor, guard.spawn_id(), fast_opts()).await;
            assert!(report.escalated.contains(&pid), "a helper keeps TERM → KILL: {report:?}");
            assert!(report.held().is_empty(), "{report:?}");
            wait_until_gone(pid);
        });
    }
}

#[test]
fn the_never_kill_split_holds_vendors_and_their_wrappers() {
    use gilde_code_lib::supervise::kill::split_never_kill;
    use gilde_code_lib::supervise::procscan::PsRow;
    let row = |pid: i32, ppid: i32, args: &str| PsRow {
        pid,
        ppid,
        pgid: 100,
        rss_kb: 1,
        cpu_pct: 0.0,
        elapsed_secs: 10,
        user: "u".into(),
        args: args.into(),
    };
    let snap = Snapshot::from_rows(vec![row(100, 1, "/bin/sh -c x"), row(101, 100, "claude"), row(102, 1, "git")]);
    let vendor: std::collections::HashSet<i32> = [101].into_iter().collect();
    // 100 and 101 are not live pids here, so only the plan-time set speaks for
    // them; 102 is not a vendor and has no vendor below it.
    let (escalate, held) = split_never_kill(&[101], &snap, &vendor);
    assert!(escalate.is_empty());
    assert_eq!(held[0].reason, reason::VENDOR_CLI);
}

// ===========================================================================
// A39 (G7c-01 BLOCKER): one vendor classifier that knows the install layouts,
// and Gilde's own vendor rows are never SIGKILLed whatever their exec path
// classifies as (contract §9.8 [C5], A39.1). The subjects are copies of the
// `fake_agent` fixture placed at the real layouts' paths and a three-line C
// program standing in for a `node`/`bun` that accepts any argv. No real
// vendor CLI runs; every signal reaches only a process this test started.
// ===========================================================================

const NATIVE_CLAUDE: &str = "/Users/u/.local/share/claude/versions/2.1.307";
const CURSOR_NODE: &str = "/Users/u/.local/share/cursor-agent/versions/2026.10.19-6bbf2c4/node";
const CURSOR_INDEX: &str = "/Users/u/.local/share/cursor-agent/versions/2026.10.19-6bbf2c4/index.js";
/// A Gilde session uid owning a PTY or pane row (`parent_uid`, §3.8).
const OWNER_UID: &str = "s_01890a5d-ac96-774b-bcce-b302099a8057";

#[test]
fn a39_the_supervise_classifier_knows_the_install_layouts() {
    use gilde_code_lib::supervise::procscan::{vendor_of_argv, vendor_of_exec};
    // The reviewer's G7c-01a/b, verbatim.
    assert_eq!(vendor_of_exec(NATIVE_CLAUDE, None), Some("claude"), "the native installer layout");
    assert_eq!(vendor_of_exec(CURSOR_NODE, Some("--use-system-ca")), Some("cursor"), "cursor's node launch");
    // Another semver, and the launcher's real argv (`exec -a "$0"`).
    assert_eq!(vendor_of_exec("/Users/u/.local/share/claude/versions/3.0.0-beta.1", None), Some("claude"));
    let launcher = ["/Users/u/.local/bin/cursor-agent", "--use-system-ca", CURSOR_INDEX, "--resume", "abc123"];
    assert_eq!(vendor_of_argv(CURSOR_NODE, &launcher), Some("cursor"));
    // Other node flags, a bare flag with its value, and bun.
    let cli = "/Users/u/.npm-global/lib/node_modules/@anthropic-ai/claude-code/cli.js";
    assert_eq!(vendor_of_argv("/opt/homebrew/bin/node", &["node", "--max-old-space-size=4096", cli]), Some("claude"));
    assert_eq!(vendor_of_argv("/opt/homebrew/bin/node", &["node", "--max-old-space-size", "4096", cli]), Some("claude"));
    assert_eq!(vendor_of_argv("/opt/homebrew/bin/bun", &["bun", "run", cli]), Some("claude"));
    // A plain node running anything else stays non-vendor.
    assert_eq!(vendor_of_argv("/opt/homebrew/bin/node", &["node", "/srv/server.js"]), None);
    assert_eq!(vendor_of_argv("/opt/homebrew/bin/node", &["node", "--inspect", "/srv/server.js"]), None);
    // tmux stays on the protected side.
    assert_eq!(vendor_of_argv::<&str>("/opt/homebrew/bin/tmux", &[]), Some("tmux"));
}

#[test]
fn a39_gilde_vendor_rows_are_the_conversation_pty_pane_turn_and_probe_rows() {
    use gilde_code_lib::supervise::registry::is_own_vendor_row;
    for kind in [ProcKind::Convo, ProcKind::Attach, ProcKind::Turn, ProcKind::Headless, ProcKind::Vendorcli] {
        assert!(is_own_vendor_row(kind, None, false), "{kind:?} launches only a vendor binary");
    }
    for kind in [ProcKind::Pty, ProcKind::Pane] {
        assert!(is_own_vendor_row(kind, None, true), "a session's {kind:?}");
        assert!(!is_own_vendor_row(kind, None, false), "an unowned {kind:?} is decided by the classifier");
        assert!(is_own_vendor_row(kind, Some("claude"), false), "a recorded vendor");
    }
    for kind in [ProcKind::Sidecar, ProcKind::Probe, ProcKind::Git, ProcKind::Ide, ProcKind::Gateway] {
        assert!(!is_own_vendor_row(kind, None, true), "{kind:?} is a helper");
    }
    assert!(!is_own_vendor_row(ProcKind::Probe, Some(""), false), "an empty vendor is none");
}

/// Copy `src` to `dest`, creating its directories.
fn place(src: &std::path::Path, dest: &std::path::Path) -> PathBuf {
    std::fs::create_dir_all(dest.parent().expect("parent")).expect("mkdir");
    std::fs::copy(src, dest).expect("copy");
    dest.to_path_buf()
}

/// A stand-in for a JavaScript runtime that accepts any argv and idles (the
/// `fake_agent` takes a mode as argv[1]; a copied system binary is arm64e and
/// the kernel kills the copy): a three-line C program built at `dest`.
fn standin(dest: &std::path::Path) -> PathBuf {
    std::fs::create_dir_all(dest.parent().expect("parent")).expect("mkdir");
    let src = dest.with_extension("standin.c");
    std::fs::write(&src, "#include <unistd.h>\nint main(void){for(;;)pause();}\n").expect("c source");
    let st = Command::new("/usr/bin/cc").arg("-o").arg(dest).arg(&src).status().expect("cc");
    assert!(st.success(), "cc failed");
    dest.to_path_buf()
}

/// Start `argv` with the row's env marker and SIGTERM ignored. An ignored
/// disposition survives `exec` and `fork`, like a CLI busy in a tool call that
/// does not honour TERM promptly; its children inherit it.
fn spawn_ignoring_term(argv: &[String], marker: &str) -> Kid {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .env("GILDE_SPAWN_ID", marker)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    // SAFETY: `signal(SIG_IGN)` is async-signal-safe and allocates nothing.
    unsafe {
        cmd.pre_exec(|| {
            libc::signal(libc::SIGTERM, libc::SIG_IGN);
            Ok(())
        });
    }
    let child = cmd.spawn().unwrap_or_else(|e| panic!("spawn {argv:?}: {e}"));
    let kid = Kid { child, reaped: false };
    wait_until_visible(kid.pid());
    kid
}

/// Plan a row of `kind` (owned by a session when `owner` is given), start its
/// process ignoring SIGTERM, mark it running.
async fn register_term_proof(h: &Harness, kind: ProcKind, argv: &[String], owner: Option<&str>) -> (SpawnGuard, Kid) {
    allow(argv, kind);
    let guard = h
        .supervisor
        .plan_guarded(fixture_request(kind, argv, "/tmp", owner, None))
        .await
        .expect("plan_spawn");
    let kid = spawn_ignoring_term(argv, guard.env_marker());
    let pid = kid.pid();
    guard.mark_running(pid, pid).expect("mark_running");
    (guard, kid)
}

/// The fenced stop of `guard`'s row: SIGTERM, then `pid` is held as a vendor
/// CLI and is still alive well after the kill-verify budget.
async fn assert_never_sigkilled(h: &Harness, guard: &SpawnGuard, pid: i32, what: &str) -> KillReport {
    let report = kill_tree(&h.supervisor, guard.spawn_id(), fast_opts()).await;
    assert_report_is_honest(&report);
    assert_eq!(report.refused_reason, None, "{what}: {report:?}");
    assert!(report.requested.contains(&pid), "{what}: SIGTERM was sent: {report:?}");
    assert!(report.escalated.is_empty(), "{what}: §9.8 nothing may be escalated: {report:?}");
    assert!(
        report.held().iter().any(|e| e.pid == pid && e.reason == reason::VENDOR_CLI),
        "{what}: held as a vendor CLI: {report:?}"
    );
    std::thread::sleep(Duration::from_millis(500));
    assert!(is_alive(pid) && !is_gone(pid), "{what}: a Gilde vendor session was SIGKILLed");
    assert_eq!(h.row(guard.spawn_id()).await.state, ProcState::KillIncomplete, "{what}");
    report
}

rusty_fork_test! {
    /// G7c-01d: a native-installer claude session (argv[0] = the realpath).
    #[test]
    fn a39_a_native_installer_claude_session_is_never_sigkilled() {
        block_on(async {
            let h = harness().await;
            let home = tempfile::tempdir().expect("home");
            let claude = place(&fake_agent(), &home.path().join(".local/share/claude/versions/2.1.307"));
            let argv = argv_of(&[&claude.to_string_lossy(), "ignore-term"]);
            let (guard, mut kid) = register_term_proof(&h, ProcKind::Convo, &argv, None).await;
            std::thread::sleep(AGE_SETTLE);
            let pid = kid.pid();
            assert_eq!(gilde_code_lib::supervise::procscan::vendor_of_pid(pid), Some("claude"));
            assert_eq!(h.row(guard.spawn_id()).await.vendor.as_deref(), Some("claude"), "the row records the vendor");
            assert_never_sigkilled(&h, &guard, pid, "native claude").await;
            kid.kill_and_reap();
        });
    }
}

rusty_fork_test! {
    /// Another semver, reached through a symlinked data dir and started by a
    /// link whose name is no vendor's. The row is an unowned PTY, so only the
    /// kernel-level classifier (exec path, then the resolved path) protects it.
    #[test]
    fn a39_b_another_semver_through_symlinks_is_still_claude() {
        block_on(async {
            let h = harness().await;
            let home = tempfile::tempdir().expect("home");
            place(&fake_agent(), &home.path().join("data/claude/versions/3.0.0-beta.1"));
            std::fs::create_dir_all(home.path().join(".local/bin")).expect("mkdir");
            std::os::unix::fs::symlink(home.path().join("data"), home.path().join(".local/share")).expect("link dir");
            let link = home.path().join(".local/bin/cl");
            std::os::unix::fs::symlink(home.path().join(".local/share/claude/versions/3.0.0-beta.1"), &link)
                .expect("link bin");
            let argv = argv_of(&[&link.to_string_lossy(), "ignore-term"]);
            let (guard, mut kid) = register_term_proof(&h, ProcKind::Pty, &argv, None).await;
            std::thread::sleep(AGE_SETTLE);
            let pid = kid.pid();
            assert_eq!(gilde_code_lib::supervise::procscan::vendor_of_pid(pid), Some("claude"));
            assert_never_sigkilled(&h, &guard, pid, "symlinked native claude").await;
            kid.kill_and_reap();
        });
    }
}

rusty_fork_test! {
    /// G7c-01e: cursor-agent's `…/versions/<v>/node --use-system-ca …/index.js`.
    #[test]
    fn a39_c_a_cursor_agent_node_session_is_never_sigkilled() {
        block_on(async {
            let h = harness().await;
            let home = tempfile::tempdir().expect("home");
            let vdir = home.path().join(".local/share/cursor-agent/versions/2026.10.19-6bbf2c4");
            let node = standin(&vdir.join("node"));
            let index = vdir.join("index.js");
            std::fs::write(&index, "// stand-in\n").expect("index");
            let argv = argv_of(&[&node.to_string_lossy(), "--use-system-ca", &index.to_string_lossy()]);
            let (guard, mut kid) = register_term_proof(&h, ProcKind::Pty, &argv, None).await;
            std::thread::sleep(AGE_SETTLE);
            let pid = kid.pid();
            assert_eq!(gilde_code_lib::supervise::procscan::vendor_of_pid(pid), Some("cursor"));
            assert_never_sigkilled(&h, &guard, pid, "cursor node").await;
            kid.kill_and_reap();
        });
    }
}

rusty_fork_test! {
    /// Other node flags before the script, and bun with its `run` word.
    #[test]
    fn a39_d_node_with_flags_and_bun_running_claude_are_never_sigkilled() {
        block_on(async {
            let h = harness().await;
            let home = tempfile::tempdir().expect("home");
            let cli = home.path().join("lib/node_modules/@anthropic-ai/claude-code/cli.js");
            std::fs::create_dir_all(cli.parent().expect("parent")).expect("mkdir");
            std::fs::write(&cli, "// stand-in\n").expect("cli");
            let cli = cli.to_string_lossy().into_owned();
            let node = standin(&home.path().join("bin/node")).to_string_lossy().into_owned();
            let bun = standin(&home.path().join("bin/bun")).to_string_lossy().into_owned();
            for argv in [
                argv_of(&[&node, "--max-old-space-size=4096", "--no-warnings", &cli]),
                argv_of(&[&node, "--max-old-space-size", "4096", &cli, "--resume"]),
                argv_of(&[&bun, "run", &cli]),
            ] {
                let (guard, mut kid) = register_term_proof(&h, ProcKind::Pty, &argv, None).await;
                std::thread::sleep(AGE_SETTLE);
                let pid = kid.pid();
                assert_eq!(gilde_code_lib::supervise::procscan::vendor_of_pid(pid), Some("claude"), "{argv:?}");
                assert_never_sigkilled(&h, &guard, pid, &format!("{argv:?}")).await;
                kid.kill_and_reap();
            }
        });
    }
}

rusty_fork_test! {
    /// The classifier does not over-protect: a plain node running another
    /// script, in an unowned row, keeps TERM → KILL.
    #[test]
    fn a39_e_a_plain_node_script_is_still_escalated() {
        block_on(async {
            let h = harness().await;
            let home = tempfile::tempdir().expect("home");
            let node = standin(&home.path().join("bin/node")).to_string_lossy().into_owned();
            let server = home.path().join("srv/server.js").to_string_lossy().into_owned();
            let argv = argv_of(&[&node, "--inspect", &server]);
            let (guard, kid) = register_term_proof(&h, ProcKind::Pty, &argv, None).await;
            std::thread::sleep(AGE_SETTLE);
            let pid = kid.pid();
            assert_eq!(gilde_code_lib::supervise::procscan::vendor_of_pid(pid), None);
            let report = kill_tree(&h.supervisor, guard.spawn_id(), fast_opts()).await;
            assert!(report.escalated.contains(&pid), "a non-vendor keeps TERM → KILL: {report:?}");
            assert!(report.held().is_empty(), "{report:?}");
            wait_until_gone(pid);
        });
    }
}

rusty_fork_test! {
    /// A39.1(b): a Gilde vendor row whose exec path classifies as nothing
    /// (an install layout nobody knows yet) is never SIGKILLed — Conversation,
    /// turn, attach, vendor probe, and a session's PTY and pane member.
    #[test]
    fn a39_f_an_unclassifiable_gilde_vendor_row_is_never_sigkilled() {
        block_on(async {
            let h = harness().await;
            let home = tempfile::tempdir().expect("home");
            let opaque = place(&fake_agent(), &home.path().join("opt/build-7/agent-cli"));
            let argv = argv_of(&[&opaque.to_string_lossy(), "ignore-term"]);
            for (kind, owner) in [
                (ProcKind::Convo, None),
                (ProcKind::Turn, None),
                (ProcKind::Attach, None),
                (ProcKind::Vendorcli, None),
                (ProcKind::Pty, Some(OWNER_UID)),
                (ProcKind::Pane, Some(OWNER_UID)),
            ] {
                let (guard, mut kid) = register_term_proof(&h, kind, &argv, owner).await;
                std::thread::sleep(AGE_SETTLE);
                let pid = kid.pid();
                assert_eq!(gilde_code_lib::supervise::procscan::vendor_of_pid(pid), None, "unclassifiable");
                assert_never_sigkilled(&h, &guard, pid, &format!("{kind:?}")).await;
                kid.kill_and_reap();
            }
        });
    }
}

rusty_fork_test! {
    /// The same unclassifiable binary in a row that is not a Gilde vendor
    /// session (an unowned PTY, a probe) keeps TERM → KILL.
    #[test]
    fn a39_g_the_same_binary_in_a_helper_row_is_still_escalated() {
        block_on(async {
            let h = harness().await;
            let home = tempfile::tempdir().expect("home");
            let opaque = place(&fake_agent(), &home.path().join("opt/build-7/agent-cli"));
            let argv = argv_of(&[&opaque.to_string_lossy(), "ignore-term"]);
            for kind in [ProcKind::Pty, ProcKind::Probe] {
                let (guard, kid) = register_term_proof(&h, kind, &argv, None).await;
                std::thread::sleep(AGE_SETTLE);
                let pid = kid.pid();
                let report = kill_tree(&h.supervisor, guard.spawn_id(), fast_opts()).await;
                assert!(report.escalated.contains(&pid), "{kind:?}: {report:?}");
                wait_until_gone(pid);
            }
        });
    }
}

rusty_fork_test! {
    /// A39.1(b): the group a Gilde vendor row recorded is never
    /// `killpg(SIGKILL)`ed. The session (unclassifiable) has a non-vendor
    /// child in its group that also ignores SIGTERM: the child is escalated by
    /// pid, the session lives — a group SIGKILL would have taken it.
    #[test]
    fn a39_h_a_gilde_vendor_rows_group_is_never_sigkilled() {
        block_on(async {
            let h = harness().await;
            let home = tempfile::tempdir().expect("home");
            let opaque = place(&fake_agent(), &home.path().join("opt/build-7/agent-cli"));
            let argv = argv_of(&[&opaque.to_string_lossy(), "spawn-grandchild", "600"]);
            let (guard, mut kid) = register_term_proof(&h, ProcKind::Convo, &argv, None).await;
            let pid = kid.pid();
            let deadline = Instant::now() + SETTLE;
            let child = loop {
                if let Some(c) = snapshot().descendants_of(pid).into_iter().next() {
                    break c;
                }
                assert!(Instant::now() < deadline, "the child never appeared");
                std::thread::sleep(Duration::from_millis(20));
            };
            std::thread::sleep(AGE_SETTLE);
            assert_eq!(gilde_code_lib::supervise::spawn::pgid_of(child), Some(pid), "the child shares the group");

            let report = kill_tree(&h.supervisor, guard.spawn_id(), fast_opts()).await;
            assert_report_is_honest(&report);
            assert_eq!(report.escalated, vec![child], "only the helper child is escalated, by pid: {report:?}");
            assert!(report.held().iter().any(|e| e.pid == pid && e.reason == reason::VENDOR_CLI), "{report:?}");
            wait_until_gone(child);
            std::thread::sleep(Duration::from_millis(500));
            assert!(is_alive(pid) && !is_gone(pid), "the session's group was SIGKILLed");
            kid.kill_and_reap();
        });
    }
}

rusty_fork_test! {
    /// A39.1(b): a turn the sidecar started is one of Gilde's vendor rows even
    /// when the sidecar is the row being stopped (sweep_generation, quit): the
    /// turn is held as a vendor CLI and the sidecar as its wrapper.
    #[test]
    fn a39_i_a_turn_below_the_stopped_sidecar_is_never_sigkilled() {
        block_on(async {
            let h = harness().await;
            let home = tempfile::tempdir().expect("home");
            let opaque = place(&fake_agent(), &home.path().join("opt/build-7/agent-cli"));
            let sidecar_argv = argv_of(&[&opaque.to_string_lossy(), "spawn-grandchild", "600"]);
            let (sidecar, mut kid) = register_term_proof(&h, ProcKind::Sidecar, &sidecar_argv, None).await;
            let pid = kid.pid();
            let deadline = Instant::now() + SETTLE;
            let turn_pid = loop {
                if let Some(c) = snapshot().descendants_of(pid).into_iter().next() {
                    break c;
                }
                assert!(Instant::now() < deadline, "the turn never appeared");
                std::thread::sleep(Duration::from_millis(20));
            };
            let turn_argv = argv_of(&[&opaque.to_string_lossy(), "__sleep", "600"]);
            allow(&turn_argv, ProcKind::Turn);
            let turn = h
                .supervisor
                .plan_guarded(fixture_request(ProcKind::Turn, &turn_argv, "/tmp", None, None))
                .await
                .expect("plan the turn");
            turn.mark_running(turn_pid, pid).expect("turn running");
            std::thread::sleep(AGE_SETTLE);

            let report = kill_tree(&h.supervisor, sidecar.spawn_id(), fast_opts()).await;
            assert_report_is_honest(&report);
            assert!(report.escalated.is_empty(), "nothing may be escalated: {report:?}");
            let held = report.held();
            assert!(held.iter().any(|e| e.pid == turn_pid && e.reason == reason::VENDOR_CLI), "{report:?}");
            assert!(held.iter().any(|e| e.pid == pid && e.reason == reason::VENDOR_DESCENDANT), "{report:?}");
            std::thread::sleep(Duration::from_millis(500));
            assert!(is_alive(turn_pid), "the turn was SIGKILLed");
            assert!(is_alive(pid), "the sidecar wrapper was SIGKILLed");
            // Test cleanup of processes this test started.
            unsafe {
                libc::kill(turn_pid, libc::SIGKILL);
            }
            kid.kill_and_reap();
        });
    }
}

rusty_fork_test! {
    /// A survivor of an earlier stop (`kill_incomplete`) killed again by pid
    /// from the Process Monitor goes the bare path (no live row claims it):
    /// its finished row still makes it one of Gilde's vendor sessions.
    #[test]
    fn a39_j_a_held_survivor_stays_protected_on_the_bare_pid_path() {
        block_on(async {
            let h = harness().await;
            let home = tempfile::tempdir().expect("home");
            let opaque = place(&fake_agent(), &home.path().join("opt/build-7/agent-cli"));
            let argv = argv_of(&[&opaque.to_string_lossy(), "ignore-term"]);
            let (guard, mut kid) = register_term_proof(&h, ProcKind::Convo, &argv, None).await;
            std::thread::sleep(AGE_SETTLE);
            let pid = kid.pid();
            assert_never_sigkilled(&h, &guard, pid, "first stop").await;

            let outcomes = kill_pids(&h.supervisor, &[KillTarget::pid(pid)], fast_opts()).await;
            let report = &outcomes[0].report;
            assert!(report.escalated.is_empty(), "the bare path escalated a Gilde session: {report:?}");
            std::thread::sleep(Duration::from_millis(500));
            assert!(is_alive(pid) && !is_gone(pid), "a held survivor was SIGKILLed by pid");
            kid.kill_and_reap();
        });
    }
}

// ===========================================================================
// A41 (G7c-05): kill targets are the tree; escapees are listed, never
// signalled. A42 (G7c-06): a `SignalPlan` never escalates, and a pid with no
// Gilde row is signalled alone. Fake processes only.
// ===========================================================================

/// The session of the G7c-05 model: a `/bin/sh` that starts `<bin> escape-group`
/// (which forks a `setsid()` child), kills that launcher so the escapee is
/// reparented to launchd with the marker in its env, then execs into the
/// session's own `claude hang`. Returns the session child, its guard and the
/// escapee's pid.
async fn session_with_escapee(h: &Harness, escapee_bin: &std::path::Path, owner: Option<&str>) -> (Kid, SpawnGuard, i32, tempfile::TempDir) {
    let bin = tempfile::tempdir().expect("bin");
    let claude = bin.path().join("claude");
    std::fs::copy(fake_agent(), &claude).expect("copy");
    let out = bin.path().join("escapee.pid");
    let script = format!(
        "{e} escape-group > {o} & l=$!; sleep 0.5; kill -9 $l; exec {c} hang",
        e = escapee_bin.display(),
        o = out.display(),
        c = claude.display()
    );
    let argv = argv_of(&["/bin/sh", "-c", &script]);
    allow(&argv, ProcKind::Pty);
    let guard = h
        .supervisor
        .plan_guarded(fixture_request(ProcKind::Pty, &argv, "/tmp", owner, None))
        .await
        .expect("plan");
    let kid = spawn_child(&argv, Some(guard.env_marker()));
    let session = kid.pid();
    guard.mark_running(session, session).expect("running");
    let deadline = Instant::now() + SETTLE;
    let escapee: i32 = loop {
        let text = std::fs::read_to_string(&out).unwrap_or_default();
        if let Some(p) = text.lines().find_map(|l| l.strip_prefix("escaped-pid ")).and_then(|p| p.trim().parse().ok()) {
            break p;
        }
        assert!(Instant::now() < deadline, "no escapee announced");
        std::thread::sleep(Duration::from_millis(20));
    };
    // Until the launcher is gone and the escapee reparented.
    let deadline = Instant::now() + SETTLE;
    while snapshot().get(escapee).map(|r| r.ppid) != Some(1) {
        assert!(Instant::now() < deadline, "the escapee was never reparented");
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(AGE_SETTLE);
    (kid, guard, escapee, bin)
}

rusty_fork_test! {
    /// G7c-05, the reviewer's test: stopping a session never signals a
    /// daemonized vendor job it launched; the job is recorded as an escapee.
    #[test]
    fn g7c_05_stopping_a_session_never_signals_a_daemonized_vendor_job_it_launched() {
        block_on(async {
            let h = harness().await;
            let vendor = tempfile::tempdir().expect("vendor");
            let claude = vendor.path().join("claude");
            std::fs::copy(fake_agent(), &claude).expect("copy");
            let (mut kid, guard, escapee, _bin) = session_with_escapee(&h, &claude, None).await;
            let snap = snapshot();
            assert!(!snap.descendants_of(kid.pid()).contains(&escapee), "precondition: not a descendant");
            assert_eq!(gilde_code_lib::supervise::procscan::vendor_of_pid(escapee), Some("claude"));

            let report = kill_tree(&h.supervisor, guard.spawn_id(), fast_opts().hup_first(true)).await;
            std::thread::sleep(Duration::from_millis(300));
            let survived = !is_gone(escapee);
            let listed = gilde_code_lib::supervise::kill::escapees()
                .iter()
                .any(|((pid, _), e)| *pid == escapee && e.spawn_id == guard.spawn_id());
            // Cleanup of what this test started.
            unsafe {
                libc::kill(escapee, libc::SIGKILL);
            }
            kid.kill_and_reap();
            assert!(!report.requested.contains(&escapee), "A41.1: an escapee is never a target: {report:?}");
            assert!(survived, "§9.8: the daemonized job was signalled");
            assert!(listed, "A41.2: the escapee is recorded for the Leftovers list");
        });
    }
}

rusty_fork_test! {
    /// A41.2: a non-vendor detached server (a `nohup`/`setsid` dev server the
    /// agent started) is not signalled by the session's stop, and the health
    /// scan lists it as a Leftover with the session it came from, from the
    /// first scan.
    #[test]
    fn a41_a_detached_server_is_listed_with_its_session_and_never_signalled() {
        block_on(async {
            use gilde_code_lib::contract::process::LeftoverReason;
            use gilde_code_lib::contract::services::{ProcInfo, ProcSnapshot, TeammateLabels};
            use gilde_code_lib::supervise::health::{registry_view, LeftoverTracker};
            let h = harness().await;
            let owner = "s_01890a5d-ac96-774b-bcce-b302099a8057";
            let (mut kid, guard, escapee, _bin) = session_with_escapee(&h, &fake_agent(), Some(owner)).await;
            assert_eq!(gilde_code_lib::supervise::procscan::vendor_of_pid(escapee), None, "precondition: not a vendor");

            let report = kill_tree(&h.supervisor, guard.spawn_id(), fast_opts().hup_first(true)).await;
            std::thread::sleep(Duration::from_millis(300));
            let survived = !is_gone(escapee);
            let start = start_time_of(escapee).expect("start");
            let view = registry_view(&h.supervisor).await;
            let p = ProcInfo {
                key: gilde_code_lib::contract::ids::ProcessKey::new(escapee as u32, start as u64),
                pid: escapee,
                ppid: 1,
                uid: 501,
                start_sec: start,
                exec_path: Some(fake_agent().to_string_lossy().into_owned()),
                argv_redacted: Vec::new(),
                labels: TeammateLabels::default(),
                cwd: None,
                rss_bytes: 1,
                cpu_ns: 0,
                cco_team_member: None,
                gilde_app_instance: None,
                vendor: None,
            };
            let records = LeftoverTracker::default().observe(&ProcSnapshot { taken_at: 1, procs: vec![p] }, &view);
            unsafe {
                libc::kill(escapee, libc::SIGKILL);
            }
            kid.kill_and_reap();
            assert!(!report.requested.contains(&escapee), "{report:?}");
            assert!(survived, "the detached server was signalled");
            let rec = records.iter().find(|r| r.pid == escapee).expect("listed by the health scan");
            assert_eq!(rec.leftover.as_ref().map(|l| l.reason), Some(LeftoverReason::ToolServerOfEndedSession));
            assert_eq!(rec.session_uid.as_ref().map(|u| u.as_str()), Some(owner), "with the session it came from");
        });
    }
}

rusty_fork_test! {
    /// G7c-06, the reviewer's test: `SignalPlan::Term` on a watched session
    /// (no Gilde row) never SIGKILLs a helper the agent started elsewhere.
    #[test]
    fn g7c_06_stopping_a_watched_session_sends_sigterm_only_and_never_sigkills_its_helpers() {
        block_on(async {
            use gilde_code_lib::contract::ids::ProcessKey;
            use gilde_code_lib::contract::services::SignalPlan;
            let h = harness().await;
            let bin = tempfile::tempdir().expect("bin");
            let claude = bin.path().join("claude");
            std::fs::copy(fake_agent(), &claude).expect("copy");
            let log = bin.path().join("helper.log");
            // The helper logs every signal it gets and ignores TERM.
            let helper = format!(
                "/bin/sh -c \"trap 'echo TERM >> {l}' TERM; trap 'echo HUP >> {l}' HUP; while :; do sleep 0.1; done\"",
                l = log.display()
            );
            let script = format!("{helper} >/dev/null & exec {c} hang", c = claude.display());
            let mut session = Command::new("/bin/sh")
                .arg("-c")
                .arg(&script)
                .env_remove("GILDE_SPAWN_ID")
                .env_remove("GILDE_APP_INSTANCE")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0)
                .spawn()
                .expect("spawn");
            let pid = session.id() as i32;
            let deadline = Instant::now() + SETTLE;
            while gilde_code_lib::supervise::procscan::vendor_of_pid(pid) != Some("claude") {
                assert!(Instant::now() < deadline, "the session never became the vendor CLI");
                std::thread::sleep(Duration::from_millis(20));
            }
            let kid = loop {
                if let Some(k) = snapshot().descendants_of(pid).into_iter().next() {
                    break k;
                }
                assert!(Instant::now() < deadline, "the helper never ran under it");
                std::thread::sleep(Duration::from_millis(20));
            };
            std::thread::sleep(AGE_SETTLE);
            let start = start_time_of(pid).expect("start") as u64;
            let out = h
                .supervisor
                .signal_key(&ProcessKey::new(pid as u32, start), SignalPlan::Term { grace_ms: 700 })
                .await
                .expect("signal");
            std::thread::sleep(Duration::from_millis(400));
            let helper_alive = !is_gone(kid);
            let received = std::fs::read_to_string(&log).unwrap_or_default();
            // Cleanup of what this test started.
            for p in snapshot().descendants_of(kid).into_iter().chain([kid]) {
                unsafe {
                    libc::kill(p, libc::SIGKILL);
                }
            }
            let _ = session.kill();
            let _ = session.wait();
            assert!(helper_alive, "SignalPlan::Term SIGKILLed a helper the agent started elsewhere; outcome={out:?}");
            assert!(received.is_empty(), "A42.1: a pid with no Gilde row is signalled alone; the helper got {received:?}");
            assert!(out.exited && !out.survivor, "the session exited on its TERM: {out:?}");
        });
    }
}

rusty_fork_test! {
    /// A42.1: the outcome is honest — a row-less process that exits on TERM
    /// is `exited`; one that ignores TERM is a `survivor`, alive and never
    /// SIGKILLed (no escalation, vendor or not).
    #[test]
    fn a42_a_signal_plan_reports_a_survivor_and_never_escalates() {
        block_on(async {
            use gilde_code_lib::contract::ids::ProcessKey;
            use gilde_code_lib::contract::services::SignalPlan;
            let h = harness().await;
            for mode in ["hang", "ignore-term"] {
                let plan = SignalPlan::Term { grace_ms: 700 };
                let argv = argv_of(&[&fake_agent().to_string_lossy(), mode]);
                let mut kid = spawn_child(&argv, None);
                let pid = kid.pid();
                std::thread::sleep(AGE_SETTLE);
                let start = start_time_of(pid).expect("start") as u64;
                let out = h.supervisor.signal_key(&ProcessKey::new(pid as u32, start), plan).await.expect("signal");
                std::thread::sleep(Duration::from_millis(500));
                let alive = !is_gone(pid);
                kid.kill_and_reap();
                if mode == "hang" {
                    assert!(out.exited && !out.survivor && !alive, "{mode} {plan:?}: {out:?}");
                } else {
                    assert!(out.survivor && !out.exited, "{mode} {plan:?}: never a clean stop that did not happen: {out:?}");
                    assert!(alive, "{mode} {plan:?}: a SignalPlan escalated to SIGKILL");
                }
            }
        });
    }
}
