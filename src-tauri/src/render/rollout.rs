//! Codex rollout rows → blocks and structure facts (contract §6.4, §5.3).
//!
//! Owner: T04.
//!
//! A rollout is `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`:
//! `{timestamp, type, payload}` rows [R2 Q2]. Content comes from `event_msg`
//! rows only (`response_item` rows carry the same turn for the model API and
//! would duplicate it):
//!
//! | `event_msg.payload.type` | Block |
//! |---|---|
//! | `item_completed` + `UserMessage` | `UserText{Human}` |
//! | `item_completed` + `AgentMessage` | `AssistantText` |
//! | `item_completed` + `Reasoning` | `Thinking` |
//! | `item_completed` + `CommandExecution` / `FileChange` / `McpToolCall` / `WebSearch` | `ToolCall` (complete) |
//! | `turn_aborted` | `System{Interrupted}` |
//! | legacy `user_message` / `agent_message` / `agent_reasoning` | as above |
//!
//! `task_started`/`task_complete`/`turn_aborted` are the turn boundaries of
//! the structure facts. Item type names are accepted in PascalCase, camelCase
//! and snake_case. Every block id is the row's byte offset (`o<offset>`):
//! stable for the file and unique, whatever the item ids look like.

use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::Value;

use crate::contract::common::EpochMs;
use crate::contract::content::{Block, BlockKind, SystemNote, ToolStatus, UserOrigin};
use crate::render::blocks::{transcript_block_id, ASSISTANT_TEXT_CAP, THINKING_CAP, TYPE_NAME_CAP, USER_TEXT_CAP};
use crate::render::sanitize::{display, sanitize_display_text};
use crate::render::tools::{self, PathBase, INPUT_PREVIEW_CAP, RESULT_PREVIEW_CAP};
use crate::render::transcript::{line_text, offset_id, parse_rfc3339_ms, ContentBase, ContentItem, LineOutcome, RowFacts, TurnEndKind};

/// Rollout row types that are never content.
pub const META_ROLLOUT_TYPES: &[&str] = &["session_meta", "turn_context", "response_item", "world_state", "token_usage_record", "compacted"];

fn norm(t: &str) -> String {
    t.chars().filter(|c| *c != '_').collect::<String>().to_ascii_lowercase()
}

#[derive(Deserialize)]
struct LiteRollout<'a> {
    #[serde(rename = "type", default)]
    ty: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(borrow, default)]
    payload: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct LitePayload<'a> {
    #[serde(rename = "type", default)]
    ty: Option<String>,
    #[serde(borrow, default)]
    item: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct LiteItem {
    #[serde(rename = "type", default)]
    ty: Option<String>,
}

/// The structure facts of one rollout line, plus its outcome.
pub fn rollout_facts(bytes: &[u8]) -> (Option<RowFacts>, LineOutcome) {
    let text = match line_text(bytes) {
        Ok(t) => t,
        Err(o) => return (None, o),
    };
    let Ok(row) = serde_json::from_str::<LiteRollout<'_>>(&text) else {
        return (None, LineOutcome::Unparseable);
    };
    let ty = row.ty.unwrap_or_default();
    let mut f = RowFacts { row_type: ty.clone(), at: row.timestamp.as_deref().and_then(parse_rfc3339_ms), ..RowFacts::default() };
    let outcome = match ty.as_str() {
        "event_msg" => {
            if let Some(p) = row.payload.and_then(|p| serde_json::from_str::<LitePayload<'_>>(p.get()).ok()) {
                let pty = p.ty.unwrap_or_default();
                f.row_type = format!("event_msg/{pty}");
                match pty.as_str() {
                    "task_complete" => f.turn_end = Some(TurnEndKind::Ok),
                    "turn_aborted" => f.turn_end = Some(TurnEndKind::Interrupted),
                    "user_message" => f.turn_open = Some(UserOrigin::Human),
                    "item_completed" => {
                        let item_ty = p.item.and_then(|i| serde_json::from_str::<LiteItem>(i.get()).ok()).and_then(|i| i.ty);
                        if item_ty.as_deref().map(norm).as_deref() == Some("usermessage") {
                            f.turn_open = Some(UserOrigin::Human);
                        }
                    }
                    _ => {}
                }
            }
            LineOutcome::Content
        }
        t if META_ROLLOUT_TYPES.contains(&t) => LineOutcome::Meta,
        t => LineOutcome::UnknownType(t.chars().take(64).collect()),
    };
    (Some(f), outcome)
}

