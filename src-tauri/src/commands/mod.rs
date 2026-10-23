//! Tauri command surface (contract §4).
//!
//! Every command is `async fn <name>(req: Req<<Name>Req>, [channel,] state…)
//! -> Result<<Name>Res, IpcError>`; the registered set is exactly the manifest
//! (`contract/ipc_manifest.rs`), listed in the generated [`registry`]. A command
//! module belongs to its build task (contract §15.2); this file's `mod` lines
//! are pre-declared by T01 and frozen after wave 0 [F-18].
//!
//! ```text
//! accounts.rs      T08  account_*, quota_refresh
//! app.rs           T09  app_bootstrap, app_mark, agents_rescan, app_diagnostics
//! changes.rs       T11  changes_get, git_*
//! convo.rs         T04  convo_*
//! health.rs        T09  health_*, undo_cancel
//! history.rs       T03  history_*
//! ide.rs           T11  ide_*, link_open
//! interaction.rs   T10  interaction_*, screen_prompt_key
//! notify.rs        T12  app_input_activity, app_view_state, queue_later, notify_*
//! orchestrator.rs  T08  (presence-gated orchestrator layer; no command in v1)
//! owner.rs         T12  owner_focus
//! project.rs       T11  project_pick
//! pty.rs           T06  pty_*
//! quit.rs          T09  app_quit*, continue_all, quit_record_dismiss
//! registry.rs      T01  generated generate_handler! list
//! repo_identity.rs T03  (no command in v1; file-based fast path, §5.6)
//! session.rs       T10  newsession_options, session_*
//! settings.rs      T09  settings_*
//! store.rs         T09  store_subscribe, ui_state_set
//! team.rs          T07  team_*
//! turn.rs          T10  turn_*
//! worktree.rs      T11  worktree_discard*
//! ```
//!
//! The legacy modules `agents`, `dispatch`, `graph`, `monitor` and `recovery`
//! were deleted by T09 in wave 1 (§2.2.1, A7.9); the T08-deleted `mesh`,
//! `cco_team`, `cco_workers` are no longer part of the tree.

pub mod accounts; // T08
pub mod app; // T09
pub mod changes; // T11
pub mod convo; // T04
pub mod health; // T09
pub mod history; // T03
pub mod ide; // T11
pub mod interaction; // T10
pub mod notify; // T12
pub mod orchestrator; // T08
pub mod owner; // T12
pub mod project; // T11
pub mod pty; // T06
pub mod quit; // T09
pub mod registry; // T01 (generated)
pub mod repo_identity; // T03
pub mod session; // T10
pub mod settings; // T09
pub mod store; // T09
pub mod team; // T07
pub mod turn; // T10
pub mod worktree; // T11
