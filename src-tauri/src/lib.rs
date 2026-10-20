//! gilde-code — application entry point.
//!
//! `setup()` runs the boot order DESIGN mandates, and T29 closed it. In order
//! (M1-PLAN T29 acceptance 1):
//!
//!   1. `AppPaths::resolve` + `ensure_all`
//!   2. tracing + the panic hook — nothing above this line may log
//!   3. `Db::open` (migrate, integrity check, quarantine-and-continue)
//!   4. `ConfigStore::load`
//!   5. the single-instance `flock` on `instance.lock` — a second launch
//!      focuses the window that is already there and exits
//!   6. `boot_id` + the `app_instance` row (and, before the first `git`, the
//!      spawn hook, the alias layer, the worktree manager, the agent registry
//!      and the account slots)
//!   7. `reaper::sweep_startup`, then signal handlers, the 30 s reconcile and
//!      retention
//!   8. `agents::path` resolution + the sidecar manager; detection and
//!      `SidecarManager::start` are pushed off the boot path
//!   9. the status engine and the PTY lane
//!  10. the memory watchdog — which is also the process scanner loop
//!  11. the dispatch engine, and the runs a previous instance left in flight
//!  12. the first graph projection, and only then the window
//!
//! Nothing between 1 and 12 may take two seconds: the boot timer below measures
//! every phase and logs a warning naming any that does.
//!
//! The way out is [`state::shutdown`]: every child of this instance through
//! `kill_tree`, then the sidecar, then `app_instance.ended_at`.

pub mod accounts; // T03  account slots
pub mod agents; // T09  PATH resolution, detection, launch templates, signatures
pub mod commands;
pub mod config; // T03  ConfigStore, AppConfig, cco import
pub mod dispatch; // T22  dispatch model + engine
pub mod error;
pub mod events; // T04  event names + payloads + bus + coalescer
pub mod log;
pub mod model; // T04  ids / graph / status
pub mod paths;
pub mod pty; // T15  PtyManager, session, ring, coalescer
pub mod sidecar; // T19  protocol, types, client, SidecarManager
pub mod state; // T25  graph projection · T29  AppState, instance lock, shutdown
pub mod status; // T16  status inference engine (ansi tail + state machine)
pub mod store; // T02  Db, migrations, models, dao
pub mod supervise; // T07/T08/T10  procscan, registry, identity, kill, spawn
pub mod worktree; // T13/T14  git, naming, manager, alias

// -- module registry ---------------------------------------------------------
//
// Every module of DESIGN §2 is declared above; the last two (`dispatch`, and
// `state`'s `AppState` half) landed with T22 and T29. `supervise` grows two
// files of its own (`reaper.rs` T11, `watchdog.rs` T12); they are declared in
// `supervise/mod.rs`, not here.
//
// (`src-tauri/src/lib.rs` is owned by T01 and completed by T29 — see
// docs/M1-PLAN.md "절대 규칙" §1.)

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tauri::Manager as _;

pub use error::{GildeError, GildeErrorPayload, CmdResult, Result};
pub use paths::AppPaths;

use crate::agents::{AgentBinary, AgentKind, AgentRegistry};
use crate::config::ConfigStore;
use crate::events::bus::EventBus;
use crate::events::{ToastEvent, ToastLevel};
use crate::model::graph::GraphHandle;
use crate::sidecar::{BinaryPaths, MeshConfig, SidecarManager, SidecarRuntime};
use crate::store::{dao, Db};
use crate::supervise::registry::{EnvBase, Supervisor};

