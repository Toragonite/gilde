//! The event bus: the one place graph mutations enter the system.
//!
//! Producers ([`EventBus::push`]) never block and never touch the graph lock —
//! they hand an op to an unbounded channel. A single coalescer task owns the
//! `RwLock<GraphState>` write side, applies each published batch atomically and
//! bumps `rev` exactly once per emitted patch.
//!
//! The sink is abstracted behind [`PatchSink`] so the coalescer can be tested
//! without a `tauri::AppHandle`; production uses [`AppSink`].

use std::sync::Arc;

use parking_lot::RwLock;
use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

use crate::events::coalesce::{self, CoalescerConfig, Lane};
use crate::events::{GRAPH_PATCH, GRAPH_RESYNC};
use crate::model::graph::{GraphOp, GraphPatch, GraphSnapshot, GraphState};

/// Where published patches and events go.
pub trait PatchSink: Send + Sync + 'static {
    fn emit_patch(&self, patch: &GraphPatch);
    fn emit_named(&self, name: &str, payload: &Value);
}

/// Production sink: the Tauri app-global event channel.
pub struct AppSink {
    app: AppHandle,
}

impl AppSink {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

impl PatchSink for AppSink {
    fn emit_patch(&self, patch: &GraphPatch) {
        if let Err(err) = self.app.emit(GRAPH_PATCH, patch) {
            tracing::warn!(target: "events", error = %err, "failed to emit graph patch");
        }
    }

    fn emit_named(&self, name: &str, payload: &Value) {
        if let Err(err) = self.app.emit(name, payload) {
            tracing::warn!(target: "events", event = name, error = %err, "failed to emit event");
        }
    }
}

struct BusInner {
    graph: Arc<RwLock<GraphState>>,
    tx: UnboundedSender<Lane>,
    sink: Arc<dyn PatchSink>,
}

/// Cheap to clone; every clone talks to the same coalescer.
#[derive(Clone)]
pub struct EventBus {
    inner: Arc<BusInner>,
}

impl EventBus {
    /// Wire the bus to the Tauri app handle and start the coalescer.
    pub fn new(app: AppHandle, graph: Arc<RwLock<GraphState>>) -> Self {
        Self::with_sink(Arc::new(AppSink::new(app)), graph)
    }

    /// Same, with an arbitrary sink (tests, headless projections).
    pub fn with_sink(sink: Arc<dyn PatchSink>, graph: Arc<RwLock<GraphState>>) -> Self {
        Self::with_sink_and_config(sink, graph, CoalescerConfig::default())
    }

    pub fn with_sink_and_config(
        sink: Arc<dyn PatchSink>,
        graph: Arc<RwLock<GraphState>>,
        config: CoalescerConfig,
    ) -> Self {
        let (tx, rx) = unbounded_channel();
        let task = coalesce::run(rx, Arc::clone(&graph), Arc::clone(&sink), config);
        // Prefer the ambient runtime (commands, setup hooks run inside one);
        // fall back to Tauri's global runtime when there is none.
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(task);
            }
            Err(_) => {
                tauri::async_runtime::spawn(task);
            }
        }
        Self {
            inner: Arc::new(BusInner { graph, tx, sink }),
        }
    }

    /// Accept an op from any thread without blocking.
    ///
    /// A closed channel means the coalescer is gone (shutdown); dropping the op
    /// is correct — the authoritative state is `GraphState` plus SQLite.
    pub fn push(&self, op: GraphOp) {
        if self.inner.tx.send(Lane::Normal(op)).is_err() {
            tracing::debug!(target: "events", "graph op dropped: coalescer stopped");
        }
    }

    /// Push onto the 1 s metric lane. Metric patches for the same node merge,
    /// so a 1 Hz RSS sampler costs one op per node per second regardless of how
    /// many times it fires.
    pub fn push_metric(&self, op: GraphOp) {
        if self.inner.tx.send(Lane::Metric(op)).is_err() {
            tracing::debug!(target: "events", "graph metric op dropped: coalescer stopped");
        }
    }

    /// Emit a non-graph `gilde://` event.
    pub fn emit<P: Serialize + Clone>(&self, name: &'static str, payload: P) {
        match serde_json::to_value(payload) {
            Ok(value) => self.inner.sink.emit_named(name, &value),
            Err(err) => {
                tracing::warn!(target: "events", event = name, error = %err, "payload not serializable")
            }
        }
    }

    /// Publish the whole graph as `gilde://graph/resync` (client revision gap).
    pub fn resync(&self) {
        let snapshot = self.snapshot();
        match serde_json::to_value(&snapshot) {
            Ok(value) => self.inner.sink.emit_named(GRAPH_RESYNC, &value),
            Err(err) => {
                tracing::warn!(target: "events", error = %err, "snapshot not serializable")
            }
        }
    }

    pub fn graph(&self) -> &Arc<RwLock<GraphState>> {
        &self.inner.graph
    }

    pub fn snapshot(&self) -> GraphSnapshot {
        self.inner.graph.read().snapshot()
    }

    pub fn rev(&self) -> u64 {
        self.inner.graph.read().rev()
    }
}

