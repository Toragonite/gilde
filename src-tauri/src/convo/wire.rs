//! Wire types with explicit renames (contract §13.1, §6.1.3–§6.1.8).
//!
//! Owner: T05.
//!
//! The Claude stream-json wire mixes cases — `request_id` lives **inside**
//! `response`, the `can_use_tool` request is snake_case, the decision payload
//! is camelCase, `system/init.permissionMode` is camelCase [R4 §1.2, §1.6,
//! §4.4] — so every outgoing frame is a typed struct with an explicit
//! `#[serde(rename)]` per field, and the golden-frame tests (T-convo-golden)
//! pin the exact JSON.
//!
//! This module is pure: line framing ([`LineSplitter`], 64 MiB cap, skip to
//! the next `\n` without buffering), the JSON depth check (≤ 64 for frames),
//! the outgoing frame builders and the R4 schema check of every decision
//! payload ([`validate_permission_result`]) — a malformed decision fails
//! closed to deny on the CLI side [R4 §1.6], so nothing leaves unchecked.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use serde_json::Value;

/// Stdout lines up to this size are parsed; longer ones are skipped to the
/// next `\n` and become `System{RowTooLarge}` (§6.1.8).
pub const LINE_CAP: usize = 64 * 1024 * 1024;
/// Frames nested deeper than this are not parsed (§6.1.8, §13.1).
pub const MAX_DEPTH: usize = 64;
/// One stdin frame (user frames, Gilde's control requests) (§6.1.3).
pub const FRAME_CAP: usize = 1024 * 1024;
/// A decision that echoes the original tool input as `updatedInput` may be as
/// large as the request it answers, which was itself ≤ [`LINE_CAP`]; capping
/// it at [`FRAME_CAP`] would make a large `Write` impossible to approve.
pub const ANSWER_FRAME_CAP: usize = LINE_CAP;
/// The stdin queue: at most this many frames…
pub const QUEUE_FRAMES: usize = 64;
/// …and this many bytes (§6.1.3). A single frame larger than this is still
/// admitted into an empty queue.
pub const QUEUE_BYTES: usize = 8 * 1024 * 1024;
/// The core-supplied deny message (§4.2.3, R4 rule 4).
pub const DENY_MESSAGE: &str = "The user denied this action.";
/// The error reply to unsupported CLI-originated requests (§6.1.4).
pub const UNSUPPORTED_ERROR: &str = "unsupported";
/// `tool_use_id` is validated only as printable ASCII ≤ 128 (§6.1.4): a
/// sandbox ask carries a UUID, not `toolu_…` [R6 §3 LP-07b].
pub const MAX_TOOL_USE_ID: usize = 128;

// ---------------------------------------------------------------------------
// framing
// ---------------------------------------------------------------------------

/// What one chunk of stdout produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineEvent {
    /// A complete line (without its `\n`), at most the cap.
    Line(Vec<u8>),
    /// A line longer than the cap ended; its bytes were dropped.
    TooLarge,
}

/// Splits a byte stream into lines without ever buffering more than `cap`
/// bytes of one line: a longer line is dropped while it streams by.
#[derive(Debug)]
pub struct LineSplitter {
    cap: usize,
    buf: Vec<u8>,
    skipping: bool,
}

impl LineSplitter {
    pub fn new(cap: usize) -> LineSplitter {
        LineSplitter { cap: cap.max(1), buf: Vec::new(), skipping: false }
    }

    /// Feed bytes; returns the lines they completed.
    pub fn push(&mut self, mut bytes: &[u8]) -> Vec<LineEvent> {
        let mut out = Vec::new();
        while !bytes.is_empty() {
            match memchr::memchr(b'\n', bytes) {
                Some(nl) => {
                    let (head, rest) = bytes.split_at(nl);
                    bytes = &rest[1..];
                    if self.skipping || self.buf.len() + head.len() > self.cap {
                        self.buf = Vec::new();
                        self.skipping = false;
                        out.push(LineEvent::TooLarge);
                    } else {
                        self.buf.extend_from_slice(head);
                        out.push(LineEvent::Line(std::mem::take(&mut self.buf)));
                    }
                }
                None => {
                    if !self.skipping {
                        if self.buf.len() + bytes.len() > self.cap {
                            self.buf = Vec::new();
                            self.skipping = true;
                        } else {
                            self.buf.extend_from_slice(bytes);
                        }
                    }
                    bytes = &[];
                }
            }
        }
        out
    }

