//! The composition root's service graph (contract §2.4 B6+, A10.6, A22).
//!
//! Owner: T09 (T09b). `lib.rs` builds the app-bound pieces (the window's
//! emitters, the native dialogs, the main-thread observers) and hands them in
//! as a [`Platform`]; [`wire`] builds every service of the other build tasks
//! against the core of `setup()` and binds every cross-service hook;
//! [`manage_all`] puts every managed state the commands read into the app.
//! Tests run exactly this path with fakes (`tests/wiring.rs`, T-wiring-*):
//! no vendor CLI, no window, no real notification, Dock or focus change.
//!
//! The hooks, each in one place (the amendment that asks for it):
//!
//! * World — `Lifecycle::bind_world` (A12.1/A12.3), `MetaStore` over
//!   `session_meta` (A18.4), the uid resolver of history (A11.4), the Claude
//!   team adapter as discovery's `TeamObserver` and the follower's
//!   `RowSink` (A11.1, A10.5), `set_notify` (A16.1), attention settings, the
//!   view/Split/quit-record refs, sidebar order and folds, the time zone;
//! * compat → `sink.compat(list)` on every change and once at boot (A10.1);
//! * discovery → `ScanHub::publish` + PaneHost's `observe_scan` (A12.2),
//!   orchestrator presence → `sink.integrations` + the sidecar flag (A11.6),
//!   the supervisor as `OwnProcesses` (A11.3), wake/visibility cadence;
//! * follow → World's sink, the usage gate on `transcriptUsage` (A10.4),
//!   `follow_structure` for every live Claude session's transcript;
//! * notifications → pause changes into `paused_until` (A16.1), settings
//!   and locale, `boot_settled`, the watchdog's memory edge;
//! * the sidecar's binary resolver (A12.6), its orchestrator snapshots into
//!   World's orchestrator mapping, the accounts service;
//! * PaneHost's World-backed hooks (A21.2), its pane table into World's
//!   Claude adapter (A21.1) and the hosted-lead record; the message-body
//!   source of `team_message_body` (A27.3): `TeamCommands::set_bodies` with
//!   `follow::hub::MessageBodySource` over World's locator (A27.1) and the
//!   on-demand transcript reader (A27.2);
//! * git/IDE through the supervisor (A17.1), the diff-dir sweep.
//!
//! * the sessions manager (T10, A22.5): built from all of the above and
//!   bound (`Services::bind_sessions`, `AccountsService::bind_sessions`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::Value;

use crate::accounts::{AccountEnvSink, AccountsService};
use crate::agents::AgentRegistry;
use crate::compat::CompatService;
use crate::config::ConfigStore;
use crate::contract::common::{AbsPath, EpochMs, Vendor};
use crate::contract::compat::ClaudeCompat;
use crate::contract::error::IpcError;
use crate::contract::ids::{AccountId, MemberKey, PtyId, SessionUid, TeamKey};
use crate::contract::ipc::{PauseUntil, ViewState};
use crate::contract::services::{
    CompatApi, CompatSink, ContentHubApi, EvidenceSink, EvidenceUpdate, LifecycleOp, LocationFacts, NotifyApi,
    PaneHostApi, ProcSnapshot, SidecarApi, SourceFacts, SupervisorApi, WorldApi,
};
use crate::contract::activity::ProcessFacts;
use crate::contract::sidecar::AccountEnv;
use crate::contract::store::{IntegrationsState, OrchestratorPresence, SourceStatus, VisibilityEvent};
use crate::convo::driver::ConvoDeps;
use crate::discovery::history::{HistoryService, HistorySources, HistoryState, IndexStore, SqliteIndexStore};
use crate::discovery::{Discovery, DiscoveryConfig};
use crate::follow::hub::{ContentHub, HistoryLocator, WorldResolver};
use crate::follow::tail::{FollowConfig, Follower, TranscriptFormat, TRANSCRIPT_USAGE_FLAG};
use crate::ide::paths::PickedRoots;
use crate::ide::IdeService;
use crate::lifecycle::boot::Lifecycle;
use crate::macos::owner::{AppActivator, OwnerFocusService, OwnerState};
use crate::macos::sleep::{SystemEvent, SystemEvents};
use crate::macos::visibility::VisibilityHub;
use crate::notify::{NotificationSink, NotifyConfig, NotifyDeps, NotifyService, NotifyState};
use crate::panehost::{LeadEnvSource, PaneHost, PaneHostConfig, PaneHostDeps, PaneSessions, WorldLeadFate};
use crate::paths::AppPaths;
use crate::pty::PtyManager;
use crate::sidecar::{MeshConfig, SidecarManager, SidecarRuntime};
use crate::state::Services;
use crate::status::StatusEngine;
use crate::store::{dao, Db};
use crate::supervise::health::HealthState;
use crate::supervise::registry::{EnvBase, Supervisor};
use crate::supervise::watchdog::Watchdog;
use crate::turns::driver::{TurnRenderer, TurnRendererFactory};
use crate::world::{ControlDetail, MemoryMetaStore, MetaRow, MetaStore, RefSource, World, WorldConfig};
use crate::worktree::{GitService, WorktreeManager, WorldPlaces};

// ---------------------------------------------------------------------------
// inputs
// ---------------------------------------------------------------------------

/// What `setup()` built (B1–B4) plus the memory-only state the commands
/// already read.
#[derive(Clone)]
pub struct Core {
    pub paths: Arc<AppPaths>,
    pub db: Db,
    pub config: Arc<ConfigStore>,
    pub lifecycle: Arc<Lifecycle>,
    pub supervisor: Arc<Supervisor>,
    pub services: Arc<Services>,
    pub health: Arc<HealthState>,
}

/// The OS/app boundary: everything that talks to the window, the Dock, the
/// notification center, a dialog or another app. `lib.rs` passes the real
/// ones; tests pass fakes.
pub struct Platform {
    /// Named events to the app (only the four global events leave).
    pub events: crate::events::Events,
    /// The notification sink for the `notificationCenter` app flag (step 4).
    pub notification_sink: Box<dyn FnOnce(bool) -> Arc<dyn NotificationSink> + Send>,
    pub dock: Arc<dyn crate::macos::dock::DockSink>,
    pub focus: Arc<dyn crate::notify::FocusEmitter>,
    pub notify_clock: Arc<dyn crate::notify::Clock>,
    pub activator: Arc<dyn AppActivator>,
    /// `project_pick`'s native picker.
    pub project_picker: Arc<dyn crate::commands::project::FolderPicker>,
    /// The accounts service's native picker.
    pub account_picker: Arc<dyn crate::accounts::FolderPicker>,
    /// The native confirmation of a local link (`DialogLinkConfirm`).
    pub link_confirm: Arc<dyn crate::ide::links::LinkConfirm>,
    /// The sidecar's node + script (resolved by `lib.rs` from the bundle).
    pub sidecar_runtime: SidecarRuntime,
    /// The macOS hubs; `lib.rs` installs their observers on the main thread.
    pub system: Arc<SystemEvents>,
    pub visibility: Arc<VisibilityHub>,
}

/// Last-word changes to discovery's configuration (tests: a fixture process
/// table and fixture accounts, no FSEvents).
pub type DiscoveryAdjust = Box<dyn FnOnce(&mut DiscoveryConfig) + Send>;

/// Knobs that differ between the app and the tests.
pub struct Options {
    /// See [`DiscoveryAdjust`].
    pub discovery: Option<DiscoveryAdjust>,
    /// The process table discovery and the sessions' liveness re-check read
    /// (`None` = the kernel; tests pass a fixture table).
    pub procs: Option<Arc<dyn crate::discovery::procs::ProcSource>>,
    /// The follower's own FSEvents watch.
    pub follow_watch: bool,
    /// Start the periodic loops (watchdog tick, account refresh, PaneHost
    /// reconcile, the status ticker). Tests drive them by hand.
    pub background: bool,
    /// The multi-thread runtime PaneHost's listener runs on.
    pub runtime: tokio::runtime::Handle,
}

impl Options {
    /// The app's options on the current runtime.
    pub fn app() -> Options {
        Options {
            discovery: None,
            procs: None,
            follow_watch: true,
            background: true,
            runtime: tokio::runtime::Handle::current(),
        }
    }
}

// ---------------------------------------------------------------------------
// the result
// ---------------------------------------------------------------------------

