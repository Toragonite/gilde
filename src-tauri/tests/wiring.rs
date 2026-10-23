//! T-wiring-* (contract A22.4 b): the composition root (`lifecycle::wiring`)
//! with fakes only — a hermetic `GILDE_HOME`, a fixture process table, no
//! vendor CLI, no window, no notification, Dock, focus or dialog — builds
//! every service, manages every state the commands read, and every
//! cross-service hook is live end to end.
//!
//! Owner: T09 (T09b).

mod fixtures;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gilde_code_lib::accounts::AccountsService;
use gilde_code_lib::commands;
use gilde_code_lib::commands::store::LocalStore;
use gilde_code_lib::contract::account::{Quota, QuotaWindow, QuotaWindowKind};
use gilde_code_lib::contract::activity::{ExitInfo, ProcessFacts};
use gilde_code_lib::contract::common::{AbsPath, Notice, Vendor};
use gilde_code_lib::contract::ids::SessionUid;
use gilde_code_lib::contract::ipc::PauseUntil;
use gilde_code_lib::contract::process::ProcKind as CKind;
use gilde_code_lib::contract::services::{
    CompatApi, CompatSink, EvidenceSink, EvidenceUpdate, IdentityHint, IdentityKey, PaneHostApi,
    SessionMetaFacts, SourceFacts, SpawnRequest, SupervisorApi, WorldApi,
};
use gilde_code_lib::contract::store::OrchestratorPresence;
use gilde_code_lib::discovery::history::{HistoryState, UidResolver};
use gilde_code_lib::discovery::procs::{FakeProc, FakeProcs};
use gilde_code_lib::discovery::OwnProcesses;
use gilde_code_lib::lifecycle::boot::{self, SetupOutcome, SetupOutput};
use gilde_code_lib::lifecycle::wiring::{self, Core, Options, Wired};
use gilde_code_lib::paths::AppPaths;
use gilde_code_lib::sessions::SessionsManager;
use gilde_code_lib::sidecar::{SidecarManager, SidecarRuntime};
use gilde_code_lib::state::Services;
use gilde_code_lib::store::now_ms;
use gilde_code_lib::supervise::health::HealthState;
use gilde_code_lib::supervise::watchdog::Watchdog;
use gilde_code_lib::world::World;
use tauri::Manager as _;

use fixtures::Hermetic;

/// The tests of this file run one at a time: `Hermetic::enter` points
/// `TMPDIR` at its own tree, so a second test's `Hermetic::new` would create
/// its root inside the first one's (and lose it when that one ends).
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// One wired core on a hermetic home.
struct Rig {
    _out: Box<SetupOutput>,
    core: Core,
    wired: Arc<Wired>,
    procs: Arc<FakeProcs>,
    app: tauri::App<tauri::test::MockRuntime>,
    home: PathBuf,
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.wired.shutdown();
    }
}

async fn rig(h: &Hermetic) -> Rig {
    let home = std::fs::canonicalize(h.home()).expect("canonical home");
    let paths = AppPaths::for_home(&home);
    let out = match boot::setup_core(paths, "0.0.0-test", "build-test").await.expect("setup") {
        SetupOutcome::Ready(out) => out,
        SetupOutcome::SecondInstance { .. } => panic!("no other instance holds the lock"),
    };
    // A38.2: detection is pinned to the fixture dir (no login shell, no real CLI).
    out.config.patch(h.detection_pins()).expect("detection pins");
    out.config.flush().expect("flush the pins");
    out.lifecycle.window_visible(now_ms());
    boot::run_boot_sweep(&out.lifecycle, &out.supervisor).await;
    let core = Core {
        paths: out.paths.clone(),
        db: out.db.clone(),
        config: out.config.clone(),
        lifecycle: out.lifecycle.clone(),
        supervisor: out.supervisor.clone(),
        services: Services::new(),
        health: HealthState::new(),
    };
    let local = LocalStore::new();
    core.lifecycle.feed.set_local_store(local.clone());
    // A runtime that cannot start: no test here may start the sidecar.
    let (platform, _recorded) =
        wiring::fakes::platform(SidecarRuntime::explicit("/nonexistent/node", "/nonexistent/sidecar.cjs"));
    let (discovery, procs) = wiring::fakes::hermetic_discovery(&home);
    let wired = wiring::wire(
        &core,
        platform,
        Options {
            discovery: Some(discovery),
            procs: Some(procs.clone()),
            follow_watch: false,
            background: false,
            runtime: tokio::runtime::Handle::current(),
        },
    )
    .await
    .expect("wired");
    let wired = Arc::new(wired);
    let app = tauri::test::mock_app();
    wiring::manage_core(app.handle(), &core, local);
    wiring::manage_all(app.handle(), &wired);
    Rig {
        _out: out,
        core,
        wired,
        procs,
        app,
        home,
    }
}

