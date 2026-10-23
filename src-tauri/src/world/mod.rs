//! World (contract §3.3, §3.5, §3.6, §8.1, §7.1): the SessionGraph, identity resolution, `derive()`, episodes, the attention queue, team assembly and the store rev/patch publisher.
//!
//! Owner: T02. The `mod` lines below are pre-declared by T01 and frozen
//! after wave 0 (contract §15.1, F-18): a new module is a contract change.
//!
//! [`World`] is the facade: [`WorldApi`] (subscribe/snapshot/queue/marks/
//! identity lock/transcript), [`EvidenceSink`] through [`WorldApi::sink`],
//! the team-event sinks it hands to adapters, and T03's
//! [`UidResolver`](crate::discovery::history::UidResolver) (A11.4). Every
//! mutation is serialized under the World lock (§2.5); store patches are
//! coalesced and flushed at most every 100 ms by the publisher thread
//! ([`World::start`]), which otherwise sleeps until the earliest time-based
//! deadline (change-driven, §11.2).
//!
//! World never writes vendor or team files and never spawns anything: it
//! only consumes evidence. Titles and other content live in memory only.
//!
//! **Wiring (T09b, A10.6/A11/A12):** see [`World`]'s doc for the exact calls.

pub mod attention;
pub mod derive;
pub mod episodes;
pub mod graph;
pub mod identity;
pub mod patch;
pub mod teams;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::thread::JoinHandle;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::{Condvar, Mutex, RwLock};

use crate::contract::account::Account;
use crate::contract::activity::OpenToolUse;
use crate::contract::attention::AttentionQueue;
use crate::contract::common::{AbsPath, EpochMs, Vendor};
use crate::contract::compat::ClaudeCompat;
use crate::contract::error::IpcError;
use crate::contract::ids::{AccountId, MessageKey, ProjectId, SessionUid, TeamKey};
use crate::contract::ipc::{AttentionSettings, DismissWhat};
use crate::contract::services::{
    EvidenceSink, EvidenceUpdate, IdentityKey, IdentityLock, LifecycleOp, LocationFacts, NotifyApi, StorePatchSink,
    WorldApi,
};
use crate::contract::session::Session;
use crate::contract::store::{IntegrationsState, SourceStatus, StorePatch, StoreSnapshot};
use crate::contract::team::{
    DeliveryStatus, MessageKind, TeamAdapter, TeamEvent, TeamEventSink, TeamMessage, TeamSource,
};
use crate::discovery::history::UidResolver;

pub use graph::{
    ControlDetail, DetailsExtra, MemoryMetaStore, MetaRow, MetaStore, RefSource, SystemClock, TeamPatterns, WorldClock, WorldSettings,
};
#[cfg(any(test, feature = "test-fixtures", feature = "showcase"))]
pub use graph::ManualClock;
pub use teams::assembly::{TeamDetails, TeamHooks};

pub use teams::claude::{DeliveredBlock, InboxCycle, InboxStats, MessageEvidence, INBOX_ENTRIES_PER_CYCLE};

use graph::Graph;

/// A27.1: where one team message's body can be read on demand
/// ([`World::message_locator`]). Ids and times only, never content; the
/// reader (`follow::hub::MessageBodyReader`) opens the transcripts through
/// [`WorldApi::transcript`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageLocator {
    pub team: TeamKey,
    pub key: MessageKey,
    pub kind: MessageKind,
    pub status: DeliveryStatus,
    /// When World first observed it (Sent, Queued or Delivered time).
    pub at: EpochMs,
    pub sender: Option<SessionUid>,
    /// The name the transcripts use for the sender (`teammate_id`).
    pub sender_name: Option<String>,
    pub recipient: Option<SessionUid>,
    /// The name the transcripts use for the recipient (`SendMessage` `to`).
    pub recipient_name: Option<String>,
    /// The sender's `SendMessage` tool_use id, when its row was seen.
    pub tool_use_id: Option<String>,
    /// The recipient's delivering row and block, when it was seen.
    pub delivered: Option<DeliveredBlock>,
}

