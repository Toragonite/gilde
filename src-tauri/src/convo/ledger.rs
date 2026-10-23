//! The request ledger (contract §3.4.5) [C12] and the table of Gilde's own
//! control requests (§6.1.4 "Two id tables") [F-51].
//!
//! Owner: T05.
//!
//! **Ledger** — every CLI-originated `request_id` seen in the life of the
//! current process with its state `Pending | Answered{decision} | Withdrawn |
//! Ended`, bounded to the last 1,000 ids (a pending id is never evicted). It
//! backs the outcomes of `interaction_answer` (§4.2.3):
//!
//! | ledger state | answer outcome |
//! |---|---|
//! | never seen (or evicted) | `NOT_FOUND{request}` |
//! | `Pending` | `answered` (this call sends the decision) |
//! | `Answered{d}` | `alreadyAnswered`, `previous = d` (no effect) |
//! | `Withdrawn` / `Ended` | `withdrawn` |
//!
//! A `control_cancel_request` marks the id `Withdrawn` even when Gilde has
//! already answered it (the CLI ignores late answers [R4 §1.7]); a later
//! answer then returns `withdrawn` (T-convo-withdrawRace).
//!
//! **Gilde table** — `initialize`, `interrupt`, `set_model`, `get_usage`, …
//! are kept apart from the ledger: a stdout `control_response` whose id is in
//! the ledger is the echo of Gilde's own answer [R6 §3 LP-01, §8 item 2], one
//! in the Gilde table is the reply to Gilde's request, and an id in neither is
//! ignored and counted (R4 rule 1).

use std::collections::{HashMap, VecDeque};

use tokio::sync::oneshot;

use crate::contract::account::Quota;
use crate::contract::interaction::DecisionKind;

/// The ledger keeps at most this many ids (§3.4.5).
pub const LEDGER_CAP: usize = 1_000;

/// The state of one CLI-originated request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerState {
    Pending,
    Answered(DecisionKind),
    Withdrawn,
    Ended,
}

/// What [`Ledger::observe`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seen {
    New,
    /// A second delivery of a known id: ignored (§6.1.4, R4 §1.3).
    Duplicate(LedgerState),
}

/// The answer check of §4.2.3 [C12].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerCheck {
    NotFound,
    Pending,
    AlreadyAnswered(DecisionKind),
    Withdrawn,
}

#[derive(Debug)]
struct Entry {
    state: LedgerState,
    tool_use_id: Option<String>,
    acked: bool,
}

/// The CLI-originated requests of the current process (never in the store).
#[derive(Debug, Default)]
pub struct Ledger {
    entries: HashMap<String, Entry>,
    order: VecDeque<String>,
    tool_uses: HashMap<String, String>,
    /// Responses or cancels naming ids in neither table (R4 rule 1).
    pub unknown_ids: u64,
}

impl Ledger {
    pub fn new() -> Ledger {
        Ledger::default()
    }

    /// Record a request id; a known id is a duplicate delivery.
    pub fn observe(&mut self, id: &str, tool_use_id: Option<&str>) -> Seen {
        if let Some(e) = self.entries.get(id) {
            return Seen::Duplicate(e.state);
        }
        self.entries.insert(
            id.to_owned(),
            Entry { state: LedgerState::Pending, tool_use_id: tool_use_id.map(str::to_owned), acked: false },
        );
        self.order.push_back(id.to_owned());
        if let Some(t) = tool_use_id {
            self.tool_uses.insert(t.to_owned(), id.to_owned());
        }
        self.evict();
        Seen::New
    }

    fn evict(&mut self) {
        while self.entries.len() > LEDGER_CAP {
            let Some(pos) = self
                .order
                .iter()
                .position(|id| self.entries.get(id).is_some_and(|e| e.state != LedgerState::Pending))
            else {
                return;
            };
            let id = self.order.remove(pos).expect("position is in range");
            if let Some(e) = self.entries.remove(&id) {
                if let Some(t) = e.tool_use_id {
                    if self.tool_uses.get(&t) == Some(&id) {
                        self.tool_uses.remove(&t);
                    }
                }
            }
        }
    }

    pub fn state(&self, id: &str) -> Option<LedgerState> {
        self.entries.get(id).map(|e| e.state)
    }

