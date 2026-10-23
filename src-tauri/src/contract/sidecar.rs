//! Sidecar JSON-RPC types (contract §4.5): NDJSON JSON-RPC 2.0 over stdio,
//! **protocol 2**.
//!
//! Framing, ids, `host.*` callbacks and generations are unchanged; line caps
//! stay (core inbound ≤ 4 MiB, sidecar inbound ≤ 8 MiB); a longer line is a
//! protocol violation → generation+1 restart. Host ids are strings, sidecar ids
//! numbers. **Both ends reject unknown fields** (`deny_unknown_fields`). The
//! sidecar imports the generated TS of these types from
//! `src-tauri/sidecar/src/gen/`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::contract::account::{Quota, SignIn};
use crate::contract::common::{AbsPath, PermissionMode, Vendor};
use crate::contract::error::VendorErrorClass;
use crate::contract::ids::{AccountId, SessionUid};

/// The protocol number of this contract.
pub const PROTOCOL: u32 = 2;
/// The sidecar exits with this code on a `buildId` mismatch.
pub const EXIT_BUILD_MISMATCH: i32 = 78;
/// Host-side cap on undelivered `turn.event`s per turn (merge `Text` on overflow;
/// `Done`/`Error` never dropped).
pub const MAX_PENDING_TURN_EVENTS: usize = 1_024;

/// Method and notification names.
pub mod methods {
    pub const MESH_HELLO: &str = "mesh.hello";
    pub const MESH_CONFIGURE: &str = "mesh.configure";
    pub const MESH_PING: &str = "mesh.ping";
    pub const MESH_SHUTDOWN: &str = "mesh.shutdown";
    pub const PROBE_AUTH_STATUS: &str = "probe.authStatus";
    pub const PROBE_MODELS: &str = "probe.models";
    pub const PROBE_QUOTA: &str = "probe.quota";
    pub const TURN_START: &str = "turn.start";
    pub const TURN_CANCEL: &str = "turn.cancel";
    /// sidecar → host notification.
    pub const TURN_EVENT: &str = "turn.event";
    /// Orchestrator observer (registered only while the orchestrator is present).
    pub const ORCH_TEAM_LIST: &str = "orch.team.list";
    pub const ORCH_TEAM_GET: &str = "orch.team.get";
    pub const ORCH_TEAM_SUBSCRIBE: &str = "orch.team.subscribe";
    pub const ORCH_WORKERS: &str = "orch.workers";
    /// sidecar → host notification, `(generation, rev)` latest-wins.
    pub const ORCH_SNAPSHOT: &str = "orch.snapshot";
    /// sidecar → host supervision seam.
    pub const HOST_REGISTER_SPAWN: &str = "host.registerSpawn";
    pub const HOST_SPAWNED: &str = "host.spawned";
    pub const HOST_EXITED: &str = "host.exited";
    pub const HOST_LOG: &str = "host.log";
}

// -- mesh.hello ----------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct MeshHelloParams {
    /// Always 2.
    pub protocol: u32,
    pub build_id: String,
    pub app_instance_id: String,
    pub providers: Providers,
}

/// Optional providers. Orchestrator methods are registered only when
/// `orchestrator` (fixes `index.ts:484`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct Providers {
    pub orchestrator: bool,
}

/// `buildId` mismatch → the sidecar exits with code 78; the host reports
/// `VERSION_MISMATCH`; a protocol-1 peer is stopped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct MeshHelloResult {
    pub protocol: u32,
    pub build_id: String,
    pub adapters: Vec<AdapterInfo>,
}

/// One agent-mesh adapter as the sidecar sees it (capabilities read off the
/// live adapter, never hardcoded).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct AdapterInfo {
    pub vendor: Vendor,
    pub resume: bool,
    pub stream: bool,
    pub quota: bool,
    pub installed: bool,
}

// -- mesh.configure --------------------------------------------------------------

/// Re-sent when presence or accounts change. Binaries are the resolved absolute
/// paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct MeshConfigureParams {
    pub binaries: BTreeMap<Vendor, AbsPath>,
    pub accounts: Vec<AccountEnv>,
    pub providers: Providers,
}

/// Carries only the env **key** name and the config dir (never a value from
/// the user's environment, never a secret).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct AccountEnv {
    pub account_id: AccountId,
    pub vendor: Vendor,
    /// `CLAUDE_CONFIG_DIR` / `CODEX_HOME`; `None` for the default login (unset, C8).
    pub env_key: Option<String>,
    pub config_dir: Option<AbsPath>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct MeshConfigureResult {}

// -- probe.* ------------------------------------------------------------------------

/// `probe.authStatus` / `probe.models` / `probe.quota` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct ProbeParams {
    pub vendor: Vendor,
    pub account_id: AccountId,
}

