//! The Claude stream-json frame → block normalizer (contract §6.1.5).
//!
//! Owner: T05. Uses T04's shared helper (`render::blocks`) so the stream and
//! the transcript follower map content the same way.
//!
//! **Ids** (§3.2 `BlockId`, F-06): every stream block is
//! `stream_block_id(message.id, content_block_start.index)` — `claude_blocks`
//! numbers by array position, but an `assistant` frame carries usually ONE
//! block whose stream index is not in the frame, so the index is recovered
//! from the open `content_block_start` of that message (the `assistant` frame
//! arrives right after the block's deltas and before its `content_block_stop`
//! [R6 §3 LP-01]); without partial messages it is the k-th `assistant` frame of
//! that message. User texts Gilde sent use the `clientMsgId`; other user rows
//! the frame `uuid`; System lines a stable key (one line per background task,
//! one collapsed retry line per turn).
//!
//! **Lifecycle of a streamed block**: `content_block_start` → `Appended`
//! (`streaming: true`, empty); `text_delta`/`thinking_delta` →
//! `Updated{TextDelta}` (the ContentHub coalesces at 50 ms); `input_json_delta`
//! → the `ToolCall` input preview grows (≤ 4 KiB, re-published in 512-byte
//! steps); the `assistant` frame → `Appended` with the same id, which the hub
//! applies as `Finalized`. A message or turn that ends with a block never
//! finalized (a cut stream) finalizes it from what streamed: text with
//! `streaming: false`, a tool call with `incomplete: true`.
//!
//! Tool results are previewed lazily (§6.1.8): a `tool_result.content` above
//! 256 KiB is never parsed into a value tree.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, VecDeque};

use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::Value;

use crate::contract::common::{AbsPath, EpochMs};
use crate::contract::content::{
    BackgroundTaskStatus, Block, BlockKind, BlockPatch, ConvoSourceKind, Delivery, ImageRef, InteractionOutcome,
    SystemNote, ToolStatus, UserOrigin,
};
use crate::contract::display::{Sanitized, TextCap};
use crate::contract::ids::{BlockId, ClientMsgId};
use crate::contract::interaction::InteractionKind;
use crate::contract::services::ContentUpdate;
use crate::render::blocks::{
    apply_tool_result, block_id, item_kinds, stream_block_id, tool_use_id, user_text_kinds, ToolResultUpdate,
    AGENT_LABEL_CAP, ASSISTANT_TEXT_CAP, SYSTEM_TEXT_CAP, THINKING_CAP, USER_TEXT_CAP,
};
use crate::render::images;
use crate::render::sanitize::{display, sanitize_display_text};
use crate::render::tools::{self, PathBase, INPUT_PREVIEW_CAP, RESULT_PREVIEW_CAP};
use crate::render::transcript::TokenUsage;

/// Tool calls remembered for their results.
const MAX_TOOL_REFS: usize = 512;
/// User blocks Gilde sent, remembered for delivery updates.
const MAX_USER_REFS: usize = 256;
/// A tool input kept for result refinement (file change of a Write/Edit).
const MAX_KEPT_INPUT: usize = 256 * 1024;
/// A tool result parsed into a value tree only up to this size.
const MAX_PARSED_RESULT: usize = 256 * 1024;
/// A structured `tool_use_result` parsed only up to this size.
const MAX_STRUCTURED_RESULT: usize = 1024 * 1024;
/// The input preview is re-published in steps of this many bytes.
const INPUT_STEP: usize = 512;

// ---------------------------------------------------------------------------
// typed stream events
// ---------------------------------------------------------------------------

/// `message.usage` / `message_delta.usage`.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct UsageIn {
    #[serde(default)]
    pub input_tokens: Option<u64>,
    #[serde(default)]
    pub output_tokens: Option<u64>,
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    pub cache_read_input_tokens: Option<u64>,
}

impl UsageIn {
    /// T04's mapping input (uncached / cache-creation / cache-read / output).
    pub fn tokens(&self) -> TokenUsage {
        TokenUsage {
            input: self.input_tokens.unwrap_or(0),
            output: self.output_tokens.unwrap_or(0),
            cache_creation: self.cache_creation_input_tokens.unwrap_or(0),
            cache_read: self.cache_read_input_tokens.unwrap_or(0),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct EventMessageIn {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub usage: Option<UsageIn>,
}

#[derive(Debug, Deserialize)]
pub struct DeltaIn<'a> {
    #[serde(rename = "type", borrow, default)]
    pub ty: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    pub text: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    pub thinking: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    pub partial_json: Option<Cow<'a, str>>,
}

#[derive(Debug, Deserialize)]
pub struct ContentBlockIn {
    #[serde(rename = "type", default)]
    pub ty: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

/// One `stream_event.event`.
#[derive(Debug, Deserialize)]
pub struct EventIn<'a> {
    #[serde(rename = "type", borrow, default)]
    pub ty: Option<Cow<'a, str>>,
    #[serde(default)]
    pub index: Option<usize>,
    #[serde(default)]
    pub message: Option<EventMessageIn>,
    #[serde(default)]
    pub content_block: Option<ContentBlockIn>,
    #[serde(borrow, default)]
    pub delta: Option<DeltaIn<'a>>,
    #[serde(default)]
    pub usage: Option<UsageIn>,
}

/// The `message` of an `assistant` frame.
#[derive(Debug, Deserialize)]
pub struct AssistantMsgIn {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub content: Option<Vec<Value>>,
    #[serde(default)]
    pub usage: Option<UsageIn>,
}

#[derive(Debug, Deserialize)]
struct UserMsgIn<'a> {
    #[serde(borrow, default)]
    content: Option<&'a RawValue>,
}

#[derive(Debug, Deserialize)]
struct UserItemIn<'a> {
    #[serde(rename = "type", borrow, default)]
    ty: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    text: Option<Cow<'a, str>>,
    #[serde(default)]
    tool_use_id: Option<String>,
    #[serde(default)]
    is_error: Option<bool>,
    #[serde(borrow, default)]
    content: Option<&'a RawValue>,
}

