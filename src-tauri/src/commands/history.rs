//! History (contract §4.2.8, §5.9) [C10].
//!
//! The commands answer from `discovery::history::HistoryService`, managed by
//! the boot wiring as `HistoryState`. Until the wiring manages it, they answer
//! `UNSUPPORTED{cap: "notBuilt"}` (the service is not running in this app).
//! The work runs on a blocking thread: on-demand head/tail reads of the rows
//! a page shows (§5.9) must never block the async runtime.

use tauri::{Manager, Runtime};

use crate::contract::ipc::{HistoryPreviewReq, HistoryPreviewRes, HistoryQueryReq, HistoryQueryRes, HistoryRow, HistoryRowReq};
use crate::contract::{IpcError, Req};
use crate::discovery::history::HistoryState;

/// `history_query` (contract §4.2.8).
#[tauri::command]
pub async fn history_query<R: Runtime>(
    app: tauri::AppHandle<R>,
    req: Req<HistoryQueryReq>,
) -> Result<HistoryQueryRes, IpcError> {
    let Some(state) = app.try_state::<HistoryState>() else {
        return Err(IpcError::not_built());
    };
    let svc = state.0.clone();
    let req = req.into_inner();
    tauri::async_runtime::spawn_blocking(move || svc.query(&req))
        .await
        .map_err(|_| IpcError::internal(&uuid::Uuid::new_v4().to_string()))?
}

/// `history_preview` (contract §4.2.8): **[content]**, read on demand, memory only.
#[tauri::command]
pub async fn history_preview<R: Runtime>(
    app: tauri::AppHandle<R>,
    req: Req<HistoryPreviewReq>,
) -> Result<HistoryPreviewRes, IpcError> {
    let Some(state) = app.try_state::<HistoryState>() else {
        return Err(IpcError::not_built());
    };
    let svc = state.0.clone();
    let uid = req.into_inner().uid;
    tauri::async_runtime::spawn_blocking(move || svc.preview(&uid))
        .await
        .map_err(|_| IpcError::internal(&uuid::Uuid::new_v4().to_string()))?
}

/// `history_row` (A29.2): the `HistoryRow` of one uid — the same row
/// `history_query` builds — for the Session screen's read-only view of a
/// session World does not hold. `NOT_FOUND{session}` when the index does not
/// know the uid; the `query` rate bucket of the other history commands.
#[tauri::command]
pub async fn history_row<R: Runtime>(
    app: tauri::AppHandle<R>,
    req: Req<HistoryRowReq>,
) -> Result<HistoryRow, IpcError> {
    let Some(state) = app.try_state::<HistoryState>() else {
        return Err(IpcError::not_built());
    };
    let svc = state.0.clone();
    let uid = req.into_inner().uid;
    tauri::async_runtime::spawn_blocking(move || svc.row(&uid))
        .await
        .map_err(|_| IpcError::internal(&uuid::Uuid::new_v4().to_string()))?
}
