//! The shared Claude message → block helper (contract §3.7, §6.1.5, §6.4):
//! one mapping for the stream (T05) and the transcript follower (T04).
//!
//! Owner: T04. The signature of [`claude_blocks`] is fixed by T01 (A7.6).
//!
//! Rules:
//! - **Ids** (§3.2 `BlockId`, F-06): stream blocks are `<message.id>#<index>`,
//!   transcript blocks are the row `uuid` (each assistant row holds exactly
//!   one block; up to 11 rows share one `message.id`, so the message id is
//!   never a transcript block id). A row or item that yields several blocks
//!   gets `#<k>` / `.<j>` suffixes. Ids that are not printable ASCII ≤ 160
//!   bytes are replaced by `h_<sha256/16>` of the raw id.
//! - **Content items**: `text` → `AssistantText` (assistant) or `UserText`
//!   (user); `thinking` → `Thinking` (text ≤ 64 KiB); `redacted_thinking` →
//!   `Thinking` without text; `tool_use`/`server_tool_use`/`mcp_tool_use` →
//!   `ToolCall{Running}`; `tool_result` items are not blocks — they update the
//!   tool call ([`claude_tool_results`], [`apply_tool_result`]); anything else
//!   → `Unknown{type_name}`.
//! - **User text**: `"[Request interrupted by user…]"` → `System{Interrupted}`;
//!   `<teammate-message>` → `UserText{Teammate}`, `<cross-session-message>` /
//!   `<agent-message>` → `UserText{Peer}`, `<task-notification>` →
//!   `UserText{TaskNotification}`; a teammate protocol frame (a JSON body with
//!   a `type`) → `System{TeamEvent}` without its text (§5.2.4 privacy).
//! - Every string passes `sanitize_display_text` with its §3.7.1 cap; a cut
//!   sets `Block.truncated`.

use serde_json::Value;

use crate::contract::common::{AbsPath, EpochMs};
use crate::contract::content::{
    Block, BlockKind, FileChange, SystemNote, ToolStatus, ToolSummary, ToolVerb, UserOrigin,
};
use crate::contract::display::TextCap;
use crate::contract::ids::{hash16, BlockId};
use crate::contract::team::MessageKind;
use crate::render::sanitize::{display, sanitize_display_text};
use crate::render::tools::{self, PathBase};

/// `UserText.text` (§3.7.1).
pub const USER_TEXT_CAP: TextCap = TextCap::Bytes(256 * 1024);
/// `AssistantText.markdown` (§3.7.1).
pub const ASSISTANT_TEXT_CAP: TextCap = TextCap::Bytes(1024 * 1024);
/// `Thinking.text` (§3.7.1).
pub const THINKING_CAP: TextCap = TextCap::Bytes(64 * 1024);
/// `System.text`.
pub const SYSTEM_TEXT_CAP: TextCap = TextCap::Bytes(4 * 1024);
/// `Unknown.type_name` (§3.7.1).
pub const TYPE_NAME_CAP: TextCap = TextCap::Chars(64);
/// A subagent or teammate label (F-28: ≤ 60 chars).
pub const AGENT_LABEL_CAP: TextCap = TextCap::Chars(60);
/// `BlockId` is printable ASCII ≤ 160 bytes (§3.2).
pub const MAX_BLOCK_ID_BYTES: usize = 160;
/// `tool_use_id` is validated only as printable ASCII ≤ 128 (§6.1.4).
pub const MAX_TOOL_USE_ID_BYTES: usize = 128;

/// How the blocks of one Claude message get their ids (§3.2 `BlockId`, F-06):
/// a view never mixes the two schemes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockIdScheme<'a> {
    /// Stream blocks: `<message.id>#<stream content index>`.
    Stream { message_id: &'a str },
    /// Transcript rows: the row `uuid` (each assistant row holds one block).
    Transcript { row_uuid: &'a str },
}

/// What block construction needs besides the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderContext {
    /// Paths are made relative to this location (§3.7.3).
    pub location: Option<AbsPath>,
    /// Replaced by `~` in paths outside the location.
    pub home: Option<AbsPath>,
    /// The row/frame timestamp.
    pub at: EpochMs,
    /// The next per-view sequence number.
    pub next_seq: u64,
}

