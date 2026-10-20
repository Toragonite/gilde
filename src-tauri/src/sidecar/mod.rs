//! The Node sidecar: runtime resolution, lifecycle, health, and the supervision
//! seam that keeps its grandchildren visible (DESIGN §1, §5, §8-3).
//!
//! The sidecar is the headless lane. It embeds `@toragonite/agent-mesh` and the
//! four vendor adapters behind an NDJSON JSON-RPC channel ([`protocol`],
//! [`client`]) and mirrors agent-mesh's data contract field for field
//! ([`types`]). What lives *here* is everything that owns a **process**:
//!
//!   * **Runtime resolution** ([`SidecarRuntime`]) — the vendored `gilde-node` next
//!     to the executable in production, `CARGO_MANIFEST_DIR/sidecar/sidecar.cjs`
//!     plus a login-shell-resolved `node` under `GILDE_SIDECAR_DEV=1`, and a
//!     system-node fallback that is reported rather than hidden.
//!   * **A self-test before the real spawn** — `<runtime> --version` under a 5 s
//!     cap, so a broken runtime is [`GildeError::SidecarRuntimeUnusable`] with the
//!     path and the stderr instead of a mysterious hang.
//!   * **Write-before-spawn** — every process this module starts, the sidecar
//!     itself and the `--version` probe included, goes through
//!     [`Supervisor::plan_spawn`] and a [`SpawnGuard`] (DESIGN §8-3), and
//!     `host.registerSpawn` extends the same guarantee to the children the
//!     sidecar spawns: the durable row lands **before** the response that
//!     unblocks the spawn, so a headless `claude` shows up in the Process
//!     Monitor within one scan.
//!   * **Health and restart** — `mesh.ping` every 10 s with a 5 s deadline,
//!     three consecutive misses ⇒ hang-kill, restart backoff
//!     `[1, 2, 5, 15, 60]s`, and a circuit breaker at five crashes under 5 s of
//!     uptime that stops restarting and surfaces the last 200 stderr lines.
//!   * **Crash semantics** — in-flight requests reject as
//!     [`SidecarCallError::Gone`], the affected runs settle as `interrupted`
//!     rather than `error`, and [`GenerationSweeper::sweep`] runs **before** the
//!     replacement sidecar can accept work, so a crash loop cannot pile up
//!     orphaned `claude -p` children.
//!
//! **The PTY lane never depends on any of this.** A sidecar that is down, hung
//! or refusing to start leaves the interactive lane fully usable — that
//! isolation is deliberate (DESIGN §5-3) and nothing in this module may weaken it.

pub mod client;
pub mod protocol;
pub mod team_cache;
pub mod types;

use std::collections::VecDeque;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use parking_lot::Mutex;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{broadcast, mpsc};

use crate::error::{GildeError, Result};
use crate::events::{SidecarStateEvent, TeamSnapshotEvent, SIDECAR_STATE, TEAM_SNAPSHOT};
use crate::events::bus::EventBus;
use crate::model::graph::SidecarState;
use crate::store::models::ProcKind;
use crate::supervise::registry::{EnvBase, SpawnGuard, SpawnPlan, Supervisor};
use crate::supervise::spawn::{is_alive, tokio_command_grouped};

pub use client::{ClientFault, HostFuture, HostHandler, SidecarCallError, SidecarClient};
pub use protocol::{code, host_method, method, RpcErrorObject, MAX_LINE_BYTES, PROTOCOL_VERSION};
pub use types::*;
pub use team_cache::TeamCache;

/// Wall clock for the `--version` self-test (DESIGN §5-11).
pub const SELF_TEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Deadline for one `mesh.ping` (DESIGN §5-3).
pub const PING_DEADLINE: Duration = Duration::from_secs(5);
/// Consecutive missed pings before the sidecar is declared *hung* (not dead).
pub const PING_MISSES_BEFORE_HANG: u32 = 3;
/// Restart backoff ladder, in seconds (DESIGN §5-3).
pub const RESTART_BACKOFF_SECS: [u64; 5] = [1, 2, 5, 15, 60];
/// A start that dies sooner than this counts against the circuit breaker.
pub const CRASH_WINDOW: Duration = Duration::from_secs(5);
/// Crashes inside [`CRASH_WINDOW`] before restarts stop (DESIGN §5-3).
pub const CRASH_LIMIT: u32 = 5;
/// stderr lines kept for the "Sidecar log" drawer and the down banner.
pub const STDERR_TAIL_LINES: usize = 200;
/// Grace between the shutdown/hang `SIGTERM` and its `SIGKILL`.
pub const TERMINATE_GRACE: Duration = Duration::from_secs(3);
/// Grace handed to `mesh.shutdown` before the signal path takes over.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
/// The host pid, handed to the sidecar's parent-death watchdog (INT-C).
///
/// The sidecar polls it every 2 s with signal 0 and exits when it is gone. This
/// is the third of its three exit paths and the only one that survives *this*
/// process being `SIGKILL`ed: no stream closes in that case on macOS — the child
/// is simply reparented to `launchd` and keeps holding whatever it spawned.
///
/// The budgets on the two sides are deliberately ordered: the sidecar's signal
/// path exits within 2 s, inside [`TERMINATE_GRACE`], so a healthy stop is
/// recorded as `SIGTERM` and never reaches the escalation.
pub const ENV_HOST_PID: &str = "GILDE_HOST_PID";

// ---------------------------------------------------------------------------
// runtime resolution
// ---------------------------------------------------------------------------

/// Where the Node runtime came from. Surfaced in diagnostics because
/// "it works in dev, not from Finder" is almost always this field (DESIGN §5-11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NodeSource {
    /// The pinned `gilde-node` shipped inside the bundle.
    Vendored,
    /// `config.sidecar.nodePath`.
    Configured,
    /// A `node` found on the resolved `PATH`. Reported as a warning: it is not
    /// the runtime we pinned and it may vanish under the user.
    System,
}

impl NodeSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            NodeSource::Vendored => "vendored",
            NodeSource::Configured => "configured",
            NodeSource::System => "system",
        }
    }
}

/// The two paths a sidecar generation needs, plus how they were found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SidecarRuntime {
    pub node: PathBuf,
    pub script: PathBuf,
    pub node_source: NodeSource,
    /// Non-fatal problems worth showing in `app_diagnostics()`.
    #[serde(default)]
    pub warnings: Vec<String>,
}

impl SidecarRuntime {
    /// Resolve the runtime for this build.
    ///
    /// `search_path` is the login-shell-resolved `PATH` (T09's `PathReport`),
    /// not the app's own: a bundle launched from Finder cannot see nvm, which is
    /// exactly where this machine's `node` lives.
    pub fn resolve(
        resource_script: Option<PathBuf>,
        configured_node: Option<&str>,
        search_path: &[PathBuf],
    ) -> Result<SidecarRuntime> {
        let dev = std::env::var("GILDE_SIDECAR_DEV").ok().as_deref() == Some("1");
        let mut warnings = Vec::new();

        let script = if dev {
            dev_script()
        } else {
            match resource_script {
                Some(path) if path.exists() => path,
                other => {
                    if let Some(path) = other {
                        warnings.push(format!(
                            "bundled sidecar.cjs not found at {}; using the source tree copy",
                            path.display()
                        ));
                    }
                    dev_script()
                }
            }
        };
        if !script.exists() {
            return Err(GildeError::SidecarRuntimeUnusable {
                path: script.display().to_string(),
                stderr: "sidecar.cjs is missing; run `npm run sidecar:build`".to_string(),
            });
        }

        // 1. an explicit override always wins.
        if let Some(configured) = configured_node.filter(|s| !s.trim().is_empty()) {
            let node = PathBuf::from(configured);
            if crate::agents::path::is_executable(&node) {
                return Ok(SidecarRuntime {
                    node,
                    script,
                    node_source: NodeSource::Configured,
                    warnings,
                });
            }
            warnings.push(format!(
                "config.sidecar.nodePath ({configured}) is not executable; falling back"
            ));
        }

        // 2. the vendored binary, unless we are deliberately in dev mode.
        if !dev {
            if let Some(node) = vendored_node() {
                return Ok(SidecarRuntime {
                    node,
                    script,
                    node_source: NodeSource::Vendored,
                    warnings,
                });
            }
            warnings.push(
                "the vendored gilde-node binary is missing; falling back to the system node"
                    .to_string(),
            );
        }

        // 3. a system node off the resolved PATH.
        match which_on(search_path, "node") {
            Some(node) => {
                warnings.push(format!(
                    "using the system node at {} (nodeSource: system)",
                    node.display()
                ));
                Ok(SidecarRuntime {
                    node,
                    script,
                    node_source: NodeSource::System,
                    warnings,
                })
            }
            None => Err(GildeError::SidecarRuntimeUnusable {
                path: "node".to_string(),
                stderr: "no vendored gilde-node and no node on the resolved PATH".to_string(),
            }),
        }
    }

