//! NDJSON JSON-RPC 2.0 framing for the sidecar channel, protocol 2 (contract
//! §4.5; DESIGN §5-1).
//!
//! One JSON object per line, UTF-8, `\n` terminated, over the sidecar's
//! stdin/stdout. **stderr is not part of the protocol** — it carries plain log
//! lines that the manager forwards to `tracing` under `target = "sidecar"`.
//!
//! The channel is bidirectional out of necessity rather than taste: the exec
//! seam must *await* `host.registerSpawn` before it spawns (§5-5), so the
//! sidecar issues requests of its own and the host answers them.
//!
//! Three tolerance rules live here, and each one exists because the alternative
//! was observed to be worse:
//!
//!   * **An unparseable line is skipped, not fatal.** A stray `console.log` from
//!     somewhere inside a dependency must cost a warning, never a session.
//!   * **A line without `"jsonrpc":"2.0"` is skipped** for the same reason.
//!   * **A line longer than [`MAX_LINE_BYTES`] is a protocol violation.** The
//!     buffer is dropped, the rest of the line is discarded, and the manager
//!     restarts the sidecar at generation + 1. The cap is what keeps a runaway
//!     sidecar from OOM-ing the host.
//!
//! Id types are deliberately asymmetric, mirroring the sidecar implementation:
//! the host sends **string** ids and the sidecar echoes them back verbatim,
//! while the sidecar's own requests carry **numeric** ids that the host must
//! echo unchanged. Nothing here normalises an id; responses reuse the exact
//! [`Value`] that arrived.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Wire protocol revision this build speaks (contract §4.5: protocol 2). A
/// mismatch is a hard error: the host stops the sidecar rather than guessing.
pub const PROTOCOL_VERSION: i64 = crate::contract::sidecar::PROTOCOL as i64;

/// Maximum accepted inbound line, host side (M1-PLAN T19 acceptance 5).
///
/// Stricter than the sidecar's own 8 MiB outbound framing limit (DESIGN §5-1):
/// nothing the sidecar legitimately sends comes close to 4 MiB — `text` events
/// are merged at 16 KiB and a `RunResult` carries one final message — so a line
/// past this cap is already evidence of a malfunctioning peer, and the host
/// bounds its own memory before the peer's own limit would.
pub const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;

/// JSON-RPC error codes used across this channel.
pub mod code {
    /// Standard JSON-RPC.
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
    /// Application range, agreed with the sidecar (`src/rpc.ts`).
    pub const ADAPTER_ERROR: i64 = -32001;
    pub const PROTOCOL_MISMATCH: i64 = -32002;
    pub const TIMEOUT: i64 = -32003;
    /// A capability the adapter (or this build) does not have.
    pub const UNSUPPORTED: i64 = -32004;
    /// The channel died under an in-flight request (host-originated).
    pub const CHANNEL_CLOSED: i64 = -32000;
    /// `mesh.hello` from another build: the sidecar answers this, then exits 78.
    pub const BUILD_MISMATCH: i64 = -32005;
    /// A method that needs `mesh.hello` / `mesh.configure` first.
    pub const NOT_READY: i64 = -32006;
    /// The host refused a `host.*` request by policy (the `registerSpawn` allowlist).
    pub const REFUSED: i64 = -32010;
}

// ---------------------------------------------------------------------------
// method names
// ---------------------------------------------------------------------------

/// host → sidecar (contract §4.5). The protocol-2 names come from the contract;
/// the rest are the protocol-1 names, kept only so the dropped dispatch lane
/// still compiles until its owner deletes it (a protocol-2 sidecar answers them
/// `-32601`).
pub mod method {
    use crate::contract::sidecar::methods as m;

    pub const HELLO: &str = m::MESH_HELLO;
    pub const CONFIGURE: &str = m::MESH_CONFIGURE;
    pub const PING: &str = m::MESH_PING;
    pub const SHUTDOWN: &str = m::MESH_SHUTDOWN;
    pub const PROBE_AUTH_STATUS: &str = m::PROBE_AUTH_STATUS;
    pub const PROBE_MODELS: &str = m::PROBE_MODELS;
    pub const PROBE_QUOTA: &str = m::PROBE_QUOTA;
    pub const TURN_START: &str = m::TURN_START;
    pub const TURN_CANCEL: &str = m::TURN_CANCEL;
    /// Orchestrator observer (registered only while the orchestrator is present).
    pub const ORCH_TEAM_LIST: &str = m::ORCH_TEAM_LIST;
    pub const ORCH_TEAM_GET: &str = m::ORCH_TEAM_GET;
    pub const ORCH_TEAM_SUBSCRIBE: &str = m::ORCH_TEAM_SUBSCRIBE;
    pub const ORCH_WORKERS: &str = m::ORCH_WORKERS;