/// Test/instrumentation sink that keeps every published patch and event.
#[derive(Default)]
pub struct RecordingSink {
    patches: parking_lot::Mutex<Vec<(std::time::Instant, GraphPatch)>>,
    events: parking_lot::Mutex<Vec<(String, Value)>>,
}

impl RecordingSink {
    pub fn patches(&self) -> Vec<GraphPatch> {
        self.patches
            .lock()
            .iter()
            .map(|(_, patch)| patch.clone())
            .collect()
    }

    pub fn patch_count(&self) -> usize {
        self.patches.lock().len()
    }

    pub fn events(&self) -> Vec<(String, Value)> {
        self.events.lock().clone()
    }

    /// The largest number of patches emitted inside any one-second window.
    pub fn max_patches_per_second(&self) -> usize {
        let times: Vec<std::time::Instant> =
            self.patches.lock().iter().map(|(at, _)| *at).collect();
        let mut max = 0usize;
        for (i, start) in times.iter().enumerate() {
            let count = times[i..]
                .iter()
                .take_while(|t| t.duration_since(*start) < std::time::Duration::from_secs(1))
                .count();
            max = max.max(count);
        }
        max
    }
}

impl PatchSink for RecordingSink {
    fn emit_patch(&self, patch: &GraphPatch) {
        self.patches
            .lock()
            .push((std::time::Instant::now(), patch.clone()));
    }

