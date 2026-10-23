//! The PTY lane — Terminal mode (contract §6.2).
//!
//! One agent CLI, one pseudo-terminal, one ring buffer, one attached view.
//!
//! ## The rule this module exists to keep
//!
//! **Bytes are never decoded into a `String` for delivery.** The pipeline is
//! `read → ring → base64 → xterm.js`, and xterm decodes exactly once.
//! `scripts/lint-forbidden.sh` fails the build if either of the two lossy
//! conversions appears anywhere under this directory. The text grid in
//! [`ansi`] is a separate, never-delivered view: `vte` decodes it `char` by
//! `char`.
//!
//! ## Layout
//!
//! | file | what it owns |
//! |---|---|
//! | [`ring`] | the scrollback ring and the `GLDP` snapshot format |
//! | [`ansi`] | the xterm-style text grid (alternate screen, pending wrap, queries, titles) |
//! | [`session`] | one live PTY: reader, coalescer, exit watcher, the one view, the write lock |
//! | [`manager`] | the PTY table and [`PtyApi`] (spawn via `SupervisorApi`, screen actions) |
//! | [`dump`] | the opt-in `GILDE_PTY_DUMP` recorder (`probe-log` builds only) |
//!
//! This file holds [`PtyDriver`], the [`SessionDriver`] of a Terminal-mode
//! session (§6.0): readiness and the first prompt (§6.2.2), `turn_send` as a
//! bracketed paste (§6.2.3), screen-prompt actions (§6.2.4), stop.

pub mod ansi;
pub mod dump;
pub mod manager;
pub mod ring;
pub mod session;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::contract::activity::{PtyStatus, StopCause};
use crate::contract::common::PermissionMode;
use crate::contract::ids::{ClientMsgId, PtyId, RequestId};
use crate::contract::interaction::Decision;
use crate::contract::ipc::{
    AppliesFrom, InterruptOutcome, ScreenKeyOutcome, ScreenPromptAction, SendDelivery,
};
use crate::contract::services::{
    AnswerOutcome, OutgoingMessage, PtyApi, SessionDriver, SignalPlan, StopOutcome,
};
use crate::contract::session::ControlMode;
use crate::contract::IpcError;

pub use ansi::{AnsiTail, TermQuery};
pub use crate::contract::content::PtyChunk;
pub use manager::{classify, PtyManager, PtyOptions};
pub use ring::{
    clamp_ring_bytes, parse_snapshot_header, snapshot_header, Ring, SNAPSHOT_HEADER_LEN,
    SNAPSHOT_MAGIC, SNAPSHOT_VERSION,
};
pub use session::{paste_bytes, sanitize_size, ChunkSink, PtySession};

/// The Starting timeout (§6.2.2): 30 s, suspended while a setup or screen
/// prompt is on screen.
pub const READY_TIMEOUT: Duration = Duration::from_secs(30);

/// The grace between SIGHUP and the fenced SIGTERM when a Terminal session is
/// stopped (§9.8, C5). Never SIGKILL.
pub const STOP_GRACE_MS: u32 = 2_000;

/// How readiness ended (§6.2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadyOutcome {
    /// Idle at the vendor's readiness confidence.
    Ready,
    /// 30 s of Starting time without a prompt on screen → Error·Didn't start.
    Timeout,
    /// The child exited first.
    Exited,
    /// The PTY is not (or no longer) in the table.
    Gone,
}

/// The [`SessionDriver`] of a Terminal-mode session.
pub struct PtyDriver {
    pty: Arc<PtyManager>,
    id: PtyId,
}

impl std::fmt::Debug for PtyDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PtyDriver").field("pty", &self.id).finish()
    }
}

impl PtyDriver {
    pub fn new(pty: Arc<PtyManager>, id: PtyId) -> PtyDriver {
        PtyDriver { pty, id }
    }

    pub fn pty_id(&self) -> &PtyId {
        &self.id
    }

