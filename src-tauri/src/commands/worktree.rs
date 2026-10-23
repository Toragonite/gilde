//! Worktree discard (contract §4.2.5) through the existing gates.
//!
//! Owner: T11.
//!
//! Thin by design: the `Req` checks, then T11's `GitApi` (managed as
//! [`crate::commands::changes::GitState`] by the composition root, T09b;
//! `UNSUPPORTED{notBuilt}` until then). Each loss of a discard needs its own
//! acknowledgement (`REFUSED{unacknowledgedLoss}`), the loss set is re-checked
//! against the preview (`CONFLICT{changed}`), and the branch is always kept.
//!
//! Note what is **not** here: there is no "switch worktree" command.
//! Switching is a UI concept (DESIGN §7-4) — running `git checkout` in the
//! user's own checkout while N agents are live would pull the ground out from
//! under all of them — and the v1 commands that took a free path or a
//! `postCreateCmd` are gone from the IPC surface (§4.8).

use std::sync::Arc;

use crate::commands::changes::git;
use crate::contract::ipc::{
    WorktreeDiscardPreviewReq, WorktreeDiscardPreviewRes, WorktreeDiscardReq, WorktreeDiscardRes,
};
use crate::contract::{IpcError, Req};
use crate::worktree::WorktreeManager;

/// The kept v1 manager (`lib.rs` manages it for the code that still uses it).
pub type WorktreeState = Arc<WorktreeManager>;

/// `worktree_discard_preview` (contract §4.2.5): every loss, each with its own
/// checkbox in the UI; the branch is kept.
#[tauri::command]
pub async fn worktree_discard_preview<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    req: Req<WorktreeDiscardPreviewReq>,
) -> Result<WorktreeDiscardPreviewRes, IpcError> {
    git(&app)?.discard_preview(&req).await
}

/// `worktree_discard` (contract §4.2.5): behind the preview's confirm token.
#[tauri::command]
pub async fn worktree_discard<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    req: Req<WorktreeDiscardReq>,
) -> Result<WorktreeDiscardRes, IpcError> {
    git(&app)?.discard(&req).await?;
    Ok(WorktreeDiscardRes {})
}
