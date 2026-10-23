//! Interaction commands (contract §4.2.3) [C12].
//!
//! Owner: T10. Thin: the `Req` envelope already ran the contract checks; each
//! command calls the bound sessions manager (`UNSUPPORTED{notBuilt}` until
//! T09b binds it). Answer races are outcomes, never errors (§4.2.3).

use crate::commands::session::sessions;
use crate::contract::ipc::{
    InteractionAnswerReq, InteractionAnswerRes, InteractionDetailReq, InteractionDetailRes, ScreenPromptKeyReq,
    ScreenPromptKeyRes,
};
use crate::contract::{IpcError, Req};

/// `interaction_answer` (contract §4.2.3): `answered` / `alreadyAnswered` /
/// `withdrawn`; `REFUSED{watching|defaultToNo}`.
#[tauri::command]
pub async fn interaction_answer<R: tauri::Runtime>(
    req: Req<InteractionAnswerReq>,
    app: tauri::AppHandle<R>,
) -> Result<InteractionAnswerRes, IpcError> {
    sessions(&app)?.answer(req.into_inner()).await
}

/// `screen_prompt_key` (contract §4.2.3, §6.2.4): `STALE_PROMPT` under the
/// PTY write lock; idempotent by `requestId` + `promptRev`.
#[tauri::command]
pub async fn screen_prompt_key<R: tauri::Runtime>(
    req: Req<ScreenPromptKeyReq>,
    app: tauri::AppHandle<R>,
) -> Result<ScreenPromptKeyRes, IpcError> {
    sessions(&app)?.screen_key(req.into_inner()).await
}

/// `interaction_detail` (contract §4.2.3): the full tool input on demand.
#[tauri::command]
pub async fn interaction_detail<R: tauri::Runtime>(
    req: Req<InteractionDetailReq>,
    app: tauri::AppHandle<R>,
) -> Result<InteractionDetailRes, IpcError> {
    sessions(&app)?.detail(&req).await
}
