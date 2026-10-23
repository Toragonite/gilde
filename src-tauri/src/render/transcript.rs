//! Claude transcript rows → blocks and structure facts (contract §6.4,
//! §5.2.5, §3.5.1 S3).
//!
//! Owner: T04.
//!
//! One line of a transcript is normalized in two independent ways:
//!
//! * **Structure** ([`row_facts`]) — always on for followed sessions, cheap:
//!   a field-allowlisted, borrowing parse (`RawValue` for every big field) of
//!   `type`, `timestamp`, `uuid`, `message.id`, `message.usage`, block types,
//!   `tool_use` ids/names, `tool_result` ids, `permissionDecision`,
//!   `queued_command.origin.kind`, `<teammate-message>` attribute values,
//!   `TaskCreate`/`TaskUpdate` ids/status/owner/blockedBy, `SendMessage` `to`,
//!   the tool-set attachments (`prompt_snapshot.tools`,
//!   `deferred_tools_delta.addedNames`) and the interrupt marker row
//!   `"[Request interrupted by user]"` [R2 Q3, R5 §8.1, R6 §5 LP-14b, LP-17b].
//!   Message text is never kept here.
//! * **Content** ([`row_content`]) — only while a view subscribes: blocks via
//!   the shared [`claude_blocks`] helper with the **transcript** id scheme
//!   (the row `uuid`, F-06) and tool-result updates.
//!
//! Row kinds: `assistant`/`user`/`system`/`attachment(queued_command)` are
//! content rows; the metadata rows of [`META_ROW_TYPES`] are never blocks;
//! anything else is ignored and counted ([`LineOutcome::UnknownType`]).
//!
//! Lines are bytes: a line that is not valid UTF-8 is skipped and counted
//! (contract §13.1 "transcript row skipped and counted"); JSON `\uD800`-style
//! lone surrogate escapes are repaired to `\uFFFD` first, so such a row is not
//! lost (serde_json rejects them).

use std::borrow::Cow;

use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::Value;

use crate::contract::activity::ErrorSub;
use crate::contract::common::{AbsPath, EpochMs};
use crate::contract::content::{Block, BlockKind, ChangeKind, SystemNote, ToolStatus, UserOrigin};
use crate::contract::team::MessageKind;
use crate::render::blocks::{
    claude_blocks, claude_tool_results, is_interrupt_marker, parse_wrapped, protocol_frame_kind, transcript_block_id,
    user_text_kinds, BlockIdScheme, RenderContext, ToolResultUpdate, SYSTEM_TEXT_CAP,
};
use crate::render::sanitize::sanitize_display_text;
use crate::render::tools::{self, PathBase};

/// Metadata rows: never blocks, never counted as unknown [R2 E13].
pub const META_ROW_TYPES: &[&str] = &[
    "ai-title",
    "custom-title",
    "agent-name",
    "last-prompt",
    "agent-setting",
    "mode",
    "permission-mode",
    "queue-operation",
    "file-history-snapshot",
    "file-history-delta",
    "api-request",
    "api-request-blob",
    "api-request-shape",
    "atis-latch",
    "frame-link",
    "bridge-session",
    "continued-in",
    "summary",
    "cost-state",
    "artifact-autoreact-ledger",
    "artifact-comment-monitor",
    "tag",
];

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// An RFC 3339 timestamp (`2026-10-22T02:58:11.244Z`, an offset, or no
/// fraction) as epoch milliseconds; `None` when malformed or outside
/// `0 < v < 2^53`.
pub fn parse_rfc3339_ms(s: &str) -> Option<EpochMs> {
    let b = s.trim().as_bytes();
    if b.len() < 20 {
        return None;
    }
    let digits = |from: usize, to: usize| -> Option<i64> {
        let part = b.get(from..to)?;
        if !part.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(part).ok()?.parse().ok()
    };
    let (year, month, day) = (digits(0, 4)?, digits(5, 7)?, digits(8, 10)?);
    let (hour, minute, second) = (digits(11, 13)?, digits(14, 16)?, digits(17, 19)?);
    if b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let mut i = 19;
    let mut millis = 0i64;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == start {
            return None;
        }
        let frac = &b[start..i.min(start + 3)];
        let mut ms = 0i64;
        for (k, d) in frac.iter().enumerate() {
            ms += i64::from(d - b'0') * [100, 10, 1][k];
        }
        millis = ms;
    }
    let offset_min = match b.get(i) {
        Some(b'Z' | b'z') if i + 1 == b.len() => 0,
        Some(sign @ (b'+' | b'-')) if i + 6 == b.len() && b[i + 3] == b':' => {
            let h = digits(i + 1, i + 3)?;
            let m = digits(i + 4, i + 6)?;
            let v = h * 60 + m;
            if *sign == b'-' {
                -v
            } else {
                v
            }
        }
        _ => return None,
    };
    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3_600 + minute * 60 + second.min(59) - offset_min * 60;
    let ms = secs.checked_mul(1000)?.checked_add(millis)?;
    (ms > 0 && ms < (1i64 << 53)).then_some(ms)
}

