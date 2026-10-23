//! Store rev/patch coalescing, ≤ 2,000 ops, `Resync` (§4.3).
//!
//! Owner: T02. World writes every store change into the [`Outbox`]: ops for
//! the same entity (or the same singleton: queue, boot, notices, compat,
//! sources, integrations, quit) collapse to the last within a window, in the
//! order of their last change. The [`Publisher`] turns the outbox into one
//! `StorePatch { fromRev, toRev = fromRev + 1, ops }` per flush, at most every
//! 100 ms (≤ 10 per second); a window with more than 2,000 ops, or a patch
//! over 1 MiB serialized, becomes a single `Resync` instead (the WebView then
//! calls `store_subscribe` again). Patches are never dropped while a
//! subscriber exists; without one, the rev still advances so a later snapshot
//! carries the current rev.

use std::collections::HashMap;

use crate::contract::services::StorePatchSink;
use crate::contract::store::{Entity, EntityKind, StoreOp, StorePatch, MAX_OPS_PER_PATCH};

/// A patch is ≤ 1 MiB (§4.3).
pub const MAX_PATCH_BYTES: usize = 1024 * 1024;
/// ≤ 10 patches per second (§4.3).
pub const MIN_FLUSH_INTERVAL_MS: u64 = 100;

/// The coalescing key of an op.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum OpKey {
    Entity(EntityKind, String),
    Queue,
    Boot,
    Notices,
    Compat,
    Sources,
    Integrations,
    Quit,
}

/// The entity key of an upsert (`kind`, id string).
pub fn entity_key(e: &Entity) -> (EntityKind, String) {
    match e {
        Entity::Project { value } => (EntityKind::Project, value.id.as_str().to_string()),
        Entity::Location { value } => (EntityKind::Location, value.id.as_str().to_string()),
        Entity::Session { value } => (EntityKind::Session, value.uid.as_str().to_string()),
        Entity::Team { value } => (EntityKind::Team, value.key.as_str().to_string()),
        Entity::Member { value } => (EntityKind::Member, value.key.as_str().to_string()),
        Entity::Task { value } => (EntityKind::Task, value.key.as_str().to_string()),
        Entity::Message { value } => (EntityKind::Message, value.key.as_str().to_string()),
        Entity::Account { value } => (EntityKind::Account, value.id.as_str().to_string()),
        Entity::Leftover { value } => (EntityKind::Leftover, value.key.as_str().to_string()),
    }
}

fn key_of(op: &StoreOp) -> Option<OpKey> {
    Some(match op {
        StoreOp::Upsert { entity } => {
            let (k, id) = entity_key(entity);
            OpKey::Entity(k, id)
        }
        StoreOp::Remove { entity_kind, id } => OpKey::Entity(*entity_kind, id.clone()),
        StoreOp::Queue { .. } => OpKey::Queue,
        StoreOp::Boot { .. } => OpKey::Boot,
        StoreOp::Notices { .. } => OpKey::Notices,
        StoreOp::Compat { .. } => OpKey::Compat,
        StoreOp::Sources { .. } => OpKey::Sources,
        StoreOp::Integrations { .. } => OpKey::Integrations,
        StoreOp::Quit { .. } => OpKey::Quit,
        StoreOp::Resync => return None,
    })
}

/// Pending store ops, collapsed per entity.
#[derive(Debug, Default)]
pub struct Outbox {
    seq: u64,
    ops: HashMap<OpKey, (u64, StoreOp)>,
    /// More than the op cap arrived in this window.
    overflowed: bool,
}

impl Outbox {
    /// Add an op; an op for the same key replaces the earlier one and moves
    /// to the end (order of last change).
    pub fn push(&mut self, op: StoreOp) {
        let Some(key) = key_of(&op) else {
            self.overflowed = true;
            return;
        };
        self.seq += 1;
        self.ops.insert(key, (self.seq, op));
        if self.ops.len() > MAX_OPS_PER_PATCH {
            self.overflowed = true;
        }
    }

    pub fn upsert(&mut self, entity: Entity) {
        self.push(StoreOp::Upsert { entity: Box::new(entity) });
    }

    pub fn remove(&mut self, entity_kind: EntityKind, id: impl Into<String>) {
        self.push(StoreOp::Remove { entity_kind, id: id.into() });
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty() && !self.overflowed
    }

    pub fn len(&self) -> usize {
        self.ops.len()
    }

    /// Take every op in order of last change; `None` = the window overflowed
    /// (send `Resync`).
    pub fn drain(&mut self) -> Option<Vec<StoreOp>> {
        let overflowed = std::mem::take(&mut self.overflowed);
        let mut v: Vec<(u64, StoreOp)> = self.ops.drain().map(|(_, x)| x).collect();
        if overflowed || v.len() > MAX_OPS_PER_PATCH {
            return None;
        }
        v.sort_by_key(|(s, _)| *s);
        Some(v.into_iter().map(|(_, op)| op).collect())
    }

    /// Forget every pending op (a new snapshot already contains them).
    pub fn clear(&mut self) {
        self.ops.clear();
        self.overflowed = false;
    }
}

/// The rev counter and the one subscriber.
#[derive(Default)]
pub struct Publisher {
    rev: u64,
    sink: Option<Box<dyn StorePatchSink>>,
    last_flush_up: Option<u64>,
    /// Patches sent (diagnostics and tests).
    sent: u64,
    resyncs: u64,
}

impl std::fmt::Debug for Publisher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Publisher")
            .field("rev", &self.rev)
            .field("subscribed", &self.sink.is_some())
            .field("sent", &self.sent)
            .field("resyncs", &self.resyncs)
            .finish()
    }
}

