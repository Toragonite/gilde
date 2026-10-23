//! Project and Location **[meta]** (contract §3.4.2).

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::contract::common::{AbsPath, EpochMs};
use crate::contract::ids::{LocationId, ProjectId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum ProjectKind {
    Git,
    Folder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum LocationKind {
    Main,
    Worktree,
    Folder,
}

/// | Field | Invariant |
/// |---|---|
/// | `id` | unique; equal for every worktree of one repo (common dir) |
/// | `order` | dense 0..n−1 over non-earlier projects; reordering is a user action only |
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct Project {
    pub id: ProjectId,
    /// Display: basename of the main checkout or folder; ≤ 255 chars; sanitized.
    pub name: String,
    /// Main checkout root (git) or the folder.
    pub root: AbsPath,
    pub kind: ProjectKind,
    /// User order [M3]; new projects append.
    pub order: u32,
    /// User fold state.
    pub folded: bool,
    /// Derived: no live session and no activity in 24 h [M3].
    pub earlier: bool,
    #[ts(type = "number | null")]
    pub last_activity_at: Option<EpochMs>,
    /// A team counts once [ux-ia §2.5 Counts].
    pub live_sessions: u32,
    /// Root under /tmp or /private/tmp (scratch projects; shown in Details).
    pub is_temp: bool,
}

/// `path`: a session's location = the location whose path is the longest
/// prefix of `realpath(cwd)`; `branch` is read from `<gitdir>/HEAD`, never by
/// spawning git in discovery.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct Location {
    pub id: LocationId,
    pub project_id: ProjectId,
    pub path: AbsPath,
    pub kind: LocationKind,
    /// ≤ 255, from the HEAD file; `None` when detached or unknown.
    pub branch: Option<String>,
    /// `wt/<name>` chip text source.
    pub worktree_name: Option<String>,
    pub gilde_created: bool,
    /// False after discard/removal ("worktree removed" chip).
    pub exists: bool,
}
