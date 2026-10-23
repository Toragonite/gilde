//! The quit of contract §10.2 and the crash record of §10.1/§10.3 (T-quit-*).
//!
//! Owner: T09. Real child processes (the `fake_agent` fixture; a copy of it
//! named `claude` stands in for a vendor CLI, so the never-SIGKILL policy
//! classifies it as one) and a fake sessions manager. No real vendor CLI runs.

mod fixtures;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use gilde_code_lib::config::ConfigStore;
use gilde_code_lib::contract::common::PermissionMode;
use gilde_code_lib::contract::ids::{AccountId, QuitRecordId, SessionUid};
use gilde_code_lib::contract::ipc::*;
use gilde_code_lib::contract::services::{SessionActorApi, StopOutcome};
use gilde_code_lib::contract::IpcError;
use gilde_code_lib::events::NoStoreOps;
use gilde_code_lib::lifecycle::boot::{self, SetupOutcome};
use gilde_code_lib::lifecycle::quit::{self, QuitCoordinator, NOTE_QUIT_SURVIVOR};
use gilde_code_lib::paths::AppPaths;
use gilde_code_lib::state::Services;
use gilde_code_lib::store::models::{ProcKind, ProcState, CAUSE_CRASH};
use gilde_code_lib::store::{dao, Db};
use gilde_code_lib::supervise::kill::is_gone;
use gilde_code_lib::supervise::registry::{SpawnGuard, Supervisor};

const U1: &str = "s_01890a5d-ac96-774b-bcce-b302099a8057";
const U2: &str = "s_01890a5d-ac96-774b-bcce-b302099a8058";
const U3: &str = "s_01890a5d-ac96-774b-bcce-b302099a8059";

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

/// A child the test started; always killed and reaped (the test may signal
/// what it started, contract brief).
struct Kid(Child);

impl Kid {
    fn pid(&self) -> i32 {
        self.0.id() as i32
    }
}