/// Every service, kept for the life of the app (and for the quit path).
pub struct Wired {
    pub world: Arc<World>,
    pub compat: Arc<CompatService>,
    pub history: Arc<HistoryService>,
    /// The uid resolver history was built with (A11.4: World's, never
    /// `MemoryUids`).
    pub uids: Arc<dyn crate::discovery::history::UidResolver>,
    pub discovery: Arc<parking_lot::RwLock<Option<Discovery>>>,
    /// What discovery classifies Gilde's own processes with (A11.3): the
    /// supervisor itself.
    pub own: Arc<Supervisor>,
    pub follower: Follower,
    pub follow: Arc<FollowDriver>,
    pub hub: Arc<ContentHub>,
    pub system: Arc<SystemEvents>,
    pub visibility: Arc<VisibilityHub>,
    pub notify: Arc<NotifyService>,
    /// What `NotifyState` and `Services` hold: notifications behind the
    /// view-state fan-out ([`ViewFanout`]).
    pub notify_api: Arc<dyn NotifyApi>,
    pub owner: Arc<OwnerFocusService>,
    pub git: Arc<GitService>,
    pub ide: Arc<IdeService>,
    pub picked: Arc<PickedRoots>,
    pub project: crate::commands::project::ProjectState,
    pub agents: Arc<AgentRegistry>,
    pub worktrees: Arc<WorktreeManager>,
    pub status: Arc<StatusEngine>,
    pub pty: Arc<PtyManager>,
    pub panehost: Arc<PaneHost>,
    pub team: Arc<crate::commands::team::TeamCommands>,
    pub sidecar: Arc<SidecarManager>,
    pub accounts: Arc<AccountsService>,
    pub watchdog: Arc<Watchdog>,
    /// PaneHost as the sessions manager sees it ([`HostedLeadsApi`]).
    pub panehost_api: Arc<dyn PaneHostApi>,
    /// The sessions manager (T10).
    pub sessions: Arc<crate::sessions::SessionsManager>,
    pub tasks: Tasks,
    /// `<app_data>/tmp/diff` (swept at boot for old copies, at quit for all).
    pub tmp_diff_dir: PathBuf,
}