    /// The production entry point: resolve against a live Tauri app.
    ///
    /// The bundled script is `sidecar/sidecar.cjs` under
    /// [`tauri::path::BaseDirectory::Resource`] — the path `tauri.conf.json`
    /// declares in `bundle.resources`. Everything else is
    /// [`SidecarRuntime::resolve`]'s ladder.
    pub fn resolve_for_app(
        app: &tauri::AppHandle,
        configured_node: Option<&str>,
        search_path: &[PathBuf],
    ) -> Result<SidecarRuntime> {
        use tauri::Manager as _;
        let resource = app
            .path()
            .resolve("sidecar/sidecar.cjs", tauri::path::BaseDirectory::Resource)
            .ok();
        SidecarRuntime::resolve(resource, configured_node, search_path)
    }

    /// An explicit pair, for tests and for a caller that already knows both.
    pub fn explicit(node: impl Into<PathBuf>, script: impl Into<PathBuf>) -> SidecarRuntime {
        SidecarRuntime {
            node: node.into(),
            script: script.into(),
            node_source: NodeSource::Configured,
            warnings: Vec::new(),
        }
    }
}

fn dev_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("sidecar")
        .join("sidecar.cjs")
}

/// `current_exe()/../gilde-node` — where Tauri places an `externalBin`.
fn vendored_node() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let candidate = exe.parent()?.join("gilde-node");
    crate::agents::path::is_executable(&candidate).then_some(candidate)
}

fn which_on(search_path: &[PathBuf], name: &str) -> Option<PathBuf> {
    search_path
        .iter()
        .map(|dir| dir.join(name))
        .find(|c| crate::agents::path::is_executable(c))
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

/// What `sidecar_status` returns — mirrored by `ipc.ts::SidecarStatus`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SidecarStatus {
    pub state: SidecarState,
    pub generation: i64,
    pub restarts: u32,
    pub last_error: Option<String>,
    pub node_path: Option<String>,
    pub script_path: Option<String>,
}

/// Broadcast when a generation dies, so the dispatch engine can settle the runs
/// it interrupted as `interrupted` — never as `error` (DESIGN §5-10 rule 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidecarDeath {
    pub generation: i64,
    pub reason: String,
    /// `true` when the app asked for it (shutdown or an explicit restart).
    pub deliberate: bool,
}

/// The reaper hook. T11 installs `reaper::sweep_generation`; until then a
/// caller may install its own, and the manager simply has nothing to call.
///
/// It is awaited **before the replacement sidecar can accept work**, which is
/// the property that keeps a crash loop from accumulating orphans.
pub trait GenerationSweeper: Send + Sync + 'static {
    fn sweep(&self, dead_generation: i64) -> Pin<Box<dyn Future<Output = ()> + Send>>;
}

// ---------------------------------------------------------------------------
// manager
// ---------------------------------------------------------------------------

struct LiveState {
    state: SidecarState,
    generation: i64,
    restarts: u32,
    crash_streak: u32,
    last_error: Option<String>,
    client: Option<SidecarClient>,
    child_pid: Option<i32>,
    guard: Option<Arc<SpawnGuard>>,
    started_at: Option<Instant>,
    hello: Option<HelloResult>,
}

impl LiveState {
    fn new() -> LiveState {
        LiveState {
            state: SidecarState::Down,
            generation: 0,
            restarts: 0,
            crash_streak: 0,
            last_error: None,
            client: None,
            child_pid: None,
            guard: None,
            started_at: None,
            hello: None,
        }
    }
}

/// Everything `mesh.hello` / `mesh.configure` need to build the fleet.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MeshConfig {
    pub binaries: BinaryPaths,
    pub accounts: Vec<Account>,
    pub defaults: MeshDefaults,
}

/// Owns the sidecar process across restarts.
///
/// Held in Tauri managed state as `Arc<SidecarManager>`; every lifecycle method
/// takes `&Arc<Self>` because the supervision tasks outlive the call that
/// started them and hold a [`Weak`] back.
pub struct SidecarManager {
    supervisor: Arc<Supervisor>,
    runtime: SidecarRuntime,
    env_base: EnvBase,
    mesh: Mutex<MeshConfig>,
    state: Mutex<LiveState>,
    events: Mutex<Option<EventBus>>,
    sweeper: Mutex<Option<Arc<dyn GenerationSweeper>>>,
    /// The sidecar's own spawn id → our registry guard. Keyed by *its* id
    /// because that is what `host.spawned` / `host.exited` carry; the row's own
    /// id is ours and is never sent back.
    guards: DashMap<String, Arc<SpawnGuard>>,
    stderr_tail: Mutex<VecDeque<String>>,
    /// Why the current generation is about to die, when we know better than the
    /// exit status does. A `SIGKILL` we sent ourselves says "signal 9"; this
    /// says "it sent a 9 MiB line", which is the sentence the user needs.
    fault_reason: Mutex<Option<String>>,
    run_events: broadcast::Sender<RunEventEnvelope>,
    statuses: broadcast::Sender<SidecarStatus>,
    deaths: broadcast::Sender<SidecarDeath>,
    ping_interval: Duration,
    ping_nonce: AtomicU64,
    /// Serialises start / restart / shutdown against each other.
    lifecycle: tokio::sync::Mutex<()>,
    shutting_down: AtomicBool,
    /// The team views relayed so far — the health tick's team facts.
    team_cache: Arc<TeamCache>,
}

impl std::fmt::Debug for SidecarManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.state.lock();
        f.debug_struct("SidecarManager")
            .field("state", &state.state)
            .field("generation", &state.generation)
            .field("restarts", &state.restarts)
            .field("node", &self.runtime.node)
            .finish()
    }
}

impl SidecarManager {
    /// Build a manager. Nothing is spawned until [`SidecarManager::start`].
    pub fn new(
        supervisor: Arc<Supervisor>,
        runtime: SidecarRuntime,
        env_base: EnvBase,
        mesh: MeshConfig,
        ping_interval: Duration,
    ) -> Arc<SidecarManager> {
        let (run_events, _) = broadcast::channel(1024);
        let (statuses, _) = broadcast::channel(64);
        let (deaths, _) = broadcast::channel(64);
        Arc::new(SidecarManager {
            supervisor,
            runtime,
            env_base,
            mesh: Mutex::new(mesh),
            state: Mutex::new(LiveState::new()),
            events: Mutex::new(None),
            sweeper: Mutex::new(None),
            guards: DashMap::new(),
            stderr_tail: Mutex::new(VecDeque::with_capacity(STDERR_TAIL_LINES)),
            fault_reason: Mutex::new(None),
            run_events,
            statuses,
            deaths,
            ping_interval,
            ping_nonce: AtomicU64::new(1),
            lifecycle: tokio::sync::Mutex::new(()),
            shutting_down: AtomicBool::new(false),
            team_cache: Arc::new(TeamCache::new()),
        })
    }

    // -- wiring --------------------------------------------------------------

    /// Attach the event bus. Separate from construction because the bus needs a
    /// window and the manager is built during boot.
    pub fn set_events(&self, events: EventBus) {
        *self.events.lock() = Some(events);
    }

    /// The team views relayed so far (`supervise::health` reads them).
    pub fn team_cache(&self) -> &Arc<TeamCache> {
        &self.team_cache
    }