impl Drop for Kid {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A copy of the fixture named `claude` (a copy, so the kernel's exec path
/// names a vendor CLI).
fn vendor_copy(dir: &Path) -> PathBuf {
    let dest = dir.join("claude");
    std::fs::copy(fixtures::fake_agent_path(), &dest).expect("copy fixture");
    dest
}

fn spawn(argv: &[String], marker: &str) -> Kid {
    use std::os::unix::process::CommandExt as _;
    let child = Command::new(&argv[0])
        .args(&argv[1..])
        .env("GILDE_SPAWN_ID", marker)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .expect("spawn");
    // Let the fixture install its signal disposition.
    std::thread::sleep(Duration::from_millis(400));
    Kid(child)
}


/// The §9.3 allowlist admits only resolved binaries: a fixture is
/// allowlisted for the kind it is planned as before it is planned (A12.8).
fn allow(argv: &[String], kind: ProcKind) {
    gilde_code_lib::supervise::spawn::allow_executable(std::path::Path::new(&argv[0]), &[kind]);
}

/// A fixture's spawn request on the one spawn path (`SupervisorApi`'s
/// checks, A12.8/A22.3); the guard stays with the test.
fn request(kind: ProcKind, argv: &[String], uid: &str) -> gilde_code_lib::contract::services::SpawnRequest {
    use gilde_code_lib::contract::process::ProcKind as C;
    let kind = match kind {
        ProcKind::Convo => C::Convo,
        ProcKind::Pty => C::Pty,
        ProcKind::Pane => C::Pane,
        other => panic!("no fixture for {other:?}"),
    };
    gilde_code_lib::contract::services::SpawnRequest {
        kind,
        argv0: gilde_code_lib::contract::common::AbsPath::parse(argv[0].clone()).expect("absolute argv[0]"),
        args: argv[1..].to_vec(),
        cwd: gilde_code_lib::contract::common::AbsPath::parse("/tmp").ok(),
        env_keys: Vec::new(),
        parent_uid: Some(SessionUid::parse(uid).expect("uid")),
        generation: None,
        turn_id: None,
    }
}

async fn register(sup: &Supervisor, kind: ProcKind, argv: &[String], uid: &str) -> (SpawnGuard, Kid) {
    allow(argv, kind);
    let guard = sup.plan_guarded(request(kind, argv, uid)).await.expect("plan");
    let kid = spawn(argv, guard.env_marker());
    guard.mark_running(kid.pid(), kid.pid()).expect("running");
    (guard, kid)
}

async fn supervisor(dir: &Path) -> (Arc<Supervisor>, Db) {
    let db = Db::open(&dir.join("state.db")).expect("db");
    let config = Arc::new(ConfigStore::open_at(&dir.join("config.json")).expect("config"));
    let sup = Arc::new(Supervisor::open_instance(&db, config, "0.0.0-test").await.expect("instance"));
    (sup, db)
}

/// The sessions manager as the quit sees it: `stop_for_quit` per uid.
#[derive(Default)]
struct FakeSessions {
    db: Option<Db>,
    /// uid → pid to SIGTERM when asked to stop (the "driver").
    stop_pids: HashMap<String, i32>,
    /// uids whose stop never returns.
    hang: Vec<String>,
    record_seen_before_stop: AtomicBool,
    stop_called: AtomicBool,
    continued: parking_lot::Mutex<Vec<String>>,
}

#[async_trait]
impl SessionActorApi for FakeSessions {
    async fn options(&self, _: &NewsessionOptionsReq) -> Result<NewSessionOptions, IpcError> {
        Err(IpcError::not_built())
    }
    async fn start(&self, _: SessionStartReq) -> Result<SessionStartRes, IpcError> {
        Err(IpcError::not_built())
    }
    async fn continue_session(&self, _: SessionContinueReq) -> Result<SessionContinueRes, IpcError> {
        Err(IpcError::not_built())
    }
    async fn retry_as_terminal(&self, _: SessionRetryAsTerminalReq) -> Result<SessionRetryAsTerminalRes, IpcError> {
        Err(IpcError::not_built())
    }
    async fn stop_preview(&self, _: &SessionUid) -> Result<SessionStopPreviewRes, IpcError> {
        Err(IpcError::not_built())
    }
    async fn stop(&self, _: SessionStopReq) -> Result<SessionStopRes, IpcError> {
        Err(IpcError::not_built())
    }
    async fn send(&self, _: TurnSendReq) -> Result<TurnSendRes, IpcError> {
        Err(IpcError::not_built())
    }
    async fn cancel_queued(&self, _: TurnCancelQueuedReq) -> Result<TurnCancelQueuedRes, IpcError> {
        Err(IpcError::not_built())
    }
    async fn interrupt(&self, _: TurnInterruptReq) -> Result<TurnInterruptRes, IpcError> {
        Err(IpcError::not_built())
    }
    async fn force_stop(&self, _: &SessionUid) -> Result<bool, IpcError> {
        Err(IpcError::not_built())
    }
    async fn retry(&self, _: TurnRetryReq) -> Result<TurnRetryRes, IpcError> {
        Err(IpcError::not_built())
    }
    async fn set_model(&self, _: &SessionUid, _: Option<String>) -> Result<AppliesFrom, IpcError> {
        Err(IpcError::not_built())
    }
    async fn set_permission_mode(&self, _: &SessionUid, _: PermissionMode) -> Result<(), IpcError> {
        Err(IpcError::not_built())
    }
    async fn answer(&self, _: InteractionAnswerReq) -> Result<InteractionAnswerRes, IpcError> {
        Err(IpcError::not_built())
    }
    async fn screen_key(&self, _: ScreenPromptKeyReq) -> Result<ScreenPromptKeyRes, IpcError> {
        Err(IpcError::not_built())
    }
    async fn detail(&self, _: &InteractionDetailReq) -> Result<InteractionDetailRes, IpcError> {
        Err(IpcError::not_built())
    }
    async fn models(&self, _: &SessionUid) -> Result<SessionModelsRes, IpcError> {
        Err(IpcError::not_built())
    }
    async fn technical(&self, _: &SessionUid) -> Result<SessionTechnicalRes, IpcError> {
        Err(IpcError::not_built())
    }
    async fn start_sign_in(&self, _: &AccountId) -> Result<AccountSignInRes, IpcError> {
        Err(IpcError::not_built())
    }
    async fn continue_all(
        &self,
        _record: &QuitRecordId,
        uids: Option<Vec<SessionUid>>,
    ) -> Result<ContinueAllRes, IpcError> {
        let started: Vec<SessionUid> = uids.unwrap_or_else(|| vec![SessionUid::parse(U1).unwrap()]);
        self.continued
            .lock()
            .extend(started.iter().map(|u| u.as_str().to_string()));
        Ok(ContinueAllRes {
            started,
            failed: Vec::new(),
        })
    }
    async fn stop_for_quit(&self, uid: &SessionUid) -> Result<StopOutcome, IpcError> {
        self.stop_called.store(true, Ordering::SeqCst);
        if let Some(db) = &self.db {
            let any = db
                .read(|conn| dao::quit_record::list_open(conn, gilde_code_lib::store::now_ms()))
                .await
                .map(|r| !r.is_empty())
                .unwrap_or(false);
            if any {
                self.record_seen_before_stop.store(true, Ordering::SeqCst);
            }
        }
        if self.hang.iter().any(|h| h == uid.as_str()) {
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
        if let Some(pid) = self.stop_pids.get(uid.as_str()) {
            // SAFETY: a process this test started.
            unsafe { libc::kill(*pid, libc::SIGTERM) };
            // A driver reports `stopped` once its process exited (a Gilde
            // vendor row still alive at the survivor scan is a survivor,
            // A39.1(b)).
            let deadline = Instant::now() + Duration::from_secs(5);
            while !is_gone(*pid) && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        Ok(StopOutcome {
            stopped: true,
            survivors: 0,
        })
    }
}

async fn preview(sup: &Supervisor) -> quit::QuitPreview {
    boot::preview_for(&Services::default(), sup).await
}

// ---------------------------------------------------------------------------
// T-quit-record
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_quit_record_is_written_before_any_signal() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (sup, db) = supervisor(dir.path()).await;
    let argv = vec![fixtures::fake_agent_path().to_string_lossy().into_owned(), "hang".to_string()];
    let (_g, kid) = register(&sup, ProcKind::Convo, &argv, U1).await;

    let p = preview(&sup).await;
    assert_eq!(p.items.len(), 1);
    assert_eq!(p.items[0].uid, U1);
    assert_eq!(p.items[0].control_mode, "conversation");

    let coord = QuitCoordinator::new(sup.clone(), Arc::new(NoStoreOps));
    let token = coord.issue(p);
    // A changed set of sessions is CONFLICT{changed}.
    let mut other = preview(&sup).await;
    other.digest = quit::digest_of(&["something else"]);
    let err = coord.redeem(&token, &other).unwrap_err();
    assert_eq!(err.params["changed"], "sessions");
    // The token is single use.
    let current = preview(&sup).await;
    assert!(coord.redeem(&token, &current).is_err());

    let token = coord.issue(current.clone());
    let redeemed = coord.redeem(&token, &current).expect("redeem");
    let fake = Arc::new(FakeSessions {
        db: Some(db.clone()),
        stop_pids: [(U1.to_string(), kid.pid())].into_iter().collect(),
        ..FakeSessions::default()
    });
    let report = coord.run(redeemed, Some(fake.clone() as Arc<dyn SessionActorApi>)).await;
    assert!(fake.stop_called.load(Ordering::SeqCst));
    assert!(
        fake.record_seen_before_stop.load(Ordering::SeqCst),
        "the quit record must be durable before the first stop"
    );
    let record = report.record_id.clone().expect("record written");
    let items = db
        .read(move |conn| dao::quit_record::items(conn, &record))
        .await
        .expect("items");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].uid, U1);
    assert!(!items[0].survivor);
    assert!(report.survivor_pids.is_empty());
    assert!(coord.is_done());
}

// ---------------------------------------------------------------------------
// T-quit-sweep: the C5 bound, no SIGKILL, survivors recorded
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t_quit_sweep_bounds_the_stop_never_sigkills_a_vendor_and_records_survivors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (sup, db) = supervisor(dir.path()).await;
    let bin = tempfile::tempdir().expect("bin");
    let claude = vendor_copy(bin.path());
    gilde_code_lib::supervise::spawn::allow_executable(&claude, &gilde_code_lib::agents::VENDOR_KINDS);

