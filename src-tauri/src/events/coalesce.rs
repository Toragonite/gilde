//! Coalescing (owner T09): the store-patch edge of `store_subscribe`
//! ([`StoreCoalescer`], contract §4.3). The legacy graph-patch coalescer went
//! with the graph projection (A22.3).

use std::time::Duration;

/// One coalescer tick. Ten of these per second.
pub const TICK: Duration = Duration::from_millis(100);

// ---------------------------------------------------------------------------
// store patches (contract §4.3)
// ---------------------------------------------------------------------------

use crate::contract::store::{Entity, StoreOp, StorePatch, MAX_OPS_PER_PATCH};

/// At most this many store patches leave per second (one per [`TICK`]).
pub const STORE_MAX_PER_SEC: usize = 10;

/// Merges the patches of one 100 ms window for one subscriber.
///
/// * consecutive patches merge into one (`fromRev` of the first, `toRev` of
///   the last); ops for the same entity collapse to the last one;
/// * a patch whose `fromRev` is not the previous `toRev` (a gap or a reorder)
///   turns the stream into one `Resync`, after which nothing more is sent —
///   the WebView resubscribes, which replaces this channel;
/// * a merged patch above [`MAX_OPS_PER_PATCH`] ops is replaced by `Resync`.
#[derive(Debug, Default)]
pub struct StoreCoalescer {
    pending: Option<StorePatch>,
    last_to: Option<u64>,
    resynced: bool,
}

impl StoreCoalescer {
    pub fn push(&mut self, patch: StorePatch) {
        if self.resynced {
            return;
        }
        if let Some(last) = self.last_to {
            if patch.from_rev != last {
                self.resync(patch.to_rev.max(last));
                return;
            }
        }
        self.last_to = Some(patch.to_rev);
        let forced_resync = patch.ops.iter().any(|op| matches!(op, StoreOp::Resync));
        match &mut self.pending {
            None => self.pending = Some(patch),
            Some(p) => {
                p.to_rev = patch.to_rev;
                p.ops.extend(patch.ops);
            }
        }
        if forced_resync {
            let to = self.last_to.unwrap_or(0);
            self.resync(to);
            return;
        }
        if let Some(p) = &mut self.pending {
            collapse_ops(&mut p.ops);
            if p.ops.len() > MAX_OPS_PER_PATCH {
                let to = p.to_rev;
                self.resync(to);
            }
        }
    }

    /// The merged patch of this window, if any.
    pub fn take(&mut self) -> Option<StorePatch> {
        self.pending.take()
    }

    pub fn is_resynced(&self) -> bool {
        self.resynced
    }

    fn resync(&mut self, to_rev: u64) {
        let from = self
            .pending
            .as_ref()
            .map(|p| p.from_rev)
            .or(self.last_to)
            .unwrap_or(to_rev);
        self.pending = Some(StorePatch {
            from_rev: from,
            to_rev,
            ops: vec![StoreOp::Resync],
        });
        self.resynced = true;
    }
}

/// The identity of what an op writes: ops with the same key collapse to the
/// last one within a window. `None` = never collapsed.
fn op_key(op: &StoreOp) -> Option<String> {
    Some(match op {
        StoreOp::Upsert { entity } => entity_key(entity),
        StoreOp::Remove { entity_kind, id } => format!("{entity_kind:?}:{id}"),
        StoreOp::Queue { .. } => "queue".into(),
        StoreOp::Boot { .. } => "boot".into(),
        StoreOp::Notices { .. } => "notices".into(),
        StoreOp::Compat { .. } => "compat".into(),
        StoreOp::Sources { .. } => "sources".into(),
        StoreOp::Integrations { .. } => "integrations".into(),
        StoreOp::Quit { .. } => "quit".into(),
        StoreOp::Resync => return None,
    })
}

fn entity_key(entity: &Entity) -> String {
    match entity {
        Entity::Project { value } => format!("Project:{}", value.id.as_str()),
        Entity::Location { value } => format!("Location:{}", value.id.as_str()),
        Entity::Session { value } => format!("Session:{}", value.uid.as_str()),
        Entity::Team { value } => format!("Team:{}", value.key.as_str()),
        Entity::Member { value } => format!("Member:{}", value.key.as_str()),
        Entity::Task { value } => format!("Task:{}", value.key.as_str()),
        Entity::Message { value } => format!("Message:{}", value.key.as_str()),
        Entity::Account { value } => format!("Account:{}", value.id.as_str()),
        Entity::Leftover { value } => format!("Leftover:{}", value.key.as_str()),
    }
}

/// Keep only the last op per key, in order.
fn collapse_ops(ops: &mut Vec<StoreOp>) {
    let mut last: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (i, op) in ops.iter().enumerate() {
        if let Some(k) = op_key(op) {
            last.insert(k, i);
        }
    }
    let mut i = 0usize;
    ops.retain(|op| {
        let keep = match op_key(op) {
            Some(k) => last.get(&k) == Some(&i),
            None => true,
        };
        i += 1;
        keep
    });
}

#[cfg(test)]
mod store_tests {
    use super::*;
    use crate::contract::store::{BootStatus, EntityKind};

    fn boot(n: bool) -> StoreOp {
        StoreOp::Boot {
            status: BootStatus {
                spawn_gate_open: n,
                first_discovery_done: false,
                agents_detected: false,
                memory_only: None,
                agents: Vec::new(),
                agents_scanned_at: None,
                recovery: None,
            },
        }
    }

    fn patch(from: u64, to: u64, ops: Vec<StoreOp>) -> StorePatch {
        StorePatch { from_rev: from, to_rev: to, ops }
    }

    #[test]
    fn consecutive_patches_merge_and_same_entity_ops_collapse() {
        let mut c = StoreCoalescer::default();
        c.push(patch(1, 2, vec![boot(false)]));
        c.push(patch(2, 3, vec![boot(true), StoreOp::Remove { entity_kind: EntityKind::Leftover, id: "x".into() }]));
        let p = c.take().unwrap();
        assert_eq!((p.from_rev, p.to_rev), (1, 3));
        assert_eq!(p.ops.len(), 2);
        assert_eq!(p.ops[0], boot(true));
        assert!(c.take().is_none());
        c.push(patch(3, 4, vec![boot(false)]));
        assert_eq!(c.take().unwrap().from_rev, 3);
    }

    #[test]
    fn a_rev_gap_becomes_one_resync_and_silence() {
        let mut c = StoreCoalescer::default();
        c.push(patch(1, 2, vec![boot(false)]));
        c.push(patch(5, 6, vec![boot(true)]));
        let p = c.take().unwrap();
        assert_eq!(p.ops, vec![StoreOp::Resync]);
        assert!(c.is_resynced());
        c.push(patch(6, 7, vec![boot(true)]));
        assert!(c.take().is_none());
    }

    #[test]
    fn too_many_ops_become_resync() {
        let mut c = StoreCoalescer::default();
        let ops: Vec<StoreOp> = (0..=MAX_OPS_PER_PATCH)
            .map(|i| StoreOp::Remove { entity_kind: EntityKind::Leftover, id: format!("k{i}") })
            .collect();
        c.push(patch(1, 2, ops));
        assert_eq!(c.take().unwrap().ops, vec![StoreOp::Resync]);
    }
}
