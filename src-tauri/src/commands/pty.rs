//! `pty_*` commands (contract §4.2.4, §6.2.3, §6.2.6) — thin: validate (the
//! `Req` envelope already did), resolve the PTY service, call it.
//!
//! * `pty_attach` takes the view's `Channel<PtyChunk>`; one view per PTY, the
//!   previous one hears `Detached{otherView}`. Attach first, then snapshot.
//! * `pty_snapshot` answers with **raw bytes** (`GLDP` header + ring).
//! * `pty_write` takes base64, so pasted emoji and IME output reach the child
//!   as the exact bytes the view produced.
//! * `pty_paste` is the bracketed paste of §6.2.3, ungated by status.
//!
//! The service is resolved per call: an app without it answers
//! `UNSUPPORTED{notBuilt}`, an unknown PTY `UNSUPPORTED{noPty}`.

use std::sync::Arc;

use base64::Engine as _;
use tauri::ipc::{Channel, Response};

use crate::commands::app::state_of;
use crate::contract::content::PtyChunk;
use crate::contract::ipc::{
    PtyAttachReq, PtyDetachReq, PtyDetachRes, PtyInfo, PtyPasteReq, PtyPasteRes, PtyResizeReq,
    PtyResizeRes, PtySnapshotReq, PtyWriteReq, PtyWriteRes,
};
use crate::contract::services::PtyApi;
use crate::contract::{IpcError, Req};
use crate::pty::PtyManager;

/// Managed state: the PTY manager.
pub type PtyState = Arc<PtyManager>;

/// `pty_attach` (+ `Channel<PtyChunk>`).
#[tauri::command]
pub async fn pty_attach<R: tauri::Runtime>(
    req: Req<PtyAttachReq>,
    channel: Channel<PtyChunk>,
    app: tauri::AppHandle<R>,
) -> Result<PtyInfo, IpcError> {
    let req = req.into_inner();
    let pty: PtyState = state_of(&app)?;
    pty.attach(&req.pty_id, req.view_id, Arc::new(channel))
}

/// `pty_snapshot`: raw bytes.
#[tauri::command]
pub async fn pty_snapshot<R: tauri::Runtime>(
    req: Req<PtySnapshotReq>,
    app: tauri::AppHandle<R>,
) -> Result<Response, IpcError> {
    let pty: PtyState = state_of(&app)?;
    Ok(Response::new(pty.snapshot(&req.pty_id)?))
}

/// `pty_write`: ≤ 1 MiB decoded (validated by the envelope).
#[tauri::command]
pub async fn pty_write<R: tauri::Runtime>(
    req: Req<PtyWriteReq>,
    app: tauri::AppHandle<R>,
) -> Result<PtyWriteRes, IpcError> {
    let pty: PtyState = state_of(&app)?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(req.b64.as_bytes())
        .map_err(|_| IpcError::invalid_argument("b64", "pattern"))?;
    pty.write(&req.pty_id, &bytes).await?;
    Ok(PtyWriteRes {})
}

/// `pty_paste`: bracketed paste, then CR iff `submit`.
#[tauri::command]
pub async fn pty_paste<R: tauri::Runtime>(
    req: Req<PtyPasteReq>,
    app: tauri::AppHandle<R>,
) -> Result<PtyPasteRes, IpcError> {
    let pty: PtyState = state_of(&app)?;
    pty.paste(&req.pty_id, &req.text, req.submit).await?;
    Ok(PtyPasteRes {})
}

/// `pty_resize`: clamped, never rejected; a no-op size is ignored.
#[tauri::command]
pub async fn pty_resize<R: tauri::Runtime>(
    req: Req<PtyResizeReq>,
    app: tauri::AppHandle<R>,
) -> Result<PtyResizeRes, IpcError> {
    let pty: PtyState = state_of(&app)?;
    pty.resize(&req.pty_id, req.cols, req.rows).await?;
    Ok(PtyResizeRes {})
}

/// `pty_detach`: idempotent.
#[tauri::command]
pub async fn pty_detach<R: tauri::Runtime>(
    req: Req<PtyDetachReq>,
    app: tauri::AppHandle<R>,
) -> Result<PtyDetachRes, IpcError> {
    let pty: PtyState = state_of(&app)?;
    pty.detach(&req.pty_id, &req.view_id);
    Ok(PtyDetachRes {})
}
