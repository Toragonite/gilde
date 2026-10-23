//! Boot phases B1–B13 and the boot timer (§2.4).
//!
//! Owner: T09.
//!
//! `setup()` does only what must happen before the window: B1 paths (and the
//! memory-only probe), B2 `Db::open`, B3 `ConfigStore::load`, B4 the
//! single-instance lock — plus three things that are memory and SQLite only:
//! the crash records of a crashed predecessor are **planned** (so
//! `app_bootstrap.launch` can name them at B5), the `app_instance` row is
//! inserted (one durable write, so every service can be built against the
//! supervisor before the window), and the UI state is cached. It never
//! spawns, never reads a vendor directory, never touches the network
//! (T-boot-1: ≤ 150 ms with a cold `cache/path.json` and a sleeping login
//! shell — the PATH probe is not on this path at all).
//!
//! Everything else runs after the window is visible (B5), in order:
//! B6 the boot sweep, which writes the planned crash records, sweeps quit
//! survivors and **opens the spawn gate**; B7/B8 discovery (T03); B9 PATH +
//! `--version` detection at QoS utility; B10 the static canary through
//! `CompatApi` (T21); B11 the history index (T03); B12 the sidecar lazily on
//! first need (T08); B13 orchestrator detection (T03/T08).
//!
//! No `proc_registry` row can be planned before `window_visible_at`: every
//! spawn waits for the gate, and the gate opens only after the window was
//! recorded visible (T-boot-2).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use parking_lot::{Mutex, RwLock};
use serde_json::Value;
use tokio::sync::watch;

use crate::agents::{AgentBinary, AgentRegistry};
use crate::config::ConfigStore;
use crate::contract::common::Vendor;
use crate::contract::ids::UndoToken;
use crate::contract::ipc::{
    self as ipc, AppMark, LaunchInfo, Platform, UiKey, VendorOption, UI_STATE_MAX_BYTES, UI_TEAM_LAYOUT_MAX_ROWS,
};
use crate::contract::services::{CompatApi, WorldApi};
use crate::contract::store::RecoveryWindow;
use crate::error::{GildeError, Result};
use crate::events::LifecycleFeed;
use crate::lifecycle::crash::{self, CrashPlan};
use crate::lifecycle::memory_only::{probe_writable, MemoryOnly, MemoryOnlyReason, NOTICE_KEY};
use crate::lifecycle::quit::{self, QuitCoordinator, QuitPreview};
use crate::lifecycle::spawn_gate::SpawnGate;
use crate::paths::AppPaths;
use crate::state::{self, InstanceLock, LockOutcome};
use crate::store::models::CAUSE_CRASH;
use crate::store::{dao, now_ms, Db};
use crate::supervise::procscan::ScanHub;
use crate::supervise::reaper::{StopGroup, StopGroupObserver};
use crate::supervise::registry::Supervisor;

// ---------------------------------------------------------------------------
// the boot timer
// ---------------------------------------------------------------------------

/// Phase stopwatch for the boot sequence (§2.4 "The boot timer stays").
///
/// Every phase is timed, the total is logged, and anything over its budget is
/// logged at `warn` with its own name.
#[derive(Debug)]
pub struct BootTimer {
    started: Instant,
    last: Instant,
    phases: Vec<(&'static str, u128)>,
}

impl Default for BootTimer {
    fn default() -> Self {
        BootTimer::start()
    }
}

impl BootTimer {
    /// The per-phase warning threshold for the synchronous phases.
    pub const BUDGET_MS: u128 = 300;

    pub fn start() -> BootTimer {
        let now = Instant::now();
        BootTimer {
            started: now,
            last: now,
            phases: Vec::new(),
        }
    }

    pub fn mark(&mut self, phase: &'static str) {
        let elapsed = self.last.elapsed().as_millis();
        self.last = Instant::now();
        if elapsed > Self::BUDGET_MS {
            tracing::warn!(
                target: "app",
                phase,
                ms = elapsed,
                budget_ms = Self::BUDGET_MS,
                "a boot phase took longer than its budget"
            );
        }
        self.phases.push((phase, elapsed));
    }

    pub fn total_ms(&self) -> u128 {
        self.started.elapsed().as_millis()
    }