    // U1: an ordinary session its driver stops at once.
    let quick = vec![fixtures::fake_agent_path().to_string_lossy().into_owned(), "hang".to_string()];
    let (_g1, k1) = register(&sup, ProcKind::Convo, &quick, U1).await;
    // U2: a "claude" that ignores SIGTERM and whose driver never answers.
    let stubborn = vec![claude.to_string_lossy().into_owned(), "ignore-term".to_string()];
    let (_g2, k2) = register(&sup, ProcKind::Convo, &stubborn, U2).await;

    let coord = QuitCoordinator::new(sup.clone(), Arc::new(NoStoreOps));
    let p = preview(&sup).await;
    assert_eq!(p.items.len(), 2);
    let fake = Arc::new(FakeSessions {
        db: Some(db.clone()),
        stop_pids: [(U1.to_string(), k1.pid())].into_iter().collect(),
        hang: vec![U2.to_string()],
        ..FakeSessions::default()
    });
    let started = Instant::now();
    let report = coord.run(p, Some(fake as Arc<dyn SessionActorApi>)).await;
    let took = started.elapsed();
    assert!(
        took < quit::QUIT_BOUND + Duration::from_millis(1_500),
        "the stop phase is bounded by 6 s (took {took:?})"
    );
    // The vendor process outlived the bound and was never SIGKILLed.
    assert!(!is_gone(k2.pid()), "a vendor CLI must survive (no SIGKILL)");
    assert!(report.survivor_pids.contains(&k2.pid()), "{report:?}");
    assert_eq!(report.survivor_sessions, vec![U2.to_string()]);
    let record = report.record_id.clone().expect("record");
    let items = db
        .read(move |conn| dao::quit_record::items(conn, &record))
        .await
        .expect("items");
    let u2 = items.iter().find(|i| i.uid == U2).expect("U2 item");
    assert!(u2.survivor, "survivor recorded in quit_record_item");
    assert!(!items.iter().find(|i| i.uid == U1).unwrap().survivor);
    // Its row stays `running` under this instance for the next boot sweep.
    let rows = db
        .read({
            let me = sup.app_instance_id().to_string();
            move |conn| dao::proc::rows_of_instance(conn, &me)
        })
        .await
        .expect("rows");
    let row = rows.iter().find(|r| r.pid == Some(i64::from(k2.pid()))).expect("row");
    assert_eq!(row.state, ProcState::Running);
    assert_eq!(row.note.as_deref(), Some(NOTE_QUIT_SURVIVOR));
}

