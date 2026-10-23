//! A29.1: detected agents live in the store (`BootStatus.agents`,
//! `agentsScannedAt`), one mapping for the store and `newsession_options`,
//! reruns that replace the rows without a flash, and the reruns' triggers.
//! Owner: T29 (T09's lifecycle code).
//!
//! No vendor CLI ever runs here: the pure tests feed synthetic detection
//! rows, and the end-to-end tests pin **every** kind (and the terminal
//! shell) to a fake `#!/bin/sh` script before the first detection, so the
//! detection never searches a `PATH` directory — a guard asserts the pins
//! before anything is probed.

mod fixtures;

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;

use gilde_code_lib::agents::{AgentBinary, AgentKind, AgentRegistry};
use gilde_code_lib::commands::settings::{reruns_detection, set_executable_then, ExecutableConfirm};
use gilde_code_lib::config::ConfigStore;
use gilde_code_lib::contract::common::{AbsPath, Notice, Vendor};
use gilde_code_lib::contract::compat::ClaudeCompat;
use gilde_code_lib::contract::ipc::{ExecutableKey, ExecutableValue, SettingsSetExecutableReq, VendorOption};
use gilde_code_lib::contract::services::CompatApi;
use gilde_code_lib::contract::session::BinaryRef;
use gilde_code_lib::contract::store::{BootStatus, StoreOp};
use gilde_code_lib::events::{LifecycleFeed, StoreOpsSink};
use gilde_code_lib::lifecycle::boot::{self, agent_rows, detect_and_publish, Lifecycle};
use gilde_code_lib::paths::AppPaths;
use gilde_code_lib::sessions::manager::Binaries;
use gilde_code_lib::sessions::AgentBinaries;

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Every `Boot` status the feed published, in order.
#[derive(Default)]
struct Boots(Mutex<Vec<BootStatus>>);

impl StoreOpsSink for Boots {
    fn ops(&self, ops: Vec<StoreOp>) {
        for op in ops {
            if let StoreOp::Boot { status } = op {
                self.0.lock().push(status);
            }
        }
    }
}

fn feed() -> (Arc<LifecycleFeed>, Arc<Boots>) {
    let feed = LifecycleFeed::new();
    let boots = Arc::new(Boots::default());
    feed.set_local_store(boots.clone());
    (feed, boots)
}

fn bin(kind: AgentKind, path: Option<&str>, version: Option<&str>, installed: bool) -> AgentBinary {
    let mut b = AgentBinary::absent(kind, None);
    b.abs_path = path.map(PathBuf::from);
    b.version = version.map(str::to_string);
    b.installed = installed;
    b.ok = installed && version.is_some();
    b
}

fn row(vendor: Vendor, installed: bool, version: Option<&str>) -> VendorOption {
    VendorOption { vendor, installed, version: version.map(str::to_string) }
}

/// A detection as the registry reports it (every kind, the UI's order is not
/// the vendor order: gemini and the shell are mixed in).
fn detection(claude: &str) -> Vec<AgentBinary> {
    vec![
        bin(AgentKind::Shell, Some("/bin/zsh"), Some("5.9"), true),
        bin(AgentKind::Antigravity, None, None, false),
        bin(AgentKind::Codex, Some("/opt/x/codex"), Some("0.50.0"), true),
        bin(AgentKind::GeminiCli, Some("/opt/x/gemini"), Some("1.0.0"), true),
        bin(AgentKind::Claude, Some("/opt/x/claude"), Some(claude), true),
        // On disk once, not any more: an old version must not leak into the row.
        bin(AgentKind::Cursor, None, Some("2025.09.01"), false),
    ]
}

// ---------------------------------------------------------------------------
// the one mapping
// ---------------------------------------------------------------------------

#[test]
fn rows_are_one_per_vendor_in_order_and_a_missing_cli_has_no_version() {
    let rows = agent_rows(&detection("2.1.306"));
    assert_eq!(
        rows,
        vec![
            row(Vendor::Claude, true, Some("2.1.306")),
            row(Vendor::Codex, true, Some("0.50.0")),
            row(Vendor::Cursor, false, None),
            row(Vendor::Agy, false, None),
        ]
    );
    // An executable without a path is not installed either.
    let rows = agent_rows(&[bin(AgentKind::Claude, None, Some("2.1.306"), true)]);
    assert_eq!(rows[0], row(Vendor::Claude, false, None));
    // Nothing detected at all: every vendor, not installed.
    let rows = agent_rows(&[]);
    assert_eq!(rows.iter().map(|r| r.vendor).collect::<Vec<_>>(), Vendor::ALL.to_vec());
    assert!(rows.iter().all(|r| !r.installed && r.version.is_none()));
}