/// Background tasks whose handle is their lifetime (dropping aborts them).
#[derive(Default)]
pub struct Tasks {
    tauri: Mutex<Vec<tauri::async_runtime::JoinHandle<()>>>,
    tokio: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Tasks {
    pub fn push(&self, task: tauri::async_runtime::JoinHandle<()>) {
        self.tauri.lock().push(task);
    }

    pub fn push_tokio(&self, task: tokio::task::JoinHandle<()>) {
        self.tokio.lock().push(task);
    }

    pub fn len(&self) -> usize {
        self.tauri.lock().len() + self.tokio.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn abort_all(&self) {
        for t in self.tauri.lock().drain(..) {
            t.abort();
        }
        for t in self.tokio.lock().drain(..) {
            t.abort();
        }
    }
}

impl Drop for Tasks {
    fn drop(&mut self) {
        self.abort_all();
    }
}

impl Wired {
    /// Stop what holds threads, sockets and temporary files (§10.2, after
    /// the sessions are down): the background loops, discovery, the
    /// follower, PaneHost, World's publisher, the IDE diff copies.
    pub fn shutdown(&self) {
        self.tasks.abort_all();
        let taken = self.discovery.write().take();
        if let Some(d) = taken {
            d.stop();
        }
        self.follower.shutdown();
        self.panehost.stop();
        crate::ide::opener::sweep_diff_dir(&self.tmp_diff_dir, Duration::ZERO);
        self.world.shutdown();
    }
}

// ---------------------------------------------------------------------------
// wire
// ---------------------------------------------------------------------------

/// Build every service against `core` and bind every hook (see the module
/// docs). Runs after the boot sweep (B6): nothing here spawns a process, and
/// everything that would (the sidecar, PTYs, git, probes) waits for the gate
/// anyway. `Err` only when the shipped compat data cannot be read (a build
/// defect); every other failure is logged and the app keeps going.
pub async fn wire(core: &Core, platform: Platform, opts: Options) -> Result<Wired, String> {
    let paths = core.paths.clone();
    let db = core.db.clone();
    let config = core.config.clone();
    let sup = core.supervisor.clone();
    let lifecycle = core.lifecycle.clone();
    let home = paths.home_dir().to_path_buf();
    let home_str = home.to_string_lossy().into_owned();
    let memory_only = db.is_memory_only() || lifecycle.memory_only.is_active();
    let tasks = Tasks::default();

    // -- World (T02) ---------------------------------------------------------
    let meta: Arc<dyn MetaStore> = if memory_only {
        Arc::new(MemoryMetaStore::default())
    } else {
        DbMetaStore::open(&db).await
    };
    let world = Arc::new(World::new(WorldConfig::system(Some(home_str.clone()), meta)));
    world.start();
    let world_api: Arc<dyn WorldApi> = world.clone();
    let sink = world_api.sink();
    // A12.1/A12.3: World bound and the lifecycle feed switched, in one call.
    if !lifecycle.bind_world(&core.services, world_api.clone()) {
        tracing::warn!(target: "app", "World was bound already");
    }

    // -- compat (T21) --------------------------------------------------------
    let store: Option<Arc<dyn crate::compat::sink::ObservedStore>> =
        (!memory_only).then(|| Arc::new(db.clone()) as Arc<dyn crate::compat::sink::ObservedStore>);
    let compat = Arc::new(CompatService::bundled(store).map_err(|e| format!("compat data: {}", e.message()))?);
    {
        // A10.1: every change re-reads the list (no compat lock held here).
        let c = Arc::downgrade(&compat);
        let s = sink.clone();
        compat.set_on_change(Arc::new(move || {
            if let Some(c) = c.upgrade() {
                s.compat(CompatApi::list(c.as_ref()));
            }
        }));
    }
    sink.compat(CompatApi::list(compat.as_ref()));
    core.services.bind_compat(compat.clone());

    // -- agents (T09) --------------------------------------------------------
    let recorder: crate::agents::Recorder = Arc::new(crate::agents::SupervisorRecorder::with_handle(
        sup.clone(),
        opts.runtime.clone(),
    ));
    crate::agents::BinaryResolver::global().set_recorder(recorder.clone());
    let agents = Arc::new(AgentRegistry::new((*paths).clone(), config.clone(), Some(db.clone()), recorder));

    // -- worktrees, git, IDE (T11) ------------------------------------------
    crate::worktree::git::bind_supervisor(sup.clone() as Arc<dyn SupervisorApi>);
    crate::worktree::alias::install(db.clone(), Some(paths.clone()));
    let worktrees = WorktreeManager::new(
        db.clone(),
        paths.clone(),
        Some(Arc::new(platform.events.clone()) as Arc<dyn crate::worktree::WorktreeEvents>),
    );
    if let Err(err) = worktrees.resume_watches().await {
        tracing::warn!(target: "app", error = %err, "could not resume worktree watches");
    }
    let git = Arc::new(GitService::new(world_api.clone(), home.clone()).with_compat(compat.clone()));
    let picked = PickedRoots::new();
    let tmp_diff_dir = paths.tmp_diff_dir().to_path_buf();
    let swept = crate::ide::opener::sweep_diff_dir(&tmp_diff_dir, crate::ide::opener::DIFF_MAX_AGE);
    if swept > 0 {
        tracing::debug!(target: "app", swept, "old diff copies removed");
    }
    let chosen: crate::ide::ChosenEditor = {
        let config = config.clone();
        Arc::new(move || config.get().ide.editor.as_deref().and_then(crate::ide::editors::from_setting))
    };
    let ide = Arc::new(
        IdeService::new(
            Arc::new(WorldPlaces(world_api.clone())),
            home.clone(),
            tmp_diff_dir.clone(),
            picked.clone(),
            chosen,
            crate::ide::Platform::macos(sup.clone() as Arc<dyn SupervisorApi>, &home, platform.link_confirm.clone()),
        )
        .with_diff_source(git.clone()),
    );
    let project = crate::commands::project::ProjectState {
        picker: platform.project_picker.clone(),
        picked: picked.clone(),
        places: Some(Arc::new(WorldPlaces(world_api.clone()))),
        home: home.clone(),
    };

    // -- PTY + status (T06) --------------------------------------------------
    let status = Arc::new(StatusEngine::new());
    status.set_signatures(agents.clone());
    status.set_stall_after_ms(config.get().status.stall_after_ms);
    let pty = Arc::new(PtyManager::new(sup.clone(), config.clone()).with_status(status.clone()));
    pty.set_evidence_sink(sink.clone());
    if opts.background {
        tasks.push(StatusEngine::spawn_ticker(status.clone()));
    }

    // -- the sidecar, turns, accounts (T08) ----------------------------------
    let path_env = std::env::var("PATH").unwrap_or_default();
    let settings = config.get();
    let sidecar = SidecarManager::with_presence(
        sup.clone(),
        platform.sidecar_runtime,
        EnvBase::from_process_env(path_env.clone()),
        MeshConfig::default(),
        Duration::from_secs(settings.sidecar.ping_interval_seconds.max(1) as u64),
        crate::commands::orchestrator::registry_present(&paths),
    );
    sidecar.set_sweeper(crate::supervise::reaper::ReaperSweeper::new(sup.clone()));
    {
        // A12.6: the vendor binaries are resolved on every sidecar start.
        let agents = Arc::downgrade(&agents);
        sidecar.set_binary_resolver(Arc::new(move |vendor: Vendor| {
            let agents = agents.clone();
            Box::pin(async move {
                let agents = agents.upgrade()?;
                agents.resolve_vendor(vendor).await
            })
        }));
    }
    let account_envs = Arc::new(Mutex::new(HashMap::<String, AccountEnv>::new()));
    let account_envs_for_liveness = account_envs.clone();
    let env_tee: Arc<dyn AccountEnvSink> = Arc::new(AccountEnvTee {
        sidecar: sidecar.clone(),
        cache: account_envs.clone(),
    });
    let accounts = AccountsService::new(
        db.clone(),
        (*paths).clone(),
        sidecar.clone() as Arc<dyn SidecarApi>,
        Some(env_tee),
        platform.account_picker.clone(),
    );
    accounts.bind_world(world_api.clone());
    if opts.background {
        tasks.push_tokio(accounts.spawn_background_refresh());
    }
    tasks.push(spawn_orchestrator_relay(&sidecar, &world));

    // -- render & follow (T04) -----------------------------------------------
    let follower = Follower::start(FollowConfig {
        watch: opts.follow_watch,
        ..FollowConfig::default()
    });
    follower.set_evidence_sink(sink.clone());
    follower.set_row_sink(world.claude_team_adapter());
    let follow = Arc::new(FollowDriver::new(sink.clone(), follower.clone(), compat.clone()));
    {
        // A10.4: no transcript spend until the flag says so for the version.
        let f = Arc::downgrade(&follow);
        follower.set_usage_gate(Arc::new(move |path: &Path, _hint| f.upgrade().is_some_and(|f| f.usage_allowed(path))));
    }
    let hub = ContentHub::new(
        follower.clone(),
        Some(Arc::new(WorldResolver::new(
            world_api.clone(),
            AbsPath::parse(home_str.clone()).ok(),
        ))),
    );

    // -- macOS integration (T12) ---------------------------------------------
    let system = platform.system.clone();
    let visibility = platform.visibility.clone();
    visibility.follow(&system);
    let notify = NotifyService::start(
        NotifyDeps {
            sink: (platform.notification_sink)(compat.app_feature("notificationCenter")),
            dock: platform.dock.clone(),
            visibility: visibility.clone(),
            system: system.clone(),
            labels: Arc::new(crate::notify::WorldLabels::new(world_api.clone())),
            focus: platform.focus.clone(),
            clock: platform.notify_clock.clone(),
        },
        NotifyConfig::default(),
    );
    apply_notify_settings(&notify, &config);
    {
        // A16.1: a pause (or its end) reaches `AttentionQueue.paused_until`.
        let s = sink.clone();
        notify.on_pause_change(Arc::new(move |until| s.notify_paused(until)));
    }
    let notify_api: Arc<dyn NotifyApi> = Arc::new(ViewFanout {
        inner: notify.clone(),
        world: world.clone(),
        accounts: accounts.clone(),
    });
    world.set_notify(notify.clone());
    core.services.bind_notify(notify_api.clone());
    let owner = OwnerFocusService::new(
        world_api.clone(),
        platform.activator.clone(),
        compat.app_feature("vscodeWindowFocus"),
    );
    owner.set_ide(ide.clone());

    // -- World's side inputs ---------------------------------------------------
    world.set_attention_settings(&config.get().to_settings().attention);
    apply_ui_state(&world, &lifecycle.ui_state());
    {
        let w = Arc::downgrade(&world);
        lifecycle.on_ui_state(Arc::new(move |key, value| {
            if let Some(w) = w.upgrade() {
                apply_ui_key(&w, key, value);
            }
        }));
    }
    {
        let w = Arc::downgrade(&world);
        let n = Arc::downgrade(&notify);
        config.on_change(Arc::new(move |cfg| {
            let settings = cfg.to_settings();
            if let Some(w) = w.upgrade() {
                w.set_attention_settings(&settings.attention);
            }
            if let Some(n) = n.upgrade() {
                n.set_settings(settings.notify.clone());
                n.set_locale(notify_locale(cfg));
            }
        }));
    }
    refresh_quit_record_refs(&db, &world).await;

    // -- PaneHost (T07) --------------------------------------------------------
    match crate::panehost::lead_env::bundled_shim() {
        Some(src) => {
            if let Err(err) = crate::panehost::install_shim(&src, &paths.shim_dir(crate::BUILD_ID)) {
                tracing::warn!(target: "panehost", error = %err, "could not install the tmux shim");
            }
        }
        None => tracing::debug!(target: "panehost", "no bundled tmux shim next to the executable"),
    }
    if let Err(err) = crate::panehost::prune_shims(paths.shim_root(), &[crate::BUILD_ID]) {
        tracing::debug!(target: "panehost", error = %err, "could not prune old shim directories");
    }
    let panehost = PaneHost::new(
        PaneHostConfig::for_paths(&paths, crate::BUILD_ID),
        PaneHostDeps {
            pty: pty.clone() as Arc<dyn crate::contract::services::PtyApi>,
            world: world_api.clone(),
            compat: compat.clone() as Arc<dyn CompatSink>,
            env: Arc::new(LeadEnv {
                world: world_api.clone(),
                supervisor: sup.clone(),
                base: EnvBase::from_process_env(path_env),
                accounts: account_envs,
            }),
            fate: Arc::new(WorldLeadFate::new(world_api.clone())),
            sessions: Arc::new(WorldPaneSessions { world: world.clone() }),
        },
    );
    // A21.1: PaneHost's own pane table is World's hosted-pane input.
    panehost.set_table_sink(world.claude_team_adapter());
    if let Err(problem) = panehost.start(opts.runtime.clone()) {
        tracing::warn!(target: "panehost", problem = ?problem, "PaneHost did not start; hosted teams fall back to in-process");
    }
    if opts.background {
        tasks.push_tokio(panehost.spawn_reconcile_loop(opts.runtime.clone(), crate::panehost::RECONCILE_EVERY));
    }
    let team = crate::commands::team::TeamCommands::new(panehost.clone());
    // A27.3: message bodies on demand (memory only, behind T07's LRU).
    team.set_bodies(Arc::new(crate::follow::hub::MessageBodySource::new(world.clone())));
    let panehost_api: Arc<dyn PaneHostApi> = Arc::new(HostedLeadsApi {
        inner: panehost.clone(),
        supervisor: sup.clone(),
        leads: Mutex::new(HashMap::new()),
    });

    // -- the watchdog (T09) ----------------------------------------------------
    let watchdog = {
        let n = Arc::downgrade(&notify);
        Arc::new(
            Watchdog::new(sup.clone(), platform.events.clone())
                .with_agents(agents.clone())
                .with_scan_hub(lifecycle.scans.clone())
                .on_memory_pressure(Arc::new(move |high| {
                    if let Some(n) = n.upgrade() {
                        n.health_memory(high);
                    }
                })),
        )
    };
    if opts.background {
        tasks.push(watchdog.start());
    }

    // -- discovery + history (T03) --------------------------------------------
    let index: Option<Arc<dyn IndexStore>> = if memory_only {
        None
    } else {
        // A51.3: the index connection gets the store's pragmas.
        match rusqlite::Connection::open(paths.state_db()).and_then(SqliteIndexStore::with_store_pragmas) {
            Ok(store) => Some(Arc::new(store)),
            Err(err) => {
                tracing::warn!(target: "discovery", error = %err, "history index store not opened; memory only");
                None
            }
        }
    };
    let uids = world.uid_resolver();
    let history = HistoryService::new(HistorySources::default(), uids.clone(), index);
    // A29.2: `convo_subscribe` asks World first, then the history index for
    // a session World does not hold (History's Read of an old session).
    hub.set_resolver(Arc::new(
        WorldResolver::new(world_api.clone(), AbsPath::parse(home_str.clone()).ok())
            .with_history(history.clone() as Arc<dyn HistoryLocator>),
    ));
    let mut dcfg = DiscoveryConfig::for_home(&home, paths.app_data_dir(), sup.clone());
    {
        let s = sup.clone();
        dcfg.redact = Arc::new(move |argv: &[String]| SupervisorApi::redact_argv(s.as_ref(), argv));
    }
    dcfg.teams = world.claude_team_adapter();
    dcfg.history = Some(history.clone());
    {
        // A12.2: the one process scan, for supervision and PaneHost.
        let scans = lifecycle.scans.clone();
        let host = Arc::downgrade(&panehost);
        dcfg.on_snapshot = Some(Arc::new(move |snap: &ProcSnapshot| {
            scans.publish(snap.clone());
            if let Some(h) = host.upgrade() {
                h.observe_scan(snap);
            }
        }));
    }
    {
        // A11.6: orchestrator presence → the store and the sidecar.
        let s = sink.clone();
        let sc = Arc::downgrade(&sidecar);
        dcfg.on_orchestrator = Some(Arc::new(move |present: bool| {
            s.integrations(IntegrationsState {
                orchestrator: if present {
                    OrchestratorPresence::Present
                } else {
                    OrchestratorPresence::Absent
                },
            });
            if let Some(sc) = sc.upgrade() {
                tauri::async_runtime::spawn(async move { sc.set_orchestrator_present(present).await });
            }
        }));
    }
    if let Some(p) = opts.procs.clone() {
        dcfg.source = p;
    }
    if let Some(adjust) = opts.discovery {
        adjust(&mut dcfg);
    }
    let discovery = Discovery::start(dcfg, follow.clone() as Arc<dyn EvidenceSink>);
    let discovery = Arc::new(parking_lot::RwLock::new(Some(discovery)));
    lifecycle.timer_mark("B7.discovery");

    // -- the sessions manager (T10, A22.5) ------------------------------------
    let procs: Arc<dyn crate::discovery::procs::ProcSource> = match opts.procs.clone() {
        Some(p) => p,
        None => Arc::new(crate::discovery::procs::Libproc::new()),
    };
    let claude_dirs: Arc<dyn Fn() -> Vec<PathBuf> + Send + Sync> = {
        let cache = account_envs_for_liveness;
        let default_dir = home.join(".claude");
        Arc::new(move || {
            let mut dirs = vec![default_dir.clone()];
            for env in cache.lock().values() {
                if env.vendor == Vendor::Claude {
                    if let Some(dir) = &env.config_dir {
                        dirs.push(PathBuf::from(dir.as_str()));
                    }
                }
            }
            dirs.sort();
            dirs.dedup();
            dirs
        })
    };
    let clock: crate::sessions::manager::WallClock = Arc::new(crate::store::now_ms);
    let prefs: Arc<dyn crate::sessions::PrefsStore> = if memory_only {
        Arc::new(crate::sessions::MemoryPrefs::default())
    } else {
        Arc::new(crate::sessions::DbPrefs::new(db.clone(), clock.clone()))
    };
    let transports = crate::sessions::LiveTransports {
        convo: ConvoDeps {
            patterns: crate::convo::frames::PatternSource::with_override(paths.app_data_dir().join("error-patterns.json")),
            // `hooks` stays `NoHooks` here: the manager sets them per session.
            ..ConvoDeps::new(
                sup.clone() as Arc<dyn SupervisorApi>,
                compat.clone() as Arc<dyn CompatApi>,
                compat.clone() as Arc<dyn CompatSink>,
                sink.clone(),
                hub.clone() as Arc<dyn ContentHubApi>,
            )
        },
        pty: pty.clone(),
        supervisor: sup.clone() as Arc<dyn SupervisorApi>,
        turns: crate::turns::driver::TurnDeps::new(
            sidecar.clone() as Arc<dyn SidecarApi>,
            sink.clone(),
            hub.clone() as Arc<dyn ContentHubApi>,
            Arc::new(MeshRenderers),
        ),
        sidecar: Some(sidecar.clone() as Arc<dyn SidecarApi>),
    };
    let sessions = crate::sessions::SessionsManager::new(crate::sessions::SessionDeps {
        world: world_api.clone(),
        binder: world.clone(),
        supervisor: sup.clone() as Arc<dyn SupervisorApi>,
        transports: Arc::new(transports),
        binaries: Arc::new(crate::sessions::AgentBinaries::new(agents.clone(), compat.clone() as Arc<dyn CompatApi>)),
        accounts: Arc::new(crate::sessions::ServiceAccounts::new(accounts.clone(), world_api.clone())),
        env: Arc::new(crate::sessions::SupervisorEnv::new(
            sup.clone(),
            EnvBase::from_process_env(std::env::var("PATH").unwrap_or_default()),
        )),
        compat: compat.clone() as Arc<dyn CompatApi>,
        liveness: Arc::new(crate::sessions::ScanLiveness::new(claude_dirs, procs)),
        prefs,
        quit_items: Arc::new(crate::sessions::DbQuitItems::new(db.clone())),
        git: Some(git.clone() as Arc<dyn crate::contract::services::GitApi>),
        panehost: Some(panehost_api.clone()),
        sidecar: Some(sidecar.clone() as Arc<dyn SidecarApi>),
        clock,
        timing: crate::sessions::SessionTiming::default(),
    });
    core.services.bind_sessions(sessions.clone());
    accounts.bind_sessions(sessions.clone());

    let wired = Wired {
        world: world.clone(),
        compat: compat.clone(),
        history,
        uids,
        discovery,
        own: sup.clone(),
        follower,
        follow,
        hub: hub.clone(),
        system: system.clone(),
        visibility: visibility.clone(),
        notify: notify.clone(),
        notify_api,
        owner,
        git: git.clone(),
        ide: ide.clone(),
        picked,
        project,
        agents,
        worktrees,
        status,
        pty: pty.clone(),
        panehost,
        team,
        sidecar: sidecar.clone(),
        accounts: accounts.clone(),
        watchdog,
        panehost_api,
        sessions,
        tasks,
        tmp_diff_dir,
    };
    bind_system_events(&wired);
    Ok(wired)
}

/// Step 11 (T12) and B8: once discovery's first reconcile ran (its `sync`
/// returns only after it), the boot is settled for notifications and the
/// boot status says so. Off the async workers.
pub fn spawn_boot_settled(wired: &Arc<Wired>, lifecycle: &Arc<Lifecycle>) -> tauri::async_runtime::JoinHandle<()> {
    let w = Arc::downgrade(wired);
    let w2 = w.clone();
    let lc = lifecycle.clone();
    tauri::async_runtime::spawn(async move {
        // Short syncs under a read lock, so a quit at boot (which takes the
        // write lock to stop discovery) waits at most one of them.
        let settled = tokio::task::spawn_blocking(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(60);
            loop {
                let w = w.upgrade()?;
                let d = w.discovery.read();
                let answered = d.as_ref()?.sync(Duration::from_secs(2)).is_some();
                if answered {
                    return Some(true);
                }
                if std::time::Instant::now() >= deadline {
                    return Some(false);
                }
            }
        })
        .await
        .ok()
        .flatten();
        lc.feed.update_boot(|b| b.first_discovery_done = true);
        lc.timer_mark("B8.reconcile");
        if settled != Some(true) {
            tracing::warn!(target: "discovery", "the first reconcile did not answer in time; settling anyway");
        }
        if let Some(w) = w2.upgrade() {
            w.notify.boot_settled();
        }
    })
}

/// Wake, sleep, time zone and window visibility to the services that care
/// (T12 step 13, §10.4).
fn bind_system_events(wired: &Wired) {
    let world = Arc::downgrade(&wired.world);
    let watchdog = Arc::downgrade(&wired.watchdog);
    let discovery_wake = wired.discovery_handle();
    wired.system.subscribe(Arc::new(move |event| match event {
        SystemEvent::DidWake => {
            if let Some(w) = watchdog.upgrade() {
                w.woke();
            }
            discovery_wake.wake();
        }
        SystemEvent::TimeZoneChanged => {
            if let Some(w) = world.upgrade() {
                w.time_zone_changed();
            }
        }
        SystemEvent::WillSleep => {}
    }));
    let discovery_vis = wired.discovery_handle();
    wired.visibility.subscribe(Arc::new(move |event: VisibilityEvent| {
        discovery_vis.set_frontmost(event.frontmost && !event.asleep);
    }));
}

/// A weak handle on the running discovery (its methods take `&self` and
/// only queue a command, so a read lock never blocks the caller).
#[derive(Clone)]
pub struct DiscoveryHandle(std::sync::Weak<parking_lot::RwLock<Option<Discovery>>>);

impl DiscoveryHandle {
    pub fn wake(&self) {
        if let Some(d) = self.0.upgrade() {
            if let Some(d) = d.read().as_ref() {
                d.wake();
            }
        }
    }

