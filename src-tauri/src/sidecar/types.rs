//! Field-for-field Rust mirrors of the agent-mesh data contract (DESIGN §5-4).
//!
//! The rule for this file, and the reason it is separate from
//! [`crate::sidecar::protocol`]: **these types are not ours.** They belong to
//! `@toragonite/agent-mesh` (`packages/core/src/types.ts`) and the sidecar hands
//! them through verbatim. Three consequences shape every declaration below:
//!
//!   1. `#[serde(rename_all = "camelCase")]` everywhere — the wire is TypeScript.
//!   2. Every optional carries `#[serde(default)]` and
//!      `skip_serializing_if = "Option::is_none"`, so a field a future adapter
//!      version stops sending does not become a deserialization failure, and a
//!      field it never sent does not reappear as `null` on the way back out.
//!   3. Anything an adapter may extend is carried through: `raw: Option<Value>`
//!      for the documented debug payloads, plus a flattened `extra` map on the
//!      pass-through shapes. A round trip (`JSON → Rust → JSON`) is therefore
//!      lossless even for a vendor field this build has never heard of — which
//!      is exactly what `tests/sidecar_faults.rs` asserts against real fixtures.
//!
//! One semantic is load-bearing and easy to lose: **`RunResult.conversation_id`
//! may legitimately be the empty string**, and that means "this run is not
//! resumable". Normalising it to `None` would promise the user a resume that
//! cannot happen, so it stays a `String` and the question is asked through
//! [`RunResult::is_resumable`].

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

/// Unmodelled sibling fields, preserved across a decode/encode round trip.
pub type Extra = BTreeMap<String, Value>;

// ---------------------------------------------------------------------------
// identity
// ---------------------------------------------------------------------------

/// A login for one vendor. `config_dir` is the isolated home
/// (`CLAUDE_CONFIG_DIR` / `CODEX_HOME` / `AGY_HOME`); `None` means "the ambient
/// CLI login on this machine".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub vendor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_dir: Option<String>,
}

impl Account {
    pub fn new(vendor: impl Into<String>) -> Account {
        Account {
            vendor: vendor.into(),
            label: None,
            config_dir: None,
        }
    }

    pub fn with_config_dir(mut self, dir: Option<String>) -> Account {
        self.config_dir = dir;
        self
    }

    pub fn with_label(mut self, label: Option<String>) -> Account {
        self.label = label;
        self
    }
}

// ---------------------------------------------------------------------------
// capabilities
// ---------------------------------------------------------------------------

/// What an adapter can do beyond the mandatory `run`/`resume`/`quota` surface.
///
/// **The host never hardcodes this matrix** (DESIGN §5-3): it is read off the
/// live adapter instance in `mesh.hello` / `mesh.adapters`, so an adapter
/// upgrade that flips `stream` moves the UI with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdapterCapabilities {
    #[serde(default)]
    pub resume: bool,
    /// `"mid-run"` | `"turn"` | `"none"`. Kept as a string because it is an open
    /// vocabulary upstream; a legacy `false` decodes to `"none"`.
    #[serde(default = "steer_none", deserialize_with = "de_steer")]
    pub steer: String,
    #[serde(default)]
    pub peer_mesh: bool,
    #[serde(default)]
    pub quota: bool,
    #[serde(default)]
    pub stream: bool,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Default for AdapterCapabilities {
    fn default() -> Self {
        AdapterCapabilities {
            resume: false,
            steer: steer_none(),
            peer_mesh: false,
            quota: false,
            stream: false,
            extra: Extra::new(),
        }
    }
}

fn steer_none() -> String {
    "none".to_string()
}

/// Accept `"turn"`, `"mid-run"`, `"none"` — and the `false` that `ipc.ts` still
/// allows for "no steering at all".
fn de_steer<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    match Value::deserialize(d)? {
        Value::String(s) => Ok(s),
        Value::Bool(false) => Ok(steer_none()),
        Value::Null => Ok(steer_none()),
        other => Ok(other.to_string()),
    }
}

/// One row of `mesh.hello`/`mesh.adapters`: the adapter plus where its CLI is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdapterInfo {
    pub vendor: String,
    pub capabilities: AdapterCapabilities,
    /// The binary name or path the adapter was configured with.
    #[serde(default)]
    pub binary: String,
    /// The resolved absolute path, or `null` when nothing on `PATH` matched.
    ///
    /// Serialized even when absent: both the sidecar and `ipc.ts::AdapterInfo`
    /// type it as `string | null`, and "the field is missing" would read as a
    /// different fact than "we looked and found nothing".
    #[serde(default)]
    pub binary_path: Option<String>,
    #[serde(default)]
    pub installed: bool,
    #[serde(flatten)]
    pub extra: Extra,
}

// ---------------------------------------------------------------------------
// work
// ---------------------------------------------------------------------------

/// One unit of headless work.
///
/// Two host-imposed rules (DESIGN §5-4) are enforced by the sidecar and honoured
/// by every caller here: `cwd` is always an absolute worktree path, and
/// `timeout_ms` is always set — a run with no wall clock is a scheduled leak.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunTask {
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<Account>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<Extra>,
}

impl RunTask {
    pub fn new(prompt: impl Into<String>, cwd: impl Into<String>, timeout_ms: u64) -> RunTask {
        RunTask {
            prompt: prompt.into(),
            cwd: Some(cwd.into()),
            timeout_ms: Some(timeout_ms),
            ..RunTask::default()
        }
    }

