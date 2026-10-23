//! PaneHost: the tmux server emulation for hosted leads (contract §7.4, §4.6).
//!
//! Owner: T07. The `mod` lines below are pre-declared by T01 and frozen
//! after wave 0 (contract §15.1, F-18): a new module is a contract change.

pub mod auth;
pub mod exits;
pub mod lead_env;
pub mod listener;
pub mod panes;
pub mod proto;
pub mod server;
pub mod subcommands;
pub mod sweep;

pub use lead_env::{apply as apply_lead_env, install_shim, prune_shims};
pub use listener::{HostProblem, Reconcile, RECONCILE_EVERY};
pub use panes::{MemberPane, Purpose};
pub use server::{
    LeadEnvSource, LeadFate, LeadPaneTable, LeadPhase, NoPaneSessions, PaneCommand, PaneHost, PaneHostConfig,
    PaneHostDeps, PaneKind, PaneSessions, PaneState, PaneTableSink, PaneView, RemainOnExit, UnknownReason,
    WorldLeadFate, DETAILS_TEAMMATE_BINARY_DIFFERS,
};