    /// Wait for readiness (§6.2.2): the S5 engine reports `Idle` at the
    /// vendor's threshold (0.6 with a verified ready signature, else 0.5).
    /// The Starting budget does not run while a screen or setup prompt is on
    /// screen (folder trust, onboarding) [F-13]. (A claude registry `idle` for
    /// the pid is the other ready signal; the session manager may stop
    /// waiting on it first.)
    pub async fn wait_ready(pty: &PtyManager, id: &PtyId, timeout: Duration) -> ReadyOutcome {
        let (Some(mut rx), Ok(session)) = (pty.subscribe(id), pty.get(id)) else {
            return ReadyOutcome::Gone;
        };
        let threshold = pty.ready_threshold(id);
        let mut remaining = timeout;
        let mut paused = false;
        let mut mark = tokio::time::Instant::now();
        loop {
            if !paused {
                remaining = remaining.saturating_sub(mark.elapsed());
            }
            mark = tokio::time::Instant::now();
            if session.has_exited() {
                return ReadyOutcome::Exited;
            }
            let facts = rx.borrow_and_update().clone();
            if let Some(facts) = &facts {
                if facts.status == PtyStatus::Idle && facts.confidence + f32::EPSILON >= threshold {
                    return ReadyOutcome::Ready;
                }
                paused = facts.prompt.is_some();
            }
            if remaining.is_zero() && !paused {
                return ReadyOutcome::Timeout;
            }
            let budget = if paused { Duration::from_secs(3_600) } else { remaining };
            tokio::select! {
                changed = rx.changed() => {
                    if changed.is_err() {
                        return if session.has_exited() { ReadyOutcome::Exited } else { ReadyOutcome::Gone };
                    }
                }
                exited = session.wait_exit(budget) => {
                    if exited {
                        return ReadyOutcome::Exited;
                    }
                }
            }
        }
    }

    /// "Typed after ready" (§6.2.2): wait for readiness, then bracketed-paste
    /// `text` + CR — never before, so the first prompt cannot be dropped by a
    /// TUI that is still starting [R1 §3 Bug].
    pub fn deliver_first_prompt(
        &self,
        text: String,
        timeout: Duration,
    ) -> tokio::task::JoinHandle<Result<ReadyOutcome, IpcError>> {
        let pty = self.pty.clone();
        let id = self.id.clone();
        tokio::spawn(async move {
            let outcome = PtyDriver::wait_ready(&pty, &id, timeout).await;
            if outcome == ReadyOutcome::Ready {
                pty.paste(&id, &text, true).await?;
            }
            Ok(outcome)
        })
    }
}

#[async_trait]
impl SessionDriver for PtyDriver {
    fn mode(&self) -> ControlMode {
        ControlMode::Terminal
    }

    /// `turn_send` → bracketed paste + CR through the ungated write path
    /// (§6.2.3); refused while a screen prompt is pending.
    async fn send(&mut self, msg: OutgoingMessage) -> Result<SendDelivery, IpcError> {
        if self.pty.screen_prompt(&self.id).is_some() {
            return Err(IpcError::refused("screenPromptPending"));
        }
        self.pty.paste(&self.id, &msg.text, true).await?;
        Ok(SendDelivery::Sent)
    }

    /// A terminal holds no queue.
    async fn cancel_queued(&mut self, _id: &ClientMsgId) -> Result<bool, IpcError> {
        Err(IpcError::not_found("message"))
    }

    /// Structured cards never exist on a PTY (screen prompts use
    /// `screen_key`).
    async fn answer(&mut self, _req: &RequestId, _d: Decision) -> Result<AnswerOutcome, IpcError> {
        Err(IpcError::not_found("request"))
    }

    async fn screen_key(
        &mut self,
        req: &RequestId,
        rev: &str,
        key: ScreenPromptAction,
    ) -> Result<ScreenKeyOutcome, IpcError> {
        if req.as_str() != format!("sp_{rev}") {
            return Err(IpcError::stale_prompt());
        }
        self.pty.screen_action(&self.id, rev, &key).await
    }

    /// Terminal: `Esc` (§4.2.3).
    async fn interrupt(&mut self, _discard_queued: bool) -> Result<InterruptOutcome, IpcError> {
        self.pty.write(&self.id, b"\x1b").await?;
        Ok(InterruptOutcome::Sent)
    }

    async fn force_stop(&mut self) -> Result<bool, IpcError> {
        Err(IpcError::unsupported("forceStop"))
    }

    async fn set_model(&mut self, _m: Option<String>) -> Result<AppliesFrom, IpcError> {
        Err(IpcError::unsupported("setModel"))
    }

    async fn set_permission_mode(&mut self, _m: PermissionMode) -> Result<(), IpcError> {
        Err(IpcError::unsupported("setPermissionMode"))
    }

    /// SIGHUP, then a fenced SIGTERM after the grace — never SIGKILL (C5).
    async fn stop(&mut self, _why: StopCause) -> Result<StopOutcome, IpcError> {
        let outcome = self
            .pty
            .close(&self.id, SignalPlan::HupThenTerm { grace_ms: STOP_GRACE_MS })
            .await?;
        Ok(StopOutcome {
            stopped: outcome.exited,
            survivors: u32::from(outcome.survivor),
        })
    }
}