    pub fn set_frontmost(&self, frontmost: bool) {
        if let Some(d) = self.0.upgrade() {
            if let Some(d) = d.read().as_ref() {
                d.set_frontmost(frontmost);
            }
        }
    }
}

impl Wired {
    /// A weak handle on discovery for subscribers.
    pub fn discovery_handle(&self) -> DiscoveryHandle {
        DiscoveryHandle(Arc::downgrade(&self.discovery))
    }
}

// ---------------------------------------------------------------------------
// managed state
// ---------------------------------------------------------------------------

/// The states `setup()` manages before the window (all memory-only): paths,
/// database, settings, supervisor, lifecycle, the late-bound services, the
/// process records, the local store of the store commands.
pub fn manage_core<R: tauri::Runtime, M: tauri::Manager<R>>(
    app: &M,
    core: &Core,
    local: Arc<crate::commands::store::LocalStore>,
) {
    app.manage(core.paths.clone() as crate::commands::app::PathsState);
    app.manage(core.db.clone() as crate::commands::accounts::DbState);
    app.manage(core.config.clone() as crate::commands::accounts::ConfigState);
    app.manage(core.supervisor.clone());
    app.manage(core.lifecycle.clone() as crate::commands::app::LifecycleState);
    app.manage(core.services.clone() as crate::commands::app::ServicesState);
    app.manage(core.health.clone() as crate::commands::app::HealthStateRef);
    app.manage(local as crate::commands::store::LocalStoreState);
}

/// Put every managed state the commands read into the app (after [`wire`]).
/// The states `setup()` manages itself (paths, db, config, supervisor,
/// lifecycle, services, health, local store) are not repeated here.
pub fn manage_all<R: tauri::Runtime, M: tauri::Manager<R>>(app: &M, wired: &Wired) {
    app.manage(HistoryState(wired.history.clone()));
    app.manage(crate::commands::convo::ConvoState(wired.hub.clone()));
    app.manage(NotifyState::new(wired.notify_api.clone(), wired.world.clone() as Arc<dyn WorldApi>));
    app.manage(OwnerState(wired.owner.clone()));
    app.manage(crate::commands::changes::GitState(wired.git.clone()));
    app.manage(crate::commands::ide::IdeState::new(wired.ide.clone(), wired.picked.clone()));
    app.manage(wired.project.clone());
    app.manage(crate::commands::team::TeamState(wired.team.clone()));
    app.manage(wired.pty.clone() as crate::commands::pty::PtyState);
    app.manage(wired.status.clone() as crate::status::StatusState);
    app.manage(wired.sidecar.clone());
    app.manage(wired.accounts.clone());
    app.manage(wired.worktrees.clone() as crate::commands::worktree::WorktreeState);
    app.manage(wired.agents.clone() as crate::commands::app::AgentsState);
    app.manage(wired.world.clone());
    app.manage(wired.watchdog.clone());
    app.manage(wired.sessions.clone());
    app.manage(wired.panehost_api.clone());
}

// ---------------------------------------------------------------------------
// World's `session_meta` store (A18.4)
// ---------------------------------------------------------------------------

/// `world::MetaStore` over `session_meta` (§3.9, metadata only — no title,
/// no content; T-priv-1's schema scan covers the table). Loaded once; every
/// change is written behind on a thread of its own (World calls it under its
/// lock, so the call only queues). A row without a vendor is not persisted
/// (the column is `NOT NULL`); a failed write is logged once per kind of
/// failure and dropped (World keeps the row in memory).
pub struct DbMetaStore {
    rows: Vec<MetaRow>,
    tx: std::sync::mpsc::Sender<MetaWrite>,
}

enum MetaWrite {
    Upsert(Box<MetaRow>),
    Remove(String),
    Barrier(std::sync::mpsc::Sender<()>),
}

impl DbMetaStore {
    /// Load every row and start the writer.
    pub async fn open(db: &Db) -> Arc<DbMetaStore> {
        let rows = db
            .read(|conn| {
                let mut stmt = conn.prepare(META_SELECT)?;
                let rows = stmt
                    .query_map([], meta_row_of)?
                    .filter_map(|r| r.ok().flatten())
                    .collect::<Vec<_>>();
                Ok(rows)
            })
            .await
            .unwrap_or_else(|err| {
                tracing::warn!(target: "world", error = %err, "session_meta could not be read; starting empty");
                Vec::new()
            });
        let (tx, rx) = std::sync::mpsc::channel::<MetaWrite>();
        let writer_db = db.clone();
        let spawned = std::thread::Builder::new()
            .name("gilde-session-meta".into())
            .spawn(move || {
                let mut warned = false;
                while let Ok(op) = rx.recv() {
                    if let MetaWrite::Barrier(done) = &op {
                        let _ = done.send(());
                        continue;
                    }
                    let result = writer_db.blocking(|conn| match &op {
                        MetaWrite::Upsert(row) => upsert_meta(conn, row),
                        MetaWrite::Remove(uid) => {
                            conn.execute("DELETE FROM session_meta WHERE uid = ?1", [uid])?;
                            Ok(())
                        }
                        MetaWrite::Barrier(_) => Ok(()),
                    });
                    if let Err(err) = result {
                        if !warned {
                            tracing::warn!(target: "world", error = %err, "a session_meta write failed (logged once)");
                            warned = true;
                        }
                    }
                }
            });
        if let Err(err) = spawned {
            tracing::warn!(target: "world", error = %err, "session_meta writer not started; changes stay in memory");
        }
        Arc::new(DbMetaStore { rows, tx })
    }

