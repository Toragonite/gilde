//! `worktree` commands (DESIGN §3, `projects / worktree`).
//!
//! Thin by design: validate, call [`WorktreeManager`], return a small ack.
//! The one thing that happens *here* and not in the manager is turning a typed
//! [`WorktreeError`] into a fully parameterised [`GildeErrorPayload`] — `error.rs`
//! is single-owner (T01) and has no structured worktree variant, so the refusal
//! is rendered at the boundary instead of adding an `GildeError` case.
//!
//! Note what is **not** in this list: there is no `worktree_switch`. Switching is
//! a UI concept (DESIGN §7-4) — running `git checkout` in the user's own
//! checkout while N agents are live would pull the ground out from under all of
//! them, so no command can ask for it.

use std::sync::Arc;

use tauri::State;

use crate::error::{GildeError, GildeErrorPayload, CmdResult};
use crate::worktree::{
    CreateWorktreeReq, PruneReport, RemoveMode, RemoveOutcome, RemovePreflight, Worktree,
    WorktreeError, WorktreeFailure, WorktreeManager, WorktreeStats,
};

/// Managed state alias so later tasks ask for the same thing.
pub type WorktreeState = Arc<WorktreeManager>;

/// Render a typed refusal with its interpolation parameters intact.
fn payload(failure: WorktreeFailure) -> GildeErrorPayload {
    match failure {
        WorktreeFailure::Refused(e) => refusal_payload(&e),
        WorktreeFailure::Other(e) => e.into(),
    }
}

fn refusal_payload(e: &WorktreeError) -> GildeErrorPayload {
    GildeErrorPayload {
        code: "REFUSED".into(),
        key: e.key().into(),
        params: e.params(),
        detail: None,
        retryable: false,
    }
}

#[tauri::command]
pub async fn worktree_list(
    project_id: String,
    worktrees: State<'_, WorktreeState>,
) -> CmdResult<Vec<Worktree>> {
    Ok(worktrees.list(&project_id).await?)
}

/// DESIGN §7-3. Every preflight failure is typed so the UI can offer the right
/// next step — `BranchInUse` in particular becomes "open the existing worktree".
#[tauri::command]
pub async fn worktree_create(
    req: CreateWorktreeReq,
    worktrees: State<'_, WorktreeState>,
) -> CmdResult<Worktree> {
    worktrees.create_typed(req).await.map_err(payload)
}

#[tauri::command]
pub async fn worktree_stats(
    id: String,
    worktrees: State<'_, WorktreeState>,
) -> CmdResult<WorktreeStats> {
    Ok(worktrees.stats(&id).await?)
}

/// Everything the removal card needs, gathered before anything is destroyed.
#[tauri::command]
pub async fn worktree_remove_preflight(
    id: String,
    worktrees: State<'_, WorktreeState>,
) -> CmdResult<RemovePreflight> {
    Ok(worktrees.remove_preflight(&id).await?)
}

/// DESIGN §7-7. `RemoveMode` has no `Force` variant, so no caller can ask for
/// the destructive path that does not exist.
#[tauri::command]
pub async fn worktree_remove(
    id: String,
    mode: RemoveMode,
    worktrees: State<'_, WorktreeState>,
) -> CmdResult<RemoveOutcome> {
    worktrees.remove_typed(&id, mode).await.map_err(payload)
}

#[tauri::command]
pub async fn worktree_prune(
    project_id: String,
    worktrees: State<'_, WorktreeState>,
) -> CmdResult<PruneReport> {
    Ok(worktrees.prune(&project_id).await?)
}

#[tauri::command]
pub async fn worktree_lock(
    id: String,
    locked: bool,
    worktrees: State<'_, WorktreeState>,
) -> CmdResult<()> {
    Ok(worktrees.set_locked(&id, locked).await?)
}

/// Not part of the IPC surface: a guard rail for the module docs above.
#[allow(dead_code)]
fn _no_switch_command() -> GildeError {
    GildeError::refused("worktreeSwitchIsAUiConcept")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_reach_the_ui_with_their_parameters() {
        let p = refusal_payload(&WorktreeError::BranchInUse {
            branch: "gilde/fix-ab12cd".into(),
            existing_path: "/wt/fix".into(),
        });
        assert_eq!(p.code, "REFUSED");
        assert_eq!(p.key, "error.worktree.branchInUse");
        assert_eq!(p.params["branch"], "gilde/fix-ab12cd");
        assert_eq!(p.params["existingPath"], "/wt/fix");
        assert!(!p.retryable);

        let p = payload(WorktreeFailure::Refused(WorktreeError::Dirty {
            dirty_files: 4,
            untracked: 2,
        }));
        assert_eq!(p.key, "error.worktree.dirty");
        assert_eq!(p.params["dirtyFiles"], 4);
    }

    #[test]
    fn ordinary_failures_keep_their_own_payload() {
        let p = payload(WorktreeFailure::Other(GildeError::Git {
            code: 128,
            stderr: "fatal: not a git repository\n".into(),
        }));
        assert_eq!(p.code, "GIT");
        assert_eq!(p.detail.as_deref(), Some("fatal: not a git repository"));
    }
}
