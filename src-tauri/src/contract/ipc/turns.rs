//! §4.2.3 Turns and interactions [C12].

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use super::sessions::{uid_req, MAX_PROMPT_BYTES};
use super::{empty_structs, model_rule};
use crate::contract::common::PermissionMode;
use crate::contract::ids::{ClientMsgId, RequestId, SessionUid};
use crate::contract::interaction::{is_option_key, Decision, DecisionKind};
use crate::contract::validate::{self as v, Checked, Invalid, Validate};

empty_structs!(res: SessionSetPermissionModeRes);

uid_req!(
    /// `turn_force_stop`: Conversation only, offered only after 5 s without a
    /// `result` (`REFUSED{tooEarly}` before); fenced SIGTERM — no SIGINT, no SIGKILL.
    TurnForceStopReq,
);

// -- turn_send -----------------------------------------------------------------

/// Routes by `caps.send`. `text` 1..=256 KiB, no NUL. Never answers a pending card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct TurnSendReq {
    pub uid: SessionUid,
    pub client_msg_id: ClientMsgId,
    pub text: String,
    pub priority: SendPriority,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum SendPriority {
    Next,
    Now,
}

impl Validate for TurnSendReq {
    fn validate(&self) -> Checked {
        v::nested("uid", &self.uid)?;
        v::nested("clientMsgId", &self.client_msg_id)?;
        v::text_range("text", &self.text, 1, MAX_PROMPT_BYTES)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum SendDelivery {
    Sent,
    Queued,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct TurnSendRes {
    pub delivery: SendDelivery,
    pub position: Option<u32>,
}

// -- turn_cancel_queued ----------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct TurnCancelQueuedReq {
    pub uid: SessionUid,
    pub client_msg_id: ClientMsgId,
}

impl Validate for TurnCancelQueuedReq {
    fn validate(&self) -> Checked {
        v::nested("uid", &self.uid)?;
        v::nested("clientMsgId", &self.client_msg_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct TurnCancelQueuedRes {
    pub cancelled: bool,
}

// -- turn_interrupt ------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct TurnInterruptReq {
    pub uid: SessionUid,
    pub discard_queued: bool,
}

impl Validate for TurnInterruptReq {
    fn validate(&self) -> Checked {
        v::nested("uid", &self.uid)
    }
}

/// Conversation → `interrupting`; Terminal: Esc → `sent`; TurnBased:
/// `turn.cancel` → `stopped` [ux-ia §4.8].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum InterruptOutcome {
    Interrupting,
    Sent,
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct TurnInterruptRes {
    pub outcome: InterruptOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct TurnForceStopRes {
    pub stopped: bool,
}

// -- turn_retry ------------------------------------------------------------------

/// Error·Agent error primary action: resend the last user text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct TurnRetryReq {
    pub uid: SessionUid,
    pub client_msg_id: ClientMsgId,
}

impl Validate for TurnRetryReq {
    fn validate(&self) -> Checked {
        v::nested("uid", &self.uid)?;
        v::nested("clientMsgId", &self.client_msg_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct TurnRetryRes {
    pub delivery: SendDelivery,
}

// -- session_set_model, session_set_permission_mode --------------------------------

/// `model: null` (or `""`) = the vendor default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct SessionSetModelReq {
    pub uid: SessionUid,
    pub model: Option<String>,
}

impl Validate for SessionSetModelReq {
    fn validate(&self) -> Checked {
        v::nested("uid", &self.uid)?;
        model_rule("model", &self.model)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum AppliesFrom {
    NextRequest,
    NextTurn,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct SessionSetModelRes {
    pub applies_from: AppliesFrom,
}

/// Conversation only; `BypassPermissions` → `REFUSED{needsLaunchFlag}` unless
/// launched with it [R4 §1.9].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct SessionSetPermissionModeReq {
    pub uid: SessionUid,
    pub mode: PermissionMode,
}

impl Validate for SessionSetPermissionModeReq {
    fn validate(&self) -> Checked {
        v::nested("uid", &self.uid)
    }
}

// -- interaction_answer ------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct InteractionAnswerReq {
    pub uid: SessionUid,
    pub request_id: RequestId,
    pub decision: Decision,
    pub via: AnswerVia,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum AnswerVia {
    Digit,
    Button,
    Shortcut,
}

impl Validate for InteractionAnswerReq {
    fn validate(&self) -> Checked {
        v::nested("uid", &self.uid)?;
        v::nested("requestId", &self.request_id)?;
        v::nested("decision", &self.decision)
    }
}

/// `answered` = this call sent the decision; `alreadyAnswered` = a decision for
/// this `requestId` was already sent (`previous` names it); `withdrawn` = no
/// longer pending.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum AnswerOutcome {
    Answered,
    AlreadyAnswered,
    Withdrawn,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct InteractionAnswerRes {
    pub outcome: AnswerOutcome,
    pub previous: Option<DecisionKind>,
}

// -- screen_prompt_key ---------------------------------------------------------------

/// §6.2.4 [R6 §4 LP-12, lead ruling v2, A6].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct ScreenPromptKeyReq {
    pub uid: SessionUid,
    pub request_id: RequestId,
    /// First 16 hex of sha256(question + "\n" + option labels).
    pub prompt_rev: String,
    pub action: ScreenPromptAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum ScreenPromptAction {
    /// "1".."9" | "esc": the digit without CR, or Esc.
    Key { key: String },
    /// The "No" digit, then the text as the next turn; 1..=256 KiB, no NUL.
    DenyWithReply { text: String },
    /// Only when the footer offers "Tab to amend"; 1..=256 KiB, no NUL.
    AllowWithNote { text: String },
    /// The recognised folder-trust dialog only (A6).
    TrustFolder,
}

/// `prompt_rev`: 16 lowercase hex chars.
pub fn is_prompt_rev(s: &str) -> bool {
    s.len() == 16 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl Validate for ScreenPromptKeyReq {
    fn validate(&self) -> Checked {
        v::nested("uid", &self.uid)?;
        v::nested("requestId", &self.request_id)?;
        if self.prompt_rev.is_empty() {
            return Err(Invalid::new("promptRev", "required"));
        }
        if !is_prompt_rev(&self.prompt_rev) {
            return Err(Invalid::new("promptRev", "pattern"));
        }
        v::nested("action", &self.action)
    }
}

impl Validate for ScreenPromptAction {
    fn validate(&self) -> Checked {
        match self {
            ScreenPromptAction::Key { key } => {
                if key.is_empty() {
                    return Err(Invalid::new("key", "required"));
                }
                if is_option_key(key) {
                    Ok(())
                } else {
                    Err(Invalid::new("key", "pattern"))
                }
            }
            ScreenPromptAction::DenyWithReply { text } | ScreenPromptAction::AllowWithNote { text } => {
                v::text_range("text", text, 1, MAX_PROMPT_BYTES)
            }
            ScreenPromptAction::TrustFolder => Ok(()),
        }
    }
}

/// `aborted` = no Enter sent (TrustFolder: the grid stopped matching, the
/// highlight was not found within 3 keys, or the screen changed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum ScreenKeyOutcome {
    Sent,
    Withdrawn,
    Aborted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct ScreenPromptKeyRes {
    pub outcome: ScreenKeyOutcome,
}

// -- interaction_detail ------------------------------------------------------------

/// For `→` "expand to show the full tool input" [ux-ia §3.1].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct InteractionDetailReq {
    pub uid: SessionUid,
    pub request_id: RequestId,
}

impl Validate for InteractionDetailReq {
    fn validate(&self) -> Checked {
        v::nested("uid", &self.uid)?;
        v::nested("requestId", &self.request_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct InteractionDetailRes {
    /// ≤ 64 KiB, sanitized.
    pub input_pretty: String,
    pub tool_name: String,
    pub decision_reason: Option<String>,
}
