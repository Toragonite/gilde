//! The store publisher and the four global events (contract §4.3).
//!
//! Owner: T09 (REWORK of the graph-centric event surface, §2.2.1).
//!
//! Gilde 2 has exactly **four** Tauri events — [`FOCUS_REQUEST`],
//! [`QUIT_REQUESTED`], [`TOAST`], [`VISIBILITY`] — and everything else travels
//! on Channels: the store mirror (`store_subscribe`, World's rev'd patches),
//! conversations and PTYs. The legacy event names below stay **declared** only
//! because kept code still names them (supervision's scan, budget and
//! registry notes, the worktree manager); every named event goes through
//! [`Events`], whose app edge ([`AppOut`]) drops every name that is not one
//! of the four, so none of them reaches the WebView. The legacy graph bus
//! and its graph projection are gone (A22.3).
//!
//! What T09 itself publishes into the store (boot progress, the memory-only
//! notice, leftovers, quit progress) goes through the [`LifecycleFeed`]: it
//! keeps the latest of each so `app_bootstrap`, the store snapshot and
//! diagnostics can answer from memory, and sends each change to **one**
//! store — World's `EvidenceSink::lifecycle` once World is bound, the local
//! store of `commands::store` only until then (A12.1).
//!
//! **There are no JS→Rust events** (`core:event:allow-emit` is not granted).

pub mod coalesce;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tauri::{AppHandle, Emitter, Runtime};

use crate::contract::common::{EpochMs, Notice};
use crate::contract::ids::UndoToken;
use crate::contract::ipc::VendorOption;
use crate::contract::process::ProcessRecord;
use crate::contract::services::{EvidenceSink, LifecycleOp, StorePatchSink};
use crate::contract::store::{
    event_names, BootStatus, EntityKind, Entity, QuitPhase, RecoveryWindow, StoreOp, StorePatch,
};

use crate::supervise::PressureLevel;
use crate::model::status::{SessionStatus, StatusEvidence};

// ---------------------------------------------------------------------------
// The four global events (contract §4.3)
// ---------------------------------------------------------------------------

/// `gilde://focus-request`: notification click, single-instance relaunch, dock menu.
pub const FOCUS_REQUEST: &str = event_names::FOCUS_REQUEST;
/// `gilde://quit-requested`: ⌘Q while sessions run in Gilde.
pub const QUIT_REQUESTED: &str = event_names::QUIT_REQUESTED;
/// `gilde://visibility`: occlusion / activation / sleep-wake.
pub const VISIBILITY: &str = event_names::VISIBILITY;

/// The only names that are ever emitted as Tauri events.
pub const GLOBAL_EVENTS: [&str; 4] = [FOCUS_REQUEST, QUIT_REQUESTED, TOAST, VISIBILITY];

/// Whether `name` is one of the four global events.
pub fn is_global_event(name: &str) -> bool {
    GLOBAL_EVENTS.contains(&name)
}

// ---------------------------------------------------------------------------
// Legacy event names (declared for kept code; never emitted, see the module docs)
// ---------------------------------------------------------------------------

/// [`SessionStatusEvent`] — on transition only.
pub const SESSION_STATUS: &str = "gilde://session/status";
/// [`SessionExitEvent`] — exactly once per session.
pub const SESSION_EXIT: &str = "gilde://session/exit";
/// [`PtyResyncEvent`] — emitted when a chunk was dropped under backpressure.
pub const PTY_RESYNC: &str = "gilde://pty/resync";
/// [`DispatchEvent`] — streaming; `text` is coalesced at 100 ms.
pub const DISPATCH_EVENT: &str = "gilde://dispatch/event";
/// [`WorktreeChangedEvent`] — 2 s debounce after an fs event.
pub const WORKTREE_CHANGED: &str = "gilde://worktree/changed";
/// [`ProcScanEvent`] — scanner cadence (3 s active, paused on blur).
pub const PROC_SCAN: &str = "gilde://proc/scan";
/// [`ProcRegistryEvent`] — registry invalidation.
pub const PROC_REGISTRY: &str = "gilde://proc/registry";
/// [`BudgetAlertEvent`] — on threshold crossing.
pub const BUDGET_ALERT: &str = "gilde://budget/alert";
/// [`AgentsDetectedEvent`] — boot and manual rescan.
pub const AGENTS_DETECTED: &str = "gilde://agents/detected";
/// [`RecoveryPendingEvent`] — after the boot sweep.
pub const RECOVERY_PENDING: &str = "gilde://recovery/pending";
/// [`ToastEvent`] — `{key, params}`, never prose.
pub const TOAST: &str = "gilde://toast";
/// [`LogEvent`] — development drawer.
pub const LOG: &str = "gilde://log";
/// [`TeamSnapshotEvent`] — one Claude Code Orchestrator team's whole view (the
/// read-only preview), latest wins; coalesced at 250 ms per team by the sidecar.
pub const TEAM_SNAPSHOT: &str = "gilde://team/snapshot";
/// `HealthSnapshot` (`supervise::health`) — "is everything running well",
/// every watchdog tick (5 s) and after a dismiss.
pub const HEALTH: &str = "gilde://health";