/// How World is built.
pub struct WorldConfig {
    pub clock: Arc<dyn WorldClock>,
    pub meta_store: Arc<dyn MetaStore>,
    pub settings: WorldSettings,
    /// The user's home directory (Details paths are shown as `~/…`).
    pub home: Option<String>,
}

impl WorldConfig {
    /// The system clocks with the given `session_meta` store.
    pub fn system(home: Option<String>, meta_store: Arc<dyn MetaStore>) -> WorldConfig {
        WorldConfig { clock: Arc::new(SystemClock), meta_store, settings: WorldSettings::default(), home }
    }
}

/// A48.2: queues reach `NotifyApi::queue_changed` in rev order. Each queue
/// is computed under the World lock but handed over after it is released, so
/// two threads can finish in either order; this hand-off drops a queue older
/// than one already taken and lets a single thread at a time make the calls
/// (a thread that finds another one handing over leaves its queue to it). No
/// lock is held during the call, so a `NotifyApi` that calls back into World
/// cannot deadlock here.
#[derive(Default)]
struct QueueHandoff {
    /// The newest rev taken (handed over or waiting).
    last_rev: u64,
    /// The newest queue not yet handed over.
    waiting: Option<AttentionQueue>,
    /// A thread is handing queues over.
    busy: bool,
}

struct Inner {
    graph: Mutex<Graph>,
    notify: RwLock<Option<Arc<dyn NotifyApi>>>,
    handoff: Mutex<QueueHandoff>,
    adapters: RwLock<Vec<Arc<dyn TeamAdapter>>>,
    locks: Mutex<HashMap<IdentityKey, Arc<tokio::sync::Mutex<()>>>>,
    wake_flag: Mutex<bool>,
    wake: Condvar,
    stop: AtomicBool,
    ticker: Mutex<Option<JoinHandle<()>>>,
    claude: Mutex<Option<Arc<teams::claude::ClaudeTeamAdapter>>>,
    orchestrator: Mutex<Option<Arc<teams::orchestrator::OrchestratorAdapter>>>,
    /// Mutations (each one refresh) since start: a structural counter
    /// (A33.5(d), A10.7: never wall time).
    mutations: AtomicU64,
}

impl Inner {
    /// Run a mutation under the World lock, then refresh; the queue observer
    /// is called after the lock is released.
    fn mutate<R>(&self, f: impl FnOnce(&mut Graph) -> R) -> R {
        self.mutations.fetch_add(1, Ordering::Relaxed);
        let (r, queue) = {
            let mut g = self.graph.lock();
            let r = f(&mut g);
            let q = g.refresh();
            (r, q)
        };
        if let Some(q) = queue {
            self.hand_off(q);
        }
        self.poke();
        r
    }

