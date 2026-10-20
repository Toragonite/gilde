//! `cco_team_*` commands — the READ-ONLY team preview (Claude Code Orchestrator
//! team contract v2 §8.2, S1-2).
//!
//! Teams of coding agents are run by an engine in another product: each Claude
//! Code session's dispatch host. Gilde only WATCHES them. The sidecar's observer
//! (`sidecar/src/methods/cco.ts`) finds the hosts, subscribes as an observer and
//! keeps one view per team; these two commands are 1:1 onto it, and every live
//! change arrives as `gilde://team/snapshot` (relayed by `sidecar/mod.rs`).
//!
//! There is deliberately nothing else here: no message composer, no member
//! action, no take-over, no board edit. Gilde signals nothing to the engine and
//! to none of its processes (contract §8.2 "it signals nothing").
//!
//! What Rust adds on the way through:
//!
//!   * **the scope** — `cco_team_list` tells the sidecar which folders Gilde has
//!     open (project roots, live worktrees, PTY session cwds) and which PTY
//!     session pids are Gilde's own, on every call, and asks it to watch the
//!     teams of every workspace of this user (`allWorkspaces`), so the Team view
//!     works with no project added. A Gilde that never opens the Team view never
//!     connects to anything.
//!   * **L7** — a team read from `state.json` (no live subscription) is believed
//!     `active` only when its owner host is alive and started when it says;
//!     only Rust can read that start time without spawning (`owner_start_ms`).
//!   * **the generation** — the sidecar's revs restart with it, so every answer
//!     carries the generation that composed it.
//!
//! A sidecar that is down is not an error: the list says `available: false` and
//! the panel says so, exactly like the rest of the headless lane (DESIGN §5-3).

use std::collections::BTreeSet;
use std::time::Duration;

use tauri::State;

use crate::commands::mesh::SidecarState;
use crate::commands::pty::PtyState;
use crate::error::{CmdResult, GildeError, GildeErrorPayload};
use crate::sidecar::{
    is_cco_team_id, method, owner_start_ms, CcoTeamListParams, CcoTeamListResult, CcoTeamView,
    SidecarCallError,
};
use crate::store::{dao, Db};
use crate::supervise::health::{TeamFacts, TeamSource};

/// Both calls are answered from the sidecar's memory; anything slower is a
/// sidecar that is not well, and the panel should say so rather than hang.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);
/// Upper bound on the scope handed over (the sidecar caps it the same way).
const SCOPE_MAX: usize = 256;

/// Every team in sight of the open projects, with the hosts found.
///
/// Also (re)states the scope, so calling it is what starts the observer and
/// what keeps its folders in step with the projects. The frontend calls it when
/// the Team view opens, every few seconds while it is shown, and when the
/// sidecar comes back.
#[tauri::command]
pub async fn cco_team_list(
    sidecar: State<'_, SidecarState>,
    db: State<'_, Db>,
    pty: State<'_, PtyState>,
) -> CmdResult<CcoTeamListResult> {
    let params = scope(db.inner(), pty.inner()).await;
    match sidecar
        .call_detailed::<_, CcoTeamListResult>(method::TEAM_LIST, &params, Some(CALL_TIMEOUT))
        .await
    {
        Ok(mut result) => {
            result.apply_belief(sidecar.generation(), owner_start_ms);
            sidecar.team_cache().apply_list(&result);
            Ok(result)
        }
        Err(SidecarCallError::Down) | Err(SidecarCallError::Gone { .. }) => Ok(CcoTeamListResult {
            available: false,
            generation: sidecar.generation(),
            ..CcoTeamListResult::default()
        }),
        Err(err) => Err(GildeErrorPayload::from(GildeError::from(err))),
    }
}

/// The latest view of one team (`None` when it is not in sight).
///
/// The same shape `gilde://team/snapshot` carries, composed now: the frontend
/// asks for it when a list summary shows a newer rev than the view it holds —
/// which is how a snapshot dropped under backpressure is recovered.
#[tauri::command]
pub async fn cco_team_get(
    team_id: String,
    sidecar: State<'_, SidecarState>,
) -> CmdResult<Option<CcoTeamView>> {
    if !is_cco_team_id(&team_id) {
        return Err(GildeErrorPayload::from(GildeError::refused("teamId")));
    }
    let view = sidecar
        .call_detailed::<_, Option<CcoTeamView>>(
            method::TEAM_GET,
            serde_json::json!({ "teamId": team_id }),
            Some(CALL_TIMEOUT),
        )
        .await
        .map_err(|e| GildeErrorPayload::from(GildeError::from(e)))?;
    let generation = sidecar.generation();
    let view = view
        .filter(|v| v.team_id == team_id && v.is_consistent())
        .map(|mut v| {
            v.apply_belief(generation, owner_start_ms);
            v
        });
    if let Some(v) = &view {
        sidecar.team_cache().apply_view(generation, v.rev, &v.team_id, Some(v));
    }
    Ok(view)
}