    /// Wait until every change queued so far is written (the quit, tests).
    /// `false` on timeout or when the writer is gone.
    pub fn sync(&self, timeout: Duration) -> bool {
        let (done, wait) = std::sync::mpsc::channel();
        if self.tx.send(MetaWrite::Barrier(done)).is_err() {
            return false;
        }
        wait.recv_timeout(timeout).is_ok()
    }
}

impl MetaStore for DbMetaStore {
    fn load(&self) -> Vec<MetaRow> {
        self.rows.clone()
    }

    fn upsert(&self, row: &MetaRow) {
        if row.vendor.is_some() {
            let _ = self.tx.send(MetaWrite::Upsert(Box::new(row.clone())));
        }
    }

    fn remove(&self, uid: &SessionUid) {
        let _ = self.tx.send(MetaWrite::Remove(uid.as_str().to_string()));
    }
}

const META_SELECT: &str = "SELECT uid, vendor, account_id, vendor_session_id, project_id, location_path, cwd,
        transcript_path, origin_last, control_mode_last, permission_mode_last, model_last, cli_path_last,
        cli_version_last, team_key, agent_id, teammate_name, parent_session_id, first_seen_at, started_at,
        ended_at, last_activity_at, last_seen_at, usage_day, usage_in, usage_out, usage_cached, cost_micros,
        parent_uid
   FROM session_meta";

/// A wire enum as its serde string (`camelCase` everywhere in the contract).
fn enum_str<T: serde::Serialize>(v: &Option<T>) -> Option<String> {
    v.as_ref()
        .and_then(|v| serde_json::to_value(v).ok())
        .and_then(|v| v.as_str().map(str::to_string))
}

fn enum_of<T: serde::de::DeserializeOwned>(s: Option<String>) -> Option<T> {
    s.and_then(|s| serde_json::from_value(Value::String(s)).ok())
}

fn u64_of(v: Option<i64>) -> Option<u64> {
    v.and_then(|v| u64::try_from(v).ok())
}

fn i64_of(v: Option<u64>) -> Option<i64> {
    v.and_then(|v| i64::try_from(v).ok())
}

/// One `session_meta` row; `None` for a row whose ids no longer parse (it is
/// skipped, never guessed).
fn meta_row_of(r: &rusqlite::Row<'_>) -> rusqlite::Result<Option<MetaRow>> {
    let uid: String = r.get(0)?;
    let Ok(uid) = SessionUid::parse(uid) else { return Ok(None) };
    let vendor: String = r.get(1)?;
    let usage_day: Option<String> = r.get(23)?;
    Ok(Some(MetaRow {
        uid,
        vendor: Vendor::parse(&vendor),
        account_id: r.get::<_, Option<String>>(2)?.and_then(|s| AccountId::parse(s).ok()),
        vendor_session_id: r.get(3)?,
        project_id: r
            .get::<_, Option<String>>(4)?
            .and_then(|s| crate::contract::ids::ProjectId::parse(s).ok()),
        location_path: r.get(5)?,
        cwd: r.get(6)?,
        transcript_path: r.get(7)?,
        origin_last: enum_of(r.get(8)?),
        control_mode_last: enum_of(r.get(9)?),
        permission_mode_last: enum_of(r.get(10)?),
        model_last: r.get(11)?,
        cli_path_last: r.get(12)?,
        cli_version_last: r.get(13)?,
        team_key: r.get::<_, Option<String>>(14)?.and_then(|s| TeamKey::parse(s).ok()),
        agent_id: r.get(15)?,
        teammate_name: r.get(16)?,
        parent_session_id: r.get(17)?,
        parent_uid: r.get::<_, Option<String>>(28)?.and_then(|s| SessionUid::parse(s).ok()),
        first_seen_at: r.get(18)?,
        started_at: r.get(19)?,
        ended_at: r.get(20)?,
        last_activity_at: r.get(21)?,
        last_seen_at: r.get(22)?,
        usage: usage_day.map(|day| crate::contract::session::UsageToday {
            day,
            input_tokens: None,
            output_tokens: None,
            cached_tokens: None,
            cost_micros: None,
        })
        .map(|mut u| -> rusqlite::Result<_> {
            u.input_tokens = u64_of(r.get(24)?);
            u.output_tokens = u64_of(r.get(25)?);
            u.cached_tokens = u64_of(r.get(26)?);
            u.cost_micros = u64_of(r.get(27)?);
            Ok(u)
        })
        .transpose()?,
    }))
}

/// One row per uid, and one per `(vendor, account, vendor session)`
/// identity: a second uid for the same identity replaces the first (World
/// merged them; the merged row is the survivor). The table's `UNIQUE` alone
/// does not do it for an unknown account (SQLite's NULLs are distinct), so
/// the other rows of the identity are removed first (`IS` matches NULL).
fn upsert_meta(conn: &rusqlite::Connection, row: &MetaRow) -> crate::error::Result<()> {
    let Some(vendor) = row.vendor else { return Ok(()) };
    let usage = row.usage.as_ref();
    let tx = conn.unchecked_transaction()?;
    if let Some(sid) = &row.vendor_session_id {
        tx.execute(
            "DELETE FROM session_meta WHERE vendor = ?1 AND account_id IS ?2 AND vendor_session_id = ?3 AND uid <> ?4",
            rusqlite::params![
                vendor.as_str(),
                row.account_id.as_ref().map(|a| a.as_str().to_string()),
                sid,
                row.uid.as_str()
            ],
        )?;
    }
    tx.execute(
        "INSERT OR REPLACE INTO session_meta (uid, vendor, account_id, vendor_session_id, project_id,
            location_path, cwd, transcript_path, origin_last, control_mode_last, permission_mode_last,
            model_last, cli_path_last, cli_version_last, team_key, agent_id, teammate_name,
            parent_session_id, first_seen_at, started_at, ended_at, last_activity_at, last_seen_at,
            usage_day, usage_in, usage_out, usage_cached, cost_micros, updated_at, parent_uid)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19,
            ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30)",
        rusqlite::params![
            row.uid.as_str(),
            vendor.as_str(),
            row.account_id.as_ref().map(|a| a.as_str().to_string()),
            row.vendor_session_id,
            row.project_id.as_ref().map(|p| p.as_str().to_string()),
            row.location_path,
            row.cwd,
            row.transcript_path,
            enum_str(&row.origin_last),
            enum_str(&row.control_mode_last),
            enum_str(&row.permission_mode_last),
            row.model_last,
            row.cli_path_last,
            row.cli_version_last,
            row.team_key.as_ref().map(|t| t.as_str().to_string()),
            row.agent_id,
            row.teammate_name,
            row.parent_session_id,
            row.first_seen_at,
            row.started_at,
            row.ended_at,
            row.last_activity_at,
            row.last_seen_at,
            usage.map(|u| u.day.clone()),
            usage.and_then(|u| i64_of(u.input_tokens)),
            usage.and_then(|u| i64_of(u.output_tokens)),
            usage.and_then(|u| i64_of(u.cached_tokens)),
            usage.and_then(|u| i64_of(u.cost_micros)),
            crate::store::now_ms(),
            row.parent_uid.as_ref().map(|u| u.as_str().to_string()),
        ],
    )?;
    tx.commit()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// follow: S3 for every live Claude session, and the usage gate (A10.4)