    /// End of stream: an unterminated last line (if any).
    pub fn finish(&mut self) -> Option<LineEvent> {
        if self.skipping {
            self.skipping = false;
            return Some(LineEvent::TooLarge);
        }
        if self.buf.is_empty() {
            None
        } else {
            Some(LineEvent::Line(std::mem::take(&mut self.buf)))
        }
    }
}

/// Whether `bytes` (one JSON text) nests objects/arrays at most `max` deep.
/// A byte scan that respects strings and escapes, run before serde so a
/// hostile frame never reaches the parser's own (128) recursion limit.
pub fn depth_within(bytes: &[u8], max: usize) -> bool {
    let mut depth = 0usize;
    let mut in_str = false;
    let mut esc = false;
    for &b in bytes {
        if in_str {
            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' | b'[' => {
                depth += 1;
                if depth > max {
                    return false;
                }
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    true
}

/// Printable ASCII without spaces, 1..=`max` bytes (ids that may reach logs and keys).
pub fn printable_ascii(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && s.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

// ---------------------------------------------------------------------------
// incoming: the envelope and the typed frames (borrowing the line)
// ---------------------------------------------------------------------------

/// `type` and `subtype` of any stdout frame.
#[derive(Debug, Deserialize)]
pub struct Head<'a> {
    #[serde(rename = "type", borrow, default)]
    pub ty: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    pub subtype: Option<Cow<'a, str>>,
}

/// `{"type":"control_request","request_id":…,"request":{…}}` (CLI → host).
/// `request_id` stays raw: a non-string id is a protocol deviation (§6.1.9).
#[derive(Debug, Deserialize)]
pub struct ControlRequestIn<'a> {
    #[serde(borrow, default)]
    pub request_id: Option<&'a RawValue>,
    #[serde(borrow, default)]
    pub request: Option<&'a RawValue>,
}

impl ControlRequestIn<'_> {
    /// The id when it is a JSON string.
    pub fn id_string(&self) -> Option<String> {
        self.request_id.and_then(|r| serde_json::from_str::<String>(r.get()).ok())
    }

    /// The request's `subtype`, when present.
    pub fn subtype(&self) -> Option<String> {
        let r = self.request?;
        serde_json::from_str::<SubtypeOnly<'_>>(r.get()).ok()?.subtype.map(Cow::into_owned)
    }
}

#[derive(Debug, Deserialize)]
pub struct SubtypeOnly<'a> {
    #[serde(borrow, default)]
    pub subtype: Option<Cow<'a, str>>,
}

/// `{"type":"control_response","response":{…}}` (either direction).
#[derive(Debug, Deserialize)]
pub struct ControlResponseIn<'a> {
    #[serde(borrow, default)]
    pub response: Option<&'a RawValue>,
}

/// The inner object of a `control_response`: `request_id` is **here**, not on
/// the envelope [R4 §1.2].
#[derive(Debug, Deserialize)]
pub struct ResponseInner<'a> {
    #[serde(borrow, default)]
    pub subtype: Option<Cow<'a, str>>,
    #[serde(default)]
    pub request_id: Option<Value>,
    #[serde(borrow, default)]
    pub response: Option<&'a RawValue>,
    #[serde(borrow, default)]
    pub error: Option<&'a RawValue>,
    #[serde(borrow, default)]
    pub pending_permission_requests: Option<&'a RawValue>,
}

impl ResponseInner<'_> {
    pub fn id_string(&self) -> Option<String> {
        self.request_id.as_ref().and_then(Value::as_str).map(str::to_owned)
    }

    pub fn is_success(&self) -> bool {
        self.subtype.as_deref() == Some("success")
    }
}