#[derive(Debug, Deserialize)]
struct TextPartIn<'a> {
    #[serde(rename = "type", borrow, default)]
    ty: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    text: Option<Cow<'a, str>>,
}

// ---------------------------------------------------------------------------
// renderer state
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum OpenKind {
    Text { acc: String },
    Thinking { acc: String },
    Tool { tool_use_id: String, name: String, json: String, published: usize },
    Other,
}

#[derive(Debug)]
struct OpenBlock {
    id: BlockId,
    kind: OpenKind,
    finalized: bool,
    at: EpochMs,
}

#[derive(Debug, Clone)]
struct ToolRef {
    block: Block,
    name: String,
    input: Option<Value>,
}

/// What an `assistant` frame told the session besides its blocks.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AssistantFacts {
    pub message_id: Option<String>,
    /// `(tool_use_id, tool name, Bash command)` of every `tool_use` block.
    pub tool_uses: Vec<(String, String, Option<String>)>,
}

/// What a `user` frame told the session besides its blocks.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UserFacts {
    /// `tool_use_id`s of the tool results.
    pub tool_results: Vec<String>,
    /// The `"[Request interrupted by user…]"` marker.
    pub interrupt_marker: bool,
}

/// One session's stream renderer. Memory only; bounded.
#[derive(Debug)]
pub struct StreamRenderer {
    location: Option<AbsPath>,
    home: Option<AbsPath>,
    current_msg: Option<String>,
    open: BTreeMap<usize, OpenBlock>,
    /// Assistant frames seen per message id (fallback index without partials).
    frames_of: HashMap<String, usize>,
    frames_order: VecDeque<String>,
    tools: HashMap<String, ToolRef>,
    tools_order: VecDeque<String>,
    users: HashMap<String, Block>,
    users_order: VecDeque<String>,
    agent_labels: HashMap<String, String>,
    counter: u64,
}

impl StreamRenderer {
    pub fn new(location: Option<AbsPath>, home: Option<AbsPath>) -> StreamRenderer {
        StreamRenderer {
            location,
            home,
            current_msg: None,
            open: BTreeMap::new(),
            frames_of: HashMap::new(),
            frames_order: VecDeque::new(),
            tools: HashMap::new(),
            tools_order: VecDeque::new(),
            users: HashMap::new(),
            users_order: VecDeque::new(),
            agent_labels: HashMap::new(),
            counter: 0,
        }
    }