// ---------------------------------------------------------------------------

/// World's evidence sink as discovery sees it, plus the follower's driver:
/// every live Claude session with a transcript path is followed for its
/// structure (`follow_structure`), and stops being followed when its process
/// is gone. The Claude version discovery reported for the session decides
/// the transcript usage gate (`transcriptUsage`, P32). Forwards everything to
/// World unchanged; never blocks (one short lock).
pub struct FollowDriver {
    inner: Arc<dyn EvidenceSink>,
    follower: Follower,
    compat: Arc<dyn CompatApi>,
    state: Mutex<FollowState>,
}

#[derive(Default)]
struct FollowState {
    /// Session key → its transcript and the version discovery reported.
    known: HashMap<String, (PathBuf, Option<String>)>,
    /// Session keys whose process is alive.
    alive: HashSet<String>,
    /// Session key → the transcript being followed.
    followed: HashMap<String, PathBuf>,
    /// Transcript → version (the usage gate's lookup).
    versions: HashMap<PathBuf, Option<String>>,
}

enum FollowChange {
    Follow(PathBuf, crate::contract::services::IdentityHint),
    Unfollow(PathBuf),
}

impl FollowDriver {
    pub fn new(inner: Arc<dyn EvidenceSink>, follower: Follower, compat: Arc<dyn CompatApi>) -> FollowDriver {
        FollowDriver {
            inner,
            follower,
            compat,
            state: Mutex::new(FollowState::default()),
        }
    }

    /// The usage gate (A10.4): the `transcriptUsage` flag of the version
    /// discovery reported for this transcript's session; unknown → no.
    pub fn usage_allowed(&self, path: &Path) -> bool {
        let version = self.state.lock().versions.get(path).cloned().flatten();
        version.is_some_and(|v| self.compat.feature(&v, TRANSCRIPT_USAGE_FLAG))
    }

    /// The transcripts followed right now (tests, diagnostics).
    pub fn followed(&self) -> Vec<PathBuf> {
        self.state.lock().followed.values().cloned().collect()
    }

    fn key_of(hint: &crate::contract::services::IdentityHint) -> Option<String> {
        if hint.vendor != Some(Vendor::Claude) {
            return None;
        }
        let account = hint.account_id.as_ref().map(|a| a.as_str().to_string()).unwrap_or_default();
        match (&hint.vendor_session_id, &hint.process) {
            (Some(sid), _) => Some(format!("{account}\0{sid}")),
            (None, Some(p)) => Some(format!("{account}\0p:{}", p.as_str())),
            (None, None) => None,
        }
    }

    fn observe(&self, update: &EvidenceUpdate) -> Vec<FollowChange> {
        let Some(key) = Self::key_of(&update.hint) else { return Vec::new() };
        let mut st = self.state.lock();
        match &update.facts {
            SourceFacts::Meta(meta) => {
                if let Some(path) = meta.transcript_path.as_ref() {
                    let version = meta
                        .claude_version
                        .clone()
                        .or_else(|| meta.binary.as_ref().and_then(|b| b.version.clone()));
                    st.known.insert(key.clone(), (PathBuf::from(path.as_str()), version));
                }
            }
            SourceFacts::Process(ProcessFacts::Alive { .. }) => {
                st.alive.insert(key.clone());
            }
            SourceFacts::Process(_) => {
                st.alive.remove(&key);
            }
            _ => return Vec::new(),
        }
        let want = st
            .alive
            .contains(&key)
            .then(|| st.known.get(&key).cloned())
            .flatten();
        let mut changes = Vec::new();
        match (st.followed.get(&key).cloned(), want) {
            (Some(old), Some((path, version))) if old == path => {
                st.versions.insert(path, version);
            }
            (old, Some((path, version))) => {
                if let Some(old) = old {
                    st.versions.remove(&old);
                    changes.push(FollowChange::Unfollow(old));
                }
                st.versions.insert(path.clone(), version);
                st.followed.insert(key, path.clone());
                changes.push(FollowChange::Follow(path, update.hint.clone()));
            }
            (Some(old), None) => {
                st.followed.remove(&key);
                st.versions.remove(&old);
                changes.push(FollowChange::Unfollow(old));
            }
            (None, None) => {}
        }
        changes
    }
}

impl EvidenceSink for FollowDriver {
    fn evidence(&self, update: EvidenceUpdate) {
        let changes = self.observe(&update);
        self.inner.evidence(update);
        for change in changes {
            match change {
                FollowChange::Follow(path, hint) => self.follower.follow_structure(&path, TranscriptFormat::Claude, hint),
                FollowChange::Unfollow(path) => self.follower.unfollow_structure(&path),
            }
        }
    }
    fn location(&self, facts: LocationFacts) {
        self.inner.location(facts);
    }
    fn source_status(&self, status: SourceStatus) {
        self.inner.source_status(status);
    }
    fn account(&self, account: crate::contract::account::Account) {
        self.inner.account(account);
    }
    fn compat(&self, list: Vec<ClaudeCompat>) {
        self.inner.compat(list);
    }
    fn integrations(&self, state: IntegrationsState) {
        self.inner.integrations(state);
    }
    fn lifecycle(&self, op: LifecycleOp) {
        self.inner.lifecycle(op);
    }
    fn notify_paused(&self, until: Option<EpochMs>) {
        self.inner.notify_paused(until);
    }
}

// ---------------------------------------------------------------------------
// notifications: settings, locale, the view fan-out
// ---------------------------------------------------------------------------

/// The resolved UI locale for notification text.
pub fn notify_locale(cfg: &crate::config::model::AppConfig) -> crate::notify::Locale {
    use crate::config::model::Locale as L;
    match cfg.locale {
        L::Ko => crate::notify::Locale::Ko,
        L::En => crate::notify::Locale::En,
        L::System => match crate::commands::app::system_locale() {
            crate::contract::ipc::Locale::En => crate::notify::Locale::En,
            _ => crate::notify::Locale::Ko,
        },
    }
}

fn apply_notify_settings(notify: &NotifyService, config: &ConfigStore) {
    let cfg = config.get();
    notify.set_settings(cfg.to_settings().notify);
    notify.set_locale(notify_locale(&cfg));
}

/// `app_view_state` reaches World's view refs (`RefSource::View` =
/// visible ∪ focused ∪ zoomed) and the accounts service's view, then
/// notifications — one fan-out instead of three command edits.
pub struct ViewFanout {
    inner: Arc<NotifyService>,
    world: Arc<World>,
    accounts: Arc<AccountsService>,
}

#[async_trait]
impl NotifyApi for ViewFanout {
    fn view_state(&self, state: ViewState) {
        let mut refs: Vec<SessionUid> = state.visible_uids.clone();
        refs.extend(state.focused_uid.clone());
        refs.extend(state.zoomed_uid.clone());
        refs.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        refs.dedup();
        self.world.set_refs(RefSource::View, refs);
        self.accounts.observe_view(state.clone());
        self.inner.view_state(state);
    }
    fn input_activity(&self) {
        self.inner.input_activity();
    }
    fn queue_changed(&self, queue: &crate::contract::attention::AttentionQueue) {
        self.inner.queue_changed(queue);
    }
    fn visibility(&self) -> VisibilityEvent {
        self.inner.visibility()
    }
    fn pause(&self, until: PauseUntil) -> Option<EpochMs> {
        self.inner.pause(until)
    }
    fn resume(&self) {
        self.inner.resume();
    }
    async fn permission_request(&self) -> bool {
        self.inner.permission_request().await
    }
    async fn test(&self) -> bool {
        self.inner.test().await
    }
}