// ---------------------------------------------------------------------------
// A39.1(b) (G7c-01): Gilde's own vendor sessions are vendor survivors of the
// quit whatever their exec path classifies as
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t_quit_records_native_and_unclassifiable_vendor_sessions_as_survivors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (sup, db) = supervisor(dir.path()).await;
    let home = tempfile::tempdir().expect("home");
    let place = |rel: &str| {
        let dest = home.path().join(rel);
        std::fs::create_dir_all(dest.parent().expect("parent")).expect("mkdir");
        std::fs::copy(fixtures::fake_agent_path(), &dest).expect("copy fixture");
        dest.to_string_lossy().into_owned()
    };
    // U2: the native installer layout Gilde spawns by realpath.
    let native = vec![place(".local/share/claude/versions/2.1.307"), "ignore-term".to_string()];
    let (_g2, k2) = register(&sup, ProcKind::Convo, &native, U2).await;
    // U3: a layout no classifier knows; only Gilde's own row says "vendor".
    let opaque = vec![place("opt/build-7/agent-cli"), "ignore-term".to_string()];
    let (_g3, k3) = register(&sup, ProcKind::Convo, &opaque, U3).await;
    assert_eq!(gilde_code_lib::supervise::procscan::vendor_of_pid(k2.pid()), Some("claude"));
    assert_eq!(gilde_code_lib::supervise::procscan::vendor_of_pid(k3.pid()), None);

    let coord = QuitCoordinator::new(sup.clone(), Arc::new(NoStoreOps));
    let p = preview(&sup).await;
    assert_eq!(p.items.len(), 2);
    let fake = Arc::new(FakeSessions {
        db: Some(db.clone()),
        hang: vec![U2.to_string(), U3.to_string()],
        ..FakeSessions::default()
    });
    let report = coord.run(p, Some(fake as Arc<dyn SessionActorApi>)).await;
    for (uid, kid) in [(U2, &k2), (U3, &k3)] {
        assert!(!is_gone(kid.pid()), "{uid}: a Gilde vendor session was SIGKILLed");
        assert!(report.survivor_pids.contains(&kid.pid()), "{uid}: {report:?}");
    }
    assert_eq!(report.survivor_sessions, vec![U2.to_string(), U3.to_string()]);
    let record = report.record_id.clone().expect("record");
    let items = db
        .read(move |conn| dao::quit_record::items(conn, &record))
        .await
        .expect("items");
    assert!(items.iter().all(|i| i.survivor), "both recorded as survivors: {items:?}");
}

