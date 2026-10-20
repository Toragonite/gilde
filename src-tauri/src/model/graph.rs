//! ★ SINGLE OWNER: T04. Node / edge / patch shapes for the live status graph.
//!
//! DESIGN §9-5. The sidebar tree, the tab status pills, the dispatch result
//! cards, the Inspector and the React Flow canvas are five renderings of *one*
//! `GraphSnapshot`. They cannot structurally disagree because there is only one
//! projection.
//!
//! Every type here is mirrored 1:1 by `src/lib/ipc.ts`. Do not redefine these
//! shapes anywhere else; if a new node kind or op is needed, change this file
//! first and build on that commit.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::model::status::{NodeBadge, NodeMetrics, StatusEvidence};

/// Rank in the layout: `account? → project → worktree → session|dispatch → run`.
/// `orphan` is a cluster hung off the root, not part of the rank chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NodeKind {
    Project,
    Worktree,
    Session,
    Dispatch,
    Run,
    Account,
    Orphan,
}

impl NodeKind {
    /// The `"<prefix>:"` used in node ids.
    pub fn prefix(self) -> &'static str {
        match self {
            NodeKind::Project => "project",
            NodeKind::Worktree => "worktree",
            NodeKind::Session => "session",
            NodeKind::Dispatch => "dispatch",
            NodeKind::Run => "run",
            NodeKind::Account => "account",
            NodeKind::Orphan => "orphan",
        }
    }

    pub fn from_prefix(prefix: &str) -> Option<Self> {
        Some(match prefix {
            "project" => NodeKind::Project,
            "worktree" => NodeKind::Worktree,
            "session" => NodeKind::Session,
            "dispatch" => NodeKind::Dispatch,
            "run" => NodeKind::Run,
            "account" => NodeKind::Account,
            "orphan" => NodeKind::Orphan,
            _ => return None,
        })
    }
}

/// One vocabulary of status for every surface. Session-ish states
/// (`idle`..`exited`), worktree-ish states (`clean`/`dirty`) and the neutral
/// `ok` all live here so the colour tokens are shared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NodeStatus {
    Idle,
    Working,
    Waiting,
    Stalled,
    Error,
    Done,
    Exited,
    Orphan,
    Detached,
    Clean,
    Dirty,
    Ok,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EdgeKind {
    Contains,
    RunsIn,
    DispatchedTo,
    Resumes,
    Aliases,
    UsesAccount,
}

/// Supervision facts a node carries for the Inspector. Kept deliberately small:
/// anything larger is fetched on demand by a command.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pgid: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphNode {
    /// `"session:0192f…"` — kind prefix plus the entity id.
    pub id: String,
    pub kind: NodeKind,
    #[serde(default)]
    pub parent_id: Option<String>,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sublabel: Option<String>,
    pub status: NodeStatus,
    /// 0..1. Below 0.5 the UI draws a hollow ring — "we are guessing too".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<Vec<StatusEvidence>>,
    #[serde(default)]
    pub badges: Vec<NodeBadge>,
    #[serde(default)]
    pub metrics: NodeMetrics,
    #[serde(default)]
    pub meta: NodeMeta,
    pub updated_at: i64,
}

