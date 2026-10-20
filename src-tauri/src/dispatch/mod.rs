//! The headless lane: one prompt fanned out to N agents, each in its own
//! worktree (DESIGN §3 `dispatch`, §5, §9-7, §10-2; M1-PLAN T22).
//!
//! Two files, and the split is the point:
//!
//! * [`model`] — the wire shapes plus every decision that needs no clock, no
//!   database and no sidecar: how a fan-out's runs fold into one task status,
//!   what a worktree is called, where the three artifacts live, whether a
//!   result spills to disk. All of it unit-tested in place.
//! * [`engine`] — [`DispatchEngine`], which owns every effect: worktrees,
//!   `mesh.run`/`mesh.stream`, the artifact files, the `dispatch_*` rows, the
//!   graph nodes, `gilde://dispatch/event`, and cancellation.
//!
//! The commands are in `crate::commands::dispatch`, thin over this.

pub mod engine;
pub mod model;

pub use engine::DispatchEngine;
pub use model::{
    cap_text, cost_usd, fold_task_status, usage_json, worktree_task_name, DispatchRun,
    DispatchScope, DispatchSpec, DispatchTarget, DispatchTask, DispatchTaskDetail, RouteDecision,
    RunArtifacts, RunOutcome, RunPhase, TaskStatus, WorktreeStrategy, AUTO_VENDOR,
    RESULT_SPILL_BYTES, RUN_TEXT_WIRE_CAP, SPILL_PREFIX, TEXT_COALESCE_MS,
};
