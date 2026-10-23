//! §4.2.7 Accounts, quota, health.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use super::empty_structs;
use super::sessions::StopMethod;
use crate::contract::account::{Account, Quota};
use crate::contract::common::EpochMs;
use crate::contract::ids::{AccountId, ClientRequestId, ConfirmToken, ProcessKey, SessionUid, UndoToken};
use crate::contract::process::{LeftoverReason, ProcessRecord};
use crate::contract::validate::{self as v, Checked, Validate};

empty_structs!(req:
    /// `health_processes`: the palette's "Show all agent processes"; argv never included.
    HealthProcessesReq,
);

empty_structs!(res:
    AccountRemoveRes,
    AccountSetDefaultRes,
    AccountSetLabelRes,
);

/// `{ accountId }` — the request of the account commands that name one account.
macro_rules! account_req {
    ($($(#[$m:meta])* $name:ident),* $(,)?) => {
        $(
            $(#[$m])*
            #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
            #[serde(rename_all = "camelCase", deny_unknown_fields)]
            #[cfg_attr(feature = "ts-export", ts(export))]
            pub struct $name {
                pub account_id: AccountId,
            }

            impl Validate for $name {
                fn validate(&self) -> Checked {
                    v::nested("accountId", &self.account_id)
                }
            }
        )*
    };
}

account_req!(
    /// A Discovered sibling becomes `User` and `usable_for_new` [C7]; `REFUSED{notDiscovered}`.
    AccountConfirmReq,
    /// User-added accounts only; never deletes the folder; `REFUSED{inUse|notUserAdded}`.
    AccountRemoveReq,
    /// One default per vendor; `REFUSED{notUsable}`.
    AccountSetDefaultReq,
    /// ≤ 1 per account per 60 s, else the cached value with `stale:true`.
    QuotaRefreshReq,
);

// -- account_add ----------------------------------------------------------------

/// Rust opens a native folder picker (dialog plugin, Rust API), pre-selecting
/// discovered siblings; never runs `claude agents --json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct AccountAddReq {
    pub vendor: AccountAddVendor,
}

/// Only claude and codex accounts are added by folder (`{ vendor: "claude"|"codex" }`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum AccountAddVendor {
    Claude,
    Codex,
}

impl Validate for AccountAddReq {}

/// `null` = cancelled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct AccountAddRes {
    pub account: Option<Account>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct AccountConfirmRes {
    pub account: Account,
}

/// 1..=64 chars, sanitized (no control characters).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct AccountSetLabelReq {
    pub account_id: AccountId,
    pub label: String,
}

impl Validate for AccountSetLabelReq {
    fn validate(&self) -> Checked {
        v::nested("accountId", &self.account_id)?;
        v::chars("label", &self.label, 1, 64)?;
        v::no_controls("label", &self.label)
    }
}

/// The Error·Sign-in needed primary action: starts a Terminal-mode session
/// running the CLI's own sign-in with that account's env; Gilde handles no
/// credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct AccountSignInReq {
    pub client_request_id: ClientRequestId,
    pub account_id: AccountId,
}

impl Validate for AccountSignInReq {
    fn validate(&self) -> Checked {
        v::nested("clientRequestId", &self.client_request_id)?;
        v::nested("accountId", &self.account_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct AccountSignInRes {
    pub uid: SessionUid,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct QuotaRefreshRes {
    pub quota: Quota,
}

// -- health ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct HealthProcessesRes {
    pub processes: Vec<ProcessRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct HealthProcessDetailReq {
    pub process_key: ProcessKey,
}

impl Validate for HealthProcessDetailReq {
    fn validate(&self) -> Checked {
        v::nested("processKey", &self.process_key)
    }
}

/// The redacted command line (§9.7) and parent chain, on demand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct HealthProcessDetailRes {
    pub command_redacted: Vec<String>,
    pub parent_chain: Vec<String>,
    #[ts(type = "number")]
    pub started_at: EpochMs,
}

/// 1..=50 keys, each from the current leftover list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct HealthStopPreviewReq {
    pub process_keys: Vec<ProcessKey>,
}

impl Validate for HealthStopPreviewReq {
    fn validate(&self) -> Checked {
        v::items_range("processKeys", &self.process_keys, 1, 50)?;
        v::each("processKeys", &self.process_keys)?;
        v::unique("processKeys", &self.process_keys)
    }
}

/// Each target: what, why it is a leftover, memory, how [ux-ia §3.15].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct StopTarget {
    pub process_key: ProcessKey,
    /// Display label (redacted, never argv values).
    pub what: String,
    pub why: LeftoverReason,
    #[ts(type = "number")]
    pub memory_bytes: u64,
    pub how: StopMethod,
}

/// Cannot be identified → never stopped (ux-ia #114).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct StopRefusal {
    pub process_key: ProcessKey,
    /// A machine token (i18n `error.refused.<reason>`).
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct HealthStopPreviewRes {
    pub targets: Vec<StopTarget>,
    pub refused: Vec<StopRefusal>,
    pub confirm_token: ConfirmToken,
}

/// Identity re-checked per target at execution; never SIGKILL a vendor process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct HealthStopReq {
    pub confirm_token: ConfirmToken,
}

impl Validate for HealthStopReq {
    fn validate(&self) -> Checked {
        v::nested("confirmToken", &self.confirm_token)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct HealthStopRes {
    pub stopped: u32,
    pub survivors: u32,
    pub undo_token: Option<UndoToken>,
}

/// Cancels a scheduled stop within its 10 s undo window; `NOT_FOUND{token}` once expired.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct UndoCancelReq {
    pub undo_token: UndoToken,
}

impl Validate for UndoCancelReq {
    fn validate(&self) -> Checked {
        v::nested("undoToken", &self.undo_token)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct UndoCancelRes {
    pub cancelled: bool,
}