    // -- protocol 1 (legacy; not on a protocol-2 channel) ---------------------
    pub const ADAPTERS: &str = "mesh.adapters";
    pub const AUTH_STATUS: &str = "mesh.authStatus";
    pub const AVAILABLE_MODELS: &str = "mesh.availableModels";
    pub const QUOTA: &str = "mesh.quota";
    pub const RUN: &str = "mesh.run";
    pub const STREAM: &str = "mesh.stream";
    pub const RESUME: &str = "mesh.resume";
    pub const CANCEL: &str = "mesh.cancel";
    pub const ROUTE: &str = "fleet.route";
    /// Team mode, read-only (`sidecar/src/methods/cco.ts`): `{roots, panePids,
    /// allWorkspaces}` → `CcoTeamListResult`. Renamed `orch.team.list` (§4.5).
    pub const TEAM_LIST: &str = m::ORCH_TEAM_LIST;
    /// `{teamId}` → `CcoTeamView | null`. Renamed `orch.team.get`.
    pub const TEAM_GET: &str = m::ORCH_TEAM_GET;
    /// Worker accounts, read-only (`cco-workers.ts`). Renamed `orch.workers`.
    pub const WORKERS: &str = m::ORCH_WORKERS;
}

/// sidecar → host (contract §4.5).
pub mod host_method {
    use crate::contract::sidecar::methods as m;

    pub const REGISTER_SPAWN: &str = m::HOST_REGISTER_SPAWN;
    pub const SPAWNED: &str = m::HOST_SPAWNED;
    pub const EXITED: &str = m::HOST_EXITED;
    pub const LOG: &str = m::HOST_LOG;
    /// A notification: one turn's events, `seq` strictly increasing, never dropped.
    pub const TURN_EVENT: &str = m::TURN_EVENT;
    /// A notification: one orchestrator team's whole view, `(generation, rev)`
    /// latest wins, lossy (a snapshot, never an event).
    pub const TEAM_SNAPSHOT: &str = m::ORCH_SNAPSHOT;
    /// Protocol 1's run event (legacy; a protocol-2 sidecar never sends it).
    pub const EVENT: &str = "mesh.event";
}

/// Whether an inbound method is one the host handles.
///
/// `host.` is the supervision seam; the `$` prefix is reserved for future
/// transport-level requests and is routed the same way so that adding one does
/// not need a reader change (M1-PLAN T19 acceptance 5).
pub fn is_host_method(method: &str) -> bool {
    method.starts_with("host.") || method.starts_with('$') || method == host_method::EVENT
}

// ---------------------------------------------------------------------------
// error objects
// ---------------------------------------------------------------------------

/// The `error` member of a JSON-RPC response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcErrorObject {
    pub code: i64,
    #[serde(default)]
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcErrorObject {
    pub fn new(code: i64, message: impl Into<String>) -> RpcErrorObject {
        RpcErrorObject {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(mut self, data: Value) -> RpcErrorObject {
        self.data = Some(data);
        self
    }

    /// The AgentMesh error name the sidecar puts in `data.name`
    /// (`AuthError`, `QuotaExceededError`, `AdapterExecError`, …), when present.
    /// This is what lets the host branch without matching on prose.
    pub fn name(&self) -> Option<&str> {
        self.data
            .as_ref()
            .and_then(|d| d.get("name"))
            .and_then(Value::as_str)
    }

    /// `true` for `-32004`: the adapter or the M1 build does not support it.
    pub fn is_unsupported(&self) -> bool {
        self.code == code::UNSUPPORTED
    }
}

impl std::fmt::Display for RpcErrorObject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.name() {
            Some(name) => write!(f, "{} ({}): {}", name, self.code, self.message),
            None => write!(f, "rpc error {}: {}", self.code, self.message),
        }
    }
}

// ---------------------------------------------------------------------------
// inbound
// ---------------------------------------------------------------------------

/// One decoded inbound line.
#[derive(Debug, Clone, PartialEq)]
pub enum Inbound {
    /// A response to one of our requests.
    Response { id: Value, outcome: Outcome },
    /// A request from the sidecar, to be answered with the same `id`.
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// A notification: no id, no answer.
    Notification { method: String, params: Value },
}

/// The two ways a response can land.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Ok(Value),
    Err(RpcErrorObject),
}

/// Why a line was skipped. Every variant is a `warn`, never a fatal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineIssue {
    /// Not JSON at all — most likely a log line that escaped stdout hardening.
    NotJson,
    /// JSON, but not a JSON-RPC 2.0 object.
    NotJsonRpc,
    /// A JSON-RPC object that is neither request, notification nor response.
    Unclassifiable,
}

impl std::fmt::Display for LineIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            LineIssue::NotJson => "line is not JSON",
            LineIssue::NotJsonRpc => "line is not a jsonrpc 2.0 object",
            LineIssue::Unclassifiable => "line is neither a request, a notification nor a response",
        })
    }
}

