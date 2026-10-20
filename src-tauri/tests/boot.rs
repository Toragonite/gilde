//! T29 — what boot assembles and what shutdown takes back down.
//!
//! The boot *order* is pinned by `lib.rs` itself (it is one function and the
//! comments name each step); what cannot be pinned by reading is the way out,
//! because it involves four real processes in four process groups and a
//! database row that has to say so afterwards. M1-PLAN T29 acceptance 2 is
//! therefore asserted here, end to end:
//!
//!   * every PTY child of this instance is gone, through `kill_tree`;
//!   * the sidecar is gone — a **real** `gilde-node` running the shipped bundle,
//!     not a stand-in, because the shutdown path's whole job is to end the
//!     process the shipped app starts;
//!   * every `proc_registry` row of this instance is in a terminal state;
//!   * `app_instance.ended_at` is filled, which is the single bit that tells
//!     the next boot's sweep "this was a quit, not a crash".
//!
//! And the property that keeps a signal racing `RunEvent::Exit` from doing all
//! of that twice.

mod fixtures;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gilde_code_lib::agents::{AgentKind, AgentRegistry, Recorder, SupervisorRecorder};
use gilde_code_lib::config::ConfigStore;
use gilde_code_lib::dispatch::{DispatchEngine, RunPhase};
use gilde_code_lib::events::bus::{EventBus, RecordingSink};
use gilde_code_lib::model::graph::{GraphHandle, GraphState};
use gilde_code_lib::model::status::{SessionStatus, SessionStatusReport};
use gilde_code_lib::paths::AppPaths;
use gilde_code_lib::pty::{OpenSpec, PtyChunk, PtyManager};
use gilde_code_lib::sidecar::{MeshConfig, SidecarManager, SidecarRuntime};
use gilde_code_lib::state::{self, AppState};
use gilde_code_lib::status::StatusEngine;
use gilde_code_lib::store::models::{ProcKind, ProjectRow};
use gilde_code_lib::store::{dao, Db};
use gilde_code_lib::supervise::registry::{EnvBase, Supervisor};
use gilde_code_lib::supervise::spawn::is_alive;
use gilde_code_lib::supervise::watchdog::Watchdog;
use gilde_code_lib::worktree::{naming, WorktreeManager};

use serde_json::json;
use tauri::ipc::Channel;

use fixtures::*;

/// How long a process may take to die before the test calls it a survivor.
const SETTLE: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

fn repo_sidecar_bundle() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("sidecar")
        .join("sidecar.cjs")
}

/// The vendored `gilde-node` when the tree has one, else the developer's `node` —
/// the same ladder `SidecarRuntime::resolve` walks, minus the login-shell probe
/// a test does not need.
fn node_binary() -> PathBuf {
    let vendored = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("binaries")
        .join("gilde-node-aarch64-apple-darwin");
    if vendored.is_file() {
        return vendored;
    }
    which::which("node").expect("a node runtime is required for the boot tests")
}

/// Everything `boot()` builds, built the same way and in the same order, minus
/// the three pieces that need a window: the event bus records instead of
/// emitting, no watcher is armed, and nothing is `app.manage`d.
struct Booted {
    _dir: tempfile::TempDir,
    state: AppState,
    pty: Arc<PtyManager>,
}

