//! `recovery_*` and `undo_cancel` — DESIGN §3 `monitor / recovery`, M1-PLAN T11.
//!
//! Three thin commands over [`crate::supervise::reaper`]:
//!
//!   * `recovery_pending` — what the boot sweep could not decide on its own.
//!     The Recovered Sessions sheet renders it and **does not close without a
//!     decision** (DESIGN §8-5), so an empty list is the signal that there is
//!     nothing to show, not an error;
//!   * `recovery_resolve` — the user's answers, one [`KillOutcome`] each. The
//!     answer is never a bare "ok": a *Terminate* comes back with the pids that
//!     were signalled, the ones that were spared and why, and the survivors;
//!   * `undo_cancel` — cancels an automatic termination inside its 10 second
//!     window. `false` means "too late or unknown token", which the UI renders
//!     as a spent undo rather than as a failure.
//!
//! The sweep itself is not a command. It runs before the window exists, which is
//! the only way the sheet can be in front of the user *before* anything reads a
//! `proc_registry` row.

use std::sync::Arc;

use tauri::State;

use crate::error::CmdResult;
use crate::supervise::kill::KillOutcome;
use crate::supervise::reaper::{self, ReconcileLoop, RecoveredProc, RecoveryDecision};
use crate::supervise::registry::Supervisor;

/// Managed state: the supervisor that owns this run's `app_instance` row.
pub type SupervisorState = Arc<Supervisor>;

/// Managed state: the periodic reconcile. Held only so that it keeps running —
/// dropping the handle aborts the ticker.
pub type ReconcileState = Arc<ReconcileLoop>;

/// Rows the startup sweep left for the user to decide on.
///
/// Recomputed against liveness on every call: a process that died between the
/// sweep and the sheet being rendered is simply gone from the list, because
/// offering to terminate something that no longer exists is a lie.
#[tauri::command]
pub async fn recovery_pending() -> CmdResult<Vec<RecoveredProc>> {
    Ok(reaper::pending())
}

/// Apply the sheet's decisions, in the order given.
#[tauri::command]
pub async fn recovery_resolve(
    supervisor: State<'_, SupervisorState>,
    decisions: Vec<RecoveryDecision>,
) -> CmdResult<Vec<KillOutcome>> {
    Ok(reaper::resolve(supervisor.inner().as_ref(), &decisions).await)
}

/// Cancel a scheduled kill inside its undo window.
#[tauri::command]
pub async fn undo_cancel(token: String) -> CmdResult<bool> {
    Ok(reaper::undo_cancel(&token))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_unknown_undo_token_is_false_and_not_an_error() {
        assert!(!undo_cancel("no-such-token".to_string())
            .await
            .expect("command result"));
    }

    #[tokio::test]
    async fn pending_is_empty_before_a_sweep_has_run() {
        reaper::clear_pending();
        assert!(recovery_pending().await.expect("command result").is_empty());
    }
}
