//! The Node sidecar: runtime resolution, lifecycle, health, protocol 2 and the
//! supervision seam that keeps its grandchildren visible (contract §2.1, §2.4
//! B12, §4.5, §4.7, §10.4).
//!
//! The sidecar is the turn-based lane: `@toragonite/agent-mesh` and its four
//! vendor adapters behind an NDJSON JSON-RPC channel ([`protocol`], [`client`]).
//! What lives *here* is everything that owns a **process** or a **turn**:
//!
//!   * **Runtime resolution** ([`SidecarRuntime`]) — the vendored `gilde-node`
//!     next to the executable in production, `CARGO_MANIFEST_DIR/sidecar/sidecar.cjs`
//!     plus a `node` off the resolved `PATH` under `GILDE_SIDECAR_DEV=1`.
//!   * **Lazy start** (B12) — nothing is spawned at boot. The first command that
//!     needs the sidecar calls [`SidecarApi::ensure_started`]; until the handshake
//!     answers it gets `NOT_READY{sidecarStarting}` (retryable). An orchestrator
//!     detected by discovery is the other trigger ([`SidecarManager::set_orchestrator_present`]).
//!   * **Protocol 2** — `mesh.hello {protocol: 2, buildId, appInstanceId,
//!     providers}` and `mesh.configure {binaries, accounts, providers}`. The build
//!     id is [`HOST_BUILD_ID`] (contract §4.7 / A8.1). A sidecar from another build
//!     exits with code 78 (or answers with another id): the manager latches
//!     `VERSION_MISMATCH`, never restarts it, and every command says "Restart Gilde
//!     to finish updating." A protocol-1 peer is stopped.
//!   * **Write-before-spawn** — the sidecar process, its `--version` self-test and
//!     every child the sidecar registers through `host.registerSpawn` get a durable
//!     registry row before they exist. `host.registerSpawn` is also an
//!     **allowlist** (§4.5, §9.3): its `cmd` must be a configured vendor binary or
//!     the bundled node, else `REFUSED{notAllowlisted}` and nothing spawns.
//!   * **Turns** — `turn.start` / `turn.cancel` and the `turn.event` stream, routed
//!     per turn through a bounded queue (≤ 1,024 undelivered events: `Text` merged
//!     on overflow, `Done` / `Error` never dropped). A generation that dies
//!     mid-turn ends each of its turns with one `Error` carrying
//!     [`HELPER_RESTARTED`] (§10.4: Error·Agent error "The helper restarted; the
//!     turn was stopped.").
//!   * **Health and restart** (§10.4) — `mesh.ping` every interval with a 5 s
//!     deadline, three consecutive misses ⇒ hang-kill; restart backoff
//!     1, 2, 4 … 60 s; three deaths within five minutes ⇒ it stays down and its
//!     commands return `SIDECAR_DOWN`.
//!
//! **Nothing else depends on this.** A sidecar that is down, hung, mismatched or
//! disabled costs turn-based turns and probes only: Conversation (L1), Terminal,
//! discovery and teams keep working (contract §2.3, D3, PM-31).

pub mod client;
pub mod protocol;
pub mod team_cache;
pub mod types;

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dashmap::DashMap;
use parking_lot::Mutex;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{broadcast, mpsc, Notify};

use crate::contract::account::Quota;
use crate::contract::common::{AbsPath, Vendor};
use crate::contract::error::{ErrorCode, IpcError, VendorErrorClass};
use crate::contract::services::{SidecarApi, TurnEventSink};
use crate::contract::sidecar as wire;
use crate::error::{GildeError, Result};
use crate::supervise::registry::{EnvBase, Supervisor};
use crate::contract::activity::ExitInfo;
use crate::contract::process::ProcKind as SpawnKind;
use crate::contract::services::{SpawnRequest, SpawnTicket, SupervisorApi};
use crate::supervise::spawn::{is_alive, tokio_command_grouped};

pub use client::{ClientFault, HostFuture, HostHandler, SidecarCallError, SidecarClient};
pub use protocol::{code, host_method, method, RpcErrorObject, MAX_LINE_BYTES, PROTOCOL_VERSION};
pub use team_cache::TeamCache;
pub use types::*;

// ---------------------------------------------------------------------------
// the build id (contract §4.7, A8.1)
// ---------------------------------------------------------------------------

/// The build id this core sends in `mesh.hello`: the core's `BUILD_ID` (A8.1:
/// the compile-time `GILDE_BUILD_ID` when set and non-empty, else the app
/// version) — the same rule as the WebView's (`src/lib/buildId.ts`) and the
/// sidecar's (`esbuild.mjs`), so a dev or test build carries the version on
/// every side.
pub const HOST_BUILD_ID: &str = crate::BUILD_ID;

const _: () = assert!(
    is_valid_build_id(HOST_BUILD_ID),
    "GILDE_BUILD_ID must match ^[0-9A-Za-z][0-9A-Za-z._+-]{{0,63}}$ (contract §4.7, A8.1); it is never trimmed or replaced"
);

/// A8.1: an empty value is unset; it is never trimmed or replaced.
pub const fn build_id_from<'a>(env: Option<&'a str>, version: &'a str) -> &'a str {
    match env {
        Some(id) if !id.is_empty() => id,
        _ => version,
    }
}

/// A8.1: `^[0-9A-Za-z][0-9A-Za-z._+-]{0,63}$`.
pub const fn is_valid_build_id(id: &str) -> bool {
    let b = id.as_bytes();
    if b.is_empty() || b.len() > 64 || !b[0].is_ascii_alphanumeric() {
        return false;
    }
    let mut i = 1;
    while i < b.len() {
        let c = b[i];
        if !(c.is_ascii_alphanumeric() || c == b'.' || c == b'_' || c == b'+' || c == b'-') {
            return false;
        }
        i += 1;
    }
    true
}

// ---------------------------------------------------------------------------
// budgets
// ---------------------------------------------------------------------------

/// Wall clock for the `--version` self-test.
pub const SELF_TEST_TIMEOUT: Duration = Duration::from_secs(5);
/// The hard deadline of the handshake (§2.4 B12: ≤ 3 s budget, 30 s hard).
pub const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(30);
/// How long [`SidecarApi::ensure_started`] waits before answering
/// `NOT_READY{sidecarStarting}` (the B12 handshake budget).
pub const ENSURE_WAIT: Duration = Duration::from_secs(3);
/// Deadline for one `mesh.ping`.
pub const PING_DEADLINE: Duration = Duration::from_secs(5);
/// Consecutive missed pings before the sidecar is declared *hung* (not dead).
pub const PING_MISSES_BEFORE_HANG: u32 = 3;
/// Restart backoff ladder, in seconds (§10.4: 1, 2, 4 … 60 s).
pub const RESTART_BACKOFF_SECS: [u64; 7] = [1, 2, 4, 8, 16, 32, 60];
/// Deaths within [`DEATH_WINDOW`] after which the sidecar stays down (§10.4).
pub const DEATHS_BEFORE_STAYING_DOWN: usize = 3;
/// The window of [`DEATHS_BEFORE_STAYING_DOWN`].
pub const DEATH_WINDOW: Duration = Duration::from_secs(5 * 60);
/// stderr lines kept for diagnostics and the down reason.
pub const STDERR_TAIL_LINES: usize = 200;
/// Grace between the shutdown/hang `SIGTERM` and its `SIGKILL`. The sidecar's
/// node is a Gilde helper, not a vendor CLI, so it may be escalated (§9.8).
pub const TERMINATE_GRACE: Duration = Duration::from_secs(3);
/// Grace handed to `mesh.shutdown` before the signal path takes over.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
/// Deadline of a probe call (agent-mesh's own probe clocks are ≤ 90 s).
pub const PROBE_DEADLINE: Duration = Duration::from_secs(120);
/// Deadline of `turn.start` / `turn.cancel` answers (the turn itself streams).
pub const TURN_CALL_DEADLINE: Duration = Duration::from_secs(30);
/// The host pid, handed to the sidecar's parent-death watchdog (INT-C).
pub const ENV_HOST_PID: &str = "GILDE_HOST_PID";

/// The `message` of the `Error` that ends every turn of a generation that died
/// (§6.3, §10.4). The turn driver shows it as `turn.helperRestarted`.
pub const HELPER_RESTARTED: &str = "turn.helperRestarted";

// ---------------------------------------------------------------------------
// runtime resolution
// ---------------------------------------------------------------------------

/// Where the Node runtime came from. Surfaced in diagnostics because
/// "it works in dev, not from Finder" is almost always this field.
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
    /// Non-fatal problems worth showing in diagnostics.
    #[serde(default)]
    pub warnings: Vec<String>,
}

impl SidecarRuntime {
    /// Resolve the runtime for this build. `search_path` is the resolved
    /// `PATH`, not the app's own: a bundle launched from Finder cannot see nvm.
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

    /// The production entry point: resolve against a live Tauri app. The bundled
    /// script is `sidecar/sidecar.cjs` under `BaseDirectory::Resource`.
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

/// Lifecycle of the sidecar (contract §10.4). The turn-based lane only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SidecarState {
    Starting,
    Ready,
    Degraded,
    #[default]
    Down,
    Restarting,
}

impl SidecarState {
    pub const fn as_str(self) -> &'static str {
        match self {
            SidecarState::Starting => "starting",
            SidecarState::Ready => "ready",
            SidecarState::Degraded => "degraded",
            SidecarState::Down => "down",
            SidecarState::Restarting => "restarting",
        }
    }
}

/// A status snapshot. `S` is the state's type: [`SidecarState`] by default.
/// [`SidecarManager::status`] can render the state into any type that decodes
/// from its camelCase name (the legacy graph projection reads it that way until
/// it is deleted, A7.9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SidecarStatus<S = SidecarState> {
    pub state: S,
    pub generation: i64,
    pub restarts: u32,
    pub last_error: Option<String>,
    pub node_path: Option<String>,
    pub script_path: Option<String>,
    /// A sidecar from another build answered: "Restart Gilde to finish updating."
    #[serde(default)]
    pub version_mismatch: bool,
    /// Three deaths within five minutes: it stays down until an explicit restart.
    #[serde(default)]
    pub stays_down: bool,
}

/// Broadcast when a generation dies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidecarDeath {
    pub generation: i64,
    pub reason: String,
    /// `true` when the app asked for it (shutdown or an explicit restart).
    pub deliberate: bool,
}

/// The reaper hook, awaited **before the replacement sidecar can accept
/// work** — the property that keeps a crash loop from accumulating orphans.
pub trait GenerationSweeper: Send + Sync + 'static {
    fn sweep(&self, dead_generation: i64) -> Pin<Box<dyn Future<Output = ()> + Send>>;
}

/// One relayed `orch.snapshot`, after Rust stamped its own fields: the team
/// views T02's orchestrator mapping consumes ([`SidecarManager::subscribe_orchestrator`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrchTeamSnapshot {
    /// `(host generation, observer generation)` folded into one ordered key.
    pub generation: i64,
    pub rev: u64,
    pub team_id: String,
    /// `None` = the team left the observer's sight.
    pub view: Option<CcoTeamView>,
}

/// The ordered key of a view: the host's sidecar generation in the high bits,
/// the sidecar's observer generation in the low 24 (a new observer restarts `rev`).
pub fn orch_generation_key(host_generation: i64, observer_generation: u64) -> i64 {
    (host_generation.max(0) << 24) | (observer_generation.min(0x00FF_FFFF) as i64)
}

// ---------------------------------------------------------------------------
// turn routing (§4.5 backpressure)
// ---------------------------------------------------------------------------

/// What one turn's queue holds and how it merges on overflow: at most
/// [`wire::MAX_PENDING_TURN_EVENTS`] undelivered events; past that a `Text`
/// event is merged into a `Text` already queued (order kept), a `Usage` replaces
/// a trailing `Usage`, and `Done` / `Error` are always admitted. `seq` must be
/// strictly increasing; anything after the terminal event is dropped.
#[derive(Debug, Default)]
pub struct TurnQueue {
    events: VecDeque<(u64, wire::TurnEvent)>,
    last_seq: u64,
    terminal: bool,
    /// Events merged into another on overflow (diagnostics).
    pub merged: u64,
    /// Events dropped as out of order or after the terminal event.
    pub rejected: u64,
}

/// Why [`TurnQueue::admit`] did not queue an event as its own entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admit {
    Queued,
    Merged,
    Rejected,
}

