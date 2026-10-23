//! Fault behaviour of the sidecar channel and its supervision seam, protocol 2
//! (contract §4.5, §10.4; T08: T-sidecar-protocol, T-sidecar-death, the
//! `registerSpawn` allowlist, lazy start).
//!
//! Everything here runs against a **real process**: either the real bundle
//! (`sidecar/sidecar.cjs`, with its self-test hooks) or a scripted stand-in that
//! misbehaves on purpose — the real sidecar hardens its stdout before any import,
//! so it cannot emit garbage, oversize lines, unknown fields or a protocol-1
//! hello. No vendor CLI is ever offered to the sidecar: the binary resolver
//! answers `None` for every vendor (the allowlist test offers the `fake_agent`
//! fixture as the "claude" binary), and every process runs with a temp `HOME`.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gilde_code_lib::config::ConfigStore;
use gilde_code_lib::contract::common::{AbsPath, Vendor};
use gilde_code_lib::contract::error::ErrorCode;
use gilde_code_lib::contract::ids::{AccountId, SessionUid};
use gilde_code_lib::contract::services::{SidecarApi, TurnEventSink};
use gilde_code_lib::contract::sidecar::{TurnDoneStatus, TurnEvent, TurnStartParams};
use gilde_code_lib::sidecar::{
    AdapterInfo, AuthStatus, GenerationSweeper, MeshConfig, ModelInfo, QuotaSnapshot, RunResult,
    resolver_from, SidecarCallError, SidecarManager, SidecarRuntime, SidecarState, HELPER_RESTARTED,
};
use gilde_code_lib::store::models::{ProcKind, ProcState};
use gilde_code_lib::store::{dao, Db};
use gilde_code_lib::supervise::registry::{EnvBase, Supervisor};
use gilde_code_lib::supervise::spawn::is_alive;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

fn repo_sidecar_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("sidecar")
}

fn real_bundle() -> PathBuf {
    repo_sidecar_root().join("sidecar.cjs")
}

/// A `node` to run the bundle with: the vendored `gilde-node` when it is in the
/// tree, else the developer's `PATH`.
fn node_binary() -> PathBuf {
    let vendored = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("binaries")
        .join("gilde-node-aarch64-apple-darwin");
    if vendored.is_file() {
        return vendored;
    }
    which::which("node").expect("a node runtime is required for the sidecar tests")
}

fn fake_agent() -> AbsPath {
    let path = std::fs::canonicalize(env!("CARGO_BIN_EXE_fake_agent")).expect("fake_agent");
    AbsPath::parse(path.display().to_string()).expect("abs path")
}

/// Offer the fixture agent as the "claude" binary: the sidecar's own allowlist
/// (configured binaries) and the §9.3 allowlist for `Turn` children — what
/// `agents::resolve_binary` does for a resolved vendor binary in the app.
fn offer_fake_agent_as_claude(manager: &Arc<SidecarManager>) -> AbsPath {
    let agent = fake_agent();
    gilde_code_lib::supervise::spawn::allow_executable(
        Path::new(agent.as_str()),
        &[gilde_code_lib::contract::process::ProcKind::Turn],
    );
    let offered = agent.clone();
    manager.set_binary_resolver(resolver_from(move |v| (v == Vendor::Claude).then(|| offered.clone())));
    agent
}

struct Harness {
    dir: tempfile::TempDir,
    db: Db,
    supervisor: Arc<Supervisor>,
}