    /// Hands a queue computed under the lock to `NotifyApi::queue_changed`,
    /// in rev order, newest wins (A48.2, [`QueueHandoff`]).
    fn hand_off(&self, queue: AttentionQueue) {
        {
            let mut h = self.handoff.lock();
            if queue.rev <= h.last_rev {
                return;
            }
            h.last_rev = queue.rev;
            h.waiting = Some(queue);
            if h.busy {
                return;
            }
            h.busy = true;
        }
        // Clears `busy` if a `queue_changed` panics (a normal return clears it
        // under the lock, in the same step that finds nothing waiting).
        struct Busy<'a>(&'a Mutex<QueueHandoff>);
        impl Drop for Busy<'_> {
            fn drop(&mut self) {
                self.0.lock().busy = false;
            }
        }
        let busy = Busy(&self.handoff);
        loop {
            let next = {
                let mut h = self.handoff.lock();
                match h.waiting.take() {
                    Some(q) => q,
                    None => {
                        h.busy = false;
                        break;
                    }
                }
            };
            let n = self.notify.read().clone();
            if let Some(n) = n {
                n.queue_changed(&next);
            }
        }
        std::mem::forget(busy);
    }

    fn poke(&self) {
        let mut f = self.wake_flag.lock();
        *f = true;
        self.wake.notify_one();
    }

    /// One publisher step: time-based work, refresh, a flush when due.
    fn step(&self) -> (Option<StorePatch>, Option<AttentionQueue>) {
        let (patch, queue) = {
            let mut g = self.graph.lock();
            g.tick();
            let q = g.refresh();
            let up = g.clock.up_ms();
            let p = if g.publisher.due(up) { g.flush() } else { None };
            (p, q)
        };
        if let Some(q) = &queue {
            self.hand_off(q.clone());
        }
        (patch, queue)
    }

    /// How long the publisher thread may sleep.
    fn wait_ms(&self) -> u64 {
        let g = self.graph.lock();
        let up = g.clock.up_ms();
        let mut until = g.next_deadline().unwrap_or(up + 300_000);
        if !g.outbox.is_empty() {
            until = until.min(g.publisher.next_due(up));
        }
        until.saturating_sub(up).clamp(1, 300_000)
    }
}

/// The World facade.
///
/// **What T09b must call to wire World** (A10.6, A11.1/A11.4/A11.6, A12.1/A12.3):
/// 1. `let world = World::new(WorldConfig::system(Some(home), meta_store))` with
///    a `state.db`-backed [`MetaStore`] (`session_meta`, §3.9; memory-only mode:
///    [`MemoryMetaStore`]), then `world.start()` (the publisher thread).
/// 2. `Services::bind_world(Arc::new(world.clone()) as Arc<dyn WorldApi>)`;
///    `store_subscribe` → [`WorldApi::subscribe`]; `session_mark_seen` /
///    `session_dismiss` / `queue_later` → the `WorldApi` methods.
/// 3. Evidence: `world.sink()` into Discovery (`DiscoveryConfig` sink),
///    T04's `Follower::set_evidence_sink`, T05/T06/T08 drivers, T21's
///    `CompatService::set_on_change(move || sink.compat(compat.list()))` plus
///    one initial `sink.compat(..)`, T03's `on_orchestrator` hook →
///    `sink.integrations(IntegrationsState{orchestrator})`, T09's
///    `LifecycleFeed` → `sink.lifecycle(op)`, T12's pause changes →
///    `sink.notify_paused(until)`.
/// 4. Teams: PaneHost's `host.set_table_sink(world.claude_team_adapter())`
///    (A21.1); `DiscoveryConfig.teams = world.claude_team_adapter()` (T03's
///    `TeamObserver`, A11.1) and `Follower::set_row_sink(world.claude_team_adapter())`
///    (A10.5), with `follow_structure` for every live Claude session's
///    transcript (leads and pane members included); the sidecar's `orch.snapshot`
///    notifications → `world.orchestrator_adapter().apply_snapshot(..)`.
/// 5. History: `HistoryService` gets `world.uid_resolver()` (A11.4).
/// 6. Notifications: `world.set_notify(notify_api)` (T12) — every queue change
///    calls `NotifyApi::queue_changed`.
/// 7. Side inputs: `world.set_attention_settings(&settings.attention)` on boot
///    and on every `settings_set`; `world.set_refs(RefSource::View, ..)` on
///    `app_view_state`, `RefSource::Split` from `ui.splitSet`,
///    `RefSource::QuitRecord` from unresolved quit records;
///    T12's `NotifyService::on_pause_change` → `sink.notify_paused(..)` (A16.1);
///    `world.set_project_order(..)` / `world.set_project_folds(..)` from
///    `ui.sidebarOrder` / `ui.sidebarFolds`;
///    `world.time_zone_changed()` on `NSSystemTimeZoneDidChange`.
/// 9. `world.set_notify(..)` is the A16.1 setter: World calls
///    `NotifyApi::queue_changed` after every queue change, outside its lock.
/// 8. Sessions (T10): before a spawn `world.bind_spawn(spawn_id, uid, vendor,
///    account)` (Resume passes the existing uid; new sessions `None` and use the
///    returned uid as `SpawnRequest.parent_uid`), or `uid_resolver().uid_for(key)`
///    when the vendor id is chosen up front (`--session-id`); after the PTY/
///    transport exists `world.set_control_detail(uid, ControlDetail{pty, transport,
///    hosted_team})` (T07 too, with each hosted pane member's `PtyId`), and
///    `world.set_queued(uid, list)` whenever a driver's queue changes (A18.3),
///    and `world.set_details_extra(uid, DetailsExtra{..})` from its
///    `ConvoHooks` implementation (A19.1).
///    T10 and T07 receive the concrete `Arc<World>` (A18.1). A copy (Fork,
///    Continue on another account) → `world.set_parent(copy, source)` (A24.2);
///    Continue fills `ConvoSpec.abandoned` from `world.open_tool_use(uid)`
///    (A24.3, F-05).
/// 10. Projects (T11, A24.4): `project_pick` → `world.register_project(facts)`
///     (the `Arc<World>` the composition root manages as Tauri state).
/// 11. Team message bodies (A27.3): `TeamCommands::set_bodies(..)` with a
///     `follow::hub::MessageBodySource` over `world.message_locator(..)`
///     (A27.1) and the transcript reader (A27.2).
#[derive(Clone)]
pub struct World {
    inner: Arc<Inner>,
}