/// `{"type":"control_cancel_request","request_id":…}`.
#[derive(Debug, Deserialize)]
pub struct CancelIn {
    #[serde(default)]
    pub request_id: Option<Value>,
}

/// `{"type":"command_lifecycle","command_uuid":…,"state":…}` [R6 §3 LP-01].
#[derive(Debug, Deserialize)]
pub struct LifecycleIn {
    #[serde(default)]
    pub command_uuid: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
}

/// `system/init` (camelCase `permissionMode`) [R4 §4.4].
#[derive(Debug, Deserialize)]
pub struct InitIn {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(rename = "permissionMode", default)]
    pub permission_mode: Option<String>,
    #[serde(default)]
    pub capabilities: Option<Vec<Value>>,
    #[serde(default)]
    pub claude_code_version: Option<String>,
}

/// `system/session_state_changed`.
#[derive(Debug, Deserialize)]
pub struct SessionStateIn {
    #[serde(default)]
    pub state: Option<String>,
}

/// `system/status`.
#[derive(Debug, Deserialize)]
pub struct StatusIn {
    #[serde(rename = "permissionMode", default)]
    pub permission_mode: Option<String>,
}

/// `system/task_*` and `system/background_tasks_changed` (only the fields
/// Gilde reads; `prompt` and `summary` are never materialized).
#[derive(Debug, Deserialize)]
pub struct TaskIn {
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub tool_use_id: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub patch: Option<TaskPatchIn>,
}

#[derive(Debug, Deserialize)]
pub struct TaskPatchIn {
    #[serde(default)]
    pub status: Option<String>,
}

/// `system/api_retry`.
#[derive(Debug, Deserialize)]
pub struct ApiRetryIn {
    #[serde(default)]
    pub attempt: Option<u64>,
    #[serde(default)]
    pub error_status: Option<i64>,
}

/// `system/permission_denied`.
#[derive(Debug, Deserialize)]
pub struct PermissionDeniedIn {
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_use_id: Option<String>,
}

/// `result` (only the fields Gilde reads).
#[derive(Debug, Deserialize)]
pub struct ResultIn<'a> {
    #[serde(borrow, default)]
    pub subtype: Option<Cow<'a, str>>,
    #[serde(default)]
    pub is_error: Option<bool>,
    #[serde(default)]
    pub terminal_reason: Option<String>,
    #[serde(default)]
    pub api_error_status: Option<i64>,
    #[serde(borrow, default)]
    pub result: Option<&'a RawValue>,
    #[serde(default)]
    pub total_cost_usd: Option<f64>,
    #[serde(default)]
    pub user_message_uuids: Option<Vec<Value>>,
    #[serde(default)]
    pub permission_denials: Option<Vec<DenialIn>>,
}

#[derive(Debug, Deserialize)]
pub struct DenialIn {
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_use_id: Option<String>,
}

/// `assistant` / `user` frames: the message stays raw until the renderer
/// reads it (tool results are previewed lazily, §6.1.8).
#[derive(Debug, Deserialize)]
pub struct MessageFrameIn<'a> {
    #[serde(borrow, default)]
    pub message: Option<&'a RawValue>,
    #[serde(default)]
    pub parent_tool_use_id: Option<Value>,
    #[serde(default)]
    pub uuid: Option<String>,
    #[serde(rename = "isReplay", default)]
    pub is_replay: Option<bool>,
    #[serde(borrow, default)]
    pub tool_use_result: Option<&'a RawValue>,
    #[serde(default)]
    pub error: Option<Value>,
}

impl MessageFrameIn<'_> {
    pub fn parent(&self) -> Option<&str> {
        self.parent_tool_use_id.as_ref().and_then(Value::as_str)
    }
}

/// `stream_event`.
#[derive(Debug, Deserialize)]
pub struct StreamEventIn<'a> {
    #[serde(borrow, default)]
    pub event: Option<&'a RawValue>,
    #[serde(default)]
    pub parent_tool_use_id: Option<Value>,
}