    pub fn with_model(mut self, model: Option<String>) -> RunTask {
        self.model = model;
        self
    }

    pub fn with_account(mut self, account: Option<Account>) -> RunTask {
        self.account = account;
        self
    }

    pub fn with_permission_mode(mut self, mode: Option<String>) -> RunTask {
        self.permission_mode = mode;
        self
    }
}

/// Options for `mesh.resume`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<Account>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<Extra>,
}

/// Terminal state of one adapter run, as agent-mesh defines it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RunStatus {
    /// Completed normally.
    #[default]
    Ok,
    /// The adapter or the agent it drives reported a failure.
    Error,
    /// Externally stopped.
    Stopped,
    /// Ended without a final answer (timeout, truncation, permission wall).
    Incomplete,
}

impl RunStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            RunStatus::Ok => "ok",
            RunStatus::Error => "error",
            RunStatus::Stopped => "stopped",
            RunStatus::Incomplete => "incomplete",
        }
    }
}

/// Token counts, when the vendor reports them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// The result of `mesh.run` / `mesh.resume`, passed through verbatim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunResult {
    pub vendor: String,
    /// **May be `""`.** Empty means the vendor returned no conversation id and
    /// the run is therefore not resumable — see [`RunResult::is_resumable`].
    #[serde(default)]
    pub conversation_id: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub status: RunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Raw final vendor payload. Shape is vendor-specific and unstable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<Value>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl RunResult {
    /// Whether `conversation_id` can be fed back to `mesh.resume`.
    ///
    /// This is the whole reason the field is not an `Option`: the contract's way
    /// of saying "not resumable" is an empty string, and the distinction reaches
    /// the user as "continue the conversation" versus "run it again".
    pub fn is_resumable(&self) -> bool {
        !self.conversation_id.is_empty()
    }
}

/// One incremental event from `adapter.stream()`.
///
/// Internally tagged on `type`, exactly like the TypeScript union. An event kind
/// this build does not know fails to decode; callers that must not lose it keep
/// the raw [`Value`] from [`RunEventEnvelope`] instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum RunEvent {
    Text {
        #[serde(default)]
        text: String,
    },
    Tool {
        #[serde(default)]
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<Value>,
    },
    Usage {
        usage: TokenUsage,
    },
    Done {
        result: RunResult,
    },
    Error {
        #[serde(default)]
        message: String,
    },
}

impl RunEvent {
    /// `done` and `error` are the only terminal kinds; exactly one arrives per
    /// stream (DESIGN §5-8, enforced sidecar-side by `pumpRunEvents`).
    pub fn is_terminal(&self) -> bool {
        matches!(self, RunEvent::Done { .. } | RunEvent::Error { .. })
    }

    pub fn kind(&self) -> &'static str {
        match self {
            RunEvent::Text { .. } => "text",
            RunEvent::Tool { .. } => "tool",
            RunEvent::Usage { .. } => "usage",
            RunEvent::Done { .. } => "done",
            RunEvent::Error { .. } => "error",
        }
    }
}

/// The `mesh.event` notification: a run id plus the adapter's own event.
///
/// The event is kept as a [`Value`] as well as a decoded [`RunEvent`] so that a
/// future event kind is still forwarded to the UI unchanged instead of being
/// dropped by this build's enum.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunEventEnvelope {
    pub run_id: String,
    pub event: Value,
    /// The sidecar generation that produced the event. Stamped by the host, not
    /// on the wire — a run's events must be attributable to a generation so a
    /// crash can settle exactly the runs it interrupted.
    #[serde(default)]
    pub generation: i64,
}

impl RunEventEnvelope {
    /// Decode the event, or `None` when it is a kind this build does not model.
    pub fn decoded(&self) -> Option<RunEvent> {
        serde_json::from_value(self.event.clone()).ok()
    }

    /// The `type` string as it arrived, even for an unmodelled kind.
    pub fn event_type(&self) -> Option<&str> {
        self.event.get("type").and_then(Value::as_str)
    }
}

// ---------------------------------------------------------------------------
// introspection
// ---------------------------------------------------------------------------

/// One rolling quota window ("5h", "weekly", …).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaWindow {
    #[serde(default)]
    pub label: String,
    /// 0..100.
    #[serde(default)]
    pub used_percent: f64,
    /// ISO 8601 instant, when the vendor reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// A vendor's headroom at one instant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaSnapshot {
    pub vendor: String,
    #[serde(default)]
    pub windows: Vec<QuotaWindow>,
    /// Whether the vendor currently permits new work.
    ///
    /// Defaults to `true` when absent: a missing gate means "unknown", and
    /// defaulting to `false` would silently stop dispatching to a healthy
    /// account because an adapter omitted one field.
    #[serde(default = "yes")]
    pub allowed: bool,
    /// ISO 8601 capture time, stamped by the sidecar (core has no clock).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captured_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<Value>,
    #[serde(flatten)]
    pub extra: Extra,
}

fn yes() -> bool {
    true
}

/// One selectable model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// `"fast"` | `"slow"`, open-ended upstream.
    #[serde(default)]
    pub latency_class: String,
    /// Auth modes under which this model is selectable; absent = any login.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_modes: Option<Vec<String>>,
    /// True for the adapter's default pick.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<bool>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Whether a vendor CLI is logged in, and how.
///
/// `detail` is the account label or email when resolvable — the adapter contract
/// states it **must never contain a token**, and nothing here ever logs it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthStatus {
    #[serde(default)]
    pub logged_in: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