/// Replace JSON `\uXXXX` escapes that are lone UTF-16 surrogates with
/// `\uFFFD` (serde_json rejects them; JavaScript writes them). Escaped
/// backslashes are respected.
pub fn repair_lone_surrogates(line: &str) -> Cow<'_, str> {
    if !line.contains("\\u") {
        return Cow::Borrowed(line);
    }
    let b = line.as_bytes();
    let hex4 = |at: usize| -> Option<u16> {
        let s = std::str::from_utf8(b.get(at..at + 4)?).ok()?;
        u16::from_str_radix(s, 16).ok()
    };
    let is_u_escape = |at: usize| b.get(at) == Some(&b'\\') && b.get(at + 1) == Some(&b'u');
    let mut out: Option<String> = None;
    let mut last = 0usize;
    let mut i = 0usize;
    while i < b.len() {
        if b[i] != b'\\' {
            i += 1;
            continue;
        }
        if b.get(i + 1) != Some(&b'u') {
            i += 2;
            continue;
        }
        let Some(unit) = hex4(i + 2) else {
            i += 2;
            continue;
        };
        let lone = match unit {
            0xD800..=0xDBFF => {
                // A high surrogate needs a low one right after it.
                if is_u_escape(i + 6) && hex4(i + 8).is_some_and(|n| (0xDC00..=0xDFFF).contains(&n)) {
                    i += 12;
                    continue;
                }
                true
            }
            0xDC00..=0xDFFF => true,
            _ => false,
        };
        if lone {
            let o = out.get_or_insert_with(|| String::with_capacity(line.len()));
            o.push_str(&line[last..i]);
            o.push_str("\\uFFFD");
            last = i + 6;
        }
        i += 6;
    }
    match out {
        None => Cow::Borrowed(line),
        Some(mut o) => {
            o.push_str(&line[last..]);
            Cow::Owned(o)
        }
    }
}

/// The text of the line, or why it cannot be parsed.
pub fn line_text(bytes: &[u8]) -> Result<Cow<'_, str>, LineOutcome> {
    let text = std::str::from_utf8(bytes).map_err(|_| LineOutcome::InvalidUtf8)?;
    let text = text.trim_end_matches(['\n', '\r']);
    if text.trim().is_empty() {
        return Err(LineOutcome::Blank);
    }
    Ok(repair_lone_surrogates(text))
}

/// The `timestamp` of a row (Claude transcript or codex rollout), parsed
/// without materializing any other field.
pub fn row_time(bytes: &[u8]) -> Option<EpochMs> {
    #[derive(Deserialize)]
    struct T<'a> {
        #[serde(borrow, default)]
        timestamp: Option<Cow<'a, str>>,
    }
    let text = line_text(bytes).ok()?;
    let t: T<'_> = serde_json::from_str(&text).ok()?;
    t.timestamp.as_deref().and_then(parse_rfc3339_ms)
}

/// Classify an API error text the way the transcript shows it
/// (`isApiErrorMessage` rows, `error` fields): usage limit (with the reset
/// time of the `…|<epoch seconds>` form), sign-in, or agent error.
pub fn classify_api_error(text: &str, error_field: Option<&str>) -> ErrorSub {
    let lower = text.to_ascii_lowercase();
    if error_field == Some("authentication_failed")
        || lower.contains("/login")
        || lower.contains("not logged in")
        || lower.contains("invalid api key")
        || lower.contains("authentication")
    {
        return ErrorSub::SignInNeeded;
    }
    if lower.contains("usage limit") || lower.contains("rate limit") || lower.contains(" 429") || error_field == Some("rate_limit") {
        let resets_at = text
            .rsplit_once('|')
            .and_then(|(_, t)| t.trim().parse::<i64>().ok())
            .and_then(|secs| secs.checked_mul(1000))
            .filter(|ms| *ms > 0 && *ms < (1i64 << 53));
        return ErrorSub::UsageLimit { account_id: None, resets_at };
    }
    ErrorSub::AgentError
}

