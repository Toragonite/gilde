//! §4.2.6 Teams.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use super::empty_structs;
use crate::contract::common::{epoch_ms, EpochMs, Notice};
use crate::contract::ids::{ClientMsgId, ConfirmToken, MemberKey, MessageKey, TaskKey, TeamKey};
use crate::contract::team::TeamMessage;
use crate::contract::validate::{self as v, Checked, Validate};

empty_structs!(res: TeamMemberCloseRes);

/// Only for hosted pane members (`act_send ∋ ViaMemberPane`): bracketed paste
/// + CR into the member's PTY. Any other member → `UNSUPPORTED{viaLeadOnly|
/// orchestratorTeam|memberGone}` (the UI prefills the lead's composer instead).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct TeamSendReq {
    pub team_key: TeamKey,
    pub member_key: MemberKey,
    pub client_msg_id: ClientMsgId,
    pub text: String,
}

impl Validate for TeamSendReq {
    fn validate(&self) -> Checked {
        v::nested("teamKey", &self.team_key)?;
        v::nested("memberKey", &self.member_key)?;
        v::nested("clientMsgId", &self.client_msg_id)?;
        v::required_str("text", &self.text, v::DEFAULT_MAX_STRING_BYTES)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum TeamSendRoute {
    MemberPane,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct TeamSendRes {
    pub route: TeamSendRoute,
}

/// `{ teamKey, memberKey }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct TeamMemberClosePreviewReq {
    pub team_key: TeamKey,
    pub member_key: MemberKey,
}

impl Validate for TeamMemberClosePreviewReq {
    fn validate(&self) -> Checked {
        v::nested("teamKey", &self.team_key)?;
        v::nested("memberKey", &self.member_key)
    }
}

/// ux-ia #105 + `team.closePaneUnread`; `UNSUPPORTED{notAPane}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct TeamMemberClosePreviewRes {
    /// The sheet's sentences in order, each a key + params (e.g.
    /// `team.closePane`, then `team.closePaneUnread {member}`); never a key
    /// name inside a param [A21.4].
    pub lines: Vec<Notice>,
    pub confirm_token: ConfirmToken,
}

/// SIGHUP to the pane's group, then fenced SIGTERM after 2 s (§7.4.7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct TeamMemberCloseReq {
    pub team_key: TeamKey,
    pub member_key: MemberKey,
    pub confirm_token: ConfirmToken,
}

impl Validate for TeamMemberCloseReq {
    fn validate(&self) -> Checked {
        v::nested("teamKey", &self.team_key)?;
        v::nested("memberKey", &self.member_key)?;
        v::nested("confirmToken", &self.confirm_token)
    }
}

/// Read on demand (LRU of 200 bodies, memory only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct TeamMessageBodyReq {
    pub message_key: MessageKey,
}

impl Validate for TeamMessageBodyReq {
    fn validate(&self) -> Checked {
        v::nested("messageKey", &self.message_key)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum MessageBodySource {
    RecipientTranscript,
    SenderToolUse,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct TeamMessageBodyRes {
    /// **[content]** ≤ 16 KiB
    pub text: Option<String>,
    pub summary: Option<String>,
    pub source: MessageBodySource,
}

/// `count` 1..=500.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct TeamMessagesPageReq {
    pub team_key: TeamKey,
    #[ts(type = "number")]
    pub before: EpochMs,
    pub count: u16,
}

impl Validate for TeamMessagesPageReq {
    fn validate(&self) -> Checked {
        v::nested("teamKey", &self.team_key)?;
        epoch_ms("before", self.before)?;
        v::range("count", self.count, 1, 500)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct TeamMessagesPageRes {
    pub messages: Vec<TeamMessage>,
    pub has_more: bool,
}

/// For tasks whose title was evicted from memory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct TeamTaskTitleReq {
    pub task_key: TaskKey,
}

impl Validate for TeamTaskTitleReq {
    fn validate(&self) -> Checked {
        v::nested("taskKey", &self.task_key)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct TeamTaskTitleRes {
    pub title: Option<String>,
}