/// `rate_limit_event{rate_limit_info{unifiedWindows{five_hour, seven_day}}}`.
#[derive(Debug, Deserialize)]
pub struct RateLimitIn {
    #[serde(default)]
    pub rate_limit_info: Option<RateLimitInfoIn>,
}

#[derive(Debug, Deserialize)]
pub struct RateLimitInfoIn {
    #[serde(rename = "unifiedWindows", default)]
    pub unified_windows: Option<UnifiedWindowsIn>,
}

#[derive(Debug, Deserialize)]
pub struct UnifiedWindowsIn {
    #[serde(default)]
    pub five_hour: Option<WindowIn>,
    #[serde(default)]
    pub seven_day: Option<WindowIn>,
}

#[derive(Debug, Deserialize)]
pub struct WindowIn {
    #[serde(default)]
    pub utilization: Option<f64>,
    #[serde(rename = "resetsAt", default)]
    pub resets_at: Option<f64>,
}

// ---------------------------------------------------------------------------
// outgoing frames (host → CLI)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct TextItemOut<'a> {
    #[serde(rename = "type")]
    ty: &'static str,
    text: &'a str,
}

#[derive(Debug, Serialize)]
struct UserMessageOut<'a> {
    role: &'static str,
    content: [TextItemOut<'a>; 1],
}

#[derive(Debug, Serialize)]
struct OriginOut {
    kind: &'static str,
}

#[derive(Debug, Serialize)]
struct UserFrameOut<'a> {
    #[serde(rename = "type")]
    ty: &'static str,
    message: UserMessageOut<'a>,
    parent_tool_use_id: Option<&'a str>,
    uuid: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    origin: Option<OriginOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    priority: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'static str>,
}

/// How a user frame is shaped (the P1 fallback switches `origin` and
/// `session_id`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserFrameShape {
    /// `"priority":"now"` (send now, §6.1.3).
    pub now: bool,
    /// `"origin":{"kind":"human"}` — frozen [R6 §3 LP-01b]; omitted by the P1 fallback.
    pub origin: bool,
    /// `"session_id":""` — the P1 fallback only.
    pub empty_session_id: bool,
}

impl Default for UserFrameShape {
    fn default() -> UserFrameShape {
        UserFrameShape { now: false, origin: true, empty_session_id: false }
    }
}

/// `{"type":"user","message":{"role":"user","content":[{"type":"text","text":…}]},"parent_tool_use_id":null,"uuid":…,"origin":{"kind":"human"}}`
/// (+ `"priority":"now"`) [R4 §4.1–4.2, §6.1.3].
pub fn user_frame(text: &str, uuid: &str, shape: UserFrameShape) -> String {
    let frame = UserFrameOut {
        ty: "user",
        message: UserMessageOut { role: "user", content: [TextItemOut { ty: "text", text }] },
        parent_tool_use_id: None,
        uuid,
        origin: shape.origin.then_some(OriginOut { kind: "human" }),
        priority: shape.now.then_some("now"),
        session_id: shape.empty_session_id.then_some(""),
    };
    serde_json::to_string(&frame).expect("a user frame always serializes")
}

/// The body of a Gilde-originated `control_request` (snake_case subtypes).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
pub enum ControlBody {
    /// No `hooks`, no `perTaskStopAffordance` (§6.1.2 step 1).
    Initialize {
        #[serde(rename = "supportedDialogKinds")]
        supported_dialog_kinds: Vec<String>,
    },
    Interrupt {
        #[serde(skip_serializing_if = "Option::is_none")]
        cancel_queued: Option<bool>,
    },
    SetModel {
        model: Option<String>,
    },
    SetPermissionMode {
        mode: String,
    },
    CancelAsyncMessage {
        message_uuid: String,
    },
    GetUsage {},
}

impl ControlBody {
    /// The subtype as sent (logs and the Gilde id table).
    pub fn subtype(&self) -> &'static str {
        match self {
            ControlBody::Initialize { .. } => "initialize",
            ControlBody::Interrupt { .. } => "interrupt",
            ControlBody::SetModel { .. } => "set_model",
            ControlBody::SetPermissionMode { .. } => "set_permission_mode",
            ControlBody::CancelAsyncMessage { .. } => "cancel_async_message",
            ControlBody::GetUsage {} => "get_usage",
        }
    }
}

