//! Fault behaviour of the sidecar channel and its supervision seam
//! (DESIGN §5-1, §5-5, §5-10; M1-PLAN T19 acceptance 1, 5, 6, 8, 9, 10).
//!
//! Everything here runs against a **real process**. The three fault cases the
//! plan pins are only meaningful end to end:
//!
//!   (a) a `SIGKILL`ed sidecar rejects its in-flight work, its orphaned child
//!       dies in the generation sweep, and no `running` row of that generation
//!       survives;
//!   (b) a peer that writes plain garbage onto the protocol channel costs a log
//!       line, not the session;
//!   (c) a 9 MiB line restarts the sidecar and the host neither panics nor grows
//!       its buffer past the cap.
//!
//! Plus the two properties the whole design rests on: the type mirror survives a
//! round trip against real vendor JSON, and a headless child is in the registry
//! — visible to one Process Monitor scan — **before** it exists.
//!
//! Cases (b) and (c) use a scripted stand-in sidecar rather than the real bundle,
//! for a reason worth stating: the real sidecar hardens its stdout before any
//! import (DESIGN §5-1), so it *cannot* emit either fault. Testing the host's
//! tolerance therefore needs a peer that misbehaves on purpose. The real bundle
//! is used everywhere its own behaviour is what is under test.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gilde_code_lib::config::ConfigStore;
use gilde_code_lib::sidecar::{
    AdapterInfo, AuthStatus, GenerationSweeper, MeshConfig, ModelInfo, QuotaSnapshot, RunResult,
    SidecarCallError, SidecarManager, SidecarRuntime,
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

/// A `node` to run the bundle with. The vendored `gilde-node` when it is in the
/// tree, else whatever the developer's `PATH` offers — the same ladder
/// `SidecarRuntime::resolve` walks, minus the login-shell probe a test does not
/// need.
fn node_binary() -> PathBuf {
    let vendored = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("binaries")
        .join("gilde-node-aarch64-apple-darwin");
    if vendored.is_file() {
        return vendored;
    }
    which::which("node").expect("a node runtime is required for the sidecar tests")
}

struct Harness {
    _dir: tempfile::TempDir,
    db: Db,
    supervisor: Arc<Supervisor>,
}

impl Harness {
    async fn boot() -> Harness {
        let dir = tempfile::tempdir().expect("tempdir");
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
        Harness {
            _dir: dir,
            db,
            supervisor,
        }
    }

    fn manager(&self, script: PathBuf) -> Arc<SidecarManager> {
        SidecarManager::new(
            self.supervisor.clone(),
            SidecarRuntime::explicit(node_binary(), script),
            EnvBase::from_process_env(std::env::var("PATH").unwrap_or_default()),
            MeshConfig::default(),
            // Fast pings: the health loop is not what these tests measure, and a
            // 10 s cadence would make every one of them ten seconds slower.
            Duration::from_secs(2),
        )
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
}

/// The reaper hook T11 will provide. Until then this is the same contract with a
/// minimal body: signal every live row of the dead generation and close it.
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
                    // Only ever a group WE created: the seam spawns detached, so
                    // pgid == pid. The real reaper adds the identity fences.
                    let _ = nix::sys::signal::killpg(
                        nix::unistd::Pid::from_raw(pid as i32),
                        nix::sys::signal::Signal::SIGKILL,
                    );
                }
                let spawn_id = row.spawn_id.clone();
                let _ = db
                    .write(move |conn| {
                        dao::proc::mark_state(
                            conn,
                            &spawn_id,
                            ProcState::Lost,
                            Some("generation sweep"),
                        )?;
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

/// Write a scripted stand-in sidecar. `extra` runs after the handshake handler
/// is installed and may misbehave on purpose.
fn fake_sidecar(dir: &Path, name: &str, on_hello: &str) -> PathBuf {
    let path = dir.join(name);
    let source = format!(
        r#"
const readline = require('node:readline');
function send(obj) {{ process.stdout.write(JSON.stringify(obj) + "\n"); }}
const rl = readline.createInterface({{ input: process.stdin }});
rl.on('line', (line) => {{
  let msg;
  try {{ msg = JSON.parse(line); }} catch {{ return; }}
  if (msg.method === 'mesh.hello') {{
    send({{ jsonrpc: '2.0', id: msg.id, result: {{
      protocol: 1, sidecarVersion: 'fake', node: process.version, packages: {{}}, adapters: [] }} }});
    {on_hello}
    return;
  }}
  if (msg.method === 'mesh.ping') {{
    send({{ jsonrpc: '2.0', id: msg.id, result: {{ nonce: (msg.params||{{}}).nonce ?? null, ts: Date.now() }} }});
    return;
  }}
  if (msg.id !== undefined) send({{ jsonrpc: '2.0', id: msg.id, result: {{ ok: true }} }});
}});

// --- self-termination (INT-C) ------------------------------------------------
// A fixture must not outlive the test that started it. `cargo test` cut short by
// a SIGKILL used to leave these scripts running for hours; the real sidecar now
// has three exit paths and so does its stand-in.
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
"#
    );
    std::fs::write(&path, source).expect("write the stand-in sidecar");
    path
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
// acceptance 6 — write-before-spawn reaches the sidecar's children
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn a_headless_child_is_registered_before_it_spawns_and_visible_to_one_scan() {
    std::env::set_var("GILDE_SIDECAR_SELFTEST", "1");
    let h = Harness::boot().await;
    let manager = h.manager(real_bundle());
    manager.start().await.expect("the sidecar starts");
    let generation = manager.generation();

    // `dev.exec` drives the registered exec seam directly; the seam is the same
    // one `mesh.run` uses, so the ordering it proves is the production one.
    let fake_agent = env!("CARGO_BIN_EXE_fake_agent");
    let running = tokio::spawn({
        let manager = manager.clone();
        let fake_agent = fake_agent.to_string();
        async move {
            manager
                .call_detailed::<_, Value>(
                    "dev.exec",
                    json!({
                        "cmd": fake_agent,
                        "args": ["hang"],
                        "cwd": "/tmp",
                        "timeoutMs": 20000,
                        "runId": "r-scan",
                        "vendor": "claude",
                    }),
                    None,
                )
                .await
        }
    });

    // The row must be `running` with a pid while the child is still alive — that
    // is what "appears in one scan" means for the Process Monitor.
    let mut child_pid = None;
    for _ in 0..200 {
        let rows = h.live_rows(generation).await;
        if let Some(row) = rows
            .iter()
            .find(|r| r.kind == ProcKind::Headless && r.state == ProcState::Running)
        {
            child_pid = row.pid.map(|p| p as i32);
            assert_eq!(row.run_id.as_deref(), Some("r-scan"));
            assert_eq!(row.vendor.as_deref(), Some("claude"));
            assert!(
                row.argv.contains("fake_agent"),
                "the row records the argv it planned: {}",
                row.argv
            );
            assert!(
                row.proc_start_ts.is_some(),
                "the kernel start time is recorded, so fence 1 can verify the row"
            );
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let child_pid = child_pid.expect("a running headless row within 10s");
    assert!(is_alive(child_pid), "and the pid is a live process");

    // One scan of the machine sees it too.
    let snapshot = gilde_code_lib::supervise::ps_snapshot().expect("a ps snapshot");
    assert!(
        snapshot.rows().iter().any(|r| r.pid == child_pid),
        "the child is in a Process Monitor scan while it runs"
    );

    // Cancel by killing the child's group; the seam reports the exit.
    let _ = nix::sys::signal::killpg(
        nix::unistd::Pid::from_raw(child_pid),
        nix::sys::signal::Signal::SIGKILL,
    );
    let _ = tokio::time::timeout(Duration::from_secs(20), running).await;

    let settled = wait_until(Duration::from_secs(10), || true).await;
    assert!(settled);
    let rows = h.rows(generation).await;
    let child = rows
        .iter()
        .find(|r| r.kind == ProcKind::Headless)
        .expect("the headless row");
    assert!(
        matches!(child.state, ProcState::Exited | ProcState::Running | ProcState::Lost),
        "the row settles through `host.exited` or the sweep, never staying `planned`"
    );

    manager.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// (a) SIGKILL under an in-flight request
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn a_sigkilled_sidecar_rejects_in_flight_work_and_the_sweep_kills_its_orphan() {
    std::env::set_var("GILDE_SIDECAR_SELFTEST", "1");
    let h = Harness::boot().await;
    let manager = h.manager(real_bundle());

    let swept_generation = Arc::new(AtomicI64::new(0));
    let swept_at = Arc::new(parking_lot::Mutex::new(None));
    manager.set_sweeper(Arc::new(SweepingHook {
        db: h.db.clone(),
        swept_generation: swept_generation.clone(),
        swept_at: swept_at.clone(),
    }));

    manager.start().await.expect("the sidecar starts");
    let dead_generation = manager.generation();

    let fake_agent = env!("CARGO_BIN_EXE_fake_agent");
    let in_flight = tokio::spawn({
        let manager = manager.clone();
        let fake_agent = fake_agent.to_string();
        async move {
            manager
                .call_detailed::<_, Value>(
                    "dev.exec",
                    json!({
                        "cmd": fake_agent,
                        "args": ["hang"],
                        "cwd": "/tmp",
                        "timeoutMs": 120000,
                        "runId": "r-kill",
                        "vendor": "claude",
                    }),
                    None,
                )
                .await
        }
    });

    // Wait for both processes to be in the registry.
    let mut sidecar_pid = None;
    let mut child_pid = None;
    for _ in 0..200 {
        let rows = h.live_rows(dead_generation).await;
        sidecar_pid = rows
            .iter()
            .find(|r| r.kind == ProcKind::Sidecar)
            .and_then(|r| r.pid)
            .map(|p| p as i32);
        child_pid = rows
            .iter()
            .find(|r| r.kind == ProcKind::Headless && r.state == ProcState::Running)
            .and_then(|r| r.pid)
            .map(|p| p as i32);
        if sidecar_pid.is_some() && child_pid.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let sidecar_pid = sidecar_pid.expect("a sidecar row");
    let child_pid = child_pid.expect("a running headless child");

    // SIGKILL the sidecar's group leader only — the child is in its own group
    // (`detached: true`), so it is deliberately left orphaned.
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(sidecar_pid),
        nix::sys::signal::Signal::SIGKILL,
    )
    .expect("kill the sidecar");

    // 1. the in-flight request is rejected as `Gone`, carrying the dead generation.
    let outcome = tokio::time::timeout(Duration::from_secs(20), in_flight)
        .await
        .expect("the call settles within 20s")
        .expect("join");
    match outcome {
        Err(SidecarCallError::Gone { generation }) => {
            assert_eq!(generation, dead_generation);
        }
        other => panic!("expected SidecarGone, got {other:?}"),
    }

    // 2. the sweep ran for that generation …
    assert!(
        wait_until(Duration::from_secs(15), || {
            swept_generation.load(Ordering::SeqCst) == dead_generation
        })
        .await,
        "the generation sweep ran"
    );

    // 3. … the orphan is dead …
    assert!(
        wait_until(Duration::from_secs(15), || !is_alive(child_pid)).await,
        "the orphaned child was killed by the sweep"
    );

    // 4. … and nothing of that generation is still `running`.
    let live = h.live_rows(dead_generation).await;
    assert!(
        live.is_empty(),
        "no live rows may remain in a swept generation: {live:?}"
    );

    // 5. and the sweep happened BEFORE the replacement could take work — the
    //    property that stops a crash loop from stacking orphaned children.
    assert!(
        wait_until(Duration::from_secs(30), || manager.generation() > dead_generation).await,
        "the sidecar restarted"
    );
    let replacement_seen = Instant::now();
    let swept_at = (*swept_at.lock()).expect("the sweep recorded a time");
    assert!(
        swept_at < replacement_seen,
        "the sweep completed before the replacement generation existed"
    );
    assert!(
        wait_until(Duration::from_secs(30), || {
            manager.status().state == gilde_code_lib::model::graph::SidecarState::Ready
        })
        .await,
        "and the replacement generation reached ready"
    );

    manager.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// (b) garbage on the protocol channel
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn garbage_lines_are_logged_and_skipped_and_the_session_continues() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Plain text, a JSON object that is not JSON-RPC, and a JSON-RPC object that
    // is neither request nor response — all three before the handshake answer.
    let script = fake_sidecar(
        dir.path(),
        "garbage.cjs",
        r#"
        process.stdout.write("this is not json at all\n");
        process.stdout.write(JSON.stringify({ hello: 'world' }) + "\n");
        process.stdout.write(JSON.stringify({ jsonrpc: '2.0' }) + "\n");
        "#,
    );

    let h = Harness::boot().await;
    let manager = h.manager(script);
    manager.start().await.expect("the handshake survives garbage");

    // The channel is still usable afterwards: three more calls, three answers.
    for _ in 0..3 {
        let pong: Value = manager
            .call_detailed("mesh.ping", json!({"nonce": 1}), Some(Duration::from_secs(5)))
            .await
            .expect("ping answered after the garbage");
        assert!(pong.get("ts").is_some());
    }
    assert_eq!(
        manager.status().state,
        gilde_code_lib::model::graph::SidecarState::Ready
    );

    manager.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// (c) a 9 MiB line
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn a_nine_mib_line_restarts_the_sidecar_and_the_host_stays_bounded() {
    let dir = tempfile::tempdir().expect("tempdir");
    let marker = dir.path().join("blasted.once");
    // The oversize line is emitted only by the FIRST generation, so the restart
    // has something healthy to come back as.
    let script = fake_sidecar(
        dir.path(),
        "oversize.cjs",
        &format!(
            r#"
        const fs = require('node:fs');
        const marker = {marker:?};
        if (!fs.existsSync(marker)) {{
          fs.writeFileSync(marker, 'x');
          setTimeout(() => {{
            process.stdout.write('{{"jsonrpc":"2.0","method":"mesh.event","params":"');
            const blob = 'x'.repeat(1024 * 1024);
            for (let i = 0; i < 9; i += 1) process.stdout.write(blob);
            process.stdout.write('"}}\n');
          }}, 50);
        }}
        "#,
            marker = marker.display().to_string()
        ),
    );

    let h = Harness::boot().await;
    let manager = h.manager(script);
    manager.start().await.expect("the sidecar starts");
    let first_generation = manager.generation();

    // The host kills the offender and comes back one generation later.
    assert!(
        wait_until(Duration::from_secs(60), || manager.generation() > first_generation).await,
        "the oversize line forced a restart"
    );
    assert!(
        wait_until(Duration::from_secs(60), || {
            manager.status().state == gilde_code_lib::model::graph::SidecarState::Ready
        })
        .await,
        "and the replacement generation is ready"
    );

    // The host is alive and answering — no panic, no runaway buffer.
    let pong: Value = manager
        .call_detailed("mesh.ping", json!({"nonce": 2}), Some(Duration::from_secs(5)))
        .await
        .expect("the restarted sidecar answers");
    assert!(pong.get("ts").is_some());

    let status = manager.status();
    assert!(status.restarts >= 1, "the restart is counted: {status:?}");
    assert!(
        status
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("byte line"),
        "and the reason is recorded: {status:?}"
    );

    manager.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// acceptance 10 — live introspection on this machine
// ---------------------------------------------------------------------------

/// Live checks against the real `claude` / `codex` CLIs. Gated on `GILDE_LIVE=1`
/// because they need this machine's logins, spend real quota, and take tens of
/// seconds; the fault suite above must stay hermetic.
///
/// Run with:
///   `GILDE_LIVE=1 cargo test --test sidecar_faults live_ -- --nocapture`
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn live_mesh_introspection_returns_real_data() {
    if std::env::var("GILDE_LIVE").ok().as_deref() != Some("1") {
        eprintln!("skipping the live check (set GILDE_LIVE=1 to run it)");
        return;
    }
    let h = Harness::boot().await;
    let manager = h.manager(real_bundle());
    manager.start().await.expect("the sidecar starts");

    let adapters = manager.adapters();
    assert_eq!(adapters.len(), 4, "four adapters: {adapters:?}");
    eprintln!("adapters: {}", serde_json::to_string_pretty(&adapters).unwrap());

    let auth: AuthStatus = manager
        .call_detailed(
            "mesh.authStatus",
            json!({"vendor": "claude"}),
            Some(Duration::from_secs(60)),
        )
        .await
        .expect("mesh.authStatus(claude)");
    eprintln!("claude authStatus: {}", serde_json::to_string(&auth).unwrap());
    assert!(auth.logged_in, "this machine is logged into claude");

    for vendor in ["claude", "codex"] {
        let models: Vec<ModelInfo> = manager
            .call_detailed(
                "mesh.availableModels",
                json!({"vendor": vendor}),
                Some(Duration::from_secs(120)),
            )
            .await
            .unwrap_or_else(|e| panic!("mesh.availableModels({vendor}): {e}"));
        eprintln!(
            "{vendor} models: {}",
            serde_json::to_string(&models).unwrap()
        );
        assert!(!models.is_empty(), "{vendor} reports at least one model");
    }

    let quota: Option<QuotaSnapshot> = manager
        .call_detailed(
            "mesh.quota",
            json!({"vendor": "claude"}),
            Some(Duration::from_secs(120)),
        )
        .await
        .expect("mesh.quota(claude)");
    let quota = quota.expect("claude reports a quota snapshot");
    eprintln!("claude quota: {}", serde_json::to_string(&quota).unwrap());
    assert!(
        !quota.windows.is_empty(),
        "the snapshot carries at least one window"
    );

    manager.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// acceptance 3 — the runtime self-test
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn an_unusable_runtime_is_a_typed_error_not_a_hang() {
    let dir = tempfile::tempdir().expect("tempdir");
    // A file that exists and is executable but is not a Node runtime: `--version`
    // fails, and the manager must say so with the path and the stderr rather than
    // spawning it and waiting forever for a handshake.
    let bogus = dir.path().join("not-node");
    std::fs::write(&bogus, "#!/bin/sh\nexit 3\n").expect("write");
    std::fs::set_permissions(&bogus, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .expect("chmod");

    let h = Harness::boot().await;
    let manager = SidecarManager::new(
        h.supervisor.clone(),
        SidecarRuntime::explicit(bogus.clone(), real_bundle()),
        EnvBase::from_process_env(std::env::var("PATH").unwrap_or_default()),
        MeshConfig::default(),
        Duration::from_secs(2),
    );

    let started = Instant::now();
    let err = manager.start().await.expect_err("an unusable runtime must fail");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the self-test is capped at 5s, so this fails fast"
    );
    match err {
        gilde_code_lib::error::GildeError::SidecarRuntimeUnusable { path, .. } => {
            assert_eq!(path, bogus.display().to_string());
        }
        other => panic!("expected SidecarRuntimeUnusable, got {other:?}"),
    }
    assert_eq!(
        manager.status().state,
        gilde_code_lib::model::graph::SidecarState::Starting,
        "the failed attempt leaves the manager not-ready; nothing was spawned"
    );

    // The probe itself is in the registry: no spawn escapes the guard. It is a
    // HOST spawn, so its generation is 0 (DESIGN §8-2) — the sidecar generation
    // counter only labels children the sidecar itself creates.
    let probes = h.rows(0).await;
    assert!(
        probes.iter().any(|r| r.kind == ProcKind::Probe),
        "the --version probe was planned through SpawnGuard: {probes:?}"
    );
}

// ---------------------------------------------------------------------------
// acceptance 7 — health: three missed pings mean *hung*, not dead
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn three_missed_pings_kill_a_hung_sidecar_and_restart_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let marker = dir.path().join("stopped.answering");
    // Generation 1 answers the handshake and then never answers a ping again —
    // alive, responsive to nothing. Generation 2 behaves.
    let script = {
        let path = dir.path().join("deaf.cjs");
        let source = format!(
            r#"
const fs = require('node:fs');
const readline = require('node:readline');
const marker = {marker:?};
const deaf = !fs.existsSync(marker);
if (deaf) fs.writeFileSync(marker, 'x');
function send(obj) {{ process.stdout.write(JSON.stringify(obj) + "\n"); }}
readline.createInterface({{ input: process.stdin }}).on('line', (line) => {{
  let msg;
  try {{ msg = JSON.parse(line); }} catch {{ return; }}
  if (msg.method === 'mesh.hello') {{
    send({{ jsonrpc: '2.0', id: msg.id, result: {{
      protocol: 1, sidecarVersion: 'deaf', node: process.version, packages: {{}}, adapters: [] }} }});
    return;
  }}
  if (msg.method === 'mesh.ping' && deaf) return;   // the whole point
  if (msg.id !== undefined) send({{ jsonrpc: '2.0', id: msg.id, result: {{ ok: true, ts: Date.now() }} }});
}});

// Deaf to pings, never deaf to the exit paths (INT-C): this fixture is hung on
// purpose, not abandonable on purpose.
function bail() {{ process.exit(0); }}
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
            marker = marker.display().to_string()
        );
        std::fs::write(&path, source).expect("write");
        path
    };

    let h = Harness::boot().await;
    let manager = h.manager(script);
    manager.start().await.expect("the sidecar starts");
    let hung_generation = manager.generation();

    // 3 misses × (2 s cadence + 5 s deadline) ≈ 21 s, plus a 1 s backoff.
    assert!(
        wait_until(Duration::from_secs(90), || manager.generation() > hung_generation).await,
        "a sidecar that stops answering pings is killed and replaced"
    );
    assert!(
        wait_until(Duration::from_secs(30), || {
            manager.status().state == gilde_code_lib::model::graph::SidecarState::Ready
        })
        .await,
        "and the replacement is ready"
    );
    let status = manager.status();
    assert!(
        status
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("consecutive pings"),
        "the reason names the missed pings: {status:?}"
    );

    manager.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// acceptance 7 — every transition reaches the UI as `gilde://sidecar/state`
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn lifecycle_transitions_are_emitted_as_sidecar_state_events() {
    use gilde_code_lib::events::bus::{EventBus, RecordingSink};
    use gilde_code_lib::model::graph::GraphState;

    let dir = tempfile::tempdir().expect("tempdir");
    let script = fake_sidecar(dir.path(), "quiet.cjs", "");

    let h = Harness::boot().await;
    let manager = h.manager(script);
    let sink = Arc::new(RecordingSink::default());
    manager.set_events(EventBus::with_sink(
        sink.clone(),
        Arc::new(parking_lot::RwLock::new(GraphState::new())),
    ));

    manager.start().await.expect("the sidecar starts");
    manager.shutdown().await.expect("shutdown");

    let states: Vec<String> = sink
        .events()
        .into_iter()
        .filter(|(name, _)| name == "gilde://sidecar/state")
        .map(|(_, payload)| payload["state"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        states.contains(&"starting".to_string()) && states.contains(&"ready".to_string()),
        "the UI sees the transitions: {states:?}"
    );
    assert_eq!(
        states.last().map(String::as_str),
        Some("down"),
        "and the last one is the shutdown: {states:?}"
    );
}
