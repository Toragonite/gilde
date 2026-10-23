//! Shared rig for the `world_*` integration tests (included with
//! `#[path = "world_support.rs"] mod support;`). Synthetic facts only: no
//! vendor CLI, no vendor directory, no file is read or written.
#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;

use gilde_code_lib::contract::activity::{
    ActivityState, GildeFacts, ProcessFacts, PtyFacts, PtyStatus, RegStatus, RegistryFacts, ScreenOptionFact,
    ScreenPromptFacts, StreamFacts, TeamFileFacts, TranscriptFacts, TurnPhase,
};
use gilde_code_lib::contract::attention::{AttentionQueue, AttentionSubject};
use gilde_code_lib::contract::common::{AbsPath, EpochMs, Vendor};
use gilde_code_lib::contract::ids::{AccountId, ProcessKey, SessionUid};
use gilde_code_lib::contract::ipc::{PauseUntil, ViewState};
use gilde_code_lib::contract::services::{
    EvidenceSink, EvidenceUpdate, IdentityHint, NotifyApi, SessionMetaFacts, SourceFacts, StorePatchSink, WorldApi,
};
use gilde_code_lib::contract::session::{ControlMode, Session};
use gilde_code_lib::contract::store::{Entity, StoreOp, StorePatch, StoreSnapshot, VisibilityEvent};
use gilde_code_lib::world::{ManualClock, MemoryMetaStore, World, WorldConfig, WorldSettings};

/// 2026-10-23T10:00:00Z.
pub const T0: EpochMs = 1_792_749_600_000;
pub const UP0: u64 = 50_000_000;

pub fn account() -> AccountId {
    AccountId::derive(Vendor::Claude, "/Users/tester/.claude")
}

pub fn sid(n: u32) -> String {
    format!("{n:08x}-0000-4000-8000-000000000001")
}

/// Records every patch and checks the rev chain.
#[derive(Clone, Default)]
pub struct Patches(pub Arc<Mutex<Vec<StorePatch>>>);

impl StorePatchSink for Patches {
    fn send(&self, patch: StorePatch) -> bool {
        self.0.lock().push(patch);
        true
    }
}

/// A NotifyApi that records queue changes.
#[derive(Default)]
pub struct RecNotify {
    pub queues: Mutex<Vec<AttentionQueue>>,
}

#[async_trait]
impl NotifyApi for RecNotify {
    fn view_state(&self, _state: ViewState) {}
    fn input_activity(&self) {}
    fn queue_changed(&self, queue: &AttentionQueue) {
        self.queues.lock().push(queue.clone());
    }
    fn visibility(&self) -> VisibilityEvent {
        VisibilityEvent { frontmost: true, occluded: false, asleep: false }
    }
    fn pause(&self, _until: PauseUntil) -> Option<EpochMs> {
        None
    }
    fn resume(&self) {}
    async fn permission_request(&self) -> bool {
        false
    }
    async fn test(&self) -> bool {
        false
    }
}

/// The WebView's mirror of the store: snapshot + patches.
#[derive(Default)]
pub struct Mirror {
    pub rev: u64,
    pub sessions: BTreeMap<String, Session>,
    pub queue: Option<AttentionQueue>,
    pub entities: HashMap<String, Entity>,
    pub resyncs: u32,
    pub other: Vec<StoreOp>,
}

impl Mirror {
    pub fn from_snapshot(s: &StoreSnapshot) -> Mirror {
        let mut m = Mirror { rev: s.rev, queue: Some(s.queue.clone()), ..Mirror::default() };
        for x in &s.sessions {
            m.sessions.insert(x.uid.as_str().to_string(), x.clone());
        }
        m
    }

    /// Apply one patch; panics on a rev gap (the WebView would resubscribe).
    pub fn apply(&mut self, p: &StorePatch) {
        assert_eq!(p.from_rev, self.rev, "rev gap: mirror at {}, patch from {}", self.rev, p.from_rev);
        self.rev = p.to_rev;
        for op in &p.ops {
            match op {
                StoreOp::Upsert { entity } => match entity.as_ref() {
                    Entity::Session { value } => {
                        self.sessions.insert(value.uid.as_str().to_string(), (**value).clone());
                    }
                    other => {
                        let (k, id) = gilde_code_lib::world::patch::entity_key(other);
                        self.entities.insert(format!("{k:?}:{id}"), other.clone());
                    }
                },
                StoreOp::Remove { entity_kind, id } => {
                    if *entity_kind == gilde_code_lib::contract::store::EntityKind::Session {
                        self.sessions.remove(id);
                    } else {
                        self.entities.remove(&format!("{entity_kind:?}:{id}"));
                    }
                }
                StoreOp::Queue { queue } => self.queue = Some(queue.clone()),
                StoreOp::Resync => self.resyncs += 1,
                o => self.other.push(o.clone()),
            }
        }
    }