impl TurnQueue {
    pub fn is_terminal(event: &wire::TurnEvent) -> bool {
        matches!(event, wire::TurnEvent::Done { .. } | wire::TurnEvent::Error { .. })
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn admit(&mut self, seq: u64, event: wire::TurnEvent) -> Admit {
        if self.terminal || seq <= self.last_seq {
            self.rejected += 1;
            return Admit::Rejected;
        }
        self.last_seq = seq;
        if Self::is_terminal(&event) {
            self.terminal = true;
            self.events.push_back((seq, event));
            return Admit::Queued;
        }
        if self.events.len() >= wire::MAX_PENDING_TURN_EVENTS {
            match event {
                wire::TurnEvent::Text { delta } => {
                    if let Some((_, wire::TurnEvent::Text { delta: tail })) = self.events.back_mut() {
                        tail.push_str(&delta);
                        self.merged += 1;
                        return Admit::Merged;
                    }
                    // Make room by joining the first adjacent pair of texts.
                    if let Some(i) = (0..self.events.len().saturating_sub(1)).find(|&i| {
                        matches!(self.events[i].1, wire::TurnEvent::Text { .. })
                            && matches!(self.events[i + 1].1, wire::TurnEvent::Text { .. })
                    }) {
                        if let Some((_, wire::TurnEvent::Text { delta: second })) = self.events.remove(i + 1) {
                            if let (_, wire::TurnEvent::Text { delta: first }) = &mut self.events[i] {
                                first.push_str(&second);
                            }
                        }
                        self.merged += 1;
                    }
                    self.events.push_back((seq, wire::TurnEvent::Text { delta }));
                    Admit::Queued
                }
                wire::TurnEvent::Usage { usage } => {
                    if let Some((tail_seq, wire::TurnEvent::Usage { usage: tail })) = self.events.back_mut() {
                        *tail = usage;
                        *tail_seq = seq;
                        self.merged += 1;
                        return Admit::Merged;
                    }
                    self.events.push_back((seq, wire::TurnEvent::Usage { usage }));
                    Admit::Queued
                }
                other => {
                    self.events.push_back((seq, other));
                    Admit::Queued
                }
            }
        } else {
            self.events.push_back((seq, event));
            Admit::Queued
        }
    }

    pub fn pop(&mut self) -> Option<(u64, wire::TurnEvent)> {
        self.events.pop_front()
    }
}

/// One live turn: its sink, its queue and the generation it runs in.
struct TurnRoute {
    generation: i64,
    sink: Arc<dyn TurnEventSink>,
    queue: Mutex<TurnQueue>,
    wake: Notify,
    done: AtomicBool,
}

impl TurnRoute {
    fn push(&self, seq: u64, event: wire::TurnEvent) -> Admit {
        let admitted = self.queue.lock().admit(seq, event);
        if admitted != Admit::Rejected {
            self.wake.notify_one();
        }
        admitted
    }

    /// The synthesized terminal of a turn whose generation died.
    fn fail_helper_restarted(&self) {
        let seq = self.queue.lock().last_seq + 1;
        self.push(
            seq,
            wire::TurnEvent::Error {
                class: VendorErrorClass::Unknown,
                message: HELPER_RESTARTED.to_string(),
            },
        );
    }
}

// ---------------------------------------------------------------------------
// configuration (legacy shape kept for the boot wiring)
// ---------------------------------------------------------------------------

/// The fleet configuration in its pre-protocol-2 shape (`lib.rs`, the boot
/// tests). [`SidecarManager::configure`] converts it into protocol 2's
/// `mesh.configure`: resolved binaries by contract vendor and [`wire::AccountEnv`]s.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MeshConfig {
    pub binaries: BinaryPaths,
    pub accounts: Vec<Account>,
    pub defaults: MeshDefaults,
}

/// A vendor binary resolver, asked on EVERY sidecar start (A12.6; boot no
/// longer starts the sidecar): the app binds `agents::resolve_binary`
/// (realpath + the A1 fingerprint cache, §6.0), whose answer is also what the
/// spawn allowlist admits. `None` = not installed. It never runs the binary
/// for the sidecar.
pub type BinaryResolver = Arc<dyn Fn(Vendor) -> ResolveFuture + Send + Sync>;

/// The answer of a [`BinaryResolver`].
pub type ResolveFuture = std::pin::Pin<Box<dyn std::future::Future<Output = Option<AbsPath>> + Send>>;

/// A resolver over a synchronous lookup (fixed paths, tests).
pub fn resolver_from<F>(lookup: F) -> BinaryResolver
where
    F: Fn(Vendor) -> Option<AbsPath> + Send + Sync + 'static,
{
    Arc::new(move |vendor| {
        let found = lookup(vendor);
        Box::pin(async move { found })
    })
}

/// The account variable rule (contract §6.0, C8): claude → `CLAUDE_CONFIG_DIR`,
/// codex → `CODEX_HOME`; cursor and agy have none.
pub const fn account_env_key(vendor: Vendor) -> Option<&'static str> {
    match vendor {
        Vendor::Claude => Some("CLAUDE_CONFIG_DIR"),
        Vendor::Codex => Some("CODEX_HOME"),
        Vendor::Cursor | Vendor::Agy => None,
    }
}

/// The realpath of `p` when it exists, else `p` itself if it already has the
/// canonical form.
pub fn canonical_abs(p: &Path) -> Option<AbsPath> {
    let resolved = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    AbsPath::parse(resolved.to_str()?.to_string()).ok()
}