// ---------------------------------------------------------------------------
// handshake (DESIGN §5-3)
// ---------------------------------------------------------------------------

/// The four vendor binaries the sidecar should drive. Names or absolute paths;
/// the sidecar resolves them without spawning anything.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BinaryPaths {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor_agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agy: Option<String>,
}

/// Host-side defaults every task inherits when it names none.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeshDefaults {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
}

/// `mesh.hello` parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HelloParams {
    pub protocol: i64,
    pub host_version: String,
    pub app_instance_id: String,
    pub boot_id: String,
    pub binaries: BinaryPaths,
    pub defaults: MeshDefaults,
    #[serde(default)]
    pub accounts: Vec<Account>,
}

/// `mesh.hello` result — the sidecar's identity and its adapter roster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HelloResult {
    pub protocol: i64,
    #[serde(default)]
    pub sidecar_version: String,
    /// The Node version actually running the bundle, e.g. `"v20.19.2"`.
    #[serde(default)]
    pub node: String,
    #[serde(default)]
    pub packages: BTreeMap<String, String>,
    #[serde(default)]
    pub adapters: Vec<AdapterInfo>,
}

impl HelloResult {
    pub fn adapter(&self, vendor: &str) -> Option<&AdapterInfo> {
        self.adapters.iter().find(|a| a.vendor == vendor)
    }

    /// Whether `vendor` streams — read off the adapter, never hardcoded
    /// (DESIGN §5-8). Unknown vendor ⇒ `false`, i.e. the host uses `mesh.run`.
    pub fn can_stream(&self, vendor: &str) -> bool {
        self.adapter(vendor)
            .map(|a| a.capabilities.stream)
            .unwrap_or(false)
    }
}

/// `mesh.configure` parameters — the adapters are rebuilt, never patched.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigureParams {
    pub binaries: BinaryPaths,
    #[serde(default)]
    pub accounts: Vec<Account>,
}

// ---------------------------------------------------------------------------
// supervision seam (DESIGN §5-5) — sidecar → host
// ---------------------------------------------------------------------------

/// `host.registerSpawn`: everything about a child that does not exist yet.
///
/// `env_keys` is **key names only**. An env value must never reach a host log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterSpawnParams {
    /// The sidecar's own id for this spawn. The host answers with its **own**
    /// marker; the two are correlated by this field for the lifetime of the run.
    pub spawn_id: String,
    #[serde(default)]
    pub run_id: String,
    #[serde(default)]
    pub vendor: String,
    /// `"headless"` | `"probe"` — the only kinds the seam produces.
    #[serde(default)]
    pub kind: String,
    pub cmd: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env_keys: Vec<String>,
    #[serde(default)]
    pub planned_at: i64,
}

/// `host.registerSpawn` result. `env_marker` is the value the sidecar stamps as
/// `GILDE_SPAWN_ID` on the child — the host chooses it, the sidecar never invents one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterSpawnResult {
    pub ok: bool,
    pub env_marker: String,
}

/// `host.spawned`: the child exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpawnedParams {
    #[serde(default)]
    pub pid: i32,
    #[serde(default)]
    pub pgid: i32,
}

/// `host.exited`: the child is gone and how.
///
/// `signal` arrives as a Node signal **name** (`"SIGKILL"`), not a number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExitedParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
    #[serde(default)]
    pub timed_out: bool,
}

/// `host.log`: a structured line from the sidecar. Not a protocol frame — it is
/// forwarded to `tracing` under `target = "sidecar"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostLogParams {
    #[serde(default)]
    pub level: String,
    #[serde(default)]
    pub msg: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fields: Option<Value>,
}

/// Whether a `host.exited` report is the signature of a **deliberate close**
/// rather than a crash: no exit code at all, plus the SIGTERM the sidecar's
/// seam sends a child it has finished with (the duplex usage probe after it read
/// its `control_response`; a cancelled turn). Since protocol 2 the seam never
/// sends SIGKILL to a vendor CLI (contract §9.8, C5); a SIGKILL here was sent by
/// someone else. The registry row records the signal either way; this only
/// decides how the event is logged.
pub fn is_deliberate_close(exit_code: Option<i32>, signal: Option<&str>) -> bool {
    exit_code.is_none() && signal == Some("SIGTERM")
}