impl World {
    pub fn new(cfg: WorldConfig) -> World {
        let graph = Graph::new(cfg.clock, cfg.meta_store, cfg.settings, cfg.home);
        let inner = Arc::new(Inner {
            graph: Mutex::new(graph),
            notify: RwLock::new(None),
            handoff: Mutex::new(QueueHandoff::default()),
            adapters: RwLock::new(Vec::new()),
            locks: Mutex::new(HashMap::new()),
            wake_flag: Mutex::new(false),
            wake: Condvar::new(),
            stop: AtomicBool::new(false),
            ticker: Mutex::new(None),
            claude: Mutex::new(None),
            orchestrator: Mutex::new(None),
            mutations: AtomicU64::new(0),
        });
        let world = World { inner };
        let hooks: Arc<dyn TeamHooks> = Arc::new(Hooks { inner: Arc::downgrade(&world.inner) });
        let claude = Arc::new(teams::claude::ClaudeTeamAdapter::new(hooks.clone()));
        let orch = Arc::new(teams::orchestrator::OrchestratorAdapter::new(hooks));
        *world.inner.claude.lock() = Some(claude.clone());
        *world.inner.orchestrator.lock() = Some(orch.clone());
        world.register_team_adapter(claude);
        world.register_team_adapter(orch);
        world
    }

    /// Start the publisher thread (flushes ≤ 10 patches per second, runs the
    /// time-based rules). Idempotent.
    pub fn start(&self) {
        let mut t = self.inner.ticker.lock();
        if t.is_some() {
            return;
        }
        let weak = Arc::downgrade(&self.inner);
        let handle = std::thread::Builder::new()
            .name("world-publisher".into())
            .spawn(move || loop {
                let Some(inner) = weak.upgrade() else { return };
                if inner.stop.load(Ordering::SeqCst) {
                    return;
                }
                let wait = inner.wait_ms();
                {
                    let mut f = inner.wake_flag.lock();
                    if !*f {
                        inner.wake.wait_for(&mut f, Duration::from_millis(wait));
                    }
                    *f = false;
                }
                if inner.stop.load(Ordering::SeqCst) {
                    return;
                }
                inner.step();
            })
            .expect("spawn the World publisher thread");
        *t = Some(handle);
    }