    fn base(&self) -> PathBase<'_> {
        PathBase { location: self.location.as_ref().map(AbsPath::as_str), home: self.home.as_ref().map(AbsPath::as_str) }
    }

    fn block(id: BlockId, at: EpochMs, kind: BlockKind, truncated: bool) -> Block {
        // `seq` is assigned by the ContentHub (§4.2.4).
        Block { id, seq: 0, at, kind, truncated }
    }

    fn next_key(&mut self, prefix: &str) -> BlockId {
        self.counter += 1;
        block_id(&format!("{prefix}{}", self.counter))
    }

    /// The first update of every Stream producer (T04's producer protocol).
    pub fn source() -> ContentUpdate {
        ContentUpdate::Source { source: ConvoSourceKind::Stream, note: None }
    }

    /// The message being streamed (`message_start` … `message_stop`).
    pub fn current_message(&self) -> Option<&str> {
        self.current_msg.as_deref()
    }

    /// The subagent label of a delegating `Task`/`Agent` call; `None` when
    /// that call is unknown (no English placeholder in content, A19.7).
    pub fn agent_label(&self, parent_tool_use_id: &str) -> Option<String> {
        self.agent_labels.get(parent_tool_use_id).cloned()
    }

    // -- user texts Gilde sent ---------------------------------------------------

    /// The block of a message Gilde wrote to stdin (id = `clientMsgId`).
    pub fn user_sent(&mut self, id: &ClientMsgId, text: &str, images: Vec<ImageRef>, at: EpochMs, delivery: Option<Delivery>) -> Vec<ContentUpdate> {
        let s = sanitize_display_text(text, USER_TEXT_CAP);
        let b = Self::block(
            block_id(id.as_str()),
            at,
            BlockKind::UserText { text: s.text, origin: UserOrigin::Human, delivery, client_msg_id: Some(id.clone()), images },
            s.truncated,
        );
        if self.users.insert(id.as_str().to_owned(), b.clone()).is_none() {
            self.users_order.push_back(id.as_str().to_owned());
        }
        while self.users_order.len() > MAX_USER_REFS {
            if let Some(old) = self.users_order.pop_front() {
                self.users.remove(&old);
            }
        }
        vec![ContentUpdate::Appended(b)]
    }

    /// Whether this id is one of Gilde's own user blocks.
    pub fn is_ours(&self, uuid: &str) -> bool {
        self.users.contains_key(uuid)
    }

    /// The delivery chip of a message Gilde sent (`command_lifecycle`, §6.1.3).
    pub fn delivery(&mut self, id: &str, d: Delivery) -> Vec<ContentUpdate> {
        let Some(b) = self.users.get_mut(id) else { return Vec::new() };
        if let BlockKind::UserText { delivery, .. } = &mut b.kind {
            if *delivery == Some(d) || *delivery == Some(Delivery::Cancelled) {
                return Vec::new();
            }
            *delivery = Some(d);
        }
        vec![ContentUpdate::Updated { block_id: b.id.clone(), patch: BlockPatch::Finalized { block: Box::new(b.clone()) } }]
    }

    /// The current delivery state of one of Gilde's messages.
    pub fn delivery_of(&self, id: &str) -> Option<Delivery> {
        match &self.users.get(id)?.kind {
            BlockKind::UserText { delivery, .. } => *delivery,
            _ => None,
        }
    }

    // -- stream events -------------------------------------------------------------

    /// One `stream_event`. Usage is read by the caller from the same `ev`.
    pub fn stream_event(&mut self, ev: &EventIn<'_>, at: EpochMs) -> Vec<ContentUpdate> {
        let mut out = Vec::new();
        match ev.ty.as_deref() {
            Some("message_start") => {
                out.extend(self.close_open(at));
                self.current_msg = ev.message.as_ref().and_then(|m| m.id.clone()).filter(|id| !id.is_empty());
            }
            Some("content_block_start") => {
                let (Some(msg), Some(index), Some(cb)) = (self.current_msg.clone(), ev.index, ev.content_block.as_ref()) else {
                    return out;
                };
                let id = stream_block_id(&msg, index);
                let (kind, open) = match cb.ty.as_deref() {
                    Some("text") => (
                        Some(BlockKind::AssistantText { markdown: String::new(), streaming: true, agent: None }),
                        OpenKind::Text { acc: String::new() },
                    ),
                    Some("thinking") => (Some(BlockKind::Thinking { seconds: None, text: Some(String::new()) }), OpenKind::Thinking { acc: String::new() }),
                    Some("tool_use" | "server_tool_use" | "mcp_tool_use") => {
                        let name = cb.name.clone().unwrap_or_default();
                        let tuid = tool_use_id(cb.id.as_deref().unwrap_or(""));
                        (
                            Some(BlockKind::ToolCall {
                                tool_use_id: tuid.clone(),
                                summary: tools::claude_tool_summary(&name, &Value::Null, self.base()),
                                input_preview: String::new(),
                                status: ToolStatus::Running,
                                result_preview: None,
                                result_images: Vec::new(),
                                file_change: None,
                                incomplete: false,
                            }),
                            OpenKind::Tool { tool_use_id: tuid, name, json: String::new(), published: 0 },
                        )
                    }
                    _ => (None, OpenKind::Other),
                };
                if let Some(kind) = kind {
                    out.push(ContentUpdate::Appended(Self::block(id.clone(), at, kind, false)));
                }
                self.open.insert(index, OpenBlock { id, kind: open, finalized: false, at });
            }
            Some("content_block_delta") => {
                let (Some(index), Some(delta)) = (ev.index, ev.delta.as_ref()) else { return out };
                let Some(ob) = self.open.get_mut(&index) else { return out };
                if ob.finalized {
                    return out;
                }
                match (&mut ob.kind, delta.ty.as_deref()) {
                    (OpenKind::Text { acc }, Some("text_delta")) => {
                        let t = delta.text.as_deref().unwrap_or("");
                        if !t.is_empty() {
                            push_capped(acc, t, 1024 * 1024);
                            out.push(ContentUpdate::Updated { block_id: ob.id.clone(), patch: BlockPatch::TextDelta { append: t.to_owned() } });
                        }
                    }
                    (OpenKind::Thinking { acc }, Some("thinking_delta")) => {
                        let t = delta.thinking.as_deref().unwrap_or("");
                        if !t.is_empty() {
                            push_capped(acc, t, 64 * 1024);
                            out.push(ContentUpdate::Updated { block_id: ob.id.clone(), patch: BlockPatch::TextDelta { append: t.to_owned() } });
                        }
                    }
                    (OpenKind::Tool { json, published, tool_use_id: tuid, name }, Some("input_json_delta")) => {
                        let t = delta.partial_json.as_deref().unwrap_or("");
                        if json.len() < 4 * 1024 {
                            push_capped(json, t, 4 * 1024);
                            if json.len() >= *published + INPUT_STEP || json.len() >= 4 * 1024 {
                                *published = json.len();
                                let s = sanitize_display_text(json, INPUT_PREVIEW_CAP);
                                let kind = BlockKind::ToolCall {
                                    tool_use_id: tuid.clone(),
                                    summary: tools::claude_tool_summary(name, &Value::Null, PathBase::default()),
                                    input_preview: s.text,
                                    status: ToolStatus::Running,
                                    result_preview: None,
                                    result_images: Vec::new(),
                                    file_change: None,
                                    incomplete: false,
                                };
                                out.push(ContentUpdate::Appended(Self::block(ob.id.clone(), ob.at, kind, s.truncated)));
                            }
                        }
                    }
                    _ => {}
                }
            }
            Some("message_stop") => {
                out.extend(self.close_open(at));
            }
            _ => {}
        }
        out
    }

    /// Finalize what streamed but never got its `assistant` frame.
    pub fn close_open(&mut self, at: EpochMs) -> Vec<ContentUpdate> {
        let mut out = Vec::new();
        for (_, ob) in std::mem::take(&mut self.open) {
            if ob.finalized {
                continue;
            }
            match ob.kind {
                OpenKind::Text { acc } => {
                    let s = sanitize_display_text(&acc, ASSISTANT_TEXT_CAP);
                    out.push(ContentUpdate::Appended(Self::block(
                        ob.id,
                        at,
                        BlockKind::AssistantText { markdown: s.text, streaming: false, agent: None },
                        s.truncated,
                    )));
                }
                OpenKind::Thinking { acc } => {
                    let s = sanitize_display_text(&acc, THINKING_CAP);
                    out.push(ContentUpdate::Appended(Self::block(
                        ob.id,
                        at,
                        BlockKind::Thinking { seconds: None, text: Some(s.text).filter(|t| !t.is_empty()) },
                        s.truncated,
                    )));
                }
                OpenKind::Tool { tool_use_id: tuid, name, json, .. } => {
                    let s = sanitize_display_text(&json, INPUT_PREVIEW_CAP);
                    out.push(ContentUpdate::Appended(Self::block(
                        ob.id,
                        at,
                        BlockKind::ToolCall {
                            tool_use_id: tuid,
                            summary: tools::claude_tool_summary(&name, &Value::Null, self.base()),
                            input_preview: s.text,
                            status: ToolStatus::Running,
                            result_preview: None,
                            result_images: Vec::new(),
                            file_change: None,
                            incomplete: true,
                        },
                        s.truncated,
                    )));
                }
                OpenKind::Other => {}
            }
        }
        out
    }

    // -- assistant frames ------------------------------------------------------------

    fn stream_index(&mut self, msg: &str, item_type: &str, tuid: Option<&str>) -> usize {
        let streamed = self.current_msg.as_deref() == Some(msg);
        if streamed {
            let pick = self.open.iter().find(|(_, ob)| {
                !ob.finalized
                    && match (&ob.kind, item_type) {
                        (OpenKind::Text { .. }, "text") => true,
                        (OpenKind::Thinking { .. }, "thinking" | "redacted_thinking") => true,
                        (OpenKind::Tool { tool_use_id: t, .. }, "tool_use" | "server_tool_use" | "mcp_tool_use") => {
                            tuid.is_none_or(|id| tool_use_id(id) == *t)
                        }
                        _ => false,
                    }
            });
            if let Some((&i, _)) = pick {
                if let Some(ob) = self.open.get_mut(&i) {
                    ob.finalized = true;
                }
                return i;
            }
        }
        let n = self.frames_of.entry(msg.to_owned()).or_insert(0);
        let i = *n;
        *n += 1;
        if *n == 1 {
            self.frames_order.push_back(msg.to_owned());
            while self.frames_order.len() > 64 {
                if let Some(old) = self.frames_order.pop_front() {
                    self.frames_of.remove(&old);
                }
            }
        }
        i
    }

    /// One complete `assistant` frame (usually one block).
    pub fn assistant(&mut self, msg: &AssistantMsgIn, parent: Option<&str>, at: EpochMs) -> (Vec<ContentUpdate>, AssistantFacts) {
        let mut out = Vec::new();
        let mut facts = AssistantFacts { message_id: msg.id.clone(), tool_uses: Vec::new() };
        let Some(mid) = msg.id.clone().filter(|m| !m.is_empty()) else { return (out, facts) };
        let agent = parent.and_then(|p| self.agent_label(p)).map(|l| display(&l, AGENT_LABEL_CAP));
        let items = msg.content.as_deref().unwrap_or(&[]);
        for item in items {
            let ty = item.get("type").and_then(Value::as_str).unwrap_or("");
            let raw_tuid = item.get("id").and_then(Value::as_str);
            let idx = self.stream_index(&mid, ty, raw_tuid);
            let base = self.base();
            for (j, (mut kind, truncated)) in item_kinds(item, "assistant", base).into_iter().enumerate() {
                let id = if j == 0 { stream_block_id(&mid, idx) } else { block_id(&format!("{mid}#{idx}.{j}")) };
                if let BlockKind::AssistantText { agent: a, .. } = &mut kind {
                    a.clone_from(&agent);
                }
                if let BlockKind::ToolCall { tool_use_id: tuid, .. } = &kind {
                    let name = item.get("name").and_then(Value::as_str).unwrap_or("").to_owned();
                    let command = item.get("input").and_then(|i| i.get("command")).and_then(Value::as_str).map(str::to_owned);
                    facts.tool_uses.push((tuid.clone(), name.clone(), command));
                    if matches!(name.as_str(), "Task" | "Agent") {
                        if let Some(d) = item.get("input").and_then(|i| i.get("description")).and_then(Value::as_str) {
                            self.agent_labels.insert(tuid.clone(), display(d, AGENT_LABEL_CAP));
                            if self.agent_labels.len() > MAX_TOOL_REFS {
                                self.agent_labels.clear();
                            }
                        }
                    }
                    let input = item.get("input").filter(|i| serde_json::to_string(i).map(|s| s.len() <= MAX_KEPT_INPUT).unwrap_or(false)).cloned();
                    let b = Self::block(id.clone(), at, kind.clone(), truncated);
                    self.remember_tool(tuid.clone(), ToolRef { block: b, name, input });
                }
                out.push(ContentUpdate::Appended(Self::block(id, at, kind, truncated)));
            }
        }
        (out, facts)
    }

    fn remember_tool(&mut self, tuid: String, r: ToolRef) {
        if self.tools.insert(tuid.clone(), r).is_none() {
            self.tools_order.push_back(tuid);
        }
        while self.tools_order.len() > MAX_TOOL_REFS {
            if let Some(old) = self.tools_order.pop_front() {
                self.tools.remove(&old);
            }
        }
    }

    // -- user frames ---------------------------------------------------------------------

    /// A `user` frame that is not the replay of one of Gilde's messages: tool
    /// results update their calls; texts become blocks (the interrupt marker
    /// a `System{Interrupted}` line).
    pub fn user(&mut self, message: &RawValue, uuid: Option<&str>, tool_use_result: Option<&RawValue>, at: EpochMs) -> (Vec<ContentUpdate>, UserFacts) {
        let mut out = Vec::new();
        let mut facts = UserFacts::default();
        let Ok(m) = serde_json::from_str::<UserMsgIn<'_>>(message.get()) else { return (out, facts) };
        let Some(content) = m.content else { return (out, facts) };
        let items: Vec<UserItemIn<'_>> = if content.get().trim_start().starts_with('"') {
            match serde_json::from_str::<String>(content.get()) {
                Ok(t) => vec![UserItemIn { ty: Some(Cow::Borrowed("text")), text: Some(Cow::Owned(t)), tool_use_id: None, is_error: None, content: None }],
                Err(_) => return (out, facts),
            }
        } else {
            match serde_json::from_str(content.get()) {
                Ok(v) => v,
                Err(_) => return (out, facts),
            }
        };
        let structured: Option<Value> = tool_use_result
            .filter(|r| r.get().len() <= MAX_STRUCTURED_RESULT)
            .and_then(|r| serde_json::from_str(r.get()).ok());
        let mut texts: Vec<String> = Vec::new();
        for item in &items {
            match item.ty.as_deref() {
                Some(t) if t == "tool_result" || t.ends_with("_tool_result") => {
                    let Some(raw_id) = item.tool_use_id.as_deref() else { continue };
                    let tuid = tool_use_id(raw_id);
                    facts.tool_results.push(tuid.clone());
                    let preview = item.content.map(raw_preview).unwrap_or_default();
                    let status = if item.is_error.unwrap_or(false) { ToolStatus::Error } else { ToolStatus::Ok };
                    let update = ToolResultUpdate {
                        tool_use_id: tuid.clone(),
                        status,
                        preview: Some(preview.text).filter(|p| !p.is_empty()),
                        images: item.content.map(|c| images::raw_images(c, images::global())).unwrap_or_default(),
                        truncated: preview.truncated,
                    };
                    let base = self.base();
                    if let Some(r) = self.tools.get(&tuid) {
                        let mut b = r.block.clone();
                        let (fc, verb) = match (&structured, &r.input) {
                            (Some(s), Some(input)) => (tools::refine_file_change(&r.name, input, s, base), tools::refine_verb(&r.name, s)),
                            (Some(s), None) => (None, tools::refine_verb(&r.name, s)),
                            _ => (None, None),
                        };
                        apply_tool_result(&mut b, &update, fc, verb);
                        b.at = at;
                        out.push(ContentUpdate::Appended(b));
                        self.tools.remove(&tuid);
                    }
                }
                Some("text") => {
                    if let Some(t) = item.text.as_deref() {
                        texts.push(t.to_owned());
                    }
                }
                _ => {}
            }
        }
        // The message's own images (an attached screenshot) go with its text.
        let mut own_images = images::raw_images(content, images::global());
        if texts.is_empty() && !own_images.is_empty() {
            texts.push(String::new());
        }
        if !texts.is_empty() {
            let joined = texts.join("\n");
            let base_id = uuid.filter(|u| !u.is_empty()).map(str::to_owned).unwrap_or_else(|| {
                self.counter += 1;
                format!("u_{}", self.counter)
            });
            for (k, (kind, truncated)) in user_text_kinds(&joined, UserOrigin::Human).into_iter().enumerate() {
                if matches!(kind, BlockKind::System { note: SystemNote::Interrupted, .. }) {
                    facts.interrupt_marker = true;
                }
                let id = if k == 0 { block_id(&base_id) } else { block_id(&format!("{base_id}#{k}")) };
                let mut kind = kind;
                if let BlockKind::UserText { images, .. } = &mut kind {
                    images.append(&mut own_images);
                }
                out.push(ContentUpdate::Appended(Self::block(id, at, kind, truncated)));
            }
        }
        (out, facts)
    }

    /// The replay of a user frame Gilde did not send (no matching uuid): shown
    /// as a user text once.
    pub fn foreign_replay(&mut self, message: &RawValue, uuid: Option<&str>, at: EpochMs) -> Vec<ContentUpdate> {
        if uuid.is_some_and(|u| self.users.contains_key(u)) {
            return Vec::new();
        }
        self.user(message, uuid, None, at).0
    }

    // -- system lines, interactions ------------------------------------------------------

    /// A System line; `key` makes it replaceable (one line per task, one
    /// collapsed retry line).
    pub fn system(&mut self, key: Option<&str>, note: SystemNote, text: Option<&str>, at: EpochMs) -> ContentUpdate {
        let id = match key {
            Some(k) => block_id(k),
            None => self.next_key("sys_"),
        };
        let (text, truncated) = match text {
            Some(t) => {
                let s = sanitize_display_text(t, SYSTEM_TEXT_CAP);
                (Some(s.text).filter(|t| !t.is_empty()), s.truncated)
            }
            None => (None, false),
        };
        ContentUpdate::Appended(Self::block(id, at, BlockKind::System { note, text }, truncated))
    }

    /// One line per background task (`system/task_*`, §6.1.5).
    pub fn task_line(&mut self, task_id: &str, status: BackgroundTaskStatus, description: Option<&str>, at: EpochMs) -> ContentUpdate {
        let key = format!("task_{}", tool_use_id(task_id));
        self.system(Some(&key), SystemNote::BackgroundTask { status }, description, at)
    }

    /// The Interaction block of a card (id `ix_<request_id>`).
    pub fn interaction(&self, request_id: &str, kind: InteractionKind, outcome: InteractionOutcome, at: EpochMs) -> ContentUpdate {
        ContentUpdate::Appended(Self::block(
            interaction_block_id(request_id),
            at,
            BlockKind::Interaction {
                request_id: crate::contract::ids::RequestId::parse(request_id.to_owned())
                    .unwrap_or_else(|_| crate::contract::ids::RequestId::parse("unknown").expect("valid")),
                interaction_kind: kind,
                outcome,
            },
            false,
        ))
    }

    /// The outcome of a card changed (answered, withdrawn, ended).
    pub fn interaction_outcome(&self, request_id: &str, outcome: InteractionOutcome) -> ContentUpdate {
        ContentUpdate::Updated { block_id: interaction_block_id(request_id), patch: BlockPatch::Interaction { outcome } }
    }
}