/// Discovery's queued work (a reconcile included) has run.
async fn settle_discovery(r: &Rig) {
    let d = r.wired.discovery.clone();
    let done = tokio::task::spawn_blocking(move || {
        d.read().as_ref().map(|d| d.sync(Duration::from_secs(30)).is_some())
    })
    .await
    .expect("join");
    assert_eq!(done, Some(true), "discovery answered");
}

/// Wait (bounded, generous) for an event another thread makes true.
async fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !f() {
        assert!(Instant::now() < deadline, "{what} never became true");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ---------------------------------------------------------------------------
// T-wiring-services
// ---------------------------------------------------------------------------

/// Every service is built and bound, and every state a command reads is
/// managed (else the command answers `UNSUPPORTED{notBuilt}`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t_wiring_every_service_is_built_and_every_state_managed() {
    let _serial = SERIAL.lock().await;
    let h = Hermetic::new();
    let _env = h.enter();
    let r = rig(&h).await;
    let a = r.app.handle();
    macro_rules! managed {
        ($($t:ty),* $(,)?) => {$(
            assert!(a.try_state::<$t>().is_some(), concat!("not managed: ", stringify!($t)));
        )*};
    }
    managed!(
        commands::app::PathsState,
        commands::accounts::DbState,
        commands::app::ConfigState,
        Arc<gilde_code_lib::supervise::registry::Supervisor>,
        commands::app::LifecycleState,
        commands::app::ServicesState,
        commands::app::HealthStateRef,
        commands::store::LocalStoreState,
        HistoryState,
        commands::convo::ConvoState,
        gilde_code_lib::notify::NotifyState,
        gilde_code_lib::macos::owner::OwnerState,
        commands::changes::GitState,
        commands::ide::IdeState,
        commands::project::ProjectState,
        commands::team::TeamState,
        commands::pty::PtyState,
        gilde_code_lib::status::StatusState,
        Arc<SidecarManager>,
        Arc<AccountsService>,
        commands::worktree::WorktreeState,
        commands::app::AgentsState,
        Arc<World>,
        Arc<Watchdog>,
        Arc<SessionsManager>,
        Arc<dyn PaneHostApi>,
    );
    let s = &r.core.services;
    assert!(s.world().is_some(), "World bound");
    assert!(s.compat().is_some(), "compat bound");
    assert!(s.notify().is_some(), "notifications bound");
    assert!(s.sessions().is_some(), "the sessions manager bound (T10, A22.5)");
    assert!(r.core.lifecycle.feed.world_bound(), "the lifecycle feed goes to World (A12.1)");
    assert!(
        gilde_code_lib::worktree::git::supervisor().is_some(),
        "git spawns through SupervisorApi (A17.1)"
    );
}

// ---------------------------------------------------------------------------
// T-wiring-compat (A10.1)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t_wiring_a_compat_change_reaches_the_store() {
    let _serial = SERIAL.lock().await;
    let h = Hermetic::new();
    let _env = h.enter();
    let r = rig(&h).await;
    let compat = r.wired.compat.clone();
    assert_eq!(
        r.wired.world.snapshot().compat,
        CompatApi::list(compat.as_ref()),
        "the initial list was sent once"
    );
    // Runtime evidence for a version: the service changes, World follows.
    CompatSink::note(compat.as_ref(), "9.8.7", "wiring test".into());
    let c = compat.clone();
    assert!(
        tokio::task::spawn_blocking(move || c.wait_idle(Duration::from_secs(20))).await.expect("join"),
        "compat settled"
    );
    let list = CompatApi::list(compat.as_ref());
    assert!(list.iter().any(|c| c.version == "9.8.7"), "the new version is listed: {list:?}");
    assert_eq!(r.wired.world.snapshot().compat, list, "the store's Compat{{list}} follows the service");
}

