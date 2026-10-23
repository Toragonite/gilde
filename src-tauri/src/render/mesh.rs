//! Agent-mesh turn events → content updates (contract §6.3, §4.5
//! `turn.event`): the turn-based mode's renderer, for T08's `TurnDriver`.
//!
//! Owner: T04.
//!
//! | `TurnEvent` | Effect |
//! |---|---|
//! | `Text{delta}` | `AssistantText{streaming:true}` appended, then `TextDelta`s (one text block per run of text) |
//! | `Tool{name, detailPreview}` | the open text is finalized; `ToolCall{status: Unknown}` (no results exist, R1 §4) |
//! | `Usage`, `Session` | — (usage and identity are the driver's) |
//! | `Done{ok\|incomplete}` | the open text is finalized |
//! | `Done{stopped}` | finalized + `System{Interrupted}` ("Stopped. The conversation can continue.") |
//! | `Error{class}` | finalized + `System{Error{sub}}`: auth → SignInNeeded, quota → UsageLimit, exec/unknown → AgentError |
//!
//! Ids use the stream scheme with the turn id as the message id
//! (`<turnId>#<k>`), so a turn-memory view never mixes schemes (F-06). Text
//! is sanitized per delta; an escape sequence split across two deltas is
//! carried over (`incomplete_escape_start`) and removed whole. A text block
//! stops growing at 1 MiB (`truncated`).

use crate::contract::activity::ErrorSub;
use crate::contract::common::EpochMs;
use crate::contract::content::{Block, BlockKind, BlockPatch, Delivery, SystemNote, ToolStatus, UserOrigin};
use crate::contract::display::TextCap;
use crate::contract::error::VendorErrorClass;
use crate::contract::ids::ClientMsgId;
use crate::contract::services::ContentUpdate;
use crate::contract::sidecar::{TurnDoneStatus, TurnEvent};
use crate::render::blocks::{stream_block_id, ASSISTANT_TEXT_CAP, SYSTEM_TEXT_CAP, USER_TEXT_CAP};
use crate::render::sanitize::{incomplete_escape_start, sanitize_display_text};
use crate::render::tools::{self, INPUT_PREVIEW_CAP};

/// A carried escape prefix longer than this is dropped (it is not an escape
/// sequence any terminal would wait for).
const MAX_CARRY_BYTES: usize = 4096;

/// The text block being streamed.
#[derive(Debug, Clone)]
struct OpenText {
    block: Block,
    carry: String,
    full: bool,
}

/// One turn's renderer (one per `turn.start`).
#[derive(Debug, Clone)]
pub struct MeshTurnRenderer {
    turn_id: String,
    next: usize,
    open: Option<OpenText>,
}

fn sub_of(class: VendorErrorClass) -> ErrorSub {
    match class {
        VendorErrorClass::Auth => ErrorSub::SignInNeeded,
        VendorErrorClass::Quota => ErrorSub::UsageLimit { account_id: None, resets_at: None },
        VendorErrorClass::Exec | VendorErrorClass::Unknown => ErrorSub::AgentError,
    }
}

impl MeshTurnRenderer {
    pub fn new(turn_id: &str) -> MeshTurnRenderer {
        MeshTurnRenderer { turn_id: turn_id.to_owned(), next: 0, open: None }
    }

    fn block(&mut self, kind: BlockKind, truncated: bool, at: EpochMs) -> Block {
        let id = stream_block_id(&self.turn_id, self.next);
        self.next += 1;
        Block { id, seq: 0, at, kind, truncated }
    }

    /// The user's message that starts this turn (sent by Gilde, so delivered).
    pub fn user_prompt(&mut self, text: &str, client_msg_id: Option<ClientMsgId>, at: EpochMs) -> ContentUpdate {
        let s = sanitize_display_text(text, USER_TEXT_CAP);
        let kind = BlockKind::UserText { text: s.text, origin: UserOrigin::Human, delivery: Some(Delivery::Delivered), client_msg_id };
        ContentUpdate::Appended(self.block(kind, s.truncated, at))
    }

    /// Close the open text block (`Finalized` with `streaming: false`).
    pub fn finish_text(&mut self) -> Option<ContentUpdate> {
        let mut open = self.open.take()?;
        if let BlockKind::AssistantText { streaming, .. } = &mut open.block.kind {
            *streaming = false;
        }
        Some(ContentUpdate::Updated { block_id: open.block.id.clone(), patch: BlockPatch::Finalized { block: Box::new(open.block) } })
    }

