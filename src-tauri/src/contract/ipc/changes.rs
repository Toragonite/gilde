//! §4.2.5 Changes, IDE, links, owners.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use super::empty_structs;
use super::sessions::uid_req;
use crate::contract::common::EpochMs;
use crate::contract::content::ChangeKind;
use crate::contract::ids::{ConfirmToken, LocationId, ProjectId, SessionUid};
use crate::contract::validate::{self as v, Checked, Invalid, Validate};

empty_structs!(req:
    /// `ide_editors` request.
    IdeEditorsReq,
);

empty_structs!(res:
    WorktreeDiscardRes,
    IdeOpenRes,
    IdeRevealRes,
);

uid_req!(
    /// `changes_get`: `UNSUPPORTED{notGit}` for folder projects.
    ChangesGetReq,
    /// `git_merge_preview`: existing gates (no `--force`, no branch delete).
    GitMergePreviewReq,
    /// `worktree_discard_preview`: each loss needs its own checkbox in the UI.
    WorktreeDiscardPreviewReq,
    /// `owner_focus` (§9.5).
    OwnerFocusReq,
);

// -- ChangesView -------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct ChangesView {
    pub location: LocationId,
    pub base: Option<ChangesBase>,
    /// ≤ 2000
    pub files: Vec<FileRow>,
    /// ≤ 200
    pub commits: Vec<CommitRow>,
    pub shared_folder: Option<SharedFolder>,
    pub actions: ChangesActions,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct ChangesBase {
    pub branch: String,
    pub commit_short: String,
    pub newer_on_main: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct FileRow {
    pub path_rel: String,
    pub change: ChangeKind,
    pub added: u32,
    pub removed: u32,
    /// **[content]** hunk-header names, ≤ 8.
    pub parts: Vec<String>,
    /// Intersection of the session's own edit/write tool calls with `git status`.
    pub made_by: MadeBy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum MadeBy {
    ThisSession,
    OtherSession { uid: SessionUid },
    NotFromKnownSession,
    /// cursor/agy.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct CommitRow {
    pub commit_short: String,
    /// **[content]** first line of the message.
    pub subject: String,
    #[ts(type = "number")]
    pub at: EpochMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct SharedFolder {
    pub other_live_sessions: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct ChangesActions {
    pub commit: ActionState,
    pub merge: ActionState,
    pub discard: ActionState,
}

/// Whether a Changes action is offered; `reason` is an i18n key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum ActionState {
    Enabled,
    Disabled { reason: String },
    Hidden,
}

// -- commit ----------------------------------------------------------------------

/// `paths` 1..=2000 relative paths, each ∈ the Changes file set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct GitCommitPreviewReq {
    pub uid: SessionUid,
    pub paths: Vec<String>,
}

fn rel_paths(field: &str, paths: &[String]) -> Checked {
    v::items_range(field, paths, 1, 2000)?;
    for (i, p) in paths.iter().enumerate() {
        v::rel_path(&format!("{field}[{i}]"), p)?;
    }
    v::unique(field, paths)
}

impl Validate for GitCommitPreviewReq {
    fn validate(&self) -> Checked {
        v::nested("uid", &self.uid)?;
        rel_paths("paths", &self.paths)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct GitCommitPreviewRes {
    pub files: Vec<FileRow>,
    /// From git config.
    pub author: GitAuthor,
    pub confirm_token: ConfirmToken,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct GitAuthor {
    pub name: String,
    pub email: String,
}

/// message 1..=8 KiB; every path ∈ the preview's set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct GitCommitReq {
    pub uid: SessionUid,
    pub message: String,
    pub paths: Vec<String>,
    pub confirm_token: ConfirmToken,
}

impl Validate for GitCommitReq {
    fn validate(&self) -> Checked {
        v::nested("uid", &self.uid)?;
        v::text_range("message", &self.message, 1, 8 * 1024)?;
        rel_paths("paths", &self.paths)?;
        v::nested("confirmToken", &self.confirm_token)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct GitCommitRes {
    pub commit: String,
}

// -- merge -------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct GitMergePreviewRes {
    pub effect: MergeEffect,
    pub confirm_token: ConfirmToken,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct MergeEffect {
    pub kind: MergeKind,
    /// Commits the merge brings into the base branch.
    pub commits: u32,
    pub files_changed: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum MergeKind {
    FastForward,
    MergeCommit,
    UpToDate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct GitMergeReq {
    pub uid: SessionUid,
    pub confirm_token: ConfirmToken,
}

impl Validate for GitMergeReq {
    fn validate(&self) -> Checked {
        v::nested("uid", &self.uid)?;
        v::nested("confirmToken", &self.confirm_token)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct GitMergeRes {
    pub merge_commit: String,
}

// -- discard -------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct WorktreeDiscardPreviewRes {
    pub losses: Vec<Loss>,
    /// Always `true`.
    pub branch_kept: bool,
    pub confirm_token: ConfirmToken,
}

/// One thing a discard would lose (each needs its own checkbox).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct Loss {
    /// `LossId`: printable ASCII 1..=64, stable within one preview.
    pub id: String,
    pub kind: LossKind,
    pub count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum LossKind {
    UncommittedChanges,
    UntrackedFiles,
    UnmergedCommits,
    Stashes,
}

/// `CONFLICT{changed}` if the set of losses changed; `REFUSED{unacknowledgedLoss}`
/// when a loss is not in `acceptedLosses`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct WorktreeDiscardReq {
    pub uid: SessionUid,
    pub confirm_token: ConfirmToken,
    pub accepted_losses: Vec<String>,
}

/// `LossId`: printable ASCII, 1..=64 bytes.
pub fn is_loss_id(s: &str) -> bool {
    (1..=64).contains(&s.len()) && s.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

impl Validate for WorktreeDiscardReq {
    fn validate(&self) -> Checked {
        v::nested("uid", &self.uid)?;
        v::nested("confirmToken", &self.confirm_token)?;
        v::max_items("acceptedLosses", &self.accepted_losses, v::DEFAULT_MAX_ITEMS)?;
        for (i, id) in self.accepted_losses.iter().enumerate() {
            if !is_loss_id(id) {
                return Err(Invalid::new(format!("acceptedLosses[{i}]"), "pattern"));
            }
        }
        v::unique("acceptedLosses", &self.accepted_losses)
    }
}

// -- ide_open, ide_reveal, ide_editors ----------------------------------------------------

/// §9.4 path rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct IdeOpenReq {
    pub target: IdeTarget,
}

/// `path` is relative to the session's location or absolute; `line` 1..=10^7.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum IdeTarget {
    File { uid: SessionUid, path: String, line: Option<u32> },
    Diff { uid: SessionUid, path: String },
    Folder { uid: SessionUid },
    ProjectRoot { project_id: ProjectId },
}

/// A path the WebView may send for `ide_open` (§3.1 rule 7): relative to the
/// session's location or absolute; non-empty, ≤ 1024 bytes, no NUL, no control
/// characters. Containment is checked by Rust after `realpath` (§9.4).
fn ide_path(field: &str, p: &str) -> Checked {
    v::required_str(field, p, 1024)?;
    v::no_controls(field, p)
}

impl Validate for IdeTarget {
    fn validate(&self) -> Checked {
        match self {
            IdeTarget::File { uid, path, line } => {
                v::nested("uid", uid)?;
                ide_path("path", path)?;
                if let Some(l) = line {
                    v::range("line", *l, 1, 10_000_000)?;
                }
                Ok(())
            }
            IdeTarget::Diff { uid, path } => {
                v::nested("uid", uid)?;
                ide_path("path", path)
            }
            IdeTarget::Folder { uid } => v::nested("uid", uid),
            IdeTarget::ProjectRoot { project_id } => v::nested("projectId", project_id),
        }
    }
}

impl Validate for IdeOpenReq {
    fn validate(&self) -> Checked {
        v::nested("target", &self.target)
    }
}

/// Finder reveal by `NSWorkspace` (no subprocess).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct IdeRevealReq {
    pub target: RevealTarget,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum RevealTarget {
    Transcript { uid: SessionUid },
    Location { uid: SessionUid },
    ProjectRoot { project_id: ProjectId },
}

impl Validate for IdeRevealReq {
    fn validate(&self) -> Checked {
        match &self.target {
            RevealTarget::Transcript { uid } | RevealTarget::Location { uid } => v::nested("target.uid", uid),
            RevealTarget::ProjectRoot { project_id } => v::nested("target.projectId", project_id),
        }
    }
}

/// VS Code (`com.microsoft.VSCode`) and Cursor (`com.todesktop.230313mzl4w4u92`)
/// only [C9]; the choice is the ordinary setting `ide.editor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum EditorKind {
    #[serde(rename = "vscode")]
    VsCode,
    Cursor,
}

impl crate::contract::validate::Validate for EditorKind {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct EditorOption {
    pub editor: EditorKind,
    pub bundle_id: String,
    /// Display form (`~/Applications/…` under the home dir).
    pub app_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct IdeEditorsRes {
    pub found: Vec<EditorOption>,
    pub chosen: Option<EditorKind>,
}

// -- link_open ---------------------------------------------------------------------------

/// http/https only, no userinfo, ≤ 2048 chars; opened by `NSWorkspace`; a
/// loopback/link-local/`.local`/private host opens only after a native
/// confirmation (`opened:false` if declined) [F-39].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct LinkOpenReq {
    pub url: String,
}

/// The `link_open` URL rule (`INVALID_ARGUMENT{field:"url", rule:"url"}`).
pub fn is_openable_url(s: &str) -> bool {
    if s.is_empty() || s.chars().count() > 2048 {
        return false;
    }
    let Ok(u) = url::Url::parse(s) else {
        return false;
    };
    matches!(u.scheme(), "http" | "https")
        && u.username().is_empty()
        && u.password().is_none()
        && u.host().is_some()
}

impl Validate for LinkOpenReq {
    fn validate(&self) -> Checked {
        if self.url.is_empty() {
            return Err(Invalid::new("url", "required"));
        }
        if is_openable_url(&self.url) {
            Ok(())
        } else {
            Err(Invalid::new("url", "url"))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct LinkOpenRes {
    pub opened: bool,
}

// -- owner_focus -----------------------------------------------------------------------

/// `window` only when the app flag `vscodeWindowFocus` is on (LP-24).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum FocusPrecision {
    Window,
    App,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct OwnerFocusRes {
    pub precision: FocusPrecision,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_rule() {
        assert!(is_openable_url("https://example.com/a?b#c"));
        assert!(is_openable_url("http://127.0.0.1:8080/"));
        assert!(!is_openable_url("javascript:alert(1)"));
        assert!(!is_openable_url("file:///etc/passwd"));
        assert!(!is_openable_url("data:text/html,x"));
        assert!(!is_openable_url("https://user:pw@example.com/"));
        assert!(!is_openable_url("https://user@example.com/"));
        assert!(!is_openable_url(&format!("https://e.com/{}", "a".repeat(2048))));
    }

    #[test]
    fn editor_wire_names() {
        assert_eq!(serde_json::to_string(&EditorKind::VsCode).unwrap(), "\"vscode\"");
        assert_eq!(serde_json::to_string(&EditorKind::Cursor).unwrap(), "\"cursor\"");
    }
}