/// This build's version, as recorded in the `app_instance` row and sent to the
/// sidecar in `mesh.hello`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Holds the `tracing-appender` worker guard for the lifetime of the process.
/// Dropping it flushes the log and stops the writer thread, so it lives in
/// managed state rather than on `setup`'s stack.
struct LogGuard(#[allow(dead_code)] Mutex<Option<tracing_appender::non_blocking::WorkerGuard>>);

/// T16: what keeps the status engine's two background pieces running. Managed
/// state, because a `Watcher` that is dropped stops watching and a `JoinHandle`
/// that is dropped aborts its task.
struct StatusHandles {
    #[allow(dead_code)]
    _signatures: Option<agents::signatures::Watcher>,
    #[allow(dead_code)]
    _ticker: tauri::async_runtime::JoinHandle<()>,
}

/// T12: holds the memory watchdog's task for the life of the process. Tauri
/// drops managed state on exit, which aborts the loop; without the handle the
/// task would outlive every service it reads.
struct WatchdogTask(#[allow(dead_code)] Mutex<Option<tauri::async_runtime::JoinHandle<()>>>);

impl Drop for WatchdogTask {
    fn drop(&mut self) {
        if let Some(task) = self.0.lock().ok().and_then(|mut t| t.take()) {
            task.abort();
        }
    }
}

/// T29: the background tasks that have no other owner — the 1 s graph
/// reprojection (T25) and the single-instance focus listener. Same contract as
/// [`WatchdogTask`]: the handle *is* the lifetime.
struct BootTasks {
    projector: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    focus: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
}

impl Drop for BootTasks {
    fn drop(&mut self) {
        for slot in [&self.projector, &self.focus] {
            if let Some(task) = slot.lock().ok().and_then(|mut t| t.take()) {
                task.abort();
            }
        }
    }
}

/// T29: keeps the `instance.lock` descriptor open for the life of the process.
/// Dropping it releases the lock, which is what lets the next launch start.
struct InstanceLockState(#[allow(dead_code)] Option<state::InstanceLock>);

/// Phase stopwatch for the boot sequence.
///
/// M1-PLAN T29 acceptance 1 puts a hard bound on this — "no step blocks the
/// window for more than two seconds" — and a bound nobody measures is a wish.
/// Every phase is timed, the total is logged, and anything over the budget is
/// logged at `warn` with its own name so the next person reading `gilde.log` knows
/// which one it was.
struct BootTimer {
    started: std::time::Instant,
    last: std::time::Instant,
    phases: Vec<(&'static str, u128)>,
}

impl BootTimer {
    /// The per-phase budget of M1-PLAN T29 acceptance 1.
    const BUDGET_MS: u128 = 2_000;

    fn start() -> BootTimer {
        let now = std::time::Instant::now();
        BootTimer {
            started: now,
            last: now,
            phases: Vec::new(),
        }
    }

    fn mark(&mut self, phase: &'static str) {
        let elapsed = self.last.elapsed().as_millis();
        self.last = std::time::Instant::now();
        if elapsed > Self::BUDGET_MS {
            tracing::warn!(
                target: "app",
                phase,
                ms = elapsed,
                budget_ms = Self::BUDGET_MS,
                "a boot phase blocked the window for longer than its budget"
            );
        }
        self.phases.push((phase, elapsed));
    }

    fn total_ms(&self) -> u128 {
        self.started.elapsed().as_millis()
    }

    /// `paths=3 db=11 config=1 …` — one line, greppable, no allocation per read.
    fn breakdown(&self) -> String {
        self.phases
            .iter()
            .map(|(name, ms)| format!("{name}={ms}"))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            boot(app)?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            // -- app ---------------------------------------------------------
            commands::app::app_bootstrap,
            commands::app::app_reveal,
            commands::app::app_diagnostics, // T28
            // -- settings / accounts (T03) -----------------------------------
            commands::accounts::app_settings_get,
            commands::accounts::app_settings_set,
            commands::accounts::accounts_list,
            commands::accounts::accounts_import_cco,
            // -- graph (T04) -------------------------------------------------
            commands::graph::graph_snapshot,
            commands::graph::graph_focus,
            // -- graph layout (T25) ------------------------------------------
            state::graph::graph_layout_get,
            state::graph::graph_layout_save,
            // -- agents (T09) ------------------------------------------------
            commands::agents::agents_detect,
            commands::agents::agents_path_report,
            commands::agents::agents_launch_templates,
            commands::agents::agents_reload_signatures,
            // -- projects / worktrees (T13) ----------------------------------
            commands::project::project_add,
            commands::project::project_list,
            commands::project::project_remove,
            commands::worktree::worktree_list,
            commands::worktree::worktree_create,
            commands::worktree::worktree_stats,
            commands::worktree::worktree_remove_preflight,
            commands::worktree::worktree_remove,
            commands::worktree::worktree_prune,
            commands::worktree::worktree_lock,
            // The alias command lives beside the layer it drives (T14), not in
            // `commands::`, because it is the only caller of `AliasLayer`.
            worktree::alias::worktree_alias_ensure,
            // -- mesh / sidecar (T19) ----------------------------------------
            commands::mesh::mesh_adapters,
            commands::mesh::mesh_models,
            commands::mesh::mesh_quota,
            commands::mesh::mesh_auth,
            commands::mesh::sidecar_status,
            commands::mesh::sidecar_restart,
            // -- process monitor / budget (T12) ------------------------------
            commands::monitor::procmon_scan,
            commands::monitor::procmon_cwd,
            commands::monitor::procmon_registry,
            commands::monitor::procmon_kill,
            commands::monitor::procmon_keepalive,
            commands::monitor::budget_get,
            commands::monitor::health_get,
            commands::monitor::health_dismiss,
            // -- recovery (T11) ----------------------------------------------
            commands::recovery::recovery_pending,
            commands::recovery::recovery_resolve,
            commands::recovery::undo_cancel,
            // -- session / pty (T15) -----------------------------------------
            commands::pty::session_open,
            commands::pty::session_attach,
            commands::pty::session_snapshot,
            commands::pty::session_write,
            commands::pty::session_resize,
            commands::pty::session_signal,
            commands::pty::session_send_prompt,
            commands::pty::session_close,
            commands::pty::session_list,
            commands::pty::session_status,
            // -- dispatch (T22) ----------------------------------------------
            commands::dispatch::dispatch_create,
            commands::dispatch::dispatch_list,
            commands::dispatch::dispatch_get,
            commands::dispatch::dispatch_cancel,
            commands::dispatch::dispatch_cancel_run,
            commands::dispatch::dispatch_resume,
            commands::dispatch::dispatch_route,
            commands::dispatch::dispatch_open_terminal,
            commands::dispatch::dispatch_artifact_dir,
            // -- team mode, read-only preview (team contract v2 §8.2) ----------
            commands::cco_team::cco_team_list,
            commands::cco_team::cco_team_get,
            commands::repo_identity::repo_identity,
            commands::cco_workers::cco_workers,
            // -- status (T16) ------------------------------------------------
            status::session_attention_clear,
        ])
        .build(tauri::generate_context!())
        .expect("error while building the tauri application")
        .run(|handle, event| {
            // T29: the whole teardown, not just the sidecar. Every PTY session
            // and every headless child of this instance is a process in its own
            // process group that nothing in Tauri or in Rust's destructors ends
            // (`kill_on_drop` is banned — M1-PLAN T19.4), and the
            // `app_instance` row must be closed or the next boot reads this
            // quit as a crash. `RunEvent::Exit` is the one place every exit
            // path passes through: ⌘Q, the last window closing, and the signal
            // handler's `app.exit(0)`.
            if matches!(event, tauri::RunEvent::Exit) {
                if let Some(report) = state::shutdown_blocking(handle) {
                    tracing::info!(
                        target: "app",
                        killed = report.killed,
                        survivors = report.survivors,
                        sidecar_stopped = report.sidecar_stopped,
                        instance_closed = report.instance_closed,
                        "shutdown complete"
                    );
                }
            }
        });
}

/// The boot sequence. Ordering is not cosmetic — several of these steps must
/// happen before something else can run at all, and each says so where it sits.
fn boot(app: &mut tauri::App) -> Result<()> {
    let mut timer = BootTimer::start();

    // 1. Paths, then logging. Nothing above this line may log: the subscriber
    //    does not exist yet and the message would be dropped.
    let paths = Arc::new(AppPaths::resolve(app.handle())?);
    paths.ensure_all()?;

    let guard = log::init_tracing(&paths)?;
    log::install_panic_hook();

    // The window stays hidden until step 12. It is created by `Builder::build`
    // with `visible: true` (`tauri.conf.json` is T00's file), but nothing is
    // painted until the event loop runs, which is after `setup()` returns — so
    // this costs no flash and the user never watches an empty canvas fill in.
    state::hide_main_window(app.handle());

    tracing::info!(
        version = VERSION,
        app_data_dir = %paths.app_data_dir().display(),
        worktrees_root = %paths.worktrees_root().display(),
        dispatch_root = %paths.dispatch_root().display(),
        logs_dir = %paths.logs_dir().display(),
        "gilde-code starting"
    );
    timer.mark("paths");

    // 2. `state.db`. `Db::open` never fails on a damaged file — it quarantines
    //    it and starts fresh, because refusing to boot is the one outcome the
    //    user cannot recover from (DESIGN §10-4). It *does* fail on a schema
    //    from a newer build, which is a real "do not touch this" condition.
    let db = Db::open(&paths.state_db())?;
    let schema_version = db.schema_version().unwrap_or(0);
    if db.recovered_from_corruption() {
        let quarantined = db
            .quarantined_to()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        tracing::warn!(
            quarantined_to = %quarantined,
            "state.db was damaged and has been replaced; history was lost, the app is usable"
        );
        events::emit_toast(
            app.handle(),
            ToastEvent::new(ToastLevel::Warn, "toast.db.quarantined").param("backup", quarantined),
        );
    }
    tracing::info!(
        schema_version,
        path = %paths.state_db().display(),
        "state.db open"
    );
    timer.mark("db");

    // 3. Settings. An unparseable `config.json` is quarantined the same way and
    //    reported as a toast rather than a failed boot.
    let config = Arc::new(ConfigStore::load(&paths)?);
    if let Some(warning) = config.warning() {
        tracing::warn!(
            backup = %warning.backup.display(),
            detail = %warning.detail,
            "config.json did not parse; defaults are in force"
        );
        events::emit_toast(
            app.handle(),
            ToastEvent::new(ToastLevel::Warn, warning.key)
                .param("backup", warning.backup.display().to_string()),
        );
    }
    timer.mark("config");

    // 4. The single-instance guard (DESIGN §10-1). Exactly here: after the
    //    settings are readable (a second launch still wants its log line and
    //    its paths) and before the `app_instance` INSERT, because a second row
    //    from a copy that is about to quit would look like a crashed instance
    //    to the *next* boot's sweep.
    //
    //    The reaper deliberately stops short of this: it can see that a live
    //    sibling owns those rows and leaves them alone, but focusing somebody
    //    else's window is not something a sweep can do (PINNED-VERSIONS §10-1).
    let lock_path = paths.instance_lock();
    let instance_lock = match state::acquire(&lock_path) {
        Ok(state::LockOutcome::Acquired(lock)) => Some(lock),
        Ok(state::LockOutcome::Held { pid }) => {
            // Hand the running copy the window and go. Nothing has been started
            // yet — no `app_instance` row, no spawn, no watcher.
            let focused = state::request_focus(&lock_path).is_ok();
            tracing::info!(
                target: "app",
                holder_pid = pid,
                focus_requested = focused,
                "another instance already holds instance.lock; focusing it and exiting"
            );
            // The appender is non-blocking; dropping the guard flushes it, and
            // `exit` runs no destructors.
            drop(guard);
            std::process::exit(0);
        }
        // A lock we cannot take at all (a read-only app data directory, say) is
        // not a reason to refuse to start — that would make the app
        // unlaunchable in exactly the situation where the user needs it to kill
        // orphans. Boot without the guarantee, and say so.
        Err(err) => {
            tracing::warn!(
                target: "app",
                error = %err,
                path = %lock_path.display(),
                "could not take the single-instance lock; a second copy will not be detected"
            );
            None
        }
    };
    app.manage(LogGuard(Mutex::new(Some(guard))));
    let focus_task = state::spawn_focus_listener(app.handle(), &lock_path);
    app.manage(InstanceLockState(instance_lock));
    timer.mark("instance-lock");

    // 5. The single normalised state, and the bus that fans it out. Built before
    //    any service, because several of them take the bus at construction.
    let graph = GraphHandle::new();
    let bus = EventBus::new(app.handle().clone(), graph.state().clone());

    // 6. Everything that needs the runtime. One `block_on` so `Handle::current()`
    //    is valid throughout (the agent recorder captures it).
    let Boot {
        supervisor,
        worktrees,
        agents,
        accounts,
    } = tauri::async_runtime::block_on(boot_services(
        db.clone(),
        config.clone(),
        paths.clone(),
        bus.clone(),
    ))?;

    timer.mark("services");

    // 7. T11: the boot sweep, before anything reads — or writes — a
    //    `proc_registry` row, and therefore before the sidecar can start and
    //    add one. It never fails: a sweep that cannot read the database reports
    //    zeros and the app still boots.
    let sweep = tauri::async_runtime::block_on(supervise::reaper::sweep_startup(&supervisor));
    tracing::info!(
        reboot_lost = sweep.reboot_lost,
        adopted = sweep.adopted,
        exited = sweep.exited,
        lost = sweep.lost,
        auto_terminated = sweep.auto_terminated.len(),
        pending = sweep.pending.len(),
        "startup sweep"
    );
    // T11: signal handling. macOS has no `PR_SET_PDEATHSIG`, so without this a
    // `SIGTERM` to the host leaves `gilde-node` and every PTY child alive.
    supervise::reaper::install_signal_handlers(app.handle(), supervisor.clone());
    // T11: the 30 s reconcile + the window-focus trigger (DESIGN §8-5).
    let reconcile = supervise::reaper::install_reconcile(app.handle(), supervisor.clone());

    //    Retention runs immediately after the sweep, as DESIGN §10-4 specifies.
    match db.blocking(|conn| dao::run_retention(conn, store::now_ms())) {
        Ok(report) if report.total() > 0 => tracing::info!(
            run_events = report.run_events_deleted,
            proc_samples = report.proc_samples_deleted,
            proc_rows = report.proc_rows_deleted,
            event_log = report.event_log_deleted,
            "retention swept old rows"
        ),
        Ok(_) => {}
        // Retention is housekeeping; failing it must not fail a boot.
        Err(err) => tracing::warn!(error = %err, "retention pass failed"),
    }
    timer.mark("sweep");

    // 8. `agents::path` resolution and the sidecar. The manager is constructed
    //    unconditionally so `sidecar_status` always has something to answer
    //    with; detection (five `--version` probes) and `start` (a 30 s
    //    handshake deadline) are pushed off the boot path, because DESIGN §5-3
    //    makes the PTY lane independent of it — a sidecar that cannot even
    //    resolve a Node runtime must not cost the user their interactive lane.
    let sidecar = build_sidecar(app.handle(), &config, &supervisor, &agents, &bus);
    // T11: the generation sweep, installed before the sidecar can have a
    // generation to lose. `SidecarManager` awaits it before a replacement takes
    // work, which is what keeps a crash loop from stacking orphans (DESIGN §8-5).
    sidecar.set_sweeper(supervise::reaper::ReaperSweeper::new(supervisor.clone()));
    if config.get().sidecar.enabled {
        // T22: the slots go in with the binaries — an adapter that never learns
        // about `CLAUDE_CONFIG_DIR` cannot honour a `DispatchTarget.accountId`.
        spawn_sidecar_start(
            sidecar.clone(),
            agents.clone(),
            accounts.clone(),
            bus.clone(),
        );
    } else {
        // Detection still has to run: the launch templates, the Process
        // Monitor's classification and the sidebar's vendor list all read it.
        spawn_agent_detection(agents.clone(), bus.clone());
        tracing::info!(target: "sidecar", "sidecar is disabled in settings; not starting one");
    }
    timer.mark("sidecar");

    // 9. T16: the status engine, before the pty manager that hands sessions to it.
    // It reads the *hot-reloaded* signature set through the agent registry, so
    // an edit to `agent-signatures.json` changes classification without a
    // restart; the watcher below is what makes that edit arrive.
    let status = Arc::new(status::StatusEngine::new());
    status.set_signatures(agents.clone());
    status.set_stall_after_ms(config.get().status.stall_after_ms);
    let signature_watch = match agents.watch_signatures() {
        Ok(watcher) => {
            tracing::info!(
                target: "status",
                dir = %watcher.watched_dir().display(),
                "watching agent-signatures.json"
            );
            Some(watcher)
        }
        // Losing the watch costs hot reload, not inference: the file is still
        // read at boot and `agents_reload_signatures` still works.
        Err(err) => {
            tracing::warn!(target: "status", error = %err, "cannot watch agent-signatures.json");
            None
        }
    };
    let status_ticker = status::StatusEngine::spawn_ticker(status.clone());
    timer.mark("status");

    // T15: the pty lane. Built here because it needs four of the services above
    // and nothing of its own; it starts no thread until the first
    // `session_open`, so it costs a boot nothing.
    let pty = Arc::new(
        pty::PtyManager::new(supervisor.clone(), config.clone())
            .with_status(status.clone()) // T16
            .with_events(bus.clone())
            .with_agents(agents.clone())
            .with_accounts(accounts.clone()),
    );

    timer.mark("pty");

    // 10. T12: the memory watchdog — and the process scanner loop, which is the
    // same task. Five seconds, independent of the panel's own polling: the
    // budget must be watched while nobody is looking, which is exactly when
    // four unattended agents page the machine to its knees. Every tick is one
    // `ps` snapshot that lands in the cache `procmon_scan` answers from and
    // goes out on `gilde://proc/scan`, so a visible panel and a running watchdog
    // cost one `ps` between them rather than two — which is why there is no
    // second scanner task here.
    // The health surface (`supervise::health`) reads the Claude Code
    // Orchestrator teams through the sidecar's team cache; the feed keeps that
    // cache fresh while no Team view is polling.
    let team_feed = Arc::new(commands::cco_team::TeamFeed::new(
        sidecar.clone(),
        db.clone(),
        pty.clone(),
    ));
    let watchdog = Arc::new(
        supervise::watchdog::Watchdog::new(supervisor.clone(), bus.clone())
            .with_agents(agents.clone())
            .with_teams(team_feed),
    );
    let watchdog_task = watchdog.start();
    timer.mark("watchdog");

    // 11. T22: the headless lane. It owns no thread of its own — each run is a task
    // spawned by `dispatch_create` — so, like the pty manager, it costs a boot
    // nothing. `recover_interrupted_runs` is what stops a restored board from
    // showing a spinner for a run that died with the last instance.
    let dispatch = dispatch::DispatchEngine::new(
        db.clone(),
        paths.clone(),
        config.clone(),
        sidecar.clone(),
        supervisor.clone(),
        worktrees.clone(),
        accounts.clone(),
    );
    dispatch.set_events(bus.clone());
    dispatch.set_pty(pty.clone());
    match tauri::async_runtime::block_on(dispatch.recover_interrupted_runs()) {
        Ok(settled) if settled > 0 => {
            tracing::info!(settled, "dispatch runs from a previous instance settled")
        }
        Ok(_) => {}
        Err(err) => tracing::warn!(error = %err, "could not settle previous dispatch runs"),
    }
    timer.mark("dispatch");

    // 12. The first projection (T25). Real rows from `state.db` folded together
    //     with the live PTY lane, the headless lane, the watchdog's last sample
    //     and the account slots — never a demo node, because a fabricated node
    //     would be indistinguishable from a real one in every surface that
    //     renders the graph (DESIGN §0 rule 6).
    //
    //     `Direct` writes straight into `GraphState`: there is no window yet to
    //     receive a patch, and the first `graph_snapshot` the frontend asks for
    //     must already be the whole tree. From here on the 1 s tick publishes
    //     differences on the bus.
    //     M1.5 BACKLOG 6: adopt the `pty_session` rows a crashed host left open
    //     *before* that first projection, so a restored tab finds its row in the
    //     very first `graph_snapshot` rather than only after the frontend's own
    //     `session_list`. The call is idempotent (a `OnceCell` guards it), so
    //     `session_list` still works on a boot where this was skipped, and a row
    //     is adopted only when nothing answers `kill(0)` behind it — a genuinely
    //     detached process is left alone.
    let adopted = tauri::async_runtime::block_on(pty.adopt_crashed_sessions());
    if adopted > 0 {
        tracing::info!(target: "pty", adopted, "sessions left open by a crashed host");
    }

    let projector = Arc::new(
        state::graph::GraphProjector::new(db.clone(), graph.clone())
            .with_events(bus.clone())
            .with_pty(pty.clone())
            .with_watchdog(watchdog.clone())
            .with_sidecar(sidecar.clone())
            .with_accounts(accounts.clone()),
    );
    match tauri::async_runtime::block_on(projector.reproject(state::graph::Publish::Direct)) {
        Ok(report) => tracing::info!(
            target: "graph",
            nodes = report.nodes,
            edges = report.edges,
            rev = graph.state().read().rev(),
            "graph seeded"
        ),
        // An empty canvas that fills in on the next tick beats a boot that
        // fails because one table could not be read.
        Err(err) => tracing::warn!(target: "graph", error = %err, "the first projection failed"),
    }
    let projector_task = projector.spawn(std::time::Duration::from_secs(1));
    timer.mark("graph");

    // 13. Managed state. The aliases are the ones each command module declares;
    //     `AppState` is the same services in one place, for the shutdown path.
    app.manage(state::AppState::new(
        paths.clone(),
        db.clone(),
        config.clone(),
        graph.clone(),
        bus.clone(),
        supervisor.clone(),
        worktrees.clone(),
        agents.clone(),
        accounts.clone(),
        sidecar.clone(),
        pty.clone(),
        dispatch.clone(),
        status.clone(),
        watchdog.clone(),
        projector.clone(),
        store::now_ms(),
    ));
    app.manage(paths as commands::app::PathsState);
    app.manage(db as commands::accounts::DbState);
    app.manage(config as commands::accounts::ConfigState);
    app.manage(graph);
    app.manage(bus);
    app.manage(supervisor);
    app.manage(worktrees as commands::worktree::WorktreeState);
    app.manage(agents as commands::agents::AgentsState);
    app.manage(accounts as commands::mesh::AccountsState);
    app.manage(sidecar as commands::mesh::SidecarState);
    app.manage(pty as commands::pty::PtyState); // T15
    app.manage(dispatch as commands::dispatch::DispatchState); // T22
    app.manage(status as status::StatusState); // T16
    // T16: both handles are kept alive by being managed — dropping the watcher
    // stops the hot reload, dropping the ticker aborts the 500 ms evaluation.
    app.manage(StatusHandles {
        _signatures: signature_watch,
        _ticker: status_ticker,
    });
    // T11: keeping the handle alive is what keeps the ticker running — dropping
    // it aborts the task.
    app.manage(reconcile as commands::recovery::ReconcileState);
    // T12: the watchdog itself, and the handle that keeps its task alive.
    app.manage(watchdog as commands::monitor::WatchdogState);
    app.manage(WatchdogTask(Mutex::new(Some(watchdog_task))));
    // T25/T29: the reprojection tick and the focus listener.
    app.manage(BootTasks {
        projector: Mutex::new(Some(projector_task)),
        focus: Mutex::new(Some(focus_task)),
    });
    app.manage(projector);

    // 14. The window, and not one step earlier.
    state::present_main_window(app.handle());
    timer.mark("window");

    tracing::info!(
        target: "app",
        total_ms = timer.total_ms() as u64,
        phases = %timer.breakdown(),
        "boot complete; command surface registered"
    );
    Ok(())
}

/// What [`boot_services`] hands back to [`boot`].
struct Boot {
    supervisor: Arc<Supervisor>,
    worktrees: Arc<worktree::WorktreeManager>,
    agents: Arc<AgentRegistry>,
    accounts: Arc<Vec<accounts::AccountSlot>>,
}

/// The half of boot that needs `await`. Kept in one future so the tokio handle
/// the agent recorder captures is unambiguously the app's runtime.
async fn boot_services(
    db: Db,
    config: Arc<ConfigStore>,
    paths: Arc<AppPaths>,
    bus: EventBus,
) -> Result<Boot> {
    // The `app_instance` row and this machine's `boot_id`. Every registry row
    // written from here on is attributable to this run, which is what lets the
    // reaper tell "ours, from this boot" from "a pid number from last Tuesday".
    // T11: the bus is attached here rather than later because the boot sweep
    // publishes `gilde://recovery/pending`, and a supervisor without a bus would
    // reap correctly and tell nobody.
    let supervisor = Arc::new(
        Supervisor::open_instance(&db, config.clone(), VERSION)
            .await?
            .with_events(bus.clone()),
    );
    tracing::info!(
        app_instance = %supervisor.app_instance_id(),
        boot_id = %supervisor.current_boot_id(),
        "app instance opened"
    );

    // **Before the first `git`.** `worktree::git` consults a global spawn hook;
    // installing it after a call would leave that process unregistered, and an
    // unregistered process is one the Process Monitor cannot show the user.
    if !supervise::registry::install_git_spawn_hook(supervisor.clone()) {
        tracing::warn!(target: "supervise", "a git spawn hook was already installed");
    }

    // The provider alias layer (DESIGN §7-6). Also a global hook, and also
    // installed before any worktree is created.
    worktree::alias::install(db.clone(), Some(paths.clone()));

    let worktrees = worktree::WorktreeManager::new(db.clone(), paths.clone(), Some(bus));
    match worktrees.resume_watches().await {
        Ok(resumed) => tracing::info!(worktrees = resumed, "worktree watches resumed"),
        // A watch we could not re-arm costs freshness, not correctness: stats
        // are recomputed on demand anyway.
        Err(err) => tracing::warn!(error = %err, "could not resume worktree watches"),
    }

    // `SupervisorRecorder::new` captures `Handle::current()`, which is why this
    // whole function is one future rather than several `block_on`s.
    let recorder: agents::Recorder = Arc::new(agents::SupervisorRecorder::new(supervisor.clone()));
    let agents = Arc::new(AgentRegistry::new(
        (*paths).clone(),
        config.clone(),
        Some(db.clone()),
        recorder,
    ));

    // Account slots. A machine with no `~/.claude-code-orchestrator` still gets
    // the ambient slot, so an empty list here means a real failure — logged, and
    // still not a reason to refuse to start.
    let accounts = match accounts::list(&db, &paths).await {
        Ok(slots) => slots,
        Err(err) => {
            tracing::warn!(error = %err, "could not load account slots");
            Vec::new()
        }
    };

    Ok(Boot {
        supervisor,
        worktrees,
        agents,
        accounts: Arc::new(accounts),
    })
}

/// Build the sidecar manager. Never fails: a runtime we cannot resolve becomes a
/// manager that reports `down` with the reason, which is a far more useful thing
/// for the user to see than a missing command.
fn build_sidecar(
    app: &tauri::AppHandle,
    config: &Arc<ConfigStore>,
    supervisor: &Arc<Supervisor>,
    agents: &Arc<AgentRegistry>,
    bus: &EventBus,
) -> Arc<SidecarManager> {
    let settings = config.get();
    let node_path = settings.sidecar.node_path.clone();

    // The login-shell `PATH`, not ours: a bundle launched from Finder sees
    // `/usr/bin:/bin:/usr/sbin:/sbin` and would never find an nvm node.
    let path_report = agents.path_report(false);
    for warning in &path_report.warnings {
        tracing::warn!(target: "agents", warning, "PATH resolution warning");
    }

    let runtime = match SidecarRuntime::resolve_for_app(app, node_path.as_deref(), &path_report.entries)
    {
        Ok(runtime) => {
            tracing::info!(
                target: "sidecar",
                node = %runtime.node.display(),
                script = %runtime.script.display(),
                source = runtime.node_source.as_str(),
                "sidecar runtime resolved"
            );
            runtime
        }
        Err(err) => {
            tracing::error!(target: "sidecar", error = %err, "no usable Node runtime for the sidecar");
            // A placeholder that fails its own `--version` self-test with the
            // real path in the error, rather than an unmanaged state that would
            // make `sidecar_status` itself unavailable.
            SidecarRuntime::explicit(
                node_path.unwrap_or_else(|| "node".to_string()),
                app.path()
                    .resolve("sidecar/sidecar.cjs", tauri::path::BaseDirectory::Resource)
                    .unwrap_or_else(|_| PathBuf::from("sidecar.cjs")),
            )
        }
    };

    let manager = SidecarManager::new(
        supervisor.clone(),
        runtime,
        EnvBase::from_process_env(path_report.joined()),
        MeshConfig::default(),
        std::time::Duration::from_secs(settings.sidecar.ping_interval_seconds.max(1) as u64),
    );
    manager.set_events(bus.clone());
    manager
}

/// Detect the agent CLIs, hand the sidecar the binaries it found, and start it.
///
/// All of it off the boot path. Detection shells out to up to five `--version`
/// probes and the sidecar handshake has a 30 s deadline; neither is allowed to
/// stand between the user and a window.
fn spawn_sidecar_start(
    sidecar: Arc<SidecarManager>,
    agents: Arc<AgentRegistry>,
    accounts: Arc<Vec<accounts::AccountSlot>>,
    bus: EventBus,
) {
    tauri::async_runtime::spawn(async move {
        let detected = detect_and_publish(&agents, &bus).await;

        let mesh = MeshConfig {
            binaries: binary_paths(&detected),
            // T22: every slot the app knows about, so an adapter can be handed
            // one by `DispatchTarget.accountId`. `None` config dir (the ambient
            // slot) is still a legitimate account: agent-mesh reads it as "use
            // the CLI's own login".
            accounts: mesh_accounts(&accounts),
            // T22: `defaults.permissionMode` STAYS UNSET. The sidecar applies
            // it to any task that names none (`methods/mesh.ts`) without
            // looking at the vendor, so putting `config.dispatch.permissionMode`
            // here would push claude's `acceptEdits` at codex's `--sandbox`
            // again — the exit-2 bug `dispatch::engine::permission_mode_for`
            // exists to prevent. Per-run modes are translated there instead.
            defaults: sidecar::MeshDefaults::default(),
        };
        if let Err(err) = sidecar.configure(mesh).await {
            tracing::warn!(target: "sidecar", error = %err, "could not configure the fleet");
        }
        if let Err(err) = sidecar.start().await {
            // Already reported as `down` with this reason on the status stream;
            // the log line is for the person reading `gilde.log` afterwards.
            tracing::warn!(target: "sidecar", error = %err, "the sidecar did not start");
        }
    });
}

/// Detection on its own, for the boot where the sidecar is switched off.
///
/// The roster is not a sidecar concern: the launch templates, the Process
/// Monitor's classification and the sidebar all read it, so a user who turned
/// the headless lane off must still get one.
fn spawn_agent_detection(agents: Arc<AgentRegistry>, bus: EventBus) {
    tauri::async_runtime::spawn(async move {
        detect_and_publish(&agents, &bus).await;
    });
}

/// Probe the vendor CLIs and put the roster on the wire. Never fails: a probe
/// that cannot run is an empty roster plus a log line, which the UI renders as
/// "not installed" rather than as an error.
async fn detect_and_publish(agents: &Arc<AgentRegistry>, bus: &EventBus) -> Vec<AgentBinary> {
    let detected = match agents.detect(false).await {
        Ok(rows) => rows,
        Err(err) => {
            tracing::warn!(target: "agents", error = %err, "agent detection failed");
            Vec::new()
        }
    };
    if !detected.is_empty() {
        bus.emit(
            events::AGENTS_DETECTED,
            events::AgentsDetectedEvent {
                agents: detected.clone(),
            },
        );
    }
    detected
}

/// The account slots the fleet is configured with (T22).
///
/// Enabled slots only: a slot the user switched off must not be reachable by an
/// `accountId` that survived in an old dispatch spec.
fn mesh_accounts(slots: &[accounts::AccountSlot]) -> Vec<sidecar::Account> {
    slots
        .iter()
        .filter(|slot| slot.enabled)
        .map(|slot| {
            sidecar::Account::new(slot.vendor.clone())
                .with_label(Some(slot.label.clone()))
                .with_config_dir(slot.config_dir.clone())
        })
        .collect()
}

/// The four vendor binaries agent-mesh's adapters take, from what detection
/// actually found. A CLI that is not installed is absent, not an empty string:
/// an empty path would make the adapter try to exec `""`.
fn binary_paths(detected: &[AgentBinary]) -> BinaryPaths {
    let path_of = |kind: AgentKind| -> Option<String> {
        detected
            .iter()
            .find(|b| b.kind == kind && b.installed)
            .and_then(|b| b.abs_path.as_ref())
            .map(|p| p.display().to_string())
    };
    BinaryPaths {
        claude: path_of(AgentKind::Claude),
        codex: path_of(AgentKind::Codex),
        cursor_agent: path_of(AgentKind::Cursor),
        agy: path_of(AgentKind::Antigravity),
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_installed_binaries_reach_the_fleet_configuration() {
        let claude = AgentBinary {
            kind: AgentKind::Claude,
            abs_path: Some(PathBuf::from("/nvm/bin/claude")),
            version: Some("2.1.263".into()),
            installed: true,
            ok: true,
            verified: true,
            verified_range: None,
            warning: None,
            source: agents::BinarySource::Detected,
            detected_at: None,
        };
        let cursor = AgentBinary::absent(AgentKind::Cursor, None);

        let paths = binary_paths(&[claude, cursor]);
        assert_eq!(paths.claude.as_deref(), Some("/nvm/bin/claude"));
        assert_eq!(
            paths.cursor_agent, None,
            "an absent CLI is omitted, never sent as an empty path"
        );
        assert_eq!(paths.codex, None);
    }
}
