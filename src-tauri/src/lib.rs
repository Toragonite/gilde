//! gilde-code — application entry point (Gilde 2 boot order, contract §2.4).
//!
//! Owner: T09.
//!
//! `setup()` does only B1–B4 (`lifecycle::boot::setup_core`): paths and the
//! memory-only probe, tracing, `state.db`, `config.json`, the single-instance
//! lock — plus memory/SQLite-only preparation (planned crash records, launch
//! kind, UI state, the `app_instance` row) — and returns. The window becomes
//! visible (B5) and is recorded; only then does the background boot run:
//! B6 the boot sweep, which opens the spawn gate; the services of the other
//! tasks, wired in one place (`lifecycle::wiring`, T09b/A22); B9 PATH +
//! agent detection; B10 the static canary (T21); the shared process scan and
//! leftovers; the memory-only re-check. The sidecar starts lazily on first
//! need (B12, T08). `setup()` never spawns, never reads a vendor directory and
//! never waits on the network (T-boot-1); it only creates the macOS system
//! and visibility hubs and installs their observers (main thread).
//!
//! The way out is the quit of contract §10.2 (`lifecycle::quit`, run by
//! `app_quit` or by [`state::shutdown`] on any other exit): the record first,
//! the bounded stop sweep (never SIGKILL a vendor CLI), the sidecar, then
//! `app_instance.ended_at`.

pub mod accounts; // T08  accounts (REWORK)
pub mod agents; // T09  PATH resolution, detection (launch/signatures: T06)
pub mod commands; // T09  command modules (registry.rs: T01)
pub mod compat; // T21  ClaudeCompat, canaries (contract §3.10)
pub mod config; // T09  ConfigStore, AppConfig (cco import: T08)
pub mod contract; // T01  the interface contract in code (contract §3, §4, §7.1)
pub mod convo; // T05  Conversation mode L1 (contract §6.1)
pub mod discovery; // T03  read-only discovery (contract §5)
pub mod error;
pub mod events; // T09  store publisher (REWORK)
pub mod follow; // T04  observed follow + ContentHub (contract §6.4)
pub mod ide; // T11  IDE opener, links (contract §9.4)
pub mod ipc_rate; // T42  the central IPC rate limiter (contract §4.1 rule 6, A45.3)
pub mod lifecycle; // T09  boot, spawn gate, quit/crash records (contract §2.4, §10)
pub mod log;
pub mod macos; // T12  dock, owner focus, visibility (contract §8.3, §9.5)
pub mod model; // T09  status evidence types (T06)
pub mod notify; // T12  notifications (contract §8.4)
pub mod panehost; // T07  PaneHost + shim protocol (contract §7.4)
pub mod paths;
pub mod pty; // T06  PtyManager, session, ring, coalescer
pub mod render; // T04  normalizers, sanitizer (claude_stream: T05)
pub mod sessions; // T10  sessions manager (contract §6.0)
pub mod sidecar; // T08  protocol 2, client, SidecarManager
pub mod state; // T09  AppState, instance lock, shutdown
pub mod status; // T06  status inference engine (source S5)
pub mod store; // T09  Db, migrations, models, dao
pub mod supervise; // T09  procscan, registry, identity, kill, spawn, redact
pub mod turns; // T08  turn-based mode (contract §6.3)
pub mod world; // T02  World (contract §3.3, §3.5, §8.1)
pub mod worktree; // T11  git, naming, manager, alias

// -- module registry ---------------------------------------------------------
//
// Gilde 2 (contract §15.1, F-18): every top-level module is declared above by
// T01 in wave 0 and the `mod` lines are frozen afterwards; a new module is a
// contract change. The command surface is exactly the manifest
// (`contract/ipc_manifest.rs`) through the generated `commands::registry`.

use std::sync::{Arc, Mutex};

use tauri::Manager as _;

pub use error::{CmdResult, GildeError, GildeErrorPayload, Result};
pub use paths::AppPaths;

use crate::commands::store::LocalStore;
use crate::lifecycle::boot::{Lifecycle, SetupOutcome};
use crate::lifecycle::wiring;
use crate::sidecar::SidecarRuntime;
use crate::state::Services;
use crate::supervise::health::HealthState;
use crate::supervise::registry::Supervisor;

/// This build's version, as recorded in the `app_instance` row.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The build id the WebView, the sidecar and the shim must match (§4.7,
/// A8.1): the compile-time `GILDE_BUILD_ID` when set and non-empty, else the
/// version. A value outside [`is_valid_build_id`] fails the build.
pub const BUILD_ID: &str = build_id_from(option_env!("GILDE_BUILD_ID"), VERSION);