#[derive(Debug, Serialize)]
struct ControlRequestOut<'a> {
    #[serde(rename = "type")]
    ty: &'static str,
    request_id: &'a str,
    request: &'a ControlBody,
}

/// `{"type":"control_request","request_id":<uuid>,"request":{"subtype":…}}`.
pub fn control_request(request_id: &str, body: &ControlBody) -> String {
    serde_json::to_string(&ControlRequestOut { ty: "control_request", request_id, request: body })
        .expect("a control request always serializes")
}

#[derive(Debug, Serialize)]
#[serde(tag = "subtype", rename_all = "lowercase")]
enum ResponseInnerOut<'a> {
    Success { request_id: &'a str, response: &'a RawValue },
    Error { request_id: &'a str, error: &'a str },
}

#[derive(Debug, Serialize)]
struct ControlResponseOut<'a> {
    #[serde(rename = "type")]
    ty: &'static str,
    response: ResponseInnerOut<'a>,
}

/// `{"type":"control_response","response":{"subtype":"success","request_id":<CLI id>,"response":<payload>}}`.
pub fn control_response_success(request_id: &str, payload: &RawValue) -> String {
    serde_json::to_string(&ControlResponseOut {
        ty: "control_response",
        response: ResponseInnerOut::Success { request_id, response: payload },
    })
    .expect("a control response always serializes")
}

/// `{"type":"control_response","response":{"subtype":"error","request_id":…,"error":…}}`.
/// Never used for a `can_use_tool` (§6.1.4).
pub fn control_response_error(request_id: &str, error: &str) -> String {
    serde_json::to_string(&ControlResponseOut {
        ty: "control_response",
        response: ResponseInnerOut::Error { request_id, error },
    })
    .expect("a control response always serializes")
}

// ---------------------------------------------------------------------------
// permission updates and decision payloads
// ---------------------------------------------------------------------------

/// One rule of an `addRules` update (camelCase keys) [R4 §1.6].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRule {
    #[serde(rename = "toolName")]
    pub tool_name: String,
    #[serde(rename = "ruleContent", default, skip_serializing_if = "Option::is_none")]
    pub rule_content: Option<String>,
}

/// The only update Gilde ever forwards: `addRules` with `destination:"session"`
/// (§6.1.4 AllowSession, F-01).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionRulesOut {
    #[serde(rename = "type")]
    pub ty: &'static str,
    pub rules: Vec<PermissionRule>,
    pub behavior: &'static str,
    pub destination: &'static str,
}

impl SessionRulesOut {
    pub fn allow(rules: Vec<PermissionRule>) -> SessionRulesOut {
        SessionRulesOut { ty: "addRules", rules, behavior: "allow", destination: "session" }
    }
}

/// `decisionClassification` values [R4 §1.6].
pub const CLASS_TEMPORARY: &str = "user_temporary";
pub const CLASS_PERMANENT: &str = "user_permanent";
pub const CLASS_REJECT: &str = "user_reject";

/// The `response.response` payload of an answer to `can_use_tool` (camelCase).
#[derive(Debug, Serialize)]
#[serde(tag = "behavior", rename_all = "lowercase")]
pub enum PermissionOut<'a> {
    Allow {
        #[serde(rename = "updatedInput")]
        updated_input: &'a RawValue,
        #[serde(rename = "updatedPermissions", skip_serializing_if = "Option::is_none")]
        updated_permissions: Option<&'a [SessionRulesOut]>,
        #[serde(rename = "toolUseID", skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<&'a str>,
        #[serde(rename = "decisionClassification", skip_serializing_if = "Option::is_none")]
        decision_classification: Option<&'static str>,
    },
    Deny {
        message: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        interrupt: Option<bool>,
        #[serde(rename = "toolUseID", skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<&'a str>,
        #[serde(rename = "decisionClassification", skip_serializing_if = "Option::is_none")]
        decision_classification: Option<&'static str>,
    },
}