    /// Stop the publisher thread (quit).
    pub fn shutdown(&self) {
        self.inner.stop.store(true, Ordering::SeqCst);
        self.inner.poke();
        if let Some(h) = self.inner.ticker.lock().take() {
            let _ = h.join();
        }
    }

    /// One publisher step now (tests and the showcase drive World with a
    /// manual clock instead of the thread). Returns the patch it sent.
    pub fn pump(&self) -> Option<StorePatch> {
        self.inner.step().0
    }

    /// Run the time-based rules and flush regardless of the 100 ms limit.
    pub fn flush_now(&self) -> Option<StorePatch> {
        let (p, queue) = {
            let mut g = self.inner.graph.lock();
            g.tick();
            let q = g.refresh();
            (g.flush(), q)
        };
        if let Some(q) = queue {
            self.inner.hand_off(q);
        }
        p
    }

    pub fn set_notify(&self, notify: Arc<dyn NotifyApi>) {
        *self.inner.notify.write() = Some(notify);
    }

    pub fn set_attention_settings(&self, s: &AttentionSettings) {
        let settings = WorldSettings {
            no_progress_after_secs: derive::clamp_no_progress(s.no_progress_after_secs),
            watched_band5_allowed: s.watched_finished_turns,
        };
        self.inner.mutate(|g| g.set_settings(settings));
    }

    pub fn set_settings(&self, s: WorldSettings) {
        self.inner.mutate(|g| g.set_settings(s));
    }

    /// The prompt attribution patterns (`agent-signatures.json`).
    pub fn set_team_patterns(&self, p: TeamPatterns) {
        self.inner.mutate(|g| g.patterns = p);
    }

    /// References that keep sessions in the store (§5.9).
    pub fn set_refs(&self, source: RefSource, uids: Vec<SessionUid>) {
        self.inner.mutate(|g| g.set_refs(source, uids));
    }

    pub fn set_paused_until(&self, until: Option<EpochMs>) {
        self.inner.mutate(|g| g.set_paused_until(until));
    }

    pub fn set_project_order(&self, order: Vec<ProjectId>) {
        self.inner.mutate(|g| g.set_project_order(order));
    }

    /// `ui.sidebarFolds`: the folded projects.
    pub fn set_project_folds(&self, folded: Vec<ProjectId>) {
        self.inner.mutate(|g| g.set_project_folds(folded));
    }

    /// `NSSystemTimeZoneDidChange`: "ended today" is recomputed (§10.4).
    pub fn time_zone_changed(&self) {
        self.inner.mutate(|g| g.time_zone_changed());
    }

    /// Bind a Gilde spawn id to a session before the spawn (identity rule 1).
    /// `uid` = the session being resumed; `None` mints a new uid.
    pub fn bind_spawn(&self, spawn_id: &str, uid: Option<&SessionUid>, vendor: Vendor, account: Option<AccountId>) -> SessionUid {
        self.inner.mutate(|g| g.bind_spawn(spawn_id, uid, vendor, account))
    }

    /// A18.3: a session's queued messages (Conversation sends while a turn
    /// runs; a turn-based session's one held message) — the full current
    /// list, oldest first; empty clears. **[content]**, memory only.
    pub fn set_queued(&self, uid: &SessionUid, list: Vec<crate::contract::session::QueuedMessage>) {
        self.inner.mutate(|g| g.set_queued(uid, list));
    }

    /// A35.1: the permission mode the Conversation driver reports (`None` =
    /// a mode Gilde does not know, A33.3); it overrides the metadata while
    /// Gilde runs the session in Conversation mode, until the next report.
    pub fn set_permission_mode(&self, uid: &SessionUid, mode: Option<crate::contract::common::PermissionMode>) {
        self.inner.mutate(|g| g.set_permission_mode(uid, mode));
    }