impl Harness {
    async fn boot() -> Harness {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("home")).expect("home");
        let db = Db::open(&dir.path().join("state.db")).expect("db");
        let config =
            Arc::new(ConfigStore::open_at(&dir.path().join("config.json")).expect("config"));
        // The sidecar's self-test hooks are gated on an env var, and
        // `compose_env` is an allowlist: without this the child never sees it.
        config
            .patch(json!({"agents": {"envPassthrough": ["GILDE_SIDECAR_SELFTEST"]}}))
            .expect("patch");
        let supervisor = Arc::new(
            Supervisor::open_instance(&db, config, "0.0.0-test")
                .await
                .expect("open_instance"),
        );
        Harness { dir, db, supervisor }
    }

    /// A manager on `script` with a temp `HOME` and no vendor binary at all.
    fn manager(&self, script: PathBuf) -> Arc<SidecarManager> {
        let manager = SidecarManager::new(
            self.supervisor.clone(),
            SidecarRuntime::explicit(node_binary(), script),
            EnvBase::new("/usr/bin:/bin", self.dir.path().join("home")),
            MeshConfig::default(),
            // Fast pings: the health loop is not what most tests measure.
            Duration::from_secs(2),
        );
        manager.set_binary_resolver(resolver_from(|_| None));
        manager
    }

    async fn live_rows(&self, generation: i64) -> Vec<gilde_code_lib::store::models::ProcRow> {
        self.db
            .read(move |conn| dao::proc::live_rows_of_generation(conn, generation))
            .await
            .expect("live rows")
    }

    async fn rows(&self, generation: i64) -> Vec<gilde_code_lib::store::models::ProcRow> {
        self.db
            .read(move |conn| dao::proc::rows_of_generation(conn, generation))
            .await
            .expect("rows")
    }

    async fn sidecar_pid(&self, generation: i64) -> i32 {
        for _ in 0..200 {
            if let Some(pid) = self
                .live_rows(generation)
                .await
                .iter()
                .find(|r| r.kind == ProcKind::Sidecar)
                .and_then(|r| r.pid)
            {
                return pid as i32;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("no sidecar row for generation {generation}");
    }
}

/// The reaper hook (T11's in the app): stop every live row of the dead
/// generation — children this test started — and close it.
struct SweepingHook {
    db: Db,
    swept_generation: Arc<AtomicI64>,
    swept_at: Arc<parking_lot::Mutex<Option<Instant>>>,
}

impl GenerationSweeper for SweepingHook {
    fn sweep(&self, dead_generation: i64) -> Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        let db = self.db.clone();
        let swept = self.swept_generation.clone();
        let at = self.swept_at.clone();
        Box::pin(async move {
            let rows = db
                .read(move |conn| dao::proc::live_rows_of_generation(conn, dead_generation))
                .await
                .unwrap_or_default();
            for row in rows {
                if let Some(pid) = row.pid.filter(|p| *p > 1) {
                    // Only ever a group this test created (pgid == pid).
                    let _ = nix::sys::signal::killpg(
                        nix::unistd::Pid::from_raw(pid as i32),
                        nix::sys::signal::Signal::SIGTERM,
                    );
                }
                let spawn_id = row.spawn_id.clone();
                let _ = db
                    .write(move |conn| {
                        dao::proc::mark_state(conn, &spawn_id, ProcState::Lost, Some("generation sweep"))?;
                        Ok(())
                    })
                    .await;
            }
            *at.lock() = Some(Instant::now());
            swept.store(dead_generation, Ordering::SeqCst);
        })
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

/// What a scripted stand-in does at each step (JavaScript snippets; `msg` is
/// the request, `send` writes one line).
#[derive(Default)]
struct Fake {
    preamble: String,
    on_hello: Option<String>,
    after_hello: String,
    on_turn_start: String,
    deaf_to_pings: bool,
}

const ANSWER_HELLO: &str = "send({ jsonrpc: '2.0', id: msg.id, result: { protocol: 2, buildId: msg.params.buildId, adapters: [] } });";

/// Write a scripted protocol-2 stand-in sidecar.
fn fake_sidecar(dir: &Path, name: &str, fake: Fake) -> PathBuf {
    let path = dir.join(name);
    let on_hello = fake.on_hello.unwrap_or_else(|| ANSWER_HELLO.to_string());
    let deaf = if fake.deaf_to_pings { "true" } else { "false" };
    let source = format!(
        r#"
const readline = require('node:readline');
function send(obj) {{ process.stdout.write(JSON.stringify(obj) + "\n"); }}
{preamble}
const deafToPings = {deaf};
const rl = readline.createInterface({{ input: process.stdin, crlfDelay: Infinity }});
rl.on('line', (line) => {{
  let msg;
  try {{ msg = JSON.parse(line); }} catch {{ return; }}
  if (msg.method === 'mesh.hello') {{
    {on_hello}
    {after_hello}
    return;
  }}
  if (msg.method === 'mesh.configure') {{ send({{ jsonrpc: '2.0', id: msg.id, result: {{}} }}); return; }}
  if (msg.method === 'mesh.ping') {{
    if (deafToPings) return;
    send({{ jsonrpc: '2.0', id: msg.id, result: {{ nonce: (msg.params || {{}}).nonce ?? null, ts: Date.now() }} }});
    return;
  }}
  if (msg.method === 'turn.start') {{
    send({{ jsonrpc: '2.0', id: msg.id, result: {{ accepted: true }} }});
    const turnId = msg.params.turnId;
    {on_turn_start}
    return;
  }}
  if (msg.method === 'turn.cancel') {{ send({{ jsonrpc: '2.0', id: msg.id, result: {{ signalled: false }} }}); return; }}
  if (msg.method === 'mesh.shutdown') {{
    send({{ jsonrpc: '2.0', id: msg.id, result: {{}} }});
    setTimeout(() => process.exit(0), 10);
    return;
  }}
  if (msg.id !== undefined && msg.method) {{
    send({{ jsonrpc: '2.0', id: msg.id, error: {{ code: -32601, message: 'method not found' }} }});
  }}
}});

// A fixture must not outlive the test that started it.
let goingDown = false;
function bail() {{
  if (goingDown) return;
  goingDown = true;
  process.exit(0);
}}
process.stdin.on('end', bail);
process.stdin.on('close', bail);
process.stdin.on('error', bail);
for (const sig of ['SIGTERM', 'SIGINT', 'SIGHUP']) process.on(sig, bail);
const bornUnder = process.ppid;
const parentWatch = setInterval(() => {{
  if (process.ppid === 1 || process.ppid !== bornUnder) bail();
}}, 1000);
parentWatch.unref();
setInterval(() => {{}}, 1000);
"#,
        preamble = fake.preamble,
        after_hello = fake.after_hello,
        on_turn_start = fake.on_turn_start,
    );
    std::fs::write(&path, source).expect("write the stand-in sidecar");
    path
}

/// `ensure_started` until ready (a lazy start answers `NOT_READY` while the
/// machine is slow; that is the contract, not a failure).
async fn started(manager: &Arc<SidecarManager>) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match manager.ensure_started().await {
            Ok(()) => return,
            Err(err) if err.code == ErrorCode::NotReady && Instant::now() < deadline => continue,
            Err(err) => panic!("the sidecar did not start: {err:?}"),
        }
    }
}

/// Records a turn's events.
#[derive(Default)]
struct Recorder(parking_lot::Mutex<Vec<(u64, TurnEvent)>>);

impl TurnEventSink for Recorder {
    fn event(&self, seq: u64, event: TurnEvent) {
        self.0.lock().push((seq, event));
    }
}

impl Recorder {
    fn events(&self) -> Vec<(u64, TurnEvent)> {
        self.0.lock().clone()
    }
}

fn turn_params(turn_id: &str) -> TurnStartParams {
    TurnStartParams {
        turn_id: turn_id.to_string(),
        session_uid: SessionUid::mint(),
        vendor: Vendor::Codex,
        account_id: AccountId::derive_default_login(Vendor::Codex),
        cwd: AbsPath::parse("/tmp").expect("abs"),
        model: None,
        permission_mode: None,
        prompt: "fixture".to_string(),
        resume_id: None,
        timeout_ms: None,
    }
}

// ---------------------------------------------------------------------------
// acceptance 1 — the type mirror is lossless against real vendor JSON
// ---------------------------------------------------------------------------

