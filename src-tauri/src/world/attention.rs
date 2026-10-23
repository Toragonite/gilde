//! The attention queue Q1–Q8 (§8.1), computed once in Rust.
//!
//! Owner: T02. Every surface renders the [`AttentionQueue`] as delivered
//! (sidebar header, `☰ n`, Home, dock badge = `need_you`, Floor counter), so
//! the counts are computed here and nowhere else (pixel-office I6).
//!
//! * Q1 membership by band (§3.5.7): 1 Needs approval, 2 Needs answer,
//!   3 Error / Crashed, 4 No progress, 5 New reply (Watching only when the
//!   setting allows — `derive` R9 already applies it), 6 leftovers.
//! * Q2 one item per session (its one state); a team is never an item.
//! * Q3 a hosted lead whose pending interactions all carry `on_behalf_of`
//!   gets no item when every such member has a session item of its own.
//! * Q4 bands ascending; bands 1, 2, 4 oldest `since` first (uptime), 3 and 5
//!   newest first; ties by uid.
//! * Q5 Later hides an item for 30 min (uptime, memory only); it returns
//!   earlier when the session moves to a more urgent band; a Later-ed item
//!   counts toward nothing.
//! * Q7 `need_you` = bands 1–2, `also` = bands 3–6, Later-ed excluded.
//! * Q8 one band-6 item for the leftover processes.

use std::collections::HashMap;

use crate::contract::activity::{ActivityState, PrimaryAction};
use crate::contract::attention::{AttentionItem, AttentionQueue, AttentionReason, AttentionSubject};
use crate::contract::common::EpochMs;
use crate::contract::ids::{hash16, EpisodeId, MemberKey, SessionUid};

/// Q5: Later hides an item for 30 min (uptime clock).
pub const LATER_MS: u64 = 30 * 60 * 1000;

/// One session as the queue sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct QueueSession {
    pub uid: SessionUid,
    pub state: ActivityState,
    pub band: Option<u8>,
    pub episode: EpisodeId,
    pub since: EpochMs,
    pub since_up: u64,
    pub primary: PrimaryAction,
    pub watched: bool,
    pub on_behalf_of: Option<MemberKey>,
    pub answer_in: Option<SessionUid>,
    /// Q3: a hosted lead represented by its members' items.
    pub suppressed: bool,
}

/// The aggregated leftover processes (Q8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LeftoverSummary {
    pub count: u32,
    pub bytes: u64,
    pub since: EpochMs,
    pub since_up: u64,
}

/// Q5 state: uid → (until on the uptime clock, until wall, band at Later).
#[derive(Debug, Clone, Default)]
pub struct LaterMap {
    map: HashMap<String, (u64, EpochMs, u8)>,
}

impl LaterMap {
    /// `queue_later`: hide the item for 30 min.
    pub fn set(&mut self, key: &str, band: u8, now_up: u64, now_wall: EpochMs) -> EpochMs {
        let until_wall = now_wall + LATER_MS as EpochMs;
        self.map.insert(key.to_string(), (now_up + LATER_MS, until_wall, band));
        until_wall
    }

    /// Drop expired entries and entries whose item left the queue or moved to
    /// a more urgent band. Returns whether anything changed.
    pub fn prune(&mut self, live: &HashMap<String, u8>, now_up: u64) -> bool {
        let before = self.map.len();
        self.map.retain(|k, (until, _, band)| {
            *until > now_up && live.get(k).is_some_and(|b| *b >= *band)
        });
        before != self.map.len()
    }

    fn get(&self, key: &str, now_up: u64) -> Option<EpochMs> {
        self.map.get(key).filter(|(until, _, _)| *until > now_up).map(|(_, w, _)| *w)
    }

    pub fn next_expiry(&self) -> Option<u64> {
        self.map.values().map(|(u, _, _)| *u).min()
    }
}

fn reason_of(state: &ActivityState) -> Option<AttentionReason> {
    Some(match state {
        ActivityState::NeedsApproval => AttentionReason::NeedsApproval,
        ActivityState::NeedsAnswer => AttentionReason::NeedsAnswer,
        ActivityState::Error { sub } => AttentionReason::Error { sub: sub.clone() },
        ActivityState::Crashed { .. } => AttentionReason::Crashed,
        ActivityState::NoProgress { for_secs } => AttentionReason::NoProgress { for_secs: *for_secs },
        ActivityState::NewReply => AttentionReason::NewReply,
        _ => return None,
    })
}

/// The item key of a session (§3.4.9).
pub fn item_key(uid: &SessionUid) -> String {
    format!("q_{}", uid.as_str())
}

/// The key of the leftovers item.
pub const LEFTOVERS_KEY: &str = "q_leftovers";

fn leftover_episode(since: EpochMs) -> EpisodeId {
    EpisodeId::parse(format!("e_{}", hash16(format!("leftovers\0{since}").as_bytes())))
        .expect("hash16 is 16 lowercase hex")
}

