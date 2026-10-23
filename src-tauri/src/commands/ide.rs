//! Open in IDE, Reveal in Finder, links (contract §4.2.5, §9.4) [C9].
//!
//! Owner: T11. `link_open` lives here (A7.7).
//!
//! The service is managed state ([`IdeState`], bound by the composition root,
//! T09b); until then every command answers `UNSUPPORTED{cap: "notBuilt"}`
//! after the `Req` checks. The native confirmation of a local link
//! ([`DialogLinkConfirm`]) is built by the composition root and handed to
//! `ide::Platform::macos`.

use std::sync::Arc;

use async_trait::async_trait;

use crate::commands::app::state_of;
use crate::config::ConfigStore;
use crate::contract::ipc::{
    IdeEditorsReq, IdeEditorsRes, IdeOpenReq, IdeOpenRes, IdeRevealReq, IdeRevealRes, LinkOpenReq, LinkOpenRes,
};
use crate::contract::services::IdeApi;
use crate::contract::{IpcError, Req};
use crate::ide::links::LinkConfirm;
use crate::ide::paths::PickedRoots;

/// The managed `IdeApi` and the folders picked in this app instance.
#[derive(Clone)]
pub struct IdeState {
    pub api: Arc<dyn IdeApi>,
    pub picked: Arc<PickedRoots>,
}

impl IdeState {
    pub fn new(api: Arc<dyn IdeApi>, picked: Arc<PickedRoots>) -> IdeState {
        IdeState { api, picked }
    }
}

fn ide<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> Result<Arc<dyn IdeApi>, IpcError> {
    state_of::<R, IdeState>(app).map(|s| s.api)
}

/// `ide_open` (contract §4.2.5): §9.4 path rules.
#[tauri::command]
pub async fn ide_open<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    req: Req<IdeOpenReq>,
) -> Result<IdeOpenRes, IpcError> {
    ide(&app)?.open(&req).await?;
    Ok(IdeOpenRes {})
}

/// `ide_reveal` (contract §4.2.5): Finder by `NSWorkspace` (no subprocess).
#[tauri::command]
pub async fn ide_reveal<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    req: Req<IdeRevealReq>,
) -> Result<IdeRevealRes, IpcError> {
    ide(&app)?.reveal(&req).await?;
    Ok(IdeRevealRes {})
}

/// `ide_editors` (contract §4.2.5): VS Code and Cursor by bundle id.
#[tauri::command]
pub async fn ide_editors<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    req: Req<IdeEditorsReq>,
) -> Result<IdeEditorsRes, IpcError> {
    let _ = req;
    let api = ide(&app)?;
    tauri::async_runtime::spawn_blocking(move || api.editors())
        .await
        .map_err(|_| IpcError::internal("ide-editors"))
}

/// `link_open` (contract §4.2.5) [F-39]: http/https only; a local address
/// opens only after a native confirmation (`opened:false` if declined).
#[tauri::command]
pub async fn link_open<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    req: Req<LinkOpenReq>,
) -> Result<LinkOpenRes, IpcError> {
    let opened = ide(&app)?.link_open(&req.url).await?;
    Ok(LinkOpenRes { opened })
}

/// The production `link.localConfirm` dialog (§9.12): a Rust-owned native
/// message box naming the address; the WebView cannot answer it.
pub struct DialogLinkConfirm<R: tauri::Runtime> {
    app: tauri::AppHandle<R>,
    config: Arc<ConfigStore>,
}

impl<R: tauri::Runtime> DialogLinkConfirm<R> {
    pub fn new(app: tauri::AppHandle<R>, config: Arc<ConfigStore>) -> DialogLinkConfirm<R> {
        DialogLinkConfirm { app, config }
    }
}

#[async_trait]
impl<R: tauri::Runtime> LinkConfirm for DialogLinkConfirm<R> {
    async fn confirm_local(&self, host: &str) -> bool {
        use tauri_plugin_dialog::{DialogExt as _, MessageDialogButtons, MessageDialogKind};
        let korean = crate::commands::app::locale_and_theme(&self.config).0 == crate::contract::ipc::Locale::Ko;
        let (message, open, cancel) = crate::ide::links::confirm_text(korean, host);
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.app
            .dialog()
            .message(message)
            .title("Gilde")
            .kind(MessageDialogKind::Warning)
            .buttons(MessageDialogButtons::OkCancelCustom(open.to_string(), cancel.to_string()))
            .show(move |answer| {
                let _ = tx.send(answer);
            });
        rx.await.unwrap_or(false)
    }
}