/// A driver that answers "stopped" for a uid it holds no actor for, while the
/// row's process still runs, did not stop it: the fenced fallback does, and a
/// process nobody signalled is never recorded as a quit survivor.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_quit_a_session_its_driver_did_not_stop_goes_through_the_fallback() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (sup, db) = supervisor(dir.path()).await;
    let argv = vec![fixtures::fake_agent_path().to_string_lossy().into_owned(), "hang".to_string()];
    let (_g, kid) = register(&sup, ProcKind::Convo, &argv, U1).await;
    let coord = QuitCoordinator::new(sup.clone(), Arc::new(NoStoreOps));
    let p = preview(&sup).await;
    // No `stop_pids`: the driver reports U1 stopped and signals nothing.
    let fake = Arc::new(FakeSessions { db: Some(db.clone()), ..FakeSessions::default() });
    let report = coord.run(p, Some(fake as Arc<dyn SessionActorApi>)).await;
    assert!(report.survivor_pids.is_empty(), "{report:?}");
    assert!(report.survivor_sessions.is_empty(), "{report:?}");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !is_gone(kid.pid()) {
        assert!(Instant::now() < deadline, "the fallback never stopped the session");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ---------------------------------------------------------------------------
// A40 (G7c-02/03): quit survivors reach the next boot; panes at quit
// ---------------------------------------------------------------------------

/// A second instance on the same database (the next boot).
async fn instance_on(db: &Db, dir: &Path) -> Arc<Supervisor> {
    let config = Arc::new(ConfigStore::open_at(&dir.join("config.json")).expect("config"));
    Arc::new(Supervisor::open_instance(db, config, "0.0.0-test").await.expect("instance"))
}

async fn row_of_pid(db: &Db, instance: &str, pid: i32) -> gilde_code_lib::store::models::ProcRow {
    let me = instance.to_string();
    db.read(move |conn| dao::proc::rows_of_instance(conn, &me))
        .await
        .expect("rows")
        .into_iter()
        .find(|r| r.pid == Some(i64::from(pid)))
        .expect("row")
}

/// G7c-02 (A40.1), the reviewer's test: the shutdown sweep leaves a row the
/// quit handed over alone, so the next boot's sweep finds it (§10.2 step 6).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn g7c_02_a_quit_survivor_reaches_the_next_boot_sweep() {
    use gilde_code_lib::supervise::reaper::{self, StopGroup};
    let dir = tempfile::tempdir().expect("tempdir");
    let (sup, db) = supervisor(dir.path()).await;
    let bin = tempfile::tempdir().expect("bin");
    let claude = vendor_copy(bin.path());
    let stubborn = vec![claude.to_string_lossy().into_owned(), "ignore-term".to_string()];
    let (_g, kid) = register(&sup, ProcKind::Convo, &stubborn, U2).await;
    let me = sup.app_instance_id().to_string();

    // §10.2 steps 3–4: app_quit's sweep. The driver never answers.
    let coord = QuitCoordinator::new(sup.clone(), Arc::new(NoStoreOps));
    let p = preview(&sup).await;
    let fake = Arc::new(FakeSessions { hang: vec![U2.to_string()], ..FakeSessions::default() });
    let report = coord.run(p, Some(fake as Arc<dyn SessionActorApi>)).await;
    assert!(report.survivor_pids.contains(&kid.pid()), "precondition: {report:?}");
    let after_quit = row_of_pid(&db, &me, kid.pid()).await;
    assert_eq!(after_quit.state, ProcState::Running, "precondition");
    assert_eq!(after_quit.note.as_deref(), Some(NOTE_QUIT_SURVIVOR), "precondition");

    // `state::shutdown` step 3, then step 4.
    let reports = reaper::shutdown_sweep(&sup).await;
    assert!(reports.is_empty(), "the handed-over row is never signalled again: {reports:?}");
    let after_sweep = row_of_pid(&db, &me, kid.pid()).await;
    assert_eq!((after_sweep.state, after_sweep.note.as_deref()), (ProcState::Running, Some(NOTE_QUIT_SURVIVOR)));
    sup.close_instance().await.expect("close");

    // §10.2 step 6 on the next boot.
    let dir2 = tempfile::tempdir().expect("tempdir2");
    let next = instance_on(&db, dir2.path()).await;
    let mut group = StopGroup::new();
    let scheduled = reaper::sweep_quit_survivors_into(&next, &mut group).await;
    assert!(!is_gone(kid.pid()), "the vendor process still lives");
    assert_eq!(scheduled, 1, "the next boot sweep finds the quit survivor: row {after_sweep:?}");
}