// ---------------------------------------------------------------------------
// World's side inputs: ui state, quit records
// ---------------------------------------------------------------------------

/// One `ui_state_set` key into World (`sidebarOrder`, `sidebarFolds`,
/// `splitSet`; with or without the `ui.` prefix of `settings_kv`), read in
/// exactly its A45.2 shape (`contract::ipc::validate_ui_value`, the shapes
/// `src/store/persist.ts` sends): `sidebarOrder` = project ids,
/// `splitSet` = `{ uids, focused }` (the uids are Split's references),
/// `sidebarFolds` = `{ [nodeKey]: expanded }` (a project is folded when its
/// `p:<ProjectId>` entry is `false`). A value of another shape sets nothing.
pub fn apply_ui_key(world: &World, key: &str, value: &Value) {
    use crate::contract::ids::ProjectId;
    use crate::contract::ipc::{validate_ui_value, UiSplitSet};
    let key = key.strip_prefix("ui.").unwrap_or(key);
    if !matches!(key, "sidebarOrder" | "sidebarFolds" | "splitSet") || validate_ui_value(key, value).is_err() {
        return;
    }
    match key {
        "sidebarOrder" => {
            if let Ok(order) = serde_json::from_value::<Vec<ProjectId>>(value.clone()) {
                world.set_project_order(order);
            }
        }
        "sidebarFolds" => {
            if let Ok(folds) = serde_json::from_value::<BTreeMap<String, bool>>(value.clone()) {
                let folded = folds
                    .iter()
                    .filter(|(_, expanded)| !**expanded)
                    .filter_map(|(k, _)| k.strip_prefix("p:").and_then(|id| ProjectId::parse(id).ok()))
                    .collect();
                world.set_project_folds(folded);
            }
        }
        "splitSet" => {
            if let Ok(split) = serde_json::from_value::<UiSplitSet>(value.clone()) {
                world.set_refs(RefSource::Split, split.uids);
            }
        }
        _ => {}
    }
}

fn apply_ui_state(world: &World, ui: &BTreeMap<String, Value>) {
    for (key, value) in ui {
        apply_ui_key(world, key, value);
    }
}

/// `RefSource::QuitRecord`: every session of an unresolved quit or crash
/// record stays referenced (its row must outlive midnight).
pub async fn refresh_quit_record_refs(db: &Db, world: &World) {
    let uids = db
        .read(|conn| {
            let mut out = Vec::new();
            for record in dao::quit_record::list_open(conn, crate::store::now_ms())? {
                for item in dao::quit_record::items(conn, &record.id)? {
                    out.push(item.uid);
                }
            }
            Ok(out)
        })
        .await
        .unwrap_or_default();
    world.set_refs(
        RefSource::QuitRecord,
        uids.into_iter().filter_map(|u| SessionUid::parse(u).ok()).collect(),
    );
}

// ---------------------------------------------------------------------------
// the sidecar: account envs, orchestrator snapshots
// ---------------------------------------------------------------------------

/// The accounts service's env sink: the sidecar (`mesh.configure`), plus the
/// cache PaneHost's lead env reads.
struct AccountEnvTee {
    sidecar: Arc<SidecarManager>,
    cache: Arc<Mutex<HashMap<String, AccountEnv>>>,
}

#[async_trait]
impl AccountEnvSink for AccountEnvTee {
    async fn accounts_changed(&self, envs: Vec<AccountEnv>) {
        *self.cache.lock() = envs.iter().map(|e| (e.account_id.as_str().to_string(), e.clone())).collect();
        AccountEnvSink::accounts_changed(self.sidecar.as_ref(), envs).await;
    }
}

/// `orch.snapshot` → World's read-only orchestrator mapping (§7.3.1).
fn spawn_orchestrator_relay(sidecar: &Arc<SidecarManager>, world: &Arc<World>) -> tauri::async_runtime::JoinHandle<()> {
    let mut rx = sidecar.subscribe_orchestrator();
    let world = Arc::downgrade(world);
    tauri::async_runtime::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(snap) => {
                    let Some(world) = world.upgrade() else { return };
                    let Some(view) = snap.view.as_ref() else { continue };
                    let Ok(team) = serde_json::to_value(view) else { continue };
                    let generation = u64::try_from(snap.generation).unwrap_or(0);
                    world
                        .orchestrator_adapter()
                        .apply_snapshot(None, generation, snap.rev, &team, crate::store::now_ms());
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::debug!(target: "sidecar", skipped = n, "orchestrator snapshots lagged; the next one is complete");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    })
}

// ---------------------------------------------------------------------------
// PaneHost's World-backed hooks (A21.2) and the hosted-lead record (A11.3)
// ---------------------------------------------------------------------------

/// The lead's composed base env (§6.0): the supervisor's `compose_env` with
/// the lead's account (its `CLAUDE_CONFIG_DIR`), no spawn marker (PaneHost
/// adds its own keys, §7.4.1).
struct LeadEnv {
    world: Arc<dyn WorldApi>,
    supervisor: Arc<Supervisor>,
    base: EnvBase,
    accounts: Arc<Mutex<HashMap<String, AccountEnv>>>,
}

impl LeadEnvSource for LeadEnv {
    fn base_env(&self, lead: &SessionUid) -> Result<BTreeMap<String, String>, IpcError> {
        let session = self.world.session(lead).ok_or_else(|| IpcError::not_found("session"))?;
        let mut env = self.supervisor.compose_env(self.base.clone(), None, "");
        if let Some(account) = session.account_id.as_ref() {
            if let Some(a) = self.accounts.lock().get(account.as_str()) {
                if let (Some(key), Some(dir)) = (&a.env_key, &a.config_dir) {
                    env.insert(key.clone(), dir.as_str().to_string());
                }
            }
        }
        Ok(env)
    }
}

/// A pane member's session row (A18.1/A20.1): bound right after the spawn,
/// shown In Gilde · Terminal with its PTY.
struct WorldPaneSessions {
    world: Arc<World>,
}

impl PaneSessions for WorldPaneSessions {
    fn pane_spawned(&self, spawn_id: &str, account: Option<AccountId>, pty: &PtyId) -> Option<SessionUid> {
        let uid = self.world.bind_spawn(spawn_id, None, Vendor::Claude, account);
        self.world.set_control_detail(
            &uid,
            ControlDetail {
                pty: Some(pty.clone()),
                transport: Some(crate::contract::session::Transport::Pty),
                hosted_team: false,
            },
        );
        Some(uid)
    }
}

/// PaneHost as T10 sees it: every hosted lead that starts is noted with the
/// supervisor (discovery's `Hosted` team join, A11.3) until it exits.
pub struct HostedLeadsApi {
    inner: Arc<PaneHost>,
    supervisor: Arc<Supervisor>,
    leads: Mutex<HashMap<String, i32>>,
}

#[async_trait]
impl PaneHostApi for HostedLeadsApi {
    fn lead_env(&self, lead: &SessionUid) -> Result<crate::contract::services::LeadEnv, IpcError> {
        self.inner.lead_env(lead)
    }
    fn lead_started(&self, lead: &SessionUid, pid: i32) {
        self.leads.lock().insert(lead.as_str().to_string(), pid);
        self.supervisor.note_hosted_lead(pid, crate::store::now_ms(), None);
        self.inner.lead_started(lead, pid);
    }
    async fn lead_exited(&self, lead: &SessionUid) {
        if let Some(pid) = self.leads.lock().remove(lead.as_str()) {
            self.supervisor.drop_hosted_lead(pid);
        }
        self.inner.lead_exited(lead).await;
    }
    async fn send_to_member(&self, team: &TeamKey, member: &MemberKey, text: &str) -> Result<(), IpcError> {
        self.inner.send_to_member(team, member, text).await
    }
    async fn close_member_pane(&self, team: &TeamKey, member: &MemberKey) -> Result<(), IpcError> {
        self.inner.close_member_pane(team, member).await
    }
}

// ---------------------------------------------------------------------------
// turns: T04's mesh renderer as T08's factory
// ---------------------------------------------------------------------------

/// `TurnRendererFactory` over `render::mesh::MeshTurnRenderer` (one per turn).
pub struct MeshRenderers;

struct MeshTurn(crate::render::mesh::MeshTurnRenderer);

impl TurnRenderer for MeshTurn {
    fn user_prompt(
        &mut self,
        text: &str,
        client_msg_id: Option<crate::contract::ids::ClientMsgId>,
        at: EpochMs,
    ) -> Vec<crate::contract::services::ContentUpdate> {
        vec![self.0.user_prompt(text, client_msg_id, at)]
    }

    fn on_event(
        &mut self,
        event: &crate::contract::sidecar::TurnEvent,
        at: EpochMs,
    ) -> Vec<crate::contract::services::ContentUpdate> {
        self.0.on_event(event, at)
    }
}