// ---------------------------------------------------------------------------
// T-wiring-orchestrator (A11.6)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t_wiring_orchestrator_presence_reaches_the_store_and_the_sidecar() {
    let _serial = SERIAL.lock().await;
    let h = Hermetic::new();
    let _env = h.enter();
    let r = rig(&h).await;
    settle_discovery(&r).await;
    assert_eq!(
        r.wired.world.snapshot().integrations.orchestrator,
        OrchestratorPresence::Absent,
        "the first reconcile reports absence"
    );
    assert!(!r.wired.sidecar.orchestrator_present());

    // The orchestrator appears in the hermetic home (a fixture registry).
    let root = r.home.join(".claude-code-orchestrator");
    std::fs::create_dir_all(&root).expect("root");
    std::fs::write(root.join("registry.json"), b"{}").expect("registry");
    r.wired.discovery_handle().wake();
    settle_discovery(&r).await;
    assert_eq!(
        r.wired.world.snapshot().integrations.orchestrator,
        OrchestratorPresence::Present,
        "Integrations{{state}} follows discovery"
    );
    let sidecar = r.wired.sidecar.clone();
    eventually("the sidecar's presence flag", || sidecar.orchestrator_present()).await;
}

// ---------------------------------------------------------------------------
// T-wiring-notifyPause (A16.1)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t_wiring_a_notification_pause_reaches_the_queue() {
    let _serial = SERIAL.lock().await;
    let h = Hermetic::new();
    let _env = h.enter();
    let r = rig(&h).await;
    assert_eq!(r.wired.world.queue().paused_until, None);
    let until = r.wired.notify_api.pause(PauseUntil::UntilResume);
    assert!(until.is_some(), "a pause has an end (or 'until resumed')");
    r.wired.notify.settle().await;
    let world = r.wired.world.clone();
    eventually("AttentionQueue.paused_until", || world.queue().paused_until.is_some()).await;
    assert_eq!(world.queue().paused_until, until);
    r.wired.notify_api.resume();
    r.wired.notify.settle().await;
    eventually("the pause cleared", || world.queue().paused_until.is_none()).await;
}

// ---------------------------------------------------------------------------
// T-wiring-lifecycle (A12.1)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t_wiring_a_lifecycle_op_reaches_the_store() {
    let _serial = SERIAL.lock().await;
    let h = Hermetic::new();
    let _env = h.enter();
    let r = rig(&h).await;
    assert!(r.wired.world.snapshot().boot.spawn_gate_open, "Boot{{status}} went to World at binding");
    r.core.lifecycle.feed.update_boot(|b| b.agents_detected = true);
    assert!(r.wired.world.snapshot().boot.agents_detected, "a later boot change reaches World");
    r.core.lifecycle.feed.set_notice(
        "test",
        Some(Notice {
            key: "notice.memoryOnly".into(),
            params: Default::default(),
        }),
    );
    assert!(
        r.wired.world.snapshot().notices.iter().any(|n| n.key == "notice.memoryOnly"),
        "Notices reach World"
    );
}

