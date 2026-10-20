//! T25 — the graph projection.
//!
//! DESIGN §9-5 says the sidebar tree, the tab pills, the dispatch cards, the
//! Inspector and the React Flow canvas are five renderings of *one*
//! `GraphSnapshot`. This module is the place that snapshot is produced from the
//! rest of the system:
//!
//! ```text
//!   state.db rows ─┐
//!   PtyManager  ───┤
//!   DispatchEngine ├─► ProjectionInput ─► project() ─► desired GraphState
//!   Watchdog scan ─┤                                        │
//!   account slots ─┘                          reconcile(current, desired)
//!                                                           │
//!                                                    Vec<GraphOp> ─► EventBus
//! ```
//!
//! Three properties are load-bearing and are pinned by the tests below:
//!
//!  1. **Node ids are stable.** They are `"<kind>:<entity id>"` and the entity
//!     id is the durable primary key, so projecting twice produces the same
//!     ids, the same `topologyHash`, and therefore no dagre re-layout.
//!  2. **The projection never runs `ps`.** Session RSS comes from the last
//!     watchdog sample that already ran one (DESIGN §8-8); a projection with no
//!     sample carries the previous value forward rather than inventing one.
//!  3. **It does not fight the incremental producers.** `PtyManager`,
//!     `DispatchEngine` and `StatusEngine` push their own ops as things happen;
//!     [`reconcile`] emits an op only for a field that actually differs, keeps
//!     fields the projection cannot know (evidence, `resumes` edges), and
//!     downgrades a metrics-only difference to a `patchNode` on the metric lane.
//!
//! The projector owns what nobody else does: projects, worktree statistics,
//! orphan clusters, account nodes, cross-rank edges and `Totals`.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tauri::State;

use crate::accounts::AccountSlot;
use crate::commands::accounts::DbState;
use crate::error::{CmdResult, Result};
use crate::events::bus::EventBus;
use crate::model::graph::{
    node_id, EdgeKind, GraphEdge, GraphHandle, GraphNode, GraphOp, GraphState, NodeKind,
    NodeStatus, PressureLevel, SidecarState, Totals,
};
use crate::model::status::{NodeBadge, NodeMetrics, StatusEvidence};
use crate::pty::PtyManager;
use crate::sidecar::SidecarManager;
use crate::store::models::{DispatchRunRow, DispatchTaskRow, ProjectRow, WorktreeRow};
use crate::store::{dao, now_ms, Db};
use crate::supervise::watchdog::Watchdog;
use crate::supervise::{ProcGroupKind, ProcRow as ScanRow, ScanResult};

/// How long a node whose entity has gone away stays on the canvas. A finished
/// run and a closed session are still worth looking at for a while — the graph
/// has a "hide exited" filter precisely because those nodes exist.
pub const TERMINAL_GRACE_MS: i64 = 10 * 60 * 1_000;

/// Recent dispatch tasks projected as nodes. Older tasks are history and belong
/// to the dispatch board, not to the live graph.
pub const TASK_WINDOW: i64 = 12;

/// Samples kept per session for the output-rate sparkline (DESIGN §9-6).
pub const RATE_SAMPLES: usize = 60;

/// Account nodes are drawn only once there is a choice to make. With a single
/// slot the rank is noise, which is why DESIGN §9-5 calls the slot free.
pub const MIN_ACCOUNTS_FOR_RANK: usize = 2;

// ===========================================================================
// Input facts
// ===========================================================================

/// One live (or recently live) PTY session, as the graph needs it.
///
/// Built from `PtySessionInfo` plus the status engine's report, so the
/// projection never re-derives status and never disagrees with the pill in the
/// terminal header.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionFacts {
    pub id: String,
    pub worktree_id: Option<String>,
    pub vendor: String,
    pub title: String,
    pub account_id: Option<String>,
    pub pid: i32,
    pub pgid: i32,
    pub cwd: String,
    pub opened_at: i64,
    pub detached: bool,
    pub status: NodeStatus,
    pub confidence: Option<f32>,
    pub evidence: Option<Vec<StatusEvidence>>,
    pub attention: bool,
    /// Summed over the session's process group by the watchdog. `None` when no
    /// sample has landed yet — never a zero, which would read as "it freed its
    /// memory" instead of "we have not looked".
    pub rss_bytes: Option<u64>,
    pub cpu_pct: Option<f32>,
    pub last_activity_at: Option<i64>,
    /// Newest last, at most [`RATE_SAMPLES`] entries.
    pub output_rate: Option<Vec<f32>>,
}

impl SessionFacts {
    pub fn new(id: impl Into<String>, vendor: impl Into<String>, opened_at: i64) -> SessionFacts {
        SessionFacts {
            id: id.into(),
            worktree_id: None,
            vendor: vendor.into(),
            title: String::new(),
            account_id: None,
            pid: 0,
            pgid: 0,
            cwd: String::new(),
            opened_at,
            detached: false,
            status: NodeStatus::Idle,
            confidence: None,
            evidence: None,
            attention: false,
            rss_bytes: None,
            cpu_pct: None,
            last_activity_at: None,
            output_rate: None,
        }
    }

    fn label(&self) -> String {
        if self.vendor.is_empty() {
            self.title.clone()
        } else {
            self.vendor.clone()
        }
    }
}

/// A process the scanner proved is ours and orphaned (DESIGN §8-7 `oursOrphan`).
#[derive(Debug, Clone, PartialEq)]
pub struct OrphanFacts {
    pub pid: i32,
    pub pgid: i32,
    pub name: String,
    /// Already masked in Rust before it left the scanner.
    pub argv_display: String,
    pub rss_bytes: u64,
    pub cpu_pct: f32,
    pub elapsed_secs: u64,
    pub worktree_id: Option<String>,
    pub session_id: Option<String>,
    pub run_id: Option<String>,
}

impl OrphanFacts {
    /// Only `oursOrphan` rows qualify: a graph node is a claim of ownership and
    /// must never be made about somebody else's process.
    pub fn from_scan_row(row: &ScanRow) -> Option<OrphanFacts> {
        if row.group != ProcGroupKind::OursOrphan {
            return None;
        }
        let owner = row.owner.clone().unwrap_or_default();
        Some(OrphanFacts {
            pid: row.pid,
            pgid: row.pgid,
            name: row.name.clone(),
            argv_display: row.argv_display.clone(),
            rss_bytes: row.rss_bytes,
            cpu_pct: row.cpu_pct,
            elapsed_secs: row.elapsed_secs,
            worktree_id: owner.worktree_id,
            session_id: owner.session_id,
            run_id: owner.run_id,
        })
    }
}

/// One `worktree_alias` row: the provider-visible path for a central worktree.
#[derive(Debug, Clone, PartialEq)]
pub struct AliasFacts {
    pub worktree_id: String,
    pub alias_path: String,
}

/// The machine-wide numbers `Totals` reports. Supplied by the watchdog, which
/// is the only component allowed to run `ps` and read memory pressure.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BudgetFacts {
    pub rss_bytes: u64,
    pub physical_ram_bytes: u64,
    pub pressure: PressureLevel,
}

/// Everything [`project`] reads. Plain data on purpose: the projection is a
/// pure function of it, which is what makes the timing test meaningful.
#[derive(Debug, Clone, Default)]
pub struct ProjectionInput {
    pub at: i64,
    pub projects: Vec<ProjectRow>,
    pub worktrees: Vec<WorktreeRow>,
    pub sessions: Vec<SessionFacts>,
    pub tasks: Vec<DispatchTaskRow>,
    pub runs: Vec<DispatchRunRow>,
    pub aliases: Vec<AliasFacts>,
    pub accounts: Vec<AccountSlot>,
    pub orphans: Vec<OrphanFacts>,
    pub budget: BudgetFacts,
    pub sidecar: SidecarState,
}

// ===========================================================================
// Status mapping
// ===========================================================================