impl PermissionOut<'_> {
    /// Serialize, then check it against the R4 schema (and Gilde's stricter
    /// policy) before it may be sent; `Err` names the violated rule.
    pub fn to_checked_raw(&self) -> Result<Box<RawValue>, &'static str> {
        let text = serde_json::to_string(self).map_err(|_| "serialize")?;
        let value: Value = serde_json::from_str(&text).map_err(|_| "reparse")?;
        validate_permission_result(&value)?;
        RawValue::from_string(text).map_err(|_| "raw")
    }
}

const ALLOW_KEYS: [&str; 5] = ["behavior", "updatedInput", "updatedPermissions", "toolUseID", "decisionClassification"];
const DENY_KEYS: [&str; 5] = ["behavior", "message", "interrupt", "toolUseID", "decisionClassification"];

/// The R4 permission-result schema (the CLI's own validator, `hy` in
/// bin2.1.304 [R4 §1.6]) plus Gilde's policy: `updatedInput` always present,
/// `updatedPermissions` only `addRules` → `session` with `behavior:"allow"`
/// (never `setMode`, `addDirectories`, `replaceRules`, `removeRules`, never a
/// settings-file destination) [F-01], `toolUseID` printable ASCII ≤ 128, deny
/// `message` non-empty.
pub fn validate_permission_result(v: &Value) -> Result<(), &'static str> {
    let obj = v.as_object().ok_or("notObject")?;
    match obj.get("behavior").and_then(Value::as_str) {
        Some("allow") => {
            if obj.keys().any(|k| !ALLOW_KEYS.contains(&k.as_str())) {
                return Err("unknownKey");
            }
            if !obj.get("updatedInput").is_some_and(Value::is_object) {
                return Err("updatedInput");
            }
            if let Some(perms) = obj.get("updatedPermissions") {
                let list = perms.as_array().ok_or("updatedPermissions")?;
                if list.is_empty() {
                    return Err("updatedPermissions");
                }
                for p in list {
                    validate_session_rule(p)?;
                }
            }
            if let Some(c) = obj.get("decisionClassification") {
                match c.as_str() {
                    Some(CLASS_TEMPORARY | CLASS_PERMANENT) => {}
                    _ => return Err("decisionClassification"),
                }
            }
        }
        Some("deny") => {
            if obj.keys().any(|k| !DENY_KEYS.contains(&k.as_str())) {
                return Err("unknownKey");
            }
            match obj.get("message").and_then(Value::as_str) {
                Some(m) if !m.trim().is_empty() => {}
                _ => return Err("message"),
            }
            if obj.get("interrupt").is_some_and(|i| !i.is_boolean()) {
                return Err("interrupt");
            }
            if let Some(c) = obj.get("decisionClassification") {
                if c.as_str() != Some(CLASS_REJECT) {
                    return Err("decisionClassification");
                }
            }
        }
        _ => return Err("behavior"),
    }
    if let Some(id) = obj.get("toolUseID") {
        match id.as_str() {
            Some(s) if printable_ascii(s, MAX_TOOL_USE_ID) => {}
            _ => return Err("toolUseID"),
        }
    }
    Ok(())
}