/// Compute the queue (Q1–Q8).
pub fn compute(
    sessions: &[QueueSession],
    leftovers: LeftoverSummary,
    later: &LaterMap,
    paused_until: Option<EpochMs>,
    rev: u64,
    now_up: u64,
) -> AttentionQueue {
    let mut items: Vec<(u8, u64, String, AttentionItem)> = Vec::new();
    for s in sessions {
        let (Some(band), Some(reason)) = (s.band, reason_of(&s.state)) else { continue };
        if s.suppressed {
            continue;
        }
        let key = item_key(&s.uid);
        let later_until = later.get(&key, now_up);
        items.push((
            band,
            s.since_up,
            s.uid.as_str().to_string(),
            AttentionItem {
                key,
                band,
                subject: AttentionSubject::Session {
                    uid: s.uid.clone(),
                    on_behalf_of: s.on_behalf_of.clone(),
                    answer_in: s.answer_in.clone(),
                },
                reason,
                since: s.since,
                waited_ms: now_up.saturating_sub(s.since_up),
                episode: s.episode.clone(),
                later_until,
                primary: s.primary,
                watched: s.watched,
            },
        ));
    }
    if leftovers.count > 0 {
        let later_until = later.get(LEFTOVERS_KEY, now_up);
        items.push((
            6,
            leftovers.since_up,
            String::new(),
            AttentionItem {
                key: LEFTOVERS_KEY.to_string(),
                band: 6,
                subject: AttentionSubject::Leftovers { count: leftovers.count, bytes: leftovers.bytes },
                reason: AttentionReason::Leftovers,
                since: leftovers.since,
                waited_ms: now_up.saturating_sub(leftovers.since_up),
                episode: leftover_episode(leftovers.since),
                later_until,
                primary: PrimaryAction::CleanUp,
                watched: false,
            },
        ));
    }
    // Q4.
    items.sort_by(|a, b| {
        a.0.cmp(&b.0).then_with(|| {
            let newest_first = matches!(a.0, 3 | 5);
            let ord = if newest_first { b.1.cmp(&a.1) } else { a.1.cmp(&b.1) };
            ord.then_with(|| a.2.cmp(&b.2))
        })
    });
    let mut need_you = 0u32;
    let mut also = 0u32;
    for (band, _, _, it) in &items {
        if it.later_until.is_some() {
            continue;
        }
        if *band <= 2 {
            need_you += 1;
        } else {
            also += 1;
        }
    }
    AttentionQueue { rev, items: items.into_iter().map(|x| x.3).collect(), need_you, also, paused_until }
}

/// Two queues are the same for publishing when they differ only in the
/// publish-time ages (`waited_ms`) and the rev.
pub fn same_queue(a: &AttentionQueue, b: &AttentionQueue) -> bool {
    a.need_you == b.need_you
        && a.also == b.also
        && a.paused_until == b.paused_until
        && a.items.len() == b.items.len()
        && a.items.iter().zip(&b.items).all(|(x, y)| {
            let mut x = x.clone();
            x.waited_ms = y.waited_ms;
            &x == y
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::activity::ErrorSub;

    fn s(state: ActivityState, band: u8, since_up: u64) -> QueueSession {
        let uid = SessionUid::mint();
        QueueSession {
            episode: EpisodeId::derive(&uid, "x", 1),
            uid,
            state,
            band: Some(band),
            since: 1_000 + since_up as i64,
            since_up,
            primary: PrimaryAction::Answer,
            watched: false,
            on_behalf_of: None,
            answer_in: None,
            suppressed: false,
        }
    }

    #[test]
    fn ranking_and_counts() {
        let a = s(ActivityState::NeedsApproval, 1, 50);
        let b = s(ActivityState::NeedsApproval, 1, 10);
        let c = s(ActivityState::Error { sub: ErrorSub::AgentError }, 3, 10);
        let d = s(ActivityState::Error { sub: ErrorSub::AgentError }, 3, 90);
        let q = compute(&[a.clone(), b.clone(), c.clone(), d.clone()], LeftoverSummary::default(), &LaterMap::default(), None, 7, 100);
        let order: Vec<&str> = q.items.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(order, vec![item_key(&b.uid), item_key(&a.uid), item_key(&d.uid), item_key(&c.uid)]);
        assert_eq!((q.need_you, q.also, q.rev), (2, 2, 7));
    }

    #[test]
    fn later_counts_toward_nothing_and_returns_on_a_more_urgent_band() {
        let a = s(ActivityState::NoProgress { for_secs: 300 }, 4, 0);
        let mut later = LaterMap::default();
        later.set(&item_key(&a.uid), 4, 0, 1_000);
        let q = compute(std::slice::from_ref(&a), LeftoverSummary::default(), &later, None, 1, 10);
        assert_eq!(q.also, 0);
        assert!(q.items[0].later_until.is_some());
        let live: HashMap<String, u8> = [(item_key(&a.uid), 1u8)].into_iter().collect();
        assert!(later.prune(&live, 20));
        assert!(later.get(&item_key(&a.uid), 20).is_none());
    }

    #[test]
    fn suppressed_lead_and_leftovers() {
        let mut lead = s(ActivityState::NeedsApproval, 1, 0);
        lead.suppressed = true;
        let q = compute(
            &[lead],
            LeftoverSummary { count: 2, bytes: 1_000, since: 5, since_up: 5 },
            &LaterMap::default(),
            None,
            1,
            10,
        );
        assert_eq!(q.items.len(), 1);
        assert_eq!(q.items[0].key, LEFTOVERS_KEY);
        assert_eq!((q.need_you, q.also), (0, 1));
    }
}