const _: () = assert!(
    is_valid_build_id(BUILD_ID),
    "GILDE_BUILD_ID must match ^[0-9A-Za-z][0-9A-Za-z._+-]{{0,63}}$ (it names the shim/<buildId> directory)"
);

/// A8.1: an empty value is unset; it is never trimmed or replaced.
pub const fn build_id_from<'a>(env: Option<&'a str>, version: &'a str) -> &'a str {
    match env {
        Some(id) if !id.is_empty() => id,
        _ => version,
    }
}

/// A8.1: `^[0-9A-Za-z][0-9A-Za-z._+-]{0,63}$`.
pub const fn is_valid_build_id(id: &str) -> bool {
    let b = id.as_bytes();
    if b.is_empty() || b.len() > 64 || !b[0].is_ascii_alphanumeric() {
        return false;
    }
    let mut i = 1;
    while i < b.len() {
        let c = b[i];
        if !(c.is_ascii_alphanumeric() || c == b'.' || c == b'_' || c == b'+' || c == b'-') {
            return false;
        }
        i += 1;
    }
    true
}

/// Holds the `tracing-appender` worker guard for the lifetime of the process.
struct LogGuard(#[allow(dead_code)] Mutex<Option<tracing_appender::non_blocking::WorkerGuard>>);

/// Keeps the `instance.lock` descriptor open for the life of the process.
struct InstanceLockState(#[allow(dead_code)] Option<state::InstanceLock>);

/// Background tasks whose handle is their lifetime (dropping one aborts it).
#[derive(Default)]
struct BootTasks(Mutex<Vec<tauri::async_runtime::JoinHandle<()>>>);

impl BootTasks {
    fn push(&self, task: tauri::async_runtime::JoinHandle<()>) {
        if let Ok(mut v) = self.0.lock() {
            v.push(task);
        }
    }
}

impl Drop for BootTasks {
    fn drop(&mut self) {
        if let Ok(mut v) = self.0.lock() {
            for t in v.drain(..) {
                t.abort();
            }
        }
    }
}

/// The agent-signature file watcher, kept alive by being managed.
struct SignatureWatch(#[allow(dead_code)] Option<agents::signatures::Watcher>);

/// The macOS system and visibility hubs, created on the main thread in
/// `setup()` (their observers need it) and handed to the wiring.
#[derive(Clone)]
struct MacHubs {
    system: Arc<macos::sleep::SystemEvents>,
    visibility: Arc<macos::visibility::VisibilityHub>,
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // A9.1 (feature `wakeup-count` only): the counting runtime becomes
    // Tauri's async runtime before anything uses it.
    let _wakeups = lifecycle::wakeups::install();
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        // Rust-side only (contract §9.2): the WebView is granted no plugin
        // permission; notifications are the LP-28 fallback from Rust.
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            setup(app)?;
            Ok(())
        })
        // Contract §4.1 rule 2: exactly the manifest commands.
        .invoke_handler(commands::registry::handler())
        .on_window_event(|window, event| {
            // Closing the last window keeps Gilde running (dock badge,
            // notifications) [ux-ia §2.7]: the window hides instead.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building the tauri application")
        .run(on_run_event);
}

fn on_run_event(handle: &tauri::AppHandle, event: tauri::RunEvent) {
    match event {
        tauri::RunEvent::Ready => {
            // B5: the event loop runs, the window is on screen.
            state::present_main_window(handle);
            if let Some(lifecycle) = handle.try_state::<Arc<Lifecycle>>() {
                let at = store::now_ms();
                lifecycle.window_visible(at);
                if let Some(sup) = lifecycle.supervisor() {
                    tauri::async_runtime::spawn(async move {
                        if let Err(err) = sup.mark_window_visible(at).await {
                            tracing::warn!(target: "app", error = %err, "could not record window_visible_at");
                        }
                    });
                }
            }
        }
        #[cfg(target_os = "macos")]
        tauri::RunEvent::Reopen { .. } => state::present_main_window(handle),
        tauri::RunEvent::ExitRequested { api, .. } => {
            // §10.2 step 1: with In-Gilde sessions the exit waits for
            // `app_quit` (the quit sheet); with none it proceeds at once.
            if exit_may_proceed(handle) {
                return;
            }
            api.prevent_exit();
            state::present_main_window(handle);
            events::emit(
                handle,
                events::QUIT_REQUESTED,
                contract::store::QuitRequestedEvent::default(),
            );
        }
        tauri::RunEvent::Exit => {
            lifecycle::wakeups::write_at_exit();
            if let Some(report) = state::shutdown_blocking(handle) {
                tracing::info!(
                    target: "app",
                    sessions = report.killed,
                    survivors = report.survivors,
                    sidecar_stopped = report.sidecar_stopped,
                    instance_closed = report.instance_closed,
                    "shutdown complete"
                );
            }
        }
        _ => {}
    }
}

/// Whether an exit request may go through now.
fn exit_may_proceed(handle: &tauri::AppHandle) -> bool {
    if lifecycle::quit::exit_authorized() {
        return true;
    }
    let Some(lifecycle) = handle.try_state::<Arc<Lifecycle>>() else {
        return true;
    };
    if lifecycle.quit().is_some_and(|q| q.is_done() || q.in_progress()) {
        return true;
    }
    let services = handle.try_state::<Arc<Services>>().map(|s| s.inner().clone());
    !has_in_gilde_sessions(&lifecycle, services.as_deref())
}

/// Whether any session runs in Gilde right now (World, else the registry).
fn has_in_gilde_sessions(lifecycle: &Lifecycle, services: Option<&Services>) -> bool {
    if let Some(world) = services.and_then(|s| s.world()) {
        return world
            .snapshot()
            .sessions
            .iter()
            .any(|s| matches!(s.control, contract::session::Control::InGilde { .. }));
    }
    let Some(sup) = lifecycle.supervisor() else {
        return false;
    };
    let me = sup.app_instance_id().to_string();
    sup.db()
        .blocking(move |conn| store::dao::proc::rows_of_instance(conn, &me))
        .map(|rows| rows.iter().any(|r| r.state.is_live() && r.kind.is_session_kind()))
        .unwrap_or(false)
}

/// `setup()`: B1–B4 only (see the module docs), then the background boot.
fn setup(app: &mut tauri::App) -> Result<()> {
    // B1: paths, then logging. Nothing above this line may log.
    let paths = AppPaths::resolve(app.handle())?;
    // An unwritable log folder (memory-only mode) must not stop the boot.
    let guard = log::init_tracing(&paths).ok();
    log::install_panic_hook();
    app.manage(LogGuard(Mutex::new(guard)));

    let outcome = tauri::async_runtime::block_on(lifecycle::boot::setup_core(paths, VERSION, BUILD_ID));
    let mut out = match outcome {
        Ok(SetupOutcome::Ready(out)) => out,
        Ok(SetupOutcome::SecondInstance { holder_pid, focus_requested }) => {
            tracing::info!(
                target: "app",
                holder_pid = ?holder_pid,
                focus_requested,
                "another instance already holds instance.lock; focusing it and exiting"
            );
            std::process::exit(0);
        }
        Err(err) => {
            if matches!(&err, GildeError::Db(msg) if msg.contains("newer build")) {
                use tauri_plugin_dialog::DialogExt as _;
                app.dialog()
                    .message(err.to_string())
                    .kind(tauri_plugin_dialog::MessageDialogKind::Error)
                    .blocking_show();
            }
            return Err(err);
        }
    };
    let lifecycle = out.lifecycle.clone();

    if out.db.recovered_from_corruption() {
        tracing::warn!(target: "app", "state.db was damaged and has been replaced");
        events::emit_toast(
            app.handle(),
            events::ToastEvent::new(events::ToastLevel::Warn, "toast.db.quarantined"),
        );
    }

    // The registry's helpers: `ps`/`lsof` fallbacks and the PATH probe become
    // `probe` rows (git goes through `SupervisorApi`, bound by the wiring).
    supervise::procscan::install_probe_hook(Arc::new(supervise::registry::ProbeHook::new(
        out.supervisor.clone(),
    )));

    // T12 steps 2–3: the system and visibility hubs, observers on the main
    // thread (`setup()` runs on it).
    let hubs = MacHubs {
        system: macos::sleep::SystemEvents::new(),
        visibility: macos::visibility::VisibilityHub::new(Arc::new(
            macos::visibility::TauriVisibilityEmitter::new(app.handle().clone()),
        )),
    };
    if let Some(mtm) = objc2_foundation::MainThreadMarker::new() {
        macos::sleep::install_observers(mtm, hubs.system.clone());
        macos::visibility::install_observers(mtm, hubs.visibility.clone());
    } else {
        tracing::warn!(target: "app", "setup() is not on the main thread; no sleep/visibility observers");
    }

    // Managed state the commands need from B5 on (all memory-only).
    let services = Services::new();
    let health = HealthState::new();
    let local = LocalStore::new();
    lifecycle.feed.set_local_store(local.clone());
    lifecycle.sync_memory_only();
    wiring::manage_core(
        app,
        &wiring::Core {
            paths: out.paths.clone(),
            db: out.db.clone(),
            config: out.config.clone(),
            lifecycle: lifecycle.clone(),
            supervisor: out.supervisor.clone(),
            services: services.clone(),
            health: health.clone(),
        },
        local,
    );
    app.manage(InstanceLockState(out.lock.take()));
    app.manage(BootTasks::default());

    let (total_ms, phases) = lifecycle.timer_line();
    tracing::info!(target: "app", total_ms = total_ms as u64, phases = %phases, "setup complete; the window comes next");

    // B6–B13, after the window.
    let handle = app.handle().clone();
    tauri::async_runtime::spawn(async move {
        background_boot(handle, *out, services, health, hubs).await;
    });
    Ok(())
}

/// Everything after B5. Never fails: a step that cannot run is logged and the
/// app keeps going.
async fn background_boot(
    handle: tauri::AppHandle,
    out: lifecycle::boot::SetupOutput,
    services: Arc<Services>,
    health: Arc<HealthState>,
    hubs: MacHubs,
) {
    let lifecycle::boot::SetupOutput {
        paths,
        db,
        config,
        lifecycle,
        supervisor,
        ..
    } = out;
    lifecycle.wait_window_visible().await;

    // B6: the boot sweep opens the spawn gate.
    let sweep = lifecycle::boot::run_boot_sweep(&lifecycle, &supervisor).await;
    tracing::info!(
        target: "supervise",
        crash_records = sweep.crash_records.len(),
        adopted = sweep.sweep.adopted,
        exited = sweep.sweep.exited,
        lost = sweep.sweep.lost,
        auto_terminated = sweep.sweep.auto_terminated,
        quit_survivors = sweep.quit_survivors_scheduled,
        "boot sweep done; spawn gate open"
    );
    supervise::reaper::install_signal_handlers(&handle, supervisor.clone());
    let reconcile = supervise::reaper::install_reconcile(&handle, supervisor.clone());
    handle.manage(reconcile);
    match db.read(|conn| store::dao::run_retention(conn, store::now_ms())).await {
        Ok(report) if report.total() > 0 => tracing::info!(
            target: "app",
            proc_rows = report.proc_rows_deleted,
            event_log = report.event_log_deleted,
            "retention swept old rows"
        ),
        Ok(_) => {}
        Err(err) => tracing::warn!(target: "app", error = %err, "retention pass failed"),
    }

    // The services of the other tasks, wired in one place (A22). None of them
    // spawns at construction; everything that would waits for the gate.
    let core = wiring::Core {
        paths: paths.clone(),
        db: db.clone(),
        config: config.clone(),
        lifecycle: lifecycle.clone(),
        supervisor: supervisor.clone(),
        services: services.clone(),
        health: health.clone(),
    };
    let platform = app_platform(&handle, &config, hubs);
    let wired = match wiring::wire(&core, platform, wiring::Options::app()).await {
        Ok(w) => Arc::new(w),
        Err(err) => {
            tracing::error!(target: "app", error = %err, "the services could not be wired; commands answer notBuilt");
            return;
        }
    };
    wiring::manage_all(&handle, &wired);
    handle.manage(SignatureWatch(wired.agents.watch_signatures().ok()));
    handle.manage(state::AppState::new(
        paths.clone(),
        db.clone(),
        config.clone(),
        supervisor.clone(),
        lifecycle.clone(),
        services.clone(),
        wired.clone(),
        store::now_ms(),
    ));
    let tasks = handle.state::<BootTasks>();
    tasks.push(wiring::spawn_boot_settled(&wired, &lifecycle));
    tasks.push(state::spawn_focus_listener(&handle, &paths.instance_lock()));
    tasks.push(lifecycle::boot::spawn_memory_only_recheck(
        lifecycle.clone(),
        db.clone(),
        paths.app_data_dir(),
    ));
    tasks.push(spawn_leftover_loop(lifecycle.clone(), supervisor.clone(), health.clone()));
    lifecycle.timer_mark("B6.services");

    // B9 + B10 (QoS utility, never on the main thread).
    let detected = lifecycle::boot::run_detection(&lifecycle, &wired.agents, services.compat()).await;
    tracing::info!(
        target: "agents",
        installed = detected.iter().filter(|b| b.installed).count(),
        "agent detection finished"
    );
    let (total_ms, phases) = lifecycle.timer_line();
    tracing::info!(target: "app", total_ms = total_ms as u64, phases = %phases, "boot complete");
}

/// The app's side of the wiring: the window's emitters, the native dialogs,
/// the Dock, the notification center, the sidecar's runtime.
fn app_platform(handle: &tauri::AppHandle, config: &Arc<config::ConfigStore>, hubs: MacHubs) -> wiring::Platform {
    let app = handle.clone();
    wiring::Platform {
        events: events::Events::app(handle.clone()),
        notification_sink: Box::new(move |flag| notify::build_sink(&app, flag)),
        dock: Arc::new(macos::dock::MacDock::new(handle.clone())),
        focus: Arc::new(notify::TauriFocusEmitter::new(handle.clone())),
        notify_clock: Arc::new(notify::SystemClock),
        activator: Arc::new(macos::owner::MacActivator),
        project_picker: Arc::new(commands::project::DialogFolderPicker::new(handle.clone())),
        account_picker: Arc::new(accounts::DialogFolderPicker::new(handle.clone())),
        link_confirm: Arc::new(commands::ide::DialogLinkConfirm::new(handle.clone(), config.clone())),
        sidecar_runtime: sidecar_runtime(handle, config),
        system: hubs.system,
        visibility: hubs.visibility,
    }
}

/// The shared process scan → records and leftovers (§3.4.10, §2.2.1): the
/// discovery scan when T03 publishes one, else an in-process kernel scan.
fn spawn_leftover_loop(
    lifecycle: Arc<Lifecycle>,
    sup: Arc<Supervisor>,
    health: Arc<HealthState>,
) -> tauri::async_runtime::JoinHandle<()> {
    tauri::async_runtime::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let now = store::now_ms();
            let snap = match lifecycle.scans.fresh(now, 20_000) {
                Some(s) => s,
                None => {
                    let Ok(Ok(s)) = tokio::task::spawn_blocking(supervise::procscan::kernel_proc_snapshot).await
                    else {
                        continue;
                    };
                    lifecycle.scans.publish(s);
                    match lifecycle.scans.latest() {
                        Some(s) => s,
                        None => continue,
                    }
                }
            };
            let registry = supervise::health::registry_view(&sup).await;
            let leftovers = health.observe(snap, &registry);
            lifecycle.feed.set_leftovers(leftovers);
        }
    })
}