// ---------------------------------------------------------------------------
// the producer: publish, rerun, no flash
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_detection_publishes_rows_detected_and_the_scan_time_together() {
    let (feed, boots) = feed();
    assert!(!feed.boot().agents_detected && feed.boot().agents.is_empty() && feed.boot().agents_scanned_at.is_none());
    let t0 = gilde_code_lib::store::now_ms();
    detect_and_publish(&feed, async { Some(detection("2.1.306")) }).await;
    let b = feed.boot();
    assert!(b.agents_detected);
    assert_eq!(b.agents, agent_rows(&detection("2.1.306")));
    assert!(b.agents_scanned_at.is_some_and(|at| at >= t0));
    // One Boot op carried all three at once (never rows without the flag).
    let published = boots.0.lock().clone();
    assert_eq!(published.len(), 1);
    assert!(published[0].agents_detected && published[0].agents.len() == 4 && published[0].agents_scanned_at.is_some());
}

#[tokio::test]
async fn the_scan_time_moves_on_every_rerun_even_with_identical_rows() {
    let (feed, boots) = feed();
    let mut times = Vec::new();
    for _ in 0..5 {
        detect_and_publish(&feed, async { Some(detection("2.1.306")) }).await;
        times.push(feed.boot().agents_scanned_at.expect("scanned"));
    }
    // Strictly increasing, even when the runs land in the same millisecond.
    assert!(times.windows(2).all(|w| w[1] > w[0]), "{times:?}");
    assert_eq!(boots.0.lock().len(), 5, "an identical rerun is still published");
    assert!(boots.0.lock().iter().all(|b| b.agents == agent_rows(&detection("2.1.306"))));
}

#[tokio::test]
async fn the_previous_rows_stay_while_a_rerun_runs() {
    let (feed, boots) = feed();
    // Before the first detection completes: still "Finding installed agents…".
    let (tx, rx) = tokio::sync::oneshot::channel::<Vec<AgentBinary>>();
    let f = feed.clone();
    let first = tokio::spawn(async move { detect_and_publish(&f, async move { rx.await.ok() }).await });
    tokio::task::yield_now().await;
    assert!(!feed.boot().agents_detected && feed.boot().agents.is_empty());
    tx.send(detection("2.1.306")).unwrap();
    first.await.unwrap();
    let before = feed.boot();
    assert!(before.agents_detected);
    let ops_before = boots.0.lock().len();

    // A rerun in flight: nothing changes, nothing is published.
    let (tx, rx) = tokio::sync::oneshot::channel::<Vec<AgentBinary>>();
    let f = feed.clone();
    let rerun = tokio::spawn(async move { detect_and_publish(&f, async move { rx.await.ok() }).await });
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert_eq!(feed.boot(), before, "no flash: the old rows, the flag and the time stay");
    assert_eq!(boots.0.lock().len(), ops_before, "nothing is published while it runs");

    // It completes: its rows replace the old ones.
    tx.send(detection("2.1.307")).unwrap();
    rerun.await.unwrap();
    let after = feed.boot();
    assert_eq!(after.agents[0], row(Vendor::Claude, true, Some("2.1.307")));
    assert!(after.agents_scanned_at > before.agents_scanned_at);
}

#[tokio::test]
async fn a_detection_that_finishes_after_a_newer_one_never_overwrites_it() {
    let (feed, _boots) = feed();
    let (tx_old, rx_old) = tokio::sync::oneshot::channel::<Vec<AgentBinary>>();
    let (tx_new, rx_new) = tokio::sync::oneshot::channel::<Vec<AgentBinary>>();
    let f = feed.clone();
    let old = tokio::spawn(async move { detect_and_publish(&f, async move { rx_old.await.ok() }).await });
    tokio::task::yield_now().await;
    let f = feed.clone();
    let new = tokio::spawn(async move { detect_and_publish(&f, async move { rx_new.await.ok() }).await });
    tokio::task::yield_now().await;
    // The newer one (started second) completes first …
    tx_new.send(detection("2.1.307")).unwrap();
    new.await.unwrap();
    let published = feed.boot();
    // … and the older one's late answer is dropped.
    tx_old.send(detection("2.1.306")).unwrap();
    old.await.unwrap();
    assert_eq!(feed.boot(), published);
    assert_eq!(feed.boot().agents[0].version.as_deref(), Some("2.1.307"));
}