    /// `paths=3 db=11 config=1 …` — one line, greppable.
    pub fn breakdown(&self) -> String {
        self.phases
            .iter()
            .map(|(name, ms)| format!("{name}={ms}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub fn phases(&self) -> &[(&'static str, u128)] {
        &self.phases
    }
}

// ---------------------------------------------------------------------------
// Lifecycle: what the boot, the quit and the app commands share
// ---------------------------------------------------------------------------

/// [`Lifecycle::on_ui_state`]'s listener: `(key, value)`.
pub type UiStateListener = Arc<dyn Fn(&str, &Value) + Send + Sync>;

/// A45.2: the UI state in memory, with the order its keys were written in.
/// At most [`UI_TEAM_LAYOUT_MAX_ROWS`] `teamLayout:*` rows and
/// [`UI_STATE_MAX_BYTES`] in all: past either cap the least recently written
/// team layout goes (never the key just written; the other keys are bounded
/// by their schemas).
#[derive(Debug, Default)]
pub struct UiStore {
    values: BTreeMap<String, Value>,
    written: HashMap<String, u64>,
    next: u64,
    bytes: usize,
}

impl UiStore {
    /// Write `key`, then apply the caps. Answers the keys evicted.
    pub fn put(&mut self, key: &str, value: Value) -> Vec<String> {
        let added = ipc::ui_entry_bytes(key, &value);
        if let Some(old) = self.values.insert(key.to_string(), value) {
            self.bytes -= ipc::ui_entry_bytes(key, &old);
        }
        self.bytes += added;
        self.next += 1;
        self.written.insert(key.to_string(), self.next);
        let mut evicted = Vec::new();
        loop {
            let layouts = self.values.keys().filter(|k| UiKey::is_team_layout_str(k)).count();
            if layouts <= UI_TEAM_LAYOUT_MAX_ROWS && self.bytes <= UI_STATE_MAX_BYTES {
                break;
            }
            let oldest = self
                .values
                .keys()
                .filter(|k| UiKey::is_team_layout_str(k) && k.as_str() != key)
                .min_by_key(|k| self.written.get(k.as_str()).copied().unwrap_or(0))
                .cloned();
            let Some(oldest) = oldest else { break };
            self.remove(&oldest);
            evicted.push(oldest);
        }
        evicted
    }

    fn remove(&mut self, key: &str) {
        if let Some(old) = self.values.remove(key) {
            self.bytes -= ipc::ui_entry_bytes(key, &old);
        }
        self.written.remove(key);
    }

    pub fn values(&self) -> &BTreeMap<String, Value> {
        &self.values
    }

    pub fn contains(&self, key: &str) -> bool {
        self.values.contains_key(key)
    }

    /// Keys + compact JSON values, the measure of [`UI_STATE_MAX_BYTES`].
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Load persisted rows, oldest write first. A row whose key is not a
    /// `UiKey` or whose value fails its schema is dropped, never served
    /// (A45.2); the caps apply as for a write. Answers every key to delete
    /// from the store (dropped or evicted).
    pub fn load(&mut self, rows: Vec<(String, Option<Value>)>) -> Vec<String> {
        let mut gone = Vec::new();
        for (key, value) in rows {
            match value {
                Some(value) if UiKey::is_valid_str(&key) && ipc::validate_ui_value(&key, &value).is_ok() => {
                    gone.extend(self.put(&key, value));
                }
                _ => gone.push(key),
            }
        }
        gone
    }
}

/// Reads the UI state of a [`Lifecycle`] (see [`Lifecycle::ui_reader`]).
#[derive(Clone)]
pub struct UiReader(Arc<RwLock<UiStore>>);

impl UiReader {
    /// The value `key` holds now, `None` once it is evicted.
    pub fn get(&self, key: &str) -> Option<Value> {
        self.0.read().values().get(key).cloned()
    }
}

/// The detection rerun slot (A45.3): one at a time; a rescan while one runs
/// joins it, a changed setting asks for one more run after it.
#[derive(Debug, Default)]
struct RescanSlot {
    /// The run in flight: its generation and its "done" signal.
    running: Option<(u64, watch::Receiver<bool>)>,
    /// A setting changed while the run was in flight.
    again: bool,
    generation: u64,
}

/// How a rerun request meets a run already in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RescanMode {
    /// `agents_rescan`: join it (it probes the same PATH and binaries).
    Join,
    /// An applied `*Binary` / `pathsOverride` setting the running detection
    /// may have read before the change: one more run after it.
    AfterChange,
}

/// The lifecycle state of this process (managed as `Arc<Lifecycle>`).
pub struct Lifecycle {
    pub version: String,
    pub build_id: String,
    pub feed: Arc<LifecycleFeed>,
    pub gate: Arc<SpawnGate>,
    pub memory_only: Arc<MemoryOnly>,
    pub scans: Arc<ScanHub>,
    pub platform: Platform,
    launch: RwLock<LaunchInfo>,
    crash_plans: Mutex<Vec<CrashPlan>>,
    ui_state: Arc<RwLock<UiStore>>,
    ui_listeners: Mutex<Vec<UiStateListener>>,
    rescan: Mutex<RescanSlot>,
    marks: Mutex<BTreeMap<&'static str, u32>>,
    timer: Mutex<BootTimer>,
    window_visible: watch::Sender<Option<i64>>,
    supervisor: OnceLock<Arc<Supervisor>>,
    quit: OnceLock<Arc<QuitCoordinator>>,
    process_started_ms: i64,
    error_codes: Mutex<BTreeMap<String, u64>>,
}

impl std::fmt::Debug for Lifecycle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lifecycle")
            .field("version", &self.version)
            .field("build_id", &self.build_id)
            .field("gate_open", &self.gate.is_open())
            .field("memory_only", &self.memory_only.reason())
            .finish()
    }
}

impl Lifecycle {
    pub fn new(version: &str, build_id: &str) -> Arc<Lifecycle> {
        let (tx, _rx) = watch::channel(None);
        Arc::new(Lifecycle {
            version: version.to_string(),
            build_id: build_id.to_string(),
            feed: LifecycleFeed::new(),
            gate: SpawnGate::closed(),
            memory_only: MemoryOnly::new(),
            scans: ScanHub::new(),
            platform: platform(),
            launch: RwLock::new(LaunchInfo::FirstLaunch),
            crash_plans: Mutex::new(Vec::new()),
            ui_state: Arc::new(RwLock::new(UiStore::default())),
            ui_listeners: Mutex::new(Vec::new()),
            rescan: Mutex::new(RescanSlot::default()),
            marks: Mutex::new(BTreeMap::new()),
            timer: Mutex::new(BootTimer::start()),
            window_visible: tx,
            supervisor: OnceLock::new(),
            quit: OnceLock::new(),
            process_started_ms: process_started_ms(),
            error_codes: Mutex::new(BTreeMap::new()),
        })
    }

    pub fn launch(&self) -> LaunchInfo {
        self.launch.read().clone()
    }