impl Booted {
    async fn new() -> Booted {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = std::fs::canonicalize(dir.path()).expect("canonicalize");

        let paths = Arc::new(AppPaths::for_home(&home));
        paths.ensure_all().expect("ensure");

        let db = Db::open(&paths.state_db()).expect("db");
        let config = Arc::new(ConfigStore::open_at(&paths.config_file()).expect("config"));
        // `compose_env` is an allowlist, and the fixture reads its mode from the
        // environment; without this the pty child never sees it.
        config
            .patch(json!({"agents": {"envPassthrough": ["GILDE_TEST_MODE"]}}))
            .expect("patch");

        let graph = GraphHandle::new();
        let bus = EventBus::with_sink(
            Arc::new(RecordingSink::default()),
            Arc::new(parking_lot::RwLock::new(GraphState::new())),
        );

        let supervisor = Arc::new(
            Supervisor::open_instance(&db, config.clone(), "0.0.0-test")
                .await
                .expect("open_instance")
                .with_events(bus.clone()),
        );
        let worktrees = WorktreeManager::new(db.clone(), paths.clone(), Some(bus.clone()));
        let recorder: Recorder = Arc::new(SupervisorRecorder::new(supervisor.clone()));
        let agents = Arc::new(AgentRegistry::new(
            (*paths).clone(),
            config.clone(),
            Some(db.clone()),
            recorder,
        ));
        let accounts = Arc::new(Vec::new());

        let sidecar = SidecarManager::new(
            supervisor.clone(),
            SidecarRuntime::explicit(node_binary(), repo_sidecar_bundle()),
            EnvBase::from_process_env(std::env::var("PATH").unwrap_or_default()),
            MeshConfig::default(),
            Duration::from_secs(5),
        );

        let status = Arc::new(StatusEngine::new());
        let pty = Arc::new(
            PtyManager::new(supervisor.clone(), config.clone())
                .with_status(status.clone())
                .with_events(bus.clone())
                .with_agents(agents.clone())
                .with_accounts(accounts.clone()),
        );
        let watchdog = Arc::new(Watchdog::new(supervisor.clone(), bus.clone()));
        let dispatch = DispatchEngine::new(
            db.clone(),
            paths.clone(),
            config.clone(),
            sidecar.clone(),
            supervisor.clone(),
            worktrees.clone(),
            accounts.clone(),
        );
        // Every input `boot()` gives it: a projector missing one of these
        // reports `down`/zero for that half, and a smoke run would then be
        // measuring the harness rather than the app.
        let projector = Arc::new(
            gilde_code_lib::state::graph::GraphProjector::new(db.clone(), graph.clone())
                .with_events(bus.clone())
                .with_pty(pty.clone())
                .with_watchdog(watchdog.clone())
                .with_sidecar(sidecar.clone())
                .with_accounts(accounts.clone()),
        );

        let state = AppState::new(
            paths,
            db,
            config,
            graph,
            bus,
            supervisor,
            worktrees,
            agents,
            accounts,
            sidecar,
            pty.clone(),
            dispatch,
            status,
            watchdog,
            projector,
            gilde_code_lib::store::now_ms(),
        );

        Booted {
            _dir: dir,
            state,
            pty,
        }
    }

    /// A pty session running the `fake_agent` fixture in `hang` mode: a child
    /// that stays alive until something kills it, which is exactly what a
    /// shutdown has to be able to do.
    async fn open_hanging_session(&self) -> i32 {
        let argv = vec![
            fake_agent_path().to_string_lossy().into_owned(),
            "hang".to_string(),
        ];
        let mut spec = OpenSpec::new(AgentKind::Shell, self._dir.path(), argv);
        spec.idle_after_ms = 250;
        // A sink that drops everything: what the child writes is not under test.
        let channel = Channel::<PtyChunk>::new(|_| Ok(()));
        let info = self.pty.open(spec, channel).await.expect("session opens");
        assert!(is_alive(info.pid), "the child is running before shutdown");
        info.pid
    }

    // -- smoke-run helpers ---------------------------------------------------

