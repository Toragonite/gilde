//! Orphan sweep hooks (§7.4.8) and the scan-side compat signals (§7.4.9).
//!
//! Owner: T07.
//!
//! What runs where:
//!
//! * Gilde's own pane rows left by a crashed instance are fenced and
//!   terminated by T09's boot sweep (SIGTERM after the 10 s undo window;
//!   vendor processes are never SIGKILLed). Nothing here signals anything.
//! * Real `tmux -L claude-swarm-<pid>` servers with a dead `<pid>` are
//!   classified by T09's health scan (`PaneServerOfDeadLead`, held 2 scans,
//!   stopped only from the Clean-up sheet).
//! * [`PaneHost::observe_scan`] — this module — reads each shared process
//!   scan (`ScanHub`, wired by T09b) for the hosted leads only:
//!   1. a real tmux process carrying `-L claude-swarm-<hosted lead pid>` means
//!      the shim fell back to stock external mode: the lead's table gets
//!      `outside_gilde` (`paneHosting = OutsideGilde`, ux-ia #104);
//!   2. a claude teammate process (`--parent-session-id` = a hosted lead's
//!      session id, started after the lead) that does not descend from one of
//!      the lead's pane processes, is not orphaned (`ppid != 1`, e.g. a
//!      survivor of a closed pane) and is not explained by (1), seen in two
//!      consecutive scans, is a **deviation** ("teammate process without
//!      respawn-pane"): `paneHosting → Failed` for the lead's version.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use crate::contract::common::Vendor;
use crate::contract::ids::SessionUid;
use crate::contract::services::{ProcInfo, ProcSnapshot};
use crate::panehost::proto::SWARM_SOCKET_PREFIX;
use crate::panehost::server::{LeadPhase, PaneHost};

/// Consecutive scans a teammate must be seen unexplained before it counts.
pub const STRIKES: u8 = 2;

/// Scan-to-scan memory of the sweep.
#[derive(Debug, Default)]
pub struct SweepState {
    strikes: HashMap<(SessionUid, i32), u8>,
}