impl RenderContext {
    /// The path base of §3.7.3 for this context.
    pub fn path_base(&self) -> PathBase<'_> {
        PathBase { location: self.location.as_ref().map(AbsPath::as_str), home: self.home.as_ref().map(AbsPath::as_str) }
    }
}

/// A `BlockId` from a raw id: kept when it is printable ASCII ≤ 160 bytes,
/// else `h_` + the first 16 hex chars of its SHA-256.
pub fn block_id(raw: &str) -> BlockId {
    let ok = !raw.is_empty() && raw.len() <= MAX_BLOCK_ID_BYTES && raw.bytes().all(|b| (0x20..=0x7e).contains(&b));
    let s = if ok { raw.to_owned() } else { format!("h_{}", hash16(raw.as_bytes())) };
    BlockId::parse(s).unwrap_or_else(|_| BlockId::parse(format!("h_{}", hash16(raw.as_bytes()))).expect("hash id"))
}

/// The id of the block at stream content index `index` of message `message_id`.
pub fn stream_block_id(message_id: &str, index: usize) -> BlockId {
    block_id(&format!("{message_id}#{index}"))
}

/// The id of the `k`-th block of a transcript row: the row uuid, then `uuid#k`.
pub fn transcript_block_id(row_uuid: &str, k: usize) -> BlockId {
    if k == 0 {
        block_id(row_uuid)
    } else {
        block_id(&format!("{row_uuid}#{k}"))
    }
}

/// A `tool_use_id` as shown and matched: printable ASCII ≤ 128, else hashed.
pub fn tool_use_id(raw: &str) -> String {
    if !raw.is_empty() && raw.len() <= MAX_TOOL_USE_ID_BYTES && raw.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
        raw.to_owned()
    } else {
        format!("h_{}", hash16(raw.as_bytes()))
    }
}

/// The interrupt markers the CLI writes as user text (R6 §3, §5 LP-14b).
pub const INTERRUPT_MARKERS: [&str; 2] = ["[Request interrupted by user]", "[Request interrupted by user for tool use]"];

/// `"[Request interrupted by user]"` and its `… for tool use` form.
pub fn is_interrupt_marker(text: &str) -> bool {
    let t = text.trim();
    t.len() <= 96 && t.starts_with("[Request interrupted by user") && t.ends_with(']')
}

// ---------------------------------------------------------------------------
// wrapped user messages (<teammate-message …>, <cross-session-message …>, …)
// ---------------------------------------------------------------------------

/// The wrapper elements the CLI puts around non-human prompts [R2 Q3, R5 §2].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrapTag {
    TeammateMessage,
    AgentMessage,
    CrossSessionMessage,
    TaskNotification,
}

impl WrapTag {
    pub const fn name(self) -> &'static str {
        match self {
            WrapTag::TeammateMessage => "teammate-message",
            WrapTag::AgentMessage => "agent-message",
            WrapTag::CrossSessionMessage => "cross-session-message",
            WrapTag::TaskNotification => "task-notification",
        }
    }

    pub const fn origin(self) -> UserOrigin {
        match self {
            WrapTag::TeammateMessage => UserOrigin::Teammate,
            WrapTag::AgentMessage | WrapTag::CrossSessionMessage => UserOrigin::Peer,
            WrapTag::TaskNotification => UserOrigin::TaskNotification,
        }
    }

    const ALL: [WrapTag; 4] =
        [WrapTag::TeammateMessage, WrapTag::AgentMessage, WrapTag::CrossSessionMessage, WrapTag::TaskNotification];
}

/// One wrapped message: its tag, attribute values (e.g. `teammate_id`,
/// `color`, `summary`) and body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrappedMessage {
    pub tag: WrapTag,
    pub attrs: Vec<(String, String)>,
    pub body: String,
}

