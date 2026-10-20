//! T22 — the dispatch engine, end to end (DESIGN §5, §9-7, §10-2).
//!
//! Every test here drives a **real** `SidecarManager` over a real stdio channel
//! against a scripted stand-in sidecar, and a real `git` against a real
//! repository. The stand-in exists for one reason: the properties under test
//! are the host's reactions to a peer that streams, refuses, hangs, dies or
//! gets `SIGSTOP`ped, and the shipping sidecar does none of those on demand.
//! Where the shipping bundle's own behaviour is what matters, that is
//! `tests/sidecar_faults.rs`'s job.
//!
//! What is pinned here, in the order M1-PLAN T22 lists it:
//!
//!   1/2/5 a two-target fan-out gets two worktrees, two `prompt.md`s, two
//!         `result.json`s, two distinct conversation ids and non-overlapping
//!         diffs, and every `RunTask` carries an absolute cwd and a timeout;
//!   3     the streaming decision is read from `AdapterCapabilities`, so the
//!         non-streaming vendor is driven through `mesh.run` without a single
//!         vendor name appearing in the host;
//!   4     an `installed:false` target is refused on its own row;
//!   7     **cancellation succeeds against a `SIGSTOP`ped sidecar**;
//!   8     a sidecar death leaves its runs `interrupted`, never `error`;
//!   10    `fleet.route` is called for an `auto` target and for nothing else;
//!   11    `dispatch_open_terminal` opens a pty in the run's worktree.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gilde_code_lib::accounts::AccountSlot;
use gilde_code_lib::agents::{AgentKind, AgentRegistry, Recorder, SupervisorRecorder};
use gilde_code_lib::config::ConfigStore;
use gilde_code_lib::dispatch::{
    DispatchEngine, DispatchScope, DispatchSpec, DispatchTarget, RunPhase, WorktreeStrategy,
};
use gilde_code_lib::events::bus::{EventBus, RecordingSink};
use gilde_code_lib::model::graph::GraphState;
use gilde_code_lib::paths::AppPaths;
use gilde_code_lib::pty::PtyManager;
use gilde_code_lib::sidecar::{MeshConfig, SidecarManager, SidecarRuntime};
use gilde_code_lib::store::models::ProjectRow;
use gilde_code_lib::store::{dao, now_ms, Db};
use gilde_code_lib::supervise::registry::{EnvBase, Supervisor};
use gilde_code_lib::supervise::spawn::is_alive;
use gilde_code_lib::worktree::{naming, WorktreeManager};
use serde_json::Value;
use tauri::ipc::Channel;

// ---------------------------------------------------------------------------
// the scripted stand-in sidecar
// ---------------------------------------------------------------------------

/// A `node` to run the stand-in with — the vendored runtime when the tree has
/// one, else the developer's.
fn node_binary() -> PathBuf {
    let vendored = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("binaries")
        .join("gilde-node-aarch64-apple-darwin");
    if vendored.is_file() {
        return vendored;
    }
    which::which("node").expect("a node runtime is required for the dispatch tests")
}

/// The stand-in. It speaks the real protocol (DESIGN §5-4/§5-5) and nothing
/// else: a handshake with a fixed adapter roster, `mesh.run`/`mesh.stream` that
/// register a **real** child through the supervision seam before answering, and
/// a `fleet.route` that records every call so a test can assert it was never
/// made. Behaviour is chosen per run by `GILDE_TEST_MODE`.
const STAND_IN: &str = r#"
const fs = require('node:fs');
const path = require('node:path');
const { spawn } = require('node:child_process');
const readline = require('node:readline');

const LOG = process.env.GILDE_TEST_LOG || '';
const MODE = process.env.GILDE_TEST_MODE || 'normal';

function send(o) { process.stdout.write(JSON.stringify(o) + '\n'); }
function note(line) { if (LOG) { try { fs.appendFileSync(LOG, line + '\n'); } catch (e) {} } }

let nextId = 1;
const pending = new Map();
function call(method, params) {
  return new Promise((resolve) => {
    const id = 'h' + (nextId++);
    pending.set(id, resolve);
    send({ jsonrpc: '2.0', id, method, params });
  });
}
function caps(stream, quota) {
  return { resume: true, steer: 'none', peerMesh: false, quota: quota, stream: stream };
}
const ADAPTERS = [
  { vendor: 'claude', capabilities: caps(true, true),  binary: 'claude', binaryPath: '/bin/echo', installed: true },
  { vendor: 'codex',  capabilities: caps(false, true), binary: 'codex',  binaryPath: '/bin/echo', installed: true },
  { vendor: 'gemini', capabilities: caps(true, false), binary: 'gemini', binaryPath: null,        installed: false },
];

const kids = [];
async function startChild(runId, vendor, cwd) {
  const spawnId = 'sc-' + (nextId++);
  await call('host.registerSpawn', {
    spawnId, runId, vendor, kind: 'headless',
    cmd: '/bin/sleep', args: ['600'], cwd, envKeys: [], plannedAt: Date.now(),
  });
  const child = spawn('/bin/sleep', ['600'], { detached: true, stdio: 'ignore' });
  kids.push(child);
  await call('host.spawned', { spawnId, pid: child.pid, pgid: child.pid });
  note('spawned ' + runId + ' ' + child.pid);
  return child;
}

function result(vendor, runId, text) {
  return {
    vendor,
    conversationId: 'conv-' + vendor + '-' + runId.slice(0, 8),
    text: text,
    status: 'ok',
    model: vendor + '-model',
    usage: { inputTokens: 11, outputTokens: 22, cachedInputTokens: 3, costUsd: 0.5 },
  };
}