/// Every event name, for tests and for the developer event drawer.
pub const ALL_EVENTS: &[&str] = &[
    SESSION_STATUS,
    SESSION_EXIT,
    PTY_RESYNC,
    DISPATCH_EVENT,
    WORKTREE_CHANGED,
    PROC_SCAN,
    PROC_REGISTRY,
    BUDGET_ALERT,
    AGENTS_DETECTED,
    RECOVERY_PENDING,
    TOAST,
    LOG,
    TEAM_SNAPSHOT,
    HEALTH,
];

// ---------------------------------------------------------------------------
// Payloads
// ---------------------------------------------------------------------------

/// `gilde://session/status`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStatusEvent {
    pub session_id: String,
    pub status: SessionStatus,
    pub confidence: f32,
    pub evidence: Vec<StatusEvidence>,
    /// Unix millis at which the current status was entered.
    pub since: i64,
    pub at: i64,
}

/// `gilde://session/exit`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionExitEvent {
    pub session_id: String,
    pub pid: i32,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub signal: Option<i32>,
    pub at: i64,
}

/// `gilde://pty/resync` — the ring kept reading, the channel dropped frames.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PtyResyncEvent {
    pub session_id: String,
    pub next_seq: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DispatchEventKind {
    Routed,
    Spawned,
    Text,
    Tool,
    Usage,
    Done,
    Error,
    Interrupted,
    Cancelled,
}

/// `gilde://dispatch/event` — the headless lane's streaming surface.
///
/// `payload` is the adapter's own `RunEvent` shape passed through verbatim; the
/// host does not reinterpret vendor data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DispatchEvent {
    pub task_id: String,
    pub run_id: String,
    pub vendor: String,
    pub kind: DispatchEventKind,
    #[serde(default)]
    pub payload: Value,
}

/// `gilde://worktree/changed`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeChangedEvent {
    pub worktree_id: String,
    pub dirty: u32,
    pub added: u64,
    pub removed: u64,
    pub ahead: u32,
    pub behind: u32,
    pub at: i64,
}

/// `gilde://proc/scan`
///
/// Generic over the row and totals types so that this file — the single owner
/// of the event catalog — does not depend on `supervise::procscan` (T07).
/// Producers instantiate it as `ProcScanEvent<ProcRow, ProcTotals>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcScanEvent<R, T> {
    pub at: i64,
    pub rows: Vec<R>,
    pub totals: T,
}

/// `gilde://proc/registry`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcRegistryEvent {
    pub changed_spawn_ids: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BudgetLevel {
    Soft,
    Hard,
    System,
}

/// `gilde://budget/alert` — warnings only. Pressure never triggers an auto-kill.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetAlertEvent {
    pub level: BudgetLevel,
    #[serde(default)]
    pub session_id: Option<String>,
    pub rss_bytes: u64,
    pub budget_percent: f32,
    pub pressure_level: PressureLevel,
    /// Present when the alert opened a 10 s undo window.
    #[serde(default)]
    pub undo_token: Option<String>,
}

/// `gilde://agents/detected`
///
/// Generic over the binary description for the same reason as
/// [`ProcScanEvent`]; T09 instantiates it as `AgentsDetectedEvent<AgentBinary>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentsDetectedEvent<A> {
    pub agents: Vec<A>,
}

/// `gilde://recovery/pending`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryPendingEvent {
    pub count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ToastLevel {
    Info,
    Success,
    Warn,
    Error,
}

/// `gilde://toast`
///
/// Rust never returns prose. `key` indexes `src/i18n/ko.ts`, `params` fills the
/// `{placeholders}`. Changing a message does not require a Rust rebuild.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToastEvent {
    pub level: ToastLevel,
    pub key: String,
    #[serde(default)]
    pub params: Map<String, Value>,
    #[serde(default)]
    pub action_id: Option<String>,
    #[serde(default)]
    pub undo_token: Option<String>,
}