#[tokio::test]
async fn a_failed_rerun_keeps_the_rows_and_a_failed_first_detection_lists_every_vendor_missing() {
    let (feed, _boots) = feed();
    detect_and_publish(&feed, async { None }).await;
    let b = feed.boot();
    assert!(b.agents_detected, "the UI leaves \"Finding installed agents…\"");
    assert_eq!(b.agents, agent_rows(&[]));

    detect_and_publish(&feed, async { Some(detection("2.1.306")) }).await;
    let good = feed.boot();
    detect_and_publish(&feed, async { None }).await;
    let b = feed.boot();
    assert_eq!(b.agents, good.agents, "a failed rerun never makes the agents vanish");
    assert!(b.agents_scanned_at > good.agents_scanned_at, "but it did complete");
}

// ---------------------------------------------------------------------------
// end to end over fake binaries: B9, the rerun, newsession_options
// ---------------------------------------------------------------------------

/// A fake CLI: answers `--version` with `version`, never anything else.
fn fake_cli(dir: &Path, name: &str, version: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\necho \"{name} {version}\"\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

struct Rig {
    _env: fixtures::Hermetic,
    bin: tempfile::TempDir,
    _path: tempfile::TempDir,
    lifecycle: Arc<Lifecycle>,
    agents: Arc<AgentRegistry>,
}

impl Rig {
    fn new() -> Rig {
        let env = fixtures::Hermetic::new();
        let paths = AppPaths::for_home(&env.home());
        paths.ensure_all().expect("dirs");
        let bin = tempfile::Builder::new().prefix("gilde-a29-bin-").tempdir().unwrap();
        let empty_path = tempfile::Builder::new().prefix("gilde-a29-path-").tempdir().unwrap();
        let config = Arc::new(ConfigStore::open_at(&paths.config_file()).expect("config"));
        let pins: Vec<(&str, PathBuf)> = vec![
            ("claude", fake_cli(bin.path(), "fake-claude", "2.1.306")),
            ("codex", fake_cli(bin.path(), "fake-codex", "0.50.0")),
            ("cursorAgent", fake_cli(bin.path(), "fake-cursor", "2025.10.01")),
            ("agy", fake_cli(bin.path(), "fake-agy", "1.3.5")),
            ("gemini", fake_cli(bin.path(), "fake-gemini", "1.0.0")),
        ];
        let shell = fake_cli(bin.path(), "fake-shell", "5.9");
        let mut binaries = serde_json::Map::new();
        for (k, p) in &pins {
            binaries.insert((*k).to_string(), serde_json::json!(p.to_string_lossy()));
        }
        config
            .patch(serde_json::json!({
                "agents": { "binaries": binaries, "pathOverride": empty_path.path().to_string_lossy() },
                "terminal": { "shell": shell.to_string_lossy() }
            }))
            .expect("pins");
        // The guard: every kind is pinned to a fake, so no PATH directory is
        // ever searched and no vendor CLI can be reached.
        let cfg = config.get();
        for kind in AgentKind::ALL {
            match kind {
                AgentKind::Shell => assert_eq!(cfg.terminal.shell.as_deref(), Some(shell.to_string_lossy().as_ref())),
                k => {
                    let pin = cfg.agent_binary(k.key()).unwrap_or_else(|| panic!("{k:?} is not pinned"));
                    assert!(pin.starts_with(bin.path().to_string_lossy().as_ref()), "{k:?} pinned outside the fakes");
                }
            }
        }
        let agents = Arc::new(AgentRegistry::without_db(paths, config));
        Rig { _env: env, bin, _path: empty_path, lifecycle: Lifecycle::new("0.0.0-test", "build"), agents }
    }
}

#[derive(Default)]
struct NoCompat;

#[async_trait]
impl CompatApi for NoCompat {
    fn list(&self) -> Vec<ClaudeCompat> {
        Vec::new()
    }
    fn lookup(&self, _version: &str) -> Option<ClaudeCompat> {
        None
    }
    async fn for_binary(&self, _binary: &BinaryRef) -> ClaudeCompat {
        unreachable!("newsession_options' vendor rows never resolve a binary")
    }
    fn feature(&self, _version: &str, _flag: &str) -> bool {
        false
    }
    fn app_feature(&self, _flag: &str) -> bool {
        false
    }
    fn notices(&self, _version: &str) -> Vec<Notice> {
        Vec::new()
    }
    fn schedule_canary(&self, _binary: &BinaryRef) {}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b9_publishes_the_rows_and_newsession_options_answers_the_same_ones() {
    let rig = Rig::new();
    let detected = boot::run_detection(&rig.lifecycle, &rig.agents, None).await;
    assert_eq!(detected.len(), AgentKind::ALL.len());
    let b = rig.lifecycle.feed.boot();
    assert!(b.agents_detected);
    assert_eq!(
        b.agents,
        vec![
            row(Vendor::Claude, true, Some("2.1.306")),
            row(Vendor::Codex, true, Some("0.50.0")),
            row(Vendor::Cursor, true, Some("2025.10.01")),
            row(Vendor::Agy, true, Some("1.3.5")),
        ]
    );
    let vendors = AgentBinaries::new(rig.agents.clone(), Arc::new(NoCompat)).installed().await.expect("installed");
    assert_eq!(vendors, b.agents, "newsession_options.vendors == BootStatus.agents");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rerun_probes_again_and_replaces_the_rows_and_both_answers_still_agree() {
    let rig = Rig::new();
    boot::run_detection(&rig.lifecycle, &rig.agents, None).await;
    let before = rig.lifecycle.feed.boot();
    // The installed claude is replaced in place (same path, new version).
    fake_cli(rig.bin.path(), "fake-claude", "2.1.307");
    // `agents_rescan` / an applied setting: scheduled, the caller never waits.
    let handle = boot::spawn_rescan(rig.lifecycle.clone(), rig.agents.clone(), None);
    handle.await.expect("rescan");
    let after = rig.lifecycle.feed.boot();
    assert_eq!(after.agents[0], row(Vendor::Claude, true, Some("2.1.307")), "the rerun bypassed the caches");
    assert!(after.agents_scanned_at > before.agents_scanned_at);
    let vendors = AgentBinaries::new(rig.agents.clone(), Arc::new(NoCompat)).installed().await.expect("installed");
    assert_eq!(vendors, after.agents);
}

// ---------------------------------------------------------------------------
// the triggers of a rerun
// ---------------------------------------------------------------------------

#[test]
fn binary_pins_and_the_path_override_rerun_detection_nothing_else_does() {
    for key in [
        ExecutableKey::ClaudeBinary,
        ExecutableKey::CodexBinary,
        ExecutableKey::CursorBinary,
        ExecutableKey::AgyBinary,
        ExecutableKey::PathsOverride,
    ] {
        assert!(reruns_detection(key), "{key:?}");
    }
    for key in [ExecutableKey::SidecarNodePath, ExecutableKey::EnvPassthrough] {
        assert!(!reruns_detection(key), "{key:?}");
    }
}

struct Answer(bool);

#[async_trait]
impl ExecutableConfirm for Answer {
    async fn confirm(&self, _key: &str, _value_text: &str) -> bool {
        self.0
    }
}

#[tokio::test]
async fn only_an_applied_binary_or_path_override_schedules_the_rerun() {
    let env = fixtures::Hermetic::new();
    let paths = AppPaths::for_home(&env.home());
    paths.ensure_all().expect("dirs");
    let config = ConfigStore::open_at(&paths.config_file()).expect("config");
    let dir = tempfile::Builder::new().prefix("gilde-a29-set-").tempdir().unwrap();
    let exe = fake_cli(dir.path(), "fake-claude", "2.1.306");
    let path_req = |key: ExecutableKey, p: &Path| SettingsSetExecutableReq {
        key,
        value: ExecutableValue::Path { path: Some(AbsPath::parse(p.to_string_lossy().into_owned()).unwrap()) },
    };
    let calls: Mutex<Vec<ExecutableKey>> = Mutex::new(Vec::new());

    // Applied binary pin → one rerun, after the value is in the config.
    let res = set_executable_then(&config, &paths, &Answer(true), path_req(ExecutableKey::ClaudeBinary, &exe), |k| {
        // A45.1: the canonical path is what is applied.
        let canonical = std::fs::canonicalize(&exe).unwrap();
        assert_eq!(config.get().agent_binary("claude"), Some(canonical.to_string_lossy().as_ref()), "applied before the rerun");
        calls.lock().push(k);
    })
    .await
    .expect("applied");
    assert!(res.applied);
    // Cancelled → nothing.
    let res = set_executable_then(&config, &paths, &Answer(false), path_req(ExecutableKey::CodexBinary, &exe), |k| calls.lock().push(k))
        .await
        .expect("answered");
    assert!(!res.applied);
    // Refused value → an error and nothing.
    let refused = set_executable_then(&config, &paths, &Answer(true), path_req(ExecutableKey::AgyBinary, dir.path()), |k| calls.lock().push(k)).await;
    assert!(refused.is_err());
    // The sidecar's node is not an agent → nothing.
    let res = set_executable_then(&config, &paths, &Answer(true), path_req(ExecutableKey::SidecarNodePath, &exe), |k| calls.lock().push(k))
        .await
        .expect("applied");
    assert!(res.applied);
    // The PATH override (a directory) → a rerun.
    let res = set_executable_then(&config, &paths, &Answer(true), path_req(ExecutableKey::PathsOverride, dir.path()), |k| calls.lock().push(k))
        .await
        .expect("applied");
    assert!(res.applied);
    assert_eq!(*calls.lock(), vec![ExecutableKey::ClaudeBinary, ExecutableKey::PathsOverride]);
}
