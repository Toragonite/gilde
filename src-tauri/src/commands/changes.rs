//! Changes, commit and merge (contract §4.2.5) through the existing gates.
//!
//! Owner: T11.
//!
//! The service is managed state ([`GitState`], bound by the composition root,
//! T09b). Until it is managed every command answers
//! `UNSUPPORTED{cap: "notBuilt"}` (the wave-0 stub behaviour) after the
//! request passed the contract checks of `Req` (envelope, size, shape, value
//! rules) — resolved per call, so a missing service is an `IpcError`, never a
//! platform rejection (A7.3).

use std::sync::Arc;

use crate::commands::app::state_of;
use crate::contract::ipc::{
    ChangesGetReq, ChangesView, GitCommitPreviewReq, GitCommitPreviewRes, GitCommitReq, GitCommitRes,
    GitMergePreviewReq, GitMergePreviewRes, GitMergeReq, GitMergeRes,
};
use crate::contract::services::GitApi;
use crate::contract::{IpcError, Req};

/// The managed `GitApi` (T11's `worktree::GitService`).
#[derive(Clone)]
pub struct GitState(pub Arc<dyn GitApi>);

pub(crate) fn git<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> Result<Arc<dyn GitApi>, IpcError> {
    state_of::<R, GitState>(app).map(|s| s.0)
}

/// `changes_get` (contract §4.2.5): `UNSUPPORTED{notGit}` for folder projects.
#[tauri::command]
pub async fn changes_get<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    req: Req<ChangesGetReq>,
) -> Result<ChangesView, IpcError> {
    git(&app)?.changes(&req).await
}

/// `git_commit_preview` (contract §4.2.5): `REFUSED{ownerAlive}` for watched
/// sessions; every path ∈ the Changes file set.
#[tauri::command]
pub async fn git_commit_preview<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    req: Req<GitCommitPreviewReq>,
) -> Result<GitCommitPreviewRes, IpcError> {
    git(&app)?.commit_preview(&req).await
}

/// `git_commit` (contract §4.2.5): behind the preview's confirm token.
#[tauri::command]
pub async fn git_commit<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    req: Req<GitCommitReq>,
) -> Result<GitCommitRes, IpcError> {
    git(&app)?.commit(&req).await
}

/// `git_merge_preview` (contract §4.2.5): no `--force`, no branch delete.
#[tauri::command]
pub async fn git_merge_preview<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    req: Req<GitMergePreviewReq>,
) -> Result<GitMergePreviewRes, IpcError> {
    git(&app)?.merge_preview(&req).await
}

/// `git_merge` (contract §4.2.5): `REFUSED{mainDirty|conflicts}`.
#[tauri::command]
pub async fn git_merge<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    req: Req<GitMergeReq>,
) -> Result<GitMergeRes, IpcError> {
    git(&app)?.merge(&req).await
}