    pub fn set_launch(&self, info: LaunchInfo) {
        *self.launch.write() = info;
    }

    pub fn ui_state(&self) -> BTreeMap<String, Value> {
        self.ui_state.read().values().clone()
    }

    /// The bytes the UI state holds (A45.2, ≤ [`UI_STATE_MAX_BYTES`]).
    pub fn ui_state_bytes(&self) -> usize {
        self.ui_state.read().bytes()
    }

    /// Whether the UI state holds `key` now.
    pub fn has_ui_key(&self, key: &str) -> bool {
        self.ui_state.read().contains(key)
    }

    /// Keep `key` (already validated against its schema by `ui_state_set`),
    /// apply the A45.2 caps and tell the listeners. Answers the keys the caps
    /// evicted: `commands::store::set_ui_state` removes their rows in the
    /// same transaction as the write.
    pub fn set_ui_state(&self, key: &str, value: Value) -> Vec<String> {
        let evicted = self.ui_state.write().put(key, value.clone());
        let listeners = self.ui_listeners.lock().clone();
        for listener in listeners {
            listener(key, &value);
        }
        evicted
    }

    /// A handle that reads the UI state at any later moment (inside a store
    /// transaction, so the rows follow memory whatever order writes land in).
    pub fn ui_reader(&self) -> UiReader {
        UiReader(self.ui_state.clone())
    }

    /// Load the persisted UI rows (oldest write first) at B4. Answers the
    /// keys to delete from `settings_kv` (A45.2: invalid rows are dropped,
    /// never served; rows over the caps are evicted).
    pub fn load_ui_state(&self, rows: Vec<(String, Option<Value>)>) -> Vec<String> {
        self.ui_state.write().load(rows)
    }

    /// Register `listener` for every later `ui_state_set` (the composition
    /// root forwards `ui.sidebarOrder`, `ui.sidebarFolds` and `ui.splitSet`
    /// to World). Called with no lifecycle lock held.
    pub fn on_ui_state(&self, listener: UiStateListener) {
        self.ui_listeners.lock().push(listener);
    }

    /// `app_mark`: the first of each mark is kept and logged; repeats are ignored.
    pub fn mark(&self, mark: AppMark, perf_ms: u32) -> bool {
        let name = match mark {
            AppMark::FirstPaint => "firstPaint",
            AppMark::Interactive => "interactive",
        };
        let mut marks = self.marks.lock();
        if marks.contains_key(name) {
            return false;
        }
        marks.insert(name, perf_ms);
        tracing::info!(target: "app", mark = name, perf_ms, "webview mark");
        true
    }

    pub fn marks(&self) -> BTreeMap<&'static str, u32> {
        self.marks.lock().clone()
    }

    pub fn timer_mark(&self, phase: &'static str) {
        self.timer.lock().mark(phase);
    }

    pub fn timer_line(&self) -> (u128, String) {
        let t = self.timer.lock();
        (t.total_ms(), t.breakdown())
    }

    pub fn timings(&self) -> BTreeMap<String, u64> {
        let mut out: BTreeMap<String, u64> = self
            .timer
            .lock()
            .phases()
            .iter()
            .map(|(k, v)| (format!("boot.{k}"), *v as u64))
            .collect();
        for (k, v) in self.marks.lock().iter() {
            out.insert(format!("mark.{k}"), u64::from(*v));
        }
        if let Some(at) = self.window_visible_at() {
            out.insert("windowVisibleMs".into(), (at - self.process_started_ms).max(0) as u64);
        }
        out
    }

    /// B5: the window is visible. Idempotent; logs `window_visible_ms`.
    pub fn window_visible(&self, at: i64) {
        if self.window_visible.borrow().is_some() {
            return;
        }
        self.window_visible.send_replace(Some(at));
        tracing::info!(
            target: "app",
            window_visible_ms = (at - self.process_started_ms).max(0),
            "window visible"
        );
    }

    pub fn window_visible_at(&self) -> Option<i64> {
        *self.window_visible.borrow()
    }

    /// Resolves once the window was recorded visible.
    pub async fn wait_window_visible(&self) -> i64 {
        let mut rx = self.window_visible.subscribe();
        loop {
            if let Some(at) = *rx.borrow_and_update() {
                return at;
            }
            if rx.changed().await.is_err() {
                return now_ms();
            }
        }
    }

    pub fn set_supervisor(&self, sup: Arc<Supervisor>) {
        let quit = Arc::new(QuitCoordinator::new(sup.clone(), self.feed.clone()));
        let _ = self.quit.set(quit);
        let _ = self.supervisor.set(sup);
    }

    pub fn supervisor(&self) -> Option<Arc<Supervisor>> {
        self.supervisor.get().cloned()
    }

    pub fn quit(&self) -> Option<Arc<QuitCoordinator>> {
        self.quit.get().cloned()
    }

    pub fn take_crash_plans(&self) -> Vec<CrashPlan> {
        std::mem::take(&mut *self.crash_plans.lock())
    }

    /// Count an error code for `app_diagnostics` (never content).
    pub fn count_error(&self, code: &str) {
        *self.error_codes.lock().entry(code.to_string()).or_insert(0) += 1;
    }

    pub fn error_codes(&self) -> BTreeMap<String, u64> {
        self.error_codes.lock().clone()
    }

    /// Enter memory-only mode and tell the store.
    pub fn enter_memory_only(&self, reason: MemoryOnlyReason) {
        self.memory_only.enter(reason);
        self.sync_memory_only();
    }

    /// Mirror the memory-only state into `Boot{status}` and the notice.
    /// Bind World (T02) and route T09's store state to it (A12.1): the one
    /// call the composition root makes, so the service binding and the
    /// feed's switch away from the local store cannot drift apart. `false`
    /// if a World was bound already (nothing changes then).
    pub fn bind_world(&self, services: &state::Services, world: Arc<dyn WorldApi>) -> bool {
        if !services.bind_world(world.clone()) {
            return false;
        }
        self.feed.bind_world(world.sink())
    }

    pub fn sync_memory_only(&self) {
        let reason = self.memory_only.reason_str();
        self.feed.update_boot(|b| b.memory_only = reason.clone());
        self.feed.set_notice(NOTICE_KEY, self.memory_only.notice());
    }
}