    /// A19.1: a session's driver-only Details facts (the unconfirmed
    /// approval channel, the version note, the `convo.notReading` /
    /// `convo.unanswerable` lines); each call fully replaces the previous one.
    /// Never persisted. The first line is the session's evidence line while
    /// it runs.
    pub fn set_details_extra(&self, uid: &SessionUid, extra: DetailsExtra) {
        self.inner.mutate(|g| g.set_details_extra(uid, extra));
    }

    /// The PTY id, transport and hosted flag of a Gilde session (A18.1: T10
    /// for its sessions, T07 for hosted pane members).
    pub fn set_control_detail(&self, uid: &SessionUid, detail: ControlDetail) {
        self.inner.mutate(|g| g.set_control_detail(uid, detail));
    }

    /// A24.2: `uid` was copied from `parent` (Fork, Continue on another
    /// account; §3.3.3); persisted as `session_meta.parent_uid` with the
    /// session's row (once it has a vendor id). `false` = refused: an unknown
    /// uid, a session as its own parent, or a loop.
    pub fn set_parent(&self, uid: &SessionUid, parent: SessionUid) -> bool {
        self.inner.mutate(|g| g.set_parent(uid, &parent))
    }

    /// A24.2: the recorded parent of a session (its live uid).
    pub fn parent_of(&self, uid: &SessionUid) -> Option<SessionUid> {
        self.inner.graph.lock().parent_of(uid)
    }

    /// A24.3 (F-05): the tool call the session's transcript ends in (id +
    /// name from its transcript facts), for `ConvoSpec.abandoned` on Continue.
    pub fn open_tool_use(&self, uid: &SessionUid) -> Option<OpenToolUse> {
        self.inner.graph.lock().open_tool_use(uid)
    }

    /// A24.4: a picked folder is a project at once — the project and its
    /// location are in the store when this returns, with or without a
    /// session, and stay. `facts` come from discovery's no-spawn identity
    /// (`discovery::location::identify`, §5.6).
    pub fn register_project(&self, facts: LocationFacts) -> ProjectId {
        self.inner.mutate(|g| g.register_project(facts))
    }

    /// T03's `TeamObserver` and T04's `RowSink` (the Claude native adapter).
    pub fn claude_team_adapter(&self) -> Arc<teams::claude::ClaudeTeamAdapter> {
        self.inner.claude.lock().clone().expect("built in World::new")
    }

    /// World mutations since start (each runs one refresh): the structural
    /// counter of A33.5(d) — e.g. a team observation's cost, never wall time.
    pub fn mutations(&self) -> u64 {
        self.inner.mutations.load(Ordering::Relaxed)
    }

    /// The read-only orchestrator mapping (§7.3.1).
    pub fn orchestrator_adapter(&self) -> Arc<teams::orchestrator::OrchestratorAdapter> {
        self.inner.orchestrator.lock().clone().expect("built in World::new")
    }

    /// T03's `history::UidResolver`, implemented by World (A11.4).
    pub fn uid_resolver(&self) -> Arc<dyn UidResolver> {
        Arc::new(self.clone())
    }

    /// `team_messages_page`: newest first, before `before`.
    pub fn team_messages_page(&self, team: &TeamKey, before: EpochMs, count: u16) -> (Vec<TeamMessage>, bool) {
        let g = self.inner.graph.lock();
        g.teams.messages_page(team, before, usize::from(count.clamp(1, 500)))
    }

    /// A27.1: where a team message's body can be read on demand — its team,
    /// both ends' sessions and native names, the observed time and status,
    /// and the transcript evidence the Claude adapter saw (the sender's
    /// `SendMessage` tool_use id, the recipient's delivering row and block).
    /// Ids and times only: World never holds message text. `None` for a
    /// message World does not (or no longer) hold.
    pub fn message_locator(&self, key: &MessageKey) -> Option<MessageLocator> {
        // The World lock is released before the adapter is asked (lock order).
        let ends = self.inner.graph.lock().teams.message_ends(key)?;
        let evidence = match ends.source {
            TeamSource::ClaudeNative => self.claude_team_adapter().message_evidence(&ends.message.team_key, key).unwrap_or_default(),
            TeamSource::Orchestrator | TeamSource::Emulated => MessageEvidence::default(),
        };
        let m = ends.message;
        Some(MessageLocator {
            team: m.team_key,
            key: m.key,
            kind: m.kind,
            status: m.status,
            at: m.at,
            sender: ends.from.session,
            sender_name: ends.from.name,
            recipient: ends.to.session,
            recipient_name: ends.to.name,
            tool_use_id: evidence.tool_use_id,
            delivered: evidence.delivered,
        })
    }

