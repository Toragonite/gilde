//! Store glue (contract §4.2.1, §4.3): the store channel and UI state.
//!
//! Owner: T09.
//!
//! `store_subscribe` wraps the Tauri `Channel<StorePatch>` in the edge sink of
//! `events::ChannelPatchSink` (rev order, ≤ 10 patches/s, same-entity
//! collapse, `Resync` above 2,000 ops) and hands it to World (T02), which
//! takes the snapshot and starts the patch stream atomically under its lock;
//! a second call replaces the first channel. World's snapshot already holds
//! T09's own store state (boot, notices, leftovers, quit progress): the
//! lifecycle feed sends it there once World is bound (A12.1).
//!
//! Until World is bound the store is served locally: a snapshot of what T09
//! holds, and its own ops as patches with a local rev. Never both.

use std::sync::Arc;

use parking_lot::Mutex;

use crate::commands::app::{state_of, LifecycleState, ServicesState};
use crate::contract::attention::AttentionQueue;
use crate::contract::ipc::{StoreSubscribeReq, UiStateSetReq, UiStateSetRes};
use crate::contract::store::{
    IntegrationsState, OrchestratorPresence, StoreOp, StorePatch, StoreSnapshot,
};
use crate::contract::{IpcError, Req};
use crate::events::{ChannelPatchSink, LifecycleFeed, PatchOut, StoreOpsSink};
use crate::lifecycle::boot::Lifecycle;
use crate::state::Services;
use crate::store::Db;

/// The store served by T09 while no World is bound: a local rev and the one
/// current subscriber.
#[derive(Default)]
pub struct LocalStore {
    inner: Mutex<LocalInner>,
}

#[derive(Default)]
struct LocalInner {
    rev: u64,
    sink: Option<Arc<ChannelPatchSink>>,
}

impl LocalStore {
    pub fn new() -> Arc<LocalStore> {
        Arc::new(LocalStore::default())
    }

    /// Replace the subscriber; returns the rev its snapshot is taken at.
    pub fn subscribe(&self, sink: Arc<ChannelPatchSink>) -> u64 {
        let mut inner = self.inner.lock();
        inner.sink = Some(sink);
        inner.rev
    }
}

impl StoreOpsSink for LocalStore {
    fn ops(&self, ops: Vec<StoreOp>) {
        let mut inner = self.inner.lock();
        let from = inner.rev;
        inner.rev += 1;
        let patch = StorePatch {
            from_rev: from,
            to_rev: inner.rev,
            ops,
        };
        if let Some(sink) = &inner.sink {
            crate::contract::services::StorePatchSink::send(sink.as_ref(), patch);
        }
    }
}

/// Managed handle of the local store.
pub type LocalStoreState = Arc<LocalStore>;

/// A snapshot of what T09 holds (the local store, used only without World).
pub fn overlay(mut snap: StoreSnapshot, feed: &LifecycleFeed) -> StoreSnapshot {
    snap.boot = feed.boot();
    let mut notices = snap.notices;
    for n in feed.notices() {
        if !notices.iter().any(|m| m.key == n.key) {
            notices.push(n);
        }
    }
    snap.notices = notices;
    if snap.leftovers.is_empty() {
        snap.leftovers = feed.leftovers();
    }
    snap
}

/// The local snapshot at `rev`.
pub fn local_snapshot(rev: u64, lifecycle: &Lifecycle, services: &Services) -> StoreSnapshot {
    overlay(
        StoreSnapshot {
            rev,
            projects: Vec::new(),
            locations: Vec::new(),
            sessions: Vec::new(),
            teams: Vec::new(),
            members: Vec::new(),
            tasks: Vec::new(),
            messages: Vec::new(),
            queue: AttentionQueue {
                rev: 0,
                items: Vec::new(),
                need_you: 0,
                also: 0,
                paused_until: None,
            },
            accounts: Vec::new(),
            leftovers: Vec::new(),
            compat: services.compat().map(|c| c.list()).unwrap_or_default(),
            notices: Vec::new(),
            boot: lifecycle.feed.boot(),
            sources: Vec::new(),
            integrations: IntegrationsState {
                orchestrator: OrchestratorPresence::Absent,
            },
        },
        &lifecycle.feed,
    )
}