// ---------------------------------------------------------------------------
// T-wiring-usageGate (A10.4) + follow_structure for live Claude sessions
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t_wiring_the_transcript_usage_gate_is_bound() {
    let _serial = SERIAL.lock().await;
    let h = Hermetic::new();
    let _env = h.enter();
    let r = rig(&h).await;
    let dir = r.home.join(".claude/projects/-w");
    std::fs::create_dir_all(&dir).expect("dir");
    let path = dir.join("0e2c0b1a-6b7d-4c51-9a8e-1f2e3d4c5b6a.jsonl");
    std::fs::write(&path, b"").expect("transcript");
    let follow = r.wired.follow.clone();
    assert!(!follow.usage_allowed(&path), "no version known: no transcript spend");
    assert!(follow.followed().is_empty());

    let hint = IdentityHint {
        vendor: Some(Vendor::Claude),
        vendor_session_id: Some("0e2c0b1a-6b7d-4c51-9a8e-1f2e3d4c5b6a".into()),
        ..IdentityHint::default()
    };
    let version = "2.1.306";
    let meta = SessionMetaFacts {
        claude_version: Some(version.into()),
        transcript_path: Some(AbsPath::parse(path.to_string_lossy().into_owned()).expect("abs")),
        ..SessionMetaFacts::default()
    };
    let at = now_ms();
    follow.evidence(EvidenceUpdate { hint: hint.clone(), facts: SourceFacts::Meta(Box::new(meta)), at });
    assert!(follow.followed().is_empty(), "not followed before the process is known alive");
    follow.evidence(EvidenceUpdate {
        hint: hint.clone(),
        facts: SourceFacts::Process(ProcessFacts::Alive { pid: 4242, start_sec: at / 1000, cpu_active_at: None }),
        at,
    });
    assert_eq!(follow.followed(), vec![path.clone()], "a live Claude session's transcript is followed");
    assert_eq!(
        follow.usage_allowed(&path),
        r.wired.compat.feature(version, "transcriptUsage"),
        "the gate is the compat flag of the session's version"
    );
    follow.evidence(EvidenceUpdate { hint, facts: SourceFacts::Process(ProcessFacts::Gone { at }), at });
    assert!(follow.followed().is_empty(), "the session ended: no longer followed");
    assert!(!follow.usage_allowed(&path));
}

// ---------------------------------------------------------------------------
// T-wiring-uidResolver (A11.4)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t_wiring_history_resolves_uids_through_world_never_memory_uids() {
    let _serial = SERIAL.lock().await;
    let h = Hermetic::new();
    let _env = h.enter();
    let r = rig(&h).await;
    let key = |sid: &str| IdentityKey {
        vendor: Vendor::Claude,
        account_id: None,
        vendor_session_id: sid.to_string(),
    };
    // A uid minted through history's resolver is World's ...
    let a = key("11111111-2222-4333-8444-555555555555");
    let uid = r.wired.uids.uid_for(&a);
    assert_eq!(UidResolver::key_of(r.wired.world.as_ref(), &uid), Some(a.clone()), "World knows it");
    // ... and one World minted resolves through history's: one map, World's.
    let b = key("66666666-7777-4888-8999-aaaaaaaaaaaa");
    let uid_b = UidResolver::uid_for(r.wired.world.as_ref(), &b);
    assert_eq!(r.wired.uids.key_of(&uid_b), Some(b));
    assert_eq!(r.wired.uids.uid_for(&a), uid, "stable");
}

// ---------------------------------------------------------------------------
// T-wiring-discovery (A11.3, A12.2)
// ---------------------------------------------------------------------------