/** Write a vendor-specific file so two runs produce non-overlapping diffs. */
function touch(cwd, vendor) {
  try { fs.writeFileSync(path.join(cwd, vendor + '.txt'), 'written by ' + vendor + '\n'); }
  catch (e) { note('touch failed ' + e.message); }
}

async function handle(msg) {
  const p = msg.params || {};
  switch (msg.method) {
    case 'mesh.hello':
      return { protocol: 1, sidecarVersion: 'stand-in', node: process.version, packages: {}, adapters: ADAPTERS };
    case 'mesh.ping':
      return { nonce: p.nonce ?? null, ts: Date.now() };
    case 'mesh.adapters':
      return ADAPTERS;
    case 'mesh.configure':
      return { ok: true };
    case 'fleet.route':
      note('route ' + JSON.stringify((p.task || {}).prompt || ''));
      return { vendor: 'claude', note: 'every candidate was gated; best effort' };
    case 'mesh.cancel':
      note('cancel ' + p.runId);
      return { stopped: true, via: 'abort' };
    case 'mesh.run': {
      note('run ' + p.vendor + ' ' + JSON.stringify(p.task.cwd) + ' mode=' + String(p.task.permissionMode) + ' ' + p.task.timeoutMs);
      await startChild(p.runId, p.vendor, p.task.cwd);
      if (MODE === 'hang') { await new Promise(() => {}); }
      touch(p.task.cwd, p.vendor);
      return result(p.vendor, p.runId, 'the ' + p.vendor + ' answer');
    }
    case 'mesh.stream': {
      note('stream ' + p.vendor + ' ' + JSON.stringify(p.task.cwd) + ' mode=' + String(p.task.permissionMode) + ' ' + p.task.timeoutMs);
      (async () => {
        await startChild(p.runId, p.vendor, p.task.cwd);
        send({ jsonrpc: '2.0', method: 'mesh.event', params: { runId: p.runId, event: { type: 'tool', name: 'Edit' } } });
        for (const chunk of ['hello ', 'from ', p.vendor]) {
          send({ jsonrpc: '2.0', method: 'mesh.event', params: { runId: p.runId, event: { type: 'text', text: chunk } } });
        }
        send({ jsonrpc: '2.0', method: 'mesh.event', params: { runId: p.runId, event: { type: 'usage', usage: { inputTokens: 11, outputTokens: 22 } } } });
        if (MODE === 'hang') return;             // never terminates
        touch(p.task.cwd, p.vendor);
        send({ jsonrpc: '2.0', method: 'mesh.event', params: { runId: p.runId, event: { type: 'done', result: result(p.vendor, p.runId, 'the ' + p.vendor + ' answer') } } });
      })().catch((e) => note('stream failed ' + e.message));
      return { accepted: true };
    }
    case 'mesh.shutdown':
      return { ok: true };
    default:
      return { ok: true };
  }
}

const rl = readline.createInterface({ input: process.stdin });
rl.on('line', (line) => {
  let msg;
  try { msg = JSON.parse(line); } catch (e) { return; }
  if (msg.id !== undefined && msg.method === undefined) {
    const resolve = pending.get(msg.id);
    if (resolve) { pending.delete(msg.id); resolve(msg.result || {}); }
    return;
  }
  if (msg.method === undefined) return;
  handle(msg).then(
    (result) => { if (msg.id !== undefined) send({ jsonrpc: '2.0', id: msg.id, result }); },
    (err) => { if (msg.id !== undefined) send({ jsonrpc: '2.0', id: msg.id, error: { code: -32001, message: String(err && err.message || err) } }); },
  );
});

// --- self-termination (INT-C) ------------------------------------------------
// A fixture must not outlive the test that started it. Before this, a `cargo
// test` that was SIGKILLed left this script and its detached `/bin/sleep 600`
// children running — measured at 7-11 hours on a developer machine, which is
// the exact orphan population this app exists to prevent. So the stand-in
// carries the same three exit paths the real sidecar does.
let goingDown = false;
function bail() {
  if (goingDown) return;
  goingDown = true;
  for (const kid of kids) {
    // Detached, so `pgid === pid`: kill the group, not just the leader.
    try { process.kill(-kid.pid, 'SIGKILL'); } catch (e) { }
  }
  process.exit(0);
}
process.stdin.on('end', bail);
process.stdin.on('close', bail);
process.stdin.on('error', bail);
for (const sig of ['SIGTERM', 'SIGINT', 'SIGHUP']) process.on(sig, bail);
const bornUnder = process.ppid;
const parentWatch = setInterval(() => {
  if (process.ppid === 1 || process.ppid !== bornUnder) bail();
}, 1000);
parentWatch.unref();

setInterval(() => {}, 1000);
"#;

// ---------------------------------------------------------------------------
// fixture
// ---------------------------------------------------------------------------