impl GraphNode {
    /// Minimal constructor; callers fill in the optional fields.
    pub fn new(
        id: impl Into<String>,
        kind: NodeKind,
        label: impl Into<String>,
        status: NodeStatus,
        updated_at: i64,
    ) -> Self {
        Self {
            id: id.into(),
            kind,
            parent_id: None,
            label: label.into(),
            sublabel: None,
            status,
            confidence: None,
            evidence: None,
            badges: Vec::new(),
            metrics: NodeMetrics::default(),
            meta: NodeMeta::default(),
            updated_at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphEdge {
    pub id: String,
    pub source: String,
    pub target: String,
    pub kind: EdgeKind,
    /// Only active edges animate. Everything else is static, on purpose.
    pub active: bool,
}

impl GraphEdge {
    /// Deterministic edge id so repeated projections do not churn the topology
    /// hash: `"<kind>:<source>→<target>"`.
    pub fn edge_id(kind: EdgeKind, source: &str, target: &str) -> String {
        let kind = match kind {
            EdgeKind::Contains => "contains",
            EdgeKind::RunsIn => "runs-in",
            EdgeKind::DispatchedTo => "dispatched-to",
            EdgeKind::Resumes => "resumes",
            EdgeKind::Aliases => "aliases",
            EdgeKind::UsesAccount => "uses-account",
        };
        format!("{kind}:{source}>{target}")
    }

    pub fn new(kind: EdgeKind, source: impl Into<String>, target: impl Into<String>) -> Self {
        let source = source.into();
        let target = target.into();
        Self {
            id: Self::edge_id(kind, &source, &target),
            source,
            target,
            kind,
            active: false,
        }
    }
}

/// System memory pressure, as read from `kern.memorystatus_vm_pressure_level`
/// (or the documented fallbacks). Never triggers an automatic kill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PressureLevel {
    #[default]
    Unknown,
    Normal,
    Warn,
    Critical,
}

/// Lifecycle of the Node sidecar. The PTY lane never depends on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SidecarState {
    Starting,
    Ready,
    Degraded,
    #[default]
    Down,
    Restarting,
}

/// Aggregate counters that drive the status bar and the graph header.
///
/// DESIGN does not enumerate `Totals`; this shape is T04's decision and is
/// mirrored verbatim in `src/lib/ipc.ts`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Totals {
    pub projects: u32,
    pub worktrees: u32,
    pub sessions: u32,
    pub sessions_working: u32,
    pub sessions_waiting: u32,
    pub sessions_idle: u32,
    pub runs_active: u32,
    pub orphans: u32,
    /// Summed RSS of our sessions plus detected residue.
    pub rss_bytes: u64,
    /// `rss_bytes` as a percentage of physical RAM.
    pub rss_budget_percent: f32,
    pub pressure_level: PressureLevel,
    pub sidecar_state: SidecarState,
    /// Unix millis at which these totals were computed.
    pub at: i64,
}

/// The whole graph, at one revision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphSnapshot {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    /// sha256 over the sorted node ids and edge ids. Changes only when the node
    /// or edge *set* changes — this is what keeps dagre from re-running on a
    /// 1 Hz RSS update.
    pub topology_hash: String,
    pub totals: Totals,
    pub rev: u64,
    pub at: i64,
}

/// One mutation of the graph. Ops are applied in publication order on both
/// sides of the IPC boundary, which is what makes patch/snapshot convergence
/// testable (DESIGN R18).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum GraphOp {
    #[serde(rename_all = "camelCase")]
    UpsertNode { node: GraphNode },
    #[serde(rename_all = "camelCase")]
    PatchNode {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<NodeStatus>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        confidence: Option<f32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        evidence: Option<Vec<StatusEvidence>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        badges: Option<Vec<NodeBadge>>,
        /// Field-wise merge, not replacement (see [`NodeMetrics::merge_from`]).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metrics: Option<NodeMetrics>,
    },
    #[serde(rename_all = "camelCase")]
    RemoveNode { id: String },
    #[serde(rename_all = "camelCase")]
    UpsertEdge { edge: GraphEdge },
    #[serde(rename_all = "camelCase")]
    PatchEdge { id: String, active: bool },
    #[serde(rename_all = "camelCase")]
    RemoveEdge { id: String },
    #[serde(rename_all = "camelCase")]
    Totals { totals: Totals },
}

impl GraphOp {
    /// A metric-only op is throttled onto the 1 s lane by the coalescer.
    pub fn is_metric_only(&self) -> bool {
        matches!(
            self,
            GraphOp::PatchNode {
                status: None,
                confidence: None,
                evidence: None,
                badges: None,
                metrics: Some(_),
                ..
            }
        )
    }

    /// The node id this op targets, when it targets one.
    pub fn node_id(&self) -> Option<&str> {
        match self {
            GraphOp::UpsertNode { node } => Some(&node.id),
            GraphOp::PatchNode { id, .. } | GraphOp::RemoveNode { id } => Some(id),
            _ => None,
        }
    }
}

/// `gilde://graph/patch` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphPatch {
    pub rev: u64,
    pub ops: Vec<GraphOp>,
}

/// The normalised store. `BTreeMap` (not `HashMap`) so iteration order — and
/// therefore the topology hash and the serialized snapshot — is deterministic.
#[derive(Debug, Clone, Default)]
pub struct GraphState {
    nodes: BTreeMap<String, GraphNode>,
    edges: BTreeMap<String, GraphEdge>,
    totals: Totals,
    rev: u64,
    at: i64,
}

