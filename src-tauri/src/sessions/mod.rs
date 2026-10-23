//! The sessions manager (contract §6.0, §6.5): one actor per Gilde-owned session.
//!
//! Owner: T10. The `mod` lines below are pre-declared by T01 and frozen
//! after wave 0 (contract §15.1, F-18): a new module is a contract change.
//!
//! | file | what it owns |
//! |---|---|
//! | [`manager`] | [`SessionsManager`], its ports ([`SessionDeps`]), idempotency, rate classes, the [`SessionActorApi`](crate::contract::services::SessionActorApi) routing |
//! | [`actor`] | the per-session actor, the binding evidence sink, T05's `ConvoHooks` |
//! | [`start`] | start, retry-as-terminal, sign-in, the one launch path by control mode; production transports, binaries, env |
//! | [`continuation`] | continue / copy / attach, the identity lock + liveness re-check, Continue all; production liveness and quit items |
//! | [`stop`] | session stop (own and watched), the per-session command routing |
//! | [`options`] | `newsession_options`, the §5.8 account rule; production accounts and `project_prefs` |
//!
//! ## Wiring (T09b, A10.6)
//!
//! The composition root builds the manager once and binds it:
//!
//! ```text
//! let transports = LiveTransports {
//!     convo: ConvoDeps { supervisor, compat (CompatApi + CompatSink), evidence: world.sink(), content: hub,
//!                        hooks: Arc::new(NoHooks) /* replaced per session */, clock, patterns, timing },
//!     pty: pty_manager.clone(),                 // Arc<PtyManager> (T06)
//!     supervisor: supervisor_api.clone(),       // Arc<dyn SupervisorApi> (redacted argv)
//!     turns: TurnDeps::new(sidecar, world.sink(), hub, mesh_renderer),
//!     sidecar: Some(sidecar),
//! };
//! let sessions = SessionsManager::new(SessionDeps {
//!     world: world.clone(),                     // Arc<dyn WorldApi>
//!     binder: world.clone(),                    // Arc<World> as Arc<dyn SessionBinder> (A18.1; impl lands with T02)
//!     supervisor: supervisor_api,
//!     transports: Arc::new(transports),
//!     binaries: Arc::new(AgentBinaries::new(agents, compat.clone())),
//!     accounts: Arc::new(ServiceAccounts::new(accounts_service.clone(), world.clone())),
//!     env: Arc::new(SupervisorEnv::new(supervisor, EnvBase::from_process_env(resolved_path))),
//!     compat,
//!     liveness: Arc::new(ScanLiveness::new(claude_config_dirs, Arc::new(Libproc::new()))),
//!     prefs: Arc::new(DbPrefs::new(db.clone(), Arc::new(now_ms))),   // MemoryPrefs in memory-only mode
//!     quit_items: Arc::new(DbQuitItems::new(db)),
//!     git: Some(git),                           // Arc<dyn GitApi> (T11)
//!     panehost: Some(host),                     // Arc<dyn PaneHostApi> (T07)
//!     sidecar: Some(sidecar),
//!     clock: Arc::new(now_ms),
//!     timing: SessionTiming::default(),
//! });
//! services.bind_sessions(sessions.clone());     // the commands, T09's continue_all and quit sweep
//! accounts_service.bind_sessions(sessions);     // account_sign_in (T08)
//! ```

pub mod actor;
pub mod continuation;
pub mod manager;
pub mod options;
pub mod start;
pub mod stop;

pub use actor::{ActorDriver, ActorHandle, SessionHooks, SessionSink, TerminalDriver, TurnBasedDriver};
pub use continuation::{DbQuitItems, ScanLiveness};
pub use manager::{
    AccountsPort, Binaries, ControlFacts, ConvoWiring, DriverDetails, EnvSource, LiveProcess, Liveness, PrefsStore,
    ProjectPrefs, QuitItems, ResolvedVendor, SessionBinder, SessionDeps, SessionTiming, SessionsManager,
    TerminalStarted, Transports,
};
pub use options::{DbPrefs, MemoryPrefs, ServiceAccounts};
pub use start::{AgentBinaries, Launch, LaunchRequest, LiveTransports, SupervisorEnv};