impl ToastEvent {
    pub fn new(level: ToastLevel, key: impl Into<String>) -> Self {
        Self {
            level,
            key: key.into(),
            params: Map::new(),
            action_id: None,
            undo_token: None,
        }
    }

    pub fn param(mut self, name: impl Into<String>, value: impl Into<Value>) -> Self {
        self.params.insert(name.into(), value.into());
        self
    }
}

/// `gilde://log`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogEvent {
    pub level: String,
    pub target: String,
    pub msg: String,
    pub at: i64,
}

/// `gilde://team/snapshot` — a team view composed by the sidecar's read-only
/// observer (Claude Code Orchestrator team contract v2 §8.2). Idempotent: a
/// receiver keeps the view with the greatest `(generation, rev)` and drops the
/// rest; `view: None` means the team left sight.
///
/// Generic over the view for the same reason as [`ProcScanEvent`]: this file
/// does not depend on the sidecar's types. The producer instantiates it as
/// `TeamSnapshotEvent<CcoTeamView>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamSnapshotEvent<V> {
    pub generation: i64,
    pub rev: u64,
    pub team_id: String,
    pub view: Option<V>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Unix milliseconds. One definition, so every `at` field means the same thing.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Emit an app-global event, logging (never propagating) a transport failure.
///
/// A failed emit is a lost UI update, not a lost fact: the authoritative state
/// lives in World and in SQLite, and the client can always resync.
pub fn emit<R: Runtime, P>(app: &AppHandle<R>, name: &str, payload: P)
where
    P: Serialize + Clone,
{
    if !is_global_event(name) {
        tracing::trace!(target: "events", event = name, "legacy event not emitted (contract §4.3)");
        return;
    }
    if name == TOAST {
        if let Ok(value) = serde_json::to_value(&payload) {
            emit_contract_toast(app, toast_of_value(&value));
        }
        return;
    }
    if let Err(err) = app.emit(name, payload) {
        tracing::warn!(target: "events", event = name, error = %err, "failed to emit event");
    }
}

/// Convenience wrapper for `gilde://toast`.
pub fn emit_toast<R: Runtime>(app: &AppHandle<R>, toast: ToastEvent) {
    emit(app, TOAST, toast);
}

/// `gilde://toast` in the contract shape `{key, params, undoToken}` (§4.3).
pub fn emit_contract_toast<R: Runtime>(app: &AppHandle<R>, toast: crate::contract::store::ToastEvent) {
    if let Err(err) = app.emit(TOAST, toast) {
        tracing::warn!(target: "events", error = %err, "failed to emit toast");
    }
}

/// A legacy toast payload (`{level, key, params, actionId, undoToken}`) in
/// the contract shape. Parameters keep only strings, numbers and booleans.
pub fn toast_of_value(value: &Value) -> crate::contract::store::ToastEvent {
    let key = value
        .get("key")
        .and_then(Value::as_str)
        .unwrap_or("error.internal")
        .to_string();
    let params = value
        .get("params")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter(|(_, v)| v.is_string() || v.is_number() || v.is_boolean())
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();
    let undo_token = value
        .get("undoToken")
        .and_then(Value::as_str)
        .and_then(|t| crate::contract::ids::UndoToken::parse(t).ok());
    crate::contract::store::ToastEvent {
        key,
        params,
        undo_token,
    }
}

// ---------------------------------------------------------------------------
// The named-event emitter (replaces the legacy graph bus, A22.3)
// ---------------------------------------------------------------------------

/// Where named events go: the app edge in production ([`AppOut`]: only the
/// four global events leave, a legacy toast payload in the contract shape),
/// a recorder in tests ([`RecordingEvents`]).
pub trait EventOut: Send + Sync + 'static {
    fn emit_named(&self, name: &str, payload: &Value);
}

/// The emitter supervision and the worktree manager publish through. Cheap
/// to clone; every clone talks to the same edge. Never blocks, never fails.
#[derive(Clone)]
pub struct Events {
    out: Arc<dyn EventOut>,
}

impl std::fmt::Debug for Events {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Events").finish_non_exhaustive()
    }
}

impl Events {
    pub fn new(out: Arc<dyn EventOut>) -> Events {
        Events { out }
    }

    /// The app edge (§4.3): only [`GLOBAL_EVENTS`] leave.
    pub fn app<R: Runtime>(app: AppHandle<R>) -> Events {
        Events::new(Arc::new(AppOut { app }))
    }

    /// An emitter that drops everything (tools, tests that do not look).
    pub fn none() -> Events {
        Events::new(Arc::new(NoEvents))
    }