/// `probe.authStatus` result; never returns a token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct AuthStatus {
    pub sign_in: SignIn,
}

/// `probe.models` result element.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct ModelInfo {
    pub id: String,
    pub label: Option<String>,
    pub is_default: bool,
}

/// `probe.quota` result: `Quota | null`.
pub type ProbeQuotaResult = Option<Quota>;

// -- turn.* ---------------------------------------------------------------------------

/// `normalizeTask` MUST forward every field (`resumeId`, `permissionMode`,
/// `model`, `account`, `cwd`, `timeoutMs`); codex resume needs a model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct TurnStartParams {
    pub turn_id: String,
    pub session_uid: SessionUid,
    pub vendor: Vendor,
    pub account_id: AccountId,
    pub cwd: AbsPath,
    pub model: Option<String>,
    pub permission_mode: Option<PermissionMode>,
    pub prompt: String,
    pub resume_id: Option<String>,
    #[ts(type = "number | null")]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct TurnStartResult {
    /// Always `true`.
    pub accepted: bool,
}

/// killpg SIGTERM of the registered child (hard stop); the core's fenced kill
/// is authoritative.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct TurnCancelParams {
    pub turn_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct TurnCancelResult {
    pub signalled: bool,
}

/// `turn.event` notification: exactly one `Done` or `Error` ends a turn; text
/// coalesced 50 ms / 16 KiB; `seq` strictly increasing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct TurnEventParams {
    pub turn_id: String,
    #[ts(type = "number")]
    pub seq: u64,
    pub event: TurnEvent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum TurnEvent {
    Text {
        delta: String,
    },
    Tool {
        name: String,
        detail_preview: Option<String>,
    },
    Usage {
        usage: TurnUsage,
    },
    Session {
        conversation_id: String,
    },
    Done {
        status: TurnDoneStatus,
        /// `""` = no resumable id.
        conversation_id: String,
        usage: Option<TurnUsage>,
        cost_usd: Option<f64>,
    },
    Error {
        class: VendorErrorClass,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum TurnDoneStatus {
    Ok,
    Incomplete,
    Stopped,
}

/// Token counts when the vendor reports them; absent = unknown (never 0).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct TurnUsage {
    #[ts(type = "number | null")]
    pub input_tokens: Option<u64>,
    #[ts(type = "number | null")]
    pub output_tokens: Option<u64>,
    #[ts(type = "number | null")]
    pub cached_input_tokens: Option<u64>,
}

// -- host.* (sidecar → host supervision seam; existing, env values never cross) ----

/// `host.registerSpawn` (a REQUEST, existing shape with `runId` → `turnId`) is
/// also an **allowlist**: `cmd` (argv[0]) must equal a configured vendor binary
/// or the bundled node; otherwise `REFUSED` (§9.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct RegisterSpawnParams {
    /// The sidecar's own id for this spawn; correlates `host.spawned`/`host.exited`.
    pub spawn_id: String,
    pub turn_id: Option<String>,
    pub vendor: Option<Vendor>,
    pub kind: SidecarSpawnKind,
    pub cmd: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    /// Env **key names only**.
    pub env_keys: Vec<String>,
}

/// The only registry kinds the sidecar seam produces (§3.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum SidecarSpawnKind {
    Turn,
    Probe,
}

/// `env_marker` is the value the sidecar stamps as `GILDE_SPAWN_ID` on the
/// child — the host chooses it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct RegisterSpawnResult {
    pub ok: bool,
    pub env_marker: String,
}

/// `host.spawned` (a REQUEST): the child exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct SpawnedParams {
    pub spawn_id: String,
    pub pid: i32,
    pub pgid: i32,
}

/// `host.exited` (a REQUEST). `signal` arrives as a Node signal **name**
/// (`"SIGTERM"`), not a number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct ExitedParams {
    pub spawn_id: String,
    pub exit_code: Option<i32>,
    pub signal: Option<String>,
    pub timed_out: bool,
}

/// A structured line from the sidecar, forwarded to `tracing` (allowlisted
/// fields; never content).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct HostLogParams {
    pub level: HostLogLevel,
    pub msg: String,
    #[ts(type = "Record<string, string | number | boolean>")]
    pub fields: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum HostLogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

// -- orch.* (existing observer payloads; schema owned by the vendored observer) ----

/// `orch.snapshot` notification envelope: `(generation, rev)` latest-wins. The
/// team payload is the existing `cco.team.snapshot` shape of the vendored
/// orchestrator observer (§4.5 "existing"), carried as JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct OrchSnapshotParams {
    #[ts(type = "number")]
    pub generation: u64,
    #[ts(type = "number")]
    pub rev: u64,
    pub team: serde_json::Value,
}