/// Map a Node signal name to its number, for the registry row.
///
/// Only the signals a child of ours can actually die from are listed; anything
/// else becomes `None` rather than a guess, because the number is written to a
/// column that the Process Monitor renders.
pub fn signal_number(name: &str) -> Option<i32> {
    Some(match name {
        "SIGHUP" => 1,
        "SIGINT" => 2,
        "SIGQUIT" => 3,
        "SIGILL" => 4,
        "SIGABRT" => 6,
        "SIGFPE" => 8,
        "SIGKILL" => 9,
        "SIGBUS" => 10,
        "SIGSEGV" => 11,
        "SIGPIPE" => 13,
        "SIGALRM" => 14,
        "SIGTERM" => 15,
        "SIGSTOP" => 17,
        "SIGXCPU" => 24,
        "SIGXFSZ" => 25,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// team mode, read-only preview (Claude Code Orchestrator team contract v2 §8.2)
// ---------------------------------------------------------------------------
//
// Unlike everything above, these shapes are not agent-mesh's: they are what
// `sidecar/src/methods/cco.ts` composes from a dispatch host's team projection
// (`TeamState`, host-protocol §2.4) plus the messages and activity lines it kept
// from the stream. The sidecar has already checked every value (well-formed
// strings, integers where these read integers, capped lists), so the rules here
// are only the mirror's: camelCase, `#[serde(default)]` on every container so a
// field a newer engine stops sending is not a decode failure, and unknown fields
// IGNORED (the contract's forward-compatibility rule) rather than carried.
//
// Two fields are Rust's own and never come from the sidecar: `generation` (the
// sidecar generation that composed the view, so a restarted sidecar's revs —
// which start again at 1 — are not mistaken for stale ones) and `believed`
// (L7: whether an `active` status and `running` members can be shown as current;
// see [`CcoTeamView::apply_belief`]).

/// `START_SLACK_MS` (host-protocol): two start-time readings within this many
/// milliseconds name the same process.
pub const CCO_START_SLACK_MS: i64 = 5_000;

/// Whether `id` has the shape of a team id (`TEAM_ID = /^tm[0-9a-z]{9,20}$/`).
pub fn is_cco_team_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    (11..=22).contains(&bytes.len())
        && bytes.starts_with(b"tm")
        && bytes[2..]
            .iter()
            .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
}

/// L7, the one rule: a team read without a live subscription is believed
/// `active` (and its members `running`) only when its owner host is alive AND
/// started within [`CCO_START_SLACK_MS`] of `owner.hostStartedAt`. A start time
/// that cannot be read is not a match — "host gone" is the safe display.
///
/// `start_ms_of_live_pid` answers the owner pid's start time in epoch ms when
/// that pid is alive, `None` otherwise.
pub fn cco_believed(
    live: bool,
    status: &str,
    owner: Option<&CcoOwner>,
    start_ms_of_live_pid: impl Fn(i64) -> Option<i64>,
) -> bool {
    if live || status != "active" {
        return true;
    }
    let Some(owner) = owner else {
        return false;
    };
    if owner.host_pid <= 0 {
        return false;
    }
    match start_ms_of_live_pid(owner.host_pid) {
        Some(started) => (started - owner.host_started_at).abs() <= CCO_START_SLACK_MS,
        None => false,
    }
}

/// `{pid, startedAt}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CcoProcessIdentity {
    pub pid: i64,
    pub started_at: i64,
}

/// `TeamState.owner`: the host that holds the lead seat.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CcoOwner {
    pub host_pid: i64,
    pub host_started_at: i64,
    pub session: CcoProcessIdentity,
}

/// `TeamState.lead`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CcoLeadInbox {
    pub unread: i64,
    pub read_seq: i64,
    pub waiter: bool,
    pub last_call_at: Option<i64>,
    pub wakes_today: i64,
}

/// `TeamState.user`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CcoUserInbox {
    pub unread: i64,
    pub read_seq: i64,
}

/// `MemberState.reason` — a `BlockedCode` while blocked, or `budget` /
/// `quota-wait` / `cancelled` (host-protocol `MemberReasonCode`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CcoMemberReason {
    pub code: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<i64>,
}

/// `MemberState.worktree`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CcoWorktree {
    pub path: String,
    pub branch: String,
    pub base_commit: String,
    pub ready: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ahead: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files_changed: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dirty: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merged: Option<bool>,
}

/// `MemberState.turn` — the running turn (believed only per L7).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CcoTurn {
    pub id: String,
    pub n: i64,
    pub started_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_output_at: Option<i64>,
}

/// `MemberState.tokens`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CcoTokens {
    #[serde(rename = "in")]
    pub tokens_in: i64,
    #[serde(rename = "out")]
    pub tokens_out: i64,
}

/// `MemberState.lastTurn` (`head` ≤ 200 chars).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CcoLastTurn {
    pub id: String,
    pub n: i64,
    pub outcome: String,
    pub ended_at: i64,
    pub head: String,
}

/// `MemberState.attach` (a Claude member taken over in a terminal).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CcoAttach {
    pub lease: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<i64>,
    pub client: String,
    pub since: i64,
}

/// `MemberState.lastActivity`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CcoLastActivity {
    pub ts: i64,
    pub text: String,
}

/// One member of the projection (`MemberState`, host-protocol §2.4).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CcoMemberState {
    pub name: String,
    pub index: i64,
    pub role: String,
    /// The ENGINE's `MemberStateName` — never a terminal-screen guess.
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<CcoMemberReason>,
    pub provider: String,
    pub worker: String,
    pub account_label: String,
    pub machine_login: bool,
    pub shares_lead_plan: bool,
    pub billable_overage: bool,
    pub runs_commands: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elevated: Option<String>,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_used: Option<String>,
    pub incarnation: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversation_short: Option<String>,
    /// `tools` | `turns-only` | `unverified`.
    pub mailbox: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mailbox_reason: Option<String>,
    pub port_base: i64,
    pub worktree: CcoWorktree,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn: Option<CcoTurn>,
    pub turns: i64,
    pub tokens: CcoTokens,
    pub cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_input_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_turn: Option<CcoLastTurn>,
    pub pending: i64,
    pub tasks: Vec<String>,
    pub peer_wakes: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rss_mb: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attach: Option<CcoAttach>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_activity: Option<CcoLastActivity>,
}

/// One report of a board task.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CcoBoardReport {
    pub at: i64,
    pub by: String,
    pub file: String,
}