    /// Emit `payload` under `name`; a payload that cannot be serialized is
    /// logged by name only (never its content).
    pub fn emit<P: Serialize>(&self, name: &str, payload: P) {
        match serde_json::to_value(&payload) {
            Ok(value) => self.out.emit_named(name, &value),
            Err(err) => tracing::warn!(target: "events", event = name, error = %err, "event payload not serializable"),
        }
    }

    /// `gilde://toast`.
    pub fn toast(&self, toast: ToastEvent) {
        self.emit(TOAST, toast);
    }
}

/// The production edge: the Tauri app-global event channel.
pub struct AppOut<R: Runtime> {
    app: AppHandle<R>,
}

impl<R: Runtime> EventOut for AppOut<R> {
    fn emit_named(&self, name: &str, payload: &Value) {
        if !is_global_event(name) {
            tracing::trace!(target: "events", event = name, "legacy event not emitted (contract §4.3)");
            return;
        }
        let result = if name == TOAST {
            self.app.emit(name, toast_of_value(payload))
        } else {
            self.app.emit(name, payload)
        };
        if let Err(err) = result {
            tracing::warn!(target: "events", event = name, error = %err, "failed to emit event");
        }
    }
}

/// Drops everything.
#[derive(Debug, Default)]
pub struct NoEvents;

impl EventOut for NoEvents {
    fn emit_named(&self, _name: &str, _payload: &Value) {}
}

/// Keeps every event (tests, instrumentation).
#[derive(Debug, Default)]
pub struct RecordingEvents {
    events: Mutex<Vec<(String, Value)>>,
}

impl RecordingEvents {
    pub fn new() -> Arc<RecordingEvents> {
        Arc::new(RecordingEvents::default())
    }

    /// Every event so far, in order.
    pub fn events(&self) -> Vec<(String, Value)> {
        self.events.lock().clone()
    }

    /// The payloads of every event named `name`, in order.
    pub fn named(&self, name: &str) -> Vec<Value> {
        self.events
            .lock()
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
            .collect()
    }
}

impl EventOut for RecordingEvents {
    fn emit_named(&self, name: &str, payload: &Value) {
        self.events.lock().push((name.to_owned(), payload.clone()));
    }
}

/// The worktree manager's notes (T11): `gilde://worktree/changed` (not a
/// global event, so it stays inside) and its toasts.
impl crate::worktree::WorktreeEvents for Events {
    fn changed(&self, event: WorktreeChangedEvent) {
        self.emit(WORKTREE_CHANGED, event);
    }

    fn toast(&self, toast: ToastEvent) {
        Events::toast(self, toast);
    }
}

// ---------------------------------------------------------------------------
// T09's store ops (contract §4.3)
// ---------------------------------------------------------------------------

/// Where T09 publishes the store ops it owns: `Boot{status}`, `Notices`,
/// `Upsert/Remove{Leftover}`, `Quit{…}`.
pub trait StoreOpsSink: Send + Sync {
    fn ops(&self, ops: Vec<StoreOp>);
}

/// A sink that drops everything (tests, tools).
#[derive(Debug, Default)]
pub struct NoStoreOps;

impl StoreOpsSink for NoStoreOps {
    fn ops(&self, _ops: Vec<StoreOp>) {}
}

/// The latest T09-owned store state, kept in memory, and the one place it
/// leaves for the store (contract A12.1).
///
/// Routing: while no World is bound, every change goes to the local store
/// ([`LifecycleFeed::set_local_store`]) as store ops; once
/// [`LifecycleFeed::bind_world`] has run, every change goes to World's
/// `EvidenceSink::lifecycle` as a [`LifecycleOp`] and the local store gets
/// nothing more. Never both.
pub struct LifecycleFeed {
    boot: RwLock<BootStatus>,
    notices: RwLock<BTreeMap<String, Notice>>,
    leftovers: RwLock<BTreeMap<String, ProcessRecord>>,
    quit: RwLock<Option<(u32, u32, QuitPhase)>>,
    route: RwLock<Route>,
    /// A29.1: `(last ticket handed out, last ticket published)` of the agent
    /// detections, so a detection that finishes after a newer one never
    /// overwrites the newer rows.
    agent_scans: Mutex<(u64, u64)>,
}

/// Where the feed's changes go.
#[derive(Default)]
struct Route {
    local: Option<Arc<dyn StoreOpsSink>>,
    world: Option<Arc<dyn EvidenceSink>>,
}