/// The legacy account list as protocol-2 `AccountEnv`s. A claude/codex account
/// whose directory is the vendor's default (`~/.claude`, `~/.codex`) or absent is
/// the default login: `envKey: null` (C8). cursor/agy never carry a variable.
pub fn account_envs_of(accounts: &[Account], home: &Path) -> Vec<wire::AccountEnv> {
    let mut out: Vec<wire::AccountEnv> = Vec::new();
    for account in accounts {
        let vendor = match account.vendor.as_str() {
            "gemini" => Vendor::Agy,
            other => match Vendor::parse(other) {
                Some(v) => v,
                None => continue,
            },
        };
        let default_dir = match vendor {
            Vendor::Claude => Some(home.join(".claude")),
            Vendor::Codex => Some(home.join(".codex")),
            _ => None,
        };
        let dir = account
            .config_dir
            .as_deref()
            .filter(|d| !d.is_empty())
            .and_then(|d| canonical_abs(Path::new(d)));
        let is_default = match (&dir, &default_dir) {
            (None, _) => true,
            (Some(d), Some(def)) => canonical_abs(def).is_some_and(|def| &def == d),
            _ => false,
        };
        let env = match (account_env_key(vendor), is_default, dir) {
            (Some(key), false, Some(dir)) => wire::AccountEnv {
                account_id: crate::contract::ids::AccountId::derive(vendor, dir.as_str()),
                vendor,
                env_key: Some(key.to_string()),
                config_dir: Some(dir),
            },
            _ => {
                let default = default_dir.as_deref().and_then(canonical_abs);
                wire::AccountEnv {
                    account_id: match &default {
                        Some(d) => crate::contract::ids::AccountId::derive(vendor, d.as_str()),
                        None => crate::contract::ids::AccountId::derive_default_login(vendor),
                    },
                    vendor,
                    env_key: None,
                    config_dir: None,
                }
            }
        };
        if !out.iter().any(|e| e.account_id == env.account_id) {
            out.push(env);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// manager
// ---------------------------------------------------------------------------

struct LiveState {
    state: SidecarState,
    generation: i64,
    restarts: u32,
    last_error: Option<String>,
    client: Option<SidecarClient>,
    child_pid: Option<i32>,
    /// The live generation's registry ticket.
    guard: Option<SpawnTicket>,
    started_at: Option<Instant>,
    roster: Vec<wire::AdapterInfo>,
    /// The binaries the live generation was configured with: the allowlist.
    configured: BTreeMap<Vendor, AbsPath>,
    /// Non-deliberate deaths, for the "three in five minutes" rule.
    deaths: VecDeque<Instant>,
    /// The latch of a build mismatch: never restarted, every command says so.
    version_mismatch: bool,
    /// Three deaths in five minutes: stays down until an explicit restart.
    stays_down: bool,
    /// A lazy start is in flight.
    starting: bool,
    /// The generation stopped on purpose for a presence change (A11.6): its
    /// exit is not a death and is not restarted.
    retired: Option<i64>,
    /// `providers.orchestrator` as the live generation's `mesh.hello` sent it.
    hello_orchestrator: Option<bool>,
}

impl LiveState {
    fn new() -> LiveState {
        LiveState {
            state: SidecarState::Down,
            generation: 0,
            restarts: 0,
            last_error: None,
            client: None,
            child_pid: None,
            guard: None,
            started_at: None,
            roster: Vec::new(),
            configured: BTreeMap::new(),
            deaths: VecDeque::new(),
            version_mismatch: false,
            stays_down: false,
            starting: false,
            retired: None,
            hello_orchestrator: None,
        }
    }
}

/// Owns the sidecar process across restarts. Held in Tauri managed state as
/// `Arc<SidecarManager>`; it is also the [`SidecarApi`] the turn driver and the
/// accounts service use (`Arc<SidecarManager>` coerces to `Arc<dyn SidecarApi>`).
pub struct SidecarManager {
    me: Weak<SidecarManager>,
    /// Env composition and the app instance id (A13.4).
    supervisor: Arc<Supervisor>,
    /// Every spawn goes through this: the §9.3 allowlist, the durable row
    /// before the process exists (A12.8).
    spawns: Arc<dyn SupervisorApi>,
    runtime: SidecarRuntime,
    env_base: EnvBase,
    /// Explicit binaries (the legacy `configure`), by contract vendor.
    binaries: Mutex<BTreeMap<Vendor, AbsPath>>,
    binary_resolver: Mutex<Option<BinaryResolver>>,
    accounts: Mutex<Vec<wire::AccountEnv>>,
    orchestrator: AtomicBool,
    state: Mutex<LiveState>,
    sweeper: Mutex<Option<Arc<dyn GenerationSweeper>>>,
    /// The sidecar's own spawn id → our registry ticket.
    guards: DashMap<String, SpawnTicket>,
    turns: DashMap<String, Arc<TurnRoute>>,
    stderr_tail: Mutex<VecDeque<String>>,
    /// Why the current generation is about to die, when we know better than the
    /// exit status does.
    fault_reason: Mutex<Option<String>>,
    run_events: broadcast::Sender<RunEventEnvelope>,
    statuses: broadcast::Sender<SidecarStatus>,
    deaths: broadcast::Sender<SidecarDeath>,
    orch_snapshots: broadcast::Sender<OrchTeamSnapshot>,
    ping_interval: Duration,
    ping_nonce: AtomicU64,
    /// Serialises start / restart / shutdown against each other.
    lifecycle: tokio::sync::Mutex<()>,
    shutting_down: AtomicBool,
    /// Held (shared) while a turn claims its route, (exclusive) while a presence
    /// stop checks for turns and detaches the generation: a turn never starts on
    /// a generation that is being stopped (A11.6).
    turn_gate: parking_lot::RwLock<()>,
    /// A presence stop is waiting for the sidecar to go idle.
    presence_stop_pending: AtomicBool,
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
    /// Build a manager. Nothing is spawned until the first [`SidecarApi::ensure_started`]
    /// (B12) or an explicit [`SidecarManager::start`].
    pub fn new(
        supervisor: Arc<Supervisor>,
        runtime: SidecarRuntime,
        env_base: EnvBase,
        mesh: MeshConfig,
        ping_interval: Duration,
    ) -> Arc<SidecarManager> {
        SidecarManager::with_presence(supervisor, runtime, env_base, mesh, ping_interval, false)
    }

    /// [`SidecarManager::new`] with the orchestrator's presence at boot
    /// (`app_bootstrap.features.orchestrator`, A11.6): the first `mesh.hello`
    /// carries it.
    pub fn with_presence(
        supervisor: Arc<Supervisor>,
        runtime: SidecarRuntime,
        env_base: EnvBase,
        mesh: MeshConfig,
        ping_interval: Duration,
        orchestrator_present: bool,
    ) -> Arc<SidecarManager> {
        let (run_events, _) = broadcast::channel(16);
        let (statuses, _) = broadcast::channel(64);
        let (deaths, _) = broadcast::channel(64);
        let (orch_snapshots, _) = broadcast::channel(256);
        let home = env_base.home.clone();
        let accounts = account_envs_of(&mesh.accounts, &home);
        let binaries = binaries_of(&mesh.binaries);
        let spawns: Arc<dyn SupervisorApi> = supervisor.clone();
        Arc::new_cyclic(|me| SidecarManager {
            me: me.clone(),
            supervisor,
            spawns,
            runtime,
            env_base,
            binaries: Mutex::new(binaries),
            binary_resolver: Mutex::new(None),
            accounts: Mutex::new(accounts),
            orchestrator: AtomicBool::new(orchestrator_present),
            state: Mutex::new(LiveState::new()),
            sweeper: Mutex::new(None),
            guards: DashMap::new(),
            turns: DashMap::new(),
            stderr_tail: Mutex::new(VecDeque::with_capacity(STDERR_TAIL_LINES)),
            fault_reason: Mutex::new(None),
            run_events,
            statuses,
            deaths,
            orch_snapshots,
            ping_interval,
            ping_nonce: AtomicU64::new(1),
            lifecycle: tokio::sync::Mutex::new(()),
            shutting_down: AtomicBool::new(false),
            turn_gate: parking_lot::RwLock::new(()),
            presence_stop_pending: AtomicBool::new(false),
            team_cache: Arc::new(TeamCache::new()),
        })
    }

    // -- wiring --------------------------------------------------------------

    /// The team views relayed so far (`supervise::health` reads them).
    pub fn team_cache(&self) -> &Arc<TeamCache> {
        &self.team_cache
    }

    /// Install the reaper hook.
    pub fn set_sweeper(&self, sweeper: Arc<dyn GenerationSweeper>) {
        *self.sweeper.lock() = Some(sweeper);
    }

    /// Install the binary resolver (`agents::resolve_binary`, §6.0). Without
    /// one, a binary comes from `configure`, else from the user's pinned
    /// `agents.binaries` setting, else from the resolved `PATH` — realpath'd,
    /// stat'ed, never executed.
    pub fn set_binary_resolver(&self, resolver: BinaryResolver) {
        *self.binary_resolver.lock() = Some(resolver);
    }

    /// The accounts the sidecar may run under (env key names and config dirs
    /// only). Re-sent through `mesh.configure` when the sidecar is up.
    pub async fn set_accounts(&self, accounts: Vec<wire::AccountEnv>) -> std::result::Result<(), IpcError> {
        *self.accounts.lock() = accounts;
        self.reconfigure().await
    }

    /// The accounts currently configured.
    pub fn accounts(&self) -> Vec<wire::AccountEnv> {
        self.accounts.lock().clone()
    }

    /// Orchestrator presence (§5.10, B13): registers or removes the sidecar's
    /// `orch.*` methods, and — present — starts the sidecar lazily (B12).
    ///
    /// A11.6: unchanged → nothing. Changed while no sidecar runs → stored; the
    /// next (lazy) start's `mesh.hello` carries it. Changed while one runs → the
    /// in-flight requests and turns finish, then it is stopped gracefully
    /// (`mesh.shutdown`, then SIGTERM — never SIGKILL, never mid-turn); the next
    /// lazy start carries the new value, so `orch.*` exists only while present.
    /// When the orchestrator appears, that start is kicked at once (its observer
    /// is the need, B12).
    pub async fn set_orchestrator_present(&self, present: bool) {
        let was = self.orchestrator.swap(present, Ordering::SeqCst);
        if was == present {
            return;
        }
        let running = {
            let state = self.state.lock();
            state.client.as_ref().map(|_| state.generation)
        };
        match running {
            None => {
                if present {
                    self.kick_lazy_start();
                }
            }
            Some(generation) => {
                let Some(me) = self.me.upgrade() else { return };
                if self.presence_stop_pending.swap(true, Ordering::SeqCst) {
                    return; // a waiter is already on it; it re-reads the value
                }
                tokio::spawn(async move {
                    me.stop_when_idle(generation).await;
                    me.presence_stop_pending.store(false, Ordering::SeqCst);
                    if me.orchestrator.load(Ordering::SeqCst) {
                        me.kick_lazy_start();
                    }
                });
            }
        }
    }

    /// What a presence stop finds on the generation it waits for.
    fn presence_check(&self, generation: i64) -> PresenceCheck {
        let state = self.state.lock();
        let Some(client) = state.client.as_ref() else { return PresenceCheck::Gone };
        if state.generation != generation {
            return PresenceCheck::Gone;
        }
        if state.state == SidecarState::Ready
            && state.hello_orchestrator == Some(self.orchestrator.load(Ordering::SeqCst))
        {
            return PresenceCheck::Unchanged; // toggled back before it went idle
        }
        let turns = self.turns.iter().any(|e| e.value().generation == generation);
        if turns || client.pending() > 0 || state.state != SidecarState::Ready {
            PresenceCheck::Busy
        } else {
            PresenceCheck::Idle
        }
    }

    /// Wait until the generation is idle, then stop it gracefully (A11.6).
    async fn stop_when_idle(self: &Arc<Self>, generation: i64) {
        loop {
            match self.presence_check(generation) {
                PresenceCheck::Gone | PresenceCheck::Unchanged => return,
                PresenceCheck::Busy => {
                    tokio::time::sleep(PRESENCE_IDLE_POLL).await;
                    continue;
                }
                PresenceCheck::Idle => {}
            }
            let _lifecycle = self.lifecycle.lock().await;
            let detached = {
                // No turn can claim a route on this generation from here on.
                let _gate = self.turn_gate.write();
                match self.presence_check(generation) {
                    PresenceCheck::Gone | PresenceCheck::Unchanged => return,
                    PresenceCheck::Busy => None,
                    PresenceCheck::Idle => {
                        self.state.lock().retired = Some(generation);
                        self.detach()
                    }
                }
            };
            let Some(detached) = detached else {
                drop(_lifecycle);
                tokio::time::sleep(PRESENCE_IDLE_POLL).await;
                continue;
            };
            tracing::info!(target: "sidecar", generation, "the orchestrator presence changed; stopping the idle sidecar");
            if let Some(client) = &detached.client {
                if let Err(err) = client
                    .call_value(method::SHUTDOWN, json!({"graceMs": 0}), Some(SHUTDOWN_GRACE))
                    .await
                {
                    tracing::debug!(target: "sidecar", error = %err, "mesh.shutdown did not answer; signalling");
                }
            }
            self.finish_detached(detached, "the orchestrator presence changed", true, Escalate::Never)
                .await;
            self.set_state(SidecarState::Down, None);
            return;
        }
    }

    /// Whether the orchestrator is present (as last told).
    pub fn orchestrator_present(&self) -> bool {
        self.orchestrator.load(Ordering::SeqCst)
    }

    /// Replace the fleet configuration (legacy shape). Takes effect on the next
    /// handshake, and immediately through `mesh.configure` when a sidecar is up.
    pub async fn configure(&self, mesh: MeshConfig) -> Result<()> {
        *self.binaries.lock() = binaries_of(&mesh.binaries);
        *self.accounts.lock() = account_envs_of(&mesh.accounts, &self.env_base.home);
        self.reconfigure()
            .await
            .map_err(|e| GildeError::Internal(format!("mesh.configure failed: {}", e.code.as_str())))
    }

    /// Every orchestrator team view relayed from now on (T02's orchestrator
    /// mapping subscribes; the cache holds the latest of each).
    pub fn subscribe_orchestrator(&self) -> broadcast::Receiver<OrchTeamSnapshot> {
        self.orch_snapshots.subscribe()
    }

    /// Legacy: protocol 2 has no `mesh.event`; nothing is ever sent here.
    pub fn subscribe_run_events(&self) -> broadcast::Receiver<RunEventEnvelope> {
        self.run_events.subscribe()
    }

    /// Lifecycle transitions.
    pub fn subscribe_status(&self) -> broadcast::Receiver<SidecarStatus> {
        self.statuses.subscribe()
    }

    /// One message per dead generation.
    pub fn subscribe_deaths(&self) -> broadcast::Receiver<SidecarDeath> {
        self.deaths.subscribe()
    }

    // -- introspection -------------------------------------------------------

    pub fn generation(&self) -> i64 {
        self.state.lock().generation
    }

    /// The current state.
    pub fn state(&self) -> SidecarState {
        self.state.lock().state
    }

    /// The status with its state rendered as `S` (see [`SidecarStatus`]).
    pub fn status<S: DeserializeOwned + Default>(&self) -> SidecarStatus<S> {
        let s = self.current_status();
        SidecarStatus {
            state: serde_json::from_value(Value::String(s.state.as_str().to_string())).unwrap_or_default(),
            generation: s.generation,
            restarts: s.restarts,
            last_error: s.last_error,
            node_path: s.node_path,
            script_path: s.script_path,
            version_mismatch: s.version_mismatch,
            stays_down: s.stays_down,
        }
    }

    /// The status, typed.
    pub fn current_status(&self) -> SidecarStatus {
        let state = self.state.lock();
        SidecarStatus {
            state: state.state,
            generation: state.generation,
            restarts: state.restarts,
            last_error: state.last_error.clone(),
            node_path: Some(self.runtime.node.display().to_string()),
            script_path: Some(self.runtime.script.display().to_string()),
            version_mismatch: state.version_mismatch,
            stays_down: state.stays_down,
        }
    }

    pub fn runtime(&self) -> &SidecarRuntime {
        &self.runtime
    }

    /// The adapter roster of the live handshake (`mesh.hello.adapters`).
    pub fn roster(&self) -> Vec<wire::AdapterInfo> {
        self.state.lock().roster.clone()
    }

    /// Legacy shape of [`SidecarManager::roster`].
    pub fn adapters(&self) -> Vec<AdapterInfo> {
        let state = self.state.lock();
        state
            .roster
            .iter()
            .map(|a| AdapterInfo {
                vendor: a.vendor.as_str().to_string(),
                capabilities: AdapterCapabilities {
                    resume: a.resume,
                    stream: a.stream,
                    quota: a.quota,
                    ..AdapterCapabilities::default()
                },
                binary: state.configured.get(&a.vendor).map(|p| p.to_string()).unwrap_or_default(),
                binary_path: state.configured.get(&a.vendor).map(|p| p.to_string()),
                installed: a.installed,
                extra: Extra::new(),
            })
            .collect()
    }

    /// Whether a vendor streams — read off the live roster, never hardcoded.
    pub fn can_stream(&self, vendor: &str) -> bool {
        self.state
            .lock()
            .roster
            .iter()
            .any(|a| a.vendor.as_str() == vendor && a.stream)
    }

    /// The last [`STDERR_TAIL_LINES`] stderr lines.
    pub fn stderr_tail(&self) -> Vec<String> {
        self.stderr_tail.lock().iter().cloned().collect()
    }

    /// Turns with a live route (diagnostics, tests).
    pub fn live_turns(&self) -> usize {
        self.turns.len()
    }

    fn client(&self) -> Option<SidecarClient> {
        self.state.lock().client.clone()
    }

    // -- calls ---------------------------------------------------------------

    /// Call the sidecar, mapping every failure to a [`GildeError`].
    pub async fn call<P: Serialize, R: DeserializeOwned>(&self, method: &str, params: P) -> Result<R> {
        self.call_detailed(method, params, None)
            .await
            .map_err(GildeError::from)
    }

    /// The same call, keeping the typed failure.
    pub async fn call_detailed<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: P,
        timeout: Option<Duration>,
    ) -> std::result::Result<R, SidecarCallError> {
        let client = self.client().ok_or(SidecarCallError::Down)?;
        client.call(method, params, timeout).await
    }

    /// The contract error of a failed call (§4.4).
    pub fn ipc_error(&self, err: SidecarCallError) -> IpcError {
        if self.state.lock().version_mismatch {
            return IpcError::version_mismatch();
        }
        match err {
            SidecarCallError::Down | SidecarCallError::Gone { .. } => IpcError::sidecar_down(),
            SidecarCallError::Timeout { .. } => IpcError::timeout(),
            SidecarCallError::Rpc(rpc) => ipc_error_of_rpc(&rpc),
            SidecarCallError::Decode { method, .. } => {
                tracing::warn!(target: "sidecar", method = %method, "an answer did not match the contract shape");
                IpcError::internal(&correlation_id())
            }
        }
    }

    // -- lifecycle -----------------------------------------------------------

    /// Start the sidecar: resolve, self-test, spawn, handshake (hello + configure).
    /// Returns once both answered. A failure leaves the manager `down` with
    /// `last_error` set; the app stays usable either way.
    pub async fn start(self: &Arc<Self>) -> Result<()> {
        let _lock = self.lifecycle.lock().await;
        self.shutting_down.store(false, Ordering::SeqCst);
        if self.state.lock().client.is_some() {
            return Ok(());
        }
        if self.state.lock().version_mismatch {
            return Err(GildeError::Spawn(
                "the sidecar is from another build; restart Gilde to finish updating".to_string(),
            ));
        }
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

    /// Kill the current generation and start a fresh one. Clears the
    /// stays-down rule, because this is a deliberate user action (a build
    /// mismatch stays: only a new Gilde fixes that).
    pub async fn restart(self: &Arc<Self>) -> Result<SidecarStatus> {
        let _lock = self.lifecycle.lock().await;
        self.shutting_down.store(false, Ordering::SeqCst);
        if let Some(dead_generation) = self.take_down("restart requested", true).await {
            self.run_sweeper(dead_generation).await;
        }
        {
            let mut state = self.state.lock();
            state.deaths.clear();
            state.stays_down = false;
            state.last_error = None;
        }
        match self.start_locked().await {
            Ok(()) => Ok(self.current_status()),
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
                .call_value(method::SHUTDOWN, json!({"graceMs": 0}), Some(SHUTDOWN_GRACE))
                .await;
            if let Err(err) = graceful {
                tracing::debug!(target: "sidecar", error = %err, "mesh.shutdown did not answer; signalling");
            }
        }
        self.take_down("shutdown", true).await;
        self.set_state(SidecarState::Down, None);
        Ok(())
    }

    /// Start lazily in the background (B12) unless one is already starting,
    /// running, latched as mismatched, staying down, or disabled.
    fn kick_lazy_start(&self) {
        let Some(me) = self.me.upgrade() else { return };
        {
            let mut state = self.state.lock();
            if state.starting
                || state.client.is_some()
                || state.version_mismatch
                || state.stays_down
                || matches!(state.state, SidecarState::Restarting)
            {
                return;
            }
            state.starting = true;
        }
        tokio::spawn(async move {
            if let Err(err) = me.start().await {
                tracing::warn!(target: "sidecar", error = %err, "the lazy sidecar start failed");
                me.note_start_failure();
            }
            me.state.lock().starting = false;
            me.publish_status();
        });
    }

    /// A failed lazy start counts like a death (§10.4): three within five
    /// minutes and the sidecar stays down instead of being retried per command.
    fn note_start_failure(&self) {
        let mut state = self.state.lock();
        if state.version_mismatch {
            return;
        }
        let now = Instant::now();
        state.deaths.push_back(now);
        while state
            .deaths
            .front()
            .is_some_and(|t| now.duration_since(*t) > DEATH_WINDOW)
        {
            state.deaths.pop_front();
        }
        if state.deaths.len() >= DEATHS_BEFORE_STAYING_DOWN {
            state.stays_down = true;
        }
    }

    /// Spawn one generation. The lifecycle lock must be held. Any failure
    /// leaves the manager `Down` with the reason (never stuck in `Starting`).
    async fn start_locked(self: &Arc<Self>) -> Result<()> {
        let generation = self.state.lock().generation + 1;
        self.set_state_generation(SidecarState::Starting, generation, None);
        let started = self.start_generation(generation).await;
        if let Err(err) = &started {
            if self.state.lock().state == SidecarState::Starting {
                self.set_state(SidecarState::Down, Some(err.to_string()));
            }
        }
        started
    }

    async fn start_generation(self: &Arc<Self>, generation: i64) -> Result<()> {
        // A13.3: the resolved node may run as the sidecar and its own
        // `--version` probe — no other kind.
        crate::supervise::spawn::allow_executable(&self.runtime.node, &[SpawnKind::Sidecar, SpawnKind::Probe]);

        // 1. Self-test the runtime before the real spawn.
        self.self_test().await?;

        // 2. The durable registry row lands before the process exists.
        let cwd = self
            .runtime
            .script
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("/"));
        let ticket = self
            .plan(SpawnRequest {
                kind: SpawnKind::Sidecar,
                argv0: abs_path(&self.runtime.node)?,
                args: vec![self.runtime.script.display().to_string()],
                cwd: AbsPath::parse(cwd.display().to_string()).ok(),
                env_keys: self.env_keys(),
                parent_uid: None,
                generation: Some(generation),
                turn_id: None,
            })
            .await?;

        let env = self
            .supervisor
            .compose_env(self.env_base.clone(), None, &ticket.env_marker);

        let mut cmd = tokio_command_grouped(&self.runtime.node);
        cmd.arg(&self.runtime.script)
            .current_dir(&cwd)
            .env_clear()
            .envs(env)
            // The sidecar's parent-death watchdog (INT-C). Set after `envs` so it
            // wins over any `agents.envPassthrough` entry of the same name.
            .env(ENV_HOST_PID, std::process::id().to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                self.spawns.mark_exited(&ticket, ExitInfo { code: None, signal: None }).await;
                return Err(GildeError::Spawn(format!("could not start the sidecar: {e}")));
            }
        };

        let pid = child.id().map(|p| p as i32).unwrap_or(-1);
        if let Err(err) = self.spawns.mark_running(&ticket, pid).await {
            tracing::warn!(target: "sidecar", code = err.code.as_str(), "could not mark the sidecar row running");
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
            state.guard = Some(ticket.clone());
            state.started_at = Some(Instant::now());
            state.roster.clear();
            state.configured.clear();
        }

        self.spawn_stderr_drain(generation, stderr);
        self.spawn_exit_watcher(generation, child, ticket.clone());
        self.spawn_fault_watcher(generation, fault_rx);

        // 3. Handshake. A protocol or build mismatch is fatal for this generation.
        match self.handshake(&client).await {
            Ok(hello) => {
                tracing::info!(
                    target: "sidecar",
                    generation,
                    pid,
                    build_id = %hello.build_id,
                    adapters = hello.adapters.len(),
                    node_source = self.runtime.node_source.as_str(),
                    "sidecar ready"
                );
                self.state.lock().roster = hello.adapters;
                self.set_state(SidecarState::Ready, None);
                self.spawn_health_loop(generation, client);
                Ok(())
            }
            Err(HandshakeError::VersionMismatch(detail)) => {
                tracing::error!(target: "sidecar", generation, detail = %detail, "the sidecar is from another build; not restarting it");
                {
                    let mut state = self.state.lock();
                    state.version_mismatch = true;
                    state.retired = Some(generation);
                }
                self.take_down(&format!("build mismatch: {detail}"), true).await;
                self.set_state(SidecarState::Down, Some(format!("build mismatch: {detail}")));
                Err(GildeError::Spawn(format!("sidecar build mismatch: {detail}")))
            }
            Err(HandshakeError::Other(detail)) => {
                // A refused handshake (e.g. a protocol-1 peer) is stopped, not
                // restarted: the same peer would refuse again. The next need
                // tries anew, under the start-failure rule.
                self.state.lock().retired = Some(generation);
                self.take_down(&format!("handshake failed: {detail}"), true).await;
                self.set_state(SidecarState::Down, Some(detail.clone()));
                Err(GildeError::Spawn(format!("sidecar handshake failed: {detail}")))
            }
        }
    }

    async fn handshake(&self, client: &SidecarClient) -> std::result::Result<wire::MeshHelloResult, HandshakeError> {
        let providers = wire::Providers {
            orchestrator: self.orchestrator.load(Ordering::SeqCst),
        };
        let params = wire::MeshHelloParams {
            protocol: wire::PROTOCOL,
            build_id: HOST_BUILD_ID.to_string(),
            app_instance_id: self.supervisor.app_instance_id().to_string(),
            providers,
        };
        let hello: wire::MeshHelloResult = match client
            .call(method::HELLO, params, Some(HANDSHAKE_DEADLINE))
            .await
        {
            Ok(hello) => hello,
            Err(SidecarCallError::Rpc(err)) if err.code == code::BUILD_MISMATCH => {
                return Err(HandshakeError::VersionMismatch(format!(
                    "host {HOST_BUILD_ID}, sidecar {}",
                    err.data
                        .as_ref()
                        .and_then(|d| d.get("expected"))
                        .and_then(Value::as_str)
                        .unwrap_or("?")
                )));
            }
            Err(SidecarCallError::Rpc(err)) if err.code == code::PROTOCOL_MISMATCH => {
                return Err(HandshakeError::Other(format!(
                    "protocol mismatch: host speaks {}, the sidecar refused it",
                    wire::PROTOCOL
                )));
            }
            Err(err) => return Err(HandshakeError::Other(err.to_string())),
        };
        if hello.protocol != wire::PROTOCOL {
            return Err(HandshakeError::Other(format!(
                "protocol mismatch: host speaks {}, sidecar speaks {}",
                wire::PROTOCOL,
                hello.protocol
            )));
        }
        if hello.build_id != HOST_BUILD_ID {
            return Err(HandshakeError::VersionMismatch(format!(
                "host {HOST_BUILD_ID}, sidecar {}",
                hello.build_id
            )));
        }
        let mut configure = self.configure_params().await;
        // configure carries the same presence the hello did.
        configure.providers = providers;
        let _: wire::MeshConfigureResult = client
            .call(method::CONFIGURE, &configure, Some(HANDSHAKE_DEADLINE))
            .await
            .map_err(|e| HandshakeError::Other(format!("mesh.configure: {e}")))?;
        {
            let mut state = self.state.lock();
            state.configured = configure.binaries.clone();
            state.hello_orchestrator = Some(providers.orchestrator);
        }
        Ok(hello)
    }

    /// `mesh.configure` from the current binaries, accounts and providers. The
    /// binaries are resolved afresh each time (A12.6).
    async fn configure_params(&self) -> wire::MeshConfigureParams {
        let mut binaries = self.binaries.lock().clone();
        let resolver = self.binary_resolver.lock().clone();
        for vendor in Vendor::ALL {
            if binaries.contains_key(&vendor) {
                continue;
            }
            let found = match &resolver {
                Some(resolve) => resolve(vendor).await,
                None => self.default_binary(vendor),
            };
            if let Some(path) = found {
                binaries.insert(vendor, path);
            }
        }
        wire::MeshConfigureParams {
            binaries,
            accounts: self.accounts.lock().clone(),
            providers: wire::Providers {
                orchestrator: self.orchestrator.load(Ordering::SeqCst),
            },
        }
    }

    /// The pinned `agents.binaries` setting, else the first executable of the
    /// vendor's command name on the resolved `PATH`; realpath'd. Never executed.
    fn default_binary(&self, vendor: Vendor) -> Option<AbsPath> {
        let (key, name) = match vendor {
            Vendor::Claude => ("claude", "claude"),
            Vendor::Codex => ("codex", "codex"),
            Vendor::Cursor => ("cursorAgent", "cursor-agent"),
            Vendor::Agy => ("agy", "agy"),
        };
        let pinned = self
            .supervisor
            .config()
            .get()
            .agent_binary(key)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute() && crate::agents::path::is_executable(p));
        let found = pinned.or_else(|| {
            let dirs: Vec<PathBuf> = std::env::split_paths(&self.env_base.path).collect();
            which_on(&dirs, name)
        })?;
        canonical_abs(&found)
    }

    /// Re-send `mesh.configure` when a sidecar is up (presence or accounts changed).
    async fn reconfigure(&self) -> std::result::Result<(), IpcError> {
        let Some(client) = self.client() else { return Ok(()) };
        let mut configure = self.configure_params().await;
        // The live generation keeps the presence its hello sent (A11.6).
        if let Some(hello) = self.state.lock().hello_orchestrator {
            configure.providers.orchestrator = hello;
        }
        match client
            .call::<_, wire::MeshConfigureResult>(method::CONFIGURE, &configure, Some(HANDSHAKE_DEADLINE))
            .await
        {
            Ok(_) => {
                self.state.lock().configured = configure.binaries;
                Ok(())
            }
            Err(err) => Err(self.ipc_error(err)),
        }
    }

    /// Plan a spawn through the supervisor (A12.8): the §9.3 allowlist, then a
    /// durable row before the process exists.
    async fn plan(&self, request: SpawnRequest) -> Result<SpawnTicket> {
        self.spawns.plan_spawn(request).await.map_err(|err| match err.code {
            ErrorCode::Refused => GildeError::Refused {
                reason: err
                    .params
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("refused")
                    .to_string(),
            },
            code => GildeError::Spawn(format!("the supervisor did not plan the spawn ({})", code.as_str())),
        })
    }

    /// The key NAMES of the env the sidecar's processes get (never values).
    fn env_keys(&self) -> Vec<String> {
        self.supervisor
            .compose_env(self.env_base.clone(), None, "")
            .into_keys()
            .chain(std::iter::once(ENV_HOST_PID.to_string()))
            .collect()
    }

    /// `<runtime> --version`, 5 s cap, through the registry like every spawn.
    async fn self_test(&self) -> Result<()> {
        let ticket = self.plan(self_test_request(&self.runtime.node, self.env_keys())?).await?;

        let mut cmd = tokio_command_grouped(&self.runtime.node);
        cmd.arg("--version")
            .current_dir("/")
            .env_clear()
            .envs(
                self.supervisor
                    .compose_env(self.env_base.clone(), None, &ticket.env_marker),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                self.spawns.mark_exited(&ticket, ExitInfo { code: None, signal: None }).await;
                return Err(GildeError::SidecarRuntimeUnusable {
                    path: self.runtime.node.display().to_string(),
                    stderr: e.to_string(),
                });
            }
        };
        let pid = child.id().map(|p| p as i32).unwrap_or(-1);
        let _ = self.spawns.mark_running(&ticket, pid).await;

        let output = match tokio::time::timeout(SELF_TEST_TIMEOUT, child.wait_with_output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => {
                self.spawns.mark_exited(&ticket, ExitInfo { code: None, signal: None }).await;
                return Err(GildeError::SidecarRuntimeUnusable {
                    path: self.runtime.node.display().to_string(),
                    stderr: e.to_string(),
                });
            }
            Err(_) => {
                let signal = terminate_group(pid, Duration::from_millis(500), Escalate::IfIgnored).await;
                self.spawns.mark_exited(&ticket, ExitInfo { code: None, signal }).await;
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
        self.spawns.mark_exited(&ticket, ExitInfo { code, signal: None }).await;
        if !output.status.success() {
            return Err(GildeError::SidecarRuntimeUnusable {
                path: self.runtime.node.display().to_string(),
                stderr: String::from_utf8(output.stderr)
                    .unwrap_or_else(|e| format!("<{} non-utf8 stderr bytes>", e.as_bytes().len())),
            });
        }
        Ok(())
    }

    // -- teardown ------------------------------------------------------------

    /// Close the channel and make sure the process is gone. Returns the
    /// generation that was taken down, if any.
    async fn take_down(&self, reason: &str, deliberate: bool) -> Option<i64> {
        let detached = self.detach()?;
        Some(self.finish_detached(detached, reason, deliberate, Escalate::IfIgnored).await)
    }

    /// Take the live generation out of the state (synchronously).
    fn detach(&self) -> Option<Detached> {
        let mut state = self.state.lock();
        if state.client.is_none() && state.child_pid.is_none() {
            return None;
        }
        state.roster.clear();
        state.configured.clear();
        state.hello_orchestrator = None;
        Some(Detached {
            client: state.client.take(),
            pid: state.child_pid.take(),
            guard: state.guard.take(),
            generation: state.generation,
        })
    }

    /// Close and stop a detached generation; its turns end, its orphans are
    /// left for the sweep.
    async fn finish_detached(&self, detached: Detached, reason: &str, deliberate: bool, escalate: Escalate) -> i64 {
        let Detached {
            client,
            pid,
            guard,
            generation,
        } = detached;
        if let Some(client) = client {
            client.close(reason);
        }
        let mut signal = None;
        if let Some(pid) = pid.filter(|p| *p > 0) {
            signal = terminate_group(pid, TERMINATE_GRACE, escalate).await;
        }
        if let Some(ticket) = guard {
            self.spawns.mark_exited(&ticket, ExitInfo { code: None, signal }).await;
        }
        self.settle_orphan_guards(reason);
        self.fail_turns_of(generation);
        let _ = self.deaths.send(SidecarDeath {
            generation,
            reason: reason.to_string(),
            deliberate,
        });
        generation
    }

    /// Every child the dead generation had registered but never reported an
    /// exit for: the rows stay `running` for the reaper's fenced sweep.
    fn settle_orphan_guards(&self, reason: &str) {
        let ids: Vec<String> = self.guards.iter().map(|e| e.key().clone()).collect();
        for id in ids {
            if let Some((_, ticket)) = self.guards.remove(&id) {
                tracing::debug!(
                    target: "sidecar",
                    spawn_id = %ticket.spawn_id,
                    reason,
                    "a sidecar child outlived its generation; leaving the row for the sweep"
                );
            }
        }
    }

    /// End every turn of a dead generation with one `Error{HELPER_RESTARTED}`.
    fn fail_turns_of(&self, generation: i64) {
        let doomed: Vec<Arc<TurnRoute>> = self
            .turns
            .iter()
            .filter(|e| e.value().generation <= generation)
            .map(|e| e.value().clone())
            .collect();
        for route in doomed {
            route.fail_helper_restarted();
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
                "no generation sweeper installed; rows are left for the boot sweep"
            ),
        }
    }

    // -- supervision tasks ---------------------------------------------------

    fn spawn_stderr_drain(self: &Arc<Self>, generation: i64, stderr: tokio::process::ChildStderr) {
        let manager = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
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
        ticket: SpawnTicket,
    ) {
        let manager = Arc::downgrade(self);
        let spawns = self.spawns.clone();
        tokio::spawn(async move {
            let status = child.wait().await;
            let (code, signal) = match &status {
                Ok(status) => (status.code(), crate::supervise::spawn::exit_signal_of(status)),
                Err(_) => (None, None),
            };
            spawns.mark_exited(&ticket, ExitInfo { code, signal }).await;

            let Some(manager) = manager.upgrade() else { return };
            if code == Some(wire::EXIT_BUILD_MISMATCH) {
                // The sidecar refused our build id (§4.5): latch, never restart.
                manager.state.lock().version_mismatch = true;
            }
            let reason = match (code, signal) {
                (Some(78), _) => "the sidecar is from another build (exit 78)".to_string(),
                (_, Some(sig)) => format!("sidecar killed by signal {sig}"),
                (Some(code), _) => format!("sidecar exited with code {code}"),
                _ => "sidecar exited".to_string(),
            };
            manager.on_generation_died(generation, reason).await;
        });
    }

    fn spawn_fault_watcher(self: &Arc<Self>, generation: i64, mut faults: mpsc::UnboundedReceiver<ClientFault>) {
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
                        // the exit watcher takes the restart path (generation + 1).
                        manager.set_fault_reason(format!(
                            "the sidecar sent a {bytes} byte line (cap {MAX_LINE_BYTES}); restarting"
                        ));
                        manager.emit_state(SidecarState::Degraded);
                        manager.kill_current_generation(generation).await;
                    }
                    ClientFault::Io { detail } => {
                        manager.note_error(format!("sidecar channel io error: {detail}"));
                    }
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
                        tracing::warn!(target: "sidecar", generation, misses, "mesh.ping missed its deadline");
                        if misses >= PING_MISSES_BEFORE_HANG {
                            manager.set_fault_reason(format!(
                                "the sidecar missed {misses} consecutive pings; killing it"
                            ));
                            manager.emit_state(SidecarState::Degraded);
                            manager.kill_current_generation(generation).await;
                            return;
                        }
                    }
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
            terminate_group(pid, TERMINATE_GRACE, Escalate::IfIgnored).await;
        }
    }

    /// The single funnel for "a generation is no longer usable".
    async fn on_generation_died(self: Arc<Self>, dead_generation: i64, reason: String) {
        let deliberate = self.shutting_down.load(Ordering::SeqCst)
            || self.state.lock().retired == Some(dead_generation);
        let reason = match self.take_fault_reason() {
            Some(fault) => format!("{fault} — {reason}"),
            None => reason,
        };
        {
            let _lock = self.lifecycle.lock().await;
            {
                let state = self.state.lock();
                if state.generation != dead_generation {
                    return; // already superseded
                }
            }
            let had_client = {
                let mut state = self.state.lock();
                let had = state.client.is_some() || state.child_pid.is_some();
                if let Some(client) = state.client.take() {
                    client.close(&reason);
                }
                state.child_pid = None;
                state.guard = None;
                state.roster.clear();
                state.configured.clear();
                state.started_at = None;
                had
            };

            self.settle_orphan_guards(&reason);
            self.fail_turns_of(dead_generation);
            if had_client {
                let _ = self.deaths.send(SidecarDeath {
                    generation: dead_generation,
                    reason: reason.clone(),
                    deliberate,
                });
            }

            let mismatch = self.state.lock().version_mismatch;
            if deliberate || mismatch {
                self.set_state(SidecarState::Down, mismatch.then(|| reason.clone()));
                self.run_sweeper(dead_generation).await;
                return;
            }

            tracing::warn!(target: "sidecar", generation = dead_generation, reason = %reason, "sidecar generation died");
            self.note_error(reason.clone());

            // §10.4: three deaths within five minutes → it stays down.
            let stays_down = {
                let mut state = self.state.lock();
                let now = Instant::now();
                state.deaths.push_back(now);
                while state
                    .deaths
                    .front()
                    .is_some_and(|t| now.duration_since(*t) > DEATH_WINDOW)
                {
                    state.deaths.pop_front();
                }
                if state.deaths.len() >= DEATHS_BEFORE_STAYING_DOWN {
                    state.stays_down = true;
                }
                state.stays_down
            };
            self.set_state(SidecarState::Restarting, Some(reason.clone()));

            // The sweep runs BEFORE any replacement can take work.
            self.run_sweeper(dead_generation).await;

            if stays_down {
                tracing::error!(
                    target: "sidecar",
                    deaths = DEATHS_BEFORE_STAYING_DOWN,
                    window_secs = DEATH_WINDOW.as_secs(),
                    "the sidecar keeps dying; it stays down (SIDECAR_DOWN for its commands)"
                );
                self.set_state(
                    SidecarState::Down,
                    Some(format!(
                        "the sidecar died {DEATHS_BEFORE_STAYING_DOWN} times within {} minutes; last: {reason}",
                        DEATH_WINDOW.as_secs() / 60
                    )),
                );
                return;
            }
        }
        self.clone().restart_with_backoff(dead_generation).await;
    }

    async fn restart_with_backoff(self: Arc<Self>, dead_generation: i64) {
        loop {
            let (attempt, restarts) = {
                let mut state = self.state.lock();
                state.restarts += 1;
                (state.deaths.len().max(1), state.restarts)
            };
            let index = (attempt - 1).min(RESTART_BACKOFF_SECS.len() - 1);
            let wait = Duration::from_secs(RESTART_BACKOFF_SECS[index]);
            tracing::info!(target: "sidecar", restarts, wait_secs = wait.as_secs(), "restarting the sidecar after a backoff");
            tokio::time::sleep(wait).await;

            let _lock = self.lifecycle.lock().await;
            if self.shutting_down.load(Ordering::SeqCst) {
                return;
            }
            {
                let state = self.state.lock();
                if state.client.is_some()
                    || state.generation != dead_generation
                    || state.version_mismatch
                    || state.stays_down
                {
                    return;
                }
            }
            match self.start_locked().await {
                Ok(()) => return,
                Err(err) => {
                    tracing::error!(target: "sidecar", error = %err, "sidecar restart failed");
                    let give_up = {
                        let mut state = self.state.lock();
                        state.deaths.push_back(Instant::now());
                        if state.deaths.len() >= DEATHS_BEFORE_STAYING_DOWN {
                            state.stays_down = true;
                        }
                        state.stays_down || state.version_mismatch
                    };
                    self.set_state(SidecarState::Down, Some(err.to_string()));
                    if give_up {
                        return;
                    }
                }
            }
        }
    }

    // -- state bookkeeping ---------------------------------------------------

    fn note_error(&self, detail: String) {
        self.state.lock().last_error = Some(detail);
    }

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
        let _ = self.statuses.send(self.current_status());
    }

    // -- the supervision seam (§4.5 host.*) ----------------------------------

    /// Whether `cmd` may be spawned by the sidecar (§4.5, §9.3): exactly a
    /// binary the live generation was configured with, or the bundled node.
    fn spawn_allowed(&self, cmd: &str) -> bool {
        let path = Path::new(cmd);
        if !path.is_absolute() {
            return false;
        }
        let canonical = std::fs::canonicalize(path).ok();
        let same = |allowed: &Path| -> bool {
            allowed == path
                || std::fs::canonicalize(allowed)
                    .ok()
                    .zip(canonical.as_ref())
                    .is_some_and(|(a, c)| &a == c)
        };
        if same(&self.runtime.node) {
            return true;
        }
        let state = self.state.lock();
        state.configured.values().any(|p| same(Path::new(p.as_str())))
    }

    async fn handle_host_call(
        self: Arc<Self>,
        generation: i64,
        method: &str,
        params: Value,
    ) -> std::result::Result<Value, RpcErrorObject> {
        match method {
            host_method::REGISTER_SPAWN => {
                let p: wire::RegisterSpawnParams = decode(params)?;
                if !self.spawn_allowed(&p.cmd) {
                    tracing::warn!(
                        target: "sidecar",
                        generation,
                        kind = ?p.kind,
                        "refused a sidecar spawn: argv[0] is not a configured vendor binary"
                    );
                    return Err(RpcErrorObject::new(code::REFUSED, "notAllowlisted")
                        .with_data(json!({"reason": "notAllowlisted"})));
                }
                let request = register_spawn_request(&p, generation)?;
                // The durable INSERT completes before this returns — the reason
                // the channel is bidirectional.
                let ticket = self.spawns.plan_spawn(request).await.map_err(|e| rpc_error_of_plan(&e))?;
                tracing::debug!(
                    target: "sidecar",
                    generation,
                    spawn_id = %ticket.spawn_id,
                    kind = ?p.kind,
                    // Key NAMES only: an env value must never reach a host log.
                    env_keys = p.env_keys.len(),
                    "registered a sidecar spawn"
                );
                let marker = ticket.env_marker.clone();
                self.guards.insert(p.spawn_id.clone(), ticket);
                to_value(wire::RegisterSpawnResult {
                    ok: true,
                    env_marker: marker,
                })
            }
            host_method::SPAWNED => {
                let p: wire::SpawnedParams = decode(params)?;
                match self.guards.get(&p.spawn_id).map(|g| g.clone()) {
                    Some(ticket) => match self.spawns.mark_running(&ticket, p.pid).await {
                        Ok(()) => to_value(json!({"ok": true})),
                        Err(e) => Err(RpcErrorObject::new(
                            code::INTERNAL_ERROR,
                            format!("could not mark the child running ({})", e.code.as_str()),
                        )),
                    },
                    None => Err(RpcErrorObject::new(code::INVALID_PARAMS, "unknown spawnId")),
                }
            }
            host_method::EXITED => {
                let p: wire::ExitedParams = decode(params)?;
                match self.guards.remove(&p.spawn_id) {
                    Some((_, ticket)) => {
                        let signal = p.signal.as_deref().and_then(types::signal_number);
                        if types::is_deliberate_close(p.exit_code, p.signal.as_deref()) {
                            tracing::debug!(target: "sidecar", generation, "a sidecar child was closed after its output was read");
                        }
                        self.spawns
                            .mark_exited(
                                &ticket,
                                ExitInfo {
                                    code: p.exit_code,
                                    signal,
                                },
                            )
                            .await;
                        to_value(json!({"ok": true}))
                    }
                    // The row is settled by the reaper either way.
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

    /// `turn.event` → the turn's route. Unknown fields, an unknown turn or an
    /// out-of-order `seq` are dropped and logged, never guessed (§13.7).
    fn turn_event(&self, generation: i64, params: Value) {
        let p: wire::TurnEventParams = match serde_json::from_value(params) {
            Ok(p) => p,
            Err(err) => {
                tracing::warn!(target: "sidecar", generation, error = %err, "turn.event did not match the contract; dropped");
                return;
            }
        };
        let Some(route) = self.turns.get(&p.turn_id).map(|r| r.clone()) else {
            tracing::warn!(target: "sidecar", generation, "turn.event for an unknown turn; dropped");
            return;
        };
        if route.generation != generation {
            tracing::warn!(target: "sidecar", generation, "turn.event from another generation; dropped");
            return;
        }
        if route.push(p.seq, p.event) == Admit::Rejected {
            tracing::warn!(target: "sidecar", generation, seq = p.seq, "turn.event out of order or after the end; dropped");
        }
    }

    /// `orch.snapshot` → the team cache and the subscribers.
    fn orch_snapshot(&self, generation: i64, params: Value) {
        let Some(event) = team_snapshot_event(generation, params, owner_start_ms) else {
            return;
        };
        self.team_cache
            .apply_view(event.generation, event.rev, &event.team_id, event.view.as_ref());
        let _ = self.orch_snapshots.send(event);
    }

    fn host_log(&self, params: Value) {
        let Ok(p) = serde_json::from_value::<wire::HostLogParams>(params) else {
            tracing::warn!(target: "sidecar", "host.log did not match the contract; dropped");
            return;
        };
        let fields = Value::Object(p.fields).to_string();
        match p.level {
            wire::HostLogLevel::Error => tracing::error!(target: "sidecar", fields = %fields, "{}", p.msg),
            wire::HostLogLevel::Warn => tracing::warn!(target: "sidecar", fields = %fields, "{}", p.msg),
            wire::HostLogLevel::Debug => tracing::debug!(target: "sidecar", fields = %fields, "{}", p.msg),
            wire::HostLogLevel::Info => tracing::info!(target: "sidecar", fields = %fields, "{}", p.msg),
        }
    }

    // -- the SidecarApi plumbing ---------------------------------------------

    /// Wait for `Ready` up to `wait`, starting lazily if nothing is running.
    async fn await_ready(&self, wait: Duration) -> std::result::Result<(), IpcError> {
        {
            let state = self.state.lock();
            if state.version_mismatch {
                return Err(IpcError::version_mismatch());
            }
            if state.stays_down {
                return Err(IpcError::sidecar_down());
            }
            if state.client.is_some() && state.state == SidecarState::Ready {
                return Ok(());
            }
            if state.state == SidecarState::Restarting {
                return Err(IpcError::sidecar_down());
            }
        }
        if !self.supervisor.config().get().sidecar.enabled {
            return Err(IpcError::sidecar_down().param("reason", "disabled"));
        }
        let mut statuses = self.statuses.subscribe();
        self.kick_lazy_start();
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            {
                let state = self.state.lock();
                if state.version_mismatch {
                    return Err(IpcError::version_mismatch());
                }
                if state.client.is_some() && state.state == SidecarState::Ready {
                    return Ok(());
                }
                if state.stays_down {
                    return Err(IpcError::sidecar_down());
                }
                if !state.starting && state.state == SidecarState::Down && state.last_error.is_some() && state.client.is_none() {
                    // The start failed (runtime unusable, handshake refused…).
                    return Err(IpcError::sidecar_down());
                }
            }
            match tokio::time::timeout_at(deadline, statuses.recv()).await {
                Ok(Ok(_)) | Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(broadcast::error::RecvError::Closed)) | Err(_) => {
                    return Err(IpcError::not_ready("sidecarStarting"));
                }
            }
        }
    }

    async fn probe_call<R: DeserializeOwned>(&self, method_name: &str, p: &wire::ProbeParams) -> std::result::Result<R, IpcError> {
        self.await_ready(ENSURE_WAIT).await?;
        self.call_detailed::<_, R>(method_name, p, Some(PROBE_DEADLINE))
            .await
            .map_err(|e| self.ipc_error(e))
    }
}

/// Whether a stop may escalate to SIGKILL: only a crashed or hung generation
/// of Gilde's own helper (§9.8); a presence stop never does (A11.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Escalate {
    IfIgnored,
    Never,
}