    /// Install the reaper hook (T11's `sweep_generation`).
    pub fn set_sweeper(&self, sweeper: Arc<dyn GenerationSweeper>) {
        *self.sweeper.lock() = Some(sweeper);
    }

    /// Replace the fleet configuration. Takes effect on the next handshake, and
    /// immediately through `mesh.configure` when a sidecar is up.
    pub async fn configure(&self, mesh: MeshConfig) -> Result<()> {
        *self.mesh.lock() = mesh.clone();
        if self.client().is_some() {
            let params = ConfigureParams {
                binaries: mesh.binaries,
                accounts: mesh.accounts,
            };
            let _: Value = self.call(method::CONFIGURE, params).await?;
        }
        Ok(())
    }

    /// Every `mesh.event` notification, tagged with the generation that produced
    /// it. The dispatch engine (T22) is the intended subscriber.
    pub fn subscribe_run_events(&self) -> broadcast::Receiver<RunEventEnvelope> {
        self.run_events.subscribe()
    }

    /// Lifecycle transitions, for anything that must react to `down`/`ready`.
    pub fn subscribe_status(&self) -> broadcast::Receiver<SidecarStatus> {
        self.statuses.subscribe()
    }

    /// One message per dead generation. Runs that were in flight belong in
    /// `interrupted` (DESIGN §5-10).
    pub fn subscribe_deaths(&self) -> broadcast::Receiver<SidecarDeath> {
        self.deaths.subscribe()
    }

    // -- introspection -------------------------------------------------------

    pub fn generation(&self) -> i64 {
        self.state.lock().generation
    }

    pub fn status(&self) -> SidecarStatus {
        let state = self.state.lock();
        SidecarStatus {
            state: state.state,
            generation: state.generation,
            restarts: state.restarts,
            last_error: state.last_error.clone(),
            node_path: Some(self.runtime.node.display().to_string()),
            script_path: Some(self.runtime.script.display().to_string()),
        }
    }

    pub fn runtime(&self) -> &SidecarRuntime {
        &self.runtime
    }

    /// The handshake result of the live generation, when there is one.
    pub fn hello(&self) -> Option<HelloResult> {
        self.state.lock().hello.clone()
    }

    /// The adapter roster, read off the live handshake. Empty while down.
    pub fn adapters(&self) -> Vec<AdapterInfo> {
        self.state
            .lock()
            .hello
            .as_ref()
            .map(|h| h.adapters.clone())
            .unwrap_or_default()
    }

    /// Whether a vendor streams — read from `AdapterCapabilities`, never from a
    /// hardcoded list (DESIGN §5-8).
    pub fn can_stream(&self, vendor: &str) -> bool {
        self.state
            .lock()
            .hello
            .as_ref()
            .map(|h| h.can_stream(vendor))
            .unwrap_or(false)
    }

    /// The last [`STDERR_TAIL_LINES`] stderr lines — the down banner's content.
    pub fn stderr_tail(&self) -> Vec<String> {
        self.stderr_tail.lock().iter().cloned().collect()
    }

    fn client(&self) -> Option<SidecarClient> {
        self.state.lock().client.clone()
    }

    // -- calls ---------------------------------------------------------------

    /// Call the sidecar, mapping every failure to an [`GildeError`].
    pub async fn call<P: Serialize, R: DeserializeOwned>(&self, method: &str, params: P) -> Result<R> {
        self.call_detailed(method, params, None)
            .await
            .map_err(GildeError::from)
    }