impl std::fmt::Debug for LifecycleFeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LifecycleFeed")
            .field("boot", &*self.boot.read())
            .field("notices", &self.notices.read().len())
            .field("leftovers", &self.leftovers.read().len())
            .field("worldBound", &self.route.read().world.is_some())
            .finish()
    }
}

impl LifecycleFeed {
    pub fn new() -> Arc<LifecycleFeed> {
        Arc::new(LifecycleFeed {
            boot: RwLock::new(BootStatus {
                spawn_gate_open: false,
                first_discovery_done: false,
                agents_detected: false,
                memory_only: None,
                agents: Vec::new(),
                agents_scanned_at: None,
                recovery: None,
            }),
            notices: RwLock::new(BTreeMap::new()),
            leftovers: RwLock::new(BTreeMap::new()),
            quit: RwLock::new(None),
            route: RwLock::new(Route::default()),
            agent_scans: Mutex::new((0, 0)),
        })
    }

    /// A29.1: a detection is starting; its ticket for [`Self::agents_scanned`].
    /// Nothing is published: the previous rows and `agents_detected` stay as
    /// they are while it runs (no flash of "Finding installed agents…").
    pub fn begin_agents_scan(&self) -> u64 {
        let mut scans = self.agent_scans.lock();
        scans.0 += 1;
        scans.0
    }

    /// A29.1: the detection with `ticket` completed at `at`; `rows` builds
    /// the new rows from the previous ones (a failed detection keeps them).
    /// They replace the previous rows, `agents_detected` turns true and
    /// `agents_scanned_at` moves forward — strictly, even when the rows are
    /// identical or the clock did not advance. `false` (nothing published)
    /// when a detection that started later has published already.
    pub fn agents_scanned(
        &self,
        ticket: u64,
        at: EpochMs,
        rows: impl FnOnce(&[VendorOption]) -> Vec<VendorOption>,
    ) -> bool {
        let mut scans = self.agent_scans.lock();
        if ticket <= scans.1 {
            return false;
        }
        scans.1 = ticket;
        // Published under the ticket lock, so two completions cannot reorder.
        self.update_boot(|b| {
            let at = match b.agents_scanned_at {
                Some(prev) if at <= prev => prev + 1,
                _ => at,
            };
            b.agents = rows(&b.agents);
            b.agents_scanned_at = Some(at);
            b.agents_detected = true;
        });
        true
    }

    /// A29.3: the Recovered dialog's window opened.
    pub fn set_recovery(&self, window: RecoveryWindow) {
        self.update_boot(|b| b.recovery = Some(window));
    }

    /// A29.3: the window of `token` ended (its stops fired, or the undo
    /// cancelled them). A window of another token is left alone.
    pub fn clear_recovery(&self, token: &UndoToken) {
        self.update_boot(|b| {
            if b.recovery.as_ref().is_some_and(|r| &r.undo_token == token) {
                b.recovery = None;
            }
        });
    }

    /// The fallback store used while no World is bound.
    pub fn set_local_store(&self, sink: Arc<dyn StoreOpsSink>) {
        self.route.write().local = Some(sink);
    }

    /// Route every later change to World (`EvidenceSink::lifecycle`), after
    /// handing it the current state once: `Boot`, the full `Notices` list,
    /// one `LeftoverUpsert` per leftover and the quit progress if a quit is
    /// running. From here on the local store receives nothing. `false` (and
    /// nothing sent) if a World was bound already.
    pub fn bind_world(&self, sink: Arc<dyn EvidenceSink>) -> bool {
        // The route lock is held while the current state is read and sent,
        // so no change can slip between the initial state and the switch.
        let mut route = self.route.write();
        if route.world.is_some() {
            return false;
        }
        sink.lifecycle(LifecycleOp::Boot(self.boot()));
        sink.lifecycle(LifecycleOp::Notices(self.notices()));
        for record in self.leftovers() {
            sink.lifecycle(LifecycleOp::LeftoverUpsert(Box::new(record)));
        }
        if let Some((stopped, total, phase)) = self.quit_progress() {
            sink.lifecycle(LifecycleOp::Quit { stopped, total, phase });
        }
        route.world = Some(sink);
        true
    }

    /// Whether World receives the feed's changes.
    pub fn world_bound(&self) -> bool {
        self.route.read().world.is_some()
    }

    pub fn boot(&self) -> BootStatus {
        self.boot.read().clone()
    }