/// The block id of a card's Interaction block.
pub fn interaction_block_id(request_id: &str) -> BlockId {
    block_id(&format!("ix_{request_id}"))
}

fn push_capped(acc: &mut String, t: &str, cap: usize) {
    if acc.len() >= cap {
        return;
    }
    let room = cap - acc.len();
    if t.len() <= room {
        acc.push_str(t);
    } else {
        let mut end = room;
        while end > 0 && !t.is_char_boundary(end) {
            end -= 1;
        }
        acc.push_str(&t[..end]);
    }
}

/// The preview of a `tool_result.content` without materializing a large
/// value tree (§6.1.8): small contents go through T04's `result_preview`;
/// a large string is decoded once and cut by the sanitizer; a large array
/// reads only its text parts.
pub fn raw_preview(raw: &RawValue) -> Sanitized {
    let s = raw.get();
    if s.len() <= MAX_PARSED_RESULT {
        return match serde_json::from_str::<Value>(s) {
            Ok(v) => tools::result_preview(&v),
            Err(_) => Sanitized::default(),
        };
    }
    let t = s.trim_start();
    if t.starts_with('"') {
        return match serde_json::from_str::<String>(s) {
            Ok(text) => sanitize_display_text(&text, RESULT_PREVIEW_CAP),
            Err(_) => Sanitized::default(),
        };
    }
    if t.starts_with('[') {
        if let Ok(parts) = serde_json::from_str::<Vec<TextPartIn<'_>>>(s) {
            let mut text = String::new();
            for p in parts {
                if p.ty.as_deref() == Some("text") {
                    if let Some(x) = p.text {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(&x);
                        if text.len() > 16 * 1024 {
                            break;
                        }
                    }
                }
            }
            let mut out = sanitize_display_text(&text, RESULT_PREVIEW_CAP);
            out.truncated = true;
            return out;
        }
    }
    Sanitized { text: String::new(), truncated: true }
}