impl WrappedMessage {
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

fn parse_attrs(s: &str) -> Option<Vec<(String, String)>> {
    let mut attrs = Vec::new();
    let mut rest = s.trim();
    while !rest.is_empty() {
        let eq = rest.find('=')?;
        let name = rest[..eq].trim();
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b':') {
            return None;
        }
        let after = rest[eq + 1..].trim_start();
        let quote = after.chars().next()?;
        if quote != '"' && quote != '\'' {
            return None;
        }
        let close = after[1..].find(quote)? + 1;
        attrs.push((name.to_owned(), after[1..close].to_owned()));
        rest = after[close + 1..].trim_start();
    }
    Some(attrs)
}

/// Parse a user text that consists only of wrapped messages. `None` when any
/// part of it is not a wrapper (the text is then plain).
pub fn parse_wrapped(text: &str) -> Option<Vec<WrappedMessage>> {
    let mut out = Vec::new();
    let mut rest = text.trim();
    while !rest.is_empty() {
        let tag = WrapTag::ALL.into_iter().find(|t| {
            rest.strip_prefix('<').and_then(|r| r.strip_prefix(t.name())).is_some_and(|r| r.starts_with(['>', ' ', '\n', '\t']))
        })?;
        let open_end = rest.find('>')?;
        let attrs = parse_attrs(&rest[1 + tag.name().len()..open_end])?;
        let close = format!("</{}>", tag.name());
        let body_start = open_end + 1;
        let close_at = rest[body_start..].find(&close)? + body_start;
        let body = rest[body_start..close_at].trim_matches('\n').to_owned();
        out.push(WrappedMessage { tag, attrs, body });
        rest = rest[close_at + close.len()..].trim_start();
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// The team protocol frame kind of a wrapped body that is a JSON object with
/// a `type` (§5.2.4, §7.2.4); `None` for plain text.
pub fn protocol_frame_kind(body: &str) -> Option<MessageKind> {
    let t = body.trim();
    if !t.starts_with('{') {
        return None;
    }
    let v: Value = serde_json::from_str(t).ok()?;
    let ty = v.get("type")?.as_str()?;
    Some(match ty {
        "idle_notification" => MessageKind::IdleNotification,
        "shutdown_request" => MessageKind::ShutdownRequest,
        "shutdown_approved" => MessageKind::ShutdownApproved,
        "shutdown_rejected" => MessageKind::ShutdownRejected,
        "shutdown_response" => {
            if v.get("approve").and_then(Value::as_bool).unwrap_or(false) {
                MessageKind::ShutdownApproved
            } else {
                MessageKind::ShutdownRejected
            }
        }
        "plan_approval_request" => MessageKind::PlanApprovalRequest,
        "plan_approval_response" => MessageKind::PlanApprovalResponse,
        "permission_request" => MessageKind::PermissionRequest,
        "permission_response" => MessageKind::PermissionResponse,
        "task_assignment" => MessageKind::TaskAssignment,
        "teammate_terminated" => MessageKind::TeammateTerminated,
        _ => MessageKind::Unknown,
    })
}

/// A block kind plus whether a field was cut.
pub type KindOut = (BlockKind, bool);

/// The blocks of one user text: the interrupt marker, wrapped messages, or a
/// plain `UserText{default_origin}`.
pub fn user_text_kinds(text: &str, default_origin: UserOrigin) -> Vec<KindOut> {
    if is_interrupt_marker(text) {
        return vec![(BlockKind::System { note: SystemNote::Interrupted, text: None }, false)];
    }
    if let Some(wrapped) = parse_wrapped(text) {
        return wrapped
            .into_iter()
            .map(|w| {
                let member = w
                    .attr("teammate_id")
                    .or_else(|| w.attr("from"))
                    .or_else(|| w.attr("agent_id"))
                    .map(|m| display(m, AGENT_LABEL_CAP));
                match protocol_frame_kind(&w.body) {
                    Some(kind) => (BlockKind::System { note: SystemNote::TeamEvent { message_kind: kind, member }, text: None }, false),
                    None => {
                        let s = sanitize_display_text(&w.body, USER_TEXT_CAP);
                        (BlockKind::UserText { text: s.text, origin: w.tag.origin(), delivery: None, client_msg_id: None }, s.truncated)
                    }
                }
            })
            .collect();
    }
    let s = sanitize_display_text(text, USER_TEXT_CAP);
    vec![(BlockKind::UserText { text: s.text, origin: default_origin, delivery: None, client_msg_id: None }, s.truncated)]
}

// ---------------------------------------------------------------------------
// the helper
// ---------------------------------------------------------------------------

fn tool_call_kind(item: &Value, base: PathBase<'_>) -> KindOut {
    let name = item.get("name").and_then(Value::as_str).unwrap_or("");
    let input = item.get("input").unwrap_or(&Value::Null);
    let id = tool_use_id(item.get("id").and_then(Value::as_str).unwrap_or(""));
    let preview = tools::input_preview(name, input);
    let kind = BlockKind::ToolCall {
        tool_use_id: id,
        summary: tools::claude_tool_summary(name, input, base),
        input_preview: preview.text,
        status: ToolStatus::Running,
        result_preview: None,
        file_change: tools::claude_file_change(name, input, base),
        incomplete: false,
    };
    (kind, preview.truncated)
}

/// The block kinds of one content item (several only for wrapped user text).
pub fn item_kinds(item: &Value, role: &str, base: PathBase<'_>) -> Vec<KindOut> {
    let ty = item.get("type").and_then(Value::as_str).unwrap_or("");
    match ty {
        "text" => {
            let text = item.get("text").and_then(Value::as_str).unwrap_or("");
            if role == "user" {
                user_text_kinds(text, UserOrigin::Human)
            } else if text.trim().is_empty() {
                Vec::new()
            } else {
                let s = sanitize_display_text(text, ASSISTANT_TEXT_CAP);
                vec![(BlockKind::AssistantText { markdown: s.text, streaming: false, agent: None }, s.truncated)]
            }
        }
        "thinking" => {
            let s = item.get("thinking").and_then(Value::as_str).map(|t| sanitize_display_text(t, THINKING_CAP));
            let truncated = s.as_ref().is_some_and(|s| s.truncated);
            let text = s.map(|s| s.text).filter(|t| !t.is_empty());
            vec![(BlockKind::Thinking { seconds: None, text }, truncated)]
        }
        "redacted_thinking" => vec![(BlockKind::Thinking { seconds: None, text: None }, false)],
        "tool_use" | "server_tool_use" | "mcp_tool_use" => vec![tool_call_kind(item, base)],
        t if is_tool_result_type(t) => Vec::new(),
        other => {
            let name = if other.is_empty() { "unknown" } else { other };
            vec![(BlockKind::Unknown { type_name: display(name, TYPE_NAME_CAP) }, false)]
        }
    }
}

fn is_tool_result_type(t: &str) -> bool {
    t == "tool_result" || t.ends_with("_tool_result")
}

/// Map the `message` object of an `assistant`/`user` frame or transcript row
/// to blocks. Unknown content types become `Unknown{type_name}`; every string
/// passes `sanitize_display_text`.
///
/// `seq` is `ctx.next_seq + i` for the i-th returned block; the ContentHub
/// re-sequences blocks per session view, so callers outside it may ignore it.
pub fn claude_blocks(message: &serde_json::Value, ids: BlockIdScheme<'_>, ctx: &RenderContext) -> Vec<Block> {
    let base = ctx.path_base();
    let content = message.get("content").unwrap_or(&Value::Null);
    let role = message.get("role").and_then(Value::as_str).unwrap_or(match content {
        Value::String(_) => "user",
        _ => "assistant",
    });
    let mut kinds: Vec<(usize, usize, KindOut)> = Vec::new();
    match content {
        Value::String(text) => {
            let item = serde_json::json!({"type": "text", "text": text});
            for (j, k) in item_kinds(&item, role, base).into_iter().enumerate() {
                kinds.push((0, j, k));
            }
        }
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                for (j, k) in item_kinds(item, role, base).into_iter().enumerate() {
                    kinds.push((i, j, k));
                }
            }
        }
        _ => {}
    }
    kinds
        .into_iter()
        .enumerate()
        .map(|(k, (i, j, (kind, truncated)))| {
            let id = match ids {
                BlockIdScheme::Stream { message_id } if j == 0 => stream_block_id(message_id, i),
                BlockIdScheme::Stream { message_id } => block_id(&format!("{message_id}#{i}.{j}")),
                BlockIdScheme::Transcript { row_uuid } => transcript_block_id(row_uuid, k),
            };
            Block { id, seq: ctx.next_seq.saturating_add(k as u64), at: ctx.at, kind, truncated }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// tool results
// ---------------------------------------------------------------------------

/// A `tool_result` item, as an update of the `ToolCall` block it answers.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolResultUpdate {
    pub tool_use_id: String,
    pub status: ToolStatus,
    pub preview: Option<String>,
    pub truncated: bool,
}

/// The `tool_result` items of a `user` message (and server tool results of an
/// assistant message), in order.
pub fn claude_tool_results(message: &Value) -> Vec<ToolResultUpdate> {
    let Some(items) = message.get("content").and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str).is_some_and(is_tool_result_type))
        .filter_map(|item| {
            let id = item.get("tool_use_id").and_then(Value::as_str)?;
            let status = if item.get("is_error").and_then(Value::as_bool).unwrap_or(false) {
                ToolStatus::Error
            } else {
                ToolStatus::Ok
            };
            let preview = tools::result_preview(item.get("content").unwrap_or(&Value::Null));
            Some(ToolResultUpdate {
                tool_use_id: tool_use_id(id),
                status,
                preview: Some(preview.text).filter(|p| !p.is_empty()),
                truncated: preview.truncated,
            })
        })
        .collect()
}