    /// The same call, keeping the typed failure.
    ///
    /// Callers that must distinguish "the agent failed" from "the app failed"
    /// (the dispatch engine settling a run as `error` versus `interrupted`) use
    /// this one and read [`SidecarCallError::is_infrastructure`].
    pub async fn call_detailed<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: P,
        timeout: Option<Duration>,
    ) -> std::result::Result<R, SidecarCallError> {
        let client = self.client().ok_or(SidecarCallError::Down)?;
        client.call(method, params, timeout).await
    }

    // -- lifecycle -----------------------------------------------------------

    /// Start the sidecar: resolve, self-test, spawn, handshake.
    ///
    /// Returns once `mesh.hello` has answered — a caller that gets `Ok` can
    /// immediately issue `mesh.*` calls. A failure leaves the manager `down`
    /// with `last_error` set; the app stays usable either way.
    pub async fn start(self: &Arc<Self>) -> Result<()> {
        let _lock = self.lifecycle.lock().await;
        self.shutting_down.store(false, Ordering::SeqCst);
        if self.state.lock().client.is_some() {
            return Ok(());
        }
        // `sidecar.enabled = false` is a supported configuration, not an error:
        // the interactive lane is fully usable without the headless one, and the
        // UI reads `down` with a reason rather than a failure toast.
        if !self.supervisor.config().get().sidecar.enabled {
            tracing::info!(target: "sidecar", "the sidecar is disabled in config; not starting it");
            self.set_state(
                SidecarState::Down,
                Some("disabled in settings (sidecar.enabled = false)".to_string()),
            );
            return Ok(());
        }
        self.start_locked().await
    }

    /// Kill the current generation and start a fresh one. Resets the breaker,
    /// because this is a deliberate user action.
    pub async fn restart(self: &Arc<Self>) -> Result<SidecarStatus> {
        let _lock = self.lifecycle.lock().await;
        self.shutting_down.store(false, Ordering::SeqCst);
        if let Some(dead_generation) = self.take_down("restart requested", true).await {
            self.run_sweeper(dead_generation).await;
        }
        {
            let mut state = self.state.lock();
            state.crash_streak = 0;
            state.last_error = None;
        }
        match self.start_locked().await {
            Ok(()) => Ok(self.status()),
            Err(err) => {
                self.set_state(SidecarState::Down, Some(err.to_string()));
                Err(err)
            }
        }
    }

    /// Ask the sidecar to exit, then make sure it did. No restart follows.
    pub async fn shutdown(self: &Arc<Self>) -> Result<()> {
        let _lock = self.lifecycle.lock().await;
        self.shutting_down.store(true, Ordering::SeqCst);

        if let Some(client) = self.client() {
            let graceful = client
                .call_value(
                    method::SHUTDOWN,
                    json!({"graceMs": 0}),
                    Some(SHUTDOWN_GRACE),
                )
                .await;
            if let Err(err) = graceful {
                tracing::debug!(target: "sidecar", error = %err, "mesh.shutdown did not answer; signalling");
            }
        }
        self.take_down("shutdown", true).await;
        // Announce it here rather than leaving it to the exit watcher: a caller
        // that awaits `shutdown()` must be able to observe `down` afterwards,
        // and on a clean stop there is no error to report.
        self.set_state(SidecarState::Down, None);
        Ok(())
    }

    /// Spawn one generation. The lifecycle lock must be held.
    async fn start_locked(self: &Arc<Self>) -> Result<()> {
        let generation = self.state.lock().generation + 1;
        self.set_state_generation(SidecarState::Starting, generation, None);

        // 1. Self-test the runtime before the real spawn (DESIGN §5-11). A
        //    broken runtime becomes a typed error with the path and the stderr,
        //    not an unexplained hang at the first `mesh.hello`.
        self.self_test().await?;

        // 2. The durable registry row lands before the process exists (§8-3).
        let argv = vec![
            self.runtime.node.display().to_string(),
            self.runtime.script.display().to_string(),
        ];
        let cwd = self
            .runtime
            .script
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("/"));
        let plan = SpawnPlan::new(ProcKind::Sidecar, argv, cwd.clone()).generation(generation);
        let guard = Arc::new(self.supervisor.plan_spawn(plan).await?);

        let env = self
            .supervisor
            .compose_env(self.env_base.clone(), None, guard.env_marker());

        let mut cmd = tokio_command_grouped(&self.runtime.node);
        cmd.arg(&self.runtime.script)
            .current_dir(&cwd)
            .env_clear()
            .envs(env)
            // The sidecar's parent-death watchdog (INT-C). It also has
            // `process.ppid`, which needs nothing from us, but that only sees our
            // *direct* parentage; this is the identity of the process whose death
            // must end it, stated explicitly. Set after `envs` so it wins over any
            // `agents.envPassthrough` entry of the same name.
            .env(ENV_HOST_PID, std::process::id().to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // `kill_on_drop` is never set anywhere in this app (M1-PLAN T19.4): a
        // child must outlive a dropped handle so the registry — not a Rust
        // destructor ordering accident — decides when it dies.

        // A failed spawn drops the guard while it is still armed, which is what
        // marks the row `lost` — the registry never keeps a `planned` row for a
        // process that was never created (DESIGN §8-3).
        let mut child = cmd
            .spawn()
            .map_err(|e| GildeError::Spawn(format!("could not start the sidecar: {e}")))?;

        let pid = child.id().map(|p| p as i32).unwrap_or(-1);
        // `process_group(0)` made the child its own group leader, so pgid == pid.
        // `mark_running` reads the kernel start time (a blocking `libproc` call)
        // and then writes SQLite with `synchronous=FULL`; neither belongs on the
        // reactor thread, so it goes to the blocking pool like every other
        // registry write in this module.
        {
            let guard = guard.clone();
            match tokio::task::spawn_blocking(move || guard.mark_running(pid, pid)).await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => tracing::warn!(
                    target: "sidecar", error = %err, "could not mark the sidecar row running"
                ),
                Err(err) => tracing::warn!(
                    target: "sidecar", error = %err, "the sidecar registry write panicked"
                ),
            }
        }

        let stdout = child.stdout.take().ok_or_else(|| GildeError::spawn("no stdout"))?;
        let stdin = child.stdin.take().ok_or_else(|| GildeError::spawn("no stdin"))?;
        let stderr = child.stderr.take().ok_or_else(|| GildeError::spawn("no stderr"))?;

        let (fault_tx, fault_rx) = mpsc::unbounded_channel();
        let handler: Arc<dyn HostHandler> = Arc::new(HostMethods {
            manager: Arc::downgrade(self),
            generation,
        });
        let client = SidecarClient::start(generation, stdout, stdin, handler, fault_tx);

        {
            let mut state = self.state.lock();
            state.client = Some(client.clone());
            state.child_pid = Some(pid);
            state.guard = Some(guard.clone());
            state.started_at = Some(Instant::now());
            state.hello = None;
        }

        self.spawn_stderr_drain(generation, stderr);
        self.spawn_exit_watcher(generation, child, guard.clone());
        self.spawn_fault_watcher(generation, fault_rx);

        // 3. Handshake. A protocol mismatch is fatal for this generation: we
        //    kill it rather than guess what the other side means (§5-3).
        match self.handshake(&client).await {
            Ok(hello) => {
                tracing::info!(
                    target: "sidecar",
                    generation,
                    pid,
                    node = %hello.node,
                    version = %hello.sidecar_version,
                    adapters = hello.adapters.len(),
                    node_source = self.runtime.node_source.as_str(),
                    "sidecar ready"
                );
                {
                    let mut state = self.state.lock();
                    state.hello = Some(hello);
                }
                self.set_state(SidecarState::Ready, None);
                self.spawn_health_loop(generation, client);
                self.spawn_healthy_start_reset(generation);
                Ok(())
            }
            Err(err) => {
                let detail = err.to_string();
                self.take_down(&format!("handshake failed: {detail}"), true)
                    .await;
                self.set_state(SidecarState::Down, Some(detail.clone()));
                Err(GildeError::Spawn(format!("sidecar handshake failed: {detail}")))
            }
        }
    }

    async fn handshake(&self, client: &SidecarClient) -> std::result::Result<HelloResult, String> {
        let mesh = self.mesh.lock().clone();
        let params = HelloParams {
            protocol: PROTOCOL_VERSION,
            host_version: self.supervisor.version().to_string(),
            app_instance_id: self.supervisor.app_instance_id().to_string(),
            boot_id: self.supervisor.current_boot_id().to_string(),
            binaries: mesh.binaries,
            defaults: mesh.defaults,
            accounts: mesh.accounts,
        };
        let hello: HelloResult = client
            .call(method::HELLO, params, Some(Duration::from_secs(30)))
            .await
            .map_err(|e| e.to_string())?;
        if hello.protocol != PROTOCOL_VERSION {
            return Err(format!(
                "protocol mismatch: host speaks {PROTOCOL_VERSION}, sidecar speaks {}",
                hello.protocol
            ));
        }
        Ok(hello)
    }

    /// `<runtime> --version`, 5 s cap, through the registry like every spawn.
    async fn self_test(&self) -> Result<()> {
        let argv = vec![
            self.runtime.node.display().to_string(),
            "--version".to_string(),
        ];
        let plan = SpawnPlan::new(ProcKind::Probe, argv, PathBuf::from("/"));
        // `Arc` only so the blocking `mark_running` below can own a handle; the
        // guard still lives to the end of this function, and dropping it while
        // armed still closes the row as `lost`.
        let guard = Arc::new(self.supervisor.plan_spawn(plan).await?);

        let mut cmd = tokio_command_grouped(&self.runtime.node);
        cmd.arg("--version")
            .current_dir("/")
            .env_clear()
            .envs(
                self.supervisor
                    .compose_env(self.env_base.clone(), None, guard.env_marker()),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // As above: dropping the armed guard closes the row as `lost`.
        let child = cmd
            .spawn()
            .map_err(|e| GildeError::SidecarRuntimeUnusable {
                path: self.runtime.node.display().to_string(),
                stderr: e.to_string(),
            })?;
        let pid = child.id().map(|p| p as i32).unwrap_or(-1);
        {
            // Same reason as `start_locked`: kernel start time + a durable write.
            let guard = guard.clone();
            let _ = tokio::task::spawn_blocking(move || guard.mark_running(pid, pid)).await;
        }

        let output = match tokio::time::timeout(SELF_TEST_TIMEOUT, child.wait_with_output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => {
                let _ = guard.mark_exited(None, None);
                return Err(GildeError::SidecarRuntimeUnusable {
                    path: self.runtime.node.display().to_string(),
                    stderr: e.to_string(),
                });
            }
            Err(_) => {
                let signal = terminate_group(pid, Duration::from_millis(500)).await;
                let _ = guard.mark_exited(None, signal);
                return Err(GildeError::SidecarRuntimeUnusable {
                    path: self.runtime.node.display().to_string(),
                    stderr: format!(
                        "--version did not answer within {}s",
                        SELF_TEST_TIMEOUT.as_secs()
                    ),
                });
            }
        };

        let code = output.status.code();
        let _ = guard.mark_exited(code, None);
        if !output.status.success() {
            return Err(GildeError::SidecarRuntimeUnusable {
                path: self.runtime.node.display().to_string(),
                stderr: String::from_utf8(output.stderr)
                    .unwrap_or_else(|e| format!("<{} non-utf8 stderr bytes>", e.as_bytes().len())),
            });
        }
        let version = String::from_utf8(output.stdout).unwrap_or_default();
        tracing::debug!(
            target: "sidecar",
            runtime = %self.runtime.node.display(),
            version = %version.trim(),
            source = self.runtime.node_source.as_str(),
            "sidecar runtime self-test passed"
        );
        Ok(())
    }

    // -- teardown ------------------------------------------------------------

    /// Close the channel and make sure the process is gone. Returns the
    /// generation that was taken down, if any.
    async fn take_down(&self, reason: &str, deliberate: bool) -> Option<i64> {
        let (client, pid, guard, generation) = {
            let mut state = self.state.lock();
            if state.client.is_none() && state.child_pid.is_none() {
                return None;
            }
            (
                state.client.take(),
                state.child_pid.take(),
                state.guard.take(),
                state.generation,
            )
        };

        if let Some(client) = client {
            client.close(reason);
        }
        let mut signal = None;
        if let Some(pid) = pid.filter(|p| *p > 0) {
            signal = terminate_group(pid, TERMINATE_GRACE).await;
        }
        if let Some(guard) = guard {
            // `signal` is what `terminate_group` ended up using — SIGKILL when
            // the sidecar ignored the SIGTERM — not the one we opened with.
            let _ = guard.mark_exited(None, signal);
        }
        self.settle_orphan_guards(reason);
        let _ = self.deaths.send(SidecarDeath {
            generation,
            reason: reason.to_string(),
            deliberate,
        });
        Some(generation)
    }

    /// Every child the dead generation had registered but never reported an exit
    /// for. The rows stay `running` for the reaper (it owns the fence check and
    /// the kill); what we close here is only our own bookkeeping.
    fn settle_orphan_guards(&self, reason: &str) {
        let ids: Vec<String> = self.guards.iter().map(|e| e.key().clone()).collect();
        for id in ids {
            if let Some((_, guard)) = self.guards.remove(&id) {
                tracing::debug!(
                    target: "sidecar",
                    spawn_id = %guard.spawn_id(),
                    reason,
                    "a sidecar child outlived its generation; leaving the row for the sweep"
                );
            }
        }
    }

    async fn run_sweeper(&self, dead_generation: i64) {
        let sweeper = self.sweeper.lock().clone();
        match sweeper {
            Some(sweeper) => {
                tracing::info!(target: "sidecar", generation = dead_generation, "sweeping the dead generation");
                sweeper.sweep(dead_generation).await;
            }
            None => tracing::debug!(
                target: "sidecar",
                generation = dead_generation,
                "no generation sweeper installed yet (T11); rows are left for the boot sweep"
            ),
        }
    }

    // -- supervision tasks ---------------------------------------------------

    fn spawn_stderr_drain(
        self: &Arc<Self>,
        generation: i64,
        stderr: tokio::process::ChildStderr,
    ) {
        let manager = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        // stderr is not the protocol: it is plain log text
                        // (DESIGN §5-1). It goes to `tracing` under its own
                        // target and into the tail kept for the down banner.
                        tracing::info!(target: "sidecar", generation, "{line}");
                        let Some(manager) = manager.upgrade() else { return };
                        let mut tail = manager.stderr_tail.lock();
                        if tail.len() == STDERR_TAIL_LINES {
                            tail.pop_front();
                        }
                        tail.push_back(line);
                    }
                    Ok(None) => return,
                    Err(err) => {
                        tracing::debug!(target: "sidecar", generation, error = %err, "stderr drain ended");
                        return;
                    }
                }
            }
        });
    }

    fn spawn_exit_watcher(
        self: &Arc<Self>,
        generation: i64,
        mut child: tokio::process::Child,
        guard: Arc<SpawnGuard>,
    ) {
        let manager = Arc::downgrade(self);
        tokio::spawn(async move {
            let status = child.wait().await;
            let (code, signal) = match &status {
                Ok(status) => (
                    status.code(),
                    crate::supervise::spawn::exit_signal_of(status),
                ),
                Err(_) => (None, None),
            };
            let _ = guard.mark_exited(code, signal);

            let Some(manager) = manager.upgrade() else { return };
            let reason = match (code, signal) {
                (_, Some(sig)) => format!("sidecar killed by signal {sig}"),
                (Some(code), _) => format!("sidecar exited with code {code}"),
                _ => "sidecar exited".to_string(),
            };
            manager.on_generation_died(generation, reason).await;
        });
    }

    fn spawn_fault_watcher(
        self: &Arc<Self>,
        generation: i64,
        mut faults: mpsc::UnboundedReceiver<ClientFault>,
    ) {
        let manager = Arc::downgrade(self);
        tokio::spawn(async move {
            while let Some(fault) = faults.recv().await {
                let Some(manager) = manager.upgrade() else { return };
                if manager.generation() != generation {
                    return;
                }
                match fault {
                    ClientFault::OversizeLine { bytes } => {
                        // A protocol violation, not a crash: kill the process so
                        // the exit watcher takes the normal restart path.
                        manager.set_fault_reason(format!(
                            "the sidecar sent a {bytes} byte line (cap {MAX_LINE_BYTES}); restarting"
                        ));
                        manager.emit_state(SidecarState::Degraded);
                        manager.kill_current_generation(generation).await;
                    }
                    ClientFault::Io { detail } => {
                        manager.note_error(format!("sidecar channel io error: {detail}"));
                    }
                    // The exit watcher owns this transition; a closed pipe with a
                    // live process is handled by the health loop.
                    ClientFault::StreamEnded => return,
                }
            }
        });
    }

    fn spawn_health_loop(self: &Arc<Self>, generation: i64, client: SidecarClient) {
        let manager = Arc::downgrade(self);
        let interval = self.ping_interval;
        tokio::spawn(async move {
            let mut misses = 0u32;
            loop {
                tokio::time::sleep(interval).await;
                let Some(manager) = manager.upgrade() else { return };
                if client.is_closed() || manager.generation() != generation {
                    return;
                }
                let nonce = manager.ping_nonce.fetch_add(1, Ordering::Relaxed);
                match client
                    .call_value(method::PING, json!({"nonce": nonce}), Some(PING_DEADLINE))
                    .await
                {
                    Ok(_) => misses = 0,
                    Err(SidecarCallError::Timeout { .. }) => {
                        misses += 1;
                        tracing::warn!(
                            target: "sidecar",
                            generation,
                            misses,
                            "mesh.ping missed its deadline"
                        );
                        if misses >= PING_MISSES_BEFORE_HANG {
                            // *Hung*, which is not the same as *dead*: the
                            // process is alive and unresponsive, so we kill it
                            // ourselves and take the restart path (DESIGN §5-3).
                            manager.set_fault_reason(format!(
                                "the sidecar missed {misses} consecutive pings; killing it"
                            ));
                            manager.emit_state(SidecarState::Degraded);
                            manager.kill_current_generation(generation).await;
                            return;
                        }
                    }
                    // Anything else means the channel is already gone; the exit
                    // watcher owns that transition.
                    Err(_) => return,
                }
            }
        });
    }

    async fn kill_current_generation(&self, generation: i64) {
        let pid = {
            let state = self.state.lock();
            if state.generation != generation {
                return;
            }
            state.child_pid
        };
        if let Some(pid) = pid.filter(|p| *p > 0) {
            terminate_group(pid, TERMINATE_GRACE).await;
        }
    }

    /// The single funnel for "a generation is no longer usable".
    async fn on_generation_died(self: Arc<Self>, dead_generation: i64, reason: String) {
        let deliberate = self.shutting_down.load(Ordering::SeqCst);
        // A fault we diagnosed ourselves outranks the exit status: "signal 15"
        // is how it died, not why.
        let reason = match self.take_fault_reason() {
            Some(fault) => format!("{fault} — {reason}"),
            None => reason,
        };
        {
            // Stage 1 — tear the generation down under the lifecycle lock.
            let _lock = self.lifecycle.lock().await;
            {
                let state = self.state.lock();
                if state.generation != dead_generation {
                    return; // already superseded
                }
            }

            let short_lived = {
                let mut state = self.state.lock();
                let short = state
                    .started_at
                    .map(|at| at.elapsed() < CRASH_WINDOW)
                    .unwrap_or(false);
                if let Some(client) = state.client.take() {
                    client.close(&reason);
                }
                state.child_pid = None;
                state.guard = None;
                state.hello = None;
                state.started_at = None;
                short
            };

            self.settle_orphan_guards(&reason);
            let _ = self.deaths.send(SidecarDeath {
                generation: dead_generation,
                reason: reason.clone(),
                deliberate,
            });

            if deliberate {
                self.set_state(SidecarState::Down, None);
                self.run_sweeper(dead_generation).await;
                return;
            }

            tracing::warn!(target: "sidecar", generation = dead_generation, reason = %reason, "sidecar generation died");
            self.note_error(reason.clone());
            self.set_state(SidecarState::Restarting, Some(reason.clone()));

            // The sweep runs BEFORE any replacement can take work, which is what
            // stops a crash loop from stacking orphaned `claude -p` children
            // (DESIGN §5-10 rule 3).
            self.run_sweeper(dead_generation).await;

            let over_limit = {
                let mut state = self.state.lock();
                if short_lived {
                    state.crash_streak += 1;
                } else {
                    state.crash_streak = 0;
                }
                state.crash_streak >= CRASH_LIMIT
            };
            if over_limit {
                let tail = self.stderr_tail();
                tracing::error!(
                    target: "sidecar",
                    crashes = CRASH_LIMIT,
                    "circuit breaker open: the sidecar keeps crashing; not restarting"
                );
                self.set_state(
                    SidecarState::Down,
                    Some(format!(
                        "the sidecar crashed {CRASH_LIMIT} times within {}s of starting; last stderr: {}",
                        CRASH_WINDOW.as_secs(),
                        tail.last().cloned().unwrap_or_default()
                    )),
                );
                return;
            }
        }

        // Stage 2 — back off, then try again. The lock is released while we
        // sleep so a user-initiated restart is never queued behind a backoff.
        self.clone().restart_with_backoff(dead_generation).await;
    }

    async fn restart_with_backoff(self: Arc<Self>, dead_generation: i64) {
        loop {
            // The ladder is indexed by the **consecutive** crash count, not by
            // the lifetime restart total. A sidecar that has been up for a week
            // and dies once must come back after 1 s; indexing by `restarts`
            // meant its fifth restart of the week waited a full minute for no
            // reason, and the counter never came back down.
            //
            // `restarts` is still incremented — it is the number the status bar
            // shows — it just no longer decides how long we wait.
            let (attempt, restarts) = {
                let mut state = self.state.lock();
                state.restarts += 1;
                (state.crash_streak.max(1), state.restarts)
            };
            let index = ((attempt as usize) - 1).min(RESTART_BACKOFF_SECS.len() - 1);
            let wait = Duration::from_secs(RESTART_BACKOFF_SECS[index]);
            tracing::info!(
                target: "sidecar",
                crash_streak = attempt,
                restarts,
                wait_secs = wait.as_secs(),
                "restarting the sidecar after a backoff"
            );
            tokio::time::sleep(wait).await;

            let _lock = self.lifecycle.lock().await;
            if self.shutting_down.load(Ordering::SeqCst) {
                return;
            }
            {
                let state = self.state.lock();
                if state.client.is_some() || state.generation != dead_generation {
                    return; // someone else already brought one up
                }
            }
            match self.start_locked().await {
                Ok(()) => return,
                Err(err) => {
                    tracing::error!(target: "sidecar", error = %err, "sidecar restart failed");
                    self.set_state(SidecarState::Down, Some(err.to_string()));
                    let over_limit = {
                        let mut state = self.state.lock();
                        state.crash_streak += 1;
                        state.crash_streak >= CRASH_LIMIT
                    };
                    if over_limit {
                        tracing::error!(
                            target: "sidecar",
                            "circuit breaker open: giving up on restarting the sidecar"
                        );
                        return;
                    }
                }
            }
        }
    }

    /// Clear the consecutive-crash counter once this generation has been up
    /// longer than [`CRASH_WINDOW`] — the definition of a *healthy start*.
    ///
    /// Without it the counter only ever fell at a death that happened to be
    /// long-lived, so a sidecar that came up cleanly still carried four old
    /// crashes into its next backoff, and the circuit breaker could open on
    /// five failures spread across an entire session. It is deliberately gated
    /// on uptime rather than on `Ready`: a generation that handshakes and then
    /// dies at 4.9 s is exactly the crash loop the breaker exists to catch.
    fn spawn_healthy_start_reset(self: &Arc<Self>, generation: i64) {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            tokio::time::sleep(CRASH_WINDOW).await;
            let Some(manager) = weak.upgrade() else {
                return;
            };
            let mut state = manager.state.lock();
            // Still the same, still up: this start earned the reset.
            if state.generation == generation && state.client.is_some() && state.crash_streak > 0 {
                tracing::info!(
                    target: "sidecar",
                    generation,
                    cleared = state.crash_streak,
                    "sidecar has been healthy past the crash window; clearing the crash streak"
                );
                state.crash_streak = 0;
            }
        });
    }

    // -- state bookkeeping ---------------------------------------------------

    fn note_error(&self, detail: String) {
        self.state.lock().last_error = Some(detail);
    }

    /// Record *why* we are about to kill this generation, so the restart reports
    /// the cause rather than the signal we used to act on it.
    fn set_fault_reason(&self, reason: String) {
        self.note_error(reason.clone());
        *self.fault_reason.lock() = Some(reason);
    }

    fn take_fault_reason(&self) -> Option<String> {
        self.fault_reason.lock().take()
    }

    fn set_state(&self, next: SidecarState, last_error: Option<String>) {
        {
            let mut state = self.state.lock();
            state.state = next;
            if last_error.is_some() {
                state.last_error = last_error;
            }
        }
        self.publish_status();
    }

    fn set_state_generation(&self, next: SidecarState, generation: i64, last_error: Option<String>) {
        {
            let mut state = self.state.lock();
            state.state = next;
            state.generation = generation;
            if last_error.is_some() {
                state.last_error = last_error;
            }
        }
        self.publish_status();
    }

    fn emit_state(&self, next: SidecarState) {
        self.set_state(next, None);
    }

    fn publish_status(&self) {
        let status = self.status();
        let _ = self.statuses.send(status.clone());
        if let Some(events) = self.events.lock().clone() {
            events.emit(
                SIDECAR_STATE,
                SidecarStateEvent {
                    state: status.state,
                    generation: status.generation.max(0) as u64,
                    restarts: status.restarts,
                    last_error: status.last_error.clone(),
                },
            );
        }
    }

    // -- the supervision seam (DESIGN §5-5) ----------------------------------

    async fn handle_host_call(
        self: Arc<Self>,
        generation: i64,
        method: &str,
        params: Value,
    ) -> std::result::Result<Value, RpcErrorObject> {
        match method {
            host_method::REGISTER_SPAWN => {
                let p: RegisterSpawnParams = decode(params)?;
                let kind = match p.kind.as_str() {
                    "probe" => ProcKind::Probe,
                    "gateway" => ProcKind::Gateway,
                    _ => ProcKind::Headless,
                };
                let mut argv = Vec::with_capacity(p.args.len() + 1);
                argv.push(p.cmd.clone());
                argv.extend(p.args.iter().cloned());

                let cwd = p
                    .cwd
                    .clone()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("/"));
                let mut plan = SpawnPlan::new(kind, argv, cwd).generation(generation);
                if !p.vendor.is_empty() && p.vendor != "unknown" {
                    plan = plan.vendor(p.vendor.clone());
                }
                if !p.run_id.is_empty() {
                    plan = plan.run(p.run_id.clone());
                }

                // The durable INSERT completes before this returns — that is the
                // whole reason the channel is bidirectional (DESIGN §5-5).
                let guard = self.supervisor.plan_spawn(plan).await.map_err(|e| {
                    RpcErrorObject::new(
                        code::INTERNAL_ERROR,
                        format!("could not record the spawn: {e}"),
                    )
                })?;
                let marker = guard.env_marker().to_string();
                tracing::debug!(
                    target: "sidecar",
                    generation,
                    sidecar_spawn_id = %p.spawn_id,
                    spawn_id = %marker,
                    vendor = %p.vendor,
                    kind = %p.kind,
                    // Key NAMES only: an env value must never reach a host log.
                    env_keys = p.env_keys.len(),
                    "registered a sidecar spawn"
                );
                self.guards.insert(p.spawn_id.clone(), Arc::new(guard));
                to_value(RegisterSpawnResult {
                    ok: true,
                    env_marker: marker,
                })
            }
            host_method::SPAWNED => {
                let spawn_id = spawn_id_of(&params)?;
                let p: SpawnedParams = decode(params)?;
                match self.guards.get(&spawn_id).map(|g| g.clone()) {
                    Some(guard) => {
                        let pgid = if p.pgid > 0 { p.pgid } else { p.pid };
                        // `mark_running` reads the kernel start time, which is a
                        // blocking `libproc` call plus a SQLite write.
                        let result = tokio::task::spawn_blocking(move || {
                            guard.mark_running(p.pid, pgid)
                        })
                        .await;
                        match result {
                            Ok(Ok(())) => to_value(json!({"ok": true})),
                            Ok(Err(e)) => Err(RpcErrorObject::new(
                                code::INTERNAL_ERROR,
                                format!("could not mark the child running: {e}"),
                            )),
                            Err(e) => Err(RpcErrorObject::new(
                                code::INTERNAL_ERROR,
                                format!("registry write panicked: {e}"),
                            )),
                        }
                    }
                    None => Err(RpcErrorObject::new(
                        code::INVALID_PARAMS,
                        format!("unknown spawnId: {spawn_id}"),
                    )),
                }
            }
            host_method::EXITED => {
                let spawn_id = spawn_id_of(&params)?;
                let p: ExitedParams = decode(params)?;
                match self.guards.remove(&spawn_id) {
                    Some((_, guard)) => {
                        // The name → number map is the registry's, not Node's:
                        // `proc_registry.exit_signal` is an integer column that
                        // the Process Monitor renders.
                        let signal = p.signal.as_deref().and_then(types::signal_number);
                        // T17 note: `exitCode: null` + `SIGKILL` is how the
                        // duplex usage probe ends a CLI it has already read the
                        // answer from. The row records signal 9 either way —
                        // that is what happened — but it is logged as the
                        // deliberate close it is, so a reader of the sidecar log
                        // does not chase a crash that never happened.
                        if types::is_deliberate_close(p.exit_code, p.signal.as_deref()) {
                            tracing::debug!(
                                target: "sidecar",
                                generation,
                                sidecar_spawn_id = %spawn_id,
                                spawn_id = %guard.spawn_id(),
                                "a sidecar child was closed deliberately after its output was read"
                            );
                        }
                        let result =
                            tokio::task::spawn_blocking(move || guard.mark_exited(p.exit_code, signal))
                                .await;
                        match result {
                            Ok(Ok(())) => to_value(json!({"ok": true})),
                            Ok(Err(e)) => Err(RpcErrorObject::new(
                                code::INTERNAL_ERROR,
                                format!("could not settle the child row: {e}"),
                            )),
                            Err(e) => Err(RpcErrorObject::new(
                                code::INTERNAL_ERROR,
                                format!("registry write panicked: {e}"),
                            )),
                        }
                    }
                    // Not an error worth failing the sidecar over: the row is
                    // settled by the reaper either way.
                    None => to_value(json!({"ok": true})),
                }
            }
            host_method::LOG => {
                self.host_log(params);
                to_value(json!({"ok": true}))
            }
            other => Err(RpcErrorObject::new(
                code::METHOD_NOT_FOUND,
                format!("unknown host method: {other}"),
            )),
        }
    }

    /// `cco.team.snapshot` (team contract v2 §8.2): one team's whole view from the
    /// read-only observer, relayed as `gilde://team/snapshot` — see
    /// [`team_snapshot_event`] for what is checked and stamped on the way.
    fn team_snapshot(&self, generation: i64, params: Value) {
        let Some(event) = team_snapshot_event(generation, params, owner_start_ms) else {
            return;
        };
        self.team_cache
            .apply_view(event.generation, event.rev, &event.team_id, event.view.as_ref());
        if let Some(events) = self.events.lock().clone() {
            events.emit(TEAM_SNAPSHOT, event);
        }
    }

    fn host_log(&self, params: Value) {
        let Ok(p) = serde_json::from_value::<HostLogParams>(params) else {
            return;
        };
        let fields = p
            .fields
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default();
        match p.level.as_str() {
            "error" => tracing::error!(target: "sidecar", fields = %fields, "{}", p.msg),
            "warn" => tracing::warn!(target: "sidecar", fields = %fields, "{}", p.msg),
            "debug" => tracing::debug!(target: "sidecar", fields = %fields, "{}", p.msg),
            _ => tracing::info!(target: "sidecar", fields = %fields, "{}", p.msg),
        }
    }
}

