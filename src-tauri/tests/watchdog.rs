//! The memory watchdog and the Process Monitor's kill gate against **real
//! processes** — DESIGN §8-7 / §8-8, M1-PLAN T12 acceptance 2, 3, 7, 8, 9.
//!
//! The unit tests in `supervise::watchdog` prove the pure rules (hysteresis, the
//! once-an-hour limit, the idle clock, the bulk refusals) against injected
//! samples. These prove the three things only a process can prove:
//!
//!   * a session that really is holding hundreds of megabytes crosses the soft
//!     threshold and produces **exactly one** toast, not one per tick;
//!   * an armed automatic termination really does end the process when its undo
//!     window closes — and really does not when the undo arrives in time;
//!   * the bulk gate and `dryRun` keep their promise: no signal reaches a
//!     process the rules refuse, and none reaches anything at all in a dry run.
//!
//! Every test runs under `rusty-fork`, in its own process: the reaper's undo
//! table is process-global, and a bug in the code under test must never be able
//! to reach the `cargo test` runner, which is by construction our ancestor.

mod fixtures;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gilde_code_lib::config::ConfigStore;
use gilde_code_lib::events::{Events, RecordingEvents};
use gilde_code_lib::store::models::{ProcKind, PtySessionRow};
use gilde_code_lib::store::{dao, Db};
use gilde_code_lib::supervise::kill::{is_gone, KillOpts, KillTarget};
use gilde_code_lib::supervise::procscan::ps_snapshot;
use gilde_code_lib::supervise::reaper;
use gilde_code_lib::supervise::registry::Supervisor;
use gilde_code_lib::supervise::spawn::is_alive;
use gilde_code_lib::supervise::watchdog::Watchdog;
use gilde_code_lib::supervise::{Alarm, ProcGroupKind, ProcTag, ScanFilter};

use rusty_fork::rusty_fork_test;
use serde_json::json;
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
const SETTLE: Duration = Duration::from_secs(20);

/// How much a session allocates in the soft-threshold test.
///
/// M1-PLAN T12 acceptance 9 names `fake_agent alloc 3000 20`. Three gigabytes
/// resident is a real risk on a machine that runs several of these builds at
/// once, and the property under test — one crossing, one toast — does not depend
/// on the number. The default is scaled down and the literal figure stays one
/// environment variable away: `GILDE_TEST_ALLOC_MIB=3000 cargo test --test watchdog`.
/// See docs/PINNED-VERSIONS.md "T12".
fn alloc_mib() -> u64 {
    std::env::var("GILDE_TEST_ALLOC_MIB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300)
}

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(f)
}

struct Harness {
    sup: Arc<Supervisor>,
    config: Arc<ConfigStore>,
    sink: Arc<RecordingEvents>,
    bus: Events,
    db: Db,
    dir: tempfile::TempDir,
}

async fn harness(supervision: serde_json::Value) -> Harness {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::open_in_memory().expect("db");
    let config =
        Arc::new(ConfigStore::open_at(&dir.path().join("config.json")).expect("config store"));
    config
        .patch(json!({ "supervision": supervision }))
        .expect("patch settings");
    let sink = RecordingEvents::new();
    let bus = Events::new(sink.clone());
    let sup = Arc::new(
        Supervisor::open_instance(&db, config.clone(), "0.0.0-test")
            .await
            .expect("open_instance")
            .with_events(bus.clone()),
    );
    Harness {
        sup,
        config,
        sink,
        bus,
        db,
        dir,
    }
}

impl Harness {
    fn watchdog(&self) -> Watchdog {
        Watchdog::new(self.sup.clone(), self.bus.clone())
    }

    fn events(&self, name: &str) -> Vec<serde_json::Value> {
        self.sink
            .events()
            .into_iter()
            .filter(|(n, _)| n == name)
            .map(|(_, v)| v)
            .collect()
    }

    /// Every `gilde://budget/alert` of one level.
    fn alerts(&self, level: &str) -> Vec<serde_json::Value> {
        self.events("gilde://budget/alert")
            .into_iter()
            .filter(|v| v["level"] == json!(level))
            .collect()
    }