/// The pane fixture: `/bin/sh -c` stays the parent and logs the signals it
/// gets (`trap_hup`: or dies on SIGHUP); its claude teammate ignores HUP and
/// TERM. Returns the argv and the signal log.
fn pane_argv(bin: &Path, claude: &Path, trap_hup: bool) -> (Vec<String>, PathBuf) {
    let log = bin.join("signals.log");
    let hup = if trap_hup { format!("trap 'echo HUP >> {}' HUP; ", log.display()) } else { String::new() };
    let script = format!(
        "{hup}trap 'echo TERM >> {log}' TERM; \
         (trap '' HUP; exec {claude} ignore-term) & c=$!; \
         while kill -0 $c 2>/dev/null; do wait $c; done",
        log = log.display(),
        claude = claude.display()
    );
    (vec!["/bin/sh".to_string(), "-c".to_string(), script], log)
}

fn teammate_of(pane: i32) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let snap = gilde_code_lib::supervise::procscan::ps_snapshot().expect("snap");
        if let Some(t) = snap
            .descendants_of(pane)
            .into_iter()
            .find(|p| gilde_code_lib::supervise::procscan::vendor_of_pid(*p) == Some("claude"))
        {
            return t;
        }
        assert!(Instant::now() < deadline, "the teammate never ran under the wrapper");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// G7c-03 (A40.3), the reviewer's test made strict: a hosted pane whose lead's