fn fixture(name: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/mesh")
        .join(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("fixture {} is missing: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("fixture {name} is not JSON: {e}"))
}

/// Decode into `T`, encode again, and require the JSON to be identical.
///
/// This is the test that keeps `types.rs` honest: every optional defaulted,
/// every unmodelled vendor field carried through, and `conversationId: ""`
/// preserved rather than normalised into `null`.
fn round_trip<T: serde::Serialize + serde::de::DeserializeOwned>(label: &str, value: &Value) {
    let decoded: T = serde_json::from_value(value.clone())
        .unwrap_or_else(|e| panic!("{label} does not decode: {e}"));
    let encoded = serde_json::to_value(&decoded).expect("encode");
    assert!(
        same_json(&encoded, value),
        "{label} lost or invented a field\n  encoded: {encoded}\n  original: {value}"
    );
}

/// JSON equality that treats `14` and `14.0` as the same number.
///
/// A percentage typed as `f64` in Rust comes back out with a decimal point; the
/// wire value is unchanged in every sense that matters to a consumer, and
/// demanding byte identity here would only push `usedPercent` back to a type
/// that cannot hold `12.5`.
fn same_json(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => match (x.as_f64(), y.as_f64()) {
            (Some(x), Some(y)) => x == y,
            _ => x == y,
        },
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(x, y)| same_json(x, y))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).map(|w| same_json(v, w)).unwrap_or(false))
        }
        _ => a == b,
    }
}