fn text_of(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|i| i.as_str().or_else(|| i.get("text").and_then(Value::as_str)))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn first_present<'v>(v: &'v Value, keys: &[&str]) -> Option<&'v Value> {
    keys.iter().find_map(|k| v.get(*k).filter(|x| !x.is_null()))
}

fn status_of(item: &Value) -> ToolStatus {
    let status = item.get("status").and_then(Value::as_str).map(norm);
    let exit = first_present(item, &["exit_code", "exitCode"]).and_then(Value::as_i64);
    match status.as_deref() {
        Some("declined" | "rejected" | "denied") => ToolStatus::Denied,
        Some("failed" | "error") => ToolStatus::Error,
        Some("inprogress" | "running") => ToolStatus::Running,
        _ => match exit {
            Some(0) => ToolStatus::Ok,
            Some(_) => ToolStatus::Error,
            None if status.as_deref() == Some("completed") => ToolStatus::Ok,
            None => ToolStatus::Unknown,
        },
    }
}

/// The block kind (and truncation) of a completed codex item.
pub fn item_kind(item: &Value, item_id: &str, base: PathBase<'_>) -> Option<(BlockKind, bool)> {
    let ty = item.get("type").and_then(Value::as_str).unwrap_or("");
    Some(match norm(ty).as_str() {
        "usermessage" => {
            let text = text_of(first_present(item, &["content", "text", "message"]));
            let s = sanitize_display_text(&text, USER_TEXT_CAP);
            (BlockKind::UserText { text: s.text, origin: UserOrigin::Human, delivery: None, client_msg_id: None }, s.truncated)
        }
        "agentmessage" => {
            let text = text_of(first_present(item, &["text", "content", "message"]));
            if text.trim().is_empty() {
                return None;
            }
            let s = sanitize_display_text(&text, ASSISTANT_TEXT_CAP);
            (BlockKind::AssistantText { markdown: s.text, streaming: false, agent: None }, s.truncated)
        }
        "reasoning" => {
            let text = text_of(first_present(item, &["summary", "content", "text"]));
            let s = sanitize_display_text(&text, THINKING_CAP);
            let truncated = s.truncated;
            (BlockKind::Thinking { seconds: None, text: Some(s.text).filter(|t| !t.is_empty()) }, truncated)
        }
        "commandexecution" | "filechange" | "mcptoolcall" | "websearch" => {
            let summary = tools::codex_tool_summary(ty, item, base);
            let input = match norm(ty).as_str() {
                "commandexecution" => match item.get("command") {
                    Some(Value::Array(parts)) => {
                        sanitize_display_text(&parts.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "), INPUT_PREVIEW_CAP)
                    }
                    Some(Value::String(s)) => sanitize_display_text(s, INPUT_PREVIEW_CAP),
                    _ => Default::default(),
                },
                "filechange" => sanitize_display_text(
                    &tools::codex_changes_in(item, base).iter().map(|c| c.path_rel.as_str()).collect::<Vec<_>>().join("\n"),
                    INPUT_PREVIEW_CAP,
                ),
                "mcptoolcall" => first_present(item, &["arguments", "args"]).map(|a| tools::bounded_json(a, INPUT_PREVIEW_CAP)).unwrap_or_default(),
                _ => sanitize_display_text(&text_of(item.get("query")), INPUT_PREVIEW_CAP),
            };
            let result = match norm(ty).as_str() {
                "commandexecution" => first_present(item, &["aggregated_output", "aggregatedOutput", "output"]).map(|o| match o {
                    Value::String(s) => sanitize_display_text(s, RESULT_PREVIEW_CAP),
                    other => tools::bounded_json(other, RESULT_PREVIEW_CAP),
                }),
                "mcptoolcall" => first_present(item, &["result", "error"]).map(|r| tools::result_preview(r.get("content").unwrap_or(r))),
                _ => None,
            };
            let truncated = input.truncated || result.as_ref().is_some_and(|r| r.truncated);
            let file_change = (norm(ty) == "filechange").then(|| tools::codex_changes_in(item, base).into_iter().next()).flatten();
            (
                BlockKind::ToolCall {
                    tool_use_id: display(item_id, TYPE_NAME_CAP),
                    summary,
                    input_preview: input.text,
                    status: status_of(item),
                    result_preview: result.map(|r| r.text).filter(|t| !t.is_empty()),
                    file_change,
                    incomplete: false,
                },
                truncated,
            )
        }
        other => (BlockKind::Unknown { type_name: display(if other.is_empty() { "item" } else { ty }, TYPE_NAME_CAP) }, false),
    })
}

/// The content items of one rollout line.
pub fn rollout_content(bytes: &[u8], offset: u64, fallback_at: EpochMs, base: &ContentBase) -> Vec<ContentItem> {
    let Ok(text) = line_text(bytes) else { return Vec::new() };
    let Ok(row) = serde_json::from_str::<Value>(&text) else { return Vec::new() };
    if row.get("type").and_then(Value::as_str) != Some("event_msg") {
        return Vec::new();
    }
    let at = row.get("timestamp").and_then(Value::as_str).and_then(parse_rfc3339_ms).unwrap_or(fallback_at);
    let Some(payload) = row.get("payload") else { return Vec::new() };
    let id = offset_id(offset);
    let pbase = PathBase { location: base.location.as_ref().map(|p| p.as_str()), home: base.home.as_ref().map(|p| p.as_str()) };
    let kind = match payload.get("type").and_then(Value::as_str).unwrap_or("") {
        "item_completed" => payload.get("item").and_then(|item| {
            let item_id = item.get("id").and_then(Value::as_str).map_or_else(|| id.clone(), str::to_owned);
            item_kind(item, &item_id, pbase)
        }),
        "user_message" => {
            let s = sanitize_display_text(&text_of(payload.get("message")), USER_TEXT_CAP);
            Some((BlockKind::UserText { text: s.text, origin: UserOrigin::Human, delivery: None, client_msg_id: None }, s.truncated))
        }
        "agent_message" => {
            let s = sanitize_display_text(&text_of(payload.get("message")), ASSISTANT_TEXT_CAP);
            (!s.text.trim().is_empty()).then_some((BlockKind::AssistantText { markdown: s.text, streaming: false, agent: None }, s.truncated))
        }
        "agent_reasoning" => {
            let s = sanitize_display_text(&text_of(payload.get("text")), THINKING_CAP);
            let truncated = s.truncated;
            Some((BlockKind::Thinking { seconds: None, text: Some(s.text).filter(|t| !t.is_empty()) }, truncated))
        }
        "turn_aborted" => Some((BlockKind::System { note: SystemNote::Interrupted, text: None }, false)),
        _ => None,
    };
    kind.map(|(kind, truncated)| vec![ContentItem::Block(Block { id: transcript_block_id(&id, 0), seq: 0, at, kind, truncated })])
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(payload: Value) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({"timestamp": "2026-10-22T01:00:00.000Z", "type": "event_msg", "payload": payload})).unwrap()
    }

    #[test]
    fn items_map_to_blocks() {
        let base = ContentBase::default();
        let b = rollout_content(&row(serde_json::json!({"type": "item_completed", "item": {"type": "AgentMessage", "id": "m1", "text": "done"}})), 10, 0, &base);
        assert!(matches!(&b[0], ContentItem::Block(Block { kind: BlockKind::AssistantText { markdown, .. }, .. }) if markdown == "done"));
        let b = rollout_content(
            &row(serde_json::json!({"type": "item_completed", "item": {"type": "CommandExecution", "id": "c1", "command": ["bash", "-lc", "ls"], "aggregated_output": "a\nb", "exit_code": 0}})),
            20,
            0,
            &base,
        );
        let ContentItem::Block(b) = &b[0] else { panic!() };
        assert_eq!(b.id.as_str(), "o20");
        assert!(matches!(&b.kind, BlockKind::ToolCall { status: ToolStatus::Ok, result_preview: Some(r), .. } if r == "a\nb"));
        let (f, o) = rollout_facts(&row(serde_json::json!({"type": "task_complete"})));
        assert_eq!(o, LineOutcome::Content);
        assert_eq!(f.unwrap().turn_end, Some(TurnEndKind::Ok));
    }
}