    /// Counter == sidebar (§3.4.9, pixel-office I6): the queue counts equal
    /// what the mirror's sessions show.
    pub fn assert_counts(&self) {
        let Some(q) = &self.queue else { return };
        let mut need = 0;
        let mut also = 0;
        for it in &q.items {
            if it.later_until.is_some() {
                continue;
            }
            if it.band <= 2 {
                need += 1;
            } else {
                also += 1;
            }
            if let AttentionSubject::Session { uid, .. } = &it.subject {
                let s = self.sessions.get(uid.as_str()).expect("queue item for a session in the store");
                let band = gilde_code_lib::world::derive::band(&s.activity.state);
                assert_eq!(band, Some(it.band), "item band equals the session's band");
            }
        }
        assert_eq!(q.need_you, need, "need_you equals the sidebar count");
        assert_eq!(q.also, also, "also equals the sidebar count");
        // Every session in bands 1–6 has exactly one item, unless it is a
        // lead represented by its members' items (Q3).
        for s in self.sessions.values() {
            if s.merged_into.is_some() {
                continue;
            }
            if let Some(b) = gilde_code_lib::world::derive::band(&s.activity.state) {
                let n = q
                    .items
                    .iter()
                    .filter(|i| matches!(&i.subject, AttentionSubject::Session { uid, .. } if uid == &s.uid))
                    .count();
                let represented =
                    !s.pending.is_empty() && s.pending.iter().all(|p| p.on_behalf_of.is_some() && p.answer_in.is_none());
                assert!(n == 1 || (n == 0 && represented), "session in band {b} has {n} items");
            }
        }
    }
}

pub struct Rig {
    pub world: World,
    pub clock: Arc<ManualClock>,
    pub sink: Arc<dyn EvidenceSink>,
    pub patches: Patches,
    pub meta: Arc<MemoryMetaStore>,
    pub notify: Arc<RecNotify>,
    pub mirror: Mutex<Option<Mirror>>,
    seen: Mutex<usize>,
}

impl Default for Rig {
    fn default() -> Rig {
        Rig::new()
    }
}

impl Rig {
    pub fn new() -> Rig {
        Rig::with_meta(Arc::new(MemoryMetaStore::default()))
    }

    pub fn with_meta(meta: Arc<MemoryMetaStore>) -> Rig {
        let clock = Arc::new(ManualClock::new(T0, UP0));
        let world = World::new(WorldConfig {
            clock: clock.clone(),
            meta_store: meta.clone(),
            settings: WorldSettings::default(),
            home: Some("/Users/tester".into()),
        });
        let notify = Arc::new(RecNotify::default());
        world.set_notify(notify.clone());
        let sink = world.sink();
        Rig { world, clock, sink, patches: Patches::default(), meta, notify, mirror: Mutex::new(None), seen: Mutex::new(0) }
    }

    /// Subscribe and start mirroring.
    pub fn subscribe(&self) -> StoreSnapshot {
        let snap = self.world.subscribe(Box::new(self.patches.clone()));
        *self.mirror.lock() = Some(Mirror::from_snapshot(&snap));
        *self.seen.lock() = self.patches.0.lock().len();
        snap
    }

    pub fn now(&self) -> EpochMs {
        use gilde_code_lib::world::WorldClock;
        self.clock.wall_ms()
    }

    pub fn advance(&self, ms: u64) {
        self.clock.advance(ms);
    }

    /// Flush and apply every new patch to the mirror (checking counts).
    pub fn pump(&self) -> Vec<StorePatch> {
        self.world.flush_now();
        let new: Vec<StorePatch> = {
            let all = self.patches.0.lock();
            let mut seen = self.seen.lock();
            let v = all[*seen..].to_vec();
            *seen = all.len();
            v
        };
        if let Some(m) = self.mirror.lock().as_mut() {
            for p in &new {
                m.apply(p);
                m.assert_counts();
            }
        }
        new
    }

    pub fn ev(&self, hint: &IdentityHint, facts: SourceFacts) {
        self.sink.evidence(EvidenceUpdate { hint: hint.clone(), facts, at: self.now() });
    }

    pub fn session(&self, uid: &SessionUid) -> Session {
        self.world.session(uid).expect("session exists")
    }

    pub fn state(&self, uid: &SessionUid) -> ActivityState {
        self.session(uid).activity.state
    }