/// Turn a `cco.team.snapshot` notification into the `gilde://team/snapshot`
/// payload, or `None` when it must be dropped.
///
/// Checked before it reaches the UI: the payload must decode into the serde
/// mirror and name ONE team consistently, or it is dropped with a warning (it is
/// a latest-wins snapshot — the next one, or the next `cco_team_list`, repairs
/// the view). Then the two fields that are Rust's own are stamped: the
/// generation, and L7's `believed`, because only Rust can read the owner host's
/// start time without spawning anything (`start_ms_of_live_pid`).
pub(crate) fn team_snapshot_event(
    generation: i64,
    params: Value,
    start_ms_of_live_pid: impl Fn(i64) -> Option<i64>,
) -> Option<TeamSnapshotEvent<CcoTeamView>> {
    let snapshot: CcoTeamSnapshotParams = match serde_json::from_value(params) {
        Ok(snapshot) => snapshot,
        Err(err) => {
            tracing::warn!(target: "sidecar", error = %err, "cco.team.snapshot did not decode; dropped");
            return None;
        }
    };
    if !is_cco_team_id(&snapshot.team_id) {
        tracing::warn!(target: "sidecar", "cco.team.snapshot without a team id; dropped");
        return None;
    }
    let mut view = snapshot.view;
    if let Some(view) = view.as_mut() {
        if view.team_id != snapshot.team_id || !view.is_consistent() {
            tracing::warn!(target: "sidecar", "cco.team.snapshot names two teams; dropped");
            return None;
        }
        view.apply_belief(generation, start_ms_of_live_pid);
    }
    Some(TeamSnapshotEvent {
        generation,
        rev: snapshot.rev,
        team_id: snapshot.team_id,
        view,
    })
}