/// `dispatch_run.status` → node status. Mirrors the mapping the dispatch engine
/// uses when it pushes its own op, so the two never disagree about a run.
pub fn run_status(raw: &str) -> NodeStatus {
    match raw {
        "queued" | "spawning" => NodeStatus::Idle,
        "running" => NodeStatus::Working,
        "done" => NodeStatus::Done,
        "error" => NodeStatus::Error,
        // The app failed, not the agent (DESIGN §5-10): never an error.
        "interrupted" => NodeStatus::Stalled,
        _ => NodeStatus::Exited,
    }
}

/// `dispatch_task.status` → node status.
pub fn task_status(raw: &str) -> NodeStatus {
    match raw {
        "queued" => NodeStatus::Idle,
        "running" => NodeStatus::Working,
        "done" => NodeStatus::Done,
        "partial" => NodeStatus::Ok,
        "error" => NodeStatus::Error,
        _ => NodeStatus::Exited,
    }
}

fn run_is_live(raw: &str) -> bool {
    matches!(raw, "queued" | "spawning" | "running")
}

/// Statuses after which nothing more will happen. Used by the removal grace.
fn is_terminal(status: NodeStatus) -> bool {
    matches!(
        status,
        NodeStatus::Exited | NodeStatus::Done | NodeStatus::Error | NodeStatus::Detached
    )
}

// ===========================================================================
// The projection
// ===========================================================================

/// Fold every source into one graph. Pure: same input, same bytes out.
pub fn project(input: &ProjectionInput) -> GraphState {
    let at = input.at;
    let mut state = GraphState::new();
    let mut totals = Totals {
        pressure_level: input.budget.pressure,
        sidecar_state: input.sidecar,
        rss_bytes: input.budget.rss_bytes,
        rss_budget_percent: if input.budget.physical_ram_bytes == 0 {
            0.0
        } else {
            (input.budget.rss_bytes as f64 / input.budget.physical_ram_bytes as f64 * 100.0) as f32
        },
        at,
        ..Totals::default()
    };

    // -- accounts (rank 0, rendered only when there is a choice) -------------
    let account_nodes: BTreeSet<String> = if input.accounts.len() >= MIN_ACCOUNTS_FOR_RANK {
        input
            .accounts
            .iter()
            .map(|slot| node_id(NodeKind::Account, &slot.id))
            .collect()
    } else {
        BTreeSet::new()
    };
    if !account_nodes.is_empty() {
        for slot in &input.accounts {
            let id = node_id(NodeKind::Account, &slot.id);
            let mut node = GraphNode::new(
                &id,
                NodeKind::Account,
                &slot.label,
                if slot.logged_in {
                    NodeStatus::Ok
                } else {
                    NodeStatus::Idle
                },
                at,
            );
            node.sublabel = Some(slot.vendor.clone());
            node.meta.vendor = Some(slot.vendor.clone());
            node.meta.account_id = Some(slot.id.clone());
            if !slot.enabled {
                node.badges.push(NodeBadge::Detached);
            }
            state.apply(GraphOp::UpsertNode { node });
        }
    }

    // -- projects -----------------------------------------------------------
    let mut project_ids = BTreeSet::new();
    for row in &input.projects {
        let id = node_id(NodeKind::Project, &row.id);
        let mut node = GraphNode::new(&id, NodeKind::Project, &row.name, NodeStatus::Ok, at);
        node.sublabel = Some(row.repo_slug.clone());
        node.meta.cwd = Some(row.canonical_path.clone());
        state.apply(GraphOp::UpsertNode { node });
        project_ids.insert(row.id.clone());
        totals.projects += 1;
    }

    // -- worktrees ----------------------------------------------------------
    let mut worktree_ids = BTreeSet::new();
    for row in &input.worktrees {
        let id = node_id(NodeKind::Worktree, &row.id);
        let dirty = row.cached_dirty > 0;
        let mut node = GraphNode::new(
            &id,
            NodeKind::Worktree,
            &row.slug,
            if dirty {
                NodeStatus::Dirty
            } else {
                NodeStatus::Clean
            },
            at,
        );
        node.sublabel = Some(row.branch.clone());
        node.meta.branch = Some(row.branch.clone());
        node.meta.worktree_id = Some(row.id.clone());
        node.meta.cwd = Some(row.path.clone());
        node.metrics.insertions = Some(row.cached_added.max(0) as u64);
        node.metrics.deletions = Some(row.cached_removed.max(0) as u64);
        node.metrics.dirty_files = Some(row.cached_dirty.max(0) as u32);
        node.metrics.ahead = Some(row.cached_ahead.max(0) as u32);
        node.metrics.behind = Some(row.cached_behind.max(0) as u32);
        if dirty {
            node.badges.push(NodeBadge::Dirty);
        }
        if row.cached_ahead > 0 {
            node.badges.push(NodeBadge::Unmerged);
        }
        if row.locked {
            node.badges.push(NodeBadge::Locked);
        }
        if row.verified_at.is_none() {
            node.badges.push(NodeBadge::Unverified);
        }
        if project_ids.contains(&row.project_id) {
            let parent = node_id(NodeKind::Project, &row.project_id);
            node.parent_id = Some(parent.clone());
            state.apply(GraphOp::UpsertEdge {
                edge: GraphEdge::new(EdgeKind::Contains, parent, id.clone()),
            });
        }
        state.apply(GraphOp::UpsertNode { node });
        worktree_ids.insert(row.id.clone());
        totals.worktrees += 1;
    }

    // -- sessions -----------------------------------------------------------
    // Alias paths, longest first: `<repo>/.claude/worktrees/x` and the central
    // path can both be prefixes, and the more specific one wins.
    let mut aliases: Vec<&AliasFacts> = input.aliases.iter().collect();
    aliases.sort_by(|a, b| b.alias_path.len().cmp(&a.alias_path.len()));

    for facts in &input.sessions {
        let id = node_id(NodeKind::Session, &facts.id);
        let mut node = GraphNode::new(&id, NodeKind::Session, facts.label(), facts.status, at);
        node.sublabel = Some(facts.title.clone()).filter(|t| !t.is_empty());
        node.confidence = facts.confidence;
        node.evidence = facts.evidence.clone();
        node.meta.vendor = Some(facts.vendor.clone());
        node.meta.account_id = facts.account_id.clone();
        node.meta.pid = Some(facts.pid);
        node.meta.pgid = Some(facts.pgid);
        node.meta.cwd = Some(facts.cwd.clone()).filter(|c| !c.is_empty());
        node.meta.worktree_id = facts.worktree_id.clone();
        node.metrics.rss_bytes = facts.rss_bytes;
        node.metrics.cpu_pct = facts.cpu_pct;
        node.metrics.elapsed_ms = Some((at - facts.opened_at).max(0) as u64);
        node.metrics.last_activity_at = facts.last_activity_at;
        node.metrics.output_rate = facts.output_rate.clone();
        if facts.attention {
            node.badges.push(NodeBadge::Attention);
        }
        if facts.detached {
            node.badges.push(NodeBadge::Detached);
        }

        let worktree = facts
            .worktree_id
            .as_ref()
            .filter(|id| worktree_ids.contains(*id));
        if let Some(worktree_id) = worktree {
            let parent = node_id(NodeKind::Worktree, worktree_id);
            node.parent_id = Some(parent.clone());
            let mut edge = GraphEdge::new(EdgeKind::Contains, parent, id.clone());
            edge.active = facts.status == NodeStatus::Working;
            state.apply(GraphOp::UpsertEdge { edge });

            // A session that reached the worktree through a provider alias
            // (DESIGN §7-6) says so: same worktree, different door.
            if let Some(alias) = aliases
                .iter()
                .find(|a| a.worktree_id == *worktree_id && path_is_under(&facts.cwd, &a.alias_path))
            {
                let _ = alias;
                state.apply(GraphOp::UpsertEdge {
                    edge: GraphEdge::new(
                        EdgeKind::Aliases,
                        id.clone(),
                        node_id(NodeKind::Worktree, worktree_id),
                    ),
                });
            }
        }
        if let Some(account_id) = facts.account_id.as_ref() {
            let target = node_id(NodeKind::Account, account_id);
            if account_nodes.contains(&target) {
                state.apply(GraphOp::UpsertEdge {
                    edge: GraphEdge::new(EdgeKind::UsesAccount, id.clone(), target),
                });
            }
        }
        state.apply(GraphOp::UpsertNode { node });

        totals.sessions += 1;
        match facts.status {
            NodeStatus::Working => totals.sessions_working += 1,
            NodeStatus::Waiting => totals.sessions_waiting += 1,
            NodeStatus::Idle => totals.sessions_idle += 1,
            _ => {}
        }
    }

    // -- dispatch tasks -----------------------------------------------------
    let mut task_ids = BTreeSet::new();
    for row in &input.tasks {
        let id = node_id(NodeKind::Dispatch, &row.id);
        let mut node = GraphNode::new(
            &id,
            NodeKind::Dispatch,
            &row.title,
            task_status(&row.status),
            at,
        );
        node.sublabel = Some(row.task_slug.clone());
        node.metrics.elapsed_ms =
            Some((row.finished_at.unwrap_or(at) - row.created_at).max(0) as u64);
        if let Some(project_id) = row.project_id.as_ref() {
            if project_ids.contains(project_id) {
                let parent = node_id(NodeKind::Project, project_id);
                node.parent_id = Some(parent.clone());
                state.apply(GraphOp::UpsertEdge {
                    edge: GraphEdge::new(EdgeKind::Contains, parent, id.clone()),
                });
            }
        }
        state.apply(GraphOp::UpsertNode { node });
        task_ids.insert(row.id.clone());
    }

    // -- runs ---------------------------------------------------------------
    for row in &input.runs {
        let id = node_id(NodeKind::Run, &row.id);
        let status = run_status(&row.status);
        let mut node = GraphNode::new(&id, NodeKind::Run, &row.vendor, status, at);
        node.sublabel = row
            .model_actual
            .clone()
            .or_else(|| row.model_requested.clone())
            .filter(|m| !m.is_empty());
        node.meta.vendor = Some(row.vendor.clone());
        node.meta.account_id = row.account_id.clone();
        node.meta.worktree_id = row.worktree_id.clone();
        node.metrics.elapsed_ms = row
            .started_at
            .map(|started| (row.finished_at.unwrap_or(at) - started).max(0) as u64);
        apply_usage(&mut node.metrics, row);
        if row.status == "interrupted" {
            node.badges.push(NodeBadge::Stale);
        }

        let worktree = row
            .worktree_id
            .as_ref()
            .filter(|id| worktree_ids.contains(*id));
        if let Some(worktree_id) = worktree {
            let parent = node_id(NodeKind::Worktree, worktree_id);
            node.parent_id = Some(parent.clone());
            let mut edge = GraphEdge::new(EdgeKind::Contains, parent, id.clone());
            edge.active = run_is_live(&row.status);
            state.apply(GraphOp::UpsertEdge { edge });
        } else if task_ids.contains(&row.task_id) {
            node.parent_id = Some(node_id(NodeKind::Dispatch, &row.task_id));
        }
        if task_ids.contains(&row.task_id) {
            let mut edge = GraphEdge::new(
                EdgeKind::DispatchedTo,
                node_id(NodeKind::Dispatch, &row.task_id),
                id.clone(),
            );
            edge.active = run_is_live(&row.status);
            state.apply(GraphOp::UpsertEdge { edge });
        }
        if let Some(account_id) = row.account_id.as_ref() {
            let target = node_id(NodeKind::Account, account_id);
            if account_nodes.contains(&target) {
                state.apply(GraphOp::UpsertEdge {
                    edge: GraphEdge::new(EdgeKind::UsesAccount, id.clone(), target),
                });
            }
        }
        state.apply(GraphOp::UpsertNode { node });
        if run_is_live(&row.status) {
            totals.runs_active += 1;
        }
    }

    // -- orphans (a cluster off the root, never inside the rank chain) ------
    for orphan in &input.orphans {
        let id = node_id(NodeKind::Orphan, &orphan.pid.to_string());
        let mut node = GraphNode::new(&id, NodeKind::Orphan, &orphan.name, NodeStatus::Orphan, at);
        node.sublabel = Some(orphan.argv_display.clone()).filter(|a| !a.is_empty());
        node.badges.push(NodeBadge::Orphan);
        node.meta.pid = Some(orphan.pid);
        node.meta.pgid = Some(orphan.pgid);
        node.meta.worktree_id = orphan.worktree_id.clone();
        node.metrics.rss_bytes = Some(orphan.rss_bytes);
        node.metrics.cpu_pct = Some(orphan.cpu_pct);
        node.metrics.elapsed_ms = Some(orphan.elapsed_secs.saturating_mul(1_000));
        if let Some(worktree_id) = orphan
            .worktree_id
            .as_ref()
            .filter(|id| worktree_ids.contains(*id))
        {
            // Deliberately *not* `contains`: the worktree does not own this
            // process any more — that is what makes it an orphan.
            state.apply(GraphOp::UpsertEdge {
                edge: GraphEdge::new(
                    EdgeKind::RunsIn,
                    id.clone(),
                    node_id(NodeKind::Worktree, worktree_id),
                ),
            });
        }
        state.apply(GraphOp::UpsertNode { node });
        totals.orphans += 1;
    }

    state.apply(GraphOp::Totals { totals });
    state.set_at(at);
    state
}