// ---------------------------------------------------------------------------
// outcomes and facts
// ---------------------------------------------------------------------------

/// What happened to one line (diagnostics count these; never content).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineOutcome {
    /// A content row.
    Content,
    /// A metadata row (not a block).
    Meta,
    /// An unknown row type (ignored, counted).
    UnknownType(String),
    /// Not valid UTF-8: skipped, counted.
    InvalidUtf8,
    /// Not JSON, or not an object: skipped, counted.
    Unparseable,
    /// Empty line.
    Blank,
    /// Longer than the line cap (`System{RowTooLarge}`).
    TooLarge,
}

/// Token usage of one assistant message (`message.usage`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
}

impl TokenUsage {
    pub fn add(&mut self, other: &TokenUsage) {
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.cache_creation = self.cache_creation.saturating_add(other.cache_creation);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
    }
}

/// How a turn ended, from transcript rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnEndKind {
    Ok,
    /// The `"[Request interrupted by user]"` row (a `TurnEnd{Interrupted}`,
    /// §3.5.3 / §7.2.2).
    Interrupted,
}

/// A `tool_use` seen in a row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolUseFact {
    pub id: String,
    pub name: String,
}

/// A `<teammate-message>` (or other wrapper) seen in a user row or a
/// `queued_command` attachment: attribute values only, never the body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrappedFact {
    pub origin: UserOrigin,
    pub teammate_id: Option<String>,
    pub color: Option<String>,
    pub msg_id: Option<String>,
    /// The protocol frame kind when the body is a frame, else `Plain`.
    pub kind: MessageKind,
    pub has_summary: bool,
}

/// A `TaskCreate`/`TaskUpdate` tool call's id/status/owner/blockedBy (never
/// the subject or description).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskOpFact {
    pub tool: String,
    pub tool_use_id: String,
    pub task_id: Option<String>,
    pub status: Option<String>,
    pub owner: Option<String>,
    pub blocked_by: Vec<String>,
}

/// A `SendMessage` call (`to`) and, once its result row arrives, its `msg_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendMessageFact {
    pub tool_use_id: String,
    pub to: Option<String>,
}

/// The structure facts of one row (no content).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RowFacts {
    /// `type` (or `type/subtype` for system rows).
    pub row_type: String,
    pub at: Option<EpochMs>,
    pub uuid: Option<String>,
    pub message_id: Option<String>,
    /// `message.usage` of an assistant row (repeats on every row of one
    /// `message.id`: sum once per id, LP-26).
    pub usage: Option<TokenUsage>,
    pub tool_uses: Vec<ToolUseFact>,
    /// `tool_use_id`s answered by this row.
    pub tool_results: Vec<String>,
    /// `permissionDecision.decision` of a result row (`accept`/`reject`).
    pub permission_decision: Option<String>,
    /// A turn-opening user text (a human prompt, a teammate/peer message or a
    /// task notification).
    pub turn_open: Option<UserOrigin>,
    pub turn_end: Option<TurnEndKind>,
    pub api_error: Option<ErrorSub>,
    pub wrapped: Vec<WrappedFact>,
    pub task_ops: Vec<TaskOpFact>,
    pub send_messages: Vec<SendMessageFact>,
    /// `toolUseResult.msg_id` of a SendMessage result row (R6 §5 LP-16).
    pub send_message_msg_id: Option<String>,
    /// Tool names from tool-set attachments.
    pub tool_names: Vec<String>,
    pub sidechain: bool,
}

