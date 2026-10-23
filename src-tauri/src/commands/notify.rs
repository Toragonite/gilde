//! Visibility, input activity, attention and notification commands (contract §4.2.1, §4.2.8, §8).
//!
//! Owner: T12 (A7.7: `app_view_state`, `app_input_activity` and `queue_later`
//! live here). Every command answers from `notify::NotifyState`, managed by
//! the composition root (T09b, A10.6); until it is managed, each answers
//! `UNSUPPORTED{cap: "notBuilt"}` after the request passed the contract checks
//! of `Req` (envelope, size, shape, value rules).

use tauri::{Manager, Runtime};

use crate::contract::ipc::{
    AppInputActivityReq, AppInputActivityRes, AppViewStateRes, NotifyPauseReq, NotifyPauseRes,
    NotifyPermissionRequestReq, NotifyPermissionRequestRes, NotifyResumeReq, NotifyResumeRes, NotifyTestReq,
    NotifyTestRes, QueueLaterReq, QueueLaterRes, ViewState,
};
use crate::contract::{IpcError, Req};
use crate::notify::NotifyState;

/// `app_input_activity` (§4.2.1): the timestamp for notification rule 5.
/// Carries no content; extra calls only move the timestamp.
#[tauri::command]
pub async fn app_input_activity<R: Runtime>(
    app: tauri::AppHandle<R>,
    req: Req<AppInputActivityReq>,
) -> Result<AppInputActivityRes, IpcError> {
    let Some(state) = app.try_state::<NotifyState>() else {
        return Err(IpcError::not_built());
    };
    let _ = req.into_inner();
    state.notify.input_activity();
    Ok(AppInputActivityRes {})
}

/// `app_view_state` (§4.2.1) [F-04]: what is on screen, memory only.
#[tauri::command]
pub async fn app_view_state<R: Runtime>(
    app: tauri::AppHandle<R>,
    req: Req<ViewState>,
) -> Result<AppViewStateRes, IpcError> {
    let Some(state) = app.try_state::<NotifyState>() else {
        return Err(IpcError::not_built());
    };
    state.notify.view_state(req.into_inner());
    Ok(AppViewStateRes {})
}

/// `queue_later` (§4.2.8): 30 min on the uptime clock (World, Q5);
/// `NOT_FOUND{queueItem}`.
#[tauri::command]
pub async fn queue_later<R: Runtime>(
    app: tauri::AppHandle<R>,
    req: Req<QueueLaterReq>,
) -> Result<QueueLaterRes, IpcError> {
    let Some(state) = app.try_state::<NotifyState>() else {
        return Err(IpcError::not_built());
    };
    let until = state.world.queue_later(&req.into_inner().uid)?;
    Ok(QueueLaterRes { until })
}

/// `notify_pause` (§4.2.8, ux-ia §6.5). "Until I resume" answers
/// `attention::PAUSED_UNTIL_RESUME` (A16.2).
#[tauri::command]
pub async fn notify_pause<R: Runtime>(
    app: tauri::AppHandle<R>,
    req: Req<NotifyPauseReq>,
) -> Result<NotifyPauseRes, IpcError> {
    let Some(state) = app.try_state::<NotifyState>() else {
        return Err(IpcError::not_built());
    };
    let paused_until = state.notify.pause(req.into_inner().until);
    Ok(NotifyPauseRes { paused_until })
}

/// `notify_resume` (§4.2.8).
#[tauri::command]
pub async fn notify_resume<R: Runtime>(
    app: tauri::AppHandle<R>,
    req: Req<NotifyResumeReq>,
) -> Result<NotifyResumeRes, IpcError> {
    let Some(state) = app.try_state::<NotifyState>() else {
        return Err(IpcError::not_built());
    };
    let _ = req.into_inner();
    state.notify.resume();
    Ok(NotifyResumeRes {})
}

/// `notify_permission_request` (§4.2.8): Rust asks macOS (the only place a
/// permission prompt can appear).
#[tauri::command]
pub async fn notify_permission_request<R: Runtime>(
    app: tauri::AppHandle<R>,
    req: Req<NotifyPermissionRequestReq>,
) -> Result<NotifyPermissionRequestRes, IpcError> {
    let Some(state) = app.try_state::<NotifyState>() else {
        return Err(IpcError::not_built());
    };
    let _ = req.into_inner();
    let notify = state.notify.clone();
    let granted = notify.permission_request().await;
    Ok(NotifyPermissionRequestRes { granted })
}

/// `notify_test` (§4.2.8): class `probe`, one call per 60 s, else
/// `BUSY{retryAfterMs}`.
#[tauri::command]
pub async fn notify_test<R: Runtime>(
    app: tauri::AppHandle<R>,
    req: Req<NotifyTestReq>,
) -> Result<NotifyTestRes, IpcError> {
    let Some(state) = app.try_state::<NotifyState>() else {
        return Err(IpcError::not_built());
    };
    let _ = req.into_inner();
    state.test_permit().map_err(IpcError::busy)?;
    let notify = state.notify.clone();
    let delivered = notify.test().await;
    Ok(NotifyTestRes { delivered })
}
