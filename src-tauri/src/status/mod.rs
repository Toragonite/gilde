//! Source S5: the PTY status engine (contract §3.5.1, §6.2.2, §6.2.4, §6.2.6).
//!
//! | file | what it owns |
//! |---|---|
//! | [`engine`] | the per-session state machine, prompt tracking, the quiet ticker |
//! | [`evidence`] | the confidence and timing tables, the screen-prompt reader, the card builder |
//!
//! The grid it reads lives next door in [`crate::pty::ansi`].
//!
//! Every status the engine publishes carries its confidence; nothing here is
//! allowed to look more certain than it is. Its output reaches World as
//! `SourceFacts::Pty(PtyFacts)` through the PTY manager.

pub mod engine;
pub mod evidence;

use std::sync::Arc;

pub use engine::{ScreenView, SignatureSource, StaticSignatures, StatusEngine, StatusListener};
pub use evidence::{
    is_low_confidence, parse_screen_prompt, prompt_rev, screen_card, CpuLatch, ForegroundSample,
    PromptGrammar, Verdict, CPU_BUSY_PCT, LOW_CONFIDENCE, PROMPT_GATE_CONFIDENCE,
};

/// Managed-state alias, so every consumer asks for the same thing.
pub type StatusState = Arc<StatusEngine>;

/// Where the engine gets the pty's **foreground** process group from
/// (`tcgetpgrp` on the master). The trait keeps the dependency pointing one
/// way (`status` → `pty`, never back) and lets the engine hold a `Weak`.
pub trait ForegroundSource: Send + Sync {
    /// `None` when the pty is gone or has no foreground group.
    fn foreground_pgid(&self) -> Option<i32>;
}

impl ForegroundSource for crate::pty::PtySession {
    fn foreground_pgid(&self) -> Option<i32> {
        crate::pty::PtySession::foreground_pgid(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_engine_default_is_usable_without_any_wiring() {
        let engine = StatusEngine::new();
        assert!(engine.is_empty());
        engine.register("s", "shell", 400, crate::store::now_ms());
        let report = engine.report("s");
        assert!(!report.evidence.is_empty());
        assert!(engine.signature_set().is_some(), "the bundled patterns are there");
    }
}