    pub fn contains(&self, id: &str) -> bool {
        self.entries.contains_key(id)
    }

    /// The §4.2.3 outcome an answer for `id` would have now.
    pub fn check(&self, id: &str) -> AnswerCheck {
        match self.state(id) {
            None => AnswerCheck::NotFound,
            Some(LedgerState::Pending) => AnswerCheck::Pending,
            Some(LedgerState::Answered(d)) => AnswerCheck::AlreadyAnswered(d),
            Some(LedgerState::Withdrawn | LedgerState::Ended) => AnswerCheck::Withdrawn,
        }
    }

    /// The decision was sent (only after it was queued for stdin).
    pub fn mark_answered(&mut self, id: &str, kind: DecisionKind) {
        if let Some(e) = self.entries.get_mut(id) {
            if e.state == LedgerState::Pending {
                e.state = LedgerState::Answered(kind);
            }
        }
    }

    /// `control_cancel_request`: the previous state, or `None` for an id never
    /// seen (ignored and counted).
    pub fn withdraw(&mut self, id: &str) -> Option<LedgerState> {
        match self.entries.get_mut(id) {
            Some(e) => {
                let prev = e.state;
                if matches!(prev, LedgerState::Pending | LedgerState::Answered(_)) {
                    e.state = LedgerState::Withdrawn;
                }
                Some(prev)
            }
            None => {
                self.unknown_ids += 1;
                None
            }
        }
    }

    /// The process ended: every pending id becomes `Ended` (returned in order).
    pub fn end_pending(&mut self) -> Vec<String> {
        let mut ended = Vec::new();
        for id in &self.order {
            if let Some(e) = self.entries.get_mut(id) {
                if e.state == LedgerState::Pending {
                    e.state = LedgerState::Ended;
                    ended.push(id.clone());
                }
            }
        }
        ended
    }

    /// The echo of Gilde's own answer arrived; `false` if the id is unknown.
    pub fn ack(&mut self, id: &str) -> bool {
        match self.entries.get_mut(id) {
            Some(e) => {
                e.acked = true;
                true
            }
            None => false,
        }
    }

    pub fn acked(&self, id: &str) -> bool {
        self.entries.get(id).is_some_and(|e| e.acked)
    }

    /// Whether a `can_use_tool` for this `tool_use_id` was seen (F-26, §6.1.11).
    pub fn has_tool_use(&self, tool_use_id: &str) -> bool {
        self.tool_uses.contains_key(tool_use_id)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Gilde-originated requests
// ---------------------------------------------------------------------------

/// What Gilde asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GildeKind {
    Initialize,
    /// `turn` = the `turn_seq` it was sent for (its receipt marks that turn).
    Interrupt { discard_queued: bool, turn: Option<u64> },
    SetModel,
    SetPermissionMode,
    CancelAsync { message_uuid: String },
    /// `baseline`: the one request after a resume whose
    /// `session.total_cost_usd` is the cost baseline [A19.5].
    GetUsage { baseline: bool },
}

/// The reply a waiter gets. Raw payloads never leave the core: `get_usage`
/// is parsed through its allowlist on arrival (§6.1.7).
#[derive(Debug, Clone, PartialEq)]
pub enum GildeReply {
    Ok,
    Usage(Option<Quota>),
    /// The CLI answered `subtype:"error"` (the text is not kept).
    Refused,
    /// The process ended (or the stop began) before a reply.
    Gone,
}

/// One pending Gilde request.
#[derive(Debug)]
pub struct GildePending {
    pub kind: GildeKind,
    pub waiter: Option<oneshot::Sender<GildeReply>>,
}

/// Gilde's own in-flight control requests.
#[derive(Debug, Default)]
pub struct GildeTable {
    map: HashMap<String, GildePending>,
}

impl GildeTable {
    pub fn new() -> GildeTable {
        GildeTable::default()
    }

    /// A fresh UUIDv4 id that is in neither table (§6.1.4: never reuse a CLI id).
    pub fn mint(&self, ledger: &Ledger) -> String {
        loop {
            let id = uuid::Uuid::new_v4().to_string();
            if !ledger.contains(&id) && !self.map.contains_key(&id) {
                return id;
            }
        }
    }