    /// Every `gilde://toast` with one key.
    fn toasts(&self, key: &str) -> Vec<serde_json::Value> {
        self.events("gilde://toast")
            .into_iter()
            .filter(|v| v["key"] == json!(key))
            .collect()
    }

    /// Make `spawn_id` look like a live PTY session the watchdog should watch.
    async fn open_session(&self, session_id: &str, spawn_id: &str, last_io_at: Option<i64>) {
        let row = PtySessionRow {
            id: session_id.to_string(),
            spawn_id: Some(spawn_id.to_string()),
            project_id: None,
            worktree_id: None,
            agent_kind: "claude".into(),
            account_id: None,
            title: session_id.to_string(),
            cwd: self.dir.path().display().to_string(),
            cols: 80,
            rows: 24,
            argv: "[]".into(),
            opened_at: gilde_code_lib::store::now_ms(),
            closed_at: None,
            exit_code: None,
            exit_signal: None,
            last_io_at,
            last_status: None,
            detached: false,
        };
        self.db
            .write(move |conn| dao::pty_session::insert(conn, &row))
            .await
            .expect("insert pty_session");
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
}

impl Drop for Kid {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.child.kill();
            let _ = self.child.wait();
            self.reaped = true;
        }
    }
}

fn fake_agent() -> PathBuf {
    fixtures::fake_agent_path()
}

fn argv_of(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|p| (*p).to_string()).collect()
}

fn spawn_child(argv: &[String], cwd: &Path, marker: &str) -> Kid {
    let child = Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(cwd)
        .env("GILDE_SPAWN_ID", marker)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {argv:?}: {e}"));
    let kid = Kid {
        child,
        reaped: false,
    };
    wait_until_visible(kid.pid());
    kid
}

/// The §9.3 allowlist admits only resolved binaries: a fixture is
/// allowlisted for the kind it is planned as before it is planned (A12.8).
fn allow(argv: &[String], kind: ProcKind) {
    gilde_code_lib::supervise::spawn::allow_executable(std::path::Path::new(&argv[0]), &[kind]);
}

