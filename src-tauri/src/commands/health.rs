//! Health and leftovers (contract §4.2.7, §3.4.10) and the undo window.
//!
//! Owner: T09. The records come from the shared process scan
//! (`supervise::health::HealthState`); nothing here carries an argv except
//! `health_process_detail`, and that one only in the redacted form of §9.7.
//! `health_stop` stops its targets after the 10 s undo window with an identity
//! re-check per target, and never SIGKILLs a vendor process.


use crate::commands::app::{state_of, HealthStateRef, LifecycleState};
use crate::contract::ipc::{
    HealthProcessDetailReq, HealthProcessDetailRes, HealthProcessesReq, HealthProcessesRes,
    HealthStopPreviewReq, HealthStopPreviewRes, HealthStopReq, HealthStopRes, UndoCancelReq,
    UndoCancelRes,
};
use crate::contract::{IpcError, Req};

/// `health_processes` (contract §4.2.7) [F-24]: argv never included.
#[tauri::command]
pub async fn health_processes<R: tauri::Runtime>(
    req: Req<HealthProcessesReq>,
    app: tauri::AppHandle<R>,
) -> Result<HealthProcessesRes, IpcError> {
    let _ = req;
    let health: HealthStateRef = state_of(&app)?;
    Ok(HealthProcessesRes {
        processes: health.records(),
    })
}

/// `health_process_detail` (contract §4.2.7): the redacted command line.
#[tauri::command]
pub async fn health_process_detail<R: tauri::Runtime>(
    req: Req<HealthProcessDetailReq>,
    app: tauri::AppHandle<R>,
) -> Result<HealthProcessDetailRes, IpcError> {
    let health: HealthStateRef = state_of(&app)?;
    health.detail(&req.process_key)
}

/// `health_stop_preview` (contract §4.2.7): 1..=50 keys, each from the current
/// leftover list; anything that cannot be identified is `refused`.
#[tauri::command]
pub async fn health_stop_preview<R: tauri::Runtime>(
    req: Req<HealthStopPreviewReq>,
    app: tauri::AppHandle<R>,
) -> Result<HealthStopPreviewRes, IpcError> {
    let health: HealthStateRef = state_of(&app)?;
    Ok(health.stop_preview(&req.process_keys))
}

/// `health_stop` (contract §4.2.7): `CONFLICT{changed}` if the previewed
/// targets changed; otherwise the stop is scheduled behind the undo window.
#[tauri::command]
pub async fn health_stop<R: tauri::Runtime>(
    req: Req<HealthStopReq>,
    app: tauri::AppHandle<R>,
) -> Result<HealthStopRes, IpcError> {
    let health: HealthStateRef = state_of(&app)?;
    let lifecycle: LifecycleState = state_of(&app)?;
    let Some(sup) = lifecycle.supervisor() else {
        return Err(IpcError::not_ready("bootSweep"));
    };
    health.stop(&req.confirm_token, sup, crate::supervise::reaper::UNDO_WINDOW)
}

/// `undo_cancel` (contract §4.2.7): a Clean-up stop or a boot-sweep
/// termination within its 10 s window; `NOT_FOUND{token}` once expired.
#[tauri::command]
pub async fn undo_cancel<R: tauri::Runtime>(
    req: Req<UndoCancelReq>,
    app: tauri::AppHandle<R>,
) -> Result<UndoCancelRes, IpcError> {
    let health: HealthStateRef = state_of(&app)?;
    let token = req.undo_token.as_str();
    if health.undo(token) || crate::supervise::reaper::undo_cancel(token) {
        Ok(UndoCancelRes { cancelled: true })
    } else {
        Err(IpcError::not_found("token"))
    }
}
