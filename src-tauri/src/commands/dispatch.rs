//! `dispatch_*` commands — the headless lane's IPC surface (DESIGN §3, T22).
//!
//! Thin, like every command module: validate what only the command layer knows
//! (the managed state, the `Channel`), call one [`DispatchEngine`] method, hand
//! back a small value. Nothing here decides anything.
//!
//! Two shapes are worth naming:
//!
//! * **`dispatch_create` returns as soon as the rows and worktrees exist.** The
//!   runs themselves report through `gilde://dispatch/event`; a command that
//!   blocked until an agent finished would block for up to 45 minutes.
//! * **`dispatch_route` is the composer's "Auto" preview.** It is one of only
//!   two callers of `fleet.route` (the other is the `auto` target inside
//!   `dispatch_create`), which is what keeps routing to the one place the user
//!   asked for it (DESIGN §9-7, T22 acceptance 10).

use std::sync::Arc;

use serde_json::Value;
use tauri::ipc::Channel;
use tauri::State;

use crate::dispatch::{
    DispatchEngine, DispatchScope, DispatchSpec, DispatchTask, DispatchTaskDetail, RouteDecision,
    RunOutcome,
};
use crate::error::CmdResult;
use crate::pty::{PtyChunk, PtySessionInfo};
use crate::supervise::kill::KillReport;

/// Managed state: the dispatch engine.
pub type DispatchState = Arc<DispatchEngine>;

/// Fan one prompt out to N `{vendor, model?, accountId?}` targets.
///
/// A target that cannot run is refused on its own run row with a reason; the
/// other targets are unaffected (T22 acceptance 4).
#[tauri::command]
pub async fn dispatch_create(
    spec: DispatchSpec,
    dispatch: State<'_, DispatchState>,
) -> CmdResult<DispatchTask> {
    let engine = dispatch.inner().clone();
    Ok(engine.create(spec).await?)
}

/// Recent tasks for the board. Runs are fetched per task by `dispatch_get`.
#[tauri::command]
pub async fn dispatch_list(
    scope: DispatchScope,
    limit: u32,
    dispatch: State<'_, DispatchState>,
) -> CmdResult<Vec<DispatchTask>> {
    Ok(dispatch.list(scope, limit).await?)
}

/// One task and its runs — what restores a board after a restart.
#[tauri::command]
pub async fn dispatch_get(
    task_id: String,
    dispatch: State<'_, DispatchState>,
) -> CmdResult<DispatchTaskDetail> {
    Ok(dispatch.get(&task_id).await?)
}

/// Cancel every unfinished run of a task. Answers with what was actually
/// signalled — never a bare "ok" (DESIGN §8-6).
#[tauri::command]
pub async fn dispatch_cancel(
    task_id: String,
    dispatch: State<'_, DispatchState>,
) -> CmdResult<Vec<KillReport>> {
    Ok(dispatch.cancel_task(&task_id).await?)
}

/// Cancel one run. The kill is the authority; `mesh.cancel` runs beside it and
/// cannot delay it (DESIGN §5-9).
#[tauri::command]
pub async fn dispatch_cancel_run(
    run_id: String,
    dispatch: State<'_, DispatchState>,
) -> CmdResult<KillReport> {
    Ok(dispatch.cancel_run(&run_id).await?)
}

/// Continue a finished run as a new run in the same task. Requires a non-empty
/// `conversationId`; an empty one means the vendor said "not resumable".
#[tauri::command]
pub async fn dispatch_resume(
    run_id: String,
    message: String,
    dispatch: State<'_, DispatchState>,
) -> CmdResult<RunOutcome> {
    let engine = dispatch.inner().clone();
    Ok(engine.resume(&run_id, &message).await?)
}

/// Ask the fleet which vendor should take this prompt, and show the answer
/// **before** running it. The composer's `Auto` affordance.
#[tauri::command]
pub async fn dispatch_route(
    prompt: String,
    policy: Option<Value>,
    dispatch: State<'_, DispatchState>,
) -> CmdResult<RouteDecision> {
    Ok(dispatch.route(&prompt, policy).await?)
}

/// Open a terminal in the run's worktree — the bridge between the two lanes.
#[tauri::command]
pub async fn dispatch_open_terminal(
    run_id: String,
    on_chunk: Channel<PtyChunk>,
    dispatch: State<'_, DispatchState>,
) -> CmdResult<PtySessionInfo> {
    Ok(dispatch.open_terminal(&run_id, on_chunk).await?)
}

/// The `~/.gilde/dispatch/<task>/<run>/` directory, for "reveal in
/// Finder". The files are the record, so the user must be able to reach them.
#[tauri::command]
pub async fn dispatch_artifact_dir(
    run_id: String,
    dispatch: State<'_, DispatchState>,
) -> CmdResult<String> {
    Ok(dispatch
        .artifact_dir(&run_id)
        .await?
        .display()
        .to_string())
}