/// The sidecar's node and script. Never fails: a runtime that cannot be
/// resolved becomes one that reports `down` with the reason when the sidecar
/// first starts (lazily, B12, T08).
fn sidecar_runtime(app: &tauri::AppHandle, config: &Arc<config::ConfigStore>) -> SidecarRuntime {
    let settings = config.get();
    let node_path = settings.sidecar.node_path.clone();
    let path = std::env::var("PATH").unwrap_or_default();
    let entries: Vec<std::path::PathBuf> = std::env::split_paths(&path).collect();
    match SidecarRuntime::resolve_for_app(app, node_path.as_deref(), &entries) {
        Ok(runtime) => runtime,
        Err(err) => {
            tracing::error!(target: "sidecar", error = %err, "no usable Node runtime for the sidecar");
            SidecarRuntime::explicit(
                node_path.unwrap_or_else(|| "node".to_string()),
                app.path()
                    .resolve("sidecar/sidecar.cjs", tauri::path::BaseDirectory::Resource)
                    .unwrap_or_else(|_| std::path::PathBuf::from("sidecar.cjs")),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T-buildId-rule (A8.1).
    #[test]
    fn t_build_id_rule() {
        assert_eq!(build_id_from(None, "0.4.0"), "0.4.0");
        assert_eq!(build_id_from(Some(""), "0.4.0"), "0.4.0", "empty = unset");
        assert_eq!(build_id_from(Some("0.4.0+abc1234.20261022T120000"), "0.4.0"), "0.4.0+abc1234.20261022T120000");
        assert!(is_valid_build_id(VERSION));
        assert!(is_valid_build_id(BUILD_ID));
        assert!(is_valid_build_id("0.4.0+abc1234.20261022T120000"));
        assert!(is_valid_build_id("2.0.0+b425b5e.20261022T073000"));
        for bad in [
            "",
            " 0.4.0",
            "0.4.0\n",
            "0.4.0 ",
            "a/b",
            ".hidden",
            "-flag",
            "a b",
            "0.4.0\t",
        ] {
            assert!(!is_valid_build_id(bad), "{bad:?} must be rejected");
        }
        assert!(!is_valid_build_id(&"a".repeat(65)), "more than 64 bytes");
        assert!(is_valid_build_id(&"a".repeat(64)));
    }
}