/// The start time, in epoch milliseconds, of `pid` when it is alive — the probe
/// L7 needs ([`cco_believed`]). libproc reports whole seconds, as `ps` does for
/// the engine's own `hostStartedAt`, and the comparison allows
/// [`CCO_START_SLACK_MS`] either way. `None` for a dead, impossible or
/// unreadable pid: "host gone" is the safe display.
pub(crate) fn owner_start_ms(pid: i64) -> Option<i64> {
    let pid = i32::try_from(pid).ok().filter(|p| *p > 1)?;
    if !is_alive(pid) {
        return None;
    }
    crate::supervise::procscan::start_time_of(pid).map(|secs| secs.saturating_mul(1000))
}

fn decode<T: DeserializeOwned>(params: Value) -> std::result::Result<T, RpcErrorObject> {
    serde_json::from_value(params)
        .map_err(|e| RpcErrorObject::new(code::INVALID_PARAMS, e.to_string()))
}

fn to_value<T: Serialize>(value: T) -> std::result::Result<Value, RpcErrorObject> {
    serde_json::to_value(value)
        .map_err(|e| RpcErrorObject::new(code::INTERNAL_ERROR, e.to_string()))
}

fn spawn_id_of(params: &Value) -> std::result::Result<String, RpcErrorObject> {
    params
        .get("spawnId")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| RpcErrorObject::new(code::INVALID_PARAMS, "spawnId is required"))
}

