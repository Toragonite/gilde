//! NDJSON JSON-RPC 2.0 framing for the sidecar channel (DESIGN §5-1).
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

/// Wire protocol revision this build speaks. A mismatch is a hard error: the
/// host kills the sidecar rather than guessing (DESIGN §5-3).
pub const PROTOCOL_VERSION: i64 = 1;

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
}

// ---------------------------------------------------------------------------
// method names
// ---------------------------------------------------------------------------

/// host → sidecar (DESIGN §5-4).
pub mod method {
    pub const HELLO: &str = "mesh.hello";
    pub const CONFIGURE: &str = "mesh.configure";
    pub const PING: &str = "mesh.ping";
    pub const ADAPTERS: &str = "mesh.adapters";
    pub const AUTH_STATUS: &str = "mesh.authStatus";
    pub const AVAILABLE_MODELS: &str = "mesh.availableModels";
    pub const QUOTA: &str = "mesh.quota";
    pub const RUN: &str = "mesh.run";
    pub const STREAM: &str = "mesh.stream";
    pub const RESUME: &str = "mesh.resume";
    pub const CANCEL: &str = "mesh.cancel";
    pub const ROUTE: &str = "fleet.route";
    pub const SHUTDOWN: &str = "mesh.shutdown";
    /// Team mode, read-only (`sidecar/src/methods/cco.ts`): `{roots, panePids,
    /// allWorkspaces}` → `CcoTeamListResult`. Also hands the sidecar what to watch.
    pub const TEAM_LIST: &str = "cco.team.list";
    /// `{teamId}` → `CcoTeamView | null`.
    pub const TEAM_GET: &str = "cco.team.get";
    /// Worker accounts, read-only (`sidecar/src/methods/cco-workers.ts`):
    /// `{refresh, roots, panePids, allWorkspaces}` → `CcoWorkersResult` (the engine's
    /// `list_workers` snapshot, `cco.workers/1`, plus its poll-floor state).
    pub const WORKERS: &str = "cco.workers";
}

/// sidecar → host (DESIGN §5-5).
pub mod host_method {
    pub const REGISTER_SPAWN: &str = "host.registerSpawn";
    pub const SPAWNED: &str = "host.spawned";
    pub const EXITED: &str = "host.exited";
    pub const LOG: &str = "host.log";
    /// A notification, and lossy by contract.
    pub const EVENT: &str = "mesh.event";
    /// A notification: one team's whole view, latest wins, lossy at `mesh.event`'s
    /// rank (team contract v2 §8.2 — a snapshot, never an event).
    pub const TEAM_SNAPSHOT: &str = "cco.team.snapshot";
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