impl TurnRendererFactory for MeshRenderers {
    fn for_turn(&self, turn_id: &str) -> Box<dyn TurnRenderer> {
        Box::new(MeshTurn(crate::render::mesh::MeshTurnRenderer::new(turn_id)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(uid: &SessionUid, sid: &str) -> MetaRow {
        MetaRow {
            uid: uid.clone(),
            vendor: Some(Vendor::Claude),
            account_id: None,
            vendor_session_id: Some(sid.into()),
            project_id: None,
            location_path: Some("/w".into()),
            cwd: Some("/w/sub".into()),
            transcript_path: Some("/h/.claude/projects/-w/x.jsonl".into()),
            origin_last: None,
            control_mode_last: Some(crate::contract::session::ControlMode::Terminal),
            permission_mode_last: None,
            model_last: Some("opus".into()),
            cli_path_last: None,
            cli_version_last: Some("2.1.306".into()),
            team_key: None,
            agent_id: None,
            teammate_name: None,
            parent_session_id: None,
            parent_uid: Some(SessionUid::parse("s_01890a5d-ac96-774b-bcce-b302099a8057").expect("uid")),
            first_seen_at: 1_000,
            started_at: Some(1_100),
            ended_at: None,
            last_activity_at: Some(1_200),
            last_seen_at: Some(1_300),
            usage: Some(crate::contract::session::UsageToday {
                day: "2026-10-23".into(),
                input_tokens: Some(10),
                output_tokens: Some(20),
                cached_tokens: Some(30),
                cost_micros: None,
            }),
        }
    }

    /// A18.4: `session_meta` round-trips World's rows (metadata only), a
    /// second uid for the same identity replaces the first, a row without a
    /// vendor is not persisted, and a removal sticks.
    #[tokio::test]
    async fn the_session_meta_store_round_trips_world_rows() {
        let db = Db::open_in_memory().expect("db");
        let store = DbMetaStore::open(&db).await;
        assert!(store.load().is_empty());
        let a = SessionUid::mint();
        store.upsert(&meta(&a, "s-1"));
        let mut no_vendor = meta(&SessionUid::mint(), "s-2");
        no_vendor.vendor = None;
        store.upsert(&no_vendor);
        assert!(store.sync(Duration::from_secs(10)));
        let again = DbMetaStore::open(&db).await;
        assert_eq!(again.load(), vec![meta(&a, "s-1")], "one row, every field kept");

        // The merged identity's survivor replaces the first uid.
        let b = SessionUid::mint();
        again.upsert(&meta(&b, "s-1"));
        assert!(again.sync(Duration::from_secs(10)));
        let rows = DbMetaStore::open(&db).await.load();
        assert_eq!(rows.iter().map(|r| r.uid.clone()).collect::<Vec<_>>(), vec![b.clone()]);

        again.remove(&b);
        assert!(again.sync(Duration::from_secs(10)));
        assert!(DbMetaStore::open(&db).await.load().is_empty());
    }

}

// ---------------------------------------------------------------------------
// fakes for the OS boundary (tests and the T-wiring suite)
// ---------------------------------------------------------------------------

/// The [`Platform`] of the tests: every OS-facing piece recorded, nothing
/// shown, focused, badged or opened (T-wiring-*, `tests/boot.rs`).
#[cfg(any(test, feature = "test-fixtures"))]
pub mod fakes {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use async_trait::async_trait;
    use parking_lot::Mutex;

    use crate::contract::common::Vendor;
    use crate::contract::store::{FocusTarget, VisibilityEvent};
    use crate::discovery::procs::FakeProcs;
    use crate::discovery::{AccountSpec, DiscoveryConfig};
    use crate::events::{Events, RecordingEvents};
    use crate::macos::owner::{Activation, AppActivator};
    use crate::notify::{Authorization, Notification, NotificationSink};

    use super::{DiscoveryAdjust, Platform};

    /// Every notification handed to the sink (never shown).
    #[derive(Default)]
    pub struct Notifications {
        pub delivered: Mutex<Vec<Notification>>,
    }

    #[async_trait]
    impl NotificationSink for Notifications {
        fn deliver(&self, n: &Notification) -> bool {
            self.delivered.lock().push(n.clone());
            true
        }
        fn withdraw(&self, _ids: &[String]) {}
        fn supports_withdrawal(&self) -> bool {
            true
        }
        fn authorization(&self) -> Authorization {
            Authorization::Granted
        }
        async fn request_authorization(&self) -> bool {
            true
        }
    }

    /// Dock badges, recorded.
    #[derive(Default)]
    pub struct Dock(pub Mutex<Vec<u32>>);

    impl crate::macos::dock::DockSink for Dock {
        fn set_badge(&self, count: u32) {
            self.0.lock().push(count);
        }
    }

    /// Focus requests, recorded.
    #[derive(Default)]
    pub struct Focus(pub Mutex<Vec<FocusTarget>>);

    impl crate::notify::FocusEmitter for Focus {
        fn focus_request(&self, target: FocusTarget) {
            self.0.lock().push(target);
        }
    }

    /// Visibility events, recorded.
    #[derive(Default)]
    pub struct Visibility(pub Mutex<Vec<VisibilityEvent>>);

    impl crate::macos::visibility::VisibilityEmitter for Visibility {
        fn emit(&self, event: VisibilityEvent) {
            self.0.lock().push(event);
        }
    }

    /// Activations, recorded; never activates anything.
    #[derive(Default)]
    pub struct Activator(pub Mutex<Vec<(i32, String)>>);

    impl AppActivator for Activator {
        fn activate(&self, pid: i32, bundle_id: &str) -> Activation {
            self.0.lock().push((pid, bundle_id.to_string()));
            Activation::NotRunning
        }
    }

    /// A folder picker that is always cancelled.
    pub struct NoPicker;

    #[async_trait]
    impl crate::commands::project::FolderPicker for NoPicker {
        async fn pick(&self) -> Option<PathBuf> {
            None
        }
    }

    #[async_trait]
    impl crate::accounts::FolderPicker for NoPicker {
        async fn pick_folder(&self, _start: Option<PathBuf>) -> Option<PathBuf> {
            None
        }
    }

    /// What the fakes recorded.
    pub struct Recorded {
        pub events: Arc<RecordingEvents>,
        pub notifications: Arc<Notifications>,
        pub dock: Arc<Dock>,
        pub focus: Arc<Focus>,
        pub visibility: Arc<Visibility>,
        pub activator: Arc<Activator>,
    }

    /// A [`Platform`] of fakes; `sidecar_runtime` is the only real piece (a
    /// node + bundle a test may start, never a vendor CLI).
    pub fn platform(sidecar_runtime: crate::sidecar::SidecarRuntime) -> (Platform, Recorded) {
        let events = RecordingEvents::new();
        let notifications = Arc::new(Notifications::default());
        let dock = Arc::new(Dock::default());
        let focus = Arc::new(Focus::default());
        let visibility = Arc::new(Visibility::default());
        let activator = Arc::new(Activator::default());
        let n = notifications.clone();
        let platform = Platform {
            events: Events::new(events.clone()),
            notification_sink: Box::new(move |_flag| n as Arc<dyn NotificationSink>),
            dock: dock.clone(),
            focus: focus.clone(),
            notify_clock: Arc::new(crate::notify::SystemClock),
            activator: activator.clone(),
            project_picker: Arc::new(NoPicker),
            account_picker: Arc::new(NoPicker),
            link_confirm: Arc::new(crate::ide::links::DeclineLocal),
            sidecar_runtime,
            system: crate::macos::sleep::SystemEvents::new(),
            visibility: crate::macos::visibility::VisibilityHub::new(visibility.clone()),
        };
        (
            platform,
            Recorded {
                events,
                notifications,
                dock,
                focus,
                visibility,
                activator,
            },
        )
    }

    /// Discovery over a fixture process table and the accounts of a
    /// hermetic home only (never the user's `CLAUDE_CONFIG_DIR`/`CODEX_HOME`),
    /// no FSEvents. Returns the table so a test can add processes.
    pub fn hermetic_discovery(home: &Path) -> (DiscoveryAdjust, Arc<FakeProcs>) {
        let procs = Arc::new(FakeProcs::new(unsafe_uid()));
        let source = procs.clone();
        let home = home.to_path_buf();
        let adjust: DiscoveryAdjust = Box::new(move |cfg: &mut DiscoveryConfig| {
            cfg.source = source;
            cfg.watch = false;
            cfg.accounts = vec![
                AccountSpec::new(Vendor::Claude, &home.join(".claude")),
                AccountSpec::new(Vendor::Codex, &home.join(".codex")),
            ];
            cfg.cursor_dir = Some(home.join(".cursor"));
            cfg.agy_dir = Some(home.join(".gemini/antigravity-cli"));
        });
        (adjust, procs)
    }

    /// This process's uid (the fixture processes belong to the test user).
    fn unsafe_uid() -> u32 {
        // SAFETY: `getuid` has no failure mode and touches no memory.
        unsafe { libc::getuid() }
    }
}