/// One board task (`BoardTask`, §5.1). `body` is at most a short head.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CcoBoardTask {
    pub id: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_file: Option<String>,
    /// `open` | `claimed` | `blocked` | `review` | `done` | `cancelled`.
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    pub paths: Vec<String>,
    pub allow_overlap: bool,
    pub created_by: String,
    pub created_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub reports: Vec<CcoBoardReport>,
    pub commits: Vec<String>,
    pub turns: Vec<String>,
    pub updated_at: i64,
    pub updated_by: String,
}

/// `TeamState.budgets`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CcoBudgets {
    pub turns_today: i64,
    pub turns_per_day: i64,
    pub running: i64,
    pub max_running: i64,
    pub ping_pong_tripped: bool,
}

/// `TeamState.totals`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CcoTotals {
    pub turns: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_usd: Option<f64>,
}

/// One `TeamState.warnings` entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CcoWarning {
    pub kind: String,
    pub text: String,
}

/// The serde mirror of `TeamState` (host-protocol §2.4) — display-safe values
/// only (no config dirs, no prompt text, no credentials).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CcoTeamState {
    pub v: i64,
    pub id: String,
    pub name: String,
    pub title: String,
    /// `active` | `suspended` | `ended` | `readonly` | `damaged`.
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub damaged_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    pub workspace: String,
    pub repo_root: String,
    pub lead_rel: String,
    pub base_commit: String,
    pub created_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<i64>,
    pub seq: i64,
    /// `null` when no host holds the seat.
    pub owner: Option<CcoOwner>,
    pub lead: CcoLeadInbox,
    pub user: CcoUserInbox,
    pub members: Vec<CcoMemberState>,
    pub board: Vec<CcoBoardTask>,
    pub budgets: CcoBudgets,
    pub totals: CcoTotals,
    pub warnings: Vec<CcoWarning>,
}

/// One message of the timeline (a `msg.posted` the sidecar saw).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CcoTeamMessage {
    pub seq: i64,
    /// `m<seq>`.
    pub id: String,
    pub ts: i64,
    pub from: String,
    pub to: String,
    pub kind: String,
    pub urgent: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub re: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// The body is longer than what is kept, or could not be read.
    pub cut: bool,
    pub bytes: i64,
}

/// One activity line (a feed item flattened to text).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CcoActivityLine {
    pub ts: i64,
    pub turn: String,
    /// `turn` | `text` | `tool` | `usage` | `error` | `done` | `notice`.
    pub k: String,
    pub text: String,
}

/// One member's kept activity, oldest first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CcoMemberActivity {
    pub member: String,
    pub lines: Vec<CcoActivityLine>,
}

/// What the sidecar composes for one team: a whole snapshot, latest wins.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CcoTeamView {
    pub team_id: String,
    /// The sidecar's composition counter (monotonic within one generation).
    pub rev: u64,
    /// `host` (a live subscription) or `disk` (`state.json`).
    pub source: String,
    pub live: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_key: Option<String>,
    /// The Gilde PTY pane whose Claude Code session leads this team.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_session_pid: Option<i64>,
    pub state: CcoTeamState,
    pub messages: Vec<CcoTeamMessage>,
    pub activity: Vec<CcoMemberActivity>,
    pub truncated: bool,
    pub composed_at: i64,
    /// Rust's: the sidecar generation that composed it.
    pub generation: i64,
    /// Rust's: L7 — see [`cco_believed`].
    pub believed: bool,
}

impl CcoTeamView {
    /// The view's identity is one team id, said the same way in both places.
    pub fn is_consistent(&self) -> bool {
        is_cco_team_id(&self.team_id) && self.state.id == self.team_id
    }

    /// Stamp the two fields that are Rust's own.
    pub fn apply_belief(
        &mut self,
        generation: i64,
        start_ms_of_live_pid: impl Fn(i64) -> Option<i64>,
    ) {
        self.generation = generation;
        self.believed = cco_believed(
            self.live,
            &self.state.status,
            self.state.owner.as_ref(),
            start_ms_of_live_pid,
        );
    }
}

/// `cco.team.snapshot` params: `view: null` = the team left the sidecar's sight.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CcoTeamSnapshotParams {
    pub rev: u64,
    pub team_id: String,
    pub view: Option<CcoTeamView>,
}

/// `cco.team.list` params: what Gilde has open. Sent at every list, so the
/// scope follows the projects as they come and go.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CcoTeamListParams {
    /// Absolute folders: project roots, live worktrees, PTY session cwds.
    pub roots: Vec<String>,
    /// Gilde PTY session pids (a host whose session is one of them is "the team of that pane").
    pub pane_pids: Vec<i64>,
    /// Watch the dispatch hosts and teams of EVERY workspace of this user, not
    /// only those related to `roots` (Gilde preview: Team works with no setup).
    /// Read-only either way; it widens discovery, not what is done with a host.
    pub all_workspaces: bool,
}

/// One dispatch host the sidecar found.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CcoHostInfo {
    pub key: String,
    pub session_pid: i64,
    /// The lead session's start time (`run/s<pid>.json` `session.startedAt`),
    /// unix millis — tells a live lead from a recycled pid.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_started_at: Option<i64>,
    pub host_pid: i64,
    pub workspace: String,
    pub pane: bool,
    /// `idle` | `connecting` | `connected` | `no-teams` | `failed`.
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub team_id: Option<String>,
}