    /// The store rev (diagnostics, tests).
    pub fn rev(&self) -> u64 {
        self.inner.graph.lock().publisher.rev()
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// WorldApi
// ---------------------------------------------------------------------------

#[async_trait]
impl WorldApi for World {
    fn sink(&self) -> Arc<dyn EvidenceSink> {
        Arc::new(WorldSink { inner: Arc::downgrade(&self.inner) })
    }

    fn subscribe(&self, sink: Box<dyn StorePatchSink>) -> StoreSnapshot {
        let (snap, queue) = {
            let mut g = self.inner.graph.lock();
            let q = g.refresh();
            let mut outbox = std::mem::take(&mut g.outbox);
            g.publisher.replace_sink(sink, &mut outbox);
            g.outbox = outbox;
            (g.snapshot(), q)
        };
        if let Some(q) = queue {
            self.inner.hand_off(q);
        }
        snap
    }

    fn snapshot(&self) -> StoreSnapshot {
        self.inner.graph.lock().snapshot()
    }

    fn session(&self, uid: &SessionUid) -> Option<Session> {
        self.inner.graph.lock().session(uid)
    }

    fn queue(&self) -> AttentionQueue {
        self.inner.graph.lock().queue.clone()
    }

    async fn identity_lock(&self, key: IdentityKey) -> IdentityLock {
        let m = {
            let mut locks = self.inner.locks.lock();
            locks.retain(|_, v| Arc::strong_count(v) > 1);
            locks.entry(key).or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))).clone()
        };
        m.lock_owned().await
    }

    fn mark_seen(&self, uid: &SessionUid) -> Result<(), IpcError> {
        self.inner.mutate(|g| g.mark_seen(uid))
    }

    fn dismiss(&self, uid: &SessionUid, what: DismissWhat) -> Result<(), IpcError> {
        self.inner.mutate(|g| g.dismiss(uid, what))
    }

    fn queue_later(&self, uid: &SessionUid) -> Result<EpochMs, IpcError> {
        self.inner.mutate(|g| g.queue_later(uid))
    }

    fn register_team_adapter(&self, adapter: Arc<dyn TeamAdapter>) {
        let sink: Arc<dyn TeamEventSink> = Arc::new(AdapterSink {
            inner: Arc::downgrade(&self.inner),
            source: adapter.source(),
            adapter: Arc::downgrade(&adapter),
        });
        self.inner.adapters.write().push(adapter.clone());
        // Outside the World lock: an adapter may emit from `subscribe`.
        adapter.subscribe(sink);
    }

    fn transcript(&self, uid: &SessionUid) -> Option<(AbsPath, Vendor)> {
        self.inner.graph.lock().transcript(uid)
    }
}

impl UidResolver for World {
    fn uid_for(&self, key: &IdentityKey) -> SessionUid {
        self.inner.mutate(|g| g.uid_for_key(key))
    }

    fn key_of(&self, uid: &SessionUid) -> Option<IdentityKey> {
        self.inner.graph.lock().key_of(uid)
    }
}

// ---------------------------------------------------------------------------
// sinks
// ---------------------------------------------------------------------------

/// `EvidenceSink` of World (bounded: one short critical section per call;
/// never fails visibly — a dropped World makes it a no-op).
struct WorldSink {
    inner: Weak<Inner>,
}