/// driver finished still gets SIGHUP (then TERM) from the quit, and its
/// surviving teammate is recorded with the lead's session; the pane row stays
/// a quit survivor for the next boot.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn g7c_03_a_hosted_pane_gets_sighup_at_quit_and_its_surviving_teammate_is_recorded() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (sup, db) = supervisor(dir.path()).await;
    let bin = tempfile::tempdir().expect("bin");
    let claude = vendor_copy(bin.path());
    let lead = vec![fixtures::fake_agent_path().to_string_lossy().into_owned(), "hang".to_string()];
    let (_gl, lead_kid) = register(&sup, ProcKind::Pty, &lead, U1).await;
    let (pane, log) = pane_argv(bin.path(), &claude, true);
    let (_gp, pane_kid) = register(&sup, ProcKind::Pane, &pane, U1).await;
    let teammate = teammate_of(pane_kid.pid());

    let coord = QuitCoordinator::new(sup.clone(), Arc::new(NoStoreOps));
    let p = preview(&sup).await;
    let fake = Arc::new(FakeSessions {
        stop_pids: [(U1.to_string(), lead_kid.pid())].into_iter().collect(),
        ..FakeSessions::default()
    });
    let report = coord.run(p, Some(fake as Arc<dyn SessionActorApi>)).await;
    std::thread::sleep(Duration::from_millis(200));
    let received = std::fs::read_to_string(&log).unwrap_or_default();
    let teammate_alive = !is_gone(teammate);
    let pane_row = row_of_pid(&db, sup.app_instance_id(), pane_kid.pid()).await;
    // Cleanup of what this test started (before asserting).
    unsafe {
        libc::kill(teammate, libc::SIGKILL);
    }
    assert!(received.contains("HUP"), "§10.2 step 4: SIGHUP to the pane's group; the sh got {received:?}");
    assert!(received.contains("TERM"), "then a fenced SIGTERM; the sh got {received:?}");
    assert!(teammate_alive, "precondition: the teammate ignores HUP and TERM");
    assert!(report.survivor_pids.contains(&teammate), "the teammate is recorded: {report:?}");
    assert_eq!(report.survivor_sessions, vec![U1.to_string()], "{report:?}");
    assert_eq!((pane_row.state, pane_row.note.as_deref()), (ProcState::Running, Some(NOTE_QUIT_SURVIVOR)));
}

/// A40.3: the pane's `sh` dies on SIGHUP and its teammate survives alone in
/// the pane's process group (reparented): still found and recorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t_quit_a_teammate_that_outlives_its_pane_wrapper_is_recorded() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (sup, _db) = supervisor(dir.path()).await;
    let bin = tempfile::tempdir().expect("bin");
    let claude = vendor_copy(bin.path());
    let (pane, _log) = pane_argv(bin.path(), &claude, false);
    let (_gp, pane_kid) = register(&sup, ProcKind::Pane, &pane, U1).await;
    let teammate = teammate_of(pane_kid.pid());

    let coord = QuitCoordinator::new(sup.clone(), Arc::new(NoStoreOps));
    let p = preview(&sup).await;
    // The lead's driver already finished (it holds no actor for U1).
    let fake = Arc::new(FakeSessions::default());
    let report = coord.run(p, Some(fake as Arc<dyn SessionActorApi>)).await;
    let wrapper_gone = is_gone(pane_kid.pid());
    let teammate_alive = !is_gone(teammate);
    unsafe {
        libc::kill(teammate, libc::SIGKILL);
    }
    assert!(wrapper_gone, "precondition: the sh died on SIGHUP");
    assert!(teammate_alive, "precondition: the teammate survived");
    assert!(report.survivor_pids.contains(&teammate), "{report:?}");
    assert_eq!(report.survivor_sessions, vec![U1.to_string()], "{report:?}");
}

/// A40.2: the shutdown sweep signals its rows concurrently — four stubborn
/// vendor rows (not quit survivors) all finish inside a budget that a
/// one-after-another sweep could not meet (about one TERM grace each).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t_quit_the_shutdown_sweep_signals_rows_concurrently_within_its_budget() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (sup, _db) = supervisor(dir.path()).await;
    let bin = tempfile::tempdir().expect("bin");
    let claude = vendor_copy(bin.path());
    let stubborn = vec![claude.to_string_lossy().into_owned(), "ignore-term".to_string()];
    let mut kids = Vec::new();
    for uid in [U1, U2, U3, "s_01890a5d-ac96-774b-bcce-b302099a805a"] {
        kids.push(register(&sup, ProcKind::Convo, &stubborn, uid).await);
    }
    // grace = budget / 3 = 1 s per row: in sequence only about two finish.
    let reports =
        gilde_code_lib::supervise::reaper::shutdown_sweep_within(&sup, Duration::from_secs(3)).await;
    assert_eq!(reports.len(), 4, "every row was signalled inside the budget: {reports:?}");
    for (_g, kid) in &kids {
        assert!(!is_gone(kid.pid()), "a vendor CLI is never SIGKILLed");
    }
}

