//! What Rust remembers of the Claude Code Orchestrator teams it relays — the
//! health builder's team facts (`supervise::health::TeamFacts`).
//!
//! The views themselves still go to the UI exactly as before
//! (`gilde://team/snapshot`, `cco_team_list` / `cco_team_get`); this cache is
//! fed from the same three places so the 5 s health tick can read them without
//! asking the sidecar anything:
//!
//!   * every relayed `cco.team.snapshot` ([`TeamCache::apply_view`]),
//!   * every `cco_team_get` answer (the same),
//!   * every `cco_team_list` answer ([`TeamCache::apply_list`]) — the list is
//!     what says the observer is up, which teams are still in sight, where the
//!     data root is and which hosts it found.
//!
//! Latest wins by `(generation, rev)`, as in the frontend's team slice. One
//! fact is kept that no single view holds: which members had a running turn in
//! the last LIVE view of a team, because "the team lost its host while members
//! were working" is a statement about the past.

use std::collections::HashMap;

use parking_lot::RwLock;

use crate::events::now_ms;
use crate::sidecar::types::{cco_believed, CcoTeamListResult, CcoTeamView};
use crate::supervise::health::{TeamFact, TeamFacts};

/// A list answer older than this no longer proves the observer is up.
pub const LIST_FRESH_MS: i64 = 45_000;

#[derive(Debug, Clone)]
struct Entry {
    generation: i64,
    rev: u64,
    view: CcoTeamView,
    running_at_last_live: Vec<String>,
}

#[derive(Debug, Default)]
struct Inner {
    views: HashMap<String, Entry>,
    /// Members that were running in the last live view, kept across a
    /// withdrawal so a team that drops out and comes back as `disk` still knows.
    last_live: HashMap<String, Vec<String>>,
    list: Option<(i64, CcoTeamListResult)>,
}

/// See the module docs.
#[derive(Debug, Default)]
pub struct TeamCache {
    inner: RwLock<Inner>,
}

impl TeamCache {
    pub fn new() -> TeamCache {
        TeamCache::default()
    }

    /// Fold one view (or a withdrawal, `None`) in. Older `(generation, rev)` is ignored.
    pub fn apply_view(&self, generation: i64, rev: u64, team_id: &str, view: Option<&CcoTeamView>) {
        let mut inner = self.inner.write();
        if let Some(held) = inner.views.get(team_id) {
            if (generation, rev) < (held.generation, held.rev) {
                return;
            }
        }
        match view {
            None => {
                inner.views.remove(team_id);
            }
            Some(view) => {
                if view.live {
                    let running: Vec<String> = view
                        .state
                        .members
                        .iter()
                        .filter(|m| m.turn.is_some() || m.state == "running")
                        .map(|m| m.name.clone())
                        .collect();
                    inner.last_live.insert(team_id.to_string(), running);
                }
                let running_at_last_live = inner.last_live.get(team_id).cloned().unwrap_or_default();
                inner.views.insert(
                    team_id.to_string(),
                    Entry {
                        generation,
                        rev,
                        view: view.clone(),
                        running_at_last_live,
                    },
                );
            }
        }
    }

    /// Fold a list answer in: remember it, and drop the views of teams it no
    /// longer lists (they left sight).
    pub fn apply_list(&self, list: &CcoTeamListResult) {
        let mut inner = self.inner.write();
        if list.available {
            let listed: std::collections::HashSet<&str> =
                list.teams.iter().map(|t| t.team_id.as_str()).collect();
            inner.views.retain(|id, _| listed.contains(id.as_str()));
        }
        inner.list = Some((now_ms(), list.clone()));
    }

    /// `(generation, rev)` held for a team.
    pub fn revision(&self, team_id: &str) -> Option<(i64, u64)> {
        self.inner
            .read()
            .views
            .get(team_id)
            .map(|e| (e.generation, e.rev))
    }