fn validate_session_rule(p: &Value) -> Result<(), &'static str> {
    let o = p.as_object().ok_or("updatedPermissions")?;
    if o.keys().any(|k| !["type", "rules", "behavior", "destination"].contains(&k.as_str())) {
        return Err("updatedPermissions.key");
    }
    if o.get("type").and_then(Value::as_str) != Some("addRules") {
        return Err("updatedPermissions.type");
    }
    if o.get("destination").and_then(Value::as_str) != Some("session") {
        return Err("updatedPermissions.destination");
    }
    if o.get("behavior").and_then(Value::as_str) != Some("allow") {
        return Err("updatedPermissions.behavior");
    }
    let rules = o.get("rules").and_then(Value::as_array).ok_or("updatedPermissions.rules")?;
    if rules.is_empty() {
        return Err("updatedPermissions.rules");
    }
    for r in rules {
        let ro = r.as_object().ok_or("updatedPermissions.rule")?;
        if ro.keys().any(|k| k != "toolName" && k != "ruleContent") {
            return Err("updatedPermissions.rule");
        }
        match ro.get("toolName").and_then(Value::as_str) {
            Some(t) if !t.is_empty() => {}
            _ => return Err("updatedPermissions.toolName"),
        }
        if ro.get("ruleContent").is_some_and(|c| !c.is_string()) {
            return Err("updatedPermissions.ruleContent");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn splitter_skips_long_lines_without_buffering_them() {
        let mut s = LineSplitter::new(8);
        assert_eq!(s.push(b"abc\nde"), vec![LineEvent::Line(b"abc".to_vec())]);
        assert_eq!(s.push(b"fghijklmnop"), vec![]);
        assert!(s.buf.is_empty(), "the long line is not buffered");
        assert_eq!(s.push(b"q\nok\n"), vec![LineEvent::TooLarge, LineEvent::Line(b"ok".to_vec())]);
        // Exactly at the cap is kept; one over is not.
        assert_eq!(s.push(b"12345678\n123456789\n"), vec![LineEvent::Line(b"12345678".to_vec()), LineEvent::TooLarge]);
        assert_eq!(s.push(b"tail"), vec![]);
        assert_eq!(s.finish(), Some(LineEvent::Line(b"tail".to_vec())));
        assert_eq!(s.finish(), None);
        assert_eq!(LINE_CAP, 64 * 1024 * 1024);
    }

    #[test]
    fn depth_is_counted_outside_strings_only() {
        assert!(depth_within(br#"{"a":[1,{"b":"[[[[[[[["}]}"#, 3));
        assert!(!depth_within(br#"{"a":[1,{"b":[]}]}"#, 3));
        let deep = format!("{}{}", "[".repeat(65), "]".repeat(65));
        assert!(!depth_within(deep.as_bytes(), MAX_DEPTH));
        let ok = format!("{}{}", "[".repeat(64), "]".repeat(64));
        assert!(depth_within(ok.as_bytes(), MAX_DEPTH));
        assert!(depth_within(br#"{"s":"a\"{{{{"}"#, 1));
    }

    #[test]
    fn user_frame_is_exact() {
        let f = user_frame("hi \"there\"", "0192f000-0000-4000-8000-000000000000", UserFrameShape::default());
        assert_eq!(
            parse(&f),
            json!({"type":"user","message":{"role":"user","content":[{"type":"text","text":"hi \"there\""}]},
                   "parent_tool_use_id":null,"uuid":"0192f000-0000-4000-8000-000000000000","origin":{"kind":"human"}})
        );
        let now = user_frame("x", "u", UserFrameShape { now: true, ..Default::default() });
        assert_eq!(parse(&now)["priority"], "now");
        let legacy = user_frame("x", "u", UserFrameShape { now: false, origin: false, empty_session_id: true });
        assert!(parse(&legacy).get("origin").is_none());
        assert_eq!(parse(&legacy)["session_id"], "");
    }

    #[test]
    fn control_requests_are_exact() {
        let init = control_request("r1", &ControlBody::Initialize { supported_dialog_kinds: vec![] });
        assert_eq!(parse(&init), json!({"type":"control_request","request_id":"r1","request":{"subtype":"initialize","supportedDialogKinds":[]}}));
        let int = control_request("r2", &ControlBody::Interrupt { cancel_queued: None });
        assert_eq!(parse(&int), json!({"type":"control_request","request_id":"r2","request":{"subtype":"interrupt"}}));
        let intq = control_request("r3", &ControlBody::Interrupt { cancel_queued: Some(true) });
        assert_eq!(parse(&intq)["request"], json!({"subtype":"interrupt","cancel_queued":true}));
        let cancel = control_request("r4", &ControlBody::CancelAsyncMessage { message_uuid: "m".into() });
        assert_eq!(parse(&cancel)["request"], json!({"subtype":"cancel_async_message","message_uuid":"m"}));
        let usage = control_request("r5", &ControlBody::GetUsage {});
        assert_eq!(parse(&usage)["request"], json!({"subtype":"get_usage"}));
        let model = control_request("r6", &ControlBody::SetModel { model: None });
        assert_eq!(parse(&model)["request"], json!({"subtype":"set_model","model":null}));
        let mode = control_request("r7", &ControlBody::SetPermissionMode { mode: "plan".into() });
        assert_eq!(parse(&mode)["request"], json!({"subtype":"set_permission_mode","mode":"plan"}));
    }

    #[test]
    fn control_responses_put_the_id_inside() {
        let raw = RawValue::from_string(r#"{"behavior":"allow","updatedInput":{}}"#.into()).unwrap();
        let ok = control_response_success("cli-1", &raw);
        assert_eq!(
            parse(&ok),
            json!({"type":"control_response","response":{"subtype":"success","request_id":"cli-1","response":{"behavior":"allow","updatedInput":{}}}})
        );
        let err = control_response_error("cli-2", UNSUPPORTED_ERROR);
        assert_eq!(parse(&err), json!({"type":"control_response","response":{"subtype":"error","request_id":"cli-2","error":"unsupported"}}));
    }

    #[test]
    fn the_validator_rejects_every_forbidden_shape() {
        let ok = json!({"behavior":"allow","updatedInput":{"command":"ls"},"toolUseID":"toolu_1","decisionClassification":"user_temporary"});
        assert!(validate_permission_result(&ok).is_ok());
        let session = json!({"behavior":"allow","updatedInput":{},"updatedPermissions":[{"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"ls"}],"behavior":"allow","destination":"session"}],"decisionClassification":"user_permanent"});
        assert!(validate_permission_result(&session).is_ok());
        for bad in [
            json!({"behavior":"allow"}),
            json!({"behavior":"allow","updatedInput":[1]}),
            json!({"behavior":"allow","updatedInput":{},"updatedPermissions":[{"type":"setMode","mode":"acceptEdits","destination":"session"}]}),
            json!({"behavior":"allow","updatedInput":{},"updatedPermissions":[{"type":"addDirectories","directories":["/x"],"destination":"session"}]}),
            json!({"behavior":"allow","updatedInput":{},"updatedPermissions":[{"type":"addRules","rules":[{"toolName":"Bash"}],"behavior":"allow","destination":"localSettings"}]}),
            json!({"behavior":"allow","updatedInput":{},"updatedPermissions":[{"type":"addRules","rules":[{"toolName":"Bash"}],"behavior":"deny","destination":"session"}]}),
            json!({"behavior":"allow","updatedInput":{},"updatedPermissions":[]}),
            json!({"behavior":"allow","updatedInput":{},"toolUseID":"bad id"}),
            json!({"behavior":"allow","updatedInput":{},"decisionClassification":"user_reject"}),
            json!({"behavior":"allow","updatedInput":{},"extra":1}),
            json!({"behavior":"deny"}),
            json!({"behavior":"deny","message":"  "}),
            json!({"behavior":"deny","message":"x","interrupt":"yes"}),
            json!({"behavior":"deny","message":"x","decisionClassification":"user_temporary"}),
            json!({"behavior":"maybe"}),
            json!([]),
        ] {
            assert!(validate_permission_result(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn incoming_ids_stay_raw_until_checked() {
        let line = r#"{"type":"control_request","request_id":7,"request":{"subtype":"can_use_tool"}}"#;
        let f: ControlRequestIn<'_> = serde_json::from_str(line).unwrap();
        assert_eq!(f.id_string(), None);
        assert_eq!(f.subtype().as_deref(), Some("can_use_tool"));
        let line = r#"{"type":"control_response","response":{"subtype":"success","request_id":"g1","response":{"x":1}}}"#;
        let f: ControlResponseIn<'_> = serde_json::from_str(line).unwrap();
        let inner: ResponseInner<'_> = serde_json::from_str(f.response.unwrap().get()).unwrap();
        assert_eq!(inner.id_string().as_deref(), Some("g1"));
        assert!(inner.is_success());
    }
}
