//! Turn commands (contract §4.2.3).
//!
//! Owner: T10. Thin: the `Req` envelope already ran the contract checks; each
//! command calls the bound sessions manager (`UNSUPPORTED{notBuilt}` until
//! T09b binds it), which routes to the session's actor (§4.1 rule 5).

use crate::commands::session::sessions;
use crate::contract::ipc::{
    TurnCancelQueuedReq, TurnCancelQueuedRes, TurnForceStopReq, TurnForceStopRes, TurnInterruptReq, TurnInterruptRes,
    TurnRetryReq, TurnRetryRes, TurnSendReq, TurnSendRes,
};
use crate::contract::{IpcError, Req};

/// `turn_send` (contract §4.2.3): idempotent by `clientMsgId`.
#[tauri::command]
pub async fn turn_send<R: tauri::Runtime>(
    req: Req<TurnSendReq>,
    app: tauri::AppHandle<R>,
) -> Result<TurnSendRes, IpcError> {
    sessions(&app)?.send(req.into_inner()).await
}

/// `turn_cancel_queued` (contract §4.2.3): `NOT_FOUND{message}`.
#[tauri::command]
pub async fn turn_cancel_queued<R: tauri::Runtime>(
    req: Req<TurnCancelQueuedReq>,
    app: tauri::AppHandle<R>,
) -> Result<TurnCancelQueuedRes, IpcError> {
    sessions(&app)?.cancel_queued(req.into_inner()).await
}

/// `turn_interrupt` (contract §4.2.3).
#[tauri::command]
pub async fn turn_interrupt<R: tauri::Runtime>(
    req: Req<TurnInterruptReq>,
    app: tauri::AppHandle<R>,
) -> Result<TurnInterruptRes, IpcError> {
    sessions(&app)?.interrupt(req.into_inner()).await
}

/// `turn_force_stop` (contract §4.2.3): fenced SIGTERM, never SIGINT or SIGKILL.
#[tauri::command]
pub async fn turn_force_stop<R: tauri::Runtime>(
    req: Req<TurnForceStopReq>,
    app: tauri::AppHandle<R>,
) -> Result<TurnForceStopRes, IpcError> {
    let stopped = sessions(&app)?.force_stop(&req.uid).await?;
    Ok(TurnForceStopRes { stopped })
}

/// `turn_retry` (contract §4.2.3): `REFUSED{notInError}`.
#[tauri::command]
pub async fn turn_retry<R: tauri::Runtime>(
    req: Req<TurnRetryReq>,
    app: tauri::AppHandle<R>,
) -> Result<TurnRetryRes, IpcError> {
    sessions(&app)?.retry(req.into_inner()).await
}