/// The `host.*` surface, bound to one generation.
struct HostMethods {
    manager: Weak<SidecarManager>,
    generation: i64,
}

impl HostHandler for HostMethods {
    fn call(&self, method: String, params: Value) -> HostFuture {
        let manager = self.manager.clone();
        let generation = self.generation;
        Box::pin(async move {
            let Some(manager) = manager.upgrade() else {
                return Err(RpcErrorObject::new(
                    code::INTERNAL_ERROR,
                    "the host is shutting down",
                ));
            };
            manager.handle_host_call(generation, &method, params).await
        })
    }

    fn notify(&self, method: &str, params: Value) {
        let Some(manager) = self.manager.upgrade() else {
            return;
        };
        match method {
            host_method::EVENT => {
                let run_id = params
                    .get("runId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let event = params.get("event").cloned().unwrap_or(Value::Null);
                if run_id.is_empty() || event.is_null() {
                    tracing::warn!(target: "sidecar", "mesh.event without a runId or an event");
                    return;
                }
                // A dropped receiver is not a failure: `mesh.event` is lossy by
                // contract, and the run's terminal state also arrives through
                // the response and the disk artifacts (DESIGN §5-8).
                let _ = manager.run_events.send(RunEventEnvelope {
                    run_id,
                    event,
                    generation: self.generation,
                });
            }
            host_method::LOG => manager.host_log(params),
            host_method::TEAM_SNAPSHOT => manager.team_snapshot(self.generation, params),
            other => tracing::warn!(target: "sidecar", method = other, "unknown notification"),
        }
    }
}

// ---------------------------------------------------------------------------
// signals
// ---------------------------------------------------------------------------

/// `SIGTERM` the group, wait out the grace, then `SIGKILL` what is left.
///
/// Deliberately narrow: it signals **one process group we created ourselves**
/// (`process_group(0)` made the child its leader), never a pid we merely found.
/// The general escalation with its identity fences is `supervise::kill`.
/// SIGTERM the group, wait out `grace`, escalate to SIGKILL if it is still
/// there. Returns **the signal that actually ended it**, which is what the
/// registry row records: writing `SIGTERM` for a process we had to SIGKILL
/// makes the Process Monitor lie about how the sidecar died.
///
/// `None` means no signal was sent at all (an impossible pid).
async fn terminate_group(pid: i32, grace: Duration) -> Option<i32> {
    use nix::sys::signal::{killpg, Signal};
    use nix::unistd::Pid;

    if pid <= 1 {
        return None;
    }
    let group = Pid::from_raw(pid);
    let _ = killpg(group, Signal::SIGTERM);

    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if !is_alive(pid) {
            return Some(libc::SIGTERM);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    if is_alive(pid) {
        tracing::warn!(target: "sidecar", pid, "sidecar ignored SIGTERM; escalating to SIGKILL");
        let _ = killpg(group, Signal::SIGKILL);
        return Some(libc::SIGKILL);
    }
    Some(libc::SIGTERM)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_backoff_ladder_is_the_one_design_specifies() {
        assert_eq!(RESTART_BACKOFF_SECS, [1, 2, 5, 15, 60]);
    }

    #[test]
    fn a_missing_script_is_a_typed_runtime_error() {
        let err = SidecarRuntime::resolve(Some(PathBuf::from("/nope/sidecar.cjs")), None, &[])
            .expect_err("must fail when no script exists");
        // The dev fallback also misses only when the source tree is absent; in
        // this repo it exists, so assert on whichever error the machine gives.
        match err {
            GildeError::SidecarRuntimeUnusable { .. } => {}
            other => panic!("expected SidecarRuntimeUnusable, got {other:?}"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn dev_mode_runs_the_source_tree_bundle_with_a_system_node() {
        // A `node` planted in a directory the caller offers as its resolved PATH.
        let dir = tempfile::tempdir().expect("tempdir");
        let node = dir.path().join("node");
        std::fs::write(&node, "#!/bin/sh\nexit 0\n").expect("write");
        std::fs::set_permissions(&node, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("chmod");

        std::env::set_var("GILDE_SIDECAR_DEV", "1");
        let runtime = SidecarRuntime::resolve(
            Some(PathBuf::from("/nonexistent/sidecar.cjs")),
            None,
            &[dir.path().to_path_buf()],
        );
        std::env::remove_var("GILDE_SIDECAR_DEV");

        let runtime = runtime.expect("dev mode resolves");
        assert_eq!(runtime.script, dev_script(), "the source tree bundle");
        assert_eq!(runtime.node, node);
        assert_eq!(runtime.node_source, NodeSource::System);
        assert!(
            runtime
                .warnings
                .iter()
                .any(|w| w.contains("nodeSource: system")),
            "a system node is reported, never silently accepted: {:?}",
            runtime.warnings
        );
    }

    #[test]
    #[serial_test::serial]
    fn an_explicit_node_path_wins_over_everything() {
        let dir = tempfile::tempdir().expect("tempdir");
        let node = dir.path().join("my-node");
        std::fs::write(&node, "#!/bin/sh\nexit 0\n").expect("write");
        std::fs::set_permissions(&node, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("chmod");

        std::env::remove_var("GILDE_SIDECAR_DEV");
        let runtime = SidecarRuntime::resolve(None, Some(&node.display().to_string()), &[])
            .expect("configured node resolves");
        assert_eq!(runtime.node, node);
        assert_eq!(runtime.node_source, NodeSource::Configured);
    }

    #[test]
    fn node_source_is_reported_not_hidden() {
        assert_eq!(NodeSource::System.as_str(), "system");
        assert_eq!(NodeSource::Vendored.as_str(), "vendored");
    }

    // -- team mode, read-only preview ----------------------------------------

    /// A `cco.team.snapshot` exactly as the sidecar sent it (written by
    /// `sidecar/test/cco-team.mjs` with `CCO_FIXTURE_OUT`).
    const TEAM_SNAPSHOT_FIXTURE: &str = include_str!("../../tests/fixtures/cco/team-snapshot.json");

    #[test]
    fn a_real_sidecar_snapshot_relays_with_rusts_own_fields_stamped() {
        let params: Value = serde_json::from_str(TEAM_SNAPSHOT_FIXTURE).expect("fixture is JSON");
        let event = team_snapshot_event(7, params.clone(), |_| None).expect("relayed");
        assert_eq!(event.generation, 7);
        assert_eq!(event.team_id, "tmabcdefghi1");
        let view = event.view.as_ref().expect("a view");
        assert_eq!(view.generation, 7);
        assert!(
            view.live && view.believed,
            "a live subscription is believed as it is"
        );
        assert_eq!(view.state.members.len(), 2);
        assert!(view.activity.iter().any(|a| a.member == "alice"));
        assert!(view.messages.iter().any(|m| m.id == "m11"));

        // The emitted payload keeps the wire's names, and decodes back to itself.
        let json = serde_json::to_value(&event).expect("encode");
        assert_eq!(json["teamId"], params["teamId"]);
        assert_eq!(
            json["view"]["state"]["members"][0]["tokens"],
            params["view"]["state"]["members"][0]["tokens"]
        );
        let back: TeamSnapshotEvent<CcoTeamView> = serde_json::from_value(json).expect("decode");
        assert_eq!(back, event);
    }

    #[test]
    fn a_disk_view_of_an_active_team_is_believed_only_with_its_owner() {
        let mut params: Value = serde_json::from_str(TEAM_SNAPSHOT_FIXTURE).expect("fixture");
        params["view"]["live"] = Value::Bool(false);
        params["view"]["source"] = Value::String("disk".into());
        let owner_pid = params["view"]["state"]["owner"]["hostPid"]
            .as_i64()
            .expect("owner pid");
        let started = params["view"]["state"]["owner"]["hostStartedAt"]
            .as_i64()
            .expect("started");
        let gone = team_snapshot_event(1, params.clone(), |_| None).expect("relayed");
        assert!(
            !gone.view.expect("view").believed,
            "a dead owner renders as host gone"
        );
        let alive = team_snapshot_event(1, params, |pid| {
            (pid == owner_pid).then_some(started + 1_000)
        })
        .expect("relayed");
        assert!(alive.view.expect("view").believed);
    }

    #[test]
    fn a_snapshot_that_names_two_teams_or_none_is_dropped() {
        let mut params: Value = serde_json::from_str(TEAM_SNAPSHOT_FIXTURE).expect("fixture");
        params["view"]["state"]["id"] = Value::String("tmothertea12".into());
        assert!(team_snapshot_event(1, params, |_| None).is_none());
        assert!(
            team_snapshot_event(1, json!({"rev": 1, "teamId": "../x", "view": null}), |_| {
                None
            })
            .is_none()
        );
        assert!(team_snapshot_event(1, json!("not an object"), |_| None).is_none());
        assert!(
            team_snapshot_event(1, json!({"rev": "seven", "teamId": "tmabcdefghi1"}), |_| {
                None
            })
            .is_none()
        );
        let withdrawn = team_snapshot_event(
            1,
            json!({"rev": 9, "teamId": "tmabcdefghi1", "view": null}),
            |_| None,
        )
        .expect("a withdrawal is relayed");
        assert!(withdrawn.view.is_none());
        assert_eq!(withdrawn.rev, 9);
    }

    #[test]
    fn an_impossible_owner_pid_is_never_probed_as_alive() {
        assert_eq!(owner_start_ms(0), None);
        assert_eq!(owner_start_ms(1), None, "launchd/init is never a host");
        assert_eq!(owner_start_ms(i64::from(i32::MAX) + 1), None);
        assert!(
            owner_start_ms(i64::from(std::process::id())).is_some(),
            "this process is alive"
        );
    }
}
