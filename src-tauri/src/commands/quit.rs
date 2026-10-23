//! Quit and Continue all (contract §4.2.1, §10.2) [M10, C5].
//!
//! Owner: T09. The sequence itself lives in `lifecycle::quit`; these commands
//! are its IPC edge: the preview (with its single-use, 60 s confirm token
//! bound to the previewed set of In-Gilde sessions), the quit (record first,
//! then the bounded stop sweep — never SIGKILL a vendor process — then the
//! exit), cancel, Continue all, and dismiss.


use crate::commands::app::{state_of, LifecycleState, ServicesState};
use crate::contract::ipc::{
    AppQuitCancelReq, AppQuitCancelRes, AppQuitPreviewReq, AppQuitPreviewRes, AppQuitReq,
    AppQuitRes, ContinueAllReq, ContinueAllRes, QuitRecordDismissReq, QuitRecordDismissRes,
};
use crate::contract::{IpcError, Req};
use crate::lifecycle::boot::{preview_for, Lifecycle};
use crate::state::Services;
use crate::store::dao;

/// `app_quit_preview` (contract §4.2.1): the In-Gilde sessions (with what
/// each waits for — lost with it, `sheet.quit.pendingLost`), the sessions
/// elsewhere (never touched) and the hosted teams.
#[tauri::command]
pub async fn app_quit_preview<R: tauri::Runtime>(
    req: Req<AppQuitPreviewReq>,
    app: tauri::AppHandle<R>,
) -> Result<AppQuitPreviewRes, IpcError> {
    let _ = req;
    let lifecycle: LifecycleState = state_of(&app)?;
    let services: ServicesState = state_of(&app)?;
    quit_preview(&lifecycle, &services).await
}

pub async fn quit_preview(lifecycle: &Lifecycle, services: &Services) -> Result<AppQuitPreviewRes, IpcError> {
    let (Some(sup), Some(quit)) = (lifecycle.supervisor(), lifecycle.quit()) else {
        return Err(IpcError::not_ready("bootSweep"));
    };
    let preview = preview_for(services, &sup).await;
    let res = AppQuitPreviewRes {
        sessions: preview.sessions.clone(),
        elsewhere: preview.elsewhere,
        teams: preview.teams.clone(),
        confirm_token: quit.issue(preview),
    };
    Ok(res)
}

/// `app_quit` (contract §4.2.1, §10.2): `CONFLICT{changed}` if sessions
/// started or ended since the preview; otherwise the record is written, the
/// sessions are stopped (progress as `Quit{…}` store ops) and the app exits.
#[tauri::command]
pub async fn app_quit<R: tauri::Runtime>(
    req: Req<AppQuitReq>,
    app: tauri::AppHandle<R>,
) -> Result<AppQuitRes, IpcError> {
    let req = req.into_inner();
    let lifecycle: LifecycleState = state_of(&app)?;
    let services: ServicesState = state_of(&app)?;
    let (Some(sup), Some(quit)) = (lifecycle.supervisor(), lifecycle.quit()) else {
        return Err(IpcError::not_ready("bootSweep"));
    };
    let current = preview_for(&services, &sup).await;
    let preview = quit.redeem(&req.confirm_token, &current)?;
    tauri::async_runtime::spawn(async move {
        let report = quit.run(preview, services.sessions()).await;
        tracing::info!(
            target: "app",
            stopped = report.stopped,
            total = report.total,
            survivors = report.survivor_pids.len(),
            "quit complete; exiting"
        );
        app.exit(0);
    });
    Ok(AppQuitRes {})
}

/// `app_quit_cancel` (contract §4.2.1): every outstanding confirm token is void.
#[tauri::command]
pub async fn app_quit_cancel<R: tauri::Runtime>(
    req: Req<AppQuitCancelReq>,
    app: tauri::AppHandle<R>,
) -> Result<AppQuitCancelRes, IpcError> {
    let _ = req;
    let lifecycle: LifecycleState = state_of(&app)?;
    if let Some(quit) = lifecycle.quit() {
        quit.cancel();
    }
    Ok(AppQuitCancelRes {})
}

/// `continue_all` (contract §4.2.1, §6.5 rule 4): `NOT_READY{bootSweep}`
/// before the gate opens; `null`/`[]` = every item; the sessions manager
/// starts them in quit order, ≤ 3 in flight, failures never abort the rest.
/// Started items leave the record; an empty record is deleted.
#[tauri::command]
pub async fn continue_all<R: tauri::Runtime>(
    req: Req<ContinueAllReq>,
    app: tauri::AppHandle<R>,
) -> Result<ContinueAllRes, IpcError> {
    let lifecycle: LifecycleState = state_of(&app)?;
    let services: ServicesState = state_of(&app)?;
    continue_all_core(&lifecycle, &services, req.into_inner()).await
}

pub async fn continue_all_core(
    lifecycle: &Lifecycle,
    services: &Services,
    req: ContinueAllReq,
) -> Result<ContinueAllRes, IpcError> {
    lifecycle.gate.wait().await?;
    let Some(sup) = lifecycle.supervisor() else {
        return Err(IpcError::not_ready("bootSweep"));
    };
    let record = req.record_id.as_str().to_string();
    let exists = {
        let id = record.clone();
        sup.db()
            .read(move |conn| dao::quit_record::get(conn, &id))
            .await
            .map_err(|e| IpcError::from(&e))?
            .is_some()
    };
    if !exists {
        return Err(IpcError::not_found("quitRecord"));
    }
    let Some(sessions) = services.sessions() else {
        return Err(IpcError::not_built());
    };
    // A38.9: `null` continues every item; a list — also an empty one — only those.
    let uids = req.uids;
    let res = sessions.continue_all(&req.record_id, uids).await?;
    let started: Vec<String> = res.started.iter().map(|u| u.as_str().to_string()).collect();
    if !started.is_empty() {
        let id = record.clone();
        let _ = sup
            .db()
            .write(move |conn| {
                for uid in &started {
                    dao::quit_record::remove_item(conn, &id, uid)?;
                }
                Ok(())
            })
            .await;
    }
    Ok(res)
}

/// `quit_record_dismiss` (contract §4.2.1): the record is deleted.
#[tauri::command]
pub async fn quit_record_dismiss<R: tauri::Runtime>(
    req: Req<QuitRecordDismissReq>,
    app: tauri::AppHandle<R>,
) -> Result<QuitRecordDismissRes, IpcError> {
    use tauri::Manager as _;
    let lifecycle: LifecycleState = state_of(&app)?;
    let res = dismiss(&lifecycle, req.into_inner()).await?;
    // World's `RefSource::QuitRecord` follows the records still open.
    if let (Some(world), Some(sup)) = (app.try_state::<std::sync::Arc<crate::world::World>>(), lifecycle.supervisor()) {
        crate::lifecycle::wiring::refresh_quit_record_refs(sup.db(), world.inner()).await;
    }
    Ok(res)
}

pub async fn dismiss(lifecycle: &Lifecycle, req: QuitRecordDismissReq) -> Result<QuitRecordDismissRes, IpcError> {
    let Some(sup) = lifecycle.supervisor() else {
        return Err(IpcError::not_ready("bootSweep"));
    };
    let id = req.record_id.as_str().to_string();
    let deleted = sup
        .db()
        .write(move |conn| dao::quit_record::delete(conn, &id))
        .await
        .map_err(|e| IpcError::from(&e))?;
    if deleted {
        Ok(QuitRecordDismissRes {})
    } else {
        Err(IpcError::not_found("quitRecord"))
    }
}