impl GraphState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn rev(&self) -> u64 {
        self.rev
    }

    pub fn at(&self) -> i64 {
        self.at
    }

    pub fn node(&self, id: &str) -> Option<&GraphNode> {
        self.nodes.get(id)
    }

    pub fn edge(&self, id: &str) -> Option<&GraphEdge> {
        self.edges.get(id)
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    pub fn totals(&self) -> &Totals {
        &self.totals
    }

    /// Apply one op. Deliberately does **not** touch `rev` or `updated_at`:
    /// the revision is bumped once per published patch by [`Self::apply_batch`],
    /// and `updated_at` is only ever set by the producer of a node so that a
    /// client replaying the same ops reaches a byte-identical state.
    pub fn apply(&mut self, op: GraphOp) {
        match op {
            GraphOp::UpsertNode { node } => {
                self.nodes.insert(node.id.clone(), node);
            }
            GraphOp::PatchNode {
                id,
                status,
                confidence,
                evidence,
                badges,
                metrics,
            } => {
                if let Some(node) = self.nodes.get_mut(&id) {
                    if let Some(status) = status {
                        node.status = status;
                    }
                    if let Some(confidence) = confidence {
                        node.confidence = Some(confidence);
                    }
                    if let Some(evidence) = evidence {
                        node.evidence = Some(evidence);
                    }
                    if let Some(badges) = badges {
                        node.badges = badges;
                    }
                    if let Some(metrics) = metrics {
                        node.metrics.merge_from(&metrics);
                    }
                }
                // A patch for an unknown node is a no-op on both sides. Events
                // are idempotent; a lost patch degrades to "briefly stale".
            }
            GraphOp::RemoveNode { id } => {
                self.nodes.remove(&id);
                self.edges
                    .retain(|_, edge| edge.source != id && edge.target != id);
            }
            GraphOp::UpsertEdge { edge } => {
                self.edges.insert(edge.id.clone(), edge);
            }
            GraphOp::PatchEdge { id, active } => {
                if let Some(edge) = self.edges.get_mut(&id) {
                    edge.active = active;
                }
            }
            GraphOp::RemoveEdge { id } => {
                self.edges.remove(&id);
            }
            GraphOp::Totals { totals } => {
                self.totals = totals;
            }
        }
    }

    /// Apply a published batch atomically and bump the revision exactly once.
    /// Returns the new revision, which becomes `GraphPatch::rev`.
    pub fn apply_batch(&mut self, ops: impl IntoIterator<Item = GraphOp>, at: i64) -> u64 {
        for op in ops {
            self.apply(op);
        }
        self.rev = self.rev.wrapping_add(1);
        self.at = at;
        self.rev
    }

    /// Used by a client replaying patches: adopt the patch revision verbatim.
    pub fn set_rev(&mut self, rev: u64) {
        self.rev = rev;
    }

    pub fn set_at(&mut self, at: i64) {
        self.at = at;
    }

    /// sha256 over the sorted node ids followed by the sorted edge ids.
    /// Insensitive to status, metrics, badges and labels by construction.
    pub fn topology_hash(&self) -> String {
        let mut hasher = Sha256::new();
        for id in self.nodes.keys() {
            hasher.update(b"n:");
            hasher.update(id.as_bytes());
            hasher.update(b"\n");
        }
        for id in self.edges.keys() {
            hasher.update(b"e:");
            hasher.update(id.as_bytes());
            hasher.update(b"\n");
        }
        hex::encode(hasher.finalize())
    }

    pub fn snapshot(&self) -> GraphSnapshot {
        GraphSnapshot {
            nodes: self.nodes.values().cloned().collect(),
            edges: self.edges.values().cloned().collect(),
            topology_hash: self.topology_hash(),
            totals: self.totals.clone(),
            rev: self.rev,
            at: self.at,
        }
    }

    /// Replace the whole state from a snapshot (`gilde://graph/resync`).
    pub fn load_snapshot(&mut self, snapshot: GraphSnapshot) {
        self.nodes = snapshot
            .nodes
            .into_iter()
            .map(|node| (node.id.clone(), node))
            .collect();
        self.edges = snapshot
            .edges
            .into_iter()
            .map(|edge| (edge.id.clone(), edge))
            .collect();
        self.totals = snapshot.totals;
        self.rev = snapshot.rev;
        self.at = snapshot.at;
    }
}

/// Managed-state handle so commands can reach the graph without depending on
/// the full `AppState` (which T29 assembles).
#[derive(Clone)]
pub struct GraphHandle(pub std::sync::Arc<parking_lot::RwLock<GraphState>>);

impl GraphHandle {
    pub fn new() -> Self {
        Self(std::sync::Arc::new(parking_lot::RwLock::new(
            GraphState::new(),
        )))
    }

    pub fn from_state(state: std::sync::Arc<parking_lot::RwLock<GraphState>>) -> Self {
        Self(state)
    }

    /// The shared state. Deliberately **not** named `inner`: `tauri::State`
    /// already has an `inner()`, and `State<GraphHandle>::inner()` would shadow
    /// it through `Deref` at every call site.
    pub fn state(&self) -> &std::sync::Arc<parking_lot::RwLock<GraphState>> {
        &self.0
    }