/// One team in `cco.team.list`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CcoTeamSummary {
    pub team_id: String,
    pub name: String,
    pub title: String,
    pub status: String,
    pub source: String,
    pub live: bool,
    pub members: i64,
    pub running: i64,
    pub blocked: i64,
    pub user_unread: i64,
    pub lead_unread: i64,
    pub created_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_session_pid: Option<i64>,
    pub owner: Option<CcoOwner>,
    pub rev: u64,
    /// Why a connected dispatch host's `cco/team.list` lists the team as it does
    /// (CCO `TeamListEntry.reason`: `host gone`, or a damaged team's reason). One
    /// line, ≤ 300 chars, host text — the sidecar flattens and caps it. Absent
    /// when no host gave one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Rust's: L7.
    pub believed: bool,
}

/// `cco.team.list` result, as `cco_team_list` returns it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CcoTeamListResult {
    /// False when the sidecar is down (Rust's own field; the preview says so).
    pub available: bool,
    /// False on platforms without team mode.
    pub supported: bool,
    pub data_root: String,
    /// `on` | `off` | `unknown` (registry.json `teamMode`).
    pub team_mode: String,
    pub hosts: Vec<CcoHostInfo>,
    pub teams: Vec<CcoTeamSummary>,
    /// Rust's: the sidecar generation that answered.
    pub generation: i64,
}

