//! §4.2.2 Sessions.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use super::{empty_structs, model_rule};
use crate::contract::account::SignIn;
use crate::contract::common::{EpochMs, Notice, PermissionMode, Vendor};
use crate::contract::ids::{AccountId, ClientRequestId, ConfirmToken, LocationId, ProjectId, SessionUid};
use crate::contract::project::Project;
use crate::contract::session::ControlMode;
use crate::contract::validate::{self as v, Checked, Invalid, Validate};

empty_structs!(req:
    /// `project_pick` request: Rust opens the native folder picker.
    ProjectPickReq,
);

empty_structs!(res:
    SessionMarkSeenRes,
    SessionDismissRes,
);

/// `{ uid }` — the request of every command that names one session only.
macro_rules! uid_req {
    ($($(#[$m:meta])* $name:ident),* $(,)?) => {
        $(
            $(#[$m])*
            #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
            #[serde(rename_all = "camelCase", deny_unknown_fields)]
            #[cfg_attr(feature = "ts-export", ts(export))]
            pub struct $name {
                pub uid: SessionUid,
            }

            impl Validate for $name {
                fn validate(&self) -> Checked {
                    v::nested("uid", &self.uid)
                }
            }
        )*
    };
}
pub(crate) use uid_req;

uid_req!(
    /// `session_stop_preview`.
    SessionStopPreviewReq,
    /// `session_mark_seen`: after ≥ 2 s on screen/focused; `⌘J` fly-through never sends it.
    SessionMarkSeenReq,
    /// `session_models`.
    SessionModelsReq,
    /// `session_technical`: on demand only; never in the store.
    SessionTechnicalReq,
);

// -- newsession_options ------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct NewsessionOptionsReq {
    pub project_id: ProjectId,
    pub vendor: Option<Vendor>,
}

impl Validate for NewsessionOptionsReq {
    fn validate(&self) -> Checked {
        v::nested("projectId", &self.project_id)
    }
}

/// Returns what is known within 1 s and pushes the rest through the store.
/// `modes` omits Conversation for a claude version that is `Failed` or below
/// 2.1.268 (with the notice); `accounts` lists only `usable_for_new` accounts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct NewSessionOptions {
    pub vendors: Vec<VendorOption>,
    pub accounts: Vec<AccountOption>,
    pub models: Vec<ModelOption>,
    pub modes: Vec<ControlMode>,
    pub permission_modes: Vec<PermissionMode>,
    pub locations: Vec<LocationOption>,
    pub team_allowed: bool,
    pub team_mode: TeamMode,
    pub defaults: StartDefaults,
    pub notices: Vec<Notice>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct VendorOption {
    pub vendor: Vendor,
    pub installed: bool,
    pub version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct AccountOption {
    pub account_id: AccountId,
    pub vendor: Vendor,
    pub label: String,
    pub is_default: bool,
    pub sign_in: SignIn,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct ModelOption {
    pub id: String,
    pub label: Option<String>,
    pub is_default: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct LocationOption {
    pub location: StartLocation,
    pub branch: Option<String>,
    pub worktree_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum TeamMode {
    Panes,
    InProcess,
}

/// Defaults follow the account rule of ux-ia §5.8 and `project_prefs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct StartDefaults {
    pub vendor: Option<Vendor>,
    pub account_id: Option<AccountId>,
    pub model: Option<String>,
    pub control_mode: Option<ControlMode>,
    pub permission_mode: Option<PermissionMode>,
    pub location: Option<StartLocation>,
}

// -- project_pick ------------------------------------------------------------

/// `null` = the user cancelled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct ProjectPickRes {
    pub project: Option<Project>,
}

// -- session_start -------------------------------------------------------------

/// §4.2.2 `SessionStartReq`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct SessionStartReq {
    pub client_request_id: ClientRequestId,
    pub project_id: ProjectId,
    pub vendor: Vendor,
    /// Must belong to `vendor` and be `usable_for_new`; sign_in ≠ SignedOut.
    pub account_id: AccountId,
    /// `^[A-Za-z0-9._:\[\]/-]{1,128}$` and ∈ `newsession_options.models`;
    /// absent/"" = vendor default.
    pub model: Option<String>,
    pub location: StartLocation,
    /// ∈ `newsession_options.permissionModes`; BypassPermissions only for claude, only here.
    pub permission_mode: Option<PermissionMode>,
    /// claude: Conversation|Terminal; codex/cursor/agy: TurnBased|Terminal.
    pub control_mode: ControlMode,
    /// ⇒ vendor = claude ∧ control_mode = Terminal.
    pub team: bool,
    /// ≤ 256 KiB UTF-8, no NUL; absent/"" = start with no first message.
    pub prompt: Option<String>,
}

/// The largest first prompt / sent message (§4.2.2, §4.2.3): 256 KiB.
pub const MAX_PROMPT_BYTES: usize = 256 * 1024;

impl Validate for SessionStartReq {
    fn validate(&self) -> Checked {
        v::nested("clientRequestId", &self.client_request_id)?;
        v::nested("projectId", &self.project_id)?;
        v::nested("accountId", &self.account_id)?;
        model_rule("model", &self.model)?;
        v::nested("location", &self.location)?;
        v::optional_str("prompt", &self.prompt, |f, s| v::text(f, s, MAX_PROMPT_BYTES))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum StartLocation {
    Here { location_id: Option<LocationId> },
    /// `slug`: `^[a-z0-9][a-z0-9-]{0,62}$`; branch = `gilde/<slug>`.
    NewWorktree { slug: String, base: Option<String> },
    ExistingWorktree { location_id: LocationId },
}

/// `NewWorktree.slug` (§4.2.2).
pub fn is_worktree_slug(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// A base ref given to `git` as one argv element: ≤ 255, `[A-Za-z0-9._/-]`,
/// never starting with `-` (no option injection) or `/`, no `..`.
pub fn is_base_ref(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 255
        && !s.starts_with('-')
        && !s.starts_with('/')
        && !s.contains("..")
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'/' | b'-'))
}

impl Validate for StartLocation {
    fn validate(&self) -> Checked {
        match self {
            StartLocation::Here { location_id } => v::nested("locationId", location_id),
            StartLocation::NewWorktree { slug, base } => {
                if slug.is_empty() {
                    return Err(Invalid::new("slug", "required"));
                }
                if !is_worktree_slug(slug) {
                    return Err(Invalid::new("slug", "pattern"));
                }
                v::optional_str("base", base, |f, s| {
                    if is_base_ref(s) {
                        Ok(())
                    } else {
                        Err(Invalid::new(f, "pattern"))
                    }
                })
            }
            StartLocation::ExistingWorktree { location_id } => v::nested("locationId", location_id),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct SessionStartRes {
    pub uid: SessionUid,
}

// -- session_continue ----------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct SessionContinueReq {
    pub client_request_id: ClientRequestId,
    pub uid: SessionUid,
    pub how: ContinueHow,
    pub control_mode: Option<ControlMode>,
    pub model: Option<String>,
    pub permission_mode: Option<PermissionMode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum ContinueHow {
    Resume,
    Fork,
    Attach,
    OtherAccount { account_id: AccountId },
}

impl Validate for SessionContinueReq {
    fn validate(&self) -> Checked {
        v::nested("clientRequestId", &self.client_request_id)?;
        v::nested("uid", &self.uid)?;
        if let ContinueHow::OtherAccount { account_id } = &self.how {
            v::nested("how.accountId", account_id)?;
        }
        model_rule("model", &self.model)
    }
}

/// Same uid for Resume/Attach, new uid for Fork/OtherAccount.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct SessionContinueRes {
    pub uid: SessionUid,
}

// -- session_retry_as_terminal ---------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct SessionRetryAsTerminalReq {
    pub client_request_id: ClientRequestId,
    pub uid: SessionUid,
}

impl Validate for SessionRetryAsTerminalReq {
    fn validate(&self) -> Checked {
        v::nested("clientRequestId", &self.client_request_id)?;
        v::nested("uid", &self.uid)
    }
}

/// Same uid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct SessionRetryAsTerminalRes {
    pub uid: SessionUid,
}

// -- stop ------------------------------------------------------------------------

/// How a process is stopped (§3.4.4 `StopCap` without `None`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum StopMethod {
    Fenced,
    ClaudeStop,
    TermAfterIdentity,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct SessionStopPreviewRes {
    pub how: StopMethod,
    /// For watched sessions the sheet text is ux-ia #113.
    pub owner_text: Notice,
    pub confirm_token: ConfirmToken,
}

/// Never SIGKILL for vendor processes (§9.8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct SessionStopReq {
    pub uid: SessionUid,
    pub confirm_token: ConfirmToken,
}

impl Validate for SessionStopReq {
    fn validate(&self) -> Checked {
        v::nested("uid", &self.uid)?;
        v::nested("confirmToken", &self.confirm_token)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct SessionStopRes {
    pub stopped: bool,
    pub survivors: u32,
}

// -- dismiss -----------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct SessionDismissReq {
    pub uid: SessionUid,
    pub what: DismissWhat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum DismissWhat {
    NewReply,
    Crash,
    Error,
}

impl Validate for SessionDismissReq {
    fn validate(&self) -> Checked {
        v::nested("uid", &self.uid)
    }
}

// -- session_models, session_technical ---------------------------------------------

/// From `initialize.models`/`system/init` (Conversation) or agent-mesh
/// (turn-based); `session_set_model` validates against this list [F-23].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct SessionModelsRes {
    pub models: Vec<ModelOption>,
    pub current: Option<String>,
}

/// The redacted command line (§9.7) and the parent chain; never in the store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct SessionTechnicalRes {
    pub pid: i32,
    #[ts(type = "number")]
    pub started_at: EpochMs,
    pub command_redacted: Vec<String>,
    pub parent_chain: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_and_refs() {
        assert!(is_worktree_slug("fix-login-2"));
        assert!(is_worktree_slug("20261022-a1b2"));
        assert!(!is_worktree_slug("-x"));
        assert!(!is_worktree_slug("Fix"));
        assert!(!is_worktree_slug(&"a".repeat(64)));
        assert!(is_base_ref("origin/main"));
        assert!(!is_base_ref("--upload-pack=x"));
        assert!(!is_base_ref("a..b"));
        assert!(!is_base_ref("a b"));
    }

    #[test]
    fn empty_slug_is_required_not_defaulted() {
        let loc = StartLocation::NewWorktree {
            slug: String::new(),
            base: None,
        };
        assert_eq!(loc.validate().unwrap_err().rule, "required");
    }
}