    pub fn snapshot(&self) -> GraphSnapshot {
        self.0.read().snapshot()
    }
}

impl Default for GraphHandle {
    fn default() -> Self {
        Self::new()
    }
}

/// Split `"session:0192f…"` into its kind and entity id.
pub fn parse_node_id(node_id: &str) -> Option<(NodeKind, &str)> {
    let (prefix, rest) = node_id.split_once(':')?;
    let kind = NodeKind::from_prefix(prefix)?;
    if rest.is_empty() {
        return None;
    }
    Some((kind, rest))
}

/// Build `"<kind>:<entity id>"`.
pub fn node_id(kind: NodeKind, entity_id: &str) -> String {
    format!("{}:{}", kind.prefix(), entity_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::status::NodeMetrics;

    fn node(id: &str, kind: NodeKind) -> GraphNode {
        GraphNode::new(id, kind, id, NodeStatus::Idle, 1)
    }

    #[test]
    fn node_ids_round_trip() {
        assert_eq!(node_id(NodeKind::Session, "abc"), "session:abc");
        assert_eq!(
            parse_node_id("session:abc"),
            Some((NodeKind::Session, "abc"))
        );
        assert_eq!(parse_node_id("nope:abc"), None);
        assert_eq!(parse_node_id("session:"), None);
        assert_eq!(parse_node_id("session"), None);
    }

    #[test]
    fn removing_a_node_removes_its_edges() {
        let mut state = GraphState::new();
        state.apply(GraphOp::UpsertNode {
            node: node("worktree:w", NodeKind::Worktree),
        });
        state.apply(GraphOp::UpsertNode {
            node: node("session:s", NodeKind::Session),
        });
        state.apply(GraphOp::UpsertEdge {
            edge: GraphEdge::new(EdgeKind::Contains, "worktree:w", "session:s"),
        });
        assert_eq!(state.edge_count(), 1);
        state.apply(GraphOp::RemoveNode {
            id: "session:s".into(),
        });
        assert_eq!(state.edge_count(), 0);
    }

    #[test]
    fn topology_hash_ignores_status_and_metrics() {
        let mut state = GraphState::new();
        for i in 0..8 {
            state.apply(GraphOp::UpsertNode {
                node: node(&format!("session:{i}"), NodeKind::Session),
            });
        }
        let before = state.topology_hash();

        for i in 0..1_000u32 {
            state.apply(GraphOp::PatchNode {
                id: format!("session:{}", i % 8),
                status: Some(if i % 2 == 0 {
                    NodeStatus::Working
                } else {
                    NodeStatus::Idle
                }),
                confidence: Some(0.9),
                evidence: None,
                badges: Some(vec![NodeBadge::Attention]),
                metrics: Some(NodeMetrics {
                    rss_bytes: Some(u64::from(i) * 1024),
                    ..Default::default()
                }),
            });
        }
        assert_eq!(before, state.topology_hash());

        // Upserting an *existing* node id must not move the hash either.
        state.apply(GraphOp::UpsertNode {
            node: node("session:0", NodeKind::Session),
        });
        assert_eq!(before, state.topology_hash());

        // Adding a node must move it.
        state.apply(GraphOp::UpsertNode {
            node: node("session:9", NodeKind::Session),
        });
        assert_ne!(before, state.topology_hash());
    }

    #[test]
    fn patching_an_unknown_node_is_a_no_op() {
        let mut state = GraphState::new();
        state.apply(GraphOp::PatchNode {
            id: "session:ghost".into(),
            status: Some(NodeStatus::Working),
            confidence: None,
            evidence: None,
            badges: None,
            metrics: None,
        });
        assert_eq!(state.node_count(), 0);
    }

    #[test]
    fn metric_only_ops_are_detected() {
        let metric = GraphOp::PatchNode {
            id: "session:s".into(),
            status: None,
            confidence: None,
            evidence: None,
            badges: None,
            metrics: Some(NodeMetrics {
                rss_bytes: Some(1),
                ..Default::default()
            }),
        };
        assert!(metric.is_metric_only());

        let structural = GraphOp::PatchNode {
            id: "session:s".into(),
            status: Some(NodeStatus::Working),
            confidence: None,
            evidence: None,
            badges: None,
            metrics: Some(NodeMetrics::default()),
        };
        assert!(!structural.is_metric_only());
    }

    #[test]
    fn ops_serialize_with_an_op_tag() {
        let json = serde_json::to_value(GraphOp::PatchEdge {
            id: "contains:a>b".into(),
            active: true,
        })
        .unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "op": "patchEdge", "id": "contains:a>b", "active": true })
        );

        let json = serde_json::to_value(GraphOp::RemoveNode { id: "x".into() }).unwrap();
        assert_eq!(json, serde_json::json!({ "op": "removeNode", "id": "x" }));

        let json = serde_json::to_value(GraphOp::PatchNode {
            id: "session:s".into(),
            status: Some(NodeStatus::Waiting),
            confidence: None,
            evidence: None,
            badges: None,
            metrics: None,
        })
        .unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "op": "patchNode", "id": "session:s", "status": "waiting" })
        );
    }

    #[test]
    fn snapshot_round_trips_through_load() {
        let mut state = GraphState::new();
        state.apply(GraphOp::UpsertNode {
            node: node("project:p", NodeKind::Project),
        });
        state.apply(GraphOp::UpsertEdge {
            edge: GraphEdge::new(EdgeKind::Contains, "project:p", "worktree:w"),
        });
        state.apply_batch(std::iter::empty(), 1234);

        let snapshot = state.snapshot();
        let mut client = GraphState::new();
        client.load_snapshot(snapshot.clone());
        assert_eq!(
            serde_json::to_vec(&client.snapshot()).unwrap(),
            serde_json::to_vec(&snapshot).unwrap()
        );
    }

    /// Guards the `src/lib/ipc.ts` mirror: if a field is added, renamed or
    /// un-camel-cased here, this fails and the TS contract must move with it.
    ///
    /// `serde_json::Map` is a `BTreeMap`, so the key lists compare sorted.
    #[test]
    fn snapshot_wire_keys_match_the_ts_contract() {
        fn keys(value: &serde_json::Value) -> Vec<&str> {
            value.as_object().unwrap().keys().map(String::as_str).collect()
        }
        fn sorted(mut expected: Vec<&str>) -> Vec<&str> {
            expected.sort_unstable();
            expected
        }

        let mut state = GraphState::new();
        let mut n = node("session:s", NodeKind::Session);
        n.parent_id = Some("worktree:w".into());
        n.sublabel = Some("opus-4-8".into());
        n.confidence = Some(0.9);
        n.evidence = Some(vec![]);
        n.badges = vec![NodeBadge::Attention];
        n.metrics.rss_bytes = Some(1);
        n.meta.vendor = Some("claude".into());
        state.apply(GraphOp::UpsertNode { node: n });
        state.apply(GraphOp::UpsertEdge {
            edge: GraphEdge::new(EdgeKind::Contains, "worktree:w", "session:s"),
        });

        let json = serde_json::to_value(state.snapshot()).unwrap();
        assert_eq!(
            keys(&json),
            sorted(vec!["nodes", "edges", "topologyHash", "totals", "rev", "at"])
        );
        assert_eq!(
            keys(&json["nodes"][0]),
            sorted(vec![
                "id",
                "kind",
                "parentId",
                "label",
                "sublabel",
                "status",
                "confidence",
                "evidence",
                "badges",
                "metrics",
                "meta",
                "updatedAt",
            ])
        );
        assert_eq!(
            keys(&json["edges"][0]),
            sorted(vec!["id", "source", "target", "kind", "active"])
        );
        assert_eq!(
            keys(&json["totals"]),
            sorted(vec![
                "projects",
                "worktrees",
                "sessions",
                "sessionsWorking",
                "sessionsWaiting",
                "sessionsIdle",
                "runsActive",
                "orphans",
                "rssBytes",
                "rssBudgetPercent",
                "pressureLevel",
                "sidecarState",
                "at",
            ])
        );

        // A node with nothing optional set stays compact on the wire.
        let bare = serde_json::to_value(node("project:p", NodeKind::Project)).unwrap();
        assert_eq!(
            keys(&bare),
            sorted(vec![
                "id",
                "kind",
                "parentId",
                "label",
                "status",
                "badges",
                "metrics",
                "meta",
                "updatedAt",
            ])
        );
        assert!(bare["parentId"].is_null(), "parentId is nullable, not absent");
    }

    #[test]
    fn apply_batch_bumps_rev_once() {
        let mut state = GraphState::new();
        let rev = state.apply_batch(
            vec![
                GraphOp::UpsertNode {
                    node: node("session:a", NodeKind::Session),
                },
                GraphOp::UpsertNode {
                    node: node("session:b", NodeKind::Session),
                },
            ],
            10,
        );
        assert_eq!(rev, 1);
        assert_eq!(state.rev(), 1);
        assert_eq!(state.node_count(), 2);
    }
}