impl CcoTeamListResult {
    /// Stamp `available`, `generation` and every summary's `believed`.
    pub fn apply_belief(
        &mut self,
        generation: i64,
        start_ms_of_live_pid: impl Fn(i64) -> Option<i64>,
    ) {
        self.available = true;
        self.generation = generation;
        for team in &mut self.teams {
            team.believed = cco_believed(
                team.live,
                &team.status,
                team.owner.as_ref(),
                &start_ms_of_live_pid,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sigterm_with_no_exit_code_reads_as_a_deliberate_close_not_a_crash() {
        // What the duplex usage probe leaves behind since protocol 2 (C5: never SIGKILL).
        assert!(is_deliberate_close(None, Some("SIGTERM")));
        // A SIGKILL is never ours any more; a real crash carries a code, or a
        // different signal, or both.
        assert!(!is_deliberate_close(None, Some("SIGKILL")));
        assert!(!is_deliberate_close(Some(1), Some("SIGTERM")));
        assert!(!is_deliberate_close(Some(0), None));
        assert!(!is_deliberate_close(None, Some("SIGSEGV")));
        assert!(!is_deliberate_close(None, None));
        // The row still records the signal that actually landed.
        assert_eq!(signal_number("SIGKILL"), Some(9));
    }

    #[test]
    fn an_empty_conversation_id_survives_and_means_not_resumable() {
        let json = r#"{"vendor":"codex","conversationId":"","text":"hi","status":"ok"}"#;
        let result: RunResult = serde_json::from_str(json).expect("decode");
        assert_eq!(result.conversation_id, "");
        assert!(!result.is_resumable());
        // and it is still an empty string on the way back out — not dropped.
        let back = serde_json::to_value(&result).expect("encode");
        assert_eq!(back["conversationId"], serde_json::json!(""));
    }

    #[test]
    fn unmodelled_vendor_fields_survive_a_round_trip() {
        let json = r#"{"vendor":"claude","conversationId":"c1","text":"t","status":"ok",
                       "somethingNew":{"a":1}}"#;
        let value: Value = serde_json::from_str(json).expect("json");
        let result: RunResult = serde_json::from_value(value.clone()).expect("decode");
        assert_eq!(
            serde_json::to_value(&result).expect("encode")["somethingNew"],
            serde_json::json!({"a": 1})
        );
    }

    #[test]
    fn steer_accepts_both_the_string_and_the_legacy_false() {
        let caps: AdapterCapabilities =
            serde_json::from_str(r#"{"resume":true,"steer":"mid-run","quota":true,"stream":true}"#)
                .expect("decode");
        assert_eq!(caps.steer, "mid-run");
        let legacy: AdapterCapabilities =
            serde_json::from_str(r#"{"steer":false}"#).expect("decode");
        assert_eq!(legacy.steer, "none");
    }

    #[test]
    fn a_missing_quota_gate_reads_as_allowed_not_as_blocked() {
        let snap: QuotaSnapshot =
            serde_json::from_str(r#"{"vendor":"claude","windows":[]}"#).expect("decode");
        assert!(snap.allowed);
    }

    #[test]
    fn run_events_decode_by_their_type_tag() {
        let done: RunEvent = serde_json::from_str(
            r#"{"type":"done","result":{"vendor":"claude","conversationId":"c","text":"x","status":"ok"}}"#,
        )
        .expect("decode");
        assert!(done.is_terminal());
        assert_eq!(done.kind(), "done");

        let text: RunEvent = serde_json::from_str(r#"{"type":"text","text":"hello"}"#).expect("decode");
        assert!(!text.is_terminal());
    }

    #[test]
    fn an_unknown_event_kind_is_carried_but_not_decoded() {
        let env = RunEventEnvelope {
            run_id: "r1".into(),
            event: serde_json::json!({"type": "thinking", "text": "…"}),
            generation: 3,
        };
        assert!(env.decoded().is_none());
        assert_eq!(env.event_type(), Some("thinking"));
    }

    #[test]
    fn node_signal_names_map_to_numbers() {
        assert_eq!(signal_number("SIGKILL"), Some(9));
        assert_eq!(signal_number("SIGTERM"), Some(15));
        assert_eq!(signal_number("NOT_A_SIGNAL"), None);
    }
    // -- team mode, read-only preview ----------------------------------------

    /// What `sidecar/src/methods/cco.ts` composes, shaped as its test fixture sends it.
    const CCO_VIEW_JSON: &str = r#"{
      "teamId":"tmabcdefghi1","rev":7,"source":"host","live":true,"hostKey":"s424242",
      "paneSessionPid":424242,"truncated":false,"composedAt":1700000000000,
      "state":{"v":1,"id":"tmabcdefghi1","name":"demo","title":"Demo","status":"active",
        "goal":"Ship it","workspace":"/w","repoRoot":"/w","leadRel":"","baseCommit":"aaaa",
        "createdAt":1700000000000,"seq":42,
        "owner":{"hostPid":4321,"hostStartedAt":1699999990000,"session":{"pid":424242,"startedAt":1699999980000}},
        "lead":{"unread":2,"readSeq":10,"waiter":true,"lastCallAt":null,"wakesToday":3},
        "user":{"unread":1,"readSeq":9},
        "members":[{"name":"alice","index":1,"role":"parser","state":"blocked",
          "reason":{"code":"quota","text":"quota wall","until":1700000600000},
          "provider":"codex","worker":"w1","accountLabel":"w1","machineLogin":false,
          "sharesLeadPlan":false,"billableOverage":false,"runsCommands":true,"model":"gpt-5.5",
          "modelUsed":"gpt-5.5","incarnation":2,"mailbox":"turns-only","mailboxReason":"agy approvals",
          "portBase":41000,"worktree":{"path":"/wt","branch":"cco/demo/alice","baseCommit":"aaaa","ready":true,"ahead":3,"dirty":true},
          "turn":{"id":"tabc1234","n":4,"startedAt":1700000000000,"pid":999},
          "turns":4,"tokens":{"in":1200,"out":300},"costUsd":null,"pending":1,"tasks":["b1"],
          "peerWakes":0,"lastActivity":{"ts":1700000000001,"text":"Edit · src/a.ts"},
          "somethingNewer":{"x":1}}],
        "board":[{"id":"b1","title":"Write the parser","state":"claimed","assignee":"alice",
          "paths":["src/parser.ts"],"allowOverlap":false,"createdBy":"lead","createdAt":1,
          "reports":[{"at":2,"by":"alice","file":"bodies/b1-r1.md"}],"commits":[],"turns":["tabc1234"],
          "updatedAt":3,"updatedBy":"alice"}],
        "budgets":{"turnsToday":3,"turnsPerDay":300,"running":1,"maxRunning":4,"pingPongTripped":false},
        "totals":{"turns":4,"inputTokens":1200,"outputTokens":300,"costUsd":0.25},
        "warnings":[{"kind":"blocked","text":"alice is blocked"}]},
      "messages":[{"seq":11,"id":"m11","ts":1700000000002,"from":"alice","to":"lead","kind":"report",
        "urgent":false,"task":"b1","body":"done","cut":false,"bytes":4}],
      "activity":[{"member":"alice","lines":[{"ts":1,"turn":"tabc1234","k":"tool","text":"Edit · src/a.ts"}]}]
    }"#;

    #[test]
    fn a_cco_team_view_round_trips() {
        let view: CcoTeamView = serde_json::from_str(CCO_VIEW_JSON).expect("decode");
        assert!(view.is_consistent());
        assert_eq!(view.rev, 7);
        assert_eq!(view.state.members[0].tokens.tokens_in, 1200);
        assert_eq!(
            view.state.members[0]
                .reason
                .as_ref()
                .map(|r| r.code.as_str()),
            Some("quota")
        );
        assert_eq!(view.state.members[0].cost_usd, None);
        assert_eq!(view.state.totals.cost_usd, Some(0.25));
        assert_eq!(view.state.owner.as_ref().map(|o| o.host_pid), Some(4321));
        assert_eq!(view.messages[0].task.as_deref(), Some("b1"));

        // Rust → JSON keeps the wire's names (camelCase, `in`/`out`), and a second
        // decode is the same value.
        let json = serde_json::to_value(&view).expect("encode");
        assert_eq!(
            json["state"]["members"][0]["tokens"]["in"],
            serde_json::json!(1200)
        );
        assert_eq!(
            json["state"]["members"][0]["accountLabel"],
            serde_json::json!("w1")
        );
        assert_eq!(
            json["state"]["lead"]["lastCallAt"],
            Value::Null,
            "nullable keys stay null"
        );
        assert_eq!(json["state"]["members"][0]["costUsd"], Value::Null);
        assert_eq!(json["paneSessionPid"], serde_json::json!(424242));
        let back: CcoTeamView = serde_json::from_value(json).expect("decode again");
        assert_eq!(back, view);
    }

    #[test]
    fn unknown_cco_fields_are_ignored_and_missing_ones_default() {
        let view: CcoTeamView = serde_json::from_str(CCO_VIEW_JSON).expect("decode");
        let json = serde_json::to_value(&view).expect("encode");
        assert!(
            json["state"]["members"][0].get("somethingNewer").is_none(),
            "a field this build does not know is dropped, not carried"
        );
        let sparse: CcoTeamView = serde_json::from_str(
            r#"{"teamId":"tmabcdefghi1","rev":1,"state":{"v":1,"id":"tmabcdefghi1","members":[{"name":"bob"}]}}"#,
        )
        .expect("a sparse view still decodes");
        assert_eq!(sparse.state.members[0].name, "bob");
        assert_eq!(sparse.state.members[0].tokens, CcoTokens::default());
        assert!(sparse.state.owner.is_none());
        assert!(sparse.messages.is_empty());
    }

    #[test]
    fn a_null_view_is_a_withdrawal() {
        let p: CcoTeamSnapshotParams =
            serde_json::from_str(r#"{"rev":9,"teamId":"tmabcdefghi1","view":null}"#)
                .expect("decode");
        assert_eq!(p.rev, 9);
        assert!(p.view.is_none());
        let p: CcoTeamSnapshotParams = serde_json::from_str(&format!(
            r#"{{"rev":10,"teamId":"tmabcdefghi1","view":{CCO_VIEW_JSON}}}"#
        ))
        .expect("decode");
        assert_eq!(p.view.map(|v| v.rev), Some(7));
    }

    #[test]
    fn cco_list_results_round_trip() {
        let json = r#"{"supported":true,"dataRoot":"/h/.claude-code-orchestrator","teamMode":"on",
          "hosts":[{"key":"s1","sessionPid":1,"hostPid":2,"workspace":"/w","pane":false,"state":"connected","teamId":"tmabcdefghi1"}],
          "teams":[{"teamId":"tmabcdefghi1","name":"demo","title":"","status":"active","source":"host","live":true,
            "members":2,"running":1,"blocked":0,"userUnread":0,"leadUnread":0,"createdAt":1,"owner":null,"rev":3}]}"#;
        let mut result: CcoTeamListResult = serde_json::from_str(json).expect("decode");
        result.apply_belief(4, |_| None);
        assert!(result.available);
        assert_eq!(result.generation, 4);
        assert!(result.teams[0].believed, "a live team is believed");
        let back: CcoTeamListResult =
            serde_json::from_value(serde_json::to_value(&result).expect("encode"))
                .expect("decode again");
        assert_eq!(back, result);
        assert_eq!(result.teams[0].reason, None, "no reason given, none kept");
        assert!(
            !serde_json::to_string(&result).expect("encode").contains("\"reason\""),
            "an absent reason is not sent as null"
        );
    }

    #[test]
    fn a_cco_team_list_reason_reaches_the_frontend() {
        let json = r#"{"supported":true,"dataRoot":"/d","teamMode":"on","hosts":[],
          "teams":[{"teamId":"tmzzzzzzzzz2","name":"old","title":"","status":"suspended","source":"disk","live":false,
            "members":1,"running":0,"blocked":0,"userUnread":0,"leadUnread":0,"createdAt":1,"owner":null,"rev":2,
            "reason":"host gone"}]}"#;
        let mut result: CcoTeamListResult = serde_json::from_str(json).expect("decode");
        result.apply_belief(1, |_| None);
        assert_eq!(result.teams[0].reason.as_deref(), Some("host gone"));
        let out = serde_json::to_value(&result).expect("encode");
        assert_eq!(out["teams"][0]["reason"], "host gone", "relayed as given");
    }

    #[test]
    fn cco_belief_follows_l7() {
        let owner = CcoOwner {
            host_pid: 4321,
            host_started_at: 1_700_000_000_000,
            session: CcoProcessIdentity::default(),
        };
        let alive_at = |ms: i64| move |pid: i64| if pid == 4321 { Some(ms) } else { None };
        // A live subscription, or a status other than `active`, is believed as it is.
        assert!(cco_believed(true, "active", None, |_| None));
        assert!(cco_believed(false, "suspended", None, |_| None));
        // `active` from disk: only with a live owner that started when it says.
        assert!(!cco_believed(false, "active", None, |_| None));
        assert!(cco_believed(
            false,
            "active",
            Some(&owner),
            alive_at(1_700_000_003_000)
        ));
        assert!(
            !cco_believed(false, "active", Some(&owner), alive_at(1_700_000_009_000)),
            "pid reuse"
        );
        assert!(
            !cco_believed(false, "active", Some(&owner), |_| None),
            "dead or unreadable"
        );
        let nobody = CcoOwner {
            host_pid: 0,
            ..owner.clone()
        };
        assert!(!cco_believed(false, "active", Some(&nobody), |_| Some(0)));

        let mut view: CcoTeamView = serde_json::from_str(CCO_VIEW_JSON).expect("decode");
        view.live = false;
        view.apply_belief(3, |_| None);
        assert_eq!(view.generation, 3);
        assert!(!view.believed);
    }

    #[test]
    fn cco_team_ids_have_the_engine_shape() {
        assert!(is_cco_team_id("tmabcdefghi1"));
        assert!(is_cco_team_id("tm0123456789abcdefghij"));
        assert!(!is_cco_team_id("tmABCDEFGHI1"), "upper case");
        assert!(!is_cco_team_id("tm123"), "too short");
        assert!(!is_cco_team_id("../../etc/x"));
        assert!(!is_cco_team_id("tm0123456789abcdefghijk"), "too long");
    }

    #[test]
    fn team_list_params_carry_all_workspaces_on_the_wire() {
        let params = CcoTeamListParams {
            roots: vec!["/r".into()],
            pane_pids: vec![42],
            all_workspaces: true,
        };
        let value = serde_json::to_value(&params).expect("serialize");
        assert_eq!(value, serde_json::json!({ "roots": ["/r"], "panePids": [42], "allWorkspaces": true }));
        let old: CcoTeamListParams =
            serde_json::from_str(r#"{"roots":[],"panePids":[]}"#).expect("parse");
        assert!(!old.all_workspaces, "absent means the folder-scoped default");
    }

}