// ---------------------------------------------------------------------------
// structure: a borrowing, allowlisted parse
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct LiteRow<'a> {
    #[serde(rename = "type", borrow, default)]
    ty: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    subtype: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    timestamp: Option<Cow<'a, str>>,
    #[serde(default)]
    uuid: Option<String>,
    #[serde(rename = "isMeta", default)]
    is_meta: Option<bool>,
    #[serde(rename = "isSidechain", default)]
    is_sidechain: Option<bool>,
    #[serde(rename = "isApiErrorMessage", default)]
    is_api_error: Option<bool>,
    #[serde(rename = "isCompactSummary", default)]
    is_compact_summary: Option<bool>,
    #[serde(borrow, default)]
    error: Option<&'a RawValue>,
    #[serde(borrow, default)]
    message: Option<LiteMessage<'a>>,
    #[serde(rename = "permissionDecision", borrow, default)]
    permission_decision: Option<&'a RawValue>,
    #[serde(rename = "toolUseResult", borrow, default)]
    tool_use_result: Option<&'a RawValue>,
    #[serde(borrow, default)]
    attachment: Option<&'a RawValue>,
    #[serde(borrow, default)]
    origin: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct LiteMessage<'a> {
    #[serde(default)]
    id: Option<String>,
    #[serde(borrow, default)]
    stop_reason: Option<Cow<'a, str>>,
    #[serde(default)]
    usage: Option<LiteUsage>,
    #[serde(borrow, default)]
    content: Option<&'a RawValue>,
}

#[derive(Deserialize, Default)]
struct LiteUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct LiteItem<'a> {
    #[serde(rename = "type", borrow, default)]
    ty: Option<Cow<'a, str>>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    tool_use_id: Option<String>,
    #[serde(borrow, default)]
    input: Option<&'a RawValue>,
    #[serde(borrow, default)]
    text: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct LiteTaskInput {
    #[serde(rename = "taskId", alias = "task_id", alias = "id", default)]
    task_id: Option<Value>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    owner: Option<String>,
    #[serde(rename = "blockedBy", alias = "addBlockedBy", default)]
    blocked_by: Option<Vec<Value>>,
}

#[derive(Deserialize)]
struct LiteSendInput {
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    recipient: Option<String>,
}

#[derive(Deserialize)]
struct LiteAttachment<'a> {
    #[serde(rename = "type", borrow, default)]
    ty: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    prompt: Option<&'a RawValue>,
    #[serde(borrow, default)]
    origin: Option<&'a RawValue>,
    #[serde(rename = "addedNames", default)]
    added_names: Option<Vec<String>>,
    #[serde(borrow, default)]
    tools: Option<&'a RawValue>,
}

fn origin_kind(raw: Option<&RawValue>) -> Option<String> {
    #[derive(Deserialize)]
    struct O {
        #[serde(default)]
        kind: Option<String>,
    }
    let o: O = serde_json::from_str(raw?.get()).ok()?;
    o.kind
}

/// `origin.kind` → the UserText origin.
pub fn origin_from_kind(kind: &str) -> Option<UserOrigin> {
    match kind {
        "human" => Some(UserOrigin::Human),
        "peer" => Some(UserOrigin::Peer),
        "teammate" => Some(UserOrigin::Teammate),
        "task-notification" | "task_notification" => Some(UserOrigin::TaskNotification),
        _ => None,
    }
}

/// A raw value that is a string or an array of `{type:"text", text}` items,
/// joined with newlines.
fn raw_text(raw: &RawValue) -> Option<String> {
    let v: Value = serde_json::from_str(raw.get()).ok()?;
    match v {
        Value::String(s) => Some(s),
        Value::Array(items) => {
            let parts: Vec<&str> = items
                .iter()
                .filter(|i| i.get("type").and_then(Value::as_str).is_none_or(|t| t == "text"))
                .filter_map(|i| i.get("text").and_then(Value::as_str).or_else(|| i.as_str()))
                .collect();
            Some(parts.join("\n"))
        }
        _ => None,
    }
}

fn wrapped_facts(text: &str, default_origin: UserOrigin) -> Vec<WrappedFact> {
    parse_wrapped(text)
        .map(|ws| {
            ws.into_iter()
                .map(|w| WrappedFact {
                    origin: w.tag.origin(),
                    teammate_id: w.attr("teammate_id").or_else(|| w.attr("from")).map(str::to_owned),
                    color: w.attr("color").map(str::to_owned),
                    msg_id: w.attr("msg_id").map(str::to_owned),
                    kind: protocol_frame_kind(&w.body).unwrap_or(MessageKind::Plain),
                    has_summary: w.attr("summary").is_some(),
                })
                .collect()
        })
        .unwrap_or_else(|| {
            if default_origin == UserOrigin::Human {
                Vec::new()
            } else {
                vec![WrappedFact {
                    origin: default_origin,
                    teammate_id: None,
                    color: None,
                    msg_id: None,
                    kind: MessageKind::Plain,
                    has_summary: false,
                }]
            }
        })
}