    pub fn insert(&mut self, id: String, kind: GildeKind, waiter: Option<oneshot::Sender<GildeReply>>) {
        self.map.insert(id, GildePending { kind, waiter });
    }

    pub fn take(&mut self, id: &str) -> Option<GildePending> {
        self.map.remove(id)
    }

    pub fn contains(&self, id: &str) -> bool {
        self.map.contains_key(id)
    }

    /// Whether any request of this kind is in flight.
    pub fn any(&self, f: impl Fn(&GildeKind) -> bool) -> bool {
        self.map.values().any(|p| f(&p.kind))
    }

    /// Ids of the in-flight requests matching `f`.
    pub fn ids(&self, f: impl Fn(&GildeKind) -> bool) -> Vec<String> {
        self.map.iter().filter(|(_, p)| f(&p.kind)).map(|(id, _)| id.clone()).collect()
    }

    /// The process is gone: every waiter gets [`GildeReply::Gone`].
    pub fn fail_all(&mut self) {
        for (_, p) in self.map.drain() {
            if let Some(w) = p.waiter {
                let _ = w.send(GildeReply::Gone);
            }
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcomes_follow_the_c12_matrix() {
        let mut l = Ledger::new();
        assert_eq!(l.check("r1"), AnswerCheck::NotFound);
        assert_eq!(l.observe("r1", Some("toolu_1")), Seen::New);
        assert_eq!(l.observe("r1", Some("toolu_1")), Seen::Duplicate(LedgerState::Pending));
        assert_eq!(l.check("r1"), AnswerCheck::Pending);
        l.mark_answered("r1", DecisionKind::AllowOnce);
        assert_eq!(l.check("r1"), AnswerCheck::AlreadyAnswered(DecisionKind::AllowOnce));
        // A second mark never overwrites the first decision.
        l.mark_answered("r1", DecisionKind::Deny);
        assert_eq!(l.check("r1"), AnswerCheck::AlreadyAnswered(DecisionKind::AllowOnce));
        assert!(l.has_tool_use("toolu_1"));
    }

    #[test]
    fn a_cancel_after_the_answer_still_withdraws() {
        let mut l = Ledger::new();
        l.observe("r1", None);
        l.mark_answered("r1", DecisionKind::AllowOnce);
        assert_eq!(l.withdraw("r1"), Some(LedgerState::Answered(DecisionKind::AllowOnce)));
        assert_eq!(l.check("r1"), AnswerCheck::Withdrawn);
        assert_eq!(l.withdraw("nope"), None);
        assert_eq!(l.unknown_ids, 1);
    }

    #[test]
    fn exit_ends_pending_only() {
        let mut l = Ledger::new();
        l.observe("a", None);
        l.observe("b", None);
        l.mark_answered("b", DecisionKind::Deny);
        l.observe("c", None);
        assert_eq!(l.end_pending(), vec!["a".to_string(), "c".to_string()]);
        assert_eq!(l.check("a"), AnswerCheck::Withdrawn);
        assert_eq!(l.check("b"), AnswerCheck::AlreadyAnswered(DecisionKind::Deny));
    }

    #[test]
    fn the_ledger_is_bounded_but_never_drops_a_pending_id() {
        let mut l = Ledger::new();
        l.observe("keep", Some("t_keep"));
        for i in 0..LEDGER_CAP + 50 {
            let id = format!("r{i}");
            l.observe(&id, None);
            l.mark_answered(&id, DecisionKind::AllowOnce);
        }
        assert_eq!(l.len(), LEDGER_CAP);
        assert_eq!(l.check("keep"), AnswerCheck::Pending);
        assert_eq!(l.check("r0"), AnswerCheck::NotFound, "the oldest answered id was evicted");
        assert!(l.has_tool_use("t_keep"));
    }

    #[test]
    fn gilde_ids_never_collide_with_cli_ids() {
        let l = Ledger::new();
        let mut t = GildeTable::new();
        let id = t.mint(&l);
        assert!(crate::contract::ids::is_uuid(&id, Some(4)));
        let (tx, mut rx) = oneshot::channel();
        t.insert(id.clone(), GildeKind::GetUsage { baseline: false }, Some(tx));
        assert!(t.contains(&id));
        t.fail_all();
        assert_eq!(rx.try_recv().unwrap(), GildeReply::Gone);
        assert!(t.is_empty());
    }
}