/// A generation taken out of the live state, still to be closed and stopped.
struct Detached {
    client: Option<SidecarClient>,
    pid: Option<i32>,
    guard: Option<SpawnTicket>,
    generation: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PresenceCheck {
    /// No sidecar, or another generation: nothing to stop.
    Gone,
    /// The running generation already has the wanted value.
    Unchanged,
    /// Requests or turns in flight (or not ready yet): wait.
    Busy,
    Idle,
}

/// How often a presence stop re-checks for idleness.
const PRESENCE_IDLE_POLL: Duration = Duration::from_millis(100);

#[derive(Debug)]
enum HandshakeError {
    VersionMismatch(String),
    Other(String),
}

/// Legacy binaries → protocol-2 binaries (absolute, canonical paths only).
fn binaries_of(paths: &BinaryPaths) -> BTreeMap<Vendor, AbsPath> {
    let mut out = BTreeMap::new();
    for (vendor, path) in [
        (Vendor::Claude, &paths.claude),
        (Vendor::Codex, &paths.codex),
        (Vendor::Cursor, &paths.cursor_agent),
        (Vendor::Agy, &paths.agy),
    ] {
        if let Some(p) = path.as_deref().filter(|p| Path::new(p).is_absolute()) {
            if let Some(abs) = canonical_abs(Path::new(p)) {
                out.insert(vendor, abs);
            }
        }
    }
    out
}

/// A fresh correlation id for an `INTERNAL` error (never content).
fn correlation_id() -> String {
    format!("sc-{}", uuid::Uuid::now_v7().simple())
}

/// The contract error of a JSON-RPC error from the sidecar (§4.4, §4.5).
pub fn ipc_error_of_rpc(err: &RpcErrorObject) -> IpcError {
    let data = err.data.as_ref();
    let str_of = |key: &str| data.and_then(|d| d.get(key)).and_then(Value::as_str);
    match err.code {
        code::INVALID_PARAMS => match (str_of("field"), str_of("rule")) {
            (Some(field), Some(rule)) => IpcError::invalid_argument(field, rule),
            _ => IpcError::internal(&correlation_id()),
        },
        code::METHOD_NOT_FOUND => IpcError::unsupported("sidecarMethod"),
        code::UNSUPPORTED => IpcError::unsupported(str_of("cap").unwrap_or("sidecar")),
        code::ADAPTER_ERROR => {
            let class = match str_of("class") {
                Some("auth") => VendorErrorClass::Auth,
                Some("quota") => VendorErrorClass::Quota,
                Some("exec") => VendorErrorClass::Exec,
                _ => VendorErrorClass::Unknown,
            };
            IpcError::vendor_error(class, matches!(class, VendorErrorClass::Exec | VendorErrorClass::Unknown))
        }
        code::BUILD_MISMATCH => IpcError::version_mismatch(),
        code::NOT_READY => IpcError::not_ready("sidecarStarting"),
        code::REFUSED => IpcError::refused(str_of("reason").unwrap_or("refused")),
        code::CHANNEL_CLOSED => IpcError::sidecar_down(),
        code::TIMEOUT => IpcError::timeout(),
        code::PROTOCOL_MISMATCH => IpcError::version_mismatch(),
        _ => IpcError::internal(&correlation_id()),
    }
}

#[async_trait]
impl SidecarApi for SidecarManager {
    async fn ensure_started(&self) -> std::result::Result<(), IpcError> {
        self.await_ready(ENSURE_WAIT).await
    }