/// Whether `path` is inside `root` (or is `root`). Prefix comparison on
/// separator boundaries, so `/a/bc` is not "inside" `/a/b`.
fn path_is_under(path: &str, root: &str) -> bool {
    if root.is_empty() || path.is_empty() {
        return false;
    }
    let root = root.trim_end_matches('/');
    match path.strip_prefix(root) {
        Some("") => true,
        Some(rest) => rest.starts_with('/'),
        None => false,
    }
}

/// Token counts and cost from the `usage_json` column, which the dispatch
/// engine writes as one blob so a card renders from a single field.
fn apply_usage(metrics: &mut NodeMetrics, row: &DispatchRunRow) {
    metrics.cost_usd = row.cost_usd;
    let Some(raw) = row.usage_json.as_deref() else {
        return;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return;
    };
    let get = |key: &str| value.get(key).and_then(serde_json::Value::as_u64);
    metrics.input_tokens = get("inputTokens");
    metrics.output_tokens = get("outputTokens");
    metrics.cached_tokens = get("cachedInputTokens").or_else(|| get("cachedTokens"));
    if metrics.cost_usd.is_none() {
        metrics.cost_usd = value.get("costUsd").and_then(serde_json::Value::as_f64);
    }
}

// ===========================================================================
// Reconciliation
// ===========================================================================

macro_rules! metric_delta {
    ($have:ident, $want:ident, $out:ident, $($field:ident),+ $(,)?) => {
        $(
            if $have.$field != $want.$field && $want.$field.is_some() {
                $out.$field = $want.$field.clone();
            }
        )+
    };
}

/// The fields of `want` that differ from `have`, and nothing else. Only present
/// (`Some`) values are emitted: a patch never erases a metric the projection
/// happens not to know about.
fn metrics_delta(have: &NodeMetrics, want: &NodeMetrics) -> NodeMetrics {
    let mut out = NodeMetrics::default();
    metric_delta!(
        have,
        want,
        out,
        rss_bytes,
        cpu_pct,
        insertions,
        deletions,
        dirty_files,
        ahead,
        behind,
        input_tokens,
        output_tokens,
        cached_tokens,
        cost_usd,
        elapsed_ms,
        last_activity_at,
        quota_percent,
        output_rate,
    );
    out
}

/// Carry forward everything the projection cannot know. The watchdog owns RSS,
/// the status engine owns evidence; a projection tick that ran without them
/// must not blank what they wrote.
fn carry_forward(want: &mut GraphNode, have: &GraphNode) {
    let mut metrics = have.metrics.clone();
    metrics.merge_from(&want.metrics);
    want.metrics = metrics;
    if want.confidence.is_none() {
        want.confidence = have.confidence;
    }
    if want.evidence.is_none() {
        want.evidence = have.evidence.clone();
    }
}