/// The `BackgroundTaskStatus` of a task frame's status string.
pub fn task_status(subtype: &str, status: Option<&str>) -> BackgroundTaskStatus {
    match (subtype, status) {
        (_, Some("completed")) => BackgroundTaskStatus::Completed,
        (_, Some("failed" | "error")) => BackgroundTaskStatus::Failed,
        (_, Some("stopped" | "killed" | "cancelled")) => BackgroundTaskStatus::Stopped,
        (_, Some("running" | "in_progress")) => BackgroundTaskStatus::Running,
        ("task_started", None) => BackgroundTaskStatus::Started,
        ("task_progress", None) => BackgroundTaskStatus::Running,
        _ => BackgroundTaskStatus::Unknown,
    }
}

/// A user-visible text cap shortcut for the core.
pub fn short(text: &str, cap: TextCap) -> String {
    display(text, cap)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(v: Value) -> String {
        v.to_string()
    }

    fn run_event(r: &mut StreamRenderer, v: Value) -> Vec<ContentUpdate> {
        let s = ev(v);
        let e: EventIn<'_> = serde_json::from_str(&s).unwrap();
        r.stream_event(&e, 10)
    }

    fn ids(u: &[ContentUpdate]) -> Vec<String> {
        u.iter()
            .map(|x| match x {
                ContentUpdate::Appended(b) => format!("+{}", b.id),
                ContentUpdate::Updated { block_id, .. } => format!("~{block_id}"),
                other => format!("{other:?}"),
            })
            .collect()
    }

    // T-convo-ordering: the assistant frame finalizes the streamed block by
    // its stream index, even when it is not array position 0.
    #[test]
    fn streamed_blocks_are_finalized_by_stream_index() {
        let mut r = StreamRenderer::new(AbsPath::parse("/w/p").ok(), None);
        run_event(&mut r, json!({"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":2}}}));
        let u = run_event(&mut r, json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking"}}));
        assert_eq!(ids(&u), ["+msg_1#0"]);
        let a: AssistantMsgIn = serde_json::from_value(json!({"id":"msg_1","content":[{"type":"thinking","thinking":"hmm"}]})).unwrap();
        let (u, _) = r.assistant(&a, None, 11);
        assert_eq!(ids(&u), ["+msg_1#0"]);
        run_event(&mut r, json!({"type":"content_block_stop","index":0}));
        run_event(&mut r, json!({"type":"content_block_start","index":1,"content_block":{"type":"text"}}));
        let u = run_event(&mut r, json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Hel"}}));
        assert!(matches!(&u[0], ContentUpdate::Updated { patch: BlockPatch::TextDelta { append }, .. } if append == "Hel"));
        let a: AssistantMsgIn = serde_json::from_value(json!({"id":"msg_1","content":[{"type":"text","text":"Hello"}]})).unwrap();
        let (u, _) = r.assistant(&a, None, 12);
        assert_eq!(ids(&u), ["+msg_1#1"], "array position 0, stream index 1");
        // Without partial messages the k-th frame of a message is index k.
        let a: AssistantMsgIn = serde_json::from_value(json!({"id":"msg_2","content":[{"type":"text","text":"a"}]})).unwrap();
        assert_eq!(ids(&r.assistant(&a, None, 13).0), ["+msg_2#0"]);
        let a: AssistantMsgIn = serde_json::from_value(json!({"id":"msg_2","content":[{"type":"tool_use","id":"toolu_9","name":"Bash","input":{"command":"ls"}}]})).unwrap();
        let (u, f) = r.assistant(&a, None, 14);
        assert_eq!(ids(&u), ["+msg_2#1"]);
        assert_eq!(f.tool_uses, vec![("toolu_9".to_string(), "Bash".to_string(), Some("ls".to_string()))]);
    }

    #[test]
    fn a_cut_tool_input_is_marked_incomplete() {
        let mut r = StreamRenderer::new(None, None);
        run_event(&mut r, json!({"type":"message_start","message":{"id":"msg_1"}}));
        run_event(&mut r, json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"Bash"}}));
        run_event(&mut r, json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"comm"}}));
        let u = r.close_open(20);
        match &u[0] {
            ContentUpdate::Appended(b) => match &b.kind {
                BlockKind::ToolCall { incomplete, input_preview, .. } => {
                    assert!(*incomplete);
                    assert_eq!(input_preview, "{\"comm");
                }
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn tool_results_update_the_call_with_its_file_change() {
        let mut r = StreamRenderer::new(AbsPath::parse("/w/p").ok(), None);
        let a: AssistantMsgIn = serde_json::from_value(json!({"id":"msg_1","content":[{"type":"tool_use","id":"toolu_1","name":"Write","input":{"file_path":"/w/p/a.txt","content":"x\ny\n"}}]})).unwrap();
        r.assistant(&a, None, 1);
        let msg = RawValue::from_string(json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"ok"}]}).to_string()).unwrap();
        let tur = RawValue::from_string(json!({"type":"create","filePath":"/w/p/a.txt","structuredPatch":[]}).to_string()).unwrap();
        let (u, f) = r.user(&msg, Some("u-1"), Some(&tur), 2);
        assert_eq!(f.tool_results, vec!["toolu_1".to_string()]);
        match &u[0] {
            ContentUpdate::Appended(b) => match &b.kind {
                BlockKind::ToolCall { status, result_preview, file_change, .. } => {
                    assert_eq!(*status, ToolStatus::Ok);
                    assert_eq!(result_preview.as_deref(), Some("ok"));
                    assert_eq!(file_change.as_ref().unwrap().path_rel, "a.txt");
                }
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn images_in_a_tool_result_and_in_a_user_message_are_shown() {
        let png = "iVBORw0KGgo=";
        let image = json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":png}});
        let mut r = StreamRenderer::new(None, None);
        let a: AssistantMsgIn = serde_json::from_value(json!({"id":"msg_1","content":[{"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"/w/p/shot.png"}}]})).unwrap();
        r.assistant(&a, None, 1);
        let msg = RawValue::from_string(
            json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":[image.clone()]}]}).to_string(),
        )
        .unwrap();
        let (u, _) = r.user(&msg, Some("u-1"), None, 2);
        let shown = |u: &[ContentUpdate]| match &u[0] {
            ContentUpdate::Appended(Block { kind: BlockKind::ToolCall { result_images, .. }, .. }) => result_images.clone(),
            ContentUpdate::Appended(Block { kind: BlockKind::UserText { images, .. }, .. }) => images.clone(),
            other => panic!("{other:?}"),
        };
        let result = shown(&u);
        assert_eq!(result.len(), 1, "the screenshot the tool returned");
        let (media_type, data) = images::global().get(&result[0].id).expect("stored");
        assert_eq!((media_type, &*data), ("image/png", png));

        let msg = RawValue::from_string(json!({"role":"user","content":[image, {"type":"text","text":"what is this?"}]}).to_string()).unwrap();
        let (u, _) = r.user(&msg, Some("u-2"), None, 3);
        assert_eq!(shown(&u).len(), 1, "an attached image goes with its text");
        let id = ClientMsgId::parse("0192f000-0000-4000-8000-000000000009").unwrap();
        let sent = r.user_sent(&id, "", result.clone(), 4, None);
        assert_eq!(shown(&sent), result, "a sent image shows on the sent block");
    }

    #[test]
    fn the_interrupt_marker_and_subagent_labels() {
        let mut r = StreamRenderer::new(None, None);
        let msg = RawValue::from_string(json!({"role":"user","content":[{"type":"text","text":"[Request interrupted by user for tool use]"}]}).to_string()).unwrap();
        let (u, f) = r.user(&msg, Some("u-2"), None, 1);
        assert!(f.interrupt_marker);
        assert!(matches!(&u[0], ContentUpdate::Appended(Block { kind: BlockKind::System { note: SystemNote::Interrupted, .. }, .. })));
        let a: AssistantMsgIn = serde_json::from_value(json!({"id":"msg_1","content":[{"type":"tool_use","id":"toolu_a","name":"Agent","input":{"description":"Create lp07-d.txt file","prompt":"…"}}]})).unwrap();
        r.assistant(&a, None, 2);
        let sub: AssistantMsgIn = serde_json::from_value(json!({"id":"msg_s","content":[{"type":"text","text":"done"}]})).unwrap();
        let (u, _) = r.assistant(&sub, Some("toolu_a"), 3);
        assert!(matches!(&u[0], ContentUpdate::Appended(Block { kind: BlockKind::AssistantText { agent: Some(a), .. }, .. }) if a == "Create lp07-d.txt file"));
        let (u, _) = r.assistant(&sub, Some("toolu_unknown"), 4);
        assert!(matches!(&u[0], ContentUpdate::Appended(Block { kind: BlockKind::AssistantText { agent: None, .. }, .. })), "unknown delegating call: no label");
    }

    #[test]
    fn large_tool_results_are_previewed_without_a_value_tree() {
        let big = "x".repeat(MAX_PARSED_RESULT + 10);
        let raw = RawValue::from_string(serde_json::to_string(&big).unwrap()).unwrap();
        let p = raw_preview(&raw);
        assert!(p.truncated);
        assert!(p.text.len() <= 8 * 1024 + 4);
        let arr = RawValue::from_string(json!([{"type":"text","text": big}]).to_string()).unwrap();
        assert!(raw_preview(&arr).truncated);
    }

    #[test]
    fn deliveries_update_our_user_block() {
        let mut r = StreamRenderer::new(None, None);
        let id = ClientMsgId::parse("0192f000-0000-4000-8000-000000000000").unwrap();
        let u = r.user_sent(&id, "hi", Vec::new(), 1, None);
        assert_eq!(ids(&u), ["+0192f000-0000-4000-8000-000000000000"]);
        assert_eq!(r.delivery(id.as_str(), Delivery::Queued).len(), 1);
        assert_eq!(r.delivery(id.as_str(), Delivery::Queued).len(), 0, "no change, no update");
        assert_eq!(r.delivery_of(id.as_str()), Some(Delivery::Queued));
        assert_eq!(r.delivery(id.as_str(), Delivery::Cancelled).len(), 1);
        assert_eq!(r.delivery(id.as_str(), Delivery::Delivered).len(), 0, "cancelled is final");
        assert!(r.is_ours(id.as_str()));
    }
}
