//! Graph patch coalescer: one `gilde://graph/patch` per 100 ms tick, with a
//! separate 1 s lane for metric-only ops.
//!
//! DESIGN §4 caps `gilde://graph/patch` at 10 Hz. The metric lane rides *inside*
//! every tenth tick rather than emitting on its own timer, so the total event
//! rate stays at or below 10 per second no matter how hard the producers push.
//!
//! The single most important property here: **ops are applied to `GraphState`
//! at flush time, in exactly the order they are published.** A client that
//! replays the published patches onto an empty state therefore reaches a
//! byte-identical snapshot (DESIGN R18).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::{interval, MissedTickBehavior};

use crate::events::bus::PatchSink;
use crate::events::now_ms;
use crate::model::graph::{GraphOp, GraphPatch, GraphState};
use crate::model::status::NodeMetrics;

/// One coalescer tick. Ten of these per second.
pub const TICK: Duration = Duration::from_millis(100);
/// The metric lane drains every Nth tick, i.e. once per second.
pub const METRIC_EVERY_TICKS: u64 = 10;

/// Which lane an op was pushed onto.
#[derive(Debug)]
pub enum Lane {
    /// Structural / status ops. Flushed on every tick.
    Normal(GraphOp),
    /// Metric-only ops. Merged per node and flushed once per second.
    Metric(GraphOp),
}

#[derive(Debug, Clone, Copy)]
pub struct CoalescerConfig {
    pub tick: Duration,
    pub metric_every_ticks: u64,
}

impl Default for CoalescerConfig {
    fn default() -> Self {
        Self {
            tick: TICK,
            metric_every_ticks: METRIC_EVERY_TICKS,
        }
    }
}

/// Accumulates metric-only patches so that a node never produces more than one
/// op per metric flush, however often the watchdog samples it.
#[derive(Default)]
struct MetricLane {
    per_node: BTreeMap<String, NodeMetrics>,
    /// Anything pushed onto the metric lane that is not a metric-only patch.
    passthrough: Vec<GraphOp>,
}

impl MetricLane {
    fn push(&mut self, op: GraphOp) {
        match op {
            GraphOp::PatchNode {
                id,
                status: None,
                confidence: None,
                evidence: None,
                badges: None,
                metrics: Some(metrics),
            } => {
                self.per_node.entry(id).or_default().merge_from(&metrics);
            }
            other => self.passthrough.push(other),
        }
    }

    fn is_empty(&self) -> bool {
        self.per_node.is_empty() && self.passthrough.is_empty()
    }

    fn drain(&mut self) -> Vec<GraphOp> {
        let mut ops: Vec<GraphOp> = std::mem::take(&mut self.per_node)
            .into_iter()
            .map(|(id, metrics)| GraphOp::PatchNode {
                id,
                status: None,
                confidence: None,
                evidence: None,
                badges: None,
                metrics: Some(metrics),
            })
            .collect();
        ops.append(&mut self.passthrough);
        ops
    }
}

/// Apply a batch under the write lock and publish it as one patch.
fn flush(ops: Vec<GraphOp>, graph: &Arc<RwLock<GraphState>>, sink: &Arc<dyn PatchSink>) {
    if ops.is_empty() {
        return;
    }
    let at = now_ms();
    let rev = {
        let mut state = graph.write();
        state.apply_batch(ops.iter().cloned(), at)
    };
    sink.emit_patch(&GraphPatch { rev, ops });
}

/// Drive the coalescer until every sender has been dropped.
pub async fn run(
    mut rx: UnboundedReceiver<Lane>,
    graph: Arc<RwLock<GraphState>>,
    sink: Arc<dyn PatchSink>,
    config: CoalescerConfig,
) {
    let mut ticker = interval(config.tick);
    // Never try to "catch up" after a stall: bursting ten patches at once is
    // exactly the behaviour the 10 Hz cap exists to prevent.
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // The first tick completes immediately; consume it so the first real flush
    // happens one tick in.
    ticker.tick().await;

    let mut normal: Vec<GraphOp> = Vec::new();
    let mut metric = MetricLane::default();
    let mut tick_count: u64 = 0;
    let metric_every = config.metric_every_ticks.max(1);

    loop {
        tokio::select! {
            received = rx.recv() => match received {
                Some(Lane::Normal(op)) => normal.push(op),
                Some(Lane::Metric(op)) => metric.push(op),
                None => break,
            },
            _ = ticker.tick() => {
                tick_count = tick_count.wrapping_add(1);
                let mut ops = std::mem::take(&mut normal);
                if tick_count % metric_every == 0 && !metric.is_empty() {
                    ops.append(&mut metric.drain());
                }
                flush(ops, &graph, &sink);
            }
        }
    }

    // Senders are gone: publish whatever is still buffered so no op is lost.
    let mut ops = std::mem::take(&mut normal);
    ops.append(&mut metric.drain());
    flush(ops, &graph, &sink);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::graph::{GraphNode, NodeKind, NodeStatus};

    #[test]
    fn metric_lane_merges_per_node() {
        let mut lane = MetricLane::default();
        for i in 0..100u64 {
            lane.push(GraphOp::PatchNode {
                id: format!("session:{}", i % 4),
                status: None,
                confidence: None,
                evidence: None,
                badges: None,
                metrics: Some(NodeMetrics {
                    rss_bytes: Some(i),
                    ..Default::default()
                }),
            });
        }
        let ops = lane.drain();
        assert_eq!(ops.len(), 4, "one op per node, not one per sample");
        match &ops[0] {
            GraphOp::PatchNode { id, metrics, .. } => {
                assert_eq!(id, "session:0");
                assert_eq!(metrics.as_ref().unwrap().rss_bytes, Some(96));
            }
            other => panic!("unexpected op {other:?}"),
        }
        assert!(lane.is_empty());
    }

    #[test]
    fn metric_lane_passes_structural_ops_through() {
        let mut lane = MetricLane::default();
        lane.push(GraphOp::UpsertNode {
            node: GraphNode::new("session:a", NodeKind::Session, "a", NodeStatus::Idle, 1),
        });
        assert_eq!(lane.drain().len(), 1);
    }
}