    /// The facts as of `now`. `believed` is recomputed here (the owner host
    /// may have died since the view arrived) with `start_ms_of_live_pid`.
    pub fn facts(&self, now: i64, start_ms_of_live_pid: impl Fn(i64) -> Option<i64>) -> TeamFacts {
        let inner = self.inner.read();
        let (known, data_root, hosts) = match &inner.list {
            Some((at, list)) if list.available && now.saturating_sub(*at) <= LIST_FRESH_MS => (
                true,
                Some(list.data_root.clone()).filter(|r| !r.is_empty()),
                list.hosts.clone(),
            ),
            Some((_, list)) => (
                false,
                Some(list.data_root.clone()).filter(|r| !r.is_empty()),
                Vec::new(),
            ),
            None => (false, None, Vec::new()),
        };
        let mut teams: Vec<TeamFact> = inner
            .views
            .values()
            .map(|e| {
                let mut view = e.view.clone();
                view.believed = cco_believed(
                    view.live,
                    &view.state.status,
                    view.state.owner.as_ref(),
                    &start_ms_of_live_pid,
                );
                TeamFact {
                    view,
                    running_at_last_live: e.running_at_last_live.clone(),
                }
            })
            .collect();
        teams.sort_by(|a, b| a.view.team_id.cmp(&b.view.team_id));
        TeamFacts {
            known,
            data_root,
            teams,
            hosts,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidecar::types::{CcoMemberState, CcoTeamState, CcoTeamSummary, CcoTurn};

    fn view(id: &str, rev: u64, live: bool, running: &[&str]) -> CcoTeamView {
        CcoTeamView {
            team_id: id.into(),
            rev,
            live,
            state: CcoTeamState {
                id: id.into(),
                name: "demo".into(),
                status: "active".into(),
                members: running
                    .iter()
                    .map(|n| CcoMemberState {
                        name: (*n).into(),
                        state: "running".into(),
                        turn: Some(CcoTurn {
                            id: "t1".into(),
                            n: 1,
                            started_at: 1,
                            pid: Some(4242),
                            last_output_at: None,
                        }),
                        ..CcoMemberState::default()
                    })
                    .collect(),
                ..CcoTeamState::default()
            },
            ..CcoTeamView::default()
        }
    }

    fn list(ids: &[&str]) -> CcoTeamListResult {
        CcoTeamListResult {
            available: true,
            data_root: "/data".into(),
            teams: ids
                .iter()
                .map(|id| CcoTeamSummary {
                    team_id: (*id).into(),
                    ..CcoTeamSummary::default()
                })
                .collect(),
            ..CcoTeamListResult::default()
        }
    }

    #[test]
    fn the_newest_view_wins_and_the_last_live_running_set_survives_a_disk_view() {
        let cache = TeamCache::new();
        cache.apply_view(1, 5, "tmabcdefghi1", Some(&view("tmabcdefghi1", 5, true, &["alpha"])));
        cache.apply_view(1, 4, "tmabcdefghi1", Some(&view("tmabcdefghi1", 4, true, &[])));
        assert_eq!(cache.revision("tmabcdefghi1"), Some((1, 5)), "an older rev is ignored");
        // The host died: the next view is from disk, nobody running.
        cache.apply_view(1, 6, "tmabcdefghi1", Some(&view("tmabcdefghi1", 6, false, &[])));
        cache.apply_list(&list(&["tmabcdefghi1"]));
        let facts = cache.facts(now_ms(), |_| None);
        assert!(facts.known);
        assert_eq!(facts.data_root.as_deref(), Some("/data"));
        assert_eq!(facts.teams.len(), 1);
        assert_eq!(facts.teams[0].running_at_last_live, vec!["alpha".to_string()]);
        assert!(!facts.teams[0].view.believed, "active, not live, owner unknown → not believed");
    }

    #[test]
    fn a_list_drops_teams_that_left_sight_and_goes_stale() {
        let cache = TeamCache::new();
        cache.apply_view(1, 1, "tmabcdefghi1", Some(&view("tmabcdefghi1", 1, true, &[])));
        cache.apply_view(1, 1, "tmabcdefghi2", Some(&view("tmabcdefghi2", 1, true, &[])));
        cache.apply_list(&list(&["tmabcdefghi2"]));
        let facts = cache.facts(now_ms(), |_| None);
        assert_eq!(facts.teams.len(), 1);
        assert_eq!(facts.teams[0].view.team_id, "tmabcdefghi2");
        // Without a fresh list the facts are not known.
        let later = cache.facts(now_ms() + LIST_FRESH_MS + 1, |_| None);
        assert!(!later.known);
        assert!(TeamCache::new().facts(now_ms(), |_| None).teams.is_empty());
        assert!(!TeamCache::new().facts(now_ms(), |_| None).known);
    }

    #[test]
    fn a_withdrawal_removes_the_view() {
        let cache = TeamCache::new();
        cache.apply_view(1, 1, "tmabcdefghi1", Some(&view("tmabcdefghi1", 1, true, &[])));
        cache.apply_view(1, 2, "tmabcdefghi1", None);
        assert!(cache.revision("tmabcdefghi1").is_none());
    }
}