#[test]
fn vendor_json_survives_a_round_trip_through_the_type_mirror() {
    for vendor in ["claude", "codex", "cursor", "gemini"] {
        let doc = fixture(&format!("{vendor}.json"));

        round_trip::<AdapterInfo>(&format!("{vendor}/adapter"), &doc["adapter"]);
        round_trip::<AuthStatus>(&format!("{vendor}/authStatus"), &doc["authStatus"]);
        for (i, model) in doc["models"]
            .as_array()
            .expect("models is an array")
            .iter()
            .enumerate()
        {
            round_trip::<ModelInfo>(&format!("{vendor}/models[{i}]"), model);
        }
        round_trip::<RunResult>(&format!("{vendor}/runResult"), &doc["runResult"]);

        // A `null` quota is "headroom unknown", never "no headroom" — the two
        // vendors whose `capabilities.quota` is false always answer that way,
        // and a vendor that *can* report one still may not for a given login.
        match &doc["quota"] {
            Value::Null => {}
            quota => round_trip::<QuotaSnapshot>(&format!("{vendor}/quota"), quota),
        }

        // Some fixtures carry a real vendor failure as well.
        if let Some(failed) = doc.get("errorRunResult").filter(|v| !v.is_null()) {
            round_trip::<RunResult>(&format!("{vendor}/errorRunResult"), failed);
        }

        for (i, event) in doc["streamEvents"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .enumerate()
        {
            round_trip::<gilde_code_lib::sidecar::RunEvent>(
                &format!("{vendor}/streamEvents[{i}]"),
                event,
            );
        }
    }
}

#[test]
fn an_unresumable_run_keeps_its_empty_conversation_id() {
    let doc = fixture("codex.json");
    let unresumable: RunResult =
        serde_json::from_value(doc["unresumableRunResult"].clone()).expect("decode");
    assert_eq!(unresumable.conversation_id, "");
    assert!(
        !unresumable.is_resumable(),
        "an empty conversationId means the run cannot be resumed"
    );
}

// ---------------------------------------------------------------------------
// the registerSpawn allowlist; write-before-spawn reaches the sidecar's children
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn a_turn_child_is_registered_before_it_spawns_and_visible_to_one_scan() {
    std::env::set_var("GILDE_SIDECAR_SELFTEST", "1");
    let h = Harness::boot().await;
    let manager = h.manager(real_bundle());
    // The fixture is offered as the "claude" binary: both allowlists admit it.
    let agent = offer_fake_agent_as_claude(&manager);
    manager.start().await.expect("the sidecar starts");
    let generation = manager.generation();

    // `dev.exec` drives the registered exec seam directly — the same seam
    // `turn.start` uses — so the ordering it proves is the production one.
    let running = tokio::spawn({
        let manager = manager.clone();
        let agent = agent.as_str().to_string();
        async move {
            manager
                .call_detailed::<_, Value>(
                    "dev.exec",
                    json!({
                        "cmd": agent,
                        "args": ["hang"],
                        "cwd": "/tmp",
                        "timeoutMs": 20000,
                        "turnId": "tn_scan",
                        "vendor": "claude",
                    }),
                    None,
                )
                .await
        }
    });

    let mut child_pid = None;
    for _ in 0..200 {
        let rows = h.live_rows(generation).await;
        if let Some(row) = rows
            .iter()
            .find(|r| r.kind == ProcKind::Turn && r.state == ProcState::Running)
        {
            child_pid = row.pid.map(|p| p as i32);
            assert_eq!(row.run_id.as_deref(), Some("tn_scan"));
            assert_eq!(row.generation, generation, "the dead-generation sweep finds it (A13.1)");
            assert!(row.argv.contains("fake_agent"), "the row records the planned argv: {}", row.argv);
            assert!(row.proc_start_ts.is_some(), "the kernel start time is recorded");
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let child_pid = child_pid.expect("a running turn row within 10s");
    assert!(is_alive(child_pid), "and the pid is a live process");

    let snapshot = gilde_code_lib::supervise::ps_snapshot().expect("a ps snapshot");
    assert!(
        snapshot.rows().iter().any(|r| r.pid == child_pid),
        "the child is in a Process Monitor scan while it runs"
    );

    // Our own fixture child: SIGTERM its group; the seam reports the exit.
    let _ = nix::sys::signal::killpg(
        nix::unistd::Pid::from_raw(child_pid),
        nix::sys::signal::Signal::SIGTERM,
    );
    let _ = tokio::time::timeout(Duration::from_secs(20), running).await;
    let rows = h.rows(generation).await;
    let child = rows.iter().find(|r| r.kind == ProcKind::Turn).expect("the turn row");
    assert!(
        matches!(child.state, ProcState::Exited | ProcState::Running | ProcState::Lost),
        "the row settles through `host.exited` or the sweep, never staying `planned`"
    );

    manager.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn a_spawn_outside_the_allowlist_is_refused_before_any_row_or_process() {
    std::env::set_var("GILDE_SIDECAR_SELFTEST", "1");
    let h = Harness::boot().await;
    let manager = h.manager(real_bundle()); // no vendor binary configured
    manager.start().await.expect("the sidecar starts");
    let generation = manager.generation();

    let err = manager
        .call_detailed::<_, Value>(
            "dev.exec",
            json!({
                "cmd": fake_agent().as_str(),
                "args": ["hang"],
                "cwd": "/tmp",
                "timeoutMs": 5000,
                "turnId": "tn_refused",
                "vendor": "claude",
            }),
            Some(Duration::from_secs(20)),
        )
        .await
        .expect_err("not allowlisted");
    match &err {
        SidecarCallError::Rpc(rpc) => assert!(
            rpc.message.contains("refused"),
            "the sidecar reports the host's refusal: {rpc:?}"
        ),
        other => panic!("expected an RPC error, got {other:?}"),
    }
    let rows = h.rows(generation).await;
    assert!(
        !rows.iter().any(|r| r.argv.contains("fake_agent")),
        "a refused spawn never becomes a row: {rows:?}"
    );
    assert_eq!(manager.state(), SidecarState::Ready, "and the sidecar is fine");

    manager.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// lazy start (B12)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn the_sidecar_starts_lazily_and_answers_not_ready_while_starting() {
    let h = Harness::boot().await;
    let script = fake_sidecar(
        h.dir.path(),
        "slow.cjs",
        Fake {
            // The hello answer takes longer than a command's 3 s wait.
            on_hello: Some(format!("setTimeout(() => {{ {ANSWER_HELLO} }}, 4500);")),
            ..Fake::default()
        },
    );
    let manager = h.manager(script);
    assert_eq!(manager.state(), SidecarState::Down);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(h.rows(1).await.is_empty(), "nothing is spawned before the first need");

    let err = manager.ensure_started().await.expect_err("still starting");
    assert_eq!(err.code, ErrorCode::NotReady);
    assert_eq!(err.params.get("reason").and_then(|v| v.as_str()), Some("sidecarStarting"));
    assert!(
        wait_until(Duration::from_secs(20), || manager.state() == SidecarState::Ready).await,
        "the started sidecar becomes ready"
    );
    manager.ensure_started().await.expect("ready now");

    manager.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// T-sidecar-protocol
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn t_sidecar_protocol_a_protocol_1_peer_is_stopped() {
    let h = Harness::boot().await;
    let script = fake_sidecar(
        h.dir.path(),
        "protocol1.cjs",
        Fake {
            on_hello: Some(
                "send({ jsonrpc: '2.0', id: msg.id, result: { protocol: 1, buildId: msg.params.buildId, adapters: [] } });"
                    .to_string(),
            ),
            ..Fake::default()
        },
    );
    let manager = h.manager(script);
    let err = manager.start().await.expect_err("protocol 1 is refused");
    assert!(err.to_string().contains("protocol"), "{err}");
    assert_eq!(manager.state(), SidecarState::Down);
    let generation = manager.generation();
    let pid = h
        .rows(generation)
        .await
        .iter()
        .find(|r| r.kind == ProcKind::Sidecar)
        .and_then(|r| r.pid)
        .expect("the peer ran") as i32;
    assert!(wait_until(Duration::from_secs(10), || !is_alive(pid)).await, "and was stopped");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(manager.generation(), generation, "no restart loop");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn t_sidecar_protocol_a_build_mismatch_is_version_mismatch_and_never_restarted() {
    let h = Harness::boot().await;

    // (1) The sidecar refuses the host's buildId and exits 78, as the real one does.
    let refusing = fake_sidecar(
        h.dir.path(),
        "refusing.cjs",
        Fake {
            on_hello: Some(
                "send({ jsonrpc: '2.0', id: msg.id, error: { code: -32005, message: 'build mismatch', data: { expected: 'other-build' } } }); setTimeout(() => process.exit(78), 20);"
                    .to_string(),
            ),
            ..Fake::default()
        },
    );
    // (2) The sidecar answers with another buildId; the host refuses it.
    let other = fake_sidecar(
        h.dir.path(),
        "other.cjs",
        Fake {
            on_hello: Some(
                "send({ jsonrpc: '2.0', id: msg.id, result: { protocol: 2, buildId: 'other-build', adapters: [] } });"
                    .to_string(),
            ),
            ..Fake::default()
        },
    );
    for script in [refusing, other] {
        let manager = h.manager(script.clone());
        manager.start().await.expect_err("a build mismatch fails the start");
        let generation = manager.generation();
        assert!(manager.current_status().version_mismatch, "{}", script.display());
        let err = manager.ensure_started().await.expect_err("latched");
        assert_eq!(err.code, ErrorCode::VersionMismatch);
        let err = manager.turn_start(turn_params("tn_mismatch"), Arc::new(Recorder::default())).await.expect_err("latched");
        assert_eq!(err.code, ErrorCode::VersionMismatch);
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(manager.generation(), generation, "never restarted: only a new Gilde fixes it");
        let pid = h
            .rows(generation)
            .await
            .iter()
            .find(|r| r.kind == ProcKind::Sidecar)
            .and_then(|r| r.pid)
            .expect("the peer ran") as i32;
        assert!(wait_until(Duration::from_secs(10), || !is_alive(pid)).await, "the peer is gone");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn t_sidecar_protocol_unknown_fields_and_notifications_are_dropped() {
    let h = Harness::boot().await;
    let script = fake_sidecar(
        h.dir.path(),
        "unknown.cjs",
        Fake {
            on_turn_start: r#"
    const ev = (seq, event, extra) => send(Object.assign({ jsonrpc: '2.0', method: 'turn.event', params: { turnId, seq, event } }, extra || {}));
    ev(1, { kind: 'text', delta: 'a', extra: 1 });
    send({ jsonrpc: '2.0', method: 'mesh.bogus', params: {} });
    send({ jsonrpc: '2.0', method: 'turn.event', params: { turnId, seq: 2, event: { kind: 'text', delta: 'b' }, extra: true } });
    ev(3, { kind: 'text', delta: 'c' });
    ev(4, { kind: 'done', status: 'ok', conversationId: 'conv', usage: null, costUsd: null });
"#
            .to_string(),
            ..Fake::default()
        },
    );
    let manager = h.manager(script);
    manager.start().await.expect("starts");
    let generation = manager.generation();
    let sink = Arc::new(Recorder::default());
    manager.turn_start(turn_params("tn_unknown"), sink.clone()).await.expect("turn");
    assert!(wait_until(Duration::from_secs(10), || sink.events().len() == 2).await, "{:?}", sink.events());
    tokio::time::sleep(Duration::from_millis(300)).await;
    let events = sink.events();
    assert_eq!(
        events,
        vec![
            (3, TurnEvent::Text { delta: "c".into() }),
            (
                4,
                TurnEvent::Done {
                    status: TurnDoneStatus::Ok,
                    conversation_id: "conv".into(),
                    usage: None,
                    cost_usd: None
                }
            ),
        ],
        "the events with unknown fields never reach the turn"
    );
    assert_eq!(manager.state(), SidecarState::Ready);
    assert_eq!(manager.generation(), generation, "an unknown notification costs a log line, not the generation");
    manager.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn t_sidecar_protocol_garbage_lines_are_logged_and_skipped() {
    let h = Harness::boot().await;
    let script = fake_sidecar(
        h.dir.path(),
        "garbage.cjs",
        Fake {
            after_hello: r#"
    process.stdout.write("this is not json at all\n");
    process.stdout.write(JSON.stringify({ hello: 'world' }) + "\n");
    process.stdout.write(JSON.stringify({ jsonrpc: '2.0' }) + "\n");
"#
            .to_string(),
            ..Fake::default()
        },
    );
    let manager = h.manager(script);
    manager.start().await.expect("the handshake survives garbage");
    for _ in 0..3 {
        let pong: Value = manager
            .call_detailed("mesh.ping", json!({"nonce": 1}), Some(Duration::from_secs(5)))
            .await
            .expect("ping answered after the garbage");
        assert!(pong.get("ts").is_some());
    }
    assert_eq!(manager.state(), SidecarState::Ready);
    manager.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn t_sidecar_protocol_a_line_over_the_core_cap_restarts_the_sidecar() {
    let h = Harness::boot().await;
    let marker = h.dir.path().join("blasted.once");
    // 5 MiB: over the core's 4 MiB inbound cap. Only the FIRST generation does
    // it, so the restart has something healthy to come back as.
    let script = fake_sidecar(
        h.dir.path(),
        "oversize.cjs",
        Fake {
            preamble: format!("const fs = require('node:fs'); const marker = {:?};", marker.display().to_string()),
            after_hello: r#"
    if (!fs.existsSync(marker)) {
      fs.writeFileSync(marker, 'x');
      setTimeout(() => {
        process.stdout.write('{"jsonrpc":"2.0","method":"host.log","params":"');
        const blob = 'x'.repeat(1024 * 1024);
        for (let i = 0; i < 5; i += 1) process.stdout.write(blob);
        process.stdout.write('"}\n');
      }, 50);
    }
"#
            .to_string(),
            ..Fake::default()
        },
    );
    let manager = h.manager(script);
    manager.start().await.expect("the sidecar starts");
    let first_generation = manager.generation();
    assert!(
        wait_until(Duration::from_secs(60), || manager.generation() > first_generation).await,
        "the oversize line forced a restart"
    );
    assert!(
        wait_until(Duration::from_secs(60), || manager.state() == SidecarState::Ready).await,
        "and the replacement generation is ready"
    );
    let pong: Value = manager
        .call_detailed("mesh.ping", json!({"nonce": 2}), Some(Duration::from_secs(5)))
        .await
        .expect("the restarted sidecar answers");
    assert!(pong.get("ts").is_some());
    let status = manager.current_status();
    assert!(status.restarts >= 1, "the restart is counted: {status:?}");
    assert!(
        status.last_error.as_deref().unwrap_or_default().contains("byte line"),
        "and the reason is recorded: {status:?}"
    );
    manager.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// T-sidecar-death
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn t_sidecar_death_mid_turn_errors_the_turn_and_leaves_other_processes_alone() {
    let h = Harness::boot().await;
    let script = fake_sidecar(
        h.dir.path(),
        "dies.cjs",
        Fake {
            on_turn_start: "send({ jsonrpc: '2.0', method: 'turn.event', params: { turnId, seq: 1, event: { kind: 'session', conversationId: 'conv-1' } } });".to_string(),
            ..Fake::default()
        },
    );
    let manager = h.manager(script);
    manager.start().await.expect("starts");
    let dead_generation = manager.generation();

    // A process outside the sidecar's generation (a Terminal / L1 session's
    // stand-in), started by this test in its own group.
    use std::os::unix::process::CommandExt as _;
    let mut bystander = std::process::Command::new(env!("CARGO_BIN_EXE_fake_agent"))
        .arg("hang")
        .process_group(0)
        .spawn()
        .expect("bystander");

    let sink = Arc::new(Recorder::default());
    manager.turn_start(turn_params("tn_death"), sink.clone()).await.expect("turn");
    assert!(wait_until(Duration::from_secs(10), || !sink.events().is_empty()).await, "the session event");

    // The sidecar dies (this test's own fixture process).
    let pid = h.sidecar_pid(dead_generation).await;
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), nix::sys::signal::Signal::SIGKILL).expect("kill the fixture");

    assert!(wait_until(Duration::from_secs(15), || sink.events().len() == 2).await, "{:?}", sink.events());
    let (_, last) = sink.events().pop().expect("terminal");
    match last {
        TurnEvent::Error { message, .. } => assert_eq!(message, HELPER_RESTARTED),
        other => panic!("expected the helper-restarted error, got {other:?}"),
    }
    assert!(wait_until(Duration::from_secs(30), || manager.generation() > dead_generation).await, "it restarts");
    assert!(wait_until(Duration::from_secs(30), || manager.state() == SidecarState::Ready).await);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(sink.events().len(), 2, "exactly one terminal event");
    assert!(bystander.try_wait().expect("wait").is_none(), "the other session's process is untouched");

    // The next turn works on the new generation.
    let next = Arc::new(Recorder::default());
    manager.turn_start(turn_params("tn_after"), next.clone()).await.expect("a new turn");
    assert!(wait_until(Duration::from_secs(10), || !next.events().is_empty()).await);

    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(bystander.id() as i32),
        nix::sys::signal::Signal::SIGTERM,
    );
    let _ = bystander.wait();
    manager.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// SIGKILL under an in-flight request: the sweep stops the orphan first
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn a_sigkilled_sidecar_rejects_in_flight_work_and_the_sweep_stops_its_orphan() {
    std::env::set_var("GILDE_SIDECAR_SELFTEST", "1");
    let h = Harness::boot().await;
    let manager = h.manager(real_bundle());
    let agent = offer_fake_agent_as_claude(&manager);

    let swept_generation = Arc::new(AtomicI64::new(0));
    let swept_at = Arc::new(parking_lot::Mutex::new(None));
    manager.set_sweeper(Arc::new(SweepingHook {
        db: h.db.clone(),
        swept_generation: swept_generation.clone(),
        swept_at: swept_at.clone(),
    }));

    manager.start().await.expect("the sidecar starts");
    let dead_generation = manager.generation();

    let in_flight = tokio::spawn({
        let manager = manager.clone();
        let agent = agent.as_str().to_string();
        async move {
            manager
                .call_detailed::<_, Value>(
                    "dev.exec",
                    json!({
                        "cmd": agent,
                        "args": ["hang"],
                        "cwd": "/tmp",
                        "timeoutMs": 120000,
                        "turnId": "tn_kill",
                        "vendor": "claude",
                    }),
                    None,
                )
                .await
        }
    });

    let sidecar_pid = h.sidecar_pid(dead_generation).await;
    let mut child_pid = None;
    for _ in 0..200 {
        child_pid = h
            .live_rows(dead_generation)
            .await
            .iter()
            .find(|r| r.kind == ProcKind::Turn && r.state == ProcState::Running)
            .and_then(|r| r.pid)
            .map(|p| p as i32);
        if child_pid.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let child_pid = child_pid.expect("a running turn child");

    // SIGKILL the sidecar (this test's own node process) — the child is in its
    // own group, so it is deliberately left orphaned.
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(sidecar_pid), nix::sys::signal::Signal::SIGKILL)
        .expect("kill the sidecar");

    let outcome = tokio::time::timeout(Duration::from_secs(20), in_flight)
        .await
        .expect("the call settles within 20s")
        .expect("join");
    match outcome {
        Err(SidecarCallError::Gone { generation }) => assert_eq!(generation, dead_generation),
        other => panic!("expected SidecarGone, got {other:?}"),
    }
    assert!(
        wait_until(Duration::from_secs(15), || swept_generation.load(Ordering::SeqCst) == dead_generation).await,
        "the generation sweep ran"
    );
    assert!(
        wait_until(Duration::from_secs(15), || !is_alive(child_pid)).await,
        "the orphaned child was stopped by the sweep"
    );
    let live = h.live_rows(dead_generation).await;
    assert!(live.is_empty(), "no live rows may remain in a swept generation: {live:?}");
    assert!(
        wait_until(Duration::from_secs(30), || manager.generation() > dead_generation).await,
        "the sidecar restarted"
    );
    let replacement_seen = Instant::now();
    let swept_at = (*swept_at.lock()).expect("the sweep recorded a time");
    assert!(swept_at < replacement_seen, "the sweep completed before the replacement existed");
    assert!(
        wait_until(Duration::from_secs(30), || manager.state() == SidecarState::Ready).await,
        "and the replacement generation reached ready"
    );
    manager.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// the runtime self-test, the health loop, status transitions
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn an_unusable_runtime_is_a_typed_error_not_a_hang() {
    let h = Harness::boot().await;
    let bogus = h.dir.path().join("not-node");
    std::fs::write(&bogus, "#!/bin/sh\nexit 3\n").expect("write");
    std::fs::set_permissions(&bogus, std::os::unix::fs::PermissionsExt::from_mode(0o755)).expect("chmod");

    let manager = SidecarManager::new(
        h.supervisor.clone(),
        SidecarRuntime::explicit(bogus.clone(), real_bundle()),
        EnvBase::new("/usr/bin:/bin", h.dir.path().join("home")),
        MeshConfig::default(),
        Duration::from_secs(2),
    );
    manager.set_binary_resolver(resolver_from(|_| None));

    let started = Instant::now();
    let err = manager.start().await.expect_err("an unusable runtime must fail");
    assert!(started.elapsed() < Duration::from_secs(10), "the self-test is capped at 5s");
    match err {
        gilde_code_lib::error::GildeError::SidecarRuntimeUnusable { path, .. } => {
            assert_eq!(path, bogus.display().to_string());
        }
        other => panic!("expected SidecarRuntimeUnusable, got {other:?}"),
    }
    let status = manager.current_status();
    assert_eq!(status.state, SidecarState::Down, "a failed start is down, never stuck starting");
    assert!(status.last_error.is_some(), "with the reason: {status:?}");

    // Commands answer at once instead of waiting for a start that cannot come.
    let err = manager.ensure_started().await.expect_err("down");
    assert_eq!(err.code, ErrorCode::SidecarDown);

    // The probe itself is in the registry: no spawn escapes the guard (a HOST
    // spawn, generation 0).
    let probes = h.rows(0).await;
    assert!(probes.iter().any(|r| r.kind == ProcKind::Probe), "the --version probe was planned: {probes:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn three_missed_pings_kill_a_hung_sidecar_and_restart_it() {
    let h = Harness::boot().await;
    let marker = h.dir.path().join("stopped.answering");
    // Generation 1 answers the handshake and then never answers a ping again;
    // generation 2 behaves.
    let script = fake_sidecar(
        h.dir.path(),
        "deaf.cjs",
        Fake {
            preamble: format!(
                "const fs = require('node:fs'); const marker = {:?}; const firstBorn = !fs.existsSync(marker); if (firstBorn) fs.writeFileSync(marker, 'x');",
                marker.display().to_string()
            ),
            ..Fake::default()
        },
    );
    // The stand-in reads `deafToPings`; make generation 1 deaf.
    let source = std::fs::read_to_string(&script).expect("read").replace(
        "const deafToPings = false;",
        "const deafToPings = firstBorn;",
    );
    std::fs::write(&script, source).expect("write");

    let manager = h.manager(script);
    manager.start().await.expect("the sidecar starts");
    let hung_generation = manager.generation();
    assert!(
        wait_until(Duration::from_secs(90), || manager.generation() > hung_generation).await,
        "a sidecar that stops answering pings is stopped and replaced"
    );
    assert!(
        wait_until(Duration::from_secs(30), || manager.state() == SidecarState::Ready).await,
        "and the replacement is ready"
    );
    let status = manager.current_status();
    assert!(
        status.last_error.as_deref().unwrap_or_default().contains("consecutive pings"),
        "the reason names the missed pings: {status:?}"
    );
    manager.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn lifecycle_transitions_reach_status_subscribers() {
    let h = Harness::boot().await;
    let script = fake_sidecar(h.dir.path(), "quiet.cjs", Fake::default());
    let manager = h.manager(script);
    let mut statuses = manager.subscribe_status();
    manager.start().await.expect("the sidecar starts");
    manager.shutdown().await.expect("shutdown");

    let mut states = Vec::new();
    while let Ok(status) = statuses.try_recv() {
        states.push(status.state);
    }
    assert!(
        states.contains(&SidecarState::Starting) && states.contains(&SidecarState::Ready),
        "subscribers see the transitions: {states:?}"
    );
    assert_eq!(states.last(), Some(&SidecarState::Down), "and the last one is the shutdown: {states:?}");
}

// ---------------------------------------------------------------------------
// A11.6: orchestrator presence → stop when idle, the next start's hello
// ---------------------------------------------------------------------------

/// A stand-in that appends each hello's `providers.orchestrator` to `log`, and
/// ends each turn `turn_ms` after it starts.
fn presence_fake(dir: &Path, name: &str, log: &Path, turn_ms: u64) -> PathBuf {
    fake_sidecar(
        dir,
        name,
        Fake {
            preamble: format!("const fs = require('node:fs'); const helloLog = {:?};", log.display().to_string()),
            on_hello: Some(format!(
                "fs.appendFileSync(helloLog, String(msg.params.providers.orchestrator) + '\\n'); {ANSWER_HELLO}"
            )),
            on_turn_start: format!(
                "setTimeout(() => send({{ jsonrpc: '2.0', method: 'turn.event', params: {{ turnId, seq: 1, event: {{ kind: 'done', status: 'ok', conversationId: 'c', usage: null, costUsd: null }} }} }}), {turn_ms});"
            ),
            ..Fake::default()
        },
    )
}

fn hellos(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn a11_6_a_presence_change_while_idle_reaches_the_next_hello() {
    let h = Harness::boot().await;
    let log = h.dir.path().join("hellos.log");
    let manager = h.manager(presence_fake(h.dir.path(), "presence-idle.cjs", &log, 50));

    // Not running: the value is stored; the orchestrator appearing starts it.
    manager.set_orchestrator_present(true).await;
    assert!(wait_until(Duration::from_secs(20), || manager.state() == SidecarState::Ready).await);
    assert_eq!(hellos(&log), vec!["true"]);
    let first = manager.generation();
    let pid = h.sidecar_pid(first).await;

    // Running and idle: stopped gracefully, not restarted until needed.
    manager.set_orchestrator_present(false).await;
    assert!(wait_until(Duration::from_secs(15), || !is_alive(pid)).await, "the idle sidecar stopped");
    assert!(wait_until(Duration::from_secs(5), || manager.state() == SidecarState::Down).await);
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert_eq!(manager.generation(), first, "no restart: the next need starts it");
    assert!(!manager.current_status().stays_down, "a presence stop is not a death");
    let row = h
        .rows(first)
        .await
        .into_iter()
        .find(|r| r.kind == ProcKind::Sidecar)
        .expect("row");
    assert_ne!(row.exit_signal, Some(i64::from(libc::SIGKILL)), "never SIGKILL: {row:?}");

    started(&manager).await;
    assert_eq!(hellos(&log), vec!["true", "false"], "the next hello carries the new value");
    manager.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn a11_6_a_presence_change_during_a_turn_waits_for_the_turn() {
    let h = Harness::boot().await;
    let log = h.dir.path().join("hellos.log");
    let manager = h.manager(presence_fake(h.dir.path(), "presence-turn.cjs", &log, 2_500));
    manager.start().await.expect("starts");
    let generation = manager.generation();
    let pid = h.sidecar_pid(generation).await;

    let sink = Arc::new(Recorder::default());
    manager.turn_start(turn_params("tn_presence"), sink.clone()).await.expect("turn");
    manager.set_orchestrator_present(true).await;

    // Mid-turn: still running, same generation.
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    assert!(is_alive(pid), "never stopped mid-turn");
    assert!(sink.events().is_empty(), "the turn is still running");

    // The turn completes normally, then the sidecar stops.
    assert!(wait_until(Duration::from_secs(10), || sink.events().len() == 1).await);
    match &sink.events()[0].1 {
        TurnEvent::Done { status, .. } => assert_eq!(*status, TurnDoneStatus::Ok),
        other => panic!("the turn ended normally, got {other:?}"),
    }
    assert!(wait_until(Duration::from_secs(15), || !is_alive(pid)).await, "stopped after the turn");

    // The orchestrator appeared: the replacement starts with the new value.
    assert!(wait_until(Duration::from_secs(20), || hellos(&log).len() == 2).await);
    assert_eq!(hellos(&log), vec!["false", "true"]);
    manager.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn a11_6_the_same_presence_twice_restarts_nothing() {
    let h = Harness::boot().await;
    let log = h.dir.path().join("hellos.log");
    let manager = h.manager(presence_fake(h.dir.path(), "presence-same.cjs", &log, 50));
    manager.start().await.expect("starts");
    let generation = manager.generation();
    let pid = h.sidecar_pid(generation).await;

    manager.set_orchestrator_present(false).await; // the constructor's value
    manager.set_orchestrator_present(false).await;
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert!(is_alive(pid));
    assert_eq!(manager.generation(), generation);
    assert_eq!(manager.state(), SidecarState::Ready);
    assert_eq!(hellos(&log), vec!["false"]);

    // A change and its reversal before the sidecar goes idle restart nothing either.
    let sink = Arc::new(Recorder::default());
    manager.turn_start(turn_params("tn_flip"), sink.clone()).await.expect("turn");
    manager.set_orchestrator_present(true).await;
    manager.set_orchestrator_present(false).await;
    assert!(wait_until(Duration::from_secs(10), || sink.events().len() == 1).await);
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(is_alive(pid), "toggled back before idle: kept");
    assert_eq!(manager.generation(), generation);
    manager.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// A12.6: the binaries are resolved on each start
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn a12_6_every_start_resolves_the_vendor_binaries_again() {
    use std::sync::atomic::AtomicUsize;
    let h = Harness::boot().await;
    let manager = h.manager(fake_sidecar(h.dir.path(), "resolve.cjs", Fake::default()));
    let asked = Arc::new(AtomicUsize::new(0));
    let counter = asked.clone();
    manager.set_binary_resolver(Arc::new(move |_vendor| {
        counter.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { None })
    }));
    assert_eq!(asked.load(Ordering::SeqCst), 0, "nothing at construction (boot starts nothing)");

    started(&manager).await;
    assert_eq!(asked.load(Ordering::SeqCst), 4, "each vendor, once per start");
    manager.shutdown().await.expect("shutdown");

    started(&manager).await;
    assert_eq!(asked.load(Ordering::SeqCst), 8, "resolved afresh, never a cached boot answer");
    manager.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// A12.8 / A13: every spawn goes through SupervisorApi and its §9.3 allowlist
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn a12_8_the_supervisor_allowlist_refuses_a_turn_child_it_never_admitted() {
    use gilde_code_lib::contract::process::ProcKind as Kind;
    use gilde_code_lib::supervise::spawn::is_allowed;
    std::env::set_var("GILDE_SIDECAR_SELFTEST", "1");
    let h = Harness::boot().await;
    let manager = h.manager(real_bundle());

    // A copy of the fixture at its own realpath: the sidecar is configured with
    // it (its own check passes), but nothing admitted it for `Turn` (§9.3).
    let copy = h.dir.path().join("bin").join("unadmitted-agent");
    std::fs::create_dir_all(copy.parent().expect("parent")).expect("mkdir");
    std::fs::copy(env!("CARGO_BIN_EXE_fake_agent"), &copy).expect("copy");
    let copy = AbsPath::parse(std::fs::canonicalize(&copy).expect("canonical").display().to_string()).expect("abs");
    let offered = copy.clone();
    manager.set_binary_resolver(resolver_from(move |v| (v == Vendor::Claude).then(|| offered.clone())));
    manager.start().await.expect("the sidecar starts");
    let generation = manager.generation();

    // The node runs as Sidecar and Probe only (A13.3).
    let node = std::fs::canonicalize(node_binary()).expect("node");
    assert!(is_allowed(&node, Kind::Sidecar) && is_allowed(&node, Kind::Probe));
    assert!(!is_allowed(&node, Kind::Turn), "no other kind is granted to node");

    let err = manager
        .call_detailed::<_, Value>(
            "dev.exec",
            json!({
                "cmd": copy.as_str(),
                "args": ["hang"],
                "cwd": "/tmp",
                "timeoutMs": 5000,
                "turnId": "tn_unadmitted",
                "vendor": "claude",
            }),
            Some(Duration::from_secs(20)),
        )
        .await
        .expect_err("refused by the supervisor");
    match &err {
        SidecarCallError::Rpc(rpc) => assert!(rpc.message.contains("refused"), "{rpc:?}"),
        other => panic!("expected an RPC error, got {other:?}"),
    }
    let rows = h.rows(generation).await;
    assert!(
        !rows.iter().any(|r| r.argv.contains("unadmitted-agent")),
        "a refused spawn never becomes a row: {rows:?}"
    );
    assert_eq!(manager.state(), SidecarState::Ready);
    manager.shutdown().await.expect("shutdown");
}