    /// Change the boot status and publish `Boot` if it changed.
    pub fn update_boot(&self, f: impl FnOnce(&mut BootStatus)) {
        let status = {
            let mut b = self.boot.write();
            let before = b.clone();
            f(&mut b);
            if *b == before {
                return;
            }
            b.clone()
        };
        self.send(vec![LifecycleOp::Boot(status)]);
    }

    /// Show (`Some`) or clear (`None`) the notice with `key`; publishes the
    /// full list when it changed.
    pub fn set_notice(&self, key: &str, notice: Option<Notice>) {
        let changed = {
            let mut n = self.notices.write();
            match notice {
                Some(v) => n.insert(key.to_string(), v.clone()).as_ref() != Some(&v),
                None => n.remove(key).is_some(),
            }
        };
        if changed {
            self.send(vec![LifecycleOp::Notices(self.notices())]);
        }
    }

    pub fn notices(&self) -> Vec<Notice> {
        self.notices.read().values().cloned().collect()
    }

    /// Replace the leftover set; publishes upserts and removals for the diff.
    pub fn set_leftovers(&self, records: Vec<ProcessRecord>) {
        let mut ops = Vec::new();
        {
            let mut cur = self.leftovers.write();
            let next: BTreeMap<String, ProcessRecord> = records
                .into_iter()
                .map(|r| (r.key.as_str().to_string(), r))
                .collect();
            for (k, v) in &next {
                if cur.get(k) != Some(v) {
                    ops.push(LifecycleOp::LeftoverUpsert(Box::new(v.clone())));
                }
            }
            for (k, old) in cur.iter() {
                if !next.contains_key(k) {
                    ops.push(LifecycleOp::LeftoverRemove(old.key.clone()));
                }
            }
            *cur = next;
        }
        if !ops.is_empty() {
            self.send(ops);
        }
    }

    pub fn leftovers(&self) -> Vec<ProcessRecord> {
        self.leftovers.read().values().cloned().collect()
    }

    pub fn quit_progress(&self) -> Option<(u32, u32, QuitPhase)> {
        *self.quit.read()
    }

    /// Record quit progress and publish it.
    pub fn set_quit(&self, stopped: u32, total: u32, phase: QuitPhase) {
        *self.quit.write() = Some((stopped, total, phase));
        self.send(vec![LifecycleOp::Quit { stopped, total, phase }]);
    }

    /// Send to World when bound, else to the local store.
    fn send(&self, ops: Vec<LifecycleOp>) {
        let route = self.route.read();
        if let Some(world) = &route.world {
            for op in ops {
                world.lifecycle(op);
            }
        } else if let Some(local) = &route.local {
            local.ops(ops.into_iter().map(store_op_of).collect());
        }
    }
}

/// The store op a [`LifecycleOp`] becomes in the local store.
pub fn store_op_of(op: LifecycleOp) -> StoreOp {
    match op {
        LifecycleOp::Boot(status) => StoreOp::Boot { status },
        LifecycleOp::Notices(notices) => StoreOp::Notices { notices },
        LifecycleOp::Quit { stopped, total, phase } => StoreOp::Quit { stopped, total, phase },
        LifecycleOp::LeftoverUpsert(record) => StoreOp::Upsert {
            entity: Box::new(Entity::Leftover { value: *record }),
        },
        LifecycleOp::LeftoverRemove(key) => StoreOp::Remove {
            entity_kind: EntityKind::Leftover,
            id: key.as_str().to_string(),
        },
    }
}

