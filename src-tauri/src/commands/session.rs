//! Session commands (contract §4.2.2, §4.2.3) over the sessions manager (§6.0).
//!
//! Owner: T10. Thin: the `Req` envelope already ran the contract checks
//! (envelope, size, shape, value rules); each command resolves the bound
//! sessions manager ([`SessionActorApi`], `Services::bind_sessions`, T09b) and
//! calls it. Until the composition root binds it every command answers
//! `UNSUPPORTED{cap: "notBuilt"}` — resolved per call, so a missing service is
//! an `IpcError`, never a platform rejection (A7.3).

use std::sync::Arc;

use crate::commands::app::{state_of, ServicesState};
use crate::contract::ipc::{
    NewSessionOptions, NewsessionOptionsReq, SessionContinueReq, SessionContinueRes, SessionDismissReq,
    SessionDismissRes, SessionMarkSeenReq, SessionMarkSeenRes, SessionModelsReq, SessionModelsRes,
    SessionRetryAsTerminalReq, SessionRetryAsTerminalRes, SessionSetModelReq, SessionSetModelRes,
    SessionSetPermissionModeReq, SessionSetPermissionModeRes, SessionStartReq, SessionStartRes, SessionStopPreviewReq,
    SessionStopPreviewRes, SessionStopReq, SessionStopRes, SessionTechnicalReq, SessionTechnicalRes,
};
use crate::contract::services::{SessionActorApi, WorldApi};
use crate::contract::{IpcError, Req};

/// The bound sessions manager, or `UNSUPPORTED{notBuilt}`.
pub(crate) fn sessions<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> Result<Arc<dyn SessionActorApi>, IpcError> {
    let services: ServicesState = state_of(app)?;
    services.sessions().ok_or_else(IpcError::not_built)
}

/// The bound World, or `UNSUPPORTED{notBuilt}`.
fn world<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> Result<Arc<dyn WorldApi>, IpcError> {
    let services: ServicesState = state_of(app)?;
    services.world().ok_or_else(IpcError::not_built)
}

/// `newsession_options` (contract §4.2.2): what is known within 1 s.
#[tauri::command]
pub async fn newsession_options<R: tauri::Runtime>(
    req: Req<NewsessionOptionsReq>,
    app: tauri::AppHandle<R>,
) -> Result<NewSessionOptions, IpcError> {
    sessions(&app)?.options(&req).await
}

/// `session_start` (contract §4.2.2): idempotent by `clientRequestId`.
#[tauri::command]
pub async fn session_start<R: tauri::Runtime>(
    req: Req<SessionStartReq>,
    app: tauri::AppHandle<R>,
) -> Result<SessionStartRes, IpcError> {
    sessions(&app)?.start(req.into_inner()).await
}

/// `session_continue` (contract §4.2.2, §6.5): liveness re-checked inside.
#[tauri::command]
pub async fn session_continue<R: tauri::Runtime>(
    req: Req<SessionContinueReq>,
    app: tauri::AppHandle<R>,
) -> Result<SessionContinueRes, IpcError> {
    sessions(&app)?.continue_session(req.into_inner()).await
}

/// `session_retry_as_terminal` (contract §4.2.2, §6.1.2).
#[tauri::command]
pub async fn session_retry_as_terminal<R: tauri::Runtime>(
    req: Req<SessionRetryAsTerminalReq>,
    app: tauri::AppHandle<R>,
) -> Result<SessionRetryAsTerminalRes, IpcError> {
    sessions(&app)?.retry_as_terminal(req.into_inner()).await
}

/// `session_stop_preview` (contract §4.2.2): `UNSUPPORTED{notLive}`.
#[tauri::command]
pub async fn session_stop_preview<R: tauri::Runtime>(
    req: Req<SessionStopPreviewReq>,
    app: tauri::AppHandle<R>,
) -> Result<SessionStopPreviewRes, IpcError> {
    sessions(&app)?.stop_preview(&req.uid).await
}

/// `session_stop` (contract §4.2.2): the driver's stop sequence; never SIGKILL.
#[tauri::command]
pub async fn session_stop<R: tauri::Runtime>(
    req: Req<SessionStopReq>,
    app: tauri::AppHandle<R>,
) -> Result<SessionStopRes, IpcError> {
    sessions(&app)?.stop(req.into_inner()).await
}

/// `session_mark_seen` (contract §4.2.2): World's marks.
#[tauri::command]
pub async fn session_mark_seen<R: tauri::Runtime>(
    req: Req<SessionMarkSeenReq>,
    app: tauri::AppHandle<R>,
) -> Result<SessionMarkSeenRes, IpcError> {
    world(&app)?.mark_seen(&req.uid)?;
    Ok(SessionMarkSeenRes {})
}

/// `session_dismiss` (contract §4.2.2): `REFUSED{notInState}`.
#[tauri::command]
pub async fn session_dismiss<R: tauri::Runtime>(
    req: Req<SessionDismissReq>,
    app: tauri::AppHandle<R>,
) -> Result<SessionDismissRes, IpcError> {
    let req = req.into_inner();
    world(&app)?.dismiss(&req.uid, req.what)?;
    Ok(SessionDismissRes {})
}

/// `session_models` (contract §4.2.2) [F-23].
#[tauri::command]
pub async fn session_models<R: tauri::Runtime>(
    req: Req<SessionModelsReq>,
    app: tauri::AppHandle<R>,
) -> Result<SessionModelsRes, IpcError> {
    sessions(&app)?.models(&req.uid).await
}

/// `session_technical` (contract §4.2.2): on demand, never in the store.
#[tauri::command]
pub async fn session_technical<R: tauri::Runtime>(
    req: Req<SessionTechnicalReq>,
    app: tauri::AppHandle<R>,
) -> Result<SessionTechnicalRes, IpcError> {
    sessions(&app)?.technical(&req.uid).await
}

/// `session_set_model` (contract §4.2.3).
#[tauri::command]
pub async fn session_set_model<R: tauri::Runtime>(
    req: Req<SessionSetModelReq>,
    app: tauri::AppHandle<R>,
) -> Result<SessionSetModelRes, IpcError> {
    let req = req.into_inner();
    let applies_from = sessions(&app)?.set_model(&req.uid, req.model).await?;
    Ok(SessionSetModelRes { applies_from })
}

/// `session_set_permission_mode` (contract §4.2.3): Conversation only;
/// `BypassPermissions` → `REFUSED{needsLaunchFlag}` unless launched with it.
#[tauri::command]
pub async fn session_set_permission_mode<R: tauri::Runtime>(
    req: Req<SessionSetPermissionModeReq>,
    app: tauri::AppHandle<R>,
) -> Result<SessionSetPermissionModeRes, IpcError> {
    sessions(&app)?.set_permission_mode(&req.uid, req.mode).await?;
    Ok(SessionSetPermissionModeRes {})
}