    /// The uid World resolved for a hint (by key).
    pub fn uid_of(&self, vsid: &str) -> SessionUid {
        let key = gilde_code_lib::contract::services::IdentityKey {
            vendor: Vendor::Claude,
            account_id: Some(account()),
            vendor_session_id: vsid.to_string(),
        };
        self.world.uid_resolver().uid_for(&key)
    }

    pub fn snapshot_uids(&self) -> Vec<SessionUid> {
        self.world.snapshot().sessions.into_iter().map(|s| s.uid).collect()
    }
}

// ---------------------------------------------------------------------------
// hints and facts
// ---------------------------------------------------------------------------

pub fn claude_hint(vsid: Option<&str>, pid: Option<u32>) -> IdentityHint {
    IdentityHint {
        vendor: Some(Vendor::Claude),
        account_id: Some(account()),
        vendor_session_id: vsid.map(str::to_string),
        process: pid.map(|p| ProcessKey::new(p, 1_792_700_000)),
        gilde_spawn_id: None,
    }
}

pub fn spawn_hint(spawn: &str, vsid: Option<&str>, pid: Option<u32>) -> IdentityHint {
    IdentityHint { gilde_spawn_id: Some(spawn.to_string()), ..claude_hint(vsid, pid) }
}

pub fn alive(pid: i32) -> SourceFacts {
    SourceFacts::Process(ProcessFacts::Alive { pid, start_sec: 1_792_700_000, cpu_active_at: None })
}

pub fn exited(at: EpochMs, code: Option<i32>, signal: Option<i32>) -> SourceFacts {
    SourceFacts::Process(ProcessFacts::Exited { at, code, signal })
}

pub fn gone(at: EpochMs) -> SourceFacts {
    SourceFacts::Process(ProcessFacts::Gone { at })
}

pub fn registry(status: RegStatus, updated: EpochMs, waiting_for: Option<&str>) -> SourceFacts {
    SourceFacts::Registry(RegistryFacts {
        status: Some(status),
        waiting_for: waiting_for.map(str::to_string),
        status_updated_at: Some(updated),
        peer_protocol_ok: true,
    })
}

pub fn transcript(last_row: EpochMs, prompt: Option<EpochMs>, end: Option<EpochMs>) -> TranscriptFacts {
    TranscriptFacts { last_row_at: Some(last_row), open_tool_use: None, last_turn_end_at: end, last_human_prompt_at: prompt, last_api_error: None }
}

pub fn gilde(spawned: EpochMs, mode: ControlMode, ready: Option<EpochMs>) -> GildeFacts {
    GildeFacts { spawned_at: spawned, mode, ready_at: ready, stop_cause: None, signals_sent: vec![], start_failure: None }
}

pub fn stream(phase: TurnPhase, at: EpochMs) -> StreamFacts {
    StreamFacts { phase, phase_since: at, pending: vec![], last_turn_start_at: None, last_turn_end: None, last_frame_at: at }
}

pub fn pty(status: PtyStatus, at: EpochMs) -> PtyFacts {
    PtyFacts { status, confidence: 0.9, last_output_at: Some(at), prompt: None }
}

pub fn prompt(rev: &str, permission: bool, header: Option<&str>, at: EpochMs) -> ScreenPromptFacts {
    let opts = if permission {
        vec![
            ScreenOptionFact { key: "1".into(), label: "Yes".into(), description: None, selected: true },
            ScreenOptionFact { key: "2".into(), label: "Yes, and switch to auto mode".into(), description: None, selected: false },
            ScreenOptionFact { key: "3".into(), label: "No".into(), description: None, selected: false },
        ]
    } else {
        vec![
            ScreenOptionFact { key: "1".into(), label: "Postgres".into(), description: None, selected: true },
            ScreenOptionFact { key: "2".into(), label: "SQLite".into(), description: None, selected: false },
        ]
    };
    ScreenPromptFacts {
        prompt_rev: rev.to_string(),
        question: Some(if permission { "Do you want to proceed?".into() } else { "Which database?".into() }),
        header: header.map(str::to_string),
        options: opts,
        footer: None,
        permission_shaped: permission,
        setup: false,
        trust_dialog: false,
        allows_note: false,
        near_bottom: true,
        first_seen_at: at,
    }
}

pub fn team_facts(active: Option<bool>) -> TeamFileFacts {
    TeamFileFacts { is_active: active, in_roster: true, permission_request: None, last_idle: None }
}

pub fn meta_cwd(cwd: &str) -> SourceFacts {
    SourceFacts::Meta(Box::new(SessionMetaFacts { cwd: Some(AbsPath::parse(cwd).unwrap()), ..SessionMetaFacts::default() }))
}