/// Apply a tool result to a `ToolCall` block (other kinds are left alone).
/// `file_change` and `verb` refine the call from the result (`toolUseResult`).
pub fn apply_tool_result(block: &mut Block, update: &ToolResultUpdate, file_change: Option<FileChange>, verb: Option<ToolVerb>) {
    if let BlockKind::ToolCall { status, result_preview, file_change: fc, summary, incomplete, .. } = &mut block.kind {
        *status = update.status;
        *result_preview = update.preview.clone();
        *incomplete = false;
        if file_change.is_some() {
            *fc = file_change;
        }
        if let Some(v) = verb {
            summary.verb = v;
        }
        block.truncated |= update.truncated;
    }
}

/// The `ToolCall` block's tool name and summary, if it is one.
pub fn tool_call_of(block: &Block) -> Option<(&str, &ToolSummary)> {
    match &block.kind {
        BlockKind::ToolCall { tool_use_id, summary, .. } => Some((tool_use_id.as_str(), summary)),
        _ => None,
    }
}

/// The UTF-8 size of a block's content (the hub's 16 MiB budget, §6.1.8).
pub fn content_bytes(block: &Block) -> usize {
    let base = 64 + block.id.as_str().len();
    base + match &block.kind {
        BlockKind::UserText { text, .. } => text.len(),
        BlockKind::AssistantText { markdown, agent, .. } => markdown.len() + agent.as_ref().map_or(0, String::len),
        BlockKind::Thinking { text, .. } => text.as_ref().map_or(0, String::len),
        BlockKind::ToolCall { tool_use_id, summary, input_preview, result_preview, file_change, .. } => {
            tool_use_id.len()
                + summary.tool.len()
                + summary.object.as_ref().map_or(0, String::len)
                + input_preview.len()
                + result_preview.as_ref().map_or(0, String::len)
                + file_change.as_ref().map_or(0, |f| f.path_rel.len())
        }
        BlockKind::Interaction { request_id, .. } => request_id.as_str().len(),
        BlockKind::System { text, .. } => text.as_ref().map_or(0, String::len),
        BlockKind::Unknown { type_name } => type_name.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx() -> RenderContext {
        RenderContext { location: AbsPath::parse("/w/p").ok(), home: AbsPath::parse("/Users/u").ok(), at: 1_000, next_seq: 7 }
    }

    #[test]
    fn assistant_items_map_to_blocks() {
        let m = json!({"id": "msg_1", "role": "assistant", "content": [
            {"type": "thinking", "thinking": "hmm"},
            {"type": "text", "text": "Hi \u{1b}[1mthere\u{1b}[0m"},
            {"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {"file_path": "/w/p/a.rs"}},
            {"type": "future_kind"}
        ]});
        let b = claude_blocks(&m, BlockIdScheme::Stream { message_id: "msg_1" }, &ctx());
        assert_eq!(b.len(), 4);
        assert_eq!(b[0].id.as_str(), "msg_1#0");
        assert_eq!(b[1].id.as_str(), "msg_1#1");
        assert_eq!(b[1].seq, 8);
        assert!(matches!(&b[1].kind, BlockKind::AssistantText { markdown, .. } if markdown == "Hi there"));
        assert!(matches!(&b[2].kind, BlockKind::ToolCall { summary, status: ToolStatus::Running, .. } if summary.object.as_deref() == Some("a.rs")));
        assert!(matches!(&b[3].kind, BlockKind::Unknown { type_name } if type_name == "future_kind"));
    }

    #[test]
    fn transcript_ids_are_row_uuids() {
        let m = json!({"role": "assistant", "content": [{"type": "text", "text": "x"}]});
        let b = claude_blocks(&m, BlockIdScheme::Transcript { row_uuid: "8e0c-uuid" }, &ctx());
        assert_eq!(b[0].id.as_str(), "8e0c-uuid");
    }

    #[test]
    fn user_text_variants() {
        let m = json!({"role": "user", "content": "[Request interrupted by user]"});
        let b = claude_blocks(&m, BlockIdScheme::Transcript { row_uuid: "u1" }, &ctx());
        assert!(matches!(b[0].kind, BlockKind::System { note: SystemNote::Interrupted, .. }));

        let text = "<teammate-message teammate_id=\"team-lead\" color=\"blue\">\nDo the thing\n</teammate-message>\n<teammate-message teammate_id=\"team-lead\">{\"type\":\"idle_notification\",\"from\":\"x\"}</teammate-message>";
        let m = json!({"role": "user", "content": [{"type": "text", "text": text}]});
        let b = claude_blocks(&m, BlockIdScheme::Transcript { row_uuid: "u2" }, &ctx());
        assert_eq!(b.len(), 2);
        assert_eq!(b[1].id.as_str(), "u2#1");
        assert!(matches!(&b[0].kind, BlockKind::UserText { text, origin: UserOrigin::Teammate, .. } if text == "Do the thing"));
        assert!(matches!(&b[1].kind, BlockKind::System { note: SystemNote::TeamEvent { message_kind: MessageKind::IdleNotification, member: Some(m) }, text: None } if m == "team-lead"));
    }

    #[test]
    fn tool_results_update_calls() {
        let m = json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "is_error": true, "content": "boom"}]});
        assert!(claude_blocks(&m, BlockIdScheme::Transcript { row_uuid: "u3" }, &ctx()).is_empty());
        let r = claude_tool_results(&m);
        assert_eq!(r[0].status, ToolStatus::Error);
        assert_eq!(r[0].preview.as_deref(), Some("boom"));
    }

    #[test]
    fn hostile_ids_are_hashed() {
        assert!(block_id(&"x".repeat(200)).as_str().starts_with("h_"));
        assert!(block_id("a\u{7}b").as_str().starts_with("h_"));
        assert_eq!(tool_use_id("toolu_01"), "toolu_01");
        assert!(tool_use_id("bad id").starts_with("h_"));
    }
}
