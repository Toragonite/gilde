//! Rust-computed bottlenecks (§3.6).
//!
//! Owner: T02. The pixel-office §2.3 rules, evaluated in Rust so Board and
//! Floor agree; the lead is exempt:
//! * a desk's queue tray (tasks Assigned to the member) holds ≥ 2 cards;
//! * the member's current task blocks ≥ 2 other tasks that are not done;
//! * a task in progress for > 3 × the median of the team's finished tasks
//!   (at least 2 finished, and at least 10 min elapsed).
//!
//! Inferred boards never have dependencies, so the second rule cannot fire
//! there (pixel-office §2.10).

use std::collections::{BTreeSet, HashMap};

use crate::contract::common::EpochMs;
use crate::contract::ids::{MemberKey, TaskKey};
use crate::contract::team::{Station, Task, TaskStatus};

/// The "3× median" rule needs at least this much elapsed time.
pub const MEDIAN_RULE_MIN_MS: EpochMs = 10 * 60 * 1000;

fn open(t: &Task) -> bool {
    !matches!(t.status, TaskStatus::Done | TaskStatus::Deleted)
}

fn median(mut v: Vec<EpochMs>) -> Option<EpochMs> {
    if v.len() < 2 {
        return None;
    }
    v.sort_unstable();
    let n = v.len();
    Some(if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2 })
}

/// The bottleneck members (sorted), and the wall time at which the median
/// rule could fire next (a recheck deadline).
pub fn bottlenecks(tasks: &[Task], lead: Option<&MemberKey>, now_wall: EpochMs) -> (Vec<MemberKey>, Option<EpochMs>) {
    let mut out: BTreeSet<MemberKey> = BTreeSet::new();
    let mut tray: HashMap<&MemberKey, u32> = HashMap::new();
    for t in tasks.iter().filter(|t| t.status != TaskStatus::Deleted) {
        if let (Some(owner), Station::Assigned) = (&t.owner, t.station) {
            *tray.entry(owner).or_default() += 1;
        }
    }
    for (m, n) in tray {
        if n >= 2 {
            out.insert(m.clone());
        }
    }
    // Blocks ≥ 2.
    let by_key: HashMap<&TaskKey, &Task> = tasks.iter().map(|t| (&t.key, t)).collect();
    for cur in tasks.iter().filter(|t| t.status == TaskStatus::InProgress) {
        let Some(owner) = &cur.owner else { continue };
        let mut waiting: BTreeSet<&TaskKey> = BTreeSet::new();
        for t in tasks.iter().filter(|t| open(t) && t.key != cur.key) {
            if t.blocked_by.contains(&cur.key) {
                waiting.insert(&t.key);
            }
        }
        for k in &cur.blocks {
            if by_key.get(k).is_some_and(|t| open(t)) {
                waiting.insert(k);
            }
        }
        if waiting.len() >= 2 {
            out.insert(owner.clone());
        }
    }
    // 3× median.
    let finished: Vec<EpochMs> = tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Done)
        .filter_map(|t| Some(t.completed_at? - t.started_at?))
        .filter(|d| *d >= 0)
        .collect();
    let mut recheck: Option<EpochMs> = None;
    if let Some(m) = median(finished) {
        let threshold = (3 * m).max(MEDIAN_RULE_MIN_MS);
        for t in tasks.iter().filter(|t| t.status == TaskStatus::InProgress) {
            let (Some(owner), Some(start)) = (&t.owner, t.started_at.or(t.first_seen_in_progress_at)) else { continue };
            let elapsed = now_wall - start;
            // "> 3 × median" and "≥ 10 min".
            if elapsed > 3 * m && elapsed >= MEDIAN_RULE_MIN_MS {
                out.insert(owner.clone());
            } else {
                let at = start + threshold + 1;
                recheck = Some(recheck.map_or(at, |r: EpochMs| r.min(at)));
            }
        }
    }
    if let Some(l) = lead {
        out.remove(l);
    }
    (out.into_iter().collect(), recheck)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::ids::TeamKey;
    use crate::contract::team::TaskSource;

    fn team() -> TeamKey {
        TeamKey::derive("claudeNative", None, "t")
    }

    fn task(n: &str, owner: Option<&MemberKey>, status: TaskStatus, station: Station) -> Task {
        Task {
            key: TaskKey::derive(&team(), n),
            team_key: team(),
            native_id: Some(n.to_string()),
            title: None,
            inferred_k: None,
            status,
            station,
            owner: owner.cloned(),
            blocked_by: vec![],
            blocks: vec![],
            created_at: None,
            started_at: None,
            completed_at: None,
            first_seen_in_progress_at: None,
            source: TaskSource::File,
            inferred: false,
        }
    }

    #[test]
    fn tray_blocks_and_median_and_lead_exempt() {
        let a = MemberKey::derive(&team(), "a@t", Some(1));
        let b = MemberKey::derive(&team(), "b@t", Some(2));
        let lead = MemberKey::derive(&team(), "team-lead@t", Some(0));
        let mut ts = vec![
            task("1", Some(&a), TaskStatus::Open, Station::Assigned),
            task("2", Some(&a), TaskStatus::Open, Station::Assigned),
            task("3", Some(&lead), TaskStatus::Open, Station::Assigned),
            task("4", Some(&lead), TaskStatus::Open, Station::Assigned),
        ];
        let (got, _) = bottlenecks(&ts, Some(&lead), 0);
        assert_eq!(got, vec![a.clone()]);
        // b's current task blocks two open tasks.
        let mut cur = task("5", Some(&b), TaskStatus::InProgress, Station::InProgress);
        cur.started_at = Some(0);
        let k5 = cur.key.clone();
        ts.push(cur);
        let mut w1 = task("6", None, TaskStatus::Open, Station::Blocked);
        w1.blocked_by = vec![k5.clone()];
        let mut w2 = task("7", None, TaskStatus::Open, Station::Blocked);
        w2.blocked_by = vec![k5];
        ts.push(w1);
        ts.push(w2);
        let (got, _) = bottlenecks(&ts, Some(&lead), 0);
        assert!(got.contains(&b));
    }

    #[test]
    fn median_rule_needs_two_finished_and_ten_minutes() {
        let a = MemberKey::derive(&team(), "a@t", Some(1));
        let min = 60_000;
        let mut d1 = task("1", Some(&a), TaskStatus::Done, Station::Done);
        d1.started_at = Some(0);
        d1.completed_at = Some(2 * min);
        let mut d2 = task("2", Some(&a), TaskStatus::Done, Station::Done);
        d2.started_at = Some(0);
        d2.completed_at = Some(4 * min);
        let mut cur = task("3", Some(&a), TaskStatus::InProgress, Station::InProgress);
        cur.started_at = Some(0);
        let ts = vec![d1, d2, cur];
        // median 3 min → threshold max(9, 10) min.
        let (got, recheck) = bottlenecks(&ts, None, 9 * min + 30_000);
        assert!(got.is_empty());
        assert_eq!(recheck, Some(10 * min + 1));
        let (got, _) = bottlenecks(&ts, None, 10 * min);
        assert_eq!(got, vec![a]);
    }
}