fn git_ok(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn init_repo(root: &Path) -> PathBuf {
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git_ok(&repo, &["init", "-q", "-b", "main", "."]);
    git_ok(&repo, &["config", "user.email", "test@example.com"]);
    git_ok(&repo, &["config", "user.name", "Test"]);
    git_ok(&repo, &["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.join("README.md"), b"hello\n").expect("write");
    git_ok(&repo, &["add", "-A"]);
    git_ok(&repo, &["commit", "-qm", "init"]);
    std::fs::canonicalize(&repo).expect("canonicalize")
}

struct Fixture {
    _tmp: tempfile::TempDir,
    db: Db,
    paths: Arc<AppPaths>,
    config: Arc<ConfigStore>,
    supervisor: Arc<Supervisor>,
    worktrees: Arc<WorktreeManager>,
    bus: EventBus,
    sink: Arc<RecordingSink>,
    project_id: String,
    log: PathBuf,
    script: PathBuf,
}

impl Fixture {
    async fn boot(mode: &str) -> Fixture {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = std::fs::canonicalize(tmp.path()).expect("canonicalize");
        let repo = init_repo(&home);

        let paths = Arc::new(AppPaths::for_home(&home));
        paths.ensure_all().expect("ensure");

        let db = Db::open_in_memory().expect("db");
        let config = Arc::new(ConfigStore::open_at(&home.join("config.json")).expect("config"));
        // The stand-in reads its behaviour from the environment, and
        // `compose_env` is an allowlist: without this the child never sees it.
        config
            .patch(serde_json::json!({
                "agents": {"envPassthrough": ["GILDE_TEST_MODE", "GILDE_TEST_LOG"]},
                "dispatch": {"defaultTimeoutMinutes": 1}
            }))
            .expect("patch");

        let sink = Arc::new(RecordingSink::default());
        let bus = EventBus::with_sink(
            sink.clone(),
            Arc::new(parking_lot::RwLock::new(GraphState::new())),
        );

        let supervisor = Arc::new(
            Supervisor::open_instance(&db, config.clone(), "0.0.0-test")
                .await
                .expect("instance")
                .with_events(bus.clone()),
        );
        let worktrees = WorktreeManager::new(db.clone(), paths.clone(), Some(bus.clone()));

        let project = ProjectRow {
            id: uuid::Uuid::now_v7().to_string(),
            path: repo.display().to_string(),
            canonical_path: repo.display().to_string(),
            repo_slug: naming::repo_slug(&repo),
            name: "repo".into(),
            color: None,
            default_base_ref: "main".into(),
            added_at: now_ms(),
            last_opened_at: None,
        };
        let project_id = project.id.clone();
        db.write(move |conn| dao::project::upsert(conn, &project))
            .await
            .expect("project");

        let script = home.join("stand-in-sidecar.cjs");
        std::fs::write(&script, STAND_IN).expect("write the stand-in");
        let log = home.join("calls.log");
        std::env::set_var("GILDE_TEST_MODE", mode);
        std::env::set_var("GILDE_TEST_LOG", &log);

        Fixture {
            _tmp: tmp,
            db,
            paths,
            config,
            supervisor,
            worktrees,
            bus,
            sink,
            project_id,
            log,
            script,
        }
    }

    fn sidecar(&self) -> Arc<SidecarManager> {
        SidecarManager::new(
            self.supervisor.clone(),
            SidecarRuntime::explicit(node_binary(), self.script.clone()),
            EnvBase::from_process_env(std::env::var("PATH").unwrap_or_default()),
            MeshConfig::default(),
            Duration::from_secs(2),
        )
    }

    fn engine(&self, sidecar: Arc<SidecarManager>) -> Arc<DispatchEngine> {
        let engine = DispatchEngine::new(
            self.db.clone(),
            self.paths.clone(),
            self.config.clone(),
            sidecar,
            self.supervisor.clone(),
            self.worktrees.clone(),
            Arc::new(Vec::<AccountSlot>::new()),
        );
        engine.set_events(self.bus.clone());
        engine
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    async fn runs(&self, task_id: &str) -> Vec<gilde_code_lib::store::models::DispatchRunRow> {
        let id = task_id.to_string();
        self.db
            .read(move |conn| dao::dispatch::runs_of_task(conn, &id))
            .await
            .expect("runs")
    }
}

fn spec(project_id: &str, slug: &str, vendors: &[&str]) -> DispatchSpec {
    DispatchSpec {
        prompt: "make the tests pass".into(),
        project_id: project_id.to_string(),
        base_ref: None,
        task_slug: slug.to_string(),
        targets: vendors.iter().map(|v| DispatchTarget::new(*v)).collect(),
        worktree_strategy: WorktreeStrategy::PerTarget,
        timeout_ms: Some(120_000),
        permission_mode: Some("acceptEdits".into()),
        title: None,
    }
}

/// Every `gilde://dispatch/event` payload the bus published, as `(kind, runId)`.
fn dispatch_events(sink: &RecordingSink) -> Vec<(String, String)> {
    sink.events()
        .into_iter()
        .filter(|(name, _)| name == "gilde://dispatch/event")
        .map(|(_, payload)| {
            (
                payload["kind"].as_str().unwrap_or_default().to_string(),
                payload["runId"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

/// The terminal kinds. Exactly one of these reaches the UI per run.
const TERMINALS: [&str; 4] = ["done", "error", "interrupted", "cancelled"];

fn terminal_events_of(sink: &RecordingSink, run_id: &str) -> Vec<String> {
    dispatch_events(sink)
        .into_iter()
        .filter(|(kind, id)| id == run_id && TERMINALS.contains(&kind.as_str()))
        .map(|(kind, _)| kind)
        .collect()
}

/// Poll `predicate` until it holds or `timeout` elapses.
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

/// Poll until every run of `task_id` is in a terminal phase.
async fn wait_for_terminal(fixture: &Fixture, task_id: &str, timeout: Duration) -> Vec<RunPhase> {
    let deadline = Instant::now() + timeout;
    loop {
        let rows = fixture.runs(task_id).await;
        let phases: Vec<RunPhase> = rows
            .iter()
            .map(|row| RunPhase::from_str(&row.status))
            .collect();
        if !phases.is_empty() && phases.iter().all(|p| p.is_terminal()) {
            return phases;
        }
        if Instant::now() >= deadline {
            return phases;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------
// acceptance 1, 2, 3, 5, 6, 12 (deterministic half)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn a_fan_out_gives_each_target_its_own_worktree_artifacts_and_conversation() {
    let f = Fixture::boot("normal").await;
    let sidecar = f.sidecar();
    sidecar.start().await.expect("the stand-in starts");
    let engine = f.engine(sidecar.clone());

    let task = engine
        .create(spec(&f.project_id, "fix-pty-resize", &["claude", "codex"]))
        .await
        .expect("dispatch_create");

    let phases = wait_for_terminal(&f, &task.id, Duration::from_secs(30)).await;
    assert_eq!(phases, vec![RunPhase::Done, RunPhase::Done], "both runs finish");

    let rows = f.runs(&task.id).await;
    assert_eq!(rows.len(), 2);

    // (1) one worktree per target, named `<task-slug>-<vendor>` on `gilde/<same>`.
    let mut paths = Vec::new();
    let mut conversations = Vec::new();
    for row in &rows {
        let worktree_id = row.worktree_id.clone().expect("a run has a worktree");
        let worktree = f
            .db
            .read(move |conn| dao::worktree::get(conn, &worktree_id))
            .await
            .expect("read")
            .expect("worktree row");
        assert!(
            worktree.slug.starts_with(&format!("fix-pty-resize-{}", row.vendor)),
            "worktree {} is not named after its target",
            worktree.slug
        );
        assert_eq!(worktree.branch, format!("gilde/{}", worktree.slug));
        assert!(Path::new(&worktree.path).is_dir());
        paths.push(PathBuf::from(worktree.path));

        // (5) the three artifacts, and `result.json` is the verbatim result.
        let dir = f.paths.dispatch_run_dir(&task.id, &row.id);
        assert_eq!(
            std::fs::read_to_string(dir.join("prompt.md")).expect("prompt.md"),
            "make the tests pass"
        );
        let result: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("result.json")).expect("result.json"))
                .expect("result.json is json");
        assert_eq!(result["status"], "ok");
        assert_eq!(result["vendor"], row.vendor.as_str());
        let stream = std::fs::read_to_string(dir.join("stream.jsonl")).expect("stream.jsonl");
        assert!(
            stream.lines().any(|l| l.contains("\"done\"")),
            "stream.jsonl ends with the terminal event"
        );

        // (6) the usage and cost the vendor reported reached the row.
        assert_eq!(row.cost_usd, Some(0.5));
        assert!(row.usage_json.as_deref().unwrap_or("").contains("inputTokens"));
        assert_eq!(row.model_actual.as_deref(), Some(format!("{}-model", row.vendor).as_str()));

        conversations.push(row.conversation_id.clone().unwrap_or_default());
    }

    // (12, deterministic half) two worktrees, two conversation ids, and diffs
    // that do not overlap: each run wrote only its own file.
    assert_ne!(paths[0], paths[1], "the two runs do not share a worktree");
    assert_ne!(conversations[0], conversations[1], "two conversations");
    for (i, row) in rows.iter().enumerate() {
        let mine = paths[i].join(format!("{}.txt", row.vendor));
        let theirs = paths[1 - i].join(format!("{}.txt", row.vendor));
        assert!(mine.is_file(), "{} wrote into its own worktree", row.vendor);
        assert!(!theirs.is_file(), "{}'s edit did not leak", row.vendor);
    }

    // (2, 3) the streaming decision came from `AdapterCapabilities`, and both
    // lanes carried an absolute cwd and a timeout.
    let calls = f.calls();
    let streamed = calls.iter().find(|l| l.starts_with("stream claude")).expect("claude streamed");
    let ran = calls.iter().find(|l| l.starts_with("run codex")).expect("codex used mesh.run");
    for line in [streamed, ran] {
        let cwd = line.split('"').nth(1).expect("a quoted cwd");
        assert!(Path::new(cwd).is_absolute(), "cwd {cwd} is absolute");
        assert!(Path::new(cwd).is_dir(), "cwd {cwd} exists");
        let timeout: u64 = line.rsplit(' ').next().expect("timeout").parse().expect("a number");
        assert_eq!(timeout, 120_000, "every RunTask carries a wall clock");
    }
    assert!(
        !calls.iter().any(|l| l.starts_with("stream codex")),
        "a vendor whose capabilities say stream:false is never streamed to"
    );

    // (12, the half that only shows up live) the task's single permission mode
    // is claude's word, and each vendor received it in its own vocabulary.
    // Verbatim `acceptEdits` reaches codex as `--sandbox acceptEdits`, which
    // clap rejects with exit 2 before the model is ever asked.
    assert!(
        streamed.contains(" mode=acceptEdits "),
        "claude keeps the vocabulary it defined; got {streamed}"
    );
    assert!(
        ran.contains(" mode=workspace-write "),
        "codex is given a `--sandbox` word it accepts; got {ran}"
    );

    // The task itself folded to `done`, and the board can be restored.
    let detail = engine.get(&task.id).await.expect("dispatch_get");
    assert_eq!(detail.runs.len(), 2);
    assert!(detail.runs.iter().all(|r| r.status == RunPhase::Done));
    assert!(detail.runs.iter().all(|r| r.text.contains("answer")));
    let listed = engine.list(DispatchScope::Recent, 10).await.expect("list");
    assert_eq!(listed.len(), 1);
    assert!(listed[0].status.is_terminal());

    // (6) every event reached `run_event` and `gilde://dispatch/event`.
    let run_id = rows[0].id.clone();
    let events = f
        .db
        .read(move |conn| dao::dispatch::run_events(conn, &run_id, 200))
        .await
        .expect("events");
    assert!(events.iter().any(|e| e.kind == "tool"));
    assert!(events.iter().any(|e| e.kind == "done"));
    let streamed = rows.iter().find(|r| r.vendor == "claude").expect("claude row");
    let kinds: Vec<String> = dispatch_events(&f.sink)
        .into_iter()
        .filter(|(_, id)| id == &streamed.id)
        .map(|(kind, _)| kind)
        .collect();
    for expected in ["spawned", "tool", "text", "usage", "done"] {
        assert!(
            kinds.iter().any(|k| k == expected),
            "the UI stream carried a {expected} event; got {kinds:?}"
        );
    }
    assert_eq!(
        kinds.iter().filter(|k| k.as_str() == "text").count(),
        1,
        "three text chunks inside one 100 ms window arrive as one merged event"
    );
    for row in &rows {
        assert_eq!(
            terminal_events_of(&f.sink, &row.id),
            vec!["done".to_string()],
            "exactly one terminal event reaches the UI for {}",
            row.vendor
        );
    }

    let _ = sidecar.shutdown().await;
}

// ---------------------------------------------------------------------------
// M1.5-A-dispatch — BACKLOG 1: the default mode is one a headless run can work
// under
// ---------------------------------------------------------------------------

/// A dispatch created without a permission mode still reaches each vendor on
/// its own edit rung, and edits land in the worktree.
///
/// The regression this pins is not the translation table (that has its own
/// unit test) but the *default*: the composer sent `permissionMode: "default"`
/// on every submit, which is claude's "ask a human". A headless run has no
/// human, so claude answered `write permission not granted` and codex — where
/// `default` translates to the most restrictive rung it has — answered
/// `workspace is read-only`, and both finished having changed nothing. The
/// worktree, not the prompt, is what keeps the edits contained.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn the_default_permission_mode_lets_a_headless_run_edit_its_worktree() {
    let f = Fixture::boot("normal").await;
    let sidecar = f.sidecar();
    sidecar.start().await.expect("the stand-in starts");
    let engine = f.engine(sidecar.clone());

    // The shipped default, with no test fixture propping it up.
    assert_eq!(
        f.config.get().dispatch.permission_mode,
        "acceptEdits",
        "config.dispatch.permissionMode is the edit rung out of the box"
    );

    // Exactly what `dispatch_create` receives when nobody touches the select.
    let mut spec = spec(&f.project_id, "default-permission", &["claude", "codex"]);
    spec.permission_mode = None;

    let task = engine.create(spec).await.expect("dispatch_create");
    assert_eq!(
        task.permission_mode, "acceptEdits",
        "the task records the mode it will run every target under"
    );

    let phases = wait_for_terminal(&f, &task.id, Duration::from_secs(30)).await;
    assert_eq!(phases, vec![RunPhase::Done, RunPhase::Done], "both runs finish");

    // The `RunTask` that actually crossed the pipe, per vendor, as the stand-in
    // saw it: one task's single mode, spoken in two different dictionaries.
    let calls = f.calls();
    let streamed = calls
        .iter()
        .find(|l| l.starts_with("stream claude"))
        .expect("claude streamed");
    let ran = calls
        .iter()
        .find(|l| l.starts_with("run codex"))
        .expect("codex used mesh.run");
    assert!(
        streamed.contains(" mode=acceptEdits "),
        "claude keeps the vocabulary it defined; got {streamed}"
    );
    assert!(
        ran.contains(" mode=workspace-write "),
        "codex is given the `--sandbox` word that lets it write; got {ran}"
    );
    for line in [streamed, ran] {
        assert!(
            !line.contains(" mode=default ")
                && !line.contains(" mode=read-only ")
                && !line.contains(" mode=undefined "),
            "no run is dispatched on a rung that cannot edit; got {line}"
        );
    }

    // And the edit really happened: each run wrote into its own worktree.
    for row in f.runs(&task.id).await {
        let worktree_id = row.worktree_id.clone().expect("a run has a worktree");
        let worktree = f
            .db
            .read(move |conn| dao::worktree::get(conn, &worktree_id))
            .await
            .expect("read")
            .expect("worktree row");
        assert!(
            Path::new(&worktree.path).join(format!("{}.txt", row.vendor)).is_file(),
            "{} edited its own worktree under the default mode",
            row.vendor
        );
    }

    let _ = sidecar.shutdown().await;
}

// ---------------------------------------------------------------------------
// acceptance 4
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn an_uninstalled_target_is_refused_on_its_own_row() {
    let f = Fixture::boot("normal").await;
    let sidecar = f.sidecar();
    sidecar.start().await.expect("start");
    let engine = f.engine(sidecar.clone());

    let task = engine
        .create(spec(&f.project_id, "mixed", &["gemini", "claude"]))
        .await
        .expect("create");

    let phases = wait_for_terminal(&f, &task.id, Duration::from_secs(30)).await;
    assert_eq!(phases.len(), 2, "the refused target still gets a row");

    let rows = f.runs(&task.id).await;
    let refused = rows.iter().find(|r| r.vendor == "gemini").expect("gemini row");
    assert_eq!(RunPhase::from_str(&refused.status), RunPhase::Error);
    assert_eq!(refused.note.as_deref(), Some("notInstalled"));
    assert!(refused.worktree_id.is_none(), "a refused target gets no worktree");

    let ran = rows.iter().find(|r| r.vendor == "claude").expect("claude row");
    assert_eq!(
        RunPhase::from_str(&ran.status),
        RunPhase::Done,
        "one refusal does not fail the other target"
    );

    let detail = engine.get(&task.id).await.expect("get");
    assert_eq!(
        detail.task.status,
        gilde_code_lib::dispatch::TaskStatus::Partial,
        "a mixed fan-out reports partial, not done and not error"
    );

    let _ = sidecar.shutdown().await;
}

// ---------------------------------------------------------------------------
// acceptance 7 — cancellation is the host's, even with the sidecar stopped
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn cancelling_works_while_the_sidecar_is_stopped() {
    let f = Fixture::boot("hang").await;
    let sidecar = f.sidecar();
    sidecar.start().await.expect("start");
    let engine = f.engine(sidecar.clone());

    let task = engine
        .create(spec(&f.project_id, "wedged", &["claude"]))
        .await
        .expect("create");

    // Wait for the run's child to be registered and running.
    let child_pid = {
        let mut pid = None;
        let db = f.db.clone();
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            let rows = f.runs(&task.id).await;
            if let Some(run) = rows.first() {
                let run_id = run.id.clone();
                let procs = db
                    .read(move |conn| dao::proc::by_run_id(conn, &run_id))
                    .await
                    .unwrap_or_default();
                if let Some(row) = procs.iter().find_map(|r| r.pid) {
                    pid = Some(row as i32);
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        pid.expect("the seam registered the run's child")
    };
    assert!(is_alive(child_pid), "the child is running");

    // **Stop the sidecar.** From here it answers nothing: `mesh.cancel` will
    // never come back, and the only thing that can end this run is the host.
    let sidecar_pid = sidecar_pid(&f).await.expect("the sidecar has a pid");
    signal(sidecar_pid, nix::sys::signal::Signal::SIGSTOP);

    let started = Instant::now();
    let report = engine.cancel_run(&run_id_of(&f, &task.id).await).await.expect("cancel");
    let elapsed = started.elapsed();

    // Resume it so the harness can shut down cleanly whatever happens next.
    signal(sidecar_pid, nix::sys::signal::Signal::SIGCONT);

    assert!(
        elapsed < Duration::from_secs(20),
        "the kill did not wait on the wedged sidecar (took {elapsed:?})"
    );
    assert!(!report.is_refused(), "the cancel signalled something: {report:?}");
    assert!(
        wait_until(Duration::from_secs(10), || !is_alive(child_pid)).await,
        "the run's child is gone"
    );

    let rows = f.runs(&task.id).await;
    assert_eq!(
        RunPhase::from_str(&rows[0].status),
        RunPhase::Cancelled,
        "the row says cancelled, not error"
    );
    assert_eq!(
        terminal_events_of(&f.sink, &rows[0].id),
        vec!["cancelled".to_string()],
        "the card is told once, and told it was cancelled"
    );

    let _ = sidecar.shutdown().await;
}

// ---------------------------------------------------------------------------
// acceptance 8 — a dead sidecar interrupts, it does not fail
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn a_dead_sidecar_leaves_its_runs_interrupted_not_errored() {
    let f = Fixture::boot("hang").await;
    let sidecar = f.sidecar();
    sidecar.start().await.expect("start");
    let engine = f.engine(sidecar.clone());

    let task = engine
        .create(spec(&f.project_id, "doomed", &["claude"]))
        .await
        .expect("create");

    // Wait until the run is actually streaming before killing the peer.
    let db = f.db.clone();
    let task_id = task.id.clone();
    assert!(
        wait_until(Duration::from_secs(20), || {
            let id = task_id.clone();
            let rows = futures_block(db.read(move |conn| dao::dispatch::runs_of_task(conn, &id)));
            rows.unwrap_or_default()
                .first()
                .map(|r| RunPhase::from_str(&r.status) == RunPhase::Running)
                .unwrap_or(false)
        })
        .await,
        "the run reached `running`"
    );

    let pid = sidecar_pid(&f).await.expect("pid");
    signal(pid, nix::sys::signal::Signal::SIGKILL);

    let phases = wait_for_terminal(&f, &task.id, Duration::from_secs(30)).await;
    assert_eq!(
        phases,
        vec![RunPhase::Interrupted],
        "a sidecar death is the app failing, not the agent"
    );
    let rows = f.runs(&task.id).await;
    assert!(
        rows[0].error.is_some(),
        "the row explains what happened: {:?}",
        rows[0].error
    );
    assert_eq!(
        terminal_events_of(&f.sink, &rows[0].id),
        vec!["interrupted".to_string()],
        "the card is told once, and told it was interrupted rather than failed"
    );

    let _ = sidecar.shutdown().await;
}

// ---------------------------------------------------------------------------
// acceptance 10 — routing happens only for an `auto` target
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn fleet_route_is_called_for_auto_and_for_nothing_else() {
    let f = Fixture::boot("normal").await;
    let sidecar = f.sidecar();
    sidecar.start().await.expect("start");
    let engine = f.engine(sidecar.clone());

    // An explicit two-target fan-out: no routing at all.
    let explicit = engine
        .create(spec(&f.project_id, "explicit", &["claude", "codex"]))
        .await
        .expect("create");
    wait_for_terminal(&f, &explicit.id, Duration::from_secs(30)).await;
    assert!(
        !f.calls().iter().any(|l| l.starts_with("route ")),
        "an explicit target never routes"
    );

    // `auto` routes exactly once, and the note is persisted with the run.
    let auto = engine
        .create(spec(&f.project_id, "auto-pick", &["auto"]))
        .await
        .expect("create");
    wait_for_terminal(&f, &auto.id, Duration::from_secs(30)).await;
    assert_eq!(
        f.calls().iter().filter(|l| l.starts_with("route ")).count(),
        1,
        "one auto target, one routing decision"
    );
    let rows = f.runs(&auto.id).await;
    assert_eq!(rows[0].vendor, "claude", "the run took the routed vendor");
    assert_eq!(
        rows[0].route_note.as_deref(),
        Some("every candidate was gated; best effort"),
        "the fleet's note is persisted verbatim"
    );

    let _ = sidecar.shutdown().await;
}

// ---------------------------------------------------------------------------
// acceptance 11 — the bridge to the interactive lane
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn open_terminal_opens_a_pty_in_the_runs_worktree() {
    let f = Fixture::boot("normal").await;
    let sidecar = f.sidecar();
    sidecar.start().await.expect("start");
    let engine = f.engine(sidecar.clone());

    // The pty lane, wired the way boot wires it.
    let recorder: Recorder = Arc::new(SupervisorRecorder::new(f.supervisor.clone()));
    let agents = Arc::new(AgentRegistry::new(
        (*f.paths).clone(),
        f.config.clone(),
        Some(f.db.clone()),
        recorder,
    ));
    let pty = Arc::new(
        PtyManager::new(f.supervisor.clone(), f.config.clone())
            .with_events(f.bus.clone())
            .with_agents(agents)
            .with_accounts(Arc::new(Vec::new())),
    );
    engine.set_pty(pty.clone());

    let task = engine
        .create(spec(&f.project_id, "bridge", &["claude"]))
        .await
        .expect("create");
    wait_for_terminal(&f, &task.id, Duration::from_secs(30)).await;
    let rows = f.runs(&task.id).await;
    let run = &rows[0];

    let seen = Arc::new(AtomicUsize::new(0));
    let counter = seen.clone();
    let channel = Channel::new(move |_body| {
        counter.fetch_add(1, Ordering::Relaxed);
        Ok(())
    });

    let info = engine
        .open_terminal(&run.id, channel)
        .await
        .expect("dispatch_open_terminal");

    let worktree_id = run.worktree_id.clone().expect("worktree");
    let worktree = f
        .db
        .read(move |conn| dao::worktree::get(conn, &worktree_id))
        .await
        .expect("read")
        .expect("row");
    assert_eq!(info.cwd, worktree.path, "the terminal opened in the run's worktree");
    assert_eq!(info.agent, AgentKind::Shell, "a shell, not a second agent");
    assert!(info.pid > 1);

    pty.close(&info.id, gilde_code_lib::pty::CloseMode::Standard)
        .await
        .expect("close");
    let _ = sidecar.shutdown().await;
}

// ---------------------------------------------------------------------------
// acceptance 12 — the live fan-out
// ---------------------------------------------------------------------------

/// One prompt, the **real** sidecar bundle, the real `claude` and `codex`.
///
/// `#[ignore]` because it spends quota, needs both CLIs logged in, and takes
/// minutes:
///
/// ```text
/// cargo test --test dispatch -- --ignored --nocapture live_fan_out
/// GILDE_LIVE_SECOND_VENDOR=gemini cargo test --test dispatch -- --ignored live_fan_out
/// ```
///
/// The codex model is pinned rather than defaulted: this machine's codex 0.145.0
/// defaults to `gpt-6-astra`, which it cannot reach (PINNED-VERSIONS §9-5). The
/// pin keeps the check about the dispatch engine instead of about that.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
#[ignore = "live: spends real quota and needs claude + codex logged in"]
async fn live_fan_out_to_two_agents() {
    let f = Fixture::boot("normal").await;

    let bundle = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("sidecar/sidecar.cjs");
    assert!(bundle.is_file(), "build the sidecar first: npm run sidecar:build");
    let mesh = MeshConfig {
        binaries: gilde_code_lib::sidecar::BinaryPaths {
            claude: which::which("claude").ok().map(|p| p.display().to_string()),
            codex: which::which("codex").ok().map(|p| p.display().to_string()),
            cursor_agent: which::which("cursor-agent").ok().map(|p| p.display().to_string()),
            agy: None,
        },
        accounts: Vec::new(),
        defaults: Default::default(),
    };
    let sidecar = SidecarManager::new(
        f.supervisor.clone(),
        SidecarRuntime::explicit(node_binary(), bundle),
        EnvBase::from_process_env(std::env::var("PATH").unwrap_or_default()),
        mesh,
        Duration::from_secs(10),
    );
    sidecar.start().await.expect("the real sidecar starts");
    let engine = f.engine(sidecar.clone());

    // The second vendor defaults to codex — the acceptance-12 pairing — and is
    // overridable so the same check can be pointed at another installed CLI
    // (`GILDE_LIVE_SECOND_VENDOR=gemini`) without editing the test.
    let second = std::env::var("GILDE_LIVE_SECOND_VENDOR").unwrap_or_else(|_| "codex".to_string());
    let mut spec = spec(&f.project_id, "live-fanout", &["claude", &second]);
    spec.prompt = "Create a file called REPORT.md whose only line is: hello from you. \
                   Do not change anything else."
        .into();
    spec.timeout_ms = Some(300_000);
    if second == "codex" {
        spec.targets[1].model = Some("gpt-5.6-sol".into());
    }

    let task = engine.create(spec).await.expect("create");
    let phases = wait_for_terminal(&f, &task.id, Duration::from_secs(360)).await;
    println!("phases: {phases:?}");

    let rows = f.runs(&task.id).await;
    assert_eq!(rows.len(), 2);
    let mut conversations = Vec::new();
    let mut paths = Vec::new();
    for row in &rows {
        println!(
            "{} -> {} model={:?} conv={:?} err={:?}",
            row.vendor, row.status, row.model_actual, row.conversation_id, row.error
        );
        assert_eq!(
            RunPhase::from_str(&row.status),
            RunPhase::Done,
            "{} did not finish: {:?}",
            row.vendor,
            row.error
        );
        let dir = f.paths.dispatch_run_dir(&task.id, &row.id);
        assert!(dir.join("prompt.md").is_file());
        assert!(dir.join("result.json").is_file(), "{} has a result.json", row.vendor);
        println!(
            "  result.json {} ({} bytes)",
            dir.join("result.json").display(),
            std::fs::metadata(dir.join("result.json")).map(|m| m.len()).unwrap_or(0)
        );
        conversations.push(row.conversation_id.clone().unwrap_or_default());

        let worktree_id = row.worktree_id.clone().expect("worktree");
        let worktree = f
            .db
            .read(move |conn| dao::worktree::get(conn, &worktree_id))
            .await
            .expect("read")
            .expect("row");
        println!("  worktree {} @ {}", worktree.branch, worktree.path);
        paths.push(PathBuf::from(worktree.path));
    }
    assert_ne!(paths[0], paths[1], "two worktrees");
    assert_ne!(conversations[0], conversations[1], "two conversations");
    assert!(
        conversations.iter().all(|c| !c.is_empty()),
        "both runs are resumable"
    );
    // Non-overlapping diffs: whatever each agent changed, it changed inside its
    // own worktree and nowhere else. At least one of them must actually have
    // edited something — otherwise the cwd was never really writable — but a CLI
    // that answers without touching the tree (gemini, on this machine) is an
    // agent behaviour, not a dispatch failure.
    //
    // "Non-overlapping" is *not* "different file names". One prompt goes to
    // every target, so two agents that both do the work both create
    // `REPORT.md`; that is the fan-out succeeding. What must hold is that no
    // edit escaped the worktree that made it — the project checkout the
    // worktrees branch off is untouched, and where the two trees hold a file of
    // the same name they are two files on disk, not one shared through a link.
    let diffs: Vec<String> = paths
        .iter()
        .map(|path| git_ok(path, &["status", "--porcelain"]))
        .collect();
    for (path, diff) in paths.iter().zip(&diffs) {
        println!("  diff of {}:\n{diff}", path.display());
    }
    assert!(
        diffs.iter().any(|d| !d.is_empty()),
        "at least one agent edited its own worktree"
    );
    let project_id = f.project_id.clone();
    let project = f
        .db
        .read(move |conn| dao::project::get(conn, &project_id))
        .await
        .expect("read")
        .expect("project row");
    let repo = PathBuf::from(&project.path);
    assert_eq!(
        git_ok(&repo, &["status", "--porcelain"]),
        "",
        "the project checkout stayed clean; nothing escaped a worktree"
    );
    for (i, diff) in diffs.iter().enumerate() {
        for line in diff.lines() {
            let file = line.split_whitespace().last().unwrap_or_default();
            let mine = paths[i].join(file);
            assert!(mine.exists(), "{file} is inside the worktree that reported it");
            let theirs = paths[1 - i].join(file);
            if theirs.exists() {
                assert_ne!(
                    inode_of(&mine),
                    inode_of(&theirs),
                    "{file} is the same file on disk in both worktrees"
                );
            }
        }
    }

    let _ = sidecar.shutdown().await;
}

// ---------------------------------------------------------------------------
// small helpers that need the fixture
// ---------------------------------------------------------------------------

async fn run_id_of(f: &Fixture, task_id: &str) -> String {
    f.runs(task_id).await.first().expect("a run").id.clone()
}

/// The pid of the live sidecar, read from its own registry row.
async fn sidecar_pid(f: &Fixture) -> Option<i32> {
    let rows = f
        .db
        .read(|conn| dao::proc::live_rows(conn, None))
        .await
        .ok()?;
    rows.iter()
        .find(|row| row.kind == gilde_code_lib::store::models::ProcKind::Sidecar)
        .and_then(|row| row.pid)
        .map(|pid| pid as i32)
}

/// The inode a path resolves to. Two worktrees may hold a file of the same
/// name; only a shared inode would mean they are the *same* file.
fn inode_of(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::metadata(path)
        .unwrap_or_else(|err| panic!("stat {}: {err}", path.display()))
        .ino()
}

fn signal(pid: i32, sig: nix::sys::signal::Signal) {
    let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), sig);
}

/// Block on a future from inside a synchronous predicate. Only used by the
/// polling helpers, where the future is a single short database read.
fn futures_block<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(future))
}
