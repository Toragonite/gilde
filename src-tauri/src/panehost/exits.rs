//! Remain-on-exit and pane exits (§7.4.6).
//!
//! Owner: T07.
//!
//! | Exit | `remain-on-exit` | Pane |
//! |---|---|---|
//! | code 0 | `failed` / `off` / unset | leaves `list-panes`; the tile closes after 1 s |
//! | non-zero, or a signal Gilde did not send | `failed` or `on` | **kept**: "Crashed · exit {code}" with its last output and `[Close tile]` |
//! | any | `on` | kept |
//! | non-zero / signal | `off` / unset | leaves `list-panes` |
//! | any, after Gilde signalled it (kill-pane, Close pane, lead exit) | any | already `Killed`; only the exit is recorded |
//!
//! The member becomes `Gone{ProcessExited}` (World, from the exit evidence);
//! the lead is not told [R5 §10]. Exits are matched to the pane's current
//! process by epoch: the exit of a process that a `respawn-pane -k` replaced
//! is not the pane's.

use std::sync::Weak;

use crate::contract::activity::ExitInfo;
use crate::contract::common::EpochMs;
use crate::contract::ids::{PtyId, SessionUid};
use crate::contract::services::PtyExitSink;
use crate::panehost::server::{LeadPhase, PaneHost, PaneState, RemainOnExit, Server};

/// Whether an exit Gilde did not cause keeps the pane (table above).
pub fn kept(remain: Option<RemainOnExit>, code: Option<i32>, signal: Option<i32>) -> bool {
    let clean = code == Some(0) && signal.is_none();
    match remain {
        Some(RemainOnExit::On) => true,
        Some(RemainOnExit::Failed) => !clean,
        Some(RemainOnExit::Off) | None => false,
    }
}

/// Apply an exit to a pane of `server` that is Running (or whose spawn just
/// returned). Returns whether the pane stays listed.
pub fn apply_exit(server: &mut Server, pane: u32, code: Option<i32>, signal: Option<i32>, at: EpochMs) -> bool {
    let Some(p) = server.pane_mut(pane) else { return false };
    if !matches!(p.state, PaneState::Running { .. }) {
        return p.state.listed();
    }
    let keep = !p.signalled && kept(p.remain_on_exit, code, signal);
    p.state = PaneState::Exited { code, signal, at, kept: keep };
    if !keep {
        server.unlist(pane);
    }
    keep
}

/// The `PtyExitSink` of one pane process (one per spawn epoch).
pub struct PaneExitSink {
    host: Weak<PaneHost>,
    lead: SessionUid,
    pane: u32,
    epoch: u64,
}

impl PaneExitSink {
    pub fn new(host: Weak<PaneHost>, lead: SessionUid, pane: u32, epoch: u64) -> PaneExitSink {
        PaneExitSink { host, lead, pane, epoch }
    }
}

impl PtyExitSink for PaneExitSink {
    fn exited(&self, pty: &PtyId, exit: ExitInfo, at: EpochMs) {
        if let Some(host) = self.host.upgrade() {
            host.pane_exited(&self.lead, self.pane, self.epoch, pty, exit, at);
        }
    }
}

impl PaneHost {
    /// A pane process exited (called from the PTY layer, any thread).
    pub(crate) fn pane_exited(&self, lead: &SessionUid, pane: u32, epoch: u64, pty: &PtyId, exit: ExitInfo, at: EpochMs) {
        // (spawn id, pid, spawned at, signalled) for a teammate's evidence.
        let mut evidence: Option<(String, i32, EpochMs, bool)> = None;
        let mut changed = false;
        {
            let mut leads = self.leads.lock();
            let Some(server) = leads.get_mut(lead).and_then(|l| l.server.as_mut()) else { return };
            let Some(p) = server.pane_mut(pane) else { return };
            if p.epoch != epoch {
                // A process a respawn replaced: not this pane's any more.
                return;
            }
            let teammate = matches!(p.kind, crate::panehost::server::PaneKind::Teammate { .. });
            let mine = p.last_proc.as_ref().is_some_and(|r| &r.pty == pty);
            match &p.state {
                PaneState::Spawning { .. } => {
                    p.early_exit = Some((exit.code, exit.signal, at));
                }
                PaneState::Running { pty: running, .. } if running == pty => {
                    if teammate {
                        if let Some(r) = &p.last_proc {
                            evidence = Some((r.spawn_id.clone(), r.pid, r.since, p.signalled));
                        }
                    }
                    let keep = apply_exit(server, pane, exit.code, exit.signal, at);
                    tracing::info!(target: "panehost", pane, code = ?exit.code, signal = ?exit.signal, kept = keep, "pane exited");
                    changed = true;
                }
                PaneState::Killed { .. } if mine && teammate => {
                    if let Some(r) = &p.last_proc {
                        evidence = Some((r.spawn_id.clone(), r.pid, r.since, true));
                    }
                }
                _ => {}
            }
        }
        if let Some((spawn_id, pid, since, signalled)) = evidence {
            self.pane_exited_evidence(&spawn_id, pid, since, exit.code, exit.signal, at, signalled);
        }
        if changed {
            self.emit_table(lead);
            self.end_if_backgrounded_and_empty(lead);
        }
    }

    /// Whether `lead` is backgrounded (for tests and the table).
    pub fn lead_phase(&self, lead: &SessionUid) -> Option<LeadPhase> {
        self.leads.lock().get(lead).map(|l| l.phase)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remain_on_exit_rules() {
        // failed: crashes stay, clean exits go.
        assert!(kept(Some(RemainOnExit::Failed), Some(1), None));
        assert!(kept(Some(RemainOnExit::Failed), None, Some(11)), "a signal Gilde did not send");
        assert!(!kept(Some(RemainOnExit::Failed), Some(0), None));
        // on: everything stays.
        assert!(kept(Some(RemainOnExit::On), Some(0), None));
        // off / unset: nothing stays.
        assert!(!kept(Some(RemainOnExit::Off), Some(1), None));
        assert!(!kept(None, Some(1), None));
        assert!(!kept(None, None, Some(9)));
    }

    #[test]
    fn a_signalled_pane_is_never_kept() {
        let mut s = Server::new();
        s.new_session(1).unwrap();
        let pty = PtyId::mint();
        let p = s.pane_mut(0).unwrap();
        p.remain_on_exit = Some(RemainOnExit::On);
        p.signalled = true;
        p.state = PaneState::Running { pty, pid: 42, spawn_id: "x".into(), since: 1 };
        assert!(!apply_exit(&mut s, 0, None, Some(15), 2));
        assert!(!s.has_session(), "the last pane left: window and session are gone");
    }

    #[test]
    fn a_crash_stays_listed_with_its_code() {
        let mut s = Server::new();
        s.new_session(1).unwrap();
        let p = s.pane_mut(0).unwrap();
        p.remain_on_exit = Some(RemainOnExit::Failed);
        p.state = PaneState::Running { pty: PtyId::mint(), pid: 42, spawn_id: "x".into(), since: 1 };
        assert!(apply_exit(&mut s, 0, Some(3), None, 2));
        assert_eq!(s.pane(0).unwrap().state, PaneState::Exited { code: Some(3), signal: None, at: 2, kept: true });
        assert!(s.has_session());
    }
}