    fn text(&mut self, delta: &str, at: EpochMs) -> Option<ContentUpdate> {
        let mut raw = match &mut self.open {
            Some(open) => std::mem::take(&mut open.carry),
            None => String::new(),
        };
        raw.push_str(delta);
        let carry = match incomplete_escape_start(&raw) {
            Some(i) if raw.len() - i <= MAX_CARRY_BYTES => raw.split_off(i),
            Some(i) => {
                raw.truncate(i);
                String::new()
            }
            None => String::new(),
        };
        let TextCap::Bytes(cap) = ASSISTANT_TEXT_CAP else { unreachable!("the text cap is in bytes") };
        match &mut self.open {
            Some(open) => {
                open.carry = carry;
                if open.full {
                    return None;
                }
                let BlockKind::AssistantText { markdown, .. } = &mut open.block.kind else { return None };
                let room = cap.saturating_sub(markdown.len());
                let s = sanitize_display_text(&raw, TextCap::Bytes(room));
                if s.truncated {
                    open.full = true;
                    open.block.truncated = true;
                }
                if s.text.is_empty() {
                    return None;
                }
                markdown.push_str(&s.text);
                Some(ContentUpdate::Updated { block_id: open.block.id.clone(), patch: BlockPatch::TextDelta { append: s.text } })
            }
            None => {
                let s = sanitize_display_text(&raw, ASSISTANT_TEXT_CAP);
                if s.text.is_empty() && carry.is_empty() {
                    return None;
                }
                let block = self.block(BlockKind::AssistantText { markdown: s.text, streaming: true, agent: None }, s.truncated, at);
                self.open = Some(OpenText { block: block.clone(), carry, full: s.truncated });
                Some(ContentUpdate::Appended(block))
            }
        }
    }

    /// The content updates of one turn event, in order.
    pub fn on_event(&mut self, event: &TurnEvent, at: EpochMs) -> Vec<ContentUpdate> {
        let mut out = Vec::new();
        match event {
            TurnEvent::Text { delta } => out.extend(self.text(delta, at)),
            TurnEvent::Tool { name, detail_preview } => {
                out.extend(self.finish_text());
                let input = detail_preview.as_deref().map(|d| sanitize_display_text(d, INPUT_PREVIEW_CAP)).unwrap_or_default();
                let id = stream_block_id(&self.turn_id, self.next);
                let kind = BlockKind::ToolCall {
                    tool_use_id: id.as_str().to_owned(),
                    summary: tools::mesh_tool_summary(name, detail_preview.as_deref()),
                    input_preview: input.text,
                    status: ToolStatus::Unknown,
                    result_preview: None,
                    file_change: None,
                    incomplete: false,
                };
                out.push(ContentUpdate::Appended(self.block(kind, input.truncated, at)));
            }
            TurnEvent::Usage { .. } | TurnEvent::Session { .. } => {}
            TurnEvent::Done { status, .. } => {
                out.extend(self.finish_text());
                if *status == TurnDoneStatus::Stopped {
                    out.push(ContentUpdate::Appended(self.block(BlockKind::System { note: SystemNote::Interrupted, text: None }, false, at)));
                }
            }
            TurnEvent::Error { class, message } => {
                out.extend(self.finish_text());
                let s = sanitize_display_text(message, SYSTEM_TEXT_CAP);
                let kind = BlockKind::System { note: SystemNote::Error { sub: sub_of(*class) }, text: Some(s.text).filter(|t| !t.is_empty()) };
                out.push(ContentUpdate::Appended(self.block(kind, s.truncated, at)));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(d: &str) -> TurnEvent {
        TurnEvent::Text { delta: d.to_owned() }
    }

    #[test]
    fn deltas_then_tool_then_done() {
        let mut r = MeshTurnRenderer::new("t1");
        let a = r.on_event(&text("Hel"), 1);
        assert!(matches!(&a[0], ContentUpdate::Appended(b) if b.id.as_str() == "t1#0"));
        let b = r.on_event(&text("lo\u{1b}[3"), 2);
        assert!(matches!(&b[0], ContentUpdate::Updated { patch: BlockPatch::TextDelta { append }, .. } if append == "lo"));
        let c = r.on_event(&text("1mX"), 3);
        assert!(matches!(&c[0], ContentUpdate::Updated { patch: BlockPatch::TextDelta { append }, .. } if append == "X"));
        let d = r.on_event(&TurnEvent::Tool { name: "Bash".into(), detail_preview: Some("ls -la".into()) }, 4);
        assert_eq!(d.len(), 2);
        assert!(matches!(&d[0], ContentUpdate::Updated { patch: BlockPatch::Finalized { block }, .. }
            if matches!(&block.kind, BlockKind::AssistantText { markdown, streaming: false, .. } if markdown == "HelloX")));
        assert!(matches!(&d[1], ContentUpdate::Appended(Block { kind: BlockKind::ToolCall { status: ToolStatus::Unknown, .. }, .. })));
        let e = r.on_event(&TurnEvent::Error { class: VendorErrorClass::Auth, message: "login".into() }, 5);
        assert!(matches!(&e[0], ContentUpdate::Appended(Block { kind: BlockKind::System { note: SystemNote::Error { sub: ErrorSub::SignInNeeded }, .. }, .. })));
    }
}