// ---------------------------------------------------------------------------
// the health tick's team facts
// ---------------------------------------------------------------------------

/// How often the health tick asks the observer for a fresh list (and so keeps
/// it watching) when nothing else did.
pub const FEED_REFRESH: Duration = Duration::from_secs(15);

/// The production [`TeamSource`]: the sidecar's team cache, kept fresh.
///
/// The Team view's own polling already refreshes the cache while it is shown;
/// the health surface must work while it is not (a user on the Worker accounts
/// view still needs the "leftover agent processes" alert), so every
/// [`FEED_REFRESH`] the feed states the same scope `cco_team_list` states and
/// re-reads the views the list shows to be newer. Read-only, exactly like the
/// Team view: it widens nothing about what is done with a host.
pub struct TeamFeed {
    sidecar: SidecarState,
    db: Db,
    pty: PtyState,
    last_poke: parking_lot::Mutex<Option<std::time::Instant>>,
    in_flight: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl TeamFeed {
    pub fn new(sidecar: SidecarState, db: Db, pty: PtyState) -> TeamFeed {
        TeamFeed {
            sidecar,
            db,
            pty,
            last_poke: parking_lot::Mutex::new(None),
            in_flight: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// One refresh: the list (with the scope), then the views it shows stale.
    pub async fn refresh(sidecar: &SidecarState, db: &Db, pty: &PtyState) {
        let params = scope(db, pty).await;
        let list = match sidecar
            .call_detailed::<_, CcoTeamListResult>(method::TEAM_LIST, &params, Some(CALL_TIMEOUT))
            .await
        {
            Ok(mut list) => {
                list.apply_belief(sidecar.generation(), owner_start_ms);
                list
            }
            // Down or failing: the cache's list goes stale on its own, which
            // is what turns the team facts to "not known".
            Err(_) => return,
        };
        let cache = sidecar.team_cache().clone();
        cache.apply_list(&list);
        for summary in &list.teams {
            let held = cache.revision(&summary.team_id);
            if held.is_some_and(|(generation, rev)| (generation, rev) >= (list.generation, summary.rev)) {
                continue;
            }
            let answer = sidecar
                .call_detailed::<_, Option<CcoTeamView>>(
                    method::TEAM_GET,
                    serde_json::json!({ "teamId": summary.team_id }),
                    Some(CALL_TIMEOUT),
                )
                .await;
            if let Ok(Some(mut view)) = answer {
                if view.team_id != summary.team_id || !view.is_consistent() {
                    continue;
                }
                let generation = sidecar.generation();
                view.apply_belief(generation, owner_start_ms);
                cache.apply_view(generation, view.rev, &view.team_id, Some(&view));
            }
        }
    }
}

impl TeamSource for TeamFeed {
    fn facts(&self) -> TeamFacts {
        self.sidecar
            .team_cache()
            .facts(crate::events::now_ms(), owner_start_ms)
    }

    fn poke(&self) {
        {
            let mut last = self.last_poke.lock();
            if last.is_some_and(|at| at.elapsed() < FEED_REFRESH) {
                return;
            }
            *last = Some(std::time::Instant::now());
        }
        if self
            .in_flight
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return;
        }
        let (sidecar, db, pty) = (self.sidecar.clone(), self.db.clone(), self.pty.clone());
        let in_flight = self.in_flight.clone();
        tauri::async_runtime::spawn(async move {
            TeamFeed::refresh(&sidecar, &db, &pty).await;
            in_flight.store(false, std::sync::atomic::Ordering::Release);
        });
    }
}

/// What Gilde has open: project roots and live worktree paths from the store,
/// the cwds and pids of its PTY sessions. A failed store read narrows the
/// scope; it never fails the call.
pub(crate) async fn scope(db: &Db, pty: &PtyState) -> CcoTeamListParams {
    let mut roots = BTreeSet::new();
    match db.read(dao::project::list).await {
        Ok(projects) => {
            for project in projects {
                roots.insert(project.canonical_path);
            }
        }
        Err(err) => tracing::warn!(target: "cco", error = %err, "could not read the projects"),
    }
    match db.read(dao::worktree::list_live).await {
        Ok(worktrees) => {
            for worktree in worktrees {
                roots.insert(worktree.path);
            }
        }
        Err(err) => tracing::warn!(target: "cco", error = %err, "could not read the worktrees"),
    }
    let mut pane_pids = BTreeSet::new();
    for session in pty.list() {
        if !session.cwd.is_empty() {
            roots.insert(session.cwd);
        }
        if session.pid > 1 {
            pane_pids.insert(i64::from(session.pid));
        }
    }
    CcoTeamListParams {
        roots: roots
            .into_iter()
            .filter(|r| std::path::Path::new(r).is_absolute())
            .take(SCOPE_MAX)
            .collect(),
        pane_pids: pane_pids.into_iter().take(SCOPE_MAX).collect(),
        all_workspaces: true,
    }
}
