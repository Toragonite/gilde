//! Owner focus, "Go to {owner}" (contract §4.2.5, §9.5) [C6].
//!
//! Owner: T12. Answers from `macos::owner::OwnerState`, managed by the
//! composition root (T09b, A10.6); until it is managed, the command answers
//! `UNSUPPORTED{cap: "notBuilt"}` after the request passed the contract checks
//! of `Req` (envelope, size, shape, value rules).

use tauri::{Manager, Runtime};

use crate::contract::ipc::{OwnerFocusReq, OwnerFocusRes};
use crate::contract::{IpcError, Req};
use crate::macos::owner::OwnerState;

/// `owner_focus` (§4.2.5): `{precision: "window" | "app"}`; `UNSUPPORTED{noOwner}`.
#[tauri::command]
pub async fn owner_focus<R: Runtime>(
    app: tauri::AppHandle<R>,
    req: Req<OwnerFocusReq>,
) -> Result<OwnerFocusRes, IpcError> {
    let Some(state) = app.try_state::<OwnerState>() else {
        return Err(IpcError::not_built());
    };
    let service = state.0.clone();
    service.focus(&req.into_inner().uid).await
}