/// Decode one line.
///
/// Never allocates beyond the parsed value and never panics: a malformed line
/// yields a [`LineIssue`] the reader logs and moves past.
pub fn parse_line(line: &str) -> Result<Inbound, LineIssue> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Err(LineIssue::NotJson);
    }
    let value: Value = serde_json::from_str(trimmed).map_err(|_| LineIssue::NotJson)?;
    let obj = match value {
        Value::Object(map) => map,
        _ => return Err(LineIssue::NotJsonRpc),
    };
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(LineIssue::NotJsonRpc);
    }

    let id = obj.get("id").cloned().filter(|v| !v.is_null());
    let method = obj.get("method").and_then(Value::as_str).map(str::to_owned);
    let params = obj.get("params").cloned().unwrap_or(Value::Null);

    match (method, id) {
        (Some(method), Some(id)) => Ok(Inbound::Request { id, method, params }),
        (Some(method), None) => Ok(Inbound::Notification { method, params }),
        (None, Some(id)) => {
            let outcome = match obj.get("error") {
                Some(Value::Object(_)) => {
                    let err: RpcErrorObject = serde_json::from_value(obj["error"].clone())
                        .unwrap_or_else(|_| {
                            RpcErrorObject::new(code::INTERNAL_ERROR, "unreadable error object")
                        });
                    Outcome::Err(err)
                }
                _ => Outcome::Ok(obj.get("result").cloned().unwrap_or(Value::Null)),
            };
            Ok(Inbound::Response { id, outcome })
        }
        (None, None) => Err(LineIssue::Unclassifiable),
    }
}

// ---------------------------------------------------------------------------
// outbound
// ---------------------------------------------------------------------------

/// `{"jsonrpc":"2.0","id":…,"method":…,"params":…}` as a single NDJSON line.
pub fn request_line(id: &str, method: &str, params: &Value) -> String {
    line(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
}

/// A notification: no id, no answer expected.
pub fn notification_line(method: &str, params: &Value) -> String {
    line(json!({"jsonrpc": "2.0", "method": method, "params": params}))
}

/// A successful response. `id` is echoed **verbatim**, preserving its JSON type.
pub fn result_line(id: &Value, result: &Value) -> String {
    line(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

/// An error response, `id` echoed verbatim.
pub fn error_line(id: &Value, err: &RpcErrorObject) -> String {
    line(json!({"jsonrpc": "2.0", "id": id, "error": err}))
}

fn line(value: Value) -> String {
    // `serde_json` never emits a raw newline inside a string, so one object per
    // line is safe framing (DESIGN §5-1).
    let mut s = value.to_string();
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_response_keeps_the_id_type_it_arrived_with() {
        let msg = parse_line(r#"{"jsonrpc":"2.0","id":"7","result":{"ok":true}}"#).expect("parse");
        match msg {
            Inbound::Response { id, outcome } => {
                assert_eq!(id, json!("7"));
                assert_eq!(outcome, Outcome::Ok(json!({"ok": true})));
            }
            other => panic!("expected a response, got {other:?}"),
        }
    }

    #[test]
    fn an_inbound_request_carries_its_numeric_id_unchanged() {
        let msg = parse_line(
            r#"{"jsonrpc":"2.0","id":3,"method":"host.registerSpawn","params":{"spawnId":"s"}}"#,
        )
        .expect("parse");
        match msg {
            Inbound::Request { id, method, params } => {
                assert_eq!(id, json!(3));
                assert_eq!(method, "host.registerSpawn");
                assert_eq!(params["spawnId"], json!("s"));
                assert!(is_host_method(&method));
            }
            other => panic!("expected a request, got {other:?}"),
        }
    }

    #[test]
    fn notifications_have_no_id() {
        let msg = parse_line(r#"{"jsonrpc":"2.0","method":"mesh.event","params":{"runId":"r"}}"#)
            .expect("parse");
        assert!(matches!(msg, Inbound::Notification { .. }));
    }

    #[test]
    fn garbage_is_classified_rather_than_fatal() {
        assert_eq!(parse_line("this is not json"), Err(LineIssue::NotJson));
        assert_eq!(parse_line(r#"{"hello":"world"}"#), Err(LineIssue::NotJsonRpc));
        assert_eq!(parse_line(r#"{"jsonrpc":"2.0"}"#), Err(LineIssue::Unclassifiable));
        assert_eq!(parse_line("   "), Err(LineIssue::NotJson));
    }

    #[test]
    fn an_error_response_keeps_the_agent_mesh_error_name() {
        let msg = parse_line(
            r#"{"jsonrpc":"2.0","id":"1","error":{"code":-32001,"message":"boom","data":{"name":"AuthError"}}}"#,
        )
        .expect("parse");
        match msg {
            Inbound::Response {
                outcome: Outcome::Err(err),
                ..
            } => {
                assert_eq!(err.code, code::ADAPTER_ERROR);
                assert_eq!(err.name(), Some("AuthError"));
            }
            other => panic!("expected an error response, got {other:?}"),
        }
    }

    #[test]
    fn every_outbound_line_is_exactly_one_line() {
        let params = json!({"text": "a\nb", "nested": {"x": 1}});
        for l in [
            request_line("1", "mesh.ping", &params),
            notification_line("mesh.event", &params),
            result_line(&json!(4), &params),
            error_line(&json!(4), &RpcErrorObject::new(code::INVALID_PARAMS, "no")),
        ] {
            assert_eq!(l.matches('\n').count(), 1, "one newline, at the end: {l}");
            assert!(l.ends_with('\n'));
            assert!(parse_line(&l).is_ok(), "and it parses back: {l}");
        }
    }
}