fn tool_names_from(raw: &RawValue) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<Value>(raw.get()) else {
        return Vec::new();
    };
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|t| t.as_str().or_else(|| t.get("name").and_then(Value::as_str)))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn ends_turn(stop_reason: Option<&str>) -> bool {
    matches!(stop_reason, Some("end_turn" | "stop_sequence" | "max_tokens" | "refusal"))
}

/// The structure facts of one Claude transcript line, plus its outcome.
pub fn row_facts(bytes: &[u8]) -> (Option<RowFacts>, LineOutcome) {
    let text = match line_text(bytes) {
        Ok(t) => t,
        Err(o) => return (None, o),
    };
    let row: LiteRow<'_> = match serde_json::from_str(&text) {
        Ok(r) => r,
        Err(_) => return (None, LineOutcome::Unparseable),
    };
    let ty = row.ty.as_deref().unwrap_or("");
    let mut f = RowFacts {
        row_type: match &row.subtype {
            Some(s) if ty == "system" => format!("system/{s}"),
            _ => ty.to_owned(),
        },
        at: row.timestamp.as_deref().and_then(parse_rfc3339_ms),
        uuid: row.uuid.clone(),
        sidechain: row.is_sidechain.unwrap_or(false),
        ..RowFacts::default()
    };
    let outcome = match ty {
        "assistant" => {
            if let Some(m) = &row.message {
                f.message_id = m.id.clone();
                f.usage = m.usage.as_ref().map(|u| TokenUsage {
                    input: u.input_tokens.unwrap_or(0),
                    output: u.output_tokens.unwrap_or(0),
                    cache_creation: u.cache_creation_input_tokens.unwrap_or(0),
                    cache_read: u.cache_read_input_tokens.unwrap_or(0),
                });
                if let Some(items) = m.content.and_then(|c| serde_json::from_str::<Vec<LiteItem<'_>>>(c.get()).ok()) {
                    for item in items {
                        if !matches!(item.ty.as_deref(), Some("tool_use" | "server_tool_use" | "mcp_tool_use")) {
                            continue;
                        }
                        let (Some(id), Some(name)) = (item.id, item.name) else { continue };
                        match name.as_str() {
                            "TaskCreate" | "TaskUpdate" => {
                                if let Some(input) = item.input.and_then(|i| serde_json::from_str::<LiteTaskInput>(i.get()).ok()) {
                                    f.task_ops.push(TaskOpFact {
                                        tool: name.clone(),
                                        tool_use_id: id.clone(),
                                        task_id: input.task_id.and_then(|v| match v {
                                            Value::String(s) => Some(s),
                                            Value::Number(n) => Some(n.to_string()),
                                            _ => None,
                                        }),
                                        status: input.status,
                                        owner: input.owner,
                                        blocked_by: input
                                            .blocked_by
                                            .unwrap_or_default()
                                            .into_iter()
                                            .filter_map(|v| match v {
                                                Value::String(s) => Some(s),
                                                Value::Number(n) => Some(n.to_string()),
                                                _ => None,
                                            })
                                            .collect(),
                                    });
                                }
                            }
                            "SendMessage" => {
                                let to = item
                                    .input
                                    .and_then(|i| serde_json::from_str::<LiteSendInput>(i.get()).ok())
                                    .and_then(|i| i.to.or(i.recipient));
                                f.send_messages.push(SendMessageFact { tool_use_id: id.clone(), to });
                            }
                            _ => {}
                        }
                        f.tool_uses.push(ToolUseFact { id, name });
                    }
                }
                if ends_turn(m.stop_reason.as_deref()) {
                    f.turn_end = Some(TurnEndKind::Ok);
                }
            }
            let error_field = row.error.and_then(|e| serde_json::from_str::<String>(e.get()).ok());
            if row.is_api_error.unwrap_or(false) || error_field.is_some() {
                let text = row
                    .message
                    .as_ref()
                    .and_then(|m| m.content)
                    .and_then(raw_text)
                    .unwrap_or_default();
                f.api_error = Some(classify_api_error(&text, error_field.as_deref()));
                f.turn_end = Some(TurnEndKind::Ok);
            }
            LineOutcome::Content
        }
        "user" => {
            if let Some(d) = row.permission_decision.and_then(|d| serde_json::from_str::<Value>(d.get()).ok()) {
                f.permission_decision = d.get("decision").and_then(Value::as_str).map(str::to_owned);
            }
            if let Some(r) = row.tool_use_result.and_then(|r| serde_json::from_str::<Value>(r.get()).ok()) {
                f.send_message_msg_id = r.get("msg_id").and_then(Value::as_str).map(str::to_owned);
            }
            let row_origin = origin_kind(row.origin).as_deref().and_then(origin_from_kind).unwrap_or(UserOrigin::Human);
            let meta = row.is_meta.unwrap_or(false) || row.is_compact_summary.unwrap_or(false);
            if let Some(content) = row.message.as_ref().and_then(|m| m.content) {
                let raw = content.get();
                if raw.starts_with('"') {
                    if let Ok(text) = serde_json::from_str::<String>(raw) {
                        user_text_facts(&mut f, &text, row_origin, meta);
                    }
                } else if let Ok(items) = serde_json::from_str::<Vec<LiteItem<'_>>>(raw) {
                    let mut texts = Vec::new();
                    for item in items {
                        match item.ty.as_deref() {
                            Some(t) if t == "tool_result" || t.ends_with("_tool_result") => {
                                if let Some(id) = item.tool_use_id {
                                    f.tool_results.push(id);
                                }
                            }
                            Some("text") => {
                                if let Some(t) = item.text.and_then(|t| serde_json::from_str::<String>(t.get()).ok()) {
                                    texts.push(t);
                                }
                            }
                            _ => {}
                        }
                    }
                    if f.tool_results.is_empty() {
                        for t in texts {
                            user_text_facts(&mut f, &t, row_origin, meta);
                        }
                    }
                }
            }
            LineOutcome::Content
        }
        "system" => {
            if row.subtype.as_deref() == Some("turn_duration") {
                f.turn_end = Some(TurnEndKind::Ok);
            }
            LineOutcome::Content
        }
        "attachment" => {
            if let Some(a) = row.attachment.and_then(|a| serde_json::from_str::<LiteAttachment<'_>>(a.get()).ok()) {
                match a.ty.as_deref() {
                    Some("queued_command") => {
                        let origin = origin_kind(a.origin).as_deref().and_then(origin_from_kind).unwrap_or(UserOrigin::Human);
                        if let Some(text) = a.prompt.and_then(raw_text) {
                            if origin != UserOrigin::Human {
                                f.wrapped = wrapped_facts(&text, origin);
                                f.turn_open = Some(origin);
                            }
                        }
                    }
                    Some("deferred_tools_delta") => f.tool_names = a.added_names.unwrap_or_default(),
                    Some("prompt_snapshot") => f.tool_names = a.tools.map(tool_names_from).unwrap_or_default(),
                    _ => {}
                }
            }
            LineOutcome::Content
        }
        t if META_ROW_TYPES.contains(&t) => LineOutcome::Meta,
        t => LineOutcome::UnknownType(t.chars().take(64).collect()),
    };
    (Some(f), outcome)
}