    fn emit_named(&self, name: &str, payload: &Value) {
        self.events.lock().push((name.to_owned(), payload.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::graph::{
        EdgeKind, GraphEdge, GraphNode, NodeKind, NodeStatus, Totals,
    };
    use crate::model::status::{NodeBadge, NodeMetrics};
    use std::time::Duration;

    fn node(i: usize) -> GraphNode {
        GraphNode::new(
            format!("session:{i}"),
            NodeKind::Session,
            format!("agent {i}"),
            NodeStatus::Idle,
            1_700_000_000_000 + i as i64,
        )
    }

    /// Replay published patches onto an empty client state.
    fn replay(patches: &[GraphPatch]) -> GraphState {
        let mut client = GraphState::new();
        for patch in patches {
            for op in &patch.ops {
                client.apply(op.clone());
            }
            client.set_rev(patch.rev);
        }
        client
    }

    async fn wait_until<F: FnMut() -> bool>(mut done: F, budget: Duration) -> bool {
        let deadline = std::time::Instant::now() + budget;
        loop {
            if done() {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    #[tokio::test]
    async fn ten_thousand_ops_converge_and_stay_at_or_below_ten_hz() {
        let sink = Arc::new(RecordingSink::default());
        let graph = Arc::new(RwLock::new(GraphState::new()));
        let bus = EventBus::with_sink(sink.clone(), Arc::clone(&graph));

        const NODES: usize = 50;
        for i in 0..NODES {
            bus.push(GraphOp::UpsertNode { node: node(i) });
        }
        for i in 0..NODES {
            let edge = GraphEdge::new(
                EdgeKind::RunsIn,
                format!("session:{i}"),
                "worktree:w".to_string(),
            );
            bus.push(GraphOp::UpsertEdge { edge });
        }

        // ~9 000 status/badge patches on the normal lane…
        for i in 0..9_000usize {
            bus.push(GraphOp::PatchNode {
                id: format!("session:{}", i % NODES),
                status: Some(if i % 3 == 0 {
                    NodeStatus::Working
                } else if i % 3 == 1 {
                    NodeStatus::Waiting
                } else {
                    NodeStatus::Idle
                }),
                confidence: Some((i % 100) as f32 / 100.0),
                evidence: None,
                badges: Some(if i % 7 == 0 {
                    vec![NodeBadge::Attention]
                } else {
                    vec![]
                }),
                metrics: None,
            });
        }
        // …and ~900 metric-only patches on the 1 s lane.
        for i in 0..900usize {
            bus.push_metric(GraphOp::PatchNode {
                id: format!("session:{}", i % NODES),
                status: None,
                confidence: None,
                evidence: None,
                badges: None,
                metrics: Some(NodeMetrics {
                    rss_bytes: Some(i as u64 * 4096),
                    cpu_pct: Some(i as f32 / 10.0),
                    ..Default::default()
                }),
            });
        }
        for i in 0..50usize {
            bus.push(GraphOp::Totals {
                totals: Totals {
                    sessions: NODES as u32,
                    runs_active: i as u32,
                    ..Default::default()
                },
            });
        }
        // 50 + 50 + 9000 + 900 + 50 = 10 050 ops.

        let converged = wait_until(
            || {
                let server = graph.read().snapshot();
                let patches = sink.patches();
                if patches.is_empty() {
                    return false;
                }
                let mut client = replay(&patches);
                client.set_at(server.at);
                serde_json::to_vec(&client.snapshot()).unwrap()
                    == serde_json::to_vec(&server).unwrap()
            },
            Duration::from_secs(10),
        )
        .await;
        assert!(converged, "client replay never converged on the server state");

        let patches = sink.patches();
        let server = graph.read().snapshot();

        // (a) emit rate
        let max_per_second = sink.max_patches_per_second();
        assert!(
            max_per_second <= 10,
            "coalescer emitted {max_per_second} patches in one second (cap is 10)"
        );

        // (b) byte-for-byte convergence
        let mut client = replay(&patches);
        client.set_at(server.at);
        assert_eq!(
            serde_json::to_vec(&client.snapshot()).unwrap(),
            serde_json::to_vec(&server).unwrap(),
            "replayed client state diverged from graph_snapshot()"
        );

        // Revisions are contiguous and monotonic.
        for (i, patch) in patches.iter().enumerate() {
            assert_eq!(patch.rev, i as u64 + 1, "rev must increase by one per patch");
        }
        assert_eq!(server.rev, patches.len() as u64);

        // The 10 050 ops collapsed into far fewer published ops.
        let published_ops: usize = patches.iter().map(|p| p.ops.len()).sum();
        assert!(
            published_ops <= 10_050,
            "coalescing must never invent ops (got {published_ops})"
        );
    }

    #[tokio::test]
    async fn topology_hash_is_stable_across_metric_traffic() {
        let sink = Arc::new(RecordingSink::default());
        let graph = Arc::new(RwLock::new(GraphState::new()));
        let bus = EventBus::with_sink(sink.clone(), Arc::clone(&graph));

        for i in 0..10 {
            bus.push(GraphOp::UpsertNode { node: node(i) });
        }
        assert!(
            wait_until(|| graph.read().node_count() == 10, Duration::from_secs(5)).await,
            "nodes were never applied"
        );
        let hash = graph.read().topology_hash();

        for i in 0..1_000usize {
            bus.push_metric(GraphOp::PatchNode {
                id: format!("session:{}", i % 10),
                status: None,
                confidence: None,
                evidence: None,
                badges: None,
                metrics: Some(NodeMetrics {
                    rss_bytes: Some(i as u64),
                    ..Default::default()
                }),
            });
        }
        assert!(
            wait_until(
                || graph
                    .read()
                    .node("session:0")
                    .and_then(|n| n.metrics.rss_bytes)
                    .is_some(),
                Duration::from_secs(5),
            )
            .await,
            "metric lane never flushed"
        );
        assert_eq!(hash, graph.read().topology_hash());
    }

    #[tokio::test]
    async fn push_is_thread_safe_and_non_blocking() {
        let sink = Arc::new(RecordingSink::default());
        let graph = Arc::new(RwLock::new(GraphState::new()));
        let bus = EventBus::with_sink(sink.clone(), Arc::clone(&graph));

        let handles: Vec<_> = (0..4)
            .map(|t| {
                let bus = bus.clone();
                std::thread::spawn(move || {
                    for i in 0..250 {
                        bus.push(GraphOp::UpsertNode {
                            node: node(t * 250 + i),
                        });
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }

        assert!(
            wait_until(|| graph.read().node_count() == 1_000, Duration::from_secs(10)).await,
            "expected 1000 nodes, saw {}",
            graph.read().node_count()
        );
    }

    #[tokio::test]
    async fn emit_and_resync_reach_the_sink() {
        let sink = Arc::new(RecordingSink::default());
        let graph = Arc::new(RwLock::new(GraphState::new()));
        let bus = EventBus::with_sink(sink.clone(), Arc::clone(&graph));

        bus.emit(
            crate::events::RECOVERY_PENDING,
            crate::events::RecoveryPendingEvent { count: 3 },
        );
        bus.resync();

        let events = sink.events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].0, "gilde://recovery/pending");
        assert_eq!(events[0].1["count"], 3);
        assert_eq!(events[1].0, "gilde://graph/resync");
        assert!(events[1].1["topologyHash"].is_string());
    }

    #[tokio::test]
    async fn empty_ticks_do_not_emit_patches() {
        let sink = Arc::new(RecordingSink::default());
        let graph = Arc::new(RwLock::new(GraphState::new()));
        let _bus = EventBus::with_sink(sink.clone(), Arc::clone(&graph));
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(sink.patch_count(), 0, "an idle bus must be silent");
        assert_eq!(graph.read().rev(), 0);
    }
}
