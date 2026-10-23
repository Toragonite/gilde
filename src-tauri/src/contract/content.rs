//! Content types — memory only, never persisted or logged (contract §3.7) —
//! and the content channels (§4.2.4).

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::contract::activity::ErrorSub;
use crate::contract::common::{EpochMs, Notice};
use crate::contract::ids::{BlockId, ClientMsgId, RequestId, SubId};
use crate::contract::interaction::InteractionKind;
use crate::contract::team::MessageKind;

/// One conversation block (§3.7.1). Caps (§3.7.2): text beyond a field cap is
/// cut at a grapheme boundary with `…` and `truncated: true`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct Block {
    pub id: BlockId,
    /// Monotonically increasing per session view.
    #[ts(type = "number")]
    pub seq: u64,
    #[ts(type = "number")]
    pub at: EpochMs,
    pub kind: BlockKind,
    /// A field of this block was cut at its cap (§3.7.2).
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum BlockKind {
    UserText {
        /// ≤ 256 KiB
        text: String,
        origin: UserOrigin,
        /// From `command_lifecycle` (§6.1.3).
        delivery: Option<Delivery>,
        client_msg_id: Option<ClientMsgId>,
    },
    AssistantText {
        /// ≤ 1 MiB
        markdown: String,
        streaming: bool,
        /// Subagent.
        agent: Option<String>,
    },
    Thinking {
        seconds: Option<u32>,
        /// ≤ 64 KiB, hidden by default.
        text: Option<String>,
    },
    ToolCall {
        tool_use_id: String,
        summary: ToolSummary,
        /// ≤ 4 KiB
        input_preview: String,
        status: ToolStatus,
        /// ≤ 8 KiB
        result_preview: Option<String>,
        file_change: Option<FileChange>,
        /// Stream cut before the input closed [R4 §3].
        incomplete: bool,
    },
    /// The contract's `kind: InteractionKind` field collides with the `kind`
    /// tag, so it is spelled `interactionKind` on the wire.
    Interaction {
        request_id: RequestId,
        interaction_kind: InteractionKind,
        outcome: InteractionOutcome,
    },
    System {
        note: SystemNote,
        text: Option<String>,
    },
    Unknown {
        /// ≤ 64
        type_name: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum UserOrigin {
    Human,
    Peer,
    Teammate,
    TaskNotification,
    Gilde,
}

/// From `command_lifecycle` (§6.1.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum Delivery {
    Queued,
    Delivered,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum ToolStatus {
    Running,
    Ok,
    Error,
    Denied,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum InteractionOutcome {
    Pending,
    Allowed,
    AllowedForSession,
    Denied,
    Answered,
    Withdrawn,
    SessionEnded,
    /// The session was stopped before this was answered and resumed without a
    /// pending request: shown with `card.abandonedOnResume` (§6.1.6, F-05)
    /// [A19.2].
    Abandoned,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum SystemNote {
    CompactBoundary,
    ApiRetry { attempt: u32 },
    PermissionDenied { tool: String },
    Interrupted,
    BackgroundTask { status: BackgroundTaskStatus },
    /// A team protocol frame shown in a conversation (§7.2.4).
    TeamEvent {
        message_kind: MessageKind,
        member: Option<String>,
    },
    Error { sub: ErrorSub },
    RowTooLarge,
    NewConversation,
}

/// `system/task_*` and `background_tasks_changed` (§6.1.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum BackgroundTaskStatus {
    Started,
    Running,
    Completed,
    Failed,
    Stopped,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct FileChange {
    pub path_rel: String,
    pub added: u32,
    pub removed: u32,
    pub change: ChangeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum ChangeKind {
    Modified,
    Added,
    Deleted,
}

/// ToolSummary (§3.7.3): computed in Rust; one table, all vendors. Paths are
/// relative to the session's location; outside it absolute with `~` for home.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct ToolSummary {
    /// The tool name as the vendor reported it (≤ 64; MCP server names are
    /// untrusted text).
    pub tool: String,
    pub verb: ToolVerb,
    /// The object column of §3.7.3 (path, first command line ≤ 200, pattern
    /// ≤ 120, host, agent/recipient, task id); `None` for `ask`.
    pub object: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum ToolVerb {
    Read,
    Edit,
    Create,
    Run,
    Search,
    Reach,
    Delegate,
    Message,
    Task,
    Ask,
    Use,
}

// ---------------------------------------------------------------------------
// content channels (§4.2.4)
// ---------------------------------------------------------------------------

/// One event on a `Channel<ConvoEnvelope>`; `seq` strictly increasing per
/// subscription.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct ConvoEnvelope {
    pub sub_id: SubId,
    #[ts(type = "number")]
    pub seq: u64,
    pub event: ConvoEvent,
}

/// The first event is always `Reset`. A subscription 256 events behind gets
/// `Reset{Lagging}` (complete blocks are never lost; deltas are). `TextDelta`
/// events are coalesced at 50 ms (≤ 16 KiB per event); `Finalized` replaces the
/// streamed text with the authoritative block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum ConvoEvent {
    Reset {
        blocks: Vec<Block>,
        has_older: bool,
        reason: ResetReason,
    },
    Appended {
        block: Block,
    },
    Updated {
        block_id: BlockId,
        patch: BlockPatch,
    },
    Older {
        blocks: Vec<Block>,
        has_older: bool,
    },
    /// e.g. "Updates arrive when each step completes · last row 10:42".
    Source {
        source: ConvoSourceKind,
        note: Option<Notice>,
    },
    Evicted,
    Ended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum ResetReason {
    Initial,
    Truncated,
    Replaced,
    Lagging,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum BlockPatch {
    TextDelta {
        append: String,
    },
    ToolResult {
        status: ToolStatus,
        preview: Option<String>,
    },
    Interaction {
        outcome: InteractionOutcome,
    },
    Finalized {
        block: Box<Block>,
    },
}

/// Where a conversation view's content comes from (`convo_subscribe.source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum ConvoSourceKind {
    Stream,
    Transcript,
    TurnMemory,
    None,
}

/// One event on a `Channel<PtyChunk>`: bytes by ring offset, a resync when the
/// view fell behind, or the notice that another view took the PTY (§6.2.6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum PtyChunk {
    Data {
        /// Ring offset of the first byte.
        #[ts(type = "number")]
        offset: u64,
        b64: String,
    },
    Resync {
        #[ts(type = "number")]
        offset: u64,
    },
    Detached {
        reason: DetachReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum DetachReason {
    OtherView,
}