fn user_text_facts(f: &mut RowFacts, text: &str, row_origin: UserOrigin, meta: bool) {
    if is_interrupt_marker(text) {
        f.turn_end = Some(TurnEndKind::Interrupted);
        return;
    }
    if meta {
        return;
    }
    let wrapped = wrapped_facts(text, row_origin);
    let origin = wrapped.first().map_or(row_origin, |w| w.origin);
    f.wrapped.extend(wrapped);
    if f.turn_open.is_none() {
        f.turn_open = Some(origin);
    }
}

// ---------------------------------------------------------------------------
// content
// ---------------------------------------------------------------------------

/// What the result row says about the call's file change (`toolUseResult`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResultMeta {
    /// `+`/`−` from `structuredPatch`.
    pub patch: Option<(u32, u32)>,
    /// `create` / `update` of a Write.
    pub write_kind: Option<ChangeKind>,
    /// `filePath`, for display.
    pub file_path: Option<String>,
}

impl ResultMeta {
    pub fn from_tool_use_result(result: &Value, base: PathBase<'_>) -> ResultMeta {
        ResultMeta {
            patch: result.get("structuredPatch").and_then(tools::patch_counts),
            write_kind: match result.get("type").and_then(Value::as_str) {
                Some("create") => Some(ChangeKind::Added),
                Some("update") => Some(ChangeKind::Modified),
                _ => None,
            },
            file_path: result.get("filePath").and_then(Value::as_str).map(|p| tools::display_path(p, base)),
        }
    }
}