    async fn auth_status(&self, p: wire::ProbeParams) -> std::result::Result<wire::AuthStatus, IpcError> {
        self.probe_call(method::PROBE_AUTH_STATUS, &p).await
    }

    async fn models(&self, p: wire::ProbeParams) -> std::result::Result<Vec<wire::ModelInfo>, IpcError> {
        self.probe_call(method::PROBE_MODELS, &p).await
    }

    async fn quota(&self, p: wire::ProbeParams) -> std::result::Result<Option<Quota>, IpcError> {
        self.probe_call(method::PROBE_QUOTA, &p).await
    }

    async fn turn_start(
        &self,
        p: wire::TurnStartParams,
        sink: Arc<dyn TurnEventSink>,
    ) -> std::result::Result<(), IpcError> {
        if p.turn_id.is_empty() || p.turn_id.len() > 128 || !p.turn_id.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
            return Err(IpcError::invalid_argument("turnId", "pattern"));
        }
        // The route exists before the request: events may follow the answer at
        // once. It is claimed under the turn gate on a READY generation, so a
        // presence stop (A11.6) never takes a generation with a turn on it; a
        // generation detached meanwhile means waiting for the next one.
        let mut attempts = 0;
        let route = loop {
            self.await_ready(ENSURE_WAIT).await?;
            let _gate = self.turn_gate.read();
            let generation = {
                let state = self.state.lock();
                (state.client.is_some() && state.state == SidecarState::Ready).then_some(state.generation)
            };
            let Some(generation) = generation else {
                attempts += 1;
                if attempts >= 3 {
                    return Err(IpcError::not_ready("sidecarStarting"));
                }
                continue;
            };
            let route = Arc::new(TurnRoute {
                generation,
                sink: sink.clone(),
                queue: Mutex::new(TurnQueue::default()),
                wake: Notify::new(),
                done: AtomicBool::new(false),
            });
            match self.turns.entry(p.turn_id.clone()) {
                dashmap::mapref::entry::Entry::Occupied(_) => return Err(IpcError::conflict("turnId")),
                dashmap::mapref::entry::Entry::Vacant(slot) => {
                    slot.insert(route.clone());
                }
            }
            break route;
        };
        spawn_turn_delivery(self.me.clone(), p.turn_id.clone(), route);
        match self
            .call_detailed::<_, wire::TurnStartResult>(method::TURN_START, &p, Some(TURN_CALL_DEADLINE))
            .await
        {
            Ok(res) if res.accepted => Ok(()),
            Ok(_) => {
                self.drop_route(&p.turn_id);
                Err(IpcError::internal(&correlation_id()))
            }
            Err(err) => {
                self.drop_route(&p.turn_id);
                if matches!(err, SidecarCallError::Timeout { .. }) {
                    // The sidecar may have started it after all: nobody listens
                    // any more, so it is cancelled rather than left running.
                    let cancel = wire::TurnCancelParams {
                        turn_id: p.turn_id.clone(),
                    };
                    if let Some(client) = self.client() {
                        tokio::spawn(async move {
                            let _ = client
                                .call::<_, wire::TurnCancelResult>(method::TURN_CANCEL, &cancel, Some(TURN_CALL_DEADLINE))
                                .await;
                        });
                    }
                }
                Err(self.ipc_error(err))
            }
        }
    }