/// Whether two nodes agree on everything a `patchNode` cannot express.
fn structurally_equal(a: &GraphNode, b: &GraphNode) -> bool {
    a.kind == b.kind
        && a.parent_id == b.parent_id
        && a.label == b.label
        && a.sublabel == b.sublabel
        && a.meta == b.meta
}

/// Diff the live graph against a fresh projection.
///
/// Emits the smallest op that expresses each difference:
/// `upsertNode` only when a structural field moved, `patchNode` otherwise, and
/// nothing at all when the projection agrees with what is already there — which
/// is the normal case, tick after tick, and the reason this can run at 1 Hz.
pub fn reconcile(current: &GraphState, desired: &GraphState, at: i64) -> Vec<GraphOp> {
    let cur = current.snapshot();
    let want = desired.snapshot();
    let mut ops = Vec::new();

    let mut wanted_ids = BTreeSet::new();
    for node in &want.nodes {
        wanted_ids.insert(node.id.clone());
        let Some(have) = current.node(&node.id) else {
            ops.push(GraphOp::UpsertNode { node: node.clone() });
            continue;
        };
        let mut merged = node.clone();
        carry_forward(&mut merged, have);
        merged.updated_at = have.updated_at;
        if merged == *have {
            continue;
        }
        if structurally_equal(&merged, have) {
            let metrics = metrics_delta(&have.metrics, &merged.metrics);
            ops.push(GraphOp::PatchNode {
                id: node.id.clone(),
                status: (merged.status != have.status).then_some(merged.status),
                confidence: (merged.confidence != have.confidence)
                    .then_some(merged.confidence)
                    .flatten(),
                evidence: (merged.evidence != have.evidence)
                    .then_some(merged.evidence)
                    .flatten(),
                badges: (merged.badges != have.badges).then_some(merged.badges),
                metrics: (metrics != NodeMetrics::default()).then_some(metrics),
            });
        } else {
            merged.updated_at = at;
            ops.push(GraphOp::UpsertNode { node: merged });
        }
    }

    for node in &cur.nodes {
        if wanted_ids.contains(&node.id) {
            continue;
        }
        // An orphan node is a claim about a process that exists right now; the
        // moment the scanner stops seeing it, the claim has to go. Everything
        // else gets a grace period so a finished run stays readable.
        let keep = node.kind != NodeKind::Orphan
            && is_terminal(node.status)
            && at.saturating_sub(node.updated_at) < TERMINAL_GRACE_MS;
        if !keep {
            ops.push(GraphOp::RemoveNode {
                id: node.id.clone(),
            });
        }
    }

    // Nodes that survived the grace keep their edges; anything hanging off a
    // removed node is dropped by `GraphState::apply` itself.
    let survivors: BTreeSet<&str> = cur
        .nodes
        .iter()
        .map(|n| n.id.as_str())
        .filter(|id| {
            wanted_ids.contains(*id)
                || !ops.iter().any(
                    |op| matches!(op, GraphOp::RemoveNode { id: removed } if removed == *id),
                )
        })
        .collect();

    let mut wanted_edges = BTreeSet::new();
    for edge in &want.edges {
        wanted_edges.insert(edge.id.clone());
        match current.edge(&edge.id) {
            None => ops.push(GraphOp::UpsertEdge { edge: edge.clone() }),
            Some(have) if have.active != edge.active => ops.push(GraphOp::PatchEdge {
                id: edge.id.clone(),
                active: edge.active,
            }),
            Some(_) => {}
        }
    }
    for edge in &cur.edges {
        if wanted_edges.contains(&edge.id) {
            continue;
        }
        // `resumes` is the one edge the projection cannot rebuild: nothing in
        // `dispatch_run` records which run a resume continued, so the engine's
        // edge is the only record of it. Keep it while both ends are alive.
        if edge.kind == EdgeKind::Resumes
            && survivors.contains(edge.source.as_str())
            && survivors.contains(edge.target.as_str())
        {
            continue;
        }
        ops.push(GraphOp::RemoveEdge {
            id: edge.id.clone(),
        });
    }

    if cur.totals != want.totals {
        ops.push(GraphOp::Totals {
            totals: want.totals.clone(),
        });
    }

    ops
}

// ===========================================================================
// The live projector
// ===========================================================================

/// Where reconciled ops go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Publish {
    /// Through the event bus, so the front end sees a `gilde://graph/patch`.
    Bus,
    /// Straight into `GraphState` — the boot seed, before any window exists.
    Direct,
}

/// What one reprojection did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProjectionReport {
    pub nodes: usize,
    pub edges: usize,
    pub ops: usize,
}

/// Collects the input, projects it and publishes the difference.
pub struct GraphProjector {
    db: Db,
    graph: GraphHandle,
    bus: Option<EventBus>,
    pty: Option<Arc<PtyManager>>,
    watchdog: Option<Arc<Watchdog>>,
    sidecar: Option<Arc<SidecarManager>>,
    accounts: Arc<Vec<AccountSlot>>,
    /// Per-session output-rate ring, the source of the sparkline. Kept here
    /// rather than in the status engine because it is a *view* concern: the
    /// engine reports a rate, the graph remembers the shape of it.
    rates: Mutex<BTreeMap<String, VecDeque<f32>>>,
}