/// The `<pid>` of a `-L claude-swarm-<pid>` pair anywhere in `argv[1..]`.
pub fn swarm_socket_lead(argv: &[String]) -> Option<i32> {
    argv.windows(2).find_map(|w| {
        if w[0] != "-L" {
            return None;
        }
        let digits = w[1].strip_prefix(SWARM_SOCKET_PREFIX)?;
        (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
            .then(|| digits.parse::<i32>().ok())
            .flatten()
    })
}

/// A real tmux process (by exec path, else `argv[0]`), never a shim copy.
pub fn is_real_tmux(p: &ProcInfo, shim_root: &Path) -> bool {
    let exe = p.exec_path.as_deref().or_else(|| p.argv_redacted.first().map(String::as_str));
    let Some(exe) = exe else { return false };
    let path = Path::new(exe);
    path.file_name().is_some_and(|n| n == "tmux") && !path.starts_with(shim_root)
}

/// Whether `pid` descends from any of `roots` in the scan (bounded walk).
fn descends_from(pid: i32, roots: &BTreeSet<i32>, parents: &HashMap<i32, i32>) -> bool {
    let mut cur = pid;
    for _ in 0..64 {
        if roots.contains(&cur) {
            return true;
        }
        match parents.get(&cur) {
            Some(&pp) if pp > 1 && pp != cur => cur = pp,
            _ => return false,
        }
    }
    false
}

struct HostedLead {
    uid: SessionUid,
    pid: i32,
    start: Option<i64>,
    session_id: Option<String>,
    pane_pids: BTreeSet<i32>,
    outside: bool,
}

impl PaneHost {
    /// Read one shared process scan (§7.4.8, §7.4.9). Never signals anything.
    pub fn observe_scan(&self, snap: &ProcSnapshot) {
        let mut hosted: Vec<HostedLead> = {
            let leads = self.leads.lock();
            leads
                .iter()
                .filter(|(_, l)| matches!(l.phase, LeadPhase::Running | LeadPhase::Backgrounded))
                .filter_map(|(uid, l)| {
                    Some(HostedLead {
                        uid: uid.clone(),
                        pid: l.pid?,
                        start: l.start_sec,
                        session_id: None,
                        pane_pids: l
                            .server
                            .as_ref()
                            .map(|s| s.panes().filter_map(|p| p.last_proc.as_ref().map(|r| r.pid)).collect())
                            .unwrap_or_default(),
                        outside: l.outside_gilde,
                    })
                })
                .collect()
        };
        if hosted.is_empty() {
            self.sweep.lock().strikes.clear();
            return;
        }
        for h in &mut hosted {
            h.session_id = self.deps.world.session(&h.uid).and_then(|s| s.vendor_session_id);
        }
        let parents: HashMap<i32, i32> = snap.procs.iter().map(|p| (p.pid, p.ppid)).collect();

        // (1) the shim fell back to a real tmux for a hosted lead.
        let mut newly_outside = Vec::new();
        for p in &snap.procs {
            if !is_real_tmux(p, &self.cfg.shim_root) {
                continue;
            }
            if let Some(lead_pid) = swarm_socket_lead(&p.argv_redacted) {
                if let Some(h) = hosted.iter_mut().find(|h| h.pid == lead_pid && !h.outside) {
                    h.outside = true;
                    newly_outside.push(h.uid.clone());
                }
            }
        }

        // (2) teammate processes that no respawn-pane explains.
        let mut seen: HashMap<(SessionUid, i32), u8> = HashMap::new();
        let mut deviations = Vec::new();
        {
            let previous = self.sweep.lock();
            for p in &snap.procs {
                if p.vendor != Some(Vendor::Claude) {
                    continue;
                }
                let Some(parent_sid) = p.labels.parent_session_id.as_deref() else { continue };
                let Some(h) = hosted.iter().find(|h| h.session_id.as_deref() == Some(parent_sid)) else { continue };
                if h.outside || p.ppid <= 1 {
                    continue;
                }
                if h.start.is_some_and(|s| p.start_sec < s) {
                    continue;
                }
                if descends_from(p.pid, &h.pane_pids, &parents) {
                    continue;
                }
                let key = (h.uid.clone(), p.pid);
                let n = previous.strikes.get(&key).copied().unwrap_or(0).saturating_add(1);
                if n >= STRIKES {
                    deviations.push(h.uid.clone());
                }
                seen.insert(key, n);
            }
        }
        self.sweep.lock().strikes = seen;

        if !newly_outside.is_empty() {
            {
                let mut leads = self.leads.lock();
                for uid in &newly_outside {
                    if let Some(l) = leads.get_mut(uid) {
                        l.outside_gilde = true;
                    }
                }
            }
            for uid in &newly_outside {
                tracing::warn!(target: "panehost", "a real tmux server runs for a hosted lead: teammate panes opened outside Gilde");
                self.emit_table(uid);
            }
        }
        let mut done = BTreeSet::new();
        for uid in deviations {
            if done.insert(uid.clone()) {
                self.record_deviation(&uid, "teammate process without respawn-pane".to_string());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_swarm_socket_is_found_anywhere_after_argv0() {
        assert_eq!(swarm_socket_lead(&argv(&["tmux", "-L", "claude-swarm-123", "new-session"])), Some(123));
        assert_eq!(swarm_socket_lead(&argv(&["/opt/tmux", "-f", "x", "-L", "claude-swarm-9"])), Some(9));
        assert_eq!(swarm_socket_lead(&argv(&["tmux", "-L", "default"])), None);
        assert_eq!(swarm_socket_lead(&argv(&["tmux", "-L", "claude-swarm-x"])), None);
        assert_eq!(swarm_socket_lead(&argv(&["tmux"])), None);
    }

    #[test]
    fn descent_is_bounded_and_stops_at_launchd() {
        let parents: HashMap<i32, i32> = [(10, 5), (5, 3), (3, 1), (20, 20)].into_iter().collect();
        let roots: BTreeSet<i32> = [3].into_iter().collect();
        assert!(descends_from(10, &roots, &parents));
        assert!(!descends_from(20, &roots, &parents), "a self-parent loop ends");
        assert!(!descends_from(99, &roots, &parents));
    }
}