/// Subscribe `out` to the store; the shared core of `store_subscribe`.
pub fn subscribe(
    out: Arc<dyn PatchOut>,
    lifecycle: &Lifecycle,
    services: &Services,
    local: &LocalStore,
) -> StoreSnapshot {
    let sink = ChannelPatchSink::new(out);
    sink.spawn_flusher();
    match services.world() {
        Some(world) => world.subscribe(Box::new(SinkHandle(sink))),
        None => {
            let rev = local.subscribe(sink);
            local_snapshot(rev, lifecycle, services)
        }
    }
}

/// `Box<dyn StorePatchSink>` over the shared edge sink.
struct SinkHandle(Arc<ChannelPatchSink>);

impl crate::contract::services::StorePatchSink for SinkHandle {
    fn send(&self, patch: StorePatch) -> bool {
        crate::contract::services::StorePatchSink::send(self.0.as_ref(), patch)
    }
}

/// `store_subscribe` (contract §4.2.1) (+ `Channel<StorePatch>`).
#[tauri::command]
pub async fn store_subscribe<R: tauri::Runtime>(
    req: Req<StoreSubscribeReq>,
    channel: tauri::ipc::Channel<StorePatch>,
    app: tauri::AppHandle<R>,
) -> Result<StoreSnapshot, IpcError> {
    let _ = req;
    let lifecycle: LifecycleState = state_of(&app)?;
    let services: ServicesState = state_of(&app)?;
    let local: LocalStoreState = state_of(&app)?;
    Ok(subscribe(Arc::new(channel), &lifecycle, &services, &local))
}

/// `ui_state_set` (contract §4.2.1, A7.7): `settings_kv` under `ui.*`;
/// the memory cache answers the next `app_bootstrap`.
#[tauri::command]
pub async fn ui_state_set<R: tauri::Runtime>(
    req: Req<UiStateSetReq>,
    app: tauri::AppHandle<R>,
) -> Result<UiStateSetRes, IpcError> {
    let req = req.into_inner();
    let lifecycle: LifecycleState = state_of(&app)?;
    let db: Db = state_of(&app)?;
    set_ui_state(&lifecycle, &db, req.key.as_str(), req.value).await?;
    Ok(UiStateSetRes {})
}

/// The shared core of `ui_state_set`. A failed write switches to memory-only
/// mode (§3.9); the value is still kept in memory.
///
/// A45.2: one transaction writes the key and deletes the rows the caps
/// evicted. A row is always written as DELETE + INSERT: the rowid order of
/// `settings_kv`'s `ui.*` rows is the "least recently written" clock that
/// boot reads back (`lifecycle::boot`), so the order survives a restart.
/// Inside the transaction every row follows what memory holds at that moment
/// (a key written or evicted again meanwhile is not resurrected or lost),
/// whatever order concurrent writes reach the store in.
pub async fn set_ui_state(lifecycle: &Lifecycle, db: &Db, key: &str, value: serde_json::Value) -> Result<(), IpcError> {
    let evicted = lifecycle.set_ui_state(key, value);
    if lifecycle.memory_only.is_active() || db.is_memory_only() {
        return Ok(());
    }
    let k = key.to_string();
    let ui = lifecycle.ui_reader();
    if let Err(err) = db
        .write(move |conn| {
            use crate::store::dao::{settings_kv, ui_state};
            let tx = conn.unchecked_transaction()?;
            let row = |key: &str| format!("{}{key}", ui_state::PREFIX);
            settings_kv::delete(&tx, &row(&k))?;
            if let Some(now) = ui.get(&k) {
                ui_state::set(&tx, &k, &now)?;
            }
            for gone in evicted.iter().filter(|g| ui.get(g).is_none()) {
                settings_kv::delete(&tx, &row(gone))?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
    {
        if let crate::error::GildeError::Db(msg) = &err {
            if let Some(reason) = crate::lifecycle::memory_only::MemoryOnlyReason::of_db_message(msg) {
                lifecycle.enter_memory_only(reason);
                return Ok(());
            }
        }
        return Err(IpcError::from(&err));
    }
    Ok(())
}