impl GraphProjector {
    pub fn new(db: Db, graph: GraphHandle) -> GraphProjector {
        GraphProjector {
            db,
            graph,
            bus: None,
            pty: None,
            watchdog: None,
            sidecar: None,
            accounts: Arc::new(Vec::new()),
            rates: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn with_events(mut self, bus: EventBus) -> GraphProjector {
        self.bus = Some(bus);
        self
    }

    pub fn with_pty(mut self, pty: Arc<PtyManager>) -> GraphProjector {
        self.pty = Some(pty);
        self
    }

    pub fn with_watchdog(mut self, watchdog: Arc<Watchdog>) -> GraphProjector {
        self.watchdog = Some(watchdog);
        self
    }

    pub fn with_sidecar(mut self, sidecar: Arc<SidecarManager>) -> GraphProjector {
        self.sidecar = Some(sidecar);
        self
    }

    pub fn with_accounts(mut self, accounts: Arc<Vec<AccountSlot>>) -> GraphProjector {
        self.accounts = accounts;
        self
    }

    pub fn graph(&self) -> &GraphHandle {
        &self.graph
    }

    /// Read every source once. The database half is a single hop onto the
    /// blocking pool; the rest is in-memory state that is already sampled.
    pub async fn collect(&self) -> Result<ProjectionInput> {
        let at = now_ms();
        let durable = self.db.read(read_durable).await?;
        let DurableRows {
            projects,
            worktrees,
            tasks,
            runs,
            aliases,
        } = durable;

        // The last watchdog sample — never a fresh `ps` (acceptance §2).
        let scan = self.watchdog.as_ref().and_then(|w| w.latest_scan());
        let budget = self
            .watchdog
            .as_ref()
            .map(|w| {
                let state = w.budget();
                BudgetFacts {
                    rss_bytes: state.rss_bytes,
                    physical_ram_bytes: state.physical_ram_bytes,
                    pressure: state.pressure_level,
                }
            })
            .unwrap_or_default();

        let sessions = self.sessions(scan.as_ref(), at);
        let orphans = scan
            .as_ref()
            .map(|scan| {
                scan.rows
                    .iter()
                    .filter_map(OrphanFacts::from_scan_row)
                    .collect()
            })
            .unwrap_or_default();

        Ok(ProjectionInput {
            at,
            projects,
            worktrees,
            sessions,
            tasks,
            runs,
            aliases,
            accounts: self.accounts.as_ref().clone(),
            orphans,
            budget,
            sidecar: self
                .sidecar
                .as_ref()
                .map(|s| s.status().state)
                .unwrap_or_default(),
        })
    }

    fn sessions(&self, scan: Option<&ScanResult>, at: i64) -> Vec<SessionFacts> {
        let Some(pty) = self.pty.as_ref() else {
            return Vec::new();
        };
        // One pass over the scan rows: RSS is summed over the whole process
        // group, because a session is its subtree, not its argv[0].
        let mut rss: HashMap<&str, (u64, f32)> = HashMap::new();
        if let Some(scan) = scan {
            for row in &scan.rows {
                let Some(session_id) = row
                    .owner
                    .as_ref()
                    .and_then(|owner| owner.session_id.as_deref())
                else {
                    continue;
                };
                let slot = rss.entry(session_id).or_insert((0, 0.0));
                slot.0 = slot.0.saturating_add(row.rss_bytes);
                slot.1 += row.cpu_pct;
            }
        }

        let mut out = Vec::new();
        let mut live = BTreeSet::new();
        for info in pty.list() {
            let report = pty.status(&info.id).ok();
            let status = report
                .as_ref()
                .map(|r| crate::status::evidence::node_status_of(&r.status))
                .unwrap_or(NodeStatus::Idle);
            let rate = report
                .as_ref()
                .map(|r| output_rate_of(&r.evidence))
                .unwrap_or(0.0);
            let mut facts = SessionFacts::new(&info.id, info.agent.key(), info.opened_at);
            facts.worktree_id = Some(info.worktree_id.clone()).filter(|w| !w.is_empty());
            facts.title = info.title.clone();
            facts.account_id = info.account_id.clone();
            facts.pid = info.pid;
            facts.pgid = info.pgid;
            facts.cwd = info.cwd.clone();
            facts.detached = info.detached;
            facts.status = status;
            facts.confidence = report.as_ref().map(|r| r.confidence);
            facts.evidence = report.as_ref().map(|r| r.evidence.clone());
            facts.attention = pty.status_engine().attention(&info.id);
            if let Some((bytes, cpu)) = rss.get(info.id.as_str()) {
                facts.rss_bytes = Some(*bytes);
                facts.cpu_pct = Some(*cpu);
            }
            facts.last_activity_at = report.as_ref().map(|r| r.since).filter(|s| *s > 0);
            facts.output_rate = Some(self.push_rate(&info.id, rate));
            live.insert(info.id.clone());
            out.push(facts);
        }
        self.forget_rates(&live);
        let _ = at;
        out
    }

    /// Append one sample to a session's ring and hand back the whole window.
    fn push_rate(&self, session_id: &str, rate: f32) -> Vec<f32> {
        let mut rates = self.rates.lock();
        let ring = rates.entry(session_id.to_string()).or_default();
        if ring.len() == RATE_SAMPLES {
            ring.pop_front();
        }
        ring.push_back(rate);
        ring.iter().copied().collect()
    }

    fn forget_rates(&self, live: &BTreeSet<String>) {
        let mut rates = self.rates.lock();
        rates.retain(|id, _| live.contains(id));
    }

    /// Project `input` and publish the difference. Returns what it did.
    pub fn apply(&self, input: &ProjectionInput, publish: Publish) -> ProjectionReport {
        let desired = project(input);
        let ops = {
            let current = self.graph.state().read();
            reconcile(&current, &desired, input.at)
        };
        let report = ProjectionReport {
            nodes: desired.node_count(),
            edges: desired.edge_count(),
            ops: ops.len(),
        };
        if ops.is_empty() {
            return report;
        }
        match (publish, self.bus.as_ref()) {
            (Publish::Bus, Some(bus)) => {
                for op in ops {
                    if op.is_metric_only() {
                        bus.push_metric(op);
                    } else {
                        bus.push(op);
                    }
                }
            }
            _ => {
                self.graph.state().write().apply_batch(ops, input.at);
            }
        }
        report
    }

    /// Collect + project + publish.
    pub async fn reproject(&self, publish: Publish) -> Result<ProjectionReport> {
        let input = self.collect().await?;
        Ok(self.apply(&input, publish))
    }

    /// The periodic pass. Everything the incremental producers cannot notice —
    /// a worktree that went dirty, a project that was added, an orphan the
    /// scanner just found, the totals — arrives here.
    pub fn spawn(self: &Arc<Self>, every: Duration) -> tauri::async_runtime::JoinHandle<()> {
        let projector = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            let mut ticker = tokio::time::interval(every);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                match projector.reproject(Publish::Bus).await {
                    Ok(report) if report.ops > 0 => {
                        tracing::debug!(target: "graph", ops = report.ops, nodes = report.nodes, "reprojected")
                    }
                    Ok(_) => {}
                    // A projection that cannot read the database is a stale
                    // canvas, not a broken app: try again on the next tick.
                    Err(err) => {
                        tracing::debug!(target: "graph", error = %err, "reprojection failed")
                    }
                }
            }
        })
    }
}

/// Highest observed output rate in a status report, in bytes/s. `outputBurst`
/// is the only evidence that carries one; quiet means zero, which is exactly
/// what the sparkline should draw.
fn output_rate_of(evidence: &[StatusEvidence]) -> f32 {
    evidence
        .iter()
        .filter_map(|e| match e {
            StatusEvidence::OutputBurst { bytes_per_s } => Some(*bytes_per_s as f32),
            _ => None,
        })
        .fold(0.0f32, f32::max)
}

struct DurableRows {
    projects: Vec<ProjectRow>,
    worktrees: Vec<WorktreeRow>,
    tasks: Vec<DispatchTaskRow>,
    runs: Vec<DispatchRunRow>,
    aliases: Vec<AliasFacts>,
}

fn read_durable(conn: &Connection) -> Result<DurableRows> {
    let projects = dao::project::list(conn)?;
    let worktrees = dao::worktree::list_live(conn)?;
    let tasks = dao::dispatch::task_list(conn, TASK_WINDOW)?;
    let mut runs = Vec::new();
    for task in &tasks {
        runs.extend(dao::dispatch::runs_of_task(conn, &task.id)?);
    }
    let mut aliases = Vec::new();
    for worktree in &worktrees {
        for row in dao::worktree_alias::list_by_worktree(conn, &worktree.id)? {
            aliases.push(AliasFacts {
                worktree_id: row.worktree_id,
                alias_path: row.alias_path,
            });
        }
    }
    Ok(DurableRows {
        projects,
        worktrees,
        tasks,
        runs,
        aliases,
    })
}

// ===========================================================================
// Manual layout persistence (`settings_kv`)
// ===========================================================================

/// One node the user dragged. `pinned` survives topology changes — the point of
/// moving a node by hand is that it stays where it was put (DESIGN §9-6).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodePosition {
    pub x: f64,
    pub y: f64,
    #[serde(default)]
    pub pinned: bool,
}

/// The manual positions for one project's canvas.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphLayout {
    #[serde(default)]
    pub positions: BTreeMap<String, NodePosition>,
    #[serde(default)]
    pub updated_at: i64,
}

/// Layouts are per project so switching projects does not scramble the canvas.
/// `None` is the "everything" canvas, which is what the graph shows by default.
pub fn layout_key(project_id: Option<&str>) -> String {
    format!("graph.layout:{}", project_id.unwrap_or("all"))
}

/// A layout with the entries a canvas cannot use removed: non-finite
/// coordinates, and anything past the cap (a runaway writer must not be able to
/// grow `settings_kv` without bound).
pub const MAX_PINNED_NODES: usize = 2_000;

pub fn sanitize_layout(mut layout: GraphLayout) -> GraphLayout {
    layout
        .positions
        .retain(|_, p| p.x.is_finite() && p.y.is_finite());
    while layout.positions.len() > MAX_PINNED_NODES {
        let Some(key) = layout.positions.keys().next_back().cloned() else {
            break;
        };
        layout.positions.remove(&key);
    }
    layout
}

pub fn layout_load(conn: &Connection, project_id: Option<&str>) -> Result<GraphLayout> {
    let raw = dao::settings_kv::get(conn, &layout_key(project_id))?;
    Ok(raw
        .and_then(|value| serde_json::from_value::<GraphLayout>(value).ok())
        .map(sanitize_layout)
        .unwrap_or_default())
}