impl Publisher {
    pub fn rev(&self) -> u64 {
        self.rev
    }

    pub fn subscribed(&self) -> bool {
        self.sink.is_some()
    }

    pub fn resyncs(&self) -> u64 {
        self.resyncs
    }

    /// Whether a flush may run now (≤ 10 per second).
    pub fn due(&self, now_up: u64) -> bool {
        self.last_flush_up.is_none_or(|t| now_up.saturating_sub(t) >= MIN_FLUSH_INTERVAL_MS)
    }

    /// When the next flush may run.
    pub fn next_due(&self, now_up: u64) -> u64 {
        self.last_flush_up.map_or(now_up, |t| (t + MIN_FLUSH_INTERVAL_MS).max(now_up))
    }

    /// Replace the subscriber (atomic `store_subscribe`, called under the World
    /// lock): pending ops are already in the snapshot the caller takes next,
    /// so they are dropped and the rev advances past them.
    pub fn replace_sink(&mut self, sink: Box<dyn StorePatchSink>, outbox: &mut Outbox) -> u64 {
        if !outbox.is_empty() {
            outbox.clear();
            self.rev += 1;
        }
        self.sink = Some(sink);
        self.rev
    }

    /// Flush the outbox into one patch (or a `Resync`) and send it. Returns
    /// the patch (also when there is no subscriber).
    pub fn flush(&mut self, outbox: &mut Outbox, now_up: u64) -> Option<StorePatch> {
        if outbox.is_empty() {
            return None;
        }
        self.last_flush_up = Some(now_up);
        let from = self.rev;
        self.rev += 1;
        let ops = match outbox.drain() {
            Some(ops) => {
                let p = StorePatch { from_rev: from, to_rev: self.rev, ops };
                let size = serde_json::to_vec(&p).map(|b| b.len()).unwrap_or(usize::MAX);
                if size > MAX_PATCH_BYTES {
                    None
                } else {
                    Some(p)
                }
            }
            None => None,
        };
        let patch = match ops {
            Some(p) => p,
            None => {
                self.resyncs += 1;
                StorePatch { from_rev: from, to_rev: self.rev, ops: vec![StoreOp::Resync] }
            }
        };
        if let Some(s) = &self.sink {
            self.sent += 1;
            if !s.send(patch.clone()) {
                // The subscriber is gone (the WebView reloaded); the next
                // `store_subscribe` installs a new one.
                self.sink = None;
            }
        }
        Some(patch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::store::{BootStatus, StoreOp};
    use std::sync::{Arc, Mutex};

    struct Rec(Arc<Mutex<Vec<StorePatch>>>);
    impl StorePatchSink for Rec {
        fn send(&self, patch: StorePatch) -> bool {
            self.0.lock().unwrap().push(patch);
            true
        }
    }

    fn boot(n: bool) -> StoreOp {
        StoreOp::Boot {
            status: BootStatus { spawn_gate_open: n, first_discovery_done: false, agents_detected: false, memory_only: None, agents: Vec::new(), agents_scanned_at: None, recovery: None },
        }
    }

    #[test]
    fn same_key_collapses_to_the_last_and_revs_chain() {
        let got = Arc::new(Mutex::new(Vec::new()));
        let mut o = Outbox::default();
        let mut p = Publisher::default();
        p.replace_sink(Box::new(Rec(got.clone())), &mut o);
        o.push(boot(false));
        o.push(boot(true));
        o.remove(EntityKind::Session, "s_x");
        let a = p.flush(&mut o, 0).unwrap();
        assert_eq!(a.ops.len(), 2);
        assert_eq!(a.ops[0], boot(true));
        assert_eq!((a.from_rev, a.to_rev), (0, 1));
        assert!(p.flush(&mut o, 200).is_none());
        o.push(boot(false));
        let b = p.flush(&mut o, 200).unwrap();
        assert_eq!(b.from_rev, a.to_rev);
        assert_eq!(got.lock().unwrap().len(), 2);
    }

    #[test]
    fn more_than_the_op_cap_is_a_resync() {
        let mut o = Outbox::default();
        let mut p = Publisher::default();
        for i in 0..=MAX_OPS_PER_PATCH {
            o.remove(EntityKind::Task, format!("k_{i}"));
        }
        let patch = p.flush(&mut o, 0).unwrap();
        assert_eq!(patch.ops, vec![StoreOp::Resync]);
        assert!(o.is_empty());
        assert_eq!(p.resyncs(), 1);
    }

    #[test]
    fn a_patch_over_one_mib_is_a_resync() {
        use crate::contract::common::Notice;
        let mut o = Outbox::default();
        let mut p = Publisher::default();
        o.push(StoreOp::Notices { notices: vec![Notice::new("notice.x").param("p", "x".repeat(MAX_PATCH_BYTES + 1))] });
        let patch = p.flush(&mut o, 0).unwrap();
        assert_eq!(patch.ops, vec![StoreOp::Resync]);
        o.push(StoreOp::Notices { notices: vec![Notice::new("notice.x")] });
        let patch = p.flush(&mut o, 200).unwrap();
        assert_ne!(patch.ops, vec![StoreOp::Resync]);
        assert!(serde_json::to_vec(&patch).unwrap().len() <= MAX_PATCH_BYTES);
    }

    #[test]
    fn flushes_are_rate_limited() {
        let p = Publisher { last_flush_up: Some(1_000), ..Publisher::default() };
        assert!(!p.due(1_050));
        assert!(p.due(1_100));
        assert_eq!(p.next_due(1_050), 1_100);
    }
}