/// Discovery classifies with the supervisor's registry (`OwnProcesses`),
/// publishes every scan into the `ScanHub`, and the watchdog reads that one
/// scan — no second scanner of the machine.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t_wiring_discovery_uses_the_supervisor_and_feeds_the_one_scan() {
    let _serial = SERIAL.lock().await;
    let h = Hermetic::new();
    let _env = h.enter();
    let r = rig(&h).await;
    let sup = r.core.supervisor.clone();
    assert!(Arc::ptr_eq(&r.wired.own, &sup), "discovery's OwnProcesses is the supervisor");

    // A process Gilde started: planned through SupervisorApi, then running.
    gilde_code_lib::supervise::spawn::allow_executable(std::path::Path::new("/bin/sleep"), &[CKind::Pty]);
    let ticket = SupervisorApi::plan_spawn(
        sup.as_ref(),
        SpawnRequest {
            kind: CKind::Pty,
            argv0: AbsPath::parse("/bin/sleep").expect("abs"),
            args: vec!["30".into()],
            cwd: AbsPath::parse("/tmp").ok(),
            env_keys: vec!["GILDE_SPAWN_ID".into()],
            parent_uid: Some(SessionUid::mint()),
            generation: None,
            turn_id: None,
        },
    )
    .await
    .expect("planned");
    let mut child = std::process::Command::new("/bin/sleep")
        .arg("30")
        .env("GILDE_SPAWN_ID", &ticket.env_marker)
        .spawn()
        .expect("sleep");
    let pid = child.id() as i32;
    SupervisorApi::mark_running(sup.as_ref(), &ticket, pid).await.expect("running");
    let start = gilde_code_lib::supervise::procscan::start_time_of(pid).expect("start time");
    let own = r.wired.own.lookup(pid, start).expect("the registry knows its own process");
    assert_eq!(own.spawn_id, ticket.spawn_id);
    assert!(own.this_instance);

    // The fixture table holds that one process; discovery scans it.
    // SAFETY: `getuid` has no failure mode.
    let uid = unsafe { libc::getuid() };
    r.procs.insert(
        pid,
        FakeProc {
            ppid: 1,
            uid,
            start_sec: start,
            comm: "sleep".into(),
            exec_path: Some("/bin/sleep".into()),
            argv: Some(vec!["/bin/sleep".into(), "30".into()]),
            rss_bytes: 1 << 20,
            ..FakeProc::default()
        },
    );
    r.wired.discovery_handle().wake();
    settle_discovery(&r).await;
    let scan = r.core.lifecycle.scans.latest().expect("discovery published a scan");
    assert!(scan.procs.iter().any(|p| p.pid == pid), "the scan holds the fixture process");
    assert_eq!(
        SupervisorApi::last_scan(sup.as_ref()).map(|s| s.taken_at),
        Some(scan.taken_at),
        "SupervisorApi::last_scan answers from the same hub"
    );

    // The watchdog reads that scan: exactly the fixture's processes, not the
    // machine (this test process is not in the table).
    r.wired.watchdog.tick().await;
    let rows = r.wired.watchdog.latest_scan().expect("watchdog scanned").rows;
    assert!(rows.iter().any(|row| row.pid == pid), "the watchdog sees the shared scan");
    let me = std::process::id() as i32;
    assert!(
        rows.iter().all(|row| row.pid != me),
        "no second scanner: the test process is not in the fixture table"
    );

    let _ = child.kill();
    let _ = child.wait();
    SupervisorApi::mark_exited(sup.as_ref(), &ticket, ExitInfo { code: None, signal: Some(libc::SIGKILL) }).await;
}

// ---------------------------------------------------------------------------
// A24.1: a quota observed by a driver survives the next publish
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t_wiring_a_driver_quota_survives_the_next_publish() {
    use gilde_code_lib::sessions::{AccountsPort as _, ServiceAccounts};
    let _serial = SERIAL.lock().await;
    let h = Hermetic::new();
    let _env = h.enter();
    let r = rig(&h).await;
    let accounts = r.wired.accounts.clone();
    let account = accounts
        .accounts()
        .await
        .into_iter()
        .find(|a| a.vendor == Vendor::Claude)
        .expect("the default Claude login is always offered");
    let quota = Quota {
        windows: vec![QuotaWindow {
            kind: QuotaWindowKind::FiveHour,
            label: None,
            used_pct: Some(42),
            resets_at: None,
        }],
        fetched_at: now_ms(),
        stale: false,
        unavailable: None,
    };
    let port = ServiceAccounts::new(accounts.clone(), r.wired.world.clone() as Arc<dyn WorldApi>);
    port.observed_quota(&account.id, quota.clone());
    let a = accounts.clone();
    let id = account.id.clone();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if a.account(&id).await.and_then(|x| x.quota).as_ref() == Some(&quota) {
            break;
        }
        assert!(Instant::now() < deadline, "the observed quota never reached the service");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // The service's next publish and background pass keep it: it is the
    // service's own reading now, not a value pushed past it — and the store
    // shows it (accounts publish without sessions, A24.8).
    accounts.publish().await;
    accounts.background_tick().await;
    assert_eq!(a.account(&id).await.and_then(|x| x.quota), Some(quota.clone()), "not reverted");
    r.wired.world.flush_now();
    let shown = r.wired.world.snapshot().accounts.into_iter().find(|x| x.id == id).expect("published");
    assert_eq!(shown.quota, Some(quota), "the store shows the observed reading");
}