pub fn layout_store(conn: &Connection, project_id: Option<&str>, layout: &GraphLayout) -> Result<()> {
    let value = serde_json::to_value(layout).map_err(crate::error::GildeError::internal)?;
    dao::settings_kv::set(conn, &layout_key(project_id), &value)
}

/// Manual node positions for a project's canvas.
#[tauri::command]
pub async fn graph_layout_get(
    project_id: Option<String>,
    db: State<'_, DbState>,
) -> CmdResult<GraphLayout> {
    let db = db.inner().clone();
    Ok(db
        .read(move |conn| layout_load(conn, project_id.as_deref()))
        .await?)
}

/// Persist the positions the user dragged. Sent debounced by the canvas.
#[tauri::command]
pub async fn graph_layout_save(
    project_id: Option<String>,
    layout: GraphLayout,
    db: State<'_, DbState>,
) -> CmdResult<GraphLayout> {
    let db = db.inner().clone();
    let mut layout = sanitize_layout(layout);
    layout.updated_at = now_ms();
    let stored = layout.clone();
    db.write(move |conn| layout_store(conn, project_id.as_deref(), &stored))
        .await?;
    Ok(layout)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::graph::GraphSnapshot;
    use crate::store::models::WorktreeOrigin;

    fn project_row(id: &str) -> ProjectRow {
        ProjectRow {
            id: id.to_string(),
            path: format!("/repos/{id}"),
            canonical_path: format!("/repos/{id}"),
            repo_slug: format!("slug-{id}"),
            name: format!("name-{id}"),
            color: None,
            default_base_ref: "main".into(),
            added_at: 1,
            last_opened_at: None,
        }
    }

    fn worktree_row(id: &str, project_id: &str) -> WorktreeRow {
        WorktreeRow {
            id: id.to_string(),
            project_id: project_id.to_string(),
            slug: format!("wt-{id}"),
            branch: format!("gilde/{id}"),
            path: format!("/wt/{id}"),
            base_sha: None,
            origin: WorktreeOrigin::Created,
            created_at: 1,
            removed_at: None,
            locked: false,
            verified_at: Some(2),
            post_create_cmd: None,
            cached_dirty: 0,
            cached_added: 0,
            cached_removed: 0,
            cached_ahead: 0,
            cached_behind: 0,
        }
    }

    fn task_row(id: &str, project_id: &str, status: &str) -> DispatchTaskRow {
        DispatchTaskRow {
            id: id.to_string(),
            project_id: Some(project_id.to_string()),
            title: format!("task {id}"),
            prompt: "p".into(),
            task_slug: format!("slug-{id}"),
            base_ref: None,
            worktree_strategy: "perTarget".into(),
            permission_mode: None,
            timeout_ms: None,
            created_at: 10,
            finished_at: None,
            status: status.to_string(),
        }
    }

    fn run_row(id: &str, task_id: &str, worktree_id: Option<&str>, status: &str) -> DispatchRunRow {
        DispatchRunRow {
            id: id.to_string(),
            task_id: task_id.to_string(),
            vendor: "claude".into(),
            model_requested: Some("opus".into()),
            model_actual: None,
            account_id: None,
            worktree_id: worktree_id.map(str::to_string),
            spawn_id: None,
            run_id: None,
            conversation_id: None,
            status: status.to_string(),
            note: None,
            route_note: None,
            text: None,
            usage_json: None,
            cost_usd: None,
            started_at: Some(20),
            finished_at: None,
            error: None,
        }
    }

    fn account(id: &str, vendor: &str) -> AccountSlot {
        AccountSlot {
            id: id.to_string(),
            vendor: vendor.to_string(),
            label: format!("label-{id}"),
            config_dir: None,
            model_hint: None,
            source: crate::store::models::AccountSource::Ambient,
            enabled: true,
            is_default: false,
            logged_in: true,
            last_seen_at: None,
            last_quota_at: None,
        }
    }

    /// Three projects, eight worktrees, six sessions — the shape acceptance §2
    /// puts a 20 ms budget on, plus a dispatch fan-out and an orphan.
    fn realistic_input(at: i64) -> ProjectionInput {
        let projects: Vec<ProjectRow> = (0..3).map(|i| project_row(&format!("p{i}"))).collect();
        let worktrees: Vec<WorktreeRow> = (0..8)
            .map(|i| worktree_row(&format!("w{i}"), &format!("p{}", i % 3)))
            .collect();
        let sessions: Vec<SessionFacts> = (0..6)
            .map(|i| {
                let mut facts = SessionFacts::new(format!("s{i}"), "claude", at - 60_000);
                facts.worktree_id = Some(format!("w{i}"));
                facts.title = format!("session {i}");
                facts.cwd = format!("/wt/w{i}");
                facts.status = if i % 2 == 0 {
                    NodeStatus::Working
                } else {
                    NodeStatus::Idle
                };
                facts.rss_bytes = Some(1_000_000 * (i as u64 + 1));
                facts.output_rate = Some(vec![1.0; RATE_SAMPLES]);
                facts
            })
            .collect();
        ProjectionInput {
            at,
            projects,
            worktrees,
            sessions,
            tasks: vec![task_row("t0", "p0", "running")],
            runs: vec![
                run_row("r0", "t0", Some("w0"), "running"),
                run_row("r1", "t0", Some("w1"), "done"),
            ],
            aliases: vec![AliasFacts {
                worktree_id: "w0".into(),
                alias_path: "/repos/p0/.claude/worktrees/w0".into(),
            }],
            accounts: vec![account("a0", "claude"), account("a1", "codex")],
            orphans: vec![OrphanFacts {
                pid: 4242,
                pgid: 4242,
                name: "claude".into(),
                argv_display: "claude --resume".into(),
                rss_bytes: 900_000_000,
                cpu_pct: 0.4,
                elapsed_secs: 900,
                worktree_id: Some("w3".into()),
                session_id: None,
                run_id: None,
            }],
            budget: BudgetFacts {
                rss_bytes: 4_000_000_000,
                physical_ram_bytes: 32_000_000_000,
                pressure: PressureLevel::Normal,
            },
            sidecar: SidecarState::Ready,
        }
    }

    fn ids(snapshot: &GraphSnapshot) -> Vec<&str> {
        snapshot.nodes.iter().map(|n| n.id.as_str()).collect()
    }

    #[test]
    fn node_ids_and_the_topology_hash_are_stable_across_calls() {
        let a = project(&realistic_input(1_000)).snapshot();
        let b = project(&realistic_input(9_999)).snapshot();
        assert_eq!(ids(&a), ids(&b), "the same entities produce the same ids");
        assert_eq!(
            a.topology_hash, b.topology_hash,
            "a later projection of the same entities must not move the layout"
        );
        assert!(a.nodes.iter().any(|n| n.id == "session:s0"));
        assert!(a.nodes.iter().any(|n| n.id == "worktree:w0"));
        assert!(a.nodes.iter().any(|n| n.id == "orphan:4242"));
    }

    #[test]
    fn the_projection_ranks_project_worktree_session_and_run() {
        let snapshot = project(&realistic_input(1_000)).snapshot();
        let find = |id: &str| snapshot.nodes.iter().find(|n| n.id == id).expect("node");
        assert_eq!(find("worktree:w0").parent_id.as_deref(), Some("project:p0"));
        assert_eq!(find("session:s0").parent_id.as_deref(), Some("worktree:w0"));
        assert_eq!(find("run:r0").parent_id.as_deref(), Some("worktree:w0"));
        assert_eq!(find("dispatch:t0").parent_id.as_deref(), Some("project:p0"));
        // The orphan cluster hangs off the root, not off the rank chain.
        assert_eq!(find("orphan:4242").parent_id, None);
        assert_eq!(find("account:a0").parent_id, None);
    }

    #[test]
    fn every_edge_kind_is_populated() {
        let mut input = realistic_input(1_000);
        // Give the session an account and an alias-shaped cwd so the two
        // cross-rank edges have something to describe.
        input.sessions[0].account_id = Some("a0".into());
        input.sessions[0].cwd = "/repos/p0/.claude/worktrees/w0/src".into();
        let snapshot = project(&input).snapshot();
        let kinds: BTreeSet<EdgeKind> = snapshot.edges.iter().map(|e| e.kind).collect();
        assert!(kinds.contains(&EdgeKind::Contains), "contains");
        assert!(kinds.contains(&EdgeKind::DispatchedTo), "dispatched-to");
        assert!(kinds.contains(&EdgeKind::RunsIn), "runs-in (orphan)");
        assert!(kinds.contains(&EdgeKind::Aliases), "aliases");
        assert!(kinds.contains(&EdgeKind::UsesAccount), "uses-account");
        // `resumes` is the engine's to write and the projection's to preserve.
        assert!(!kinds.contains(&EdgeKind::Resumes));
    }

    #[test]
    fn only_live_edges_animate() {
        let snapshot = project(&realistic_input(1_000)).snapshot();
        let edge = |id: &str| snapshot.edges.iter().find(|e| e.id == id).expect("edge");
        assert!(edge("contains:worktree:w0>session:s0").active, "working");
        assert!(!edge("contains:worktree:w1>session:s1").active, "idle");
        assert!(edge("dispatched-to:dispatch:t0>run:r0").active, "running");
        assert!(!edge("dispatched-to:dispatch:t0>run:r1").active, "done");
    }

    #[test]
    fn totals_count_what_the_status_bar_shows() {
        let totals = project(&realistic_input(1_000)).totals().clone();
        assert_eq!(totals.projects, 3);
        assert_eq!(totals.worktrees, 8);
        assert_eq!(totals.sessions, 6);
        assert_eq!(totals.sessions_working, 3);
        assert_eq!(totals.sessions_idle, 3);
        assert_eq!(totals.runs_active, 1);
        assert_eq!(totals.orphans, 1);
        assert_eq!(totals.sidecar_state, SidecarState::Ready);
        assert!((totals.rss_budget_percent - 12.5).abs() < 0.01);
    }

    #[test]
    fn account_nodes_appear_only_when_there_is_a_choice() {
        let mut input = realistic_input(1_000);
        input.accounts.truncate(1);
        input.sessions[0].account_id = Some("a0".into());
        let snapshot = project(&input).snapshot();
        assert!(!snapshot.nodes.iter().any(|n| n.kind == NodeKind::Account));
        assert!(
            !snapshot
                .edges
                .iter()
                .any(|e| e.kind == EdgeKind::UsesAccount),
            "no dangling edge to a node that is not drawn"
        );
    }

    #[test]
    fn reconciling_an_unchanged_projection_emits_nothing() {
        let input = realistic_input(1_000);
        let mut live = GraphState::new();
        let first = reconcile(&live, &project(&input), input.at);
        assert!(!first.is_empty());
        live.apply_batch(first, input.at);

        let again = reconcile(&live, &project(&input), input.at);
        assert!(
            again.is_empty(),
            "a second identical projection must be silent, got {again:?}"
        );
    }

    #[test]
    fn a_metrics_only_change_is_a_metric_patch_and_leaves_the_topology_alone() {
        let input = realistic_input(1_000);
        let mut live = GraphState::new();
        live.apply_batch(reconcile(&live, &project(&input), input.at), input.at);
        let before = live.topology_hash();

        let mut next = input.clone();
        next.at = 2_000;
        next.sessions[0].rss_bytes = Some(2_500_000_000);
        let ops = reconcile(&live, &project(&next), next.at);

        // The elapsed clock moves with `at` for every session and run, so more
        // than one node is touched — but every node op must be a metric patch.
        // `Totals` moves too: it carries the projection timestamp, which is the
        // heartbeat behind the "last updated N seconds ago" chip.
        assert!(!ops.is_empty());
        assert!(
            ops.iter()
                .all(|op| op.is_metric_only() || matches!(op, GraphOp::Totals { .. })),
            "expected only metric patches and totals, got {ops:?}"
        );
        let rss = ops.iter().find_map(|op| match op {
            GraphOp::PatchNode {
                id,
                metrics: Some(m),
                ..
            } if id == "session:s0" => Some(m.rss_bytes),
            _ => None,
        });
        assert_eq!(rss, Some(Some(2_500_000_000)));

        live.apply_batch(ops, next.at);
        assert_eq!(before, live.topology_hash(), "dagre must not re-run");
    }

    #[test]
    fn a_status_change_is_a_patch_not_a_replacement() {
        let input = realistic_input(1_000);
        let mut live = GraphState::new();
        live.apply_batch(reconcile(&live, &project(&input), input.at), input.at);

        let mut next = input.clone();
        next.sessions[1].status = NodeStatus::Waiting;
        let ops = reconcile(&live, &project(&next), next.at);
        let patched = ops.iter().any(|op| {
            matches!(op, GraphOp::PatchNode { id, status: Some(NodeStatus::Waiting), .. } if id == "session:s1")
        });
        assert!(patched, "status moved as a patchNode, got {ops:?}");
        assert!(!ops.iter().any(|op| matches!(op, GraphOp::UpsertNode { .. })));
    }

    #[test]
    fn evidence_and_rss_written_by_other_producers_are_carried_forward() {
        let mut input = realistic_input(1_000);
        input.sessions[0].evidence = Some(vec![StatusEvidence::Bell]);
        let mut live = GraphState::new();
        live.apply_batch(reconcile(&live, &project(&input), input.at), input.at);

        // A tick where the watchdog has not sampled and the status engine has
        // no report: neither must blank what is already on the node.
        let mut next = input.clone();
        next.sessions[0].rss_bytes = None;
        next.sessions[0].evidence = None;
        next.sessions[0].confidence = None;
        let ops = reconcile(&live, &project(&next), next.at);
        live.apply_batch(ops, next.at);

        let node = live.node("session:s0").expect("node");
        assert_eq!(node.metrics.rss_bytes, Some(1_000_000));
        assert_eq!(node.evidence.as_deref(), Some(&[StatusEvidence::Bell][..]));
    }

    #[test]
    fn a_finished_run_lingers_then_leaves_and_an_orphan_leaves_at_once() {
        let input = realistic_input(1_000);
        let mut live = GraphState::new();
        live.apply_batch(reconcile(&live, &project(&input), input.at), input.at);

        // Both are gone from the sources on the next pass.
        let mut next = input.clone();
        next.runs.retain(|r| r.id != "r1");
        next.orphans.clear();
        next.at = input.at + 1_000;
        let ops = reconcile(&live, &project(&next), next.at);
        assert!(
            ops.iter()
                .any(|op| matches!(op, GraphOp::RemoveNode { id } if id == "orphan:4242")),
            "an orphan that the scanner stopped seeing is removed immediately"
        );
        assert!(
            !ops.iter()
                .any(|op| matches!(op, GraphOp::RemoveNode { id } if id == "run:r1")),
            "a finished run stays readable for a while"
        );
        live.apply_batch(ops, next.at);

        let mut later = next.clone();
        later.at = input.at + TERMINAL_GRACE_MS + 1;
        let ops = reconcile(&live, &project(&later), later.at);
        assert!(
            ops.iter()
                .any(|op| matches!(op, GraphOp::RemoveNode { id } if id == "run:r1")),
            "and is dropped once the grace expires"
        );
    }

    #[test]
    fn a_resumes_edge_written_by_the_engine_survives_reprojection() {
        let input = realistic_input(1_000);
        let mut live = GraphState::new();
        live.apply_batch(reconcile(&live, &project(&input), input.at), input.at);
        live.apply(GraphOp::UpsertEdge {
            edge: GraphEdge::new(EdgeKind::Resumes, "run:r0", "run:r1"),
        });

        let ops = reconcile(&live, &project(&input), input.at);
        assert!(
            !ops.iter().any(|op| matches!(op, GraphOp::RemoveEdge { .. })),
            "the projection cannot rebuild `resumes`, so it must not delete it"
        );
    }

    #[test]
    fn a_removed_node_takes_its_edges_with_it() {
        let input = realistic_input(1_000);
        let mut live = GraphState::new();
        live.apply_batch(reconcile(&live, &project(&input), input.at), input.at);
        let before = live.edge_count();

        let mut next = input.clone();
        next.orphans.clear();
        next.at = input.at + 1;
        live.apply_batch(reconcile(&live, &project(&next), next.at), next.at);
        assert!(live.edge_count() < before);
        assert!(live.node("orphan:4242").is_none());
    }

    #[test]
    fn a_projection_of_three_projects_eight_worktrees_and_six_sessions_is_well_under_20ms() {
        let input = realistic_input(now_ms());
        let mut live = GraphState::new();
        live.apply_batch(reconcile(&live, &project(&input), input.at), input.at);

        // Ten passes so a single scheduling hiccup cannot decide the verdict:
        // the budget is per call.
        let started = std::time::Instant::now();
        const PASSES: u32 = 10;
        for _ in 0..PASSES {
            let desired = project(&input);
            let ops = reconcile(&live, &desired, input.at);
            std::hint::black_box(ops);
            std::hint::black_box(live.snapshot());
        }
        let per_call = started.elapsed() / PASSES;
        eprintln!("graph projection + reconcile + snapshot: {per_call:?} per call");
        assert!(
            per_call < Duration::from_millis(20),
            "graph projection + snapshot took {per_call:?} per call, budget is 20ms"
        );
    }

    #[test]
    fn masked_argv_and_no_prose_reach_the_graph() {
        let snapshot = project(&realistic_input(1_000)).snapshot();
        let orphan = snapshot
            .nodes
            .iter()
            .find(|n| n.kind == NodeKind::Orphan)
            .expect("orphan");
        // The scanner masked it; the projection passes it through verbatim.
        assert_eq!(orphan.sublabel.as_deref(), Some("claude --resume"));
        // Every label is an identifier, never a localized sentence.
        for node in &snapshot.nodes {
            assert!(!node.label.contains(' ') || node.kind == NodeKind::Dispatch);
        }
    }

    #[test]
    fn usage_json_becomes_token_and_cost_metrics() {
        let mut input = realistic_input(1_000);
        input.runs[0].usage_json = Some(
            r#"{"inputTokens":1200,"outputTokens":340,"cachedInputTokens":90,"costUsd":0.12}"#
                .into(),
        );
        let snapshot = project(&input).snapshot();
        let run = snapshot.nodes.iter().find(|n| n.id == "run:r0").unwrap();
        assert_eq!(run.metrics.input_tokens, Some(1_200));
        assert_eq!(run.metrics.output_tokens, Some(340));
        assert_eq!(run.metrics.cached_tokens, Some(90));
        assert_eq!(run.metrics.cost_usd, Some(0.12));
    }

    #[test]
    fn a_worktree_carries_its_git_statistics() {
        let mut input = realistic_input(1_000);
        input.worktrees[0].cached_dirty = 3;
        input.worktrees[0].cached_added = 412;
        input.worktrees[0].cached_removed = 38;
        input.worktrees[0].cached_ahead = 2;
        input.worktrees[0].locked = true;
        let snapshot = project(&input).snapshot();
        let node = snapshot.nodes.iter().find(|n| n.id == "worktree:w0").unwrap();
        assert_eq!(node.status, NodeStatus::Dirty);
        assert_eq!(node.metrics.insertions, Some(412));
        assert_eq!(node.metrics.deletions, Some(38));
        assert_eq!(node.metrics.dirty_files, Some(3));
        assert_eq!(node.metrics.ahead, Some(2));
        assert!(node.badges.contains(&NodeBadge::Dirty));
        assert!(node.badges.contains(&NodeBadge::Unmerged));
        assert!(node.badges.contains(&NodeBadge::Locked));
    }

    #[test]
    fn path_containment_respects_separators() {
        assert!(path_is_under("/a/b/c", "/a/b"));
        assert!(path_is_under("/a/b", "/a/b"));
        assert!(path_is_under("/a/b/c", "/a/b/"));
        assert!(!path_is_under("/a/bc", "/a/b"));
        assert!(!path_is_under("/a", "/a/b"));
        assert!(!path_is_under("/a/b", ""));
    }

    #[test]
    fn the_output_rate_ring_keeps_sixty_samples_and_forgets_dead_sessions() {
        let db = Db::open_in_memory().expect("db");
        let projector = GraphProjector::new(db, GraphHandle::new());
        let mut last = Vec::new();
        for i in 0..(RATE_SAMPLES as u32 + 20) {
            last = projector.push_rate("s0", i as f32);
        }
        assert_eq!(last.len(), RATE_SAMPLES);
        assert_eq!(last[RATE_SAMPLES - 1], (RATE_SAMPLES as u32 + 19) as f32);

        projector.forget_rates(&BTreeSet::new());
        assert_eq!(projector.push_rate("s0", 1.0).len(), 1);
    }

    #[test]
    fn a_status_report_without_a_burst_samples_zero() {
        assert_eq!(output_rate_of(&[]), 0.0);
        assert_eq!(output_rate_of(&[StatusEvidence::Bell]), 0.0);
        assert_eq!(
            output_rate_of(&[
                StatusEvidence::OutputBurst { bytes_per_s: 4_096 },
                StatusEvidence::OutputBurst { bytes_per_s: 128 },
            ]),
            4_096.0
        );
    }

    #[test]
    fn the_layout_round_trips_through_settings_kv() {
        let db = Db::open_in_memory().expect("db");
        db.blocking(|conn| {
            assert_eq!(layout_load(conn, Some("p0"))?, GraphLayout::default());
            let layout = GraphLayout {
                positions: BTreeMap::from([
                    (
                        "session:s0".to_string(),
                        NodePosition {
                            x: 12.0,
                            y: -4.5,
                            pinned: true,
                        },
                    ),
                    (
                        "worktree:w0".to_string(),
                        NodePosition {
                            x: f64::NAN,
                            y: 0.0,
                            pinned: true,
                        },
                    ),
                ]),
                updated_at: 42,
            };
            layout_store(conn, Some("p0"), &sanitize_layout(layout))?;

            let loaded = layout_load(conn, Some("p0"))?;
            assert_eq!(loaded.positions.len(), 1, "NaN entries are dropped");
            assert_eq!(loaded.positions["session:s0"].x, 12.0);
            assert!(loaded.positions["session:s0"].pinned);
            // Layouts are per project: another project starts empty.
            assert_eq!(layout_load(conn, Some("p1"))?, GraphLayout::default());
            assert_eq!(layout_load(conn, None)?, GraphLayout::default());
            Ok(())
        })
        .expect("layout round trip");
    }

    #[test]
    fn the_layout_key_is_scoped_per_project() {
        assert_eq!(layout_key(Some("p0")), "graph.layout:p0");
        assert_eq!(layout_key(None), "graph.layout:all");
    }

    #[test]
    fn an_empty_projection_is_an_empty_graph_never_a_demo_node() {
        let state = project(&ProjectionInput::default());
        assert_eq!(state.node_count(), 0);
        assert_eq!(state.edge_count(), 0);
    }

    #[test]
    fn a_projector_without_a_pty_manager_reports_no_sessions() {
        let db = Db::open_in_memory().expect("db");
        let projector = GraphProjector::new(db, GraphHandle::new());
        assert!(projector.sessions(None, 1).is_empty());
    }

    #[test]
    fn apply_writes_straight_into_the_graph_when_there_is_no_bus() {
        let db = Db::open_in_memory().expect("db");
        let graph = GraphHandle::new();
        let projector = GraphProjector::new(db, graph.clone());
        let input = realistic_input(1_000);

        let report = projector.apply(&input, Publish::Bus);
        assert!(report.ops > 0);
        assert_eq!(graph.state().read().node_count(), report.nodes);

        // Idempotent: the second pass has nothing left to say.
        let again = projector.apply(&input, Publish::Bus);
        assert_eq!(again.ops, 0);
    }
}