// ---------------------------------------------------------------------------
// T-quit-planned
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_quit_planned_rows_are_recorded_and_closed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (sup, db) = supervisor(dir.path()).await;
    allow(&["/bin/sleep".to_string()], ProcKind::Pty);
    let guard = sup
        .plan_guarded(request(ProcKind::Pty, &["/bin/sleep".into(), "30".into()], U3))
        .await
        .expect("plan");
    let p = preview(&sup).await;
    assert_eq!(p.items.len(), 1, "a session between planned and running is in the record");
    assert_eq!(p.items[0].control_mode, "terminal");
    let coord = QuitCoordinator::new(sup.clone(), Arc::new(NoStoreOps));
    let report = coord.run(p, None).await;
    assert!(report.record_id.is_some());
    let id = guard.spawn_id().to_string();
    let row = db.read(move |conn| dao::proc::get(conn, &id)).await.expect("read").expect("row");
    assert_eq!(row.state, ProcState::Lost, "a planned row never spawns after the quit");
    std::mem::forget(guard);
}

// ---------------------------------------------------------------------------
// T-quit-crash + Continue all
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_quit_crash_record_and_continue_all() {
    let h = fixtures::Hermetic::new();
    let paths = AppPaths::for_home(&h.home());
    paths.ensure_all().expect("dirs");
    // A previous instance that ran two sessions and never closed its row.
    {
        let db = Db::open(&paths.state_db()).expect("db");
        let config = Arc::new(ConfigStore::open_at(&paths.config_file()).expect("config"));
        let old = Supervisor::open_instance(&db, config, "0.0.0-old").await.expect("old");
        for (uid, kind) in [(U1, ProcKind::Convo), (U2, ProcKind::Pty)] {
            allow(&["/bin/sleep".to_string()], kind);
            let g = old
                .plan_guarded(request(kind, &["/bin/sleep".into(), "1".into()], uid))
                .await
                .expect("plan");
            // A pid that is gone now: the crashed sessions ended with it.
            g.mark_running(999_999, 999_999).expect("running");
            std::mem::forget(g);
        }
    }
    let out = match boot::setup_core(paths.clone(), "0.0.0-test", "build").await.expect("setup") {
        SetupOutcome::Ready(out) => out,
        SetupOutcome::SecondInstance { .. } => panic!("no other instance"),
    };
    let record_id = match out.lifecycle.launch() {
        LaunchInfo::AfterCrash { record_id, count } => {
            assert_eq!(count, 2);
            record_id
        }
        other => panic!("expected AfterCrash, got {other:?}"),
    };
    out.lifecycle.window_visible(gilde_code_lib::store::now_ms());
    let sweep = boot::run_boot_sweep(&out.lifecycle, &out.supervisor).await;
    assert_eq!(sweep.crash_records, vec![record_id.as_str().to_string()]);
    assert!(out.lifecycle.gate.is_open());
    let rid = record_id.as_str().to_string();
    let rec = out
        .db
        .read(move |conn| dao::quit_record::get(conn, &rid))
        .await
        .expect("read")
        .expect("record");
    assert_eq!(rec.cause, CAUSE_CRASH);

    // Continue all: started items leave the record; the empty record goes.
    let services = Services::default();
    let fake = Arc::new(FakeSessions::default());
    services.bind_sessions(fake.clone());
    let res = gilde_code_lib::commands::quit::continue_all_core(
        &out.lifecycle,
        &services,
        ContinueAllReq {
            record_id: record_id.clone(),
            uids: Some(vec![SessionUid::parse(U1).unwrap(), SessionUid::parse(U2).unwrap()]),
        },
    )
    .await
    .expect("continue all");
    assert_eq!(res.started.len(), 2);
    let rid = record_id.as_str().to_string();
    let gone = out
        .db
        .read(move |conn| dao::quit_record::get(conn, &rid))
        .await
        .expect("read");
    assert!(gone.is_none(), "a fully continued record is deleted");
    // A dismissed or unknown record is NOT_FOUND.
    let err = gilde_code_lib::commands::quit::dismiss(
        &out.lifecycle,
        QuitRecordDismissReq { record_id },
    )
    .await
    .unwrap_err();
    assert_eq!(err.params["entity"], "quitRecord");
}