fn platform() -> Platform {
    let release = {
        // SAFETY: `uname` fills a zeroed, correctly sized struct.
        let mut u: libc::utsname = unsafe { std::mem::zeroed() };
        if unsafe { libc::uname(&mut u) } == 0 {
            let bytes: Vec<u8> = u
                .release
                .iter()
                .take_while(|c| **c != 0)
                .map(|c| *c as u8)
                .collect();
            String::from_utf8_lossy(&bytes).into_owned()
        } else {
            String::new()
        }
    };
    // Darwin 20 = macOS 11, 21 = 12, … 25 = 26 (the 2025 renumbering).
    let darwin: u32 = release.split('.').next().and_then(|m| m.parse().ok()).unwrap_or(0);
    let macos_major = match darwin {
        0 => 0,
        d if d >= 25 => d + 1,
        d if d >= 20 => d - 9,
        d => 10 + d.saturating_sub(4) / 100,
    };
    Platform {
        macos_major,
        arm64: cfg!(target_arch = "aarch64"),
    }
}

fn process_started_ms() -> i64 {
    let pid = std::process::id() as i32;
    libproc::libproc::proc_pid::pidinfo::<libproc::libproc::bsd_info::BSDInfo>(pid, 0)
        .map(|b| (b.pbi_start_tvsec as i64) * 1000 + (b.pbi_start_tvusec as i64) / 1000)
        .unwrap_or_else(|_| now_ms())
}

