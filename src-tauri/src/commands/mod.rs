//! Tauri command surface.
//!
//! Commands are thin (DESIGN §3): validate, call a service, return a small ack.
//! **They never return large state** — state flows to the UI as `gilde://` events.
//! Every command returns `Result<T, GildeErrorPayload>` so the frontend gets
//! `{ code, key, params }` instead of prose (§9-9).
//!
//! ## Module registry
//!
//! Each command module is declared here and its handlers are listed in
//! `lib.rs::run()`'s `generate_handler!`. The remaining modules from DESIGN §2
//! land with their owning task:
//!
//! ```text
//! agents.rs    T09    agents_detect / agents_path_report / agents_launch_templates
//! pty.rs       T15    session_open / write / resize / signal / snapshot / close
//! worktree.rs  T13    project_* / worktree_*
//! dispatch.rs  T22    dispatch_*
//! mesh.rs      T19    mesh_*
//! monitor.rs   T12    procmon_* / budget_get
//! recovery.rs  T11    recovery_*
//! accounts.rs  T03    accounts_list / accounts_import_cco
//! graph.rs     T04    graph_snapshot / graph_focus
//! ```

pub mod accounts; // T03  app_settings_get/set, accounts_list, accounts_import_cco
pub mod agents; // T09  agents_detect / path_report / launch_templates / reload_signatures
pub mod app;
pub mod cco_team; // T8 (team contract v2 §8.2)  cco_team_list / cco_team_get — read-only
pub mod cco_workers; // plan #3c  cco_workers — the engine's worker accounts, read-only
pub mod dispatch; // T22  dispatch_create / list / get / cancel / resume / route / open_terminal
pub mod graph; // T04  graph_snapshot / graph_focus
pub mod mesh; // T19  mesh_adapters / models / quota / auth, sidecar_status / restart
pub mod monitor; // T12  procmon_scan / cwd / registry / kill / keepalive, budget_get
pub mod project;
pub mod pty; // T15  session_open / attach / snapshot / write / resize / signal / close / list
pub mod recovery; // T11  recovery_pending / recovery_resolve / undo_cancel
pub mod repo_identity; // Team view: worktree-aware repository identity of folders, read-only
pub mod worktree;