/// One content effect of a row, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum ContentItem {
    Block(Block),
    /// A result for an earlier `ToolCall` (matched by `tool_use_id`).
    ToolResult { update: ToolResultUpdate, meta: ResultMeta },
}

/// Paths and the clock base for content rows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContentBase {
    pub location: Option<AbsPath>,
    pub home: Option<AbsPath>,
}

/// The synthetic block id of a row without a `uuid` (or of an oversized
/// row): stable for the file, `o<byte offset>`.
pub fn offset_id(offset: u64) -> String {
    format!("o{offset}")
}

/// The `System{RowTooLarge}` block of a line over the cap.
pub fn row_too_large_block(offset: u64, at: EpochMs) -> Block {
    Block {
        id: transcript_block_id(&offset_id(offset), 0),
        seq: 0,
        at,
        kind: BlockKind::System { note: SystemNote::RowTooLarge, text: None },
        truncated: false,
    }
}

fn kinds_to_blocks(kinds: Vec<(BlockKind, bool)>, row_id: &str, at: EpochMs) -> Vec<ContentItem> {
    kinds
        .into_iter()
        .enumerate()
        .map(|(k, (kind, truncated))| ContentItem::Block(Block { id: transcript_block_id(row_id, k), seq: 0, at, kind, truncated }))
        .collect()
}

