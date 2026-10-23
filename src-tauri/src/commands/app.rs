//! App commands (contract §4.2.1): bootstrap, marks, agent rescan, diagnostics.
//!
//! Owner: T09 (REWORK, §2.2.1). `app_bootstrap` answers from memory (≤ 50 ms,
//! never fails after B4 except `INTERNAL`); `agents_rescan` re-runs PATH +
//! `--version` detection and the static canary off the main thread;
//! `app_diagnostics` (debug capability only) carries counts, versions, compat,
//! timings and error codes — never content, home paths only as `~/…` (§9.7).

use std::collections::BTreeMap;
use std::sync::Arc;


use crate::agents::AgentRegistry;
use crate::config::model::Locale as ConfigLocale;
use crate::config::ConfigStore;
use crate::contract::ipc::{
    AgentsRescanReq, AgentsRescanRes, AppBootstrapReq, AppBootstrapRes, AppDiagnosticsReq,
    AppDiagnosticsRes, AppFeatures, AppMarkReq, AppMarkRes, Locale, Theme,
};
use crate::contract::store::OrchestratorPresence;
use crate::contract::{IpcError, Req};
use crate::lifecycle::boot::Lifecycle;
use crate::paths::AppPaths;
use crate::state::Services;
use crate::supervise::health::HealthState;

/// Managed state alias so later tasks can ask for the same thing.
pub type PathsState = Arc<AppPaths>;
/// The lifecycle state (boot, gate, memory-only, quit coordinator).
pub type LifecycleState = Arc<Lifecycle>;
/// The late-bound service traits of the other tasks.
pub type ServicesState = Arc<Services>;
/// The agent registry (detection, signatures).
pub type AgentsState = Arc<AgentRegistry>;
/// Process records, leftovers and the Clean-up flow.
pub type HealthStateRef = Arc<HealthState>;
/// Settings.
pub type ConfigState = Arc<ConfigStore>;

/// A managed service, or `UNSUPPORTED{notBuilt}` when this app was built
/// without it (the bare contract harness). Resolved per call instead of as a
/// `State` parameter, so a missing service is an `IpcError`, never a raw
/// platform rejection (A7.3).
pub fn state_of<R: tauri::Runtime, T: Clone + Send + Sync + 'static>(
    app: &tauri::AppHandle<R>,
) -> Result<T, IpcError> {
    use tauri::Manager as _;
    app.try_state::<T>()
        .map(|s| s.inner().clone())
        .ok_or_else(IpcError::not_built)
}

/// A service managed only after the boot sweep (B6): `NOT_READY{bootSweep}`
/// until then, `UNSUPPORTED{notBuilt}` in an app without a core.
pub fn late_state_of<R: tauri::Runtime, T: Clone + Send + Sync + 'static>(
    app: &tauri::AppHandle<R>,
) -> Result<T, IpcError> {
    use tauri::Manager as _;
    if app.try_state::<LifecycleState>().is_none() {
        return Err(IpcError::not_built());
    }
    app.try_state::<T>()
        .map(|s| s.inner().clone())
        .ok_or_else(|| IpcError::not_ready("bootSweep"))
}

/// The UI locale for `system`: the first macOS preferred language, `ko*` → ko.
pub fn system_locale() -> Locale {
    let langs = objc2_foundation::NSLocale::preferredLanguages();
    let first = langs.iter().next().map(|s| s.to_string()).unwrap_or_default();
    if first.to_ascii_lowercase().starts_with("ko") {
        Locale::Ko
    } else {
        Locale::En
    }
}

/// The resolved locale and theme of the current settings.
pub fn locale_and_theme(config: &ConfigStore) -> (Locale, Theme) {
    let cfg = config.get();
    let locale = match cfg.locale {
        ConfigLocale::Ko => Locale::Ko,
        ConfigLocale::En => Locale::En,
        ConfigLocale::System => system_locale(),
    };
    (locale, cfg.to_settings().theme)
}

/// `app_bootstrap` (contract §4.2.1): memory only.
#[tauri::command]
pub async fn app_bootstrap<R: tauri::Runtime>(
    req: Req<AppBootstrapReq>,
    app: tauri::AppHandle<R>,
) -> Result<AppBootstrapRes, IpcError> {
    let _ = req;
    let lifecycle: LifecycleState = state_of(&app)?;
    let config: ConfigState = state_of(&app)?;
    let services: ServicesState = state_of(&app)?;
    Ok(bootstrap(&lifecycle, &config, &services))
}

