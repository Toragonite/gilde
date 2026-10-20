//! Session status inference (DESIGN §6-4, M1-PLAN T16).
//!
//! DECISIONS M1-4 asks for "세션 상태". A green dot meaning "the process has not
//! died yet" is not that. This module answers the question the user actually
//! has — *is it working, is it waiting for me, or is it stuck* — and, because
//! every way of answering it is fallible, it publishes the reasoning alongside
//! the answer.
//!
//! | file | what it owns |
//! |---|---|
//! | [`engine`] | the state machine, the per-session state, the 500 ms tick |
//! | [`evidence`] | the confidence table, the timings, the verdict type |
//!
//! The fourth piece lives next door in [`crate::pty::ansi`]: the `vte` tail that
//! turns a terminal's escape soup back into the handful of lines a human would
//! have read.
//!
//! ## The contract with the UI
//!
//! Every `gilde://session/status` carries `status`, `confidence` and a **non-empty**
//! `evidence` vector. The Inspector renders that vector verbatim: `matched (esc
//! to interrupt) — 시그니처 편집`. When confidence drops below
//! [`evidence::LOW_CONFIDENCE`] the pill goes hollow and dashed. Nothing here is
//! allowed to look more certain than it is, because the weakest of the three
//! signals is a regex against a vendor's spinner text and that text will change.

pub mod engine;
pub mod evidence;

use std::sync::Arc;

use tauri::State;

pub use engine::{SignatureSource, StaticSignatures, StatusEngine};
pub use evidence::{
    is_low_confidence, node_status_of, CpuLatch, ForegroundSample, Verdict, CPU_BUSY_PCT,
    LOW_CONFIDENCE, PROMPT_GATE_CONFIDENCE,
};

use crate::error::CmdResult;

/// Managed-state alias, so every consumer asks for the same thing.
pub type StatusState = Arc<StatusEngine>;

/// Where the engine gets the pty's **foreground** process group from.
///
/// DESIGN §6-4 signal 2 is `tcgetpgrp(master_fd)` — which
/// `portable-pty` exposes as `MasterPty::process_group_leader()` (PINNED §2) —
/// and that is a property of a live pty, not of the engine. The trait keeps the
/// dependency pointing one way (`status` → `pty`, never back) and lets the
/// engine hold a `Weak`, so a closed session's master is dropped on time.
pub trait ForegroundSource: Send + Sync {
    /// `None` when the pty is gone or has no foreground group.
    fn foreground_pgid(&self) -> Option<i32>;
}

impl ForegroundSource for crate::pty::PtySession {
    fn foreground_pgid(&self) -> Option<i32> {
        crate::pty::PtySession::foreground_pgid(self)
    }
}

/// The tab was looked at, so the bell has been answered (DESIGN §6-4).
///
/// A BEL raises a per-session attention flag that is deliberately sticky: the
/// point of the signal is that the user was not looking. This is the only thing
/// that lowers it, and the frontend calls it when a terminal tab takes focus.
/// Returns whether a flag was actually cleared, so a caller can avoid a
/// pointless re-render.
#[tauri::command]
pub async fn session_attention_clear(
    id: String,
    status: State<'_, StatusState>,
) -> CmdResult<bool> {
    Ok(status.clear_attention(&id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_engine_default_is_usable_without_any_wiring() {
        let engine = StatusEngine::new();
        assert!(engine.is_empty());
        assert!(!engine.has_events());
        engine.register("s", "shell", 400, crate::store::now_ms());
        // No bus, no db, no ticker: still answers, still with evidence, and
        // still with the bundled patterns behind it.
        let report = engine.report("s");
        assert!(!report.evidence.is_empty());
    }
}
