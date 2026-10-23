//! Conversation content channels (contract §4.2.4) over the ContentHub.
//!
//! Owner: T04.
//!
//! The hub is managed state ([`ConvoState`]); the app's setup (T09/T10 wiring)
//! does `app.manage(ConvoState(hub))`. Until it is managed every command
//! answers `UNSUPPORTED{cap: "notBuilt"}` (the wave-0 stub behaviour), after
//! the request passed the contract checks of `Req` (envelope, size, shape,
//! value rules).
//!
//! `convo_subscribe` and `convo_load_older` read transcript bytes (≤ 4 MiB /
//! ≤ 8 MiB), so they run on the blocking pool, never on the IPC thread.

use std::sync::Arc;

use tauri::Manager as _;

use crate::contract::content::ConvoEnvelope;
use crate::contract::ipc::{
    ConvoLoadOlderReq, ConvoLoadOlderRes, ConvoSubscribeReq, ConvoSubscribeRes, ConvoUnsubscribeReq, ConvoUnsubscribeRes,
};
use crate::contract::{IpcError, Req};
use crate::follow::hub::{ContentHub, ConvoChannel};

/// The managed ContentHub.
pub struct ConvoState(pub Arc<ContentHub>);

fn hub<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> Result<Arc<ContentHub>, IpcError> {
    app.try_state::<ConvoState>().map(|s| s.0.clone()).ok_or_else(IpcError::not_built)
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, IpcError> + Send + 'static) -> Result<T, IpcError> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(|_| IpcError::internal("convo-task"))?
}

/// `convo_subscribe` (contract §4.2.4) (+ `Channel<ConvoEnvelope>`).
#[tauri::command]
pub async fn convo_subscribe<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    req: Req<ConvoSubscribeReq>,
    channel: tauri::ipc::Channel<ConvoEnvelope>,
) -> Result<ConvoSubscribeRes, IpcError> {
    let hub = hub(&app)?;
    let req = req.into_inner();
    let channel: Arc<dyn ConvoChannel> = Arc::new(channel);
    blocking(move || hub.subscribe(&req.uid, &req.view_id, req.tail_blocks, channel)).await
}

/// `convo_unsubscribe` (contract §4.2.4): idempotent.
#[tauri::command]
pub async fn convo_unsubscribe<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    req: Req<ConvoUnsubscribeReq>,
) -> Result<ConvoUnsubscribeRes, IpcError> {
    let hub = hub(&app)?;
    hub.unsubscribe(&req.sub_id);
    Ok(ConvoUnsubscribeRes {})
}

/// `convo_load_older` (contract §4.2.4): the blocks arrive as an `Older`
/// event; `NOT_FOUND{subscription|block}`.
#[tauri::command]
pub async fn convo_load_older<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    req: Req<ConvoLoadOlderReq>,
) -> Result<ConvoLoadOlderRes, IpcError> {
    let hub = hub(&app)?;
    let req = req.into_inner();
    blocking(move || hub.load_older(&req.sub_id, &req.before_block_id, req.count)).await?;
    Ok(ConvoLoadOlderRes {})
}