impl WorldSink {
    fn with(&self, f: impl FnOnce(&mut Graph)) {
        if let Some(inner) = self.inner.upgrade() {
            inner.mutate(f);
        }
    }
}

impl EvidenceSink for WorldSink {
    fn evidence(&self, update: EvidenceUpdate) {
        self.with(|g| g.apply_evidence(update));
    }
    fn location(&self, facts: LocationFacts) {
        self.with(|g| g.apply_location(facts));
    }
    fn source_status(&self, status: SourceStatus) {
        self.with(|g| g.apply_source_status(status));
    }
    fn account(&self, account: Account) {
        self.with(|g| g.apply_account(account));
    }
    fn compat(&self, list: Vec<ClaudeCompat>) {
        self.with(|g| g.apply_compat(list));
    }
    fn integrations(&self, state: IntegrationsState) {
        self.with(|g| g.apply_integrations(state));
    }
    fn lifecycle(&self, op: LifecycleOp) {
        self.with(|g| g.apply_lifecycle(op));
    }
    /// A16.1: the notification pause sets `AttentionQueue.paused_until` and
    /// emits the `Queue` op (`attention::PAUSED_UNTIL_RESUME` = until resumed).
    fn notify_paused(&self, until: Option<EpochMs>) {
        self.with(|g| g.set_paused_until(until));
    }
}

/// The team-event sink of one adapter. The descriptor of a `TeamSeen` is read
/// from the adapter before World is locked (lock order, see `assembly`).
struct AdapterSink {
    inner: Weak<Inner>,
    source: TeamSource,
    adapter: Weak<dyn TeamAdapter>,
}

impl TeamEventSink for AdapterSink {
    fn emit(&self, event: TeamEvent) {
        let Some(inner) = self.inner.upgrade() else { return };
        let caps = match &event {
            TeamEvent::TeamSeen { key, .. } => self.adapter.upgrade().map(|a| a.capabilities(key)),
            _ => None,
        };
        let source = self.source;
        inner.mutate(|g| g.apply_team_event(source, event, caps));
    }
}

/// What World offers its own adapters (lookups and adapter-side details).
struct Hooks {
    inner: Weak<Inner>,
}

impl TeamHooks for Hooks {
    fn uid_for(&self, key: &IdentityKey) -> Option<SessionUid> {
        let inner = self.inner.upgrade()?;
        Some(inner.mutate(|g| g.uid_for_key(key)))
    }
    fn uid_by_pid(&self, pid: i32) -> Option<SessionUid> {
        let inner = self.inner.upgrade()?;
        let g = inner.graph.lock();
        g.uid_by_pid(pid)
    }
    fn team_details(&self, key: &TeamKey, source: TeamSource, details: TeamDetails) {
        if let Some(inner) = self.inner.upgrade() {
            inner.mutate(|g| g.set_team_details(key, source, details));
        }
    }
    fn session_facts(&self, uid: &SessionUid, facts: crate::contract::services::SourceFacts) {
        if let Some(inner) = self.inner.upgrade() {
            inner.mutate(|g| g.apply_session_facts(uid, facts));
        }
    }
    fn session_lines(&self, uid: &SessionUid, lines: Vec<crate::contract::activity::EvidenceLine>) {
        if let Some(inner) = self.inner.upgrade() {
            inner.mutate(|g| g.set_pane_lines(uid, lines));
        }
    }
    fn team_events(
        &self,
        source: TeamSource,
        events: Vec<(TeamEvent, Option<crate::contract::team::TeamCapabilities>)>,
    ) -> Option<Vec<(TeamEvent, Option<crate::contract::team::TeamCapabilities>)>> {
        // A dropped World makes it a no-op, as for every sink.
        let inner = self.inner.upgrade()?;
        inner.mutate(|g| {
            for (e, caps) in events {
                g.apply_team_event(source, e, caps);
            }
        });
        None
    }
}