    async fn turn_cancel(&self, turn_id: &str) -> std::result::Result<bool, IpcError> {
        if self.client().is_none() {
            // The generation is gone: its turns already ended (HELPER_RESTARTED)
            // and the sweep stops its children; nothing to signal.
            return Ok(false);
        }
        self.call_detailed::<_, wire::TurnCancelResult>(
            method::TURN_CANCEL,
            &wire::TurnCancelParams {
                turn_id: turn_id.to_string(),
            },
            Some(TURN_CALL_DEADLINE),
        )
        .await
        .map(|r| r.signalled)
        .map_err(|e| self.ipc_error(e))
    }
}

impl SidecarManager {
    fn drop_route(&self, turn_id: &str) {
        if let Some((_, route)) = self.turns.remove(turn_id) {
            route.done.store(true, Ordering::SeqCst);
            route.wake.notify_one();
        }
    }
}

/// One delivery task per turn: pops the queue in order and calls the sink;
/// ends (and forgets the route) after the terminal event.
fn spawn_turn_delivery(manager: Weak<SidecarManager>, turn_id: String, route: Arc<TurnRoute>) {
    tokio::spawn(async move {
        loop {
            let next = route.queue.lock().pop();
            match next {
                Some((seq, event)) => {
                    let terminal = TurnQueue::is_terminal(&event);
                    route.sink.event(seq, event);
                    if terminal {
                        route.done.store(true, Ordering::SeqCst);
                        if let Some(manager) = manager.upgrade() {
                            manager.turns.remove_if(&turn_id, |_, r| Arc::ptr_eq(r, &route));
                        }
                        return;
                    }
                }
                None => {
                    if route.done.load(Ordering::SeqCst) {
                        return;
                    }
                    route.wake.notified().await;
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// orchestrator snapshots
// ---------------------------------------------------------------------------

/// Turn an `orch.snapshot` notification into the relayed view, or `None` when
/// it must be dropped. The envelope is the contract's `OrchSnapshotParams`
/// (strict); the `team` payload is the observer's `{teamId, view}`, which must
/// decode into the mirror and name ONE team consistently. Then the fields that
/// are Rust's own are stamped: the generation key, and L7's `believed`.
pub(crate) fn team_snapshot_event(
    host_generation: i64,
    params: Value,
    start_ms_of_live_pid: impl Fn(i64) -> Option<i64>,
) -> Option<OrchTeamSnapshot> {
    let envelope: wire::OrchSnapshotParams = match serde_json::from_value(params) {
        Ok(envelope) => envelope,
        Err(err) => {
            tracing::warn!(target: "sidecar", error = %err, "orch.snapshot did not match the contract; dropped");
            return None;
        }
    };
    let mut team = envelope.team;
    if let Value::Object(map) = &mut team {
        map.insert("rev".to_string(), json!(envelope.rev));
    }
    let snapshot: CcoTeamSnapshotParams = match serde_json::from_value(team) {
        Ok(snapshot) => snapshot,
        Err(err) => {
            tracing::warn!(target: "sidecar", error = %err, "orch.snapshot team did not decode; dropped");
            return None;
        }
    };
    if !is_cco_team_id(&snapshot.team_id) {
        tracing::warn!(target: "sidecar", "orch.snapshot without a team id; dropped");
        return None;
    }
    let generation = orch_generation_key(host_generation, envelope.generation);
    let mut view = snapshot.view;
    if let Some(view) = view.as_mut() {
        if view.team_id != snapshot.team_id || !view.is_consistent() {
            tracing::warn!(target: "sidecar", "orch.snapshot names two teams; dropped");
            return None;
        }
        view.apply_belief(generation, start_ms_of_live_pid);
    }
    Some(OrchTeamSnapshot {
        generation,
        rev: envelope.rev,
        team_id: snapshot.team_id,
        view,
    })
}

/// The start time, in epoch milliseconds, of `pid` when it is alive — the probe
/// L7 needs ([`cco_believed`]). `None` for a dead, impossible or unreadable pid.
pub(crate) fn owner_start_ms(pid: i64) -> Option<i64> {
    let pid = i32::try_from(pid).ok().filter(|p| *p > 1)?;
    if !is_alive(pid) {
        return None;
    }
    crate::supervise::procscan::start_time_of(pid).map(|secs| secs.saturating_mul(1000))
}

fn decode<T: DeserializeOwned>(params: Value) -> std::result::Result<T, RpcErrorObject> {
    serde_json::from_value(params).map_err(|e| RpcErrorObject::new(code::INVALID_PARAMS, e.to_string()))
}

fn to_value<T: Serialize>(value: T) -> std::result::Result<Value, RpcErrorObject> {
    serde_json::to_value(value).map_err(|e| RpcErrorObject::new(code::INTERNAL_ERROR, e.to_string()))
}

/// The `host.*` surface and the notifications, bound to one generation.
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
                return Err(RpcErrorObject::new(code::INTERNAL_ERROR, "the host is shutting down"));
            };
            manager.handle_host_call(generation, &method, params).await
        })
    }

    fn notify(&self, method: &str, params: Value) {
        let Some(manager) = self.manager.upgrade() else {
            return;
        };
        match method {
            host_method::TURN_EVENT => manager.turn_event(self.generation, params),
            host_method::TEAM_SNAPSHOT => manager.orch_snapshot(self.generation, params),
            host_method::LOG => manager.host_log(params),
            other => tracing::warn!(target: "sidecar", method = other, "unknown notification; ignored"),
        }
    }
}

/// A path as the contract's absolute path (`argv0` of a spawn request).
/// The registry request for a child the sidecar registers
/// (`host.registerSpawn`), through `SupervisorApi` (A12.8: §9.3 allowlist,
/// A13.1 ids): a turn child is `Turn` with its generation and turn id; a probe
/// of a vendor CLI is `Vendorcli` with the generation too (A15.1), so the
/// dead-generation sweep finds a probe orphaned by a sidecar crash.
fn register_spawn_request(
    p: &wire::RegisterSpawnParams,
    generation: i64,
) -> std::result::Result<SpawnRequest, RpcErrorObject> {
    let (kind, turn_id) = match p.kind {
        wire::SidecarSpawnKind::Probe => (SpawnKind::Vendorcli, None),
        wire::SidecarSpawnKind::Turn => (SpawnKind::Turn, p.turn_id.clone().filter(|t| !t.is_empty())),
    };
    let argv0 = AbsPath::parse(p.cmd.clone())
        .map_err(|_| RpcErrorObject::new(code::REFUSED, "notAllowlisted").with_data(json!({"reason": "notAllowlisted"})))?;
    Ok(SpawnRequest {
        kind,
        argv0,
        args: p.args.clone(),
        cwd: p.cwd.clone().and_then(|c| AbsPath::parse(c).ok()),
        env_keys: p.env_keys.clone(),
        parent_uid: None,
        generation: Some(generation),
        turn_id,
    })
}

/// The node `<runtime> --version` self-test: a `Probe` row of no generation
/// (A13.3: node is allowlisted as Sidecar + Probe only; A15.1 leaves Probe's
/// generation reserved).
fn self_test_request(node: &Path, env_keys: Vec<String>) -> Result<SpawnRequest> {
    Ok(SpawnRequest {
        kind: SpawnKind::Probe,
        argv0: abs_path(node)?,
        args: vec!["--version".to_string()],
        cwd: None,
        env_keys,
        parent_uid: None,
        generation: None,
        turn_id: None,
    })
}

fn abs_path(path: &Path) -> Result<AbsPath> {
    AbsPath::parse(path.display().to_string())
        .map_err(|_| GildeError::Spawn(format!("not an absolute path: {}", path.display())))
}

/// The RPC answer to the sidecar for a spawn the supervisor would not plan.
fn rpc_error_of_plan(err: &IpcError) -> RpcErrorObject {
    match err.code {
        ErrorCode::Refused => {
            let reason = err
                .params
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("notAllowlisted")
                .to_string();
            RpcErrorObject::new(code::REFUSED, reason.clone()).with_data(json!({ "reason": reason }))
        }
        ErrorCode::InvalidArgument => RpcErrorObject::new(code::INVALID_PARAMS, "invalid spawn request")
            .with_data(json!({ "field": err.params.get("field"), "rule": err.params.get("rule") })),
        other => RpcErrorObject::new(
            code::INTERNAL_ERROR,
            format!("could not record the spawn ({})", other.as_str()),
        ),
    }
}

// ---------------------------------------------------------------------------
// signals
// ---------------------------------------------------------------------------

/// SIGTERM the group, wait out `grace`, escalate to SIGKILL if it is still
/// there. Only ever used on the SIDECAR's own node process group (created with
/// `process_group(0)`), a Gilde helper that §9.8 allows to escalate — never on a
/// vendor CLI, whose stops go through the fenced kill path. Returns the signal
/// that actually ended it (`None`: no signal was sent).
async fn terminate_group(pid: i32, grace: Duration, escalate: Escalate) -> Option<i32> {
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
    if escalate == Escalate::Never {
        // A11.6: a repeated SIGTERM, never SIGKILL; a survivor is logged.
        let _ = killpg(group, Signal::SIGTERM);
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline && is_alive(pid) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        if is_alive(pid) {
            tracing::warn!(target: "sidecar", pid, "the sidecar outlived two SIGTERMs; left to its parent watchdog");
        }
        return Some(libc::SIGTERM);
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
    use crate::contract::error::ErrorCode;
    use crate::contract::sidecar::{TurnEvent, TurnUsage};

    fn register(kind: wire::SidecarSpawnKind, turn_id: Option<&str>) -> wire::RegisterSpawnParams {
        wire::RegisterSpawnParams {
            spawn_id: "sc-1".into(),
            turn_id: turn_id.map(str::to_string),
            vendor: Some(crate::contract::common::Vendor::Claude),
            kind,
            cmd: "/opt/bin/claude".into(),
            args: vec!["--version".into()],
            cwd: None,
            env_keys: vec!["PATH".into()],
        }
    }

    /// A15.1: a vendor-CLI probe the sidecar registers carries the sidecar's
    /// generation (the dead-generation sweep finds it after a crash), and the
    /// validator of the one spawn path accepts it.
    #[test]
    fn a_sidecar_probe_child_carries_the_generation() {
        let req = register_spawn_request(&register(wire::SidecarSpawnKind::Probe, None), 7).expect("request");
        assert_eq!(req.kind, SpawnKind::Vendorcli);
        assert_eq!(req.generation, Some(7));
        assert_eq!(req.turn_id, None);
        crate::supervise::registry::validate_spawn_ids(req.kind, req.generation, req.turn_id.as_deref())
            .expect("a Vendorcli generation ≥ 1 is valid");
        let turn = register_spawn_request(&register(wire::SidecarSpawnKind::Turn, Some("t-1")), 7).expect("turn");
        assert_eq!((turn.kind, turn.generation, turn.turn_id.as_deref()), (SpawnKind::Turn, Some(7), Some("t-1")));
    }

    /// A13.3/A15.1: the node `--version` self-test stays a `Probe` row with
    /// no generation (reserved for Probe).
    #[test]
    fn the_node_self_test_is_a_probe_without_a_generation() {
        let req = self_test_request(Path::new("/opt/node/bin/node"), vec!["PATH".into()]).expect("request");
        assert_eq!(req.kind, SpawnKind::Probe);
        assert_eq!(req.generation, None);
        crate::supervise::registry::validate_spawn_ids(req.kind, req.generation, None).expect("valid");
        let err = crate::supervise::registry::validate_spawn_ids(SpawnKind::Probe, Some(7), None)
            .expect_err("a Probe generation is reserved");
        assert_eq!(err.params["rule"], "reserved");
    }

    /// A15.1: a `Vendorcli` probe from a caller other than the sidecar (the
    /// app's own detection) has no generation, and that is valid too.
    #[test]
    fn an_app_vendorcli_probe_has_no_generation() {
        crate::supervise::registry::validate_spawn_ids(SpawnKind::Vendorcli, None, None).expect("valid without");
        let err = crate::supervise::registry::validate_spawn_ids(SpawnKind::Vendorcli, Some(0), None)
            .expect_err("< 1 is out of range");
        assert_eq!(err.params["rule"], "range");
    }

    #[test]
    fn the_backoff_ladder_doubles_to_sixty_seconds() {
        assert_eq!(RESTART_BACKOFF_SECS, [1, 2, 4, 8, 16, 32, 60]);
        assert_eq!(DEATHS_BEFORE_STAYING_DOWN, 3);
        assert_eq!(DEATH_WINDOW, Duration::from_secs(300));
    }

    /// T-buildId-rule, the core-of-the-sidecar half: the same rule as A8.1.
    #[test]
    fn t_build_id_rule() {
        assert_eq!(build_id_from(None, "0.4.0"), "0.4.0");
        assert_eq!(build_id_from(Some(""), "0.4.0"), "0.4.0", "empty = unset");
        assert_eq!(
            build_id_from(Some("2.0.0+b425b5e.20261022T073000"), "0.4.0"),
            "2.0.0+b425b5e.20261022T073000"
        );
        assert!(is_valid_build_id(HOST_BUILD_ID));
        assert!(is_valid_build_id(env!("CARGO_PKG_VERSION")));
        assert!(is_valid_build_id("2.0.0+b425b5e.20261022T073000"));
        assert!(is_valid_build_id(&"a".repeat(64)));
        for bad in ["", " 0.4.0", "0.4.0\n", "0.4.0 ", "a/b", ".hidden", "-flag", "é", &"a".repeat(65)] {
            assert!(!is_valid_build_id(bad), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn a_missing_script_is_a_typed_runtime_error() {
        let err = SidecarRuntime::resolve(Some(PathBuf::from("/nope/sidecar.cjs")), None, &[])
            .expect_err("must fail when no script exists");
        match err {
            GildeError::SidecarRuntimeUnusable { .. } => {}
            other => panic!("expected SidecarRuntimeUnusable, got {other:?}"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn dev_mode_runs_the_source_tree_bundle_with_a_system_node() {
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
        assert_eq!(runtime.script, dev_script());
        assert_eq!(runtime.node, node);
        assert_eq!(runtime.node_source, NodeSource::System);
        assert!(runtime.warnings.iter().any(|w| w.contains("nodeSource: system")));
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

    // -- turn routing ------------------------------------------------------------

    fn text(s: &str) -> TurnEvent {
        TurnEvent::Text { delta: s.to_string() }
    }

    fn done() -> TurnEvent {
        TurnEvent::Done {
            status: wire::TurnDoneStatus::Ok,
            conversation_id: "c".to_string(),
            usage: None,
            cost_usd: None,
        }
    }

    #[test]
    fn a_turn_queue_merges_text_on_overflow_and_never_drops_the_terminal() {
        let mut q = TurnQueue::default();
        let cap = wire::MAX_PENDING_TURN_EVENTS as u64;
        for seq in 1..=cap {
            assert_eq!(q.admit(seq, text("a")), Admit::Queued);
        }
        assert_eq!(q.len(), cap as usize);
        // Overflow: merged into the trailing text, nothing lost.
        assert_eq!(q.admit(cap + 1, text("b")), Admit::Merged);
        assert_eq!(q.len(), cap as usize);
        // A tool at the cap is still queued (never dropped)…
        assert_eq!(
            q.admit(cap + 2, TurnEvent::Tool { name: "Bash".into(), detail_preview: None }),
            Admit::Queued
        );
        // …and a text after it makes room by joining an adjacent pair.
        assert_eq!(q.admit(cap + 3, text("c")), Admit::Queued);
        // The terminal always gets in, and nothing follows it.
        assert_eq!(q.admit(cap + 4, done()), Admit::Queued);
        assert_eq!(q.admit(cap + 5, text("late")), Admit::Rejected);
        let mut all = String::new();
        let mut last_seq = 0;
        let mut terminals = 0;
        while let Some((seq, ev)) = q.pop() {
            assert!(seq > last_seq, "seq stays strictly increasing");
            last_seq = seq;
            match ev {
                TurnEvent::Text { delta } => all.push_str(&delta),
                TurnEvent::Done { .. } | TurnEvent::Error { .. } => terminals += 1,
                _ => {}
            }
        }
        assert_eq!(all, format!("{}b", "a".repeat(cap as usize)) + "c", "every byte of text kept, in order");
        assert_eq!(terminals, 1);
    }

    #[test]
    fn a_turn_queue_rejects_out_of_order_seq_and_coalesces_usage_at_the_cap() {
        let mut q = TurnQueue::default();
        assert_eq!(q.admit(5, text("x")), Admit::Queued);
        assert_eq!(q.admit(5, text("dup")), Admit::Rejected);
        assert_eq!(q.admit(3, text("old")), Admit::Rejected);
        let usage = |n| TurnEvent::Usage {
            usage: TurnUsage { input_tokens: Some(n), output_tokens: None, cached_input_tokens: None },
        };
        let mut seq = 6;
        while q.len() < wire::MAX_PENDING_TURN_EVENTS {
            q.admit(seq, usage(seq));
            seq += 1;
        }
        assert_eq!(q.admit(seq, usage(999)), Admit::Merged, "the latest usage replaces the trailing one");
        assert_eq!(q.rejected, 2);
    }

    // -- errors ------------------------------------------------------------------

    /// The typed error mapping (contract §4.4 / §4.5): every sidecar error code
    /// becomes a documented `IpcError` code.
    #[test]
    fn sidecar_errors_map_to_the_contract_codes() {
        let rpc = |code: i64, data: Value| RpcErrorObject::new(code, "x").with_data(data);
        let cases: Vec<(RpcErrorObject, ErrorCode)> = vec![
            (rpc(code::ADAPTER_ERROR, json!({"class": "auth"})), ErrorCode::VendorError),
            (rpc(code::BUILD_MISMATCH, json!({})), ErrorCode::VersionMismatch),
            (rpc(code::NOT_READY, json!({})), ErrorCode::NotReady),
            (rpc(code::REFUSED, json!({"reason": "notAllowlisted"})), ErrorCode::Refused),
            (rpc(code::UNSUPPORTED, json!({"cap": "vendorNotInstalled"})), ErrorCode::Unsupported),
            (rpc(code::INVALID_PARAMS, json!({"field": "model", "rule": "required"})), ErrorCode::InvalidArgument),
            (rpc(code::INVALID_PARAMS, json!({})), ErrorCode::Internal),
            (rpc(code::METHOD_NOT_FOUND, json!({})), ErrorCode::Unsupported),
            (rpc(code::CHANNEL_CLOSED, json!({})), ErrorCode::SidecarDown),
            (rpc(code::TIMEOUT, json!({})), ErrorCode::Timeout),
            (rpc(-31999, json!({})), ErrorCode::Internal),
        ];
        for (err, want) in cases {
            assert_eq!(ipc_error_of_rpc(&err).code, want, "{err:?}");
        }
        let auth = ipc_error_of_rpc(&rpc(code::ADAPTER_ERROR, json!({"class": "quota"})));
        assert_eq!(auth.params.get("class"), Some(&json!("quota")));
        let refused = ipc_error_of_rpc(&rpc(code::REFUSED, json!({"reason": "notAllowlisted"})));
        assert_eq!(refused.params.get("reason"), Some(&json!("notAllowlisted")));
        let invalid = ipc_error_of_rpc(&rpc(code::INVALID_PARAMS, json!({"field": "model", "rule": "required"})));
        assert_eq!(invalid.params.get("field"), Some(&json!("model")));
    }

    // -- accounts ----------------------------------------------------------------

    /// T-env-defaultAccount (sidecar half): the default claude/codex login is
    /// `envKey: null` (the variable is UNSET, C8); another dir carries its realpath.
    #[test]
    fn legacy_accounts_become_account_envs_with_the_c8_rule() {
        let home = tempfile::tempdir().expect("home");
        let home_path = std::fs::canonicalize(home.path()).expect("canonical");
        std::fs::create_dir_all(home_path.join(".claude")).expect("mkdir");
        std::fs::create_dir_all(home_path.join(".claude-work")).expect("mkdir");
        let accounts = vec![
            Account::new("claude"),
            Account::new("claude").with_config_dir(Some(home_path.join(".claude").display().to_string())),
            Account::new("claude").with_config_dir(Some(home_path.join(".claude-work").display().to_string())),
            Account::new("gemini").with_config_dir(Some("/somewhere".to_string())),
            Account::new("vim"),
        ];
        let envs = account_envs_of(&accounts, &home_path);
        assert_eq!(envs.len(), 3, "the two spellings of the default login are one account: {envs:?}");
        let default = &envs[0];
        assert_eq!(default.env_key, None);
        assert_eq!(default.config_dir, None);
        let work = &envs[1];
        assert_eq!(work.env_key.as_deref(), Some("CLAUDE_CONFIG_DIR"));
        assert_eq!(
            work.config_dir.as_ref().map(AbsPath::as_str),
            Some(home_path.join(".claude-work").to_str().expect("utf8"))
        );
        let agy = &envs[2];
        assert_eq!((agy.vendor, agy.env_key.as_deref(), agy.config_dir.as_ref()), (Vendor::Agy, None, None));
        assert_eq!(agy.account_id, crate::contract::ids::AccountId::derive_default_login(Vendor::Agy));
    }

    // -- team mode, read-only preview ----------------------------------------------

    /// A `cco.team.snapshot` payload exactly as the observer composes it (the
    /// `team` of an `orch.snapshot`), from `sidecar/test/cco-team.mjs`.
    const TEAM_SNAPSHOT_FIXTURE: &str = include_str!("../../tests/fixtures/cco/team-snapshot.json");

    fn envelope(observer_generation: u64) -> Value {
        let inner: Value = serde_json::from_str(TEAM_SNAPSHOT_FIXTURE).expect("fixture is JSON");
        json!({
            "generation": observer_generation,
            "rev": inner["rev"],
            "team": {"teamId": inner["teamId"], "view": inner["view"]},
        })
    }

    #[test]
    fn a_real_sidecar_snapshot_relays_with_rusts_own_fields_stamped() {
        let event = team_snapshot_event(7, envelope(1), |_| None).expect("relayed");
        assert_eq!(event.generation, orch_generation_key(7, 1));
        assert_eq!(event.team_id, "tmabcdefghi1");
        let view = event.view.as_ref().expect("a view");
        assert_eq!(view.generation, orch_generation_key(7, 1));
        assert!(view.live && view.believed, "a live subscription is believed as it is");
        assert_eq!(view.state.members.len(), 2);
        assert!(view.activity.iter().any(|a| a.member == "alice"));
        assert!(view.messages.iter().any(|m| m.id == "m11"));
    }

    #[test]
    fn a_new_observer_or_a_new_sidecar_outranks_every_older_view() {
        assert!(orch_generation_key(1, 2) > orch_generation_key(1, 1));
        assert!(orch_generation_key(2, 1) > orch_generation_key(1, 9));
    }

    #[test]
    fn a_snapshot_that_breaks_the_envelope_or_names_two_teams_is_dropped() {
        let mut two = envelope(1);
        two["team"]["view"]["state"]["id"] = Value::String("tmothertea12".into());
        assert!(team_snapshot_event(1, two, |_| None).is_none());
        let mut extra = envelope(1);
        extra["unknown"] = json!(1);
        assert!(team_snapshot_event(1, extra, |_| None).is_none(), "unknown envelope fields are rejected");
        assert!(team_snapshot_event(1, json!({"generation": 1, "rev": 1, "team": {"teamId": "../x", "view": null}}), |_| None).is_none());
        assert!(team_snapshot_event(1, json!("not an object"), |_| None).is_none());
        let withdrawn = team_snapshot_event(
            1,
            json!({"generation": 1, "rev": 9, "team": {"teamId": "tmabcdefghi1", "view": null}}),
            |_| None,
        )
        .expect("a withdrawal is relayed");
        assert!(withdrawn.view.is_none());
        assert_eq!(withdrawn.rev, 9);
    }

    #[test]
    fn a_disk_view_of_an_active_team_is_believed_only_with_its_owner() {
        let mut params = envelope(1);
        params["team"]["view"]["live"] = Value::Bool(false);
        params["team"]["view"]["source"] = Value::String("disk".into());
        let owner_pid = params["team"]["view"]["state"]["owner"]["hostPid"].as_i64().expect("owner pid");
        let started = params["team"]["view"]["state"]["owner"]["hostStartedAt"].as_i64().expect("started");
        let gone = team_snapshot_event(1, params.clone(), |_| None).expect("relayed");
        assert!(!gone.view.expect("view").believed, "a dead owner renders as host gone");
        let alive = team_snapshot_event(1, params, |pid| (pid == owner_pid).then_some(started + 1_000)).expect("relayed");
        assert!(alive.view.expect("view").believed);
    }

    #[test]
    fn an_impossible_owner_pid_is_never_probed_as_alive() {
        assert_eq!(owner_start_ms(0), None);
        assert_eq!(owner_start_ms(1), None, "launchd/init is never a host");
        assert_eq!(owner_start_ms(i64::from(i32::MAX) + 1), None);
        assert!(owner_start_ms(i64::from(std::process::id())).is_some(), "this process is alive");
    }

    #[test]
    fn the_status_renders_into_any_type_that_decodes_its_name() {
        #[derive(Debug, Default, PartialEq, Deserialize)]
        #[serde(rename_all = "camelCase")]
        enum Other {
            Ready,
            #[default]
            Down,
        }
        let s: SidecarStatus<Other> = SidecarStatus {
            state: serde_json::from_value(json!(SidecarState::Ready.as_str())).unwrap_or_default(),
            generation: 1,
            restarts: 0,
            last_error: None,
            node_path: None,
            script_path: None,
            version_mismatch: false,
            stays_down: false,
        };
        assert_eq!(s.state, Other::Ready);
    }
}