/// The quit coordinator reports through [`StoreOpsSink`]; the feed keeps the
/// T09-owned ops it understands and routes them like its own changes.
impl StoreOpsSink for LifecycleFeed {
    fn ops(&self, ops: Vec<StoreOp>) {
        for op in ops {
            match op {
                StoreOp::Quit { stopped, total, phase } => self.set_quit(stopped, total, phase),
                StoreOp::Boot { status } => self.update_boot(|b| *b = status),
                StoreOp::Notices { notices } => {
                    let keys: Vec<String> = self.notices.read().keys().cloned().collect();
                    for k in keys {
                        if !notices.iter().any(|n| n.key == k) {
                            self.set_notice(&k, None);
                        }
                    }
                    for n in notices {
                        let key = n.key.clone();
                        self.set_notice(&key, Some(n));
                    }
                }
                other => {
                    tracing::debug!(target: "events", op = ?std::mem::discriminant(&other), "not a lifecycle op; dropped");
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// the channel edge of `store_subscribe` (contract §4.3)
// ---------------------------------------------------------------------------

/// Where a [`ChannelPatchSink`] delivers (a Tauri `Channel<StorePatch>` in
/// production, a vector in tests). `false` = the receiver is gone.
pub trait PatchOut: Send + Sync {
    fn deliver(&self, patch: StorePatch) -> bool;
}

impl PatchOut for tauri::ipc::Channel<StorePatch> {
    fn deliver(&self, patch: StorePatch) -> bool {
        self.send(patch).is_ok()
    }
}

/// The `StorePatchSink` World pushes into for one subscriber.
///
/// It enforces the edge rules of §4.3 whatever the producer does: patches are
/// strictly rev-ordered (a gap or a reordering becomes `Resync`), at most
/// [`coalesce::STORE_MAX_PER_SEC`] patches a second leave (consecutive patches
/// inside one 100 ms window are merged, ops for the same entity collapsing to
/// the last), and a merged patch above `MAX_OPS_PER_PATCH` ops is replaced by
/// `Resync`. Ops are never dropped otherwise.
pub struct ChannelPatchSink {
    out: Arc<dyn PatchOut>,
    state: Mutex<coalesce::StoreCoalescer>,
}

impl ChannelPatchSink {
    pub fn new(out: Arc<dyn PatchOut>) -> Arc<ChannelPatchSink> {
        Arc::new(ChannelPatchSink {
            out,
            state: Mutex::new(coalesce::StoreCoalescer::default()),
        })
    }

    /// Deliver whatever is pending now (the 100 ms timer calls this).
    pub fn flush(&self) -> bool {
        let pending = self.state.lock().take();
        match pending {
            Some(p) => self.out.deliver(p),
            None => true,
        }
    }

    /// Start the 100 ms flush timer on the current runtime. Ends when the
    /// receiver is gone.
    pub fn spawn_flusher(self: &Arc<Self>) {
        let me = Arc::downgrade(self);
        tauri::async_runtime::spawn(async move {
            let mut tick = tokio::time::interval(coalesce::TICK);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let Some(sink) = me.upgrade() else { break };
                if !sink.flush() {
                    break;
                }
            }
        });
    }
}

impl StorePatchSink for ChannelPatchSink {
    fn send(&self, patch: StorePatch) -> bool {
        self.state.lock().push(patch);
        true
    }
}

// ---------------------------------------------------------------------------
// Event names
// ---------------------------------------------------------------------------


#[cfg(test)]
mod tests {
    use super::*;

    /// A12.1: before World is bound the feed's changes reach only the local
    /// store; `bind_world` hands World the current state once, and every
    /// later change reaches only World, as `LifecycleOp`s.
    #[test]
    fn t_lifecycle_ops_reach_world_once_bound_and_never_both() {
        use crate::contract::account::Account;
        use crate::contract::compat::ClaudeCompat;
        use crate::contract::ids::ProcessKey;
        use crate::contract::process::ProcRole;
        use crate::contract::services::{EvidenceUpdate, LocationFacts};
        use crate::contract::store::SourceStatus;

        #[derive(Default)]
        struct Local(Mutex<Vec<StoreOp>>);
        impl StoreOpsSink for Local {
            fn ops(&self, ops: Vec<StoreOp>) {
                self.0.lock().extend(ops);
            }
        }
        #[derive(Default)]
        struct World(Mutex<Vec<LifecycleOp>>);
        impl EvidenceSink for World {
            fn evidence(&self, _update: EvidenceUpdate) {}
            fn location(&self, _facts: LocationFacts) {}
            fn source_status(&self, _status: SourceStatus) {}
            fn account(&self, _account: Account) {}
            fn compat(&self, _list: Vec<ClaudeCompat>) {}
            fn lifecycle(&self, op: LifecycleOp) {
                self.0.lock().push(op);
            }
        }
        fn record(pid: i32) -> ProcessRecord {
            ProcessRecord {
                key: ProcessKey::new(pid as u32, 1_761_000_000),
                pid,
                start_sec: 1_761_000_000,
                vendor: None,
                role: ProcRole::Other,
                session_uid: None,
                gilde_owned: false,
                rss_bytes: 0,
                leftover: None,
            }
        }

        let feed = LifecycleFeed::new();
        let local = Arc::new(Local::default());
        let world = Arc::new(World::default());
        feed.set_local_store(local.clone());

        // Before World: the local store only.
        feed.update_boot(|b| b.spawn_gate_open = true);
        feed.set_leftovers(vec![record(4242)]);
        let local_before = local.0.lock().len();
        assert_eq!(local_before, 2, "{:?}", local.0.lock());
        assert!(matches!(local.0.lock()[0], StoreOp::Boot { .. }));
        assert!(matches!(local.0.lock()[1], StoreOp::Upsert { .. }));
        assert!(!feed.world_bound());

        // Binding hands World the current state once.
        assert!(feed.bind_world(world.clone()));
        let mut booted = feed.boot();
        assert!(booted.spawn_gate_open);
        assert_eq!(
            *world.0.lock(),
            vec![
                LifecycleOp::Boot(booted.clone()),
                LifecycleOp::Notices(Vec::new()),
                LifecycleOp::LeftoverUpsert(Box::new(record(4242))),
            ]
        );
        world.0.lock().clear();

        // After: World only, in the A12.1 shapes.
        let notice = Notice::new("notice.memoryOnly");
        feed.set_notice("notice.memoryOnly", Some(notice.clone()));
        feed.set_leftovers(Vec::new());
        StoreOpsSink::ops(
            &*feed,
            vec![StoreOp::Quit { stopped: 1, total: 2, phase: QuitPhase::Stopping }],
        );
        feed.update_boot(|b| b.agents_detected = true);
        feed.update_boot(|b| b.agents_detected = true); // unchanged: nothing sent
        booted.agents_detected = true;
        assert_eq!(
            *world.0.lock(),
            vec![
                LifecycleOp::Notices(vec![notice]),
                LifecycleOp::LeftoverRemove(ProcessKey::new(4242, 1_761_000_000)),
                LifecycleOp::Quit { stopped: 1, total: 2, phase: QuitPhase::Stopping },
                LifecycleOp::Boot(booted),
            ]
        );
        assert_eq!(local.0.lock().len(), local_before, "never both");
        assert_eq!(feed.quit_progress(), Some((1, 2, QuitPhase::Stopping)));

        // A second World is refused and nothing is re-sent.
        world.0.lock().clear();
        assert!(!feed.bind_world(Arc::new(World::default())));
        assert!(world.0.lock().is_empty());
    }

    #[test]
    fn every_event_name_is_namespaced_and_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for name in ALL_EVENTS {
            assert!(name.starts_with("gilde://"), "{name} must be gilde:// namespaced");
            assert!(seen.insert(*name), "duplicate event name {name}");
        }
        // 17 before the graph patch/resync and sidecar-state names went with
        // the legacy graph (A22.3).
        assert_eq!(seen.len(), 14);
    }

    #[test]
    fn payloads_are_camel_case() {
        let json = serde_json::to_value(SessionExitEvent {
            session_id: "s".into(),
            pid: 42,
            exit_code: Some(0),
            signal: None,
            at: 1,
        })
        .unwrap();
        assert_eq!(json["sessionId"], "s");
        assert_eq!(json["exitCode"], 0);
        assert!(json.get("signal").is_some(), "explicit null is kept");

        let json = serde_json::to_value(PtyResyncEvent {
            session_id: "s".into(),
            next_seq: 9,
        })
        .unwrap();
        assert_eq!(json["nextSeq"], 9);

        let json = serde_json::to_value(ProcRegistryEvent {
            changed_spawn_ids: vec!["a".into()],
        })
        .unwrap();
        assert_eq!(json["changedSpawnIds"][0], "a");
    }

    #[test]
    fn dispatch_event_kinds_are_lower_camel() {
        let json = serde_json::to_value(DispatchEvent {
            task_id: "t".into(),
            run_id: "r".into(),
            vendor: "claude".into(),
            kind: DispatchEventKind::Interrupted,
            payload: Value::Null,
        })
        .unwrap();
        assert_eq!(json["kind"], "interrupted");
    }

    #[test]
    fn toast_carries_keys_not_prose() {
        let toast = ToastEvent::new(ToastLevel::Warn, "worktree.remove.dirty").param("n", 3);
        let json = serde_json::to_value(&toast).unwrap();
        assert_eq!(json["key"], "worktree.remove.dirty");
        assert_eq!(json["params"]["n"], 3);
        assert_eq!(json["level"], "warn");
    }

    #[test]
    fn generic_events_instantiate() {
        #[derive(Clone, Serialize)]
        struct Row {
            pid: i32,
        }
        let json = serde_json::to_value(ProcScanEvent {
            at: 1,
            rows: vec![Row { pid: 7 }],
            totals: 1u32,
        })
        .unwrap();
        assert_eq!(json["rows"][0]["pid"], 7);
    }

    #[test]
    fn now_ms_is_plausible() {
        // 2020-01-01T00:00:00Z
        assert!(now_ms() > 1_577_836_800_000);
    }
}