/// Plan a row, start the process it describes, mark it running — DESIGN §8-3.
async fn register(
    sup: &Supervisor,
    argv: &[String],
    cwd: &Path,
    session_id: Option<&str>,
) -> (String, Kid) {
    allow(argv, ProcKind::Pty);
    let guard = sup
        .plan_guarded(fixture_request(ProcKind::Pty, argv, cwd, None, None))
        .await
        .expect("plan_spawn");
    // What `SupervisorApi` cannot express, the fixture sets on the row: the
    // PTY session the watchdog attributes by, and the vendor the fixture
    // stands in for.
    let id = guard.spawn_id().to_string();
    let session = session_id.map(str::to_string);
    sup.db()
        .write(move |conn| {
            conn.execute(
                "UPDATE proc_registry SET vendor = 'claude', session_id = ?2 WHERE spawn_id = ?1",
                rusqlite::params![id, session],
            )?;
            Ok(())
        })
        .await
        .expect("fixture columns");
    let kid = spawn_child(argv, cwd, guard.env_marker());
    let pid = kid.pid();
    guard.mark_running(pid, pid).expect("mark_running");
    let spawn_id = guard.spawn_id().to_string();
    drop(guard);
    (spawn_id, kid)
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

/// Wait until `ps` agrees the process really is holding `mib` MiB.
fn wait_until_resident(pid: i32, mib: u64) {
    let want_kb = mib * 1024 * 9 / 10;
    let deadline = Instant::now() + SETTLE;
    loop {
        let rss_kb = ps_snapshot()
            .ok()
            .and_then(|s| s.get(pid).map(|r| r.rss_kb))
            .unwrap_or(0);
        if rss_kb >= want_kb {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "pid {pid} never reached {want_kb} KiB (last {rss_kb} KiB)"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Wait until the registry row is closed. The kill itself is asynchronous: the
/// process dies first and the row is written a moment later.
async fn wait_until_row_terminal(db: &Db, spawn_id: &str) {
    let deadline = Instant::now() + SETTLE;
    loop {
        let id = spawn_id.to_string();
        let row = db
            .read(move |conn| dao::proc::get(conn, &id))
            .await
            .expect("read row")
            .expect("row exists");
        if row.state.is_terminal() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{spawn_id} is still {:?} long after its process ended",
            row.state
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn wait_until_gone(pid: i32) {
    let deadline = Instant::now() + SETTLE;
    while !is_gone(pid) {
        assert!(Instant::now() < deadline, "pid {pid} never exited");
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ===========================================================================
// 1. one crossing is one toast  (acceptance 1, 2, 9)
// ===========================================================================

rusty_fork_test! {
    /// G5 benchmark (A10.7): the same rule against a real allocation and the
    /// kernel's RSS, which needs wall-clock waits (the RSS settle, ticks until
    /// the compressor lets the crossing happen). The functional version is
    /// [`a_session_over_the_soft_budget_alerts_exactly_once`] below.
    #[test]
    #[ignore = "G5 benchmark (A10.7): real RSS of a real allocation, timing-dependent"]
    fn watchdog_bench_a_session_over_the_soft_budget_alerts_exactly_once() {
        block_on(async {
            let mib = alloc_mib();
            // Two thirds of what the session will allocate: crossed on the very
            // first tick, and far enough above that no rounding decides it.
            let soft = (mib * 2 / 3).max(1);
            let h = harness(json!({
                "softRssMiB": soft,
                "hardRssMiB": mib * 100,
                "idleTimeoutMinutes": 0,
                "autoKillOverHardCeiling": false,
                // Whatever else this machine is running must not decide the
                // outcome: the global banner is not what is under test here.
                "globalWarnPct": 100,
                "globalCriticalPct": 100,
            }))
            .await;

            let cwd = h.dir.path().to_path_buf();
            let argv = argv_of(&[
                &fake_agent().to_string_lossy(),
                "alloc",
                &mib.to_string(),
                "120",
            ]);
            let (spawn_id, kid) = register(&h.sup, &argv, &cwd, Some("sess-1")).await;
            h.open_session("sess-1", &spawn_id, Some(gilde_code_lib::store::now_ms()))
                .await;
            wait_until_resident(kid.pid(), mib);

            // A4 (load verdict: TEST timing, plus one real product hole that this
            // in-memory harness cannot reach — see the report / watchdog.rs).
            // Under heavy memory pressure the compressor can hold the session's
            // subtree below `soft` for the first ticks (macOS compresses the
            // fixture's pages between its 100 ms re-touches), and a `ps` that
            // exceeds its 5 s timeout makes a tick skip without scanning. Neither
            // says anything about "one crossing, one alarm". So: tick until the
            // crossing has happened (bounded), then twelve more scanned ticks
            // past it, and judge every tick — skipped ones included.
            let watchdog = h.watchdog();
            let mut soft_alarms = 0;
            let mut scanned = 0usize;
            let mut skipped = 0usize;
            let mut after_crossing = 0usize;
            let mut ticks = 0usize;
            const MAX_TICKS: usize = 120;
            while after_crossing < 12 {
                assert!(
                    ticks < MAX_TICKS,
                    "no soft crossing in {MAX_TICKS} ticks ({scanned} scanned, {skipped} skipped): \
                     the machine never let the session hold {soft} MiB resident"
                );
                ticks += 1;
                let report = watchdog.tick().await;
                assert!(
                    report.undo_tokens.is_empty(),
                    "a soft crossing must never arm a termination"
                );
                if report.rows == 0 {
                    // `ps` gave no snapshot this tick: it must have judged nothing.
                    assert!(report.alarms.is_empty(), "a tick that could not scan alarmed: {report:?}");
                    skipped += 1;
                    continue;
                }
                scanned += 1;
                assert_eq!(report.sessions, 1, "the session is attributed every scanned tick");
                soft_alarms += report
                    .alarms
                    .iter()
                    .filter(|a| matches!(a, Alarm::Soft { .. }))
                    .count();
                if soft_alarms > 0 {
                    after_crossing += 1;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            println!("soft budget: {ticks} ticks, {scanned} scanned, {skipped} skipped");

            assert_eq!(soft_alarms, 1, "{scanned} scanned ticks, one crossing, one alarm");
            assert_eq!(h.alerts("soft").len(), 1, "one gilde://budget/alert");
            assert_eq!(h.toasts("toast.budget.soft").len(), 1, "one toast");
            assert!(h.alerts("hard").is_empty(), "nothing else fired");

            let alert = &h.alerts("soft")[0];
            assert_eq!(alert["sessionId"], json!("sess-1"));
            assert!(alert["undoToken"].is_null(), "a soft alert is a warning");
            assert!(
                alert["rssBytes"].as_u64().unwrap_or(0) >= soft * 1024 * 1024,
                "the alert carries the measured RSS: {alert}"
            );

            // acceptance 1: every tick publishes `gilde://proc/scan`.
            let scans = h.events("gilde://proc/scan");
            assert_eq!(scans.len(), scanned, "one scan event per scanned tick");
            assert!(scans[0]["rows"].as_array().is_some_and(|r| !r.is_empty()));
            assert!(scans[0]["totals"]["physicalRamBytes"].as_u64().unwrap_or(0) > 0);

            // acceptance 2: the session's subtree RSS reached `proc_sample` (the
            // graph's metric lane went with the graph projection, A22.3).
            let samples = h
                .db
                .read(|conn| dao::proc_sample::range(conn, "session", Some("sess-1"), 0, i64::MAX))
                .await
                .expect("read proc_sample");
            assert_eq!(samples.len(), scanned, "one row per scanned tick");
            assert!(
                samples.iter().all(|s| s.rss_bytes > 0),
                "every tick attributed the session's subtree"
            );
            // `any`, not `all`: macOS compresses an idle allocation, so a later
            // tick legitimately measures less than the crossing one did.
            assert!(
                samples
                    .iter()
                    .any(|s| s.rss_bytes >= (soft * 1024 * 1024) as i64),
                "the sample is the subtree, not a single process"
            );
        });
    }
}

/// A10.7 functional version: the watchdog reads the shared scan (A12.2),
/// so the session's subtree RSS is whatever that scan says — deterministic,
/// no allocation, no RSS wait. Twelve scans over the soft budget: one alarm,
/// one alert, one toast, a scan event and a `proc_sample` row per tick.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_over_the_soft_budget_alerts_exactly_once() {
    use gilde_code_lib::contract::ids::ProcessKey;
    use gilde_code_lib::contract::services::{ProcInfo, ProcSnapshot};
    use gilde_code_lib::supervise::procscan::{start_time_of, ScanHub};

    const MIB: u64 = 1024 * 1024;
    let soft: u64 = 64;
    let h = harness(json!({
        "softRssMiB": soft,
        "hardRssMiB": 100_000,
        "idleTimeoutMinutes": 0,
        "autoKillOverHardCeiling": false,
        "globalWarnPct": 100,
        "globalCriticalPct": 100,
    }))
    .await;
    let cwd = h.dir.path().to_path_buf();
    let argv = argv_of(&[&fake_agent().to_string_lossy(), "hang"]);
    let (spawn_id, kid) = register(&h.sup, &argv, &cwd, Some("sess-1")).await;
    h.open_session("sess-1", &spawn_id, Some(gilde_code_lib::store::now_ms())).await;
    let pid = kid.pid();
    let start = start_time_of(pid).expect("the fixture's start time");
    let hub = ScanHub::new();
    let watchdog = h.watchdog().with_scan_hub(hub.clone());
    // SAFETY: `getuid` has no failure mode.
    let uid = unsafe { libc::getuid() };
    let scan = |rss_bytes: u64| ProcSnapshot {
        taken_at: gilde_code_lib::store::now_ms(),
        procs: vec![ProcInfo {
            key: ProcessKey::new(pid as u32, start as u64),
            pid,
            ppid: std::process::id() as i32,
            uid,
            start_sec: start,
            exec_path: Some(argv[0].clone()),
            argv_redacted: argv.clone(),
            labels: Default::default(),
            cwd: None,
            rss_bytes,
            cpu_ns: 0,
            cco_team_member: None,
            gilde_app_instance: None,
            vendor: None,
        }],
    };

    let mut soft_alarms = 0;
    for _ in 0..12 {
        hub.publish(scan(soft * 2 * MIB));
        let report = watchdog.tick().await;
        assert_eq!(report.sessions, 1, "the session is attributed every tick");
        assert!(report.undo_tokens.is_empty(), "a soft crossing never arms a termination");
        soft_alarms += report.alarms.iter().filter(|a| matches!(a, Alarm::Soft { .. })).count();
    }
    assert_eq!(soft_alarms, 1, "twelve ticks over the budget, one crossing, one alarm");
    assert_eq!(h.alerts("soft").len(), 1, "one gilde://budget/alert");
    assert_eq!(h.toasts("toast.budget.soft").len(), 1, "one toast");
    assert!(h.alerts("hard").is_empty(), "nothing else fired");
    let alert = &h.alerts("soft")[0];
    assert_eq!(alert["sessionId"], json!("sess-1"));
    assert!(alert["undoToken"].is_null(), "a soft alert is a warning");
    assert_eq!(alert["rssBytes"].as_u64(), Some(soft * 2 * MIB), "the alert carries the shared scan's RSS");
    assert_eq!(h.events("gilde://proc/scan").len(), 12, "one scan event per tick");
    let samples = h
        .db
        .read(|conn| dao::proc_sample::range(conn, "session", Some("sess-1"), 0, i64::MAX))
        .await
        .expect("read proc_sample");
    assert_eq!(samples.len(), 12, "one row per tick");
    assert!(samples.iter().all(|s| s.rss_bytes == (soft * 2 * MIB) as i64));
}

// ===========================================================================
// 2. the undo window really is a window  (acceptance 3)
// ===========================================================================

rusty_fork_test! {
    #[test]
    fn an_armed_termination_fires_unless_the_undo_arrives_in_time() {
        block_on(async {
            // Hard ceiling of 1 MiB with the auto-kill explicitly turned on:
            // the default is off, and a test that needed the default to be on
            // would be testing the wrong thing.
            let h = harness(json!({
                "softRssMiB": 1,
                "hardRssMiB": 1,
                "autoKillOverHardCeiling": true,
                "killGraceMs": 700,
                "globalWarnPct": 100,
                "globalCriticalPct": 100,
            }))
            .await;
            let cwd = h.dir.path().to_path_buf();
            let argv = argv_of(&[&fake_agent().to_string_lossy(), "alloc", "64", "120"]);

            let (kept_id, kept) = register(&h.sup, &argv, &cwd, Some("sess-keep")).await;
            let (doomed_id, doomed) = register(&h.sup, &argv, &cwd, Some("sess-kill")).await;
            h.open_session("sess-keep", &kept_id, Some(0)).await;
            h.open_session("sess-kill", &doomed_id, Some(0)).await;
            wait_until_resident(kept.pid(), 64);
            wait_until_resident(doomed.pid(), 64);

            let report = h.watchdog().tick().await;
            let hard: Vec<&Alarm> = report
                .alarms
                .iter()
                .filter(|a| matches!(a, Alarm::Hard { .. }))
                .collect();
            assert_eq!(hard.len(), 2, "both sessions crossed: {:?}", report.alarms);
            assert_eq!(report.undo_tokens.len(), 2, "each one armed a termination");

            // The alerts carry the tokens, which is what makes the modal's undo
            // button able to do anything at all.
            let alerts = h.alerts("hard");
            assert_eq!(alerts.len(), 2);
            assert!(alerts.iter().all(|a| a["undoToken"].as_str().is_some()));

            // Cancel the one whose session id sorts first in the alarm order.
            let keep_index = report
                .alarms
                .iter()
                .filter(|a| matches!(a, Alarm::Hard { .. }))
                .position(|a| a.session_id() == Some("sess-keep"))
                .expect("the kept session raised an alarm");
            let keep_token = report.undo_tokens[keep_index].clone();
            assert!(reaper::undo_is_pending(&keep_token));
            assert!(reaper::undo_cancel(&keep_token), "the undo is in time");
            assert!(!reaper::undo_cancel(&keep_token), "a token is spent once");

            // The window is ten seconds; give the escalation room after it.
            wait_until_gone(doomed.pid());
            assert!(
                is_alive(kept.pid()),
                "the cancelled termination must never have been sent"
            );

            wait_until_row_terminal(&h.db, &doomed_id).await;
        });
    }
}

// (3. "the bulk gate" was a Process Monitor policy of `procmon_kill`,
// retired with `commands::monitor` — contract §2.2.1; the Clean-up sheet's
// `health_stop` replaces it.)

// ===========================================================================
// 4. the fence is re-run per row, immediately before the signal  (acceptance 7)
// ===========================================================================

rusty_fork_test! {
    #[test]
    fn every_row_is_fenced_again_before_its_signal() {
        block_on(async {
            let h = harness(json!({ "killGraceMs": 700 })).await;
            let cwd = h.dir.path().to_path_buf();
            let argv = argv_of(&[&fake_agent().to_string_lossy(), "hang"]);
            let (gone_id, mut gone) = register(&h.sup, &argv, &cwd, None).await;
            let (live_id, live) = register(&h.sup, &argv, &cwd, None).await;

            // The scan the user clicked on is now stale: this row's process died
            // between the click and the signal.
            let dead_pid = gone.pid();
            let _ = gone.child.kill();
            let _ = gone.child.wait();
            gone.reaped = true;
            wait_until_gone(dead_pid);

            let targets = vec![KillTarget::spawn(&gone_id), KillTarget::spawn(&live_id)];
            let outcomes = gilde_code_lib::supervise::kill::kill_pids(
                &h.sup,
                &targets,
                KillOpts::default().grace(Duration::from_millis(700)),
            )
            .await;

            assert_eq!(outcomes.len(), 2);
            assert_eq!(
                outcomes[0].report.refused_reason.as_deref(),
                Some("gone"),
                "the fence caught the dead row instead of signalling its pid"
            );
            assert_eq!(outcomes[0].report.fence, "gone");
            assert!(
                !outcomes[1].report.is_refused(),
                "one refusal must not stop the rest: {:?}",
                outcomes[1].report
            );
            wait_until_gone(live.pid());

            // The refused row was closed rather than left claiming a live pid.
            wait_until_row_terminal(&h.db, &gone_id).await;
        });
    }
}

// ===========================================================================
// 5. idle uses last_io_at, and the default leaves it off  (acceptance 4)
// ===========================================================================

rusty_fork_test! {
    #[test]
    fn an_idle_session_is_terminated_only_when_the_timeout_is_configured() {
        block_on(async {
            let h = harness(json!({
                "idleTimeoutMinutes": 0,
                "softRssMiB": 1000000,
                "hardRssMiB": 1000000,
                "killGraceMs": 700,
                "globalWarnPct": 100,
                "globalCriticalPct": 100,
            }))
            .await;
            let cwd = h.dir.path().to_path_buf();
            let argv = argv_of(&[&fake_agent().to_string_lossy(), "hang"]);
            let (spawn_id, kid) = register(&h.sup, &argv, &cwd, Some("sess-1")).await;
            // Last I/O two hours ago.
            let two_hours_ago = gilde_code_lib::store::now_ms() - 2 * 3600 * 1000;
            h.open_session("sess-1", &spawn_id, Some(two_hours_ago)).await;

            let watchdog = h.watchdog();
            let report = watchdog.tick().await;
            assert!(
                report.alarms.is_empty(),
                "DECISIONS: idle auto-termination ships off — {:?}",
                report.alarms
            );
            assert!(is_alive(kid.pid()));

            // Turn it on and the same session is proposed for termination.
            h.config
                .patch(json!({ "supervision": { "idleTimeoutMinutes": 30 } }))
                .expect("patch settings");
            let report = watchdog.tick().await;
            assert_eq!(report.alarms.len(), 1, "{:?}", report.alarms);
            assert!(matches!(report.alarms[0], Alarm::Idle { .. }));
            assert_eq!(report.undo_tokens.len(), 1, "behind a 10 s undo window");
            assert_eq!(h.toasts("toast.budget.idle").len(), 1);

            // …and it is proposed once, not once per tick.
            let again = watchdog.tick().await;
            assert!(again.alarms.is_empty(), "{:?}", again.alarms);

            wait_until_gone(kid.pid());
        });
    }
}

// ===========================================================================
// 6. pressure is reported, and never a reason to kill  (acceptance 6)
// ===========================================================================

rusty_fork_test! {
    #[test]
    fn the_budget_reports_the_pressure_source_it_actually_used() {
        block_on(async {
            let h = harness(json!({})).await;
            let watchdog = h.watchdog();
            let report = watchdog.tick().await;
            assert!(report.rows > 0);

            let budget = watchdog.budget();
            assert_eq!(
                serde_json::to_value(budget.pressure_source).expect("json"),
                json!("kern.memorystatus_vm_pressure_level"),
                "the primary probe answers on this machine"
            );
            assert!(budget.physical_ram_bytes > 0);
            assert_eq!(budget.warn_pct, 70);
            assert_eq!(budget.critical_pct, 85);
            assert!(
                report.undo_tokens.is_empty(),
                "pressure never arms a termination"
            );
        });
    }
}

// ===========================================================================
// 7. the scan classifies our own rows, and the cache is the cache  (acceptance 1)
// ===========================================================================

rusty_fork_test! {
    #[test]
    fn a_scan_classifies_our_own_process_and_serves_the_cache_until_asked_not_to() {
        block_on(async {
            let h = harness(json!({ "scanIntervalSecondsActive": 3 })).await;
            let cwd = h.dir.path().to_path_buf();
            let argv = argv_of(&[&fake_agent().to_string_lossy(), "hang"]);
            let (spawn_id, kid) = register(&h.sup, &argv, &cwd, Some("sess-1")).await;
            h.open_session("sess-1", &spawn_id, Some(gilde_code_lib::store::now_ms()))
                .await;

            let watchdog = h.watchdog();
            let scan = watchdog
                .scan(&ScanFilter::default(), true)
                .await
                .expect("scan");
            let row = scan
                .rows
                .iter()
                .find(|r| r.pid == kid.pid())
                .expect("our own process is in the scan");
            assert_eq!(row.group, ProcGroupKind::OursLive, "{row:?}");
            assert_eq!(row.spawn_id.as_deref(), Some(spawn_id.as_str()));
            assert!(row.has_tag(ProcTag::Ours));
            assert!(!row.has_tag(ProcTag::Detached));
            assert!(row.cwd.is_none(), "cwd stays unresolved unless asked for");
            assert!(scan.totals.physical_ram_bytes > 0);

            // `keepAlive` is what puts the Detached marker on the row.
            let id = spawn_id.clone();
            assert!(h
                .db
                .write(move |conn| dao::proc::set_keep_alive(conn, &id, true))
                .await
                .expect("set_keep_alive"));

            let forced = watchdog
                .scan(&ScanFilter::default(), true)
                .await
                .expect("scan");
            assert!(forced
                .rows
                .iter()
                .find(|r| r.pid == kid.pid())
                .expect("row")
                .has_tag(ProcTag::Detached));
            assert!(forced.at >= scan.at, "a forced scan is a new scan");

            // Two polls inside the active interval share one `ps`.
            let a = watchdog
                .scan(&ScanFilter::default(), false)
                .await
                .expect("scan");
            let b = watchdog
                .scan(&ScanFilter::default(), false)
                .await
                .expect("scan");
            assert_eq!(a.at, b.at, "the second poll was served from the cache");
            assert_eq!(a.at, forced.at);

            // A narrowing filter keeps the machine-wide footer.
            let narrowed = watchdog
                .scan(
                    &ScanFilter {
                        query: Some(spawn_id.clone()),
                        ..Default::default()
                    },
                    false,
                )
                .await
                .expect("scan");
            assert!(narrowed.rows.len() < a.rows.len());
            assert_eq!(narrowed.totals.processes, a.totals.processes);
        });
    }
}