/// The bootstrap payload (shared with tests).
pub fn bootstrap(lifecycle: &Lifecycle, config: &ConfigStore, services: &Services) -> AppBootstrapRes {
    let (locale, theme) = locale_and_theme(config);
    let orchestrator = match services.world() {
        Some(world) => world.snapshot().integrations.orchestrator != OrchestratorPresence::Absent,
        None => false,
    };
    AppBootstrapRes {
        build_id: lifecycle.build_id.clone(),
        version: lifecycle.version.clone(),
        locale,
        theme,
        launch: lifecycle.launch(),
        ui_state: lifecycle.ui_state(),
        platform: lifecycle.platform,
        features: AppFeatures { orchestrator },
        compat: services.compat().map(|c| c.list()).unwrap_or_default(),
    }
}

/// `app_mark` (contract §4.2.1): a repeated mark is ignored.
#[tauri::command]
pub async fn app_mark<R: tauri::Runtime>(
    req: Req<AppMarkReq>,
    app: tauri::AppHandle<R>,
) -> Result<AppMarkRes, IpcError> {
    let req = req.into_inner();
    let lifecycle: LifecycleState = state_of(&app)?;
    lifecycle.mark(req.mark, req.perf_ms);
    Ok(AppMarkRes {})
}

/// `agents_rescan` (contract §4.2.1): class `probe`; PATH + `--version` and
/// the static canary run in the background, never on the calling thread.
/// A29.1: the rerun replaces `BootStatus.agents` when it completes (the
/// previous rows stay until then) and moves `agentsScannedAt` forward.
#[tauri::command]
pub async fn agents_rescan<R: tauri::Runtime>(
    req: Req<AgentsRescanReq>,
    app: tauri::AppHandle<R>,
) -> Result<AgentsRescanRes, IpcError> {
    let _ = req;
    let lifecycle: LifecycleState = state_of(&app)?;
    let services: ServicesState = state_of(&app)?;
    let agents: AgentsState = late_state_of(&app)?;
    crate::lifecycle::boot::spawn_rescan(lifecycle, agents, services.compat());
    Ok(AgentsRescanRes { scheduled: true })
}

/// `app_diagnostics` (debug capability only, contract §4.2.1, §9.7).
#[tauri::command]
pub async fn app_diagnostics<R: tauri::Runtime>(
    req: Req<AppDiagnosticsReq>,
    app: tauri::AppHandle<R>,
) -> Result<AppDiagnosticsRes, IpcError> {
    let _ = req;
    let lifecycle: LifecycleState = state_of(&app)?;
    let services: ServicesState = state_of(&app)?;
    let health: HealthStateRef = state_of(&app)?;
    Ok(diagnostics(&lifecycle, &services, &health).await)
}

/// The diagnostics payload (shared with tests): counts, versions, compat,
/// timings, error codes. No paths, no argv, no content.
pub async fn diagnostics(lifecycle: &Lifecycle, services: &Services, health: &HealthState) -> AppDiagnosticsRes {
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    let records = health.records();
    counts.insert("processes".into(), records.len() as u64);
    counts.insert(
        "leftovers".into(),
        records.iter().filter(|r| r.leftover.is_some()).count() as u64,
    );
    counts.insert(
        "gildeOwnedProcesses".into(),
        records.iter().filter(|r| r.gilde_owned).count() as u64,
    );
    counts.insert("notices".into(), lifecycle.feed.notices().len() as u64);
    counts.insert("memoryOnly".into(), u64::from(lifecycle.memory_only.is_active()));
    if let Some(sup) = lifecycle.supervisor() {
        let me = sup.app_instance_id().to_string();
        if let Ok(rows) = sup
            .db()
            .read(move |conn| crate::store::dao::proc::rows_of_instance(conn, &me))
            .await
        {
            counts.insert("registryRows".into(), rows.len() as u64);
            counts.insert(
                "registryLive".into(),
                rows.iter().filter(|r| r.state.is_live()).count() as u64,
            );
        }
    }
    let mut versions: BTreeMap<String, String> = BTreeMap::new();
    versions.insert("app".into(), lifecycle.version.clone());
    versions.insert("build".into(), lifecycle.build_id.clone());
    versions.insert("macos".into(), lifecycle.platform.macos_major.to_string());
    versions.insert("schema".into(), crate::store::migrations::latest_version().to_string());
    AppDiagnosticsRes {
        counts,
        versions,
        compat: services.compat().map(|c| c.list()).unwrap_or_default(),
        timings: lifecycle.timings(),
        error_codes: lifecycle.error_codes(),
    }
}
