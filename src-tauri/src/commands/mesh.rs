//! `mesh_*` and `sidecar_*` commands — DESIGN §3 `dispatch / mesh`, M1-PLAN T19.
//!
//! Thin over [`SidecarManager`], like every command module: validate, call one
//! service method, return the shape `src/lib/ipc.ts` declares. The **run**
//! surface (`dispatch_*`) is T22's; what lives here is introspection plus the
//! sidecar's own lifecycle.
//!
//! Two rules this module keeps:
//!
//!   * **The account is resolved host-side.** The UI passes an `accountId`; the
//!     vendor adapters want an `Account { vendor, configDir }`. Translating here
//!     means the sidecar never learns about our account slots, and a stale id
//!     fails as `NOT_FOUND` instead of silently running under the ambient login.
//!   * **A capability question is answered by the adapter**, never by a table in
//!     Rust: `mesh_adapters` returns whatever `mesh.hello` reported (DESIGN §5-3).
//!
//! Wiring — `app.manage(...)` plus the `generate_handler!` entries — is T29's.

use std::sync::Arc;

use serde_json::json;
use tauri::State;

use crate::accounts::AccountSlot;
use crate::error::{GildeError, CmdResult, Result};
use crate::sidecar::{
    method, Account, AdapterInfo, AuthStatus, ModelInfo, QuotaSnapshot, SidecarManager,
    SidecarStatus,
};

/// Managed state: the sidecar manager.
pub type SidecarState = Arc<SidecarManager>;
/// Managed state: the account slots, for `accountId` → `Account` resolution.
/// T03 owns the loader; the manager only needs the resolved list.
pub type AccountsState = Arc<Vec<AccountSlot>>;

/// Resolve an optional `accountId` into the `Account` the adapters expect.
///
/// `None` means "the ambient CLI login", which is a legitimate choice and the
/// M1 default. An id that does not resolve is an error rather than a silent
/// fallback: running under the wrong login is exactly the failure the account
/// slots exist to prevent.
fn resolve_account(
    accounts: &[AccountSlot],
    vendor: &str,
    account_id: Option<&str>,
) -> Result<Option<Account>> {
    let Some(id) = account_id.filter(|id| !id.is_empty()) else {
        return Ok(None);
    };
    let slot = accounts
        .iter()
        .find(|slot| slot.id == id)
        .ok_or_else(|| GildeError::not_found(format!("account {id}")))?;
    Ok(Some(
        Account::new(vendor)
            .with_label(Some(slot.label.clone()))
            .with_config_dir(slot.config_dir.clone()),
    ))
}

/// Every adapter the sidecar built, with the capabilities it reports.
///
/// Empty while the sidecar is down — not an error: the PTY lane works without
/// it, and the panel shows the sidecar banner instead of a failure toast.
#[tauri::command]
pub async fn mesh_adapters(sidecar: State<'_, SidecarState>) -> CmdResult<Vec<AdapterInfo>> {
    let cached = sidecar.adapters();
    if !cached.is_empty() {
        return Ok(cached);
    }
    match sidecar.call::<_, Vec<AdapterInfo>>(method::ADAPTERS, json!({})).await {
        Ok(adapters) => Ok(adapters),
        // A down sidecar answers with an empty roster; anything else is real.
        Err(GildeError::SidecarDown) => Ok(Vec::new()),
        Err(err) => Err(err.into()),
    }
}

/// The models one vendor offers under the given login.
#[tauri::command]
pub async fn mesh_models(
    vendor: String,
    account_id: Option<String>,
    sidecar: State<'_, SidecarState>,
    accounts: State<'_, AccountsState>,
) -> CmdResult<Vec<ModelInfo>> {
    let account = resolve_account(accounts.inner(), &vendor, account_id.as_deref())?;
    Ok(sidecar
        .call(
            method::AVAILABLE_MODELS,
            json!({"vendor": vendor, "account": account}),
        )
        .await?)
}

/// The vendor's rate-limit headroom, or `None` when the adapter cannot report
/// one (`capabilities.quota == false` — cursor and gemini today).
///
/// `None` is "headroom unknown", never "no headroom": fabricating an empty
/// snapshot would tell the router an exhausted account is fully available.
#[tauri::command]
pub async fn mesh_quota(
    vendor: String,
    account_id: Option<String>,
    sidecar: State<'_, SidecarState>,
    accounts: State<'_, AccountsState>,
) -> CmdResult<Option<QuotaSnapshot>> {
    let account = resolve_account(accounts.inner(), &vendor, account_id.as_deref())?;
    Ok(sidecar
        .call(
            method::QUOTA,
            json!({"vendor": vendor, "account": account}),
        )
        .await?)
}

/// Whether a vendor CLI is logged in, and how.
#[tauri::command]
pub async fn mesh_auth(
    vendor: String,
    account_id: Option<String>,
    sidecar: State<'_, SidecarState>,
    accounts: State<'_, AccountsState>,
) -> CmdResult<AuthStatus> {
    let account = resolve_account(accounts.inner(), &vendor, account_id.as_deref())?;
    Ok(sidecar
        .call(
            method::AUTH_STATUS,
            json!({"vendor": vendor, "account": account}),
        )
        .await?)
}

/// The sidecar's lifecycle state, for the status bar and the down banner.
#[tauri::command]
pub async fn sidecar_status(sidecar: State<'_, SidecarState>) -> CmdResult<SidecarStatus> {
    Ok(sidecar.status())
}

/// Kill the current generation and start a fresh one — the "Restart sidecar"
/// button behind the circuit breaker's banner. Resets the breaker, because it
/// is an explicit user decision.
#[tauri::command]
pub async fn sidecar_restart(sidecar: State<'_, SidecarState>) -> CmdResult<SidecarStatus> {
    let manager = sidecar.inner().clone();
    Ok(manager.restart().await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::models::AccountSource;

    fn slot(id: &str, vendor: &str, dir: Option<&str>) -> AccountSlot {
        AccountSlot {
            id: id.to_string(),
            vendor: vendor.to_string(),
            label: "work".to_string(),
            config_dir: dir.map(str::to_string),
            model_hint: None,
            source: AccountSource::Manual,
            enabled: true,
            is_default: false,
            logged_in: true,
            last_seen_at: None,
            last_quota_at: None,
        }
    }

    #[test]
    fn no_account_id_means_the_ambient_login() {
        let accounts = vec![slot("a1", "claude", Some("/tmp/one"))];
        assert!(resolve_account(&accounts, "claude", None)
            .expect("ok")
            .is_none());
        assert!(resolve_account(&accounts, "claude", Some(""))
            .expect("ok")
            .is_none());
    }

    #[test]
    fn an_account_id_resolves_to_the_config_dir_the_adapters_need() {
        let accounts = vec![slot("a1", "claude", Some("/tmp/one"))];
        let account = resolve_account(&accounts, "claude", Some("a1"))
            .expect("ok")
            .expect("some");
        assert_eq!(account.vendor, "claude");
        assert_eq!(account.config_dir.as_deref(), Some("/tmp/one"));
    }

    #[test]
    fn an_unknown_account_id_is_not_found_rather_than_a_silent_fallback() {
        let accounts = vec![slot("a1", "claude", Some("/tmp/one"))];
        let err = resolve_account(&accounts, "claude", Some("nope")).expect_err("must fail");
        assert_eq!(err.code(), "NOT_FOUND");
    }
}
