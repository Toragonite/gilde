//! The account commands of contract §4.2.7: `account_add`, `account_confirm`,
//! `account_remove`, `account_set_default`, `account_set_label`,
//! `account_sign_in`, `quota_refresh`.
//!
//! Thin, like every command module: the request has already passed the `Req`
//! checks (envelope, size, shape, value rules, §4.1 rule 3); each command calls
//! one method of [`AccountsService`] and returns its typed error. The service is
//! built on first use from the managed [`AppState`] (database, paths, the
//! sidecar) with the dialog plugin's native folder picker, and managed from then
//! on, so these commands need no extra boot wiring; the integration binds the
//! sessions manager (`account_sign_in`) and World (publishing) through
//! [`AccountsService::bind_sessions`] / [`AccountsService::bind_world`].
//!
//! The pre-Gilde-2 settings and account commands (`app_settings_*`,
//! `accounts_list`, `accounts_import_cco`) are gone: settings moved to
//! `commands/settings.rs` (T09), and nothing here takes a path, an argv or a
//! secret from the WebView (§4.8).

use std::sync::Arc;

use tauri::Manager as _;

use crate::accounts::{AccountsService, DialogFolderPicker};
use crate::config::ConfigStore;
use crate::contract::ipc::{
    AccountAddReq, AccountAddRes, AccountConfirmReq, AccountConfirmRes, AccountRemoveReq,
    AccountRemoveRes, AccountSetDefaultReq, AccountSetDefaultRes, AccountSetLabelReq,
    AccountSetLabelRes, AccountSignInReq, AccountSignInRes, QuotaRefreshReq, QuotaRefreshRes,
};
use crate::contract::{IpcError, Req};
use crate::state::AppState;
use crate::store::Db;

/// Managed state: the loaded `config.json` (the boot registers it under this alias).
pub type ConfigState = Arc<ConfigStore>;

/// Managed state: the `state.db` handle (cheap to clone, one shared connection).
pub type DbState = Db;

/// The managed [`AccountsService`], built from [`AppState`] on first use.
pub fn accounts_service<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> Result<Arc<AccountsService>, IpcError> {
    if let Some(service) = app.try_state::<Arc<AccountsService>>() {
        return Ok(service.inner().clone());
    }
    let state = app.try_state::<AppState>().ok_or_else(IpcError::not_built)?;
    let service = AccountsService::new(
        state.db.clone(),
        (*state.paths).clone(),
        state.sidecar.clone(),
        Some(state.sidecar.clone()),
        Arc::new(DialogFolderPicker::new(app.clone())),
    );
    // The first registration wins; a racing second one is dropped.
    app.manage(service);
    app.try_state::<Arc<AccountsService>>()
        .map(|s| s.inner().clone())
        .ok_or_else(|| IpcError::internal("accounts-service"))
}

/// `account_add`: a native folder picker (Rust), validated by `stat`;
/// `REFUSED{notAConfigDir|alreadyAdded}`; `null` = cancelled.
#[tauri::command]
pub async fn account_add<R: tauri::Runtime>(app: tauri::AppHandle<R>, req: Req<AccountAddReq>) -> Result<AccountAddRes, IpcError> {
    let service = accounts_service(&app)?;
    let account = service.add(req.into_inner().vendor).await?;
    Ok(AccountAddRes { account })
}

/// `account_confirm`: a Discovered sibling becomes `User` and usable (C7);
/// `REFUSED{notDiscovered}`.
#[tauri::command]
pub async fn account_confirm<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    req: Req<AccountConfirmReq>,
) -> Result<AccountConfirmRes, IpcError> {
    let account = accounts_service(&app)?.confirm(&req.account_id).await?;
    Ok(AccountConfirmRes { account })
}

/// `account_remove`: user-added accounts only, never deletes the folder;
/// `REFUSED{inUse|notUserAdded}`.
#[tauri::command]
pub async fn account_remove<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    req: Req<AccountRemoveReq>,
) -> Result<AccountRemoveRes, IpcError> {
    accounts_service(&app)?.remove(&req.account_id).await?;
    Ok(AccountRemoveRes {})
}

/// `account_set_default`: one default per vendor; `REFUSED{notUsable}`.
#[tauri::command]
pub async fn account_set_default<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    req: Req<AccountSetDefaultReq>,
) -> Result<AccountSetDefaultRes, IpcError> {
    accounts_service(&app)?.set_default(&req.account_id).await?;
    Ok(AccountSetDefaultRes {})
}

/// `account_set_label`: 1..=64 chars, sanitized.
#[tauri::command]
pub async fn account_set_label<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    req: Req<AccountSetLabelReq>,
) -> Result<AccountSetLabelRes, IpcError> {
    accounts_service(&app)?.set_label(&req.account_id, &req.label).await?;
    Ok(AccountSetLabelRes {})
}

/// `account_sign_in`: the CLI's own sign-in in a Terminal-mode session with that
/// account's env; Gilde handles no credentials. Idempotent by `clientRequestId`.
#[tauri::command]
pub async fn account_sign_in<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    req: Req<AccountSignInReq>,
) -> Result<AccountSignInRes, IpcError> {
    accounts_service(&app)?.sign_in(&req).await
}

/// `quota_refresh`: ≤ 1 per account per 60 s (agy 5 min), else the cached value
/// with `stale: true`; `SIDECAR_DOWN`, `NOT_READY{sidecarStarting}`.
#[tauri::command]
pub async fn quota_refresh<R: tauri::Runtime>(app: tauri::AppHandle<R>, req: Req<QuotaRefreshReq>) -> Result<QuotaRefreshRes, IpcError> {
    let quota = accounts_service(&app)?.refresh_quota(&req.account_id).await?;
    Ok(QuotaRefreshRes { quota })
}