/// The content items of one Claude transcript line. `fallback_at` is used
/// for rows without a timestamp; `offset` names rows without a `uuid`.
pub fn row_content(bytes: &[u8], offset: u64, fallback_at: EpochMs, base: &ContentBase) -> Vec<ContentItem> {
    let Ok(text) = line_text(bytes) else {
        return Vec::new();
    };
    let Ok(row) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    let ty = row.get("type").and_then(Value::as_str).unwrap_or("");
    if row.get("isSidechain").and_then(Value::as_bool).unwrap_or(false) && ty != "system" {
        return Vec::new();
    }
    let at = row.get("timestamp").and_then(Value::as_str).and_then(parse_rfc3339_ms).unwrap_or(fallback_at);
    let row_id = row.get("uuid").and_then(Value::as_str).filter(|u| !u.is_empty()).map_or_else(|| offset_id(offset), str::to_owned);
    let ctx = RenderContext { location: base.location.clone(), home: base.home.clone(), at, next_seq: 0 };
    let pbase = ctx.path_base();
    match ty {
        "assistant" => {
            let Some(message) = row.get("message") else { return Vec::new() };
            let error_field = row.get("error").and_then(Value::as_str);
            if row.get("isApiErrorMessage").and_then(Value::as_bool).unwrap_or(false) || error_field.is_some() {
                let text = match message.get("content") {
                    Some(Value::String(s)) => s.clone(),
                    Some(Value::Array(items)) => items
                        .iter()
                        .filter_map(|i| i.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => String::new(),
                };
                let sub = classify_api_error(&text, error_field);
                let s = sanitize_display_text(&text, SYSTEM_TEXT_CAP);
                return kinds_to_blocks(
                    vec![(BlockKind::System { note: SystemNote::Error { sub }, text: Some(s.text).filter(|t| !t.is_empty()) }, s.truncated)],
                    &row_id,
                    at,
                );
            }
            let seconds = row
                .get("thinking_duration_ms")
                .and_then(Value::as_u64)
                .map(|ms| u32::try_from(ms.div_ceil(1000)).unwrap_or(u32::MAX));
            claude_blocks(message, BlockIdScheme::Transcript { row_uuid: &row_id }, &ctx)
                .into_iter()
                .map(|mut b| {
                    if let BlockKind::Thinking { seconds: s, .. } = &mut b.kind {
                        if s.is_none() {
                            *s = seconds;
                        }
                    }
                    ContentItem::Block(b)
                })
                .collect()
        }
        "user" => {
            if row.get("isMeta").and_then(Value::as_bool).unwrap_or(false)
                || row.get("isCompactSummary").and_then(Value::as_bool).unwrap_or(false)
            {
                // The interrupt marker can be a meta row too; it still ends a turn.
                let marker = match row.pointer("/message/content") {
                    Some(Value::String(s)) => is_interrupt_marker(s),
                    _ => false,
                };
                if !marker {
                    return Vec::new();
                }
            }
            let Some(message) = row.get("message") else { return Vec::new() };
            let row_origin = row
                .pointer("/origin/kind")
                .and_then(Value::as_str)
                .and_then(origin_from_kind)
                .unwrap_or(UserOrigin::Human);
            let mut out: Vec<ContentItem> = claude_blocks(message, BlockIdScheme::Transcript { row_uuid: &row_id }, &ctx)
                .into_iter()
                .map(|mut b| {
                    if let BlockKind::UserText { origin, .. } = &mut b.kind {
                        if *origin == UserOrigin::Human {
                            *origin = row_origin;
                        }
                    }
                    ContentItem::Block(b)
                })
                .collect();
            let denied = row
                .pointer("/permissionDecision/decision")
                .and_then(Value::as_str)
                .is_some_and(|d| matches!(d, "reject" | "deny" | "denied"));
            let meta = row
                .get("toolUseResult")
                .filter(|r| r.is_object())
                .map(|r| ResultMeta::from_tool_use_result(r, pbase))
                .unwrap_or_default();
            for mut update in claude_tool_results(message) {
                if denied {
                    update.status = ToolStatus::Denied;
                }
                out.push(ContentItem::ToolResult { update, meta: meta.clone() });
            }
            out
        }
        "system" => {
            let note = match row.get("subtype").and_then(Value::as_str) {
                Some("compact_boundary") => SystemNote::CompactBoundary,
                Some("api_error") => SystemNote::ApiRetry {
                    attempt: row
                        .get("retryAttempt")
                        .and_then(Value::as_u64)
                        .map_or(0, |a| u32::try_from(a).unwrap_or(u32::MAX)),
                },
                _ => return Vec::new(),
            };
            kinds_to_blocks(vec![(BlockKind::System { note, text: None }, false)], &row_id, at)
        }
        "attachment" => {
            let Some(a) = row.get("attachment") else { return Vec::new() };
            if a.get("type").and_then(Value::as_str) != Some("queued_command") {
                return Vec::new();
            }
            let origin = a.pointer("/origin/kind").and_then(Value::as_str).and_then(origin_from_kind).unwrap_or(UserOrigin::Human);
            if origin == UserOrigin::Human {
                return Vec::new();
            }
            let text = match a.get("prompt") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Array(items)) => items.iter().filter_map(|i| i.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"),
                _ => return Vec::new(),
            };
            kinds_to_blocks(user_text_kinds(&text, origin), &row_id, at)
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:01Z"), Some(1000));
        assert_eq!(parse_rfc3339_ms("2026-10-22T02:58:11.244Z"), Some(1_792_637_891_244));
        assert_eq!(parse_rfc3339_ms("2026-10-22T11:58:11.244+09:00"), Some(1_792_637_891_244));
        assert_eq!(parse_rfc3339_ms("2026-10-22T02:58:11.2449999Z"), Some(1_792_637_891_244));
        assert_eq!(parse_rfc3339_ms("2026-13-22T02:58:11Z"), None);
        assert_eq!(parse_rfc3339_ms("garbage-not-a-date!!"), None);
        assert_eq!(parse_rfc3339_ms("2026-10-22T02:58:11"), None);
    }

    #[test]
    fn surrogate_repair() {
        assert_eq!(repair_lone_surrogates(r#"{"t":"a\ud800b"}"#), r#"{"t":"a\uFFFDb"}"#);
        assert_eq!(repair_lone_surrogates(r#"{"t":"\udc00"}"#), r#"{"t":"\uFFFD"}"#);
        let pair = r#"{"t":"\ud83d\ude00"}"#;
        assert_eq!(repair_lone_surrogates(pair), pair);
        let escaped = r#"{"t":"\\ud800"}"#;
        assert_eq!(repair_lone_surrogates(escaped), escaped);
        let v: Value = serde_json::from_str(&repair_lone_surrogates(r#"{"t":"x\ud800"}"#)).unwrap();
        assert_eq!(v["t"], "x\u{FFFD}");
    }

    #[test]
    fn api_errors_classify() {
        assert_eq!(classify_api_error("Not logged in · Please run /login", None), ErrorSub::SignInNeeded);
        assert_eq!(
            classify_api_error("Claude AI usage limit reached|1792640000", None),
            ErrorSub::UsageLimit { account_id: None, resets_at: Some(1_792_640_000_000) }
        );
        assert_eq!(classify_api_error("API Error: 500", None), ErrorSub::AgentError);
    }
}