/// The persisted UI rows (`settings_kv` under `ui.*`), least recently
/// written first: `ui_state_set` writes a row as DELETE + INSERT, so the
/// rowid order is the LRW clock of A45.2 across restarts. A value that is not
/// JSON reads as `None` (dropped by [`Lifecycle::load_ui_state`]).
fn read_ui_rows(conn: &rusqlite::Connection) -> Result<Vec<(String, Option<Value>)>> {
    let mut stmt = conn.prepare("SELECT key, value_json FROM settings_kv WHERE substr(key, 1, 3) = 'ui.' ORDER BY rowid")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let mut out = Vec::new();
    for row in rows {
        let (key, json) = row?;
        let key = key[dao::ui_state::PREFIX.len()..].to_string();
        out.push((key, serde_json::from_str(&json).ok()));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// setup(): B1–B4 (+ memory-and-SQLite-only preparation)
// ---------------------------------------------------------------------------

/// What [`setup_core`] produced.
pub enum SetupOutcome {
    Ready(Box<SetupOutput>),
    /// Another instance holds the lock; it was asked to come forward.
    SecondInstance { holder_pid: Option<i32>, focus_requested: bool },
}

pub struct SetupOutput {
    pub paths: Arc<AppPaths>,
    pub db: Db,
    pub config: Arc<ConfigStore>,
    pub lock: Option<InstanceLock>,
    pub lifecycle: Arc<Lifecycle>,
    pub supervisor: Arc<Supervisor>,
}

/// B1–B4 and the in-memory preparation of B5 (see the module docs).
///
/// `version` is this build's version, `build_id` the id the WebView must match.
pub async fn setup_core(paths: AppPaths, version: &str, build_id: &str) -> Result<SetupOutcome> {
    let lifecycle = Lifecycle::new(version, build_id);
    let paths = Arc::new(paths);

    // B1: the roots, and whether they can be written at all.
    let mut memory_only: Option<MemoryOnlyReason> = None;
    if let Err(err) = paths.ensure_all() {
        memory_only = Some(match &err {
            GildeError::Io(io) => MemoryOnlyReason::of_io(io),
            _ => MemoryOnlyReason::Unwritable,
        });
    }
    if memory_only.is_none() {
        if let Err(reason) = probe_writable(paths.app_data_dir()) {
            memory_only = Some(reason);
        }
    }
    lifecycle.timer_mark("B1.paths");

    // B2: `state.db`, or the in-memory stand-in of memory-only mode.
    let db = match memory_only {
        Some(_) => Db::open_memory_only(&paths.state_db())?,
        None => match Db::open(&paths.state_db()) {
            Ok(db) => db,
            Err(err) => {
                let reason = match &err {
                    GildeError::Io(io) => Some(MemoryOnlyReason::of_io(io)),
                    GildeError::Db(msg) => MemoryOnlyReason::of_db_message(msg),
                    _ => None,
                };
                match reason {
                    Some(r) => {
                        memory_only = Some(r);
                        Db::open_memory_only(&paths.state_db())?
                    }
                    // A newer schema refuses to boot (the caller shows the dialog).
                    None => return Err(err),
                }
            }
        },
    };
    if let Some(reason) = memory_only {
        lifecycle.enter_memory_only(reason);
    }
    lifecycle.timer_mark("B2.db");

    // B3: settings.
    let config = Arc::new(ConfigStore::load(&paths)?);
    lifecycle.timer_mark("B3.config");

    // B4: the single-instance lock. A second launch focuses the first and goes.
    let lock_path = paths.instance_lock();
    let lock = match state::acquire(&lock_path) {
        Ok(LockOutcome::Acquired(lock)) => Some(lock),
        Ok(LockOutcome::Held { pid }) => {
            let focus_requested = state::request_focus(&lock_path).is_ok();
            return Ok(SetupOutcome::SecondInstance {
                holder_pid: pid,
                focus_requested,
            });
        }
        Err(err) => {
            tracing::warn!(target: "app", error = %err, "could not take the single-instance lock");
            None
        }
    };
    lifecycle.timer_mark("B4.lock");

    // Memory and SQLite only from here: the crash records to write in B6 and
    // the launch kind for `app_bootstrap` (B5 answers from memory).
    let plans = if memory_only.is_none() {
        db.read(crash::plan_crash_records).await.unwrap_or_else(|err| {
            tracing::warn!(target: "app", error = %err, "could not plan crash records");
            Vec::new()
        })
    } else {
        Vec::new()
    };
    let now = now_ms();
    let plans_for_launch = plans.clone();
    let launch = db
        .read(move |conn| crash::launch_info(conn, &plans_for_launch, now))
        .await
        .unwrap_or(LaunchInfo::Restore);
    lifecycle.set_launch(launch);
    *lifecycle.crash_plans.lock() = plans;
    if let Ok(rows) = db.read(read_ui_rows).await {
        let gone = lifecycle.load_ui_state(rows);
        if !gone.is_empty() && memory_only.is_none() {
            tracing::info!(target: "app", rows = gone.len(), "persisted UI rows dropped (schema or caps, A45.2)");
            let result = db
                .write(move |conn| {
                    for key in &gone {
                        dao::settings_kv::delete(conn, &format!("{}{key}", dao::ui_state::PREFIX))?;
                    }
                    Ok(())
                })
                .await;
            if let Err(err) = result {
                tracing::warn!(target: "app", error = %err, "could not drop the invalid UI rows");
            }
        }
    }

    // The instance row (one durable insert; the gate stays closed until B6).
    let supervisor = Arc::new(
        Supervisor::open_instance(&db, config.clone(), version)
            .await?
            .with_spawn_gate(lifecycle.gate.clone())
            .with_memory_only(lifecycle.memory_only.clone())
            .with_scan_hub(lifecycle.scans.clone()),
    );
    lifecycle.set_supervisor(supervisor.clone());
    lifecycle.timer_mark("B4.instance");

    Ok(SetupOutcome::Ready(Box::new(SetupOutput {
        paths,
        db,
        config,
        lock,
        lifecycle,
        supervisor,
    })))
}

// ---------------------------------------------------------------------------
// B6: the boot sweep, which opens the spawn gate
// ---------------------------------------------------------------------------

/// What B6 did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BootSweepReport {
    pub crash_records: Vec<String>,
    pub sweep: crate::supervise::reaper::SweepCounts,
    pub quit_survivors_scheduled: u32,
    /// A29.3: the one window of every stop the sweep scheduled (`None` when
    /// it scheduled none) — what `BootStatus.recovery` was set to.
    pub recovery: Option<RecoveryWindow>,
}

/// A29.3: the boot sweep's stop group reports its window to the feed:
/// `BootStatus.recovery` is set when the window opens and cleared when it
/// ends (the stops fire) or the undo cancels it.
struct FeedRecovery(Arc<LifecycleFeed>);

impl StopGroupObserver for FeedRecovery {
    fn started(&self, window: &RecoveryWindow) {
        self.0.set_recovery(window.clone());
    }

    fn ended(&self, token: &UndoToken, _fired: bool) {
        self.0.clear_recovery(token);
    }
}

/// B6: write the planned crash records, run the reaper's boot sweep, schedule
/// the quit survivors of the previous instance (fenced SIGTERM after the 10 s
/// undo window, never SIGKILL), then **open the spawn gate**.
///
/// A29.3: every stop the sweep schedules — a crashed instance's session
/// children (§10.3) and the previous instance's quit survivors (§10.2 step
/// 6) — joins **one** group under one undo token; `BootStatus.recovery`
/// carries its window for the Recovered dialog.
pub async fn run_boot_sweep(lifecycle: &Lifecycle, sup: &Arc<Supervisor>) -> BootSweepReport {
    run_boot_sweep_with(lifecycle, sup, crate::supervise::reaper::SweepOpts::default()).await
}

/// [`run_boot_sweep`] with the undo window and kill grace made explicit
/// (tests; the app uses the 10 s window).
pub async fn run_boot_sweep_with(
    lifecycle: &Lifecycle,
    sup: &Arc<Supervisor>,
    opts: crate::supervise::reaper::SweepOpts,
) -> BootSweepReport {
    use crate::supervise::reaper;
    let mut report = BootSweepReport::default();
    for plan in lifecycle.take_crash_plans() {
        if let Some(id) =
            quit::write_record_with_id(sup, &plan.record_id, CAUSE_CRASH, &plan.items).await
        {
            report.crash_records.push(id);
        }
    }
    let mut group = StopGroup::new();
    let sweep = reaper::sweep_startup_into(sup, opts, &mut group).await;
    report.sweep = reaper::SweepCounts::of(&sweep);
    report.quit_survivors_scheduled = reaper::sweep_quit_survivors_into(sup, &mut group).await;
    report.recovery = group.arm(
        sup.clone(),
        opts.undo_window,
        Some(Arc::new(FeedRecovery(lifecycle.feed.clone()))),
    );
    lifecycle.gate.open();
    lifecycle.feed.update_boot(|b| b.spawn_gate_open = true);
    lifecycle.timer_mark("B6.sweep");
    report
}

// ---------------------------------------------------------------------------
// B9 / B10: detection and the static canary, off the boot path
// ---------------------------------------------------------------------------

/// A29.1: the detected-agent rows — one per vendor in the order claude,
/// codex, cursor, agy; a vendor without an executable is `installed: false`
/// with no version. The one mapping behind both `BootStatus.agents` and
/// `newsession_options.vendors` (T10's builder calls it), so the two never
/// disagree.
pub fn agent_rows(detected: &[AgentBinary]) -> Vec<VendorOption> {
    Vendor::ALL
        .iter()
        .map(|v| {
            let kind = crate::agents::AgentKind::of_vendor(*v);
            let bin = detected.iter().find(|b| b.kind == kind);
            let installed = bin.is_some_and(|b| b.installed && b.abs_path.is_some());
            VendorOption {
                vendor: *v,
                installed,
                version: bin.filter(|_| installed).and_then(|b| b.version.clone()),
            }
        })
        .collect()
}

/// A29.1: run one detection and publish its rows through the feed. While
/// `detect` runs nothing is published, so the previous rows (and
/// `agents_detected`) stay; when it completes its rows replace them and
/// `agents_scanned_at` moves forward. `detect` yields `None` when the
/// detection failed: the previous rows are kept then (the first detection
/// publishes every vendor as not installed, so the UI leaves "Finding
/// installed agents…"). A detection that started before a newer one never
/// overwrites the newer rows.
pub async fn detect_and_publish<F>(feed: &LifecycleFeed, detect: F) -> Option<Vec<AgentBinary>>
where
    F: std::future::Future<Output = Option<Vec<AgentBinary>>>,
{
    let ticket = feed.begin_agents_scan();
    let detected = detect.await;
    let published = feed.agents_scanned(ticket, now_ms(), |prev| match &detected {
        Some(bins) => agent_rows(bins),
        None if prev.is_empty() => agent_rows(&[]),
        None => prev.to_vec(),
    });
    if !published {
        tracing::debug!(target: "agents", "a newer agent detection published first; this one is dropped");
    }
    detected
}

/// B9 + B10: PATH + `--version` detection (registered `vendorcli` rows), the
/// detected vendor binaries allowlisted (§9.3), then the static canary for
/// every detected `claude` through `CompatApi::schedule_canary` (T21) when it
/// is bound. Runs on the blocking pool; never on the boot path.
pub async fn run_detection(
    lifecycle: &Lifecycle,
    agents: &Arc<AgentRegistry>,
    compat: Option<Arc<dyn CompatApi>>,
) -> Vec<AgentBinary> {
    run_detection_with(lifecycle, agents, compat, false).await
}

/// [`run_detection`], or with `refresh` a **rerun** (A29.1: `agents_rescan`,
/// an applied `*Binary` / `pathsOverride` setting): the PATH is resolved
/// again and every binary probed again, bypassing the detection caches.
/// The rows of the previous detection stay published until this one
/// completes.
pub async fn run_detection_with(
    lifecycle: &Lifecycle,
    agents: &Arc<AgentRegistry>,
    compat: Option<Arc<dyn CompatApi>>,
    refresh: bool,
) -> Vec<AgentBinary> {
    let detect = async {
        let report = {
            let agents = agents.clone();
            tokio::task::spawn_blocking(move || agents.path_report(refresh))
                .await
                .ok()
        };
        if let Some(r) = &report {
            crate::agents::BinaryResolver::global().set_path_env(r.joined());
        }
        let detected = match agents.detect(refresh).await {
            Ok(rows) => rows,
            Err(err) => {
                tracing::warn!(target: "agents", error = %err, "agent detection failed");
                return None;
            }
        };
        for bin in detected.iter().filter(|b| b.installed) {
            if let Some(path) = &bin.abs_path {
                crate::supervise::spawn::allow_executable(path, &crate::agents::VENDOR_KINDS);
            }
        }
        Some(detected)
    };
    let detected = detect_and_publish(&lifecycle.feed, detect).await.unwrap_or_default();
    lifecycle.timer_mark("B9.agents");

    if let Some(compat) = compat {
        for bin in detected.iter().filter(|b| b.installed && b.kind == crate::agents::AgentKind::Claude) {
            let Some(path) = bin.abs_path.clone() else { continue };
            match crate::agents::resolve_binary(&path, bin.kind, None).await {
                Ok(resolved) => compat.schedule_canary(&resolved.binary),
                Err(err) => {
                    let (code, key, _) = err.log_fields();
                    tracing::debug!(target: "agents", code, key, "could not resolve claude for the canary");
                }
            }
        }
        lifecycle.timer_mark("B10.canary");
    }
    detected
}

/// A29.1: rerun detection in the background (`agents_rescan`, an applied
/// `*Binary` / `pathsOverride` setting) and replace the published rows when
/// it completes. Returns at once: the caller's answer never waits for it;
/// the PATH probe and the `--version` probes run on the blocking pool.
///
/// A45.3: one rerun at a time — a rescan while one runs **joins** it (the
/// handle completes with that run). See [`spawn_rescan_as`].
pub fn spawn_rescan(
    lifecycle: Arc<Lifecycle>,
    agents: Arc<AgentRegistry>,
    compat: Option<Arc<dyn CompatApi>>,
) -> tauri::async_runtime::JoinHandle<()> {
    spawn_rescan_as(lifecycle, agents, compat, RescanMode::Join)
}

/// [`spawn_rescan`] with the way it meets a run in flight: `Join` adds
/// nothing; `AfterChange` asks the run in flight for exactly one more pass
/// when it completes (any number of changes meanwhile → one pass). The
/// handle completes when the run it joined, and any pass after it, is done.
pub fn spawn_rescan_as(
    lifecycle: Arc<Lifecycle>,
    agents: Arc<AgentRegistry>,
    compat: Option<Arc<dyn CompatApi>>,
    mode: RescanMode,
) -> tauri::async_runtime::JoinHandle<()> {
    let (generation, tx) = {
        let mut slot = lifecycle.rescan.lock();
        if let Some((_, done)) = slot.running.clone() {
            if mode == RescanMode::AfterChange {
                slot.again = true;
            }
            drop(slot);
            return tauri::async_runtime::spawn(wait_done(done));
        }
        slot.generation += 1;
        let (tx, rx) = watch::channel(false);
        slot.running = Some((slot.generation, rx));
        slot.again = false;
        (slot.generation, tx)
    };
    tauri::async_runtime::spawn(async move {
        // Frees the slot even if a pass panics or the task is cancelled.
        let _run = RescanRun { lifecycle: lifecycle.clone(), generation, done: tx };
        loop {
            let detected = run_detection_with(&lifecycle, &agents, compat.clone(), true).await;
            tracing::info!(
                target: "agents",
                installed = detected.iter().filter(|b| b.installed).count(),
                "agent detection rerun finished"
            );
            let mut slot = lifecycle.rescan.lock();
            if !std::mem::take(&mut slot.again) {
                slot.running = None;
                break;
            }
        }
    })
}

/// Clears its rerun from the slot (unless a newer one holds it) and signals
/// "done" to every joined caller.
struct RescanRun {
    lifecycle: Arc<Lifecycle>,
    generation: u64,
    done: watch::Sender<bool>,
}

impl Drop for RescanRun {
    fn drop(&mut self) {
        let mut slot = self.lifecycle.rescan.lock();
        if slot.running.as_ref().is_some_and(|(g, _)| *g == self.generation) {
            slot.running = None;
            slot.again = false;
        }
        drop(slot);
        let _ = self.done.send(true);
    }
}

async fn wait_done(mut done: watch::Receiver<bool>) {
    while !*done.borrow_and_update() {
        if done.changed().await.is_err() {
            break;
        }
    }
}

/// Whether a detection rerun is in flight (A45.3; tests and diagnostics).
pub fn rescan_running(lifecycle: &Lifecycle) -> bool {
    lifecycle.rescan.lock().running.is_some()
}

/// The quit preview right now: from World when it is bound, else from the
/// registry's view of this instance's session rows.
pub async fn current_quit_preview(state: &state::AppState) -> QuitPreview {
    preview_for(&state.services, &state.supervisor).await
}

/// [`current_quit_preview`] from its two inputs: World's sessions when it is
/// bound, plus every live session row of this instance's registry that World
/// has not classified yet (a session spawned a moment before ⌘Q is still in
/// the record, §10.2 "the record first"); the registry alone otherwise.
pub async fn preview_for(services: &state::Services, sup: &Supervisor) -> QuitPreview {
    let me = sup.app_instance_id().to_string();
    let (rows, parents) = sup
        .db()
        .read(move |conn| {
            let rows = dao::proc::rows_of_instance(conn, &me)?;
            let parents = dao::proc::parents_of_instance(conn, &me)?.into_iter().collect();
            Ok((rows, parents))
        })
        .await
        .unwrap_or_default();
    let from_rows = quit::preview_from_rows(&rows, &parents);
    match services.world() {
        Some(world) => quit::merge_previews(quit::preview_from_snapshot(&world.snapshot()), from_rows),
        None => from_rows,
    }
}

/// The memory-only re-check (§2.4): every 60 s while active; when writes work
/// again the on-disk `state.db` is swapped back in.
pub fn spawn_memory_only_recheck(
    lifecycle: Arc<Lifecycle>,
    db: Db,
    app_data_dir: &Path,
) -> tauri::async_runtime::JoinHandle<()> {
    let lc = lifecycle.clone();
    let on_resume = Arc::new(move || {
        if let Err(err) = db.resume_on_disk() {
            tracing::warn!(target: "app", error = %err, "writes resumed but state.db could not be reopened");
        }
        lc.memory_only.leave();
        lc.sync_memory_only();
    });
    crate::lifecycle::memory_only::spawn_recheck(
        lifecycle.memory_only.clone(),
        app_data_dir.to_path_buf(),
        crate::lifecycle::memory_only::RECHECK_INTERVAL,
        on_resume,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_are_kept_once() {
        let lc = Lifecycle::new("0.0.0", "b");
        assert!(lc.mark(AppMark::FirstPaint, 120));
        assert!(!lc.mark(AppMark::FirstPaint, 999));
        assert_eq!(lc.marks()["firstPaint"], 120);
        assert_eq!(lc.timings()["mark.firstPaint"], 120);
    }

    #[tokio::test]
    async fn window_visible_is_recorded_once_and_wakes_waiters() {
        let lc = Lifecycle::new("0.0.0", "b");
        let l2 = lc.clone();
        let waiter = tokio::spawn(async move { l2.wait_window_visible().await });
        lc.window_visible(1_000);
        lc.window_visible(2_000);
        assert_eq!(waiter.await.unwrap(), 1_000);
        assert_eq!(lc.window_visible_at(), Some(1_000));
    }

    #[test]
    fn the_platform_is_plausible() {
        let p = platform();
        assert!(p.macos_major >= 11, "{p:?}");
    }

    // -- A45.2: the bounded UI store ------------------------------------------

    fn team(i: usize) -> String {
        format!("teamLayout:t_{i:016x}")
    }

    fn member(i: usize) -> String {
        format!("m_{i:016x}")
    }

    fn layout() -> Value {
        serde_json::json!({"mode": "panes", "focused": null, "zoomed": null, "leadRatio": null, "tileOrder": null})
    }

    #[test]
    fn the_65th_team_layout_evicts_the_least_recently_written_one() {
        let mut ui = UiStore::default();
        assert!(ui.put("homeMode", serde_json::json!("focus")).is_empty());
        for i in 0..64 {
            assert!(ui.put(&team(i), layout()).is_empty(), "{i}");
        }
        // Writing team 0 again makes team 1 the least recently written.
        assert!(ui.put(&team(0), layout()).is_empty());
        assert_eq!(ui.put(&team(64), layout()), vec![team(1)]);
        assert_eq!(ui.values().keys().filter(|k| UiKey::is_team_layout_str(k)).count(), 64);
        assert!(ui.contains(&team(0)) && ui.contains(&team(64)) && !ui.contains(&team(1)));
        assert!(ui.contains("homeMode"), "only team layouts are evicted");
        assert_eq!(ui.put(&team(65), layout()), vec![team(2)]);
    }

    #[test]
    fn the_whole_state_stays_under_256_kib_and_the_written_key_stays() {
        let mut ui = UiStore::default();
        // Values over the schema's own sizes, to drive the byte cap alone.
        let big = Value::String("x".repeat(60 * 1024));
        for i in 0..4 {
            assert!(ui.put(&team(i), big.clone()).is_empty());
        }
        assert_eq!(ui.put(&team(4), big.clone()), vec![team(0)], "a fifth 60 KiB row is over 256 KiB");
        assert!(ui.bytes() <= UI_STATE_MAX_BYTES, "{}", ui.bytes());
        // A key that is not a team layout is never evicted, and neither is the
        // key just written, even when nothing else can go.
        let mut ui = UiStore::default();
        for k in ["lastView", "splitSet", "sidebarOrder", "sidebarFolds", "floorPrefs"] {
            ui.put(k, big.clone());
        }
        assert!(ui.put(&team(9), big.clone()).is_empty());
        assert!(ui.contains(&team(9)));
        // Replacing a value replaces its bytes.
        let mut ui = UiStore::default();
        ui.put("homeMode", big.clone());
        ui.put("homeMode", serde_json::json!("focus"));
        assert_eq!(ui.bytes(), ipc::ui_entry_bytes("homeMode", &serde_json::json!("focus")));
    }

    #[test]
    fn the_largest_values_the_schemas_accept_fit_the_cap() {
        let uid = |i: usize| format!("s_0190a1b2-c3d4-7e5f-8a9b-{i:012x}");
        let project = |i: usize| format!("p_{i:016x}");
        let mut values: Vec<(String, Value)> = vec![
            ("lastView".into(), serde_json::json!({"screen": "session", "uid": uid(1), "sub": "conversation"})),
            ("splitSet".into(), serde_json::json!({"uids": (0..6).map(uid).collect::<Vec<_>>(), "focused": uid(0)})),
            ("homeMode".into(), serde_json::json!("classic")),
            ("floorPrefs".into(), serde_json::json!({"list": false, "traffic": false, "legend": false})),
            ("sidebarOrder".into(), Value::from((0..500).map(project).collect::<Vec<_>>())),
            (
                "sidebarFolds".into(),
                Value::Object((0..500).map(|i| (format!("s:{}", uid(i)), Value::Bool(false))).collect()),
            ),
        ];
        let full = serde_json::json!({
            "mode": "board",
            "focused": member(0),
            "zoomed": member(1),
            "leadRatio": 0.55,
            "tileOrder": (0..64).map(member).collect::<Vec<_>>(),
        });
        for i in 0..64 {
            values.push((team(i), full.clone()));
        }
        let mut ui = UiStore::default();
        for (k, v) in values {
            ipc::validate_ui_value(&k, &v).unwrap_or_else(|e| panic!("{k}: {e}"));
            assert!(ui.put(&k, v).is_empty(), "{k}: the schemas alone keep the state under the cap");
        }
        assert!(ui.bytes() <= UI_STATE_MAX_BYTES, "{} bytes", ui.bytes());
    }

    #[test]
    fn rows_that_fail_their_schema_at_load_are_dropped_never_served() {
        let lc = Lifecycle::new("0.0.0", "b");
        let mut rows: Vec<(String, Option<Value>)> = vec![
            ("lastView".into(), Some(serde_json::json!({"screen": "home", "draft": "CANARY"}))),
            ("homeMode".into(), Some(serde_json::json!("CANARY"))),
            ("evil".into(), Some(serde_json::json!(1))),
            ("splitSet".into(), None),
            ("floorPrefs".into(), Some(serde_json::json!({"list": true}))),
        ];
        for i in 0..66 {
            rows.push((team(i), Some(layout())));
        }
        let mut gone = lc.load_ui_state(rows);
        gone.sort();
        let mut want = vec!["evil".to_string(), "homeMode".into(), "lastView".into(), "splitSet".into(), team(0), team(1)];
        want.sort();
        assert_eq!(gone, want, "the invalid rows and the two oldest layouts");
        let ui = lc.ui_state();
        assert_eq!(ui.len(), 65, "floorPrefs + 64 layouts");
        assert!(!serde_json::to_string(&ui).unwrap().contains("CANARY"));
    }
}