    /// `project_add` without the command layer: the same row, from the same
    /// canonicalisation and the same slug function.
    async fn add_project(&self, repo: &Path) -> String {
        let project = ProjectRow {
            id: uuid::Uuid::now_v7().to_string(),
            path: repo.display().to_string(),
            canonical_path: repo.display().to_string(),
            repo_slug: naming::repo_slug(repo),
            name: repo
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "repo".into()),
            color: None,
            default_base_ref: "main".into(),
            added_at: gilde_code_lib::store::now_ms(),
            last_opened_at: None,
        };
        let id = project.id.clone();
        self.state
            .db
            .write(move |conn| dao::project::upsert(conn, &project))
            .await
            .expect("project row");
        id
    }

    /// Wait for a session to reach a *quiescent* classification.
    ///
    /// M1-PLAN T29 acceptance 4 asks for `idle`, and `idle` is what a session
    /// in a worktree the CLI already trusts reaches. In a **fresh** worktree
    /// both `claude` and `codex` put a folder-trust gate up first, and the
    /// honest classification of that screen is `waitingForInput` — so both
    /// count, and `working` (still drawing its splash) does not. Whatever the
    /// session settled on at the deadline is returned either way; the caller
    /// reports it and the smoke script judges.
    async fn settled_status(
        &self,
        session_id: &str,
        timeout: Duration,
    ) -> Option<SessionStatusReport> {
        let deadline = Instant::now() + timeout;
        let mut last = None;
        while Instant::now() < deadline {
            match self.pty.status(session_id) {
                Ok(report) => {
                    let quiescent = matches!(
                        report.status,
                        SessionStatus::Idle | SessionStatus::WaitingForInput { .. }
                    ) && !report.evidence.is_empty();
                    last = Some(report);
                    if quiescent {
                        return last;
                    }
                }
                Err(_) => return last,
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        last
    }

    /// Wait for every run of a fan-out to reach a terminal phase.
    async fn settled_task(
        &self,
        task_id: &str,
        timeout: Duration,
    ) -> gilde_code_lib::dispatch::DispatchTaskDetail {
        let deadline = Instant::now() + timeout;
        loop {
            let detail = self
                .state
                .dispatch
                .get(task_id)
                .await
                .expect("the task is readable");
            let live = detail.runs.iter().any(|run| {
                matches!(
                    run.status,
                    RunPhase::Queued | RunPhase::Spawning | RunPhase::Running
                )
            });
            if !live || Instant::now() >= deadline {
                return detail;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    /// The four vendor binaries the adapters take, from what detection found.
    /// An absent CLI is omitted, never sent as an empty path.
    fn binary_paths(
        &self,
        detected: &[gilde_code_lib::agents::AgentBinary],
    ) -> gilde_code_lib::sidecar::BinaryPaths {
        let path_of = |kind: AgentKind| -> Option<String> {
            detected
                .iter()
                .find(|b| b.kind == kind && b.installed)
                .and_then(|b| b.abs_path.as_ref())
                .map(|p| p.display().to_string())
        };
        gilde_code_lib::sidecar::BinaryPaths {
            claude: path_of(AgentKind::Claude),
            codex: path_of(AgentKind::Codex),
            cursor_agent: path_of(AgentKind::Cursor),
            agy: path_of(AgentKind::Antigravity),
        }
    }

    /// The `gilde/smoke-*` and `gilde/measure-*` branches a run leaves behind.
    ///
    /// It reports them; it does not delete them. `git branch -d` is not in this
    /// app's vocabulary and `tests/worktree_safety.rs` scans this tree for the
    /// literal argv to prove it (DESIGN §7-7) — a smoke run is not the place to
    /// make an exception, so the operator prunes them by hand.
    fn smoke_branches(&self, repo: &Path) -> Vec<String> {
        let listed = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args([
                "branch",
                "--list",
                "gilde/smoke-*",
                "gilde/measure-*",
                "--format=%(refname:short)",
            ])
            .output();
        let Ok(out) = listed else { return Vec::new() };
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::trim)
            .filter(|b| !b.is_empty())
            .map(str::to_string)
            .collect()
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

// ---------------------------------------------------------------------------
// acceptance 2
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_ends_every_pty_child_and_the_sidecar_and_closes_the_instance_row() {
    let booted = Booted::new().await;
    let state = booted.state.clone();

    // Two interactive sessions and one live sidecar — the shape of a real quit.
    let first = booted.open_hanging_session().await;
    let second = booted.open_hanging_session().await;
    state.sidecar.start().await.expect("the sidecar starts");
    // The pid comes from the registry rather than the manager: a sidecar that
    // is not in `proc_registry` before it spawns is a bug of its own (T19), and
    // reading it here means the test cannot pass while that hole exists.
    let instance = state.supervisor.app_instance_id().to_string();
    let sidecar_pid = state
        .db
        .read(move |conn| dao::proc::rows_of_instance(conn, &instance))
        .await
        .expect("registry rows")
        .into_iter()
        .find(|row| row.kind == ProcKind::Sidecar && row.state.is_live())
        .and_then(|row| row.pid)
        .expect("a started sidecar has a live registry row with a pid")
        as i32;
    assert!(is_alive(sidecar_pid), "the sidecar is running before shutdown");

    let report = state::shutdown(&state).await;

    assert_eq!(
        report.survivors, 0,
        "no pid may outlive the shutdown: {report:?}"
    );
    assert!(report.sidecar_stopped, "the sidecar was asked to stop");
    assert!(report.instance_closed, "the app_instance row was closed");
    assert!(
        report.killed >= 2,
        "both pty sessions were swept, got {}",
        report.killed
    );

    for pid in [first, second, sidecar_pid] {
        assert!(
            wait_until(SETTLE, || !is_alive(pid)).await,
            "pid {pid} survived the shutdown"
        );
    }

    // The registry has to agree with the process table, or the next boot's
    // sweep goes looking for processes that are already gone.
    let instance = state.supervisor.app_instance_id().to_string();
    let rows = state
        .db
        .read(move |conn| dao::proc::rows_of_instance(conn, &instance))
        .await
        .expect("registry rows");
    assert!(!rows.is_empty(), "the instance owned rows");
    for row in &rows {
        assert!(
            !row.state.is_live(),
            "{:?} row {} is still {:?} after shutdown",
            row.kind,
            row.spawn_id,
            row.state
        );
    }
    assert!(
        rows.iter().any(|r| r.kind == ProcKind::Pty),
        "the pty rows are the ones under test"
    );
    assert!(
        rows.iter().any(|r| r.kind == ProcKind::Sidecar),
        "the sidecar row is the one the sweep deliberately skips, so the \
         manager's own shutdown must have closed it"
    );

    // `ended_at` is what turns "crashed with orphans" into "quit".
    let id = state.supervisor.app_instance_id().to_string();
    let open = state
        .db
        .read(dao::app_instance::list_open)
        .await
        .expect("open instances");
    assert!(
        !open.iter().any(|i| i.id == id),
        "app_instance.ended_at was not filled; the next boot would read this quit as a crash"
    );
}

/// `RunEvent::Exit` and the signal handler can both arrive. The second one must
/// find the door already closed rather than start a second `ps` sweep while the
/// app is draining.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_shutdown_is_refused_by_the_gate() {
    let booted = Booted::new().await;
    let state = booted.state.clone();

    assert!(!state.is_shutting_down());
    assert!(state.begin_shutdown(), "the first caller goes through");
    assert!(state.is_shutting_down());
    assert!(
        !state.begin_shutdown(),
        "the second caller is turned away, whichever exit path it came from"
    );

    // A clone is the same instance, not a second one: `shutdown_blocking` looks
    // the state up through the app handle and must see the same gate.
    let clone = state.clone();
    assert!(!clone.begin_shutdown());
}

/// A shutdown with nothing to shut down still closes the row. The empty case is
/// the common one — most quits happen with no session open — and it is the one
/// where a `?` on a missing service would silently skip the close.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idle_instance_still_closes_its_row() {
    let booted = Booted::new().await;
    let report = state::shutdown(&booted.state).await;

    assert_eq!(report.killed, 0);
    assert_eq!(report.survivors, 0);
    assert!(report.instance_closed);

    let open = booted
        .state
        .db
        .read(dao::app_instance::list_open)
        .await
        .expect("open instances");
    assert!(open.is_empty(), "no instance row is left open");
}

// ---------------------------------------------------------------------------
// acceptance 4 — the full-path smoke run
// ---------------------------------------------------------------------------
//
// `#[ignore]`d, and driven by `scripts/smoke.mjs`:
//
//     node scripts/smoke.mjs
//
// It is ignored because it drives the **real** vendor CLIs against the network:
// a plain `cargo test` must stay hermetic and must not depend on somebody's
// login session or on a model being reachable today. Every step prints a
// `SMOKE {json}` line, which is the wire `scripts/smoke.mjs` reads — the script
// owns the pass/fail verdict, the leftover-process assertion and the timings,
// because "no process of ours survived" can only be answered from outside the
// process that started them.
//
// Environment (all optional, all set by the script):
//
//   GILDE_SMOKE_REPO      repository to add as a project (default: this one)
//   GILDE_SMOKE_VENDORS   comma-separated vendors        (default: claude,codex)
//   GILDE_SMOKE_PROMPT    the dispatched prompt
//   GILDE_SMOKE_MODEL_<V> pin a model for vendor V, e.g. GILDE_SMOKE_MODEL_CODEX
//   GILDE_SMOKE_PTY_WAIT_S / GILDE_SMOKE_RUN_WAIT_S   patience, in seconds

/// One line of the smoke wire.
fn smoke(step: &str, value: serde_json::Value) {
    let mut payload = json!({ "step": step });
    if let (Some(obj), Some(extra)) = (payload.as_object_mut(), value.as_object()) {
        for (k, v) in extra {
            obj.insert(k.clone(), v.clone());
        }
    }
    println!("SMOKE {payload}");
}

fn env_or(key: &str, fallback: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

fn env_secs(key: &str, fallback: u64) -> Duration {
    Duration::from_secs(
        std::env::var(key)
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(fallback),
    )
}

/// The repository this checkout lives in.
fn this_repo() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or(manifest)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "drives the real vendor CLIs; run it through scripts/smoke.mjs"]
async fn full_path_smoke() {
    let repo = std::fs::canonicalize(PathBuf::from(env_or(
        "GILDE_SMOKE_REPO",
        &this_repo().display().to_string(),
    )))
    .expect("the smoke repository exists");
    let vendors: Vec<String> = env_or("GILDE_SMOKE_VENDORS", "claude,codex")
        .split(',')
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .collect();
    let prompt = env_or(
        "GILDE_SMOKE_PROMPT",
        "Reply with exactly the word OK and nothing else.",
    );
    let pty_wait = env_secs("GILDE_SMOKE_PTY_WAIT_S", 120);
    let run_wait = env_secs("GILDE_SMOKE_RUN_WAIT_S", 300);

    let booted = Booted::new().await;
    let state = booted.state.clone();
    smoke(
        "boot",
        json!({
            "ok": true,
            "repo": repo.display().to_string(),
            "vendors": vendors,
            "appInstance": state.supervisor.app_instance_id(),
            "home": booted._dir.path().display().to_string(),
        }),
    );

    // -- 1. the project, and one worktree per vendor -------------------------
    let project_id = booted.add_project(&repo).await;
    let mut worktrees: Vec<(String, String, PathBuf)> = Vec::new();
    for vendor in &vendors {
        let started = Instant::now();
        let wt = booted
            .state
            .worktrees
            .create(gilde_code_lib::worktree::CreateWorktreeReq {
                project_id: project_id.clone(),
                task_name: format!("smoke-pty-{vendor}"),
                base_ref: None,
                alias_providers: Vec::new(),
                launch: None,
                post_create_cmd: None,
            })
            .await
            .expect("a worktree is created");
        smoke(
            "worktree",
            json!({
                "ok": true,
                "vendor": vendor,
                "id": wt.id,
                "branch": wt.branch,
                "path": wt.path,
                "ms": started.elapsed().as_millis() as u64,
            }),
        );
        worktrees.push((vendor.clone(), wt.id.clone(), PathBuf::from(&wt.path)));
    }
    assert_eq!(worktrees.len(), vendors.len(), "one worktree per vendor");

    // -- 2. an interactive pty per vendor, in its own worktree ---------------
    let detected = state.agents.detect(true).await.unwrap_or_default();
    let installed = |vendor: &str| {
        AgentKind::from_key(vendor)
            .and_then(|kind| detected.iter().find(|b| b.kind == kind))
            .map(|b| b.installed && b.ok)
            .unwrap_or(false)
    };

    let mut sessions: Vec<(String, String, i32)> = Vec::new();
    for (vendor, worktree_id, _) in &worktrees {
        let Some(kind) = AgentKind::from_key(vendor) else {
            smoke("pty", json!({"ok": false, "vendor": vendor, "skipped": "unknown vendor"}));
            continue;
        };
        if !installed(vendor) {
            smoke("pty", json!({"ok": false, "vendor": vendor, "skipped": "not installed"}));
            continue;
        }
        let started = Instant::now();
        // A real winsize: `codex` renders nothing into an 0x0 pty.
        let req = gilde_code_lib::pty::OpenSessionReq {
            project_id: project_id.clone(),
            worktree_id: worktree_id.clone(),
            agent: kind,
            account_id: None,
            cols: 120,
            rows: 32,
            title: None,
            prompt: None,
            args: None,
        };
        let channel = Channel::<PtyChunk>::new(|_| Ok(()));
        let info = match booted.pty.open_request(req, channel).await {
            Ok(info) => info,
            Err(err) => {
                smoke("pty", json!({"ok": false, "vendor": vendor, "error": err.to_string()}));
                continue;
            }
        };

        // The status engine classifies from the rendered screen. A *fresh*
        // worktree makes both CLIs ask for folder trust first, so the settled
        // state is `waitingForInput` as often as it is `idle` — both are
        // classifications with evidence, which is the property under test; a
        // session stuck on `starting` is not.
        let report = booted
            .settled_status(&info.id, pty_wait)
            .await
            .expect("the session reports a status");
        smoke(
            "pty",
            json!({
                "ok": true,
                "vendor": vendor,
                "sessionId": info.id,
                "pid": info.pid,
                "status": report.status,
                "confidence": report.confidence,
                "evidence": report.evidence,
                "ms": started.elapsed().as_millis() as u64,
            }),
        );
        sessions.push((vendor.clone(), info.id.clone(), info.pid));
    }

    // -- 3. one prompt, dispatched headless to every vendor ------------------
    let mut mesh = MeshConfig::default();
    mesh.binaries = booted.binary_paths(&detected);
    state.sidecar.configure(mesh).await.expect("configure the fleet");
    match state.sidecar.start().await {
        Ok(()) => smoke("sidecar", json!({"ok": true})),
        Err(err) => smoke("sidecar", json!({"ok": false, "error": err.to_string()})),
    }

    let targets: Vec<gilde_code_lib::dispatch::DispatchTarget> = vendors
        .iter()
        .filter(|v| installed(v))
        .map(|vendor| {
            let mut target = gilde_code_lib::dispatch::DispatchTarget::new(vendor.clone());
            // The model is pinned from the outside, never guessed: this
            // machine's `codex` cannot reach its own default, and a smoke run
            // that silently used a different model would be telling us nothing.
            target.model = std::env::var(format!("GILDE_SMOKE_MODEL_{}", vendor.to_uppercase()))
                .ok()
                .filter(|m| !m.trim().is_empty());
            target
        })
        .collect();

    if !targets.is_empty() {
        let started = Instant::now();
        let task = state
            .dispatch
            .create(gilde_code_lib::dispatch::DispatchSpec {
                prompt: prompt.clone(),
                project_id: project_id.clone(),
                base_ref: None,
                task_slug: "smoke-run".into(),
                targets: targets.clone(),
                worktree_strategy:
                    gilde_code_lib::dispatch::WorktreeStrategy::PerTarget,
                timeout_ms: Some(run_wait.as_millis() as u64),
                permission_mode: None,
                title: Some("smoke".into()),
            })
            .await
            .expect("the fan-out is created");

        let detail = booted.settled_task(&task.id, run_wait).await;
        for run in &detail.runs {
            let dir = state.dispatch.artifact_dir(&run.id).await.ok();
            let result_json = dir.as_ref().map(|d| d.join("result.json"));
            smoke(
                "run",
                json!({
                    "ok": run.status == gilde_code_lib::dispatch::RunPhase::Done,
                    "vendor": run.vendor,
                    "runId": run.id,
                    "status": run.status,
                    "model": run.model_actual,
                    "usage": run.usage,
                    "costUsd": run.cost_usd,
                    "error": run.error,
                    "note": run.note,
                    "resultJson": result_json.as_ref().map(|p| p.display().to_string()),
                    "resultJsonExists": result_json.as_ref().map(|p| p.is_file()).unwrap_or(false),
                    "textBytes": run.text.len(),
                    "ms": started.elapsed().as_millis() as u64,
                }),
            );
        }
    }

    // -- 3b. the dashboard graph, from the same projection the canvas reads --
    //
    // M1-PLAN T29's manual scenario 5 asks the dashboard to show
    // project → worktree → session → task live. The canvas is T25's surface and
    // its rendering is tested there; what can be pinned from here is the thing
    // it renders — one projection of the live state, with a node per real
    // object and an edge per real relationship.
    // One watchdog tick first: RSS and the orphan count come from its scan, and
    // in the app it has been ticking every five seconds since boot.
    let _ = state.watchdog.tick().await;
    match state
        .projector
        .reproject(gilde_code_lib::state::graph::Publish::Direct)
        .await
    {
        Ok(_) => {
            let snapshot = state.graph.snapshot();
            let mut by_kind: std::collections::BTreeMap<String, usize> =
                std::collections::BTreeMap::new();
            for node in &snapshot.nodes {
                *by_kind
                    .entry(serde_json::to_value(node.kind)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_string))
                        .unwrap_or_else(|| "?".into()))
                    .or_default() += 1;
            }
            smoke(
                "graph",
                json!({
                    "ok": !snapshot.nodes.is_empty(),
                    "nodes": snapshot.nodes.len(),
                    "edges": snapshot.edges.len(),
                    "byKind": by_kind,
                    "totals": snapshot.totals,
                }),
            );
        }
        Err(err) => smoke("graph", json!({"ok": false, "error": err.to_string()})),
    }

    // -- 4. every vendor process this run spawned is in the registry ---------
    let instance = state.supervisor.app_instance_id().to_string();
    let rows = state
        .db
        .read(move |conn| dao::proc::rows_of_instance(conn, &instance))
        .await
        .expect("registry rows");
    smoke(
        "registry",
        json!({
            "ok": !rows.is_empty(),
            "rows": rows.iter().map(|r| json!({
                "kind": r.kind,
                "vendor": r.vendor,
                "state": r.state,
                "pid": r.pid,
            })).collect::<Vec<_>>(),
        }),
    );
    for (vendor, _, pid) in &sessions {
        let found = rows
            .iter()
            .any(|r| r.pid == Some(*pid as i64) && r.vendor.as_deref() == Some(vendor.as_str()));
        assert!(found, "the {vendor} pty child {pid} is not in the registry");
    }

    // -- 5. the way out ------------------------------------------------------
    let pids: Vec<i32> = rows.iter().filter_map(|r| r.pid).map(|p| p as i32).collect();
    let started = Instant::now();
    let report = state::shutdown(&state).await;
    let alive: Vec<i32> = pids.into_iter().filter(|p| is_alive(*p)).collect();
    smoke(
        "shutdown",
        json!({
            "ok": report.survivors == 0 && alive.is_empty(),
            "killed": report.killed,
            "survivors": report.survivors,
            "stillAlive": alive,
            "sidecarStopped": report.sidecar_stopped,
            "instanceClosed": report.instance_closed,
            "ms": started.elapsed().as_millis() as u64,
        }),
    );

    // -- 6. leave the repository as we found it ------------------------------
    //
    // Every live worktree of the project, not just the two this test created:
    // `WorktreeStrategy::PerTarget` made one more per dispatch target, and a
    // linked worktree whose directory is deleted with the temp home would leave
    // a stale entry in the *user's* `.git/worktrees`.
    let live = state
        .db
        .read(dao::worktree::list_live)
        .await
        .unwrap_or_default();
    for row in live.iter().filter(|w| w.project_id == project_id) {
        let outcome = state
            .worktrees
            .remove(
                &row.id,
                gilde_code_lib::worktree::RemoveMode::KeepBranch,
            )
            .await;
        smoke(
            "cleanup",
            json!({
                "ok": outcome.is_ok(),
                "slug": row.slug,
                "path": row.path,
                "error": outcome.as_ref().err().map(|e| e.to_string()),
            }),
        );
    }
    let branches = booted.smoke_branches(&repo);
    smoke(
        "branches",
        json!({
            "ok": true,
            "kept": branches,
            "note": "the app never deletes a branch (DESIGN §7-7); prune them by hand",
        }),
    );

    assert_eq!(report.survivors, 0, "the shutdown left survivors");
    assert!(alive.is_empty(), "processes outlived the shutdown: {alive:?}");
}

// ---------------------------------------------------------------------------
// acceptance 7 — the numbers docs/M1-VERIFICATION.md records
// ---------------------------------------------------------------------------

/// `#[ignore]`d because it is a measurement, not an assertion: it reports what
/// this machine does rather than gating a build on it.
///
///     cargo test --test boot -- --ignored --nocapture measurements
///
/// Two of the five numbers M1-PLAN T29 acceptance 7 asks for live here — the
/// `ps` snapshot and the graph snapshot. The other three come from where they
/// are actually observable: PTY throughput from
/// `tests/pty_fidelity.rs::four_concurrent_streams…` (R1), boot time from the
/// app's own `boot complete` line, and app RSS from `ps` against a running
/// window.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "measurement; run it explicitly"]
async fn measurements() {
    use gilde_code_lib::supervise::procscan;

    // -- ps snapshot ---------------------------------------------------------
    // The whole process table, parsed and indexed by pid and by pgid. This is
    // what every watchdog tick, every fence check and every kill plan pays.
    let mut rows = 0;
    let mut samples = Vec::new();
    for _ in 0..10 {
        let started = Instant::now();
        let snap = procscan::ps_snapshot().expect("ps");
        samples.push(started.elapsed().as_secs_f64() * 1000.0);
        rows = snap.len();
    }
    samples.sort_by(f64::total_cmp);
    smoke(
        "measure.ps_snapshot",
        json!({
            "rows": rows,
            "medianMs": (samples[samples.len() / 2] * 100.0).round() / 100.0,
            "minMs": (samples[0] * 100.0).round() / 100.0,
            "maxMs": (samples[samples.len() - 1] * 100.0).round() / 100.0,
        }),
    );

    // -- graph snapshot ------------------------------------------------------
    // `graph_snapshot()` is a read of `GraphState` plus a serialisation; the
    // projection that fills it is measured separately in `state::graph`'s own
    // tests. Both are on the boot path and both are on every reconnect.
    let booted = Booted::new().await;
    let repo = std::fs::canonicalize(this_repo()).expect("repo");
    let project_id = booted.add_project(&repo).await;
    for i in 0..8 {
        let _ = booted
            .state
            .worktrees
            .create(gilde_code_lib::worktree::CreateWorktreeReq {
                project_id: project_id.clone(),
                task_name: format!("measure-{i}"),
                base_ref: None,
                alias_providers: Vec::new(),
                launch: None,
                post_create_cmd: None,
            })
            .await;
    }
    let projected = {
        let started = Instant::now();
        let report = booted
            .state
            .projector
            .reproject(gilde_code_lib::state::graph::Publish::Direct)
            .await
            .expect("projection");
        (started.elapsed().as_secs_f64() * 1000.0, report)
    };

    let mut snap_samples = Vec::new();
    let mut bytes = 0;
    for _ in 0..20 {
        let started = Instant::now();
        let snapshot = booted.state.graph.snapshot();
        let json = serde_json::to_vec(&snapshot).expect("serialize");
        snap_samples.push(started.elapsed().as_secs_f64() * 1000.0);
        bytes = json.len();
    }
    snap_samples.sort_by(f64::total_cmp);
    smoke(
        "measure.graph_snapshot",
        json!({
            "nodes": projected.1.nodes,
            "edges": projected.1.edges,
            "reprojectMs": (projected.0 * 100.0).round() / 100.0,
            "medianMs": (snap_samples[snap_samples.len() / 2] * 1000.0).round() / 1000.0,
            "maxMs": (snap_samples[snap_samples.len() - 1] * 1000.0).round() / 1000.0,
            "jsonBytes": bytes,
        }),
    );

    // -- RSS with four agent sessions ---------------------------------------
    //
    // Four *real* `claude` TUIs, one per worktree — the shape M1-PLAN T29
    // acceptance 7 asks about. Opening the TUI costs no tokens: nothing is
    // sent, the CLI just draws. The numbers come from the app's own instrument
    // (`Watchdog::tick`, which is the `ps` snapshot the Process Monitor and the
    // budget bar read), plus this process's own RSS — the host half of the
    // total. The WebKit half is not in this process and is measured against a
    // running window instead; `docs/M1-VERIFICATION.md` adds the two up.
    let claude_installed = booted
        .state
        .agents
        .detect(true)
        .await
        .unwrap_or_default()
        .iter()
        .any(|b| b.kind == AgentKind::Claude && b.installed && b.ok);

    if claude_installed {
        let live = booted
            .state
            .db
            .read(dao::worktree::list_live)
            .await
            .unwrap_or_default();
        let mut opened = 0;
        for row in live
            .iter()
            .filter(|w| w.project_id == project_id)
            .take(4)
        {
            let req = gilde_code_lib::pty::OpenSessionReq {
                project_id: project_id.clone(),
                worktree_id: row.id.clone(),
                agent: AgentKind::Claude,
                account_id: None,
                cols: 120,
                rows: 32,
                title: None,
                prompt: None,
                args: None,
            };
            let channel = Channel::<PtyChunk>::new(|_| Ok(()));
            if booted.pty.open_request(req, channel).await.is_ok() {
                opened += 1;
            }
        }
        // Let the four TUIs finish drawing before anything is weighed: a CLI
        // that is still loading its bundle has not reached its working set.
        tokio::time::sleep(Duration::from_secs(20)).await;

        let report = booted.state.watchdog.tick().await;
        let scan = booted.state.watchdog.latest_scan();
        let host_rss = procscan::ps_snapshot()
            .ok()
            .and_then(|snap| snap.get(std::process::id() as i32).map(|r| r.rss_kb * 1024))
            .unwrap_or(0);
        // `ProcTotals` counts the whole machine (that is what the budget bar
        // is *for*), so ours is summed separately: the rows the scanner
        // attributed to this instance, plus every descendant of them.
        let ours: Vec<_> = scan
            .as_ref()
            .map(|s| {
                s.rows
                    .iter()
                    .filter(|r| {
                        matches!(
                            r.group,
                            gilde_code_lib::supervise::ProcGroupKind::OursLive
                        )
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let ours_rss: u64 = ours.iter().map(|r| r.rss_bytes).sum();
        smoke(
            "measure.rss",
            json!({
                "sessionsOpened": opened,
                "sessionsSampled": report.sessions,
                "hostRssBytes": host_rss,
                "oursProcesses": ours.len(),
                "oursRssBytes": ours_rss,
                "oursRows": ours.iter().map(|r| json!({
                    "name": r.name,
                    "rssBytes": r.rss_bytes,
                })).collect::<Vec<_>>(),
                "machineRssBytes": scan.as_ref().map(|s| s.totals.rss_bytes),
                "machineProcesses": scan.as_ref().map(|s| s.totals.processes),
                "physicalRamBytes": scan.as_ref().map(|s| s.totals.physical_ram_bytes),
            }),
        );
    } else {
        smoke("measure.rss", json!({ "skipped": "claude is not installed" }));
    }

    // Leave the repository as we found it — sessions first, then worktrees.
    let shutdown = state::shutdown(&booted.state).await;
    smoke(
        "measure.shutdown",
        json!({ "killed": shutdown.killed, "survivors": shutdown.survivors }),
    );
    let live = booted
        .state
        .db
        .read(dao::worktree::list_live)
        .await
        .unwrap_or_default();
    for row in live.iter().filter(|w| w.project_id == project_id) {
        let _ = booted
            .state
            .worktrees
            .remove(
                &row.id,
                gilde_code_lib::worktree::RemoveMode::KeepBranch,
            )
            .await;
    }
    smoke(
        "measure.branches",
        json!({ "kept": booted.smoke_branches(&repo) }),
    );
}
