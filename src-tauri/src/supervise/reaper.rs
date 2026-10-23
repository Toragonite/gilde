//! The reaper — DESIGN §8-5: the boot sweep, planned-row adoption, the
//! generation sweep and the periodic reconcile.
//!
//! Everything here answers one question: *which of the processes this app once
//! started are still out there, and what may we do about them?* The answer is
//! deliberately conservative at every branch, because the two failure modes are
//! not symmetric — leaking an orphan costs memory the user can see and clean up
//! from the Process Monitor, while signalling a process we merely *believe* is
//! ours costs somebody their work.
//!
//! The five steps of the boot sweep, in order:
//!
//! 1. read this machine's `boot_id`;
//! 2. for every `app_instance` still open: a different `boot_id` means every pid
//!    it recorded is meaningless, so its rows are marked `lost` **without a
//!    fence check and without a single signal**; a live sibling copy of the app
//!    is left strictly alone; anything else crashed;
//! 3. the crashed instance's live rows: `planned` rows go through
//!    [`adopt_planned_row`] (exactly-one match or nothing), `running`/`adopted`
//!    rows through the three identity fences;
//! 4. the untracked-orphan sweep — live processes stamped with a dead
//!    `GILDE_APP_INSTANCE`. Best-effort by construction (DESIGN §8-4: reading
//!    another process's environment block is routinely refused on macOS);
//! 5. close the crashed instances.
//!
//! ## Why the auto-termination is *scheduled* rather than performed
//!
//! DESIGN §8-5 gives headless processes past `maxHeadlessLifetimeMinutes` an
//! automatic termination **with a toast and a 10 second undo**. An undo that
//! cannot undo anything is theatre, so the kill is scheduled for the end of the
//! window and [`undo_cancel`] removes it before it fires. What
//! [`SweepReport::auto_terminated`] carries is therefore the *dry-run* report of
//! each scheduled kill: the exact target and exclusion sets, computed at sweep
//! time, with `dry_run: true` and not one signal sent. The real report is logged
//! and published on `gilde://proc/registry` when the window closes.
//!
//! ## What this module never does
//!
//! It never resolves a `cwd` for a row nobody is looking at. `cwd` costs a
//! syscall per pid and this machine can have 800 processes; the registry already
//! stores the cwd the process was *started* in, which is what the recovery sheet
//! shows. [`reconcile`] therefore does liveness, RSS and idle only — DESIGN
//! §8-5's "표시 중인 행에만 `libproc` cwd를 조회한다".

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::events::{
    self, ProcRegistryEvent, RecoveryPendingEvent, ToastEvent, ToastLevel, PROC_REGISTRY,
    RECOVERY_PENDING,
};
use crate::store::models::{AppInstanceRow, ProcKind, ProcRow, ProcState};
use crate::store::{dao, now_ms};
use crate::supervise::identity::{
    argv_sig_of, live_argv, verify, Fence, MismatchReason, ENV_SPAWN_ID,
};
use crate::supervise::kill::{kill_tree, KillOpts, KillOutcome, KillReport, KillTarget};
use crate::supervise::procscan::{self, ps_snapshot, PsRow, Snapshot};
use crate::supervise::registry::{SpawnPlan, Supervisor, ENV_APP_INSTANCE};
use crate::supervise::spawn::{is_alive, pgid_of};

// ---------------------------------------------------------------------------
// constants
// ---------------------------------------------------------------------------

/// How long an automatic termination may be undone (DESIGN §8-5).
pub const UNDO_WINDOW: Duration = Duration::from_secs(10);

/// Periodic reconcile cadence (DESIGN §8-5 "30초마다, 그리고 윈도 포커스 시").
pub const RECONCILE_INTERVAL: Duration = Duration::from_secs(30);

/// How long the shutdown sweep may spend ending this instance's children before
/// the app stops waiting. A quit must never turn into a beachball.
pub const SHUTDOWN_SWEEP_BUDGET: Duration = Duration::from_secs(6);

/// `note` values. Every non-obvious state transition carries one, because the
/// note column is the only thing that can explain a `lost` row after the fact.
pub mod note {
    pub const REBOOT: &str = "reboot";
    pub const REAPED: &str = "reaped";
    pub const CRASH_ADOPTED: &str = "adopted from a crashed instance";
    pub const PLANNED_ADOPTED: &str = "planned row adopted";
    pub const DETACHED: &str = "detached (keepAlive)";
    pub const AMBIGUOUS: &str = "ambiguous planned-row candidates";
    pub const NO_CANDIDATE: &str = "no live process matched the planned row";
    pub const UNTRACKED: &str = "untracked orphan";
    pub const GENERATION: &str = "sidecar generation swept";
    pub const HEADLESS_LIFETIME: &str = "past maxHeadlessLifetimeMinutes";
    pub const KEPT: &str = "kept by the user";
    pub const SHUTDOWN: &str = "host shutdown";
    /// A Gilde session's process left by a crashed instance (contract §10.3):
    /// fenced SIGTERM after the undo window, never SIGKILL for a vendor CLI.
    pub const CRASH_LEFTOVER: &str = "crash leftover";
    /// A vendor process that outlived the quit bound (contract §10.2 step 6).
    pub const QUIT_SURVIVOR: &str = crate::lifecycle::quit::NOTE_QUIT_SURVIVOR;
}

/// Toast keys this module emits. Mirrored in `src/i18n/{ko,en}.ts`.
pub mod toast_key {
    pub const HEADLESS_TERMINATED: &str = "toast.recovery.headlessTerminated";
}

// ---------------------------------------------------------------------------
// wire types
// ---------------------------------------------------------------------------

/// What the recovery sheet proposes for a row. Mirrors `ipc.ts::RecoveryAction`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RecoveryAction {
    Terminate,
    Keep,
    Adopt,
}

/// One live process the boot sweep could not decide about on its own.
///
/// Field names follow M1-PLAN T11; two of them carry a `serde(rename)` so the
/// wire shape stays exactly `ipc.ts::RecoveredProc` (`argvDisplay`,
/// `worktreeId`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveredProc {
    pub spawn_id: String,
    pub kind: ProcKind,
    pub vendor: Option<String>,
    pub pid: Option<i32>,
    #[serde(rename = "argvDisplay")]
    pub argv_masked: String,
    pub cwd: String,
    #[serde(rename = "worktreeId")]
    pub worktree: Option<String>,
    pub rss_bytes: u64,
    pub age_secs: u64,
    pub keep_alive: bool,
    /// Pre-selected action. Always [`RecoveryAction::Terminate`] — DESIGN §8-5
    /// makes *Terminate* the default so one click resolves the whole sheet.
    pub suggested: RecoveryAction,
}

/// One user decision from the recovery sheet. Wire shape:
/// `{ "action": "terminate", "spawnId": "…" }` = `ipc.ts::RecoveryDecision`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum RecoveryDecision {
    Terminate { spawn_id: String },
    Keep { spawn_id: String },
    Adopt { spawn_id: String },
}

impl RecoveryDecision {
    pub fn spawn_id(&self) -> &str {
        match self {
            RecoveryDecision::Terminate { spawn_id }
            | RecoveryDecision::Keep { spawn_id }
            | RecoveryDecision::Adopt { spawn_id } => spawn_id,
        }
    }

    pub fn action(&self) -> RecoveryAction {
        match self {
            RecoveryDecision::Terminate { .. } => RecoveryAction::Terminate,
            RecoveryDecision::Keep { .. } => RecoveryAction::Keep,
            RecoveryDecision::Adopt { .. } => RecoveryAction::Adopt,
        }
    }
}

/// What one boot sweep did. Never an error: a sweep that could not read the
/// database reports zeros and the app still boots (M1-PLAN T11 acceptance 9).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SweepReport {
    /// Rows closed by the reboot branch — no fence, no signal.
    pub reboot_lost: u32,
    /// Rows taken over by this instance (`keep_alive`, or a planned-row match).
    pub adopted: u32,
    /// Rows whose process was already gone.
    pub exited: u32,
    /// Rows we could not prove anything about. Never signalled.
    pub lost: u32,
    /// Dry-run reports of the kills scheduled behind a 10 s undo window.
    pub auto_terminated: Vec<KillReport>,
    /// Rows the user must decide about — the Recovered Sessions sheet.
    /// A29.3: each is also published as Leftover `GildeCrashLeftover`.
    pub pending: Vec<RecoveredProc>,
    /// A29.3: the window of the stops this sweep scheduled, when it armed
    /// them itself ([`sweep_startup_with`]); `None` from
    /// [`sweep_startup_into`], whose caller arms the group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery: Option<crate::contract::store::RecoveryWindow>,
}

impl SweepReport {
    pub fn pending_count(&self) -> u32 {
        self.pending.len() as u32
    }

    /// Whether the sweep changed anything at all.
    pub fn touched_anything(&self) -> bool {
        self.reboot_lost > 0
            || self.adopted > 0
            || self.exited > 0
            || self.lost > 0
            || !self.auto_terminated.is_empty()
            || !self.pending.is_empty()
    }
}

/// Knobs the boot sweep exposes for tests and for a future settings screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweepOpts {
    /// How long an automatic termination stays undoable.
    pub undo_window: Duration,
    /// Grace given to a scheduled kill's SIGTERM before SIGKILL.
    pub kill_grace: Duration,
}

impl Default for SweepOpts {
    fn default() -> Self {
        SweepOpts {
            undo_window: UNDO_WINDOW,
            kill_grace: crate::supervise::kill::DEFAULT_GRACE,
        }
    }
}

/// One reconciled registry row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReconciledRow {
    pub spawn_id: String,
    pub pid: i32,
    pub alive: bool,
    pub rss_bytes: u64,
    /// `now - last_io_at` for a PTY row, `now - started_at` otherwise.
    pub idle_secs: u64,
}

/// What one reconcile pass observed. Carries **no cwd**: see the module header.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReconcileReport {
    pub checked: u32,
    pub exited: u32,
    /// Rows whose state changed — the payload of `gilde://proc/registry`.
    pub changed: Vec<String>,
    pub rows: Vec<ReconciledRow>,
    /// Summed RSS of the rows that are still alive.
    pub rss_bytes: u64,
    pub at: i64,
}

// ---------------------------------------------------------------------------
// the pending set
// ---------------------------------------------------------------------------

fn pending_store() -> &'static Mutex<Vec<RecoveredProc>> {
    static PENDING: OnceLock<Mutex<Vec<RecoveredProc>>> = OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(Vec::new()))
}

/// The rows the last boot sweep left undecided, minus the ones that have since
/// died on their own. `recovery_pending()` is exactly this.
pub fn pending() -> Vec<RecoveredProc> {
    let mut store = pending_store().lock();
    store.retain(|p| match p.pid {
        Some(pid) => is_alive(pid),
        None => false,
    });
    store.clone()
}

/// Replace the pending set (the boot sweep owns it).
fn set_pending(rows: Vec<RecoveredProc>) {
    *pending_store().lock() = rows;
}

/// Drop one row from the pending set once the user has decided about it.
fn resolve_pending(spawn_id: &str) {
    pending_store().lock().retain(|p| p.spawn_id != spawn_id);
}

/// Forget every pending row. Exposed for tests, which share one process.
pub fn clear_pending() {
    pending_store().lock().clear();
}

// ---------------------------------------------------------------------------
// the undo table
// ---------------------------------------------------------------------------

/// One undo token: a single scheduled kill, or (A29.3) a group of them.
struct ScheduledKill {
    /// The spawn ids the token would stop (one for a single kill).
    targets: Vec<String>,
    group: bool,
    cancelled: Arc<AtomicBool>,
    /// Runs once when the undo lands in time (a group's window closes).
    on_cancel: Option<Box<dyn FnOnce() + Send>>,
}

fn undo_table() -> &'static Mutex<HashMap<String, ScheduledKill>> {
    static UNDO: OnceLock<Mutex<HashMap<String, ScheduledKill>>> = OnceLock::new();
    UNDO.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Cancel a scheduled kill — or every still-pending stop of a group
/// (A29.3: one token, many stops) — inside its undo window.
///
/// Returns `false` for a token that never existed, already fired or was already
/// cancelled — the frontend renders that as "too late", never as an error.
pub fn undo_cancel(token: &str) -> bool {
    let Some(mut entry) = undo_table().lock().remove(token) else {
        return false;
    };
    entry.cancelled.store(true, Ordering::SeqCst);
    if entry.group {
        tracing::info!(
            target: "supervise",
            token,
            stops = entry.targets.len(),
            "scheduled group of terminations cancelled by the user"
        );
    } else {
        tracing::info!(
            target: "supervise",
            token,
            spawn_id = %entry.targets.first().map(String::as_str).unwrap_or_default(),
            "scheduled termination cancelled by the user"
        );
    }
    // Outside the table lock: the hook publishes to the store.
    if let Some(hook) = entry.on_cancel.take() {
        hook();
    }
    true
}

/// Whether `token` still names a kill (or a group) that has not fired.
pub fn undo_is_pending(token: &str) -> bool {
    undo_table().lock().contains_key(token)
}

/// The `spawn_id` a still-open single-kill undo token would kill (`None`
/// for a group token: see [`undo_targets`]).
pub fn undo_target(token: &str) -> Option<String> {
    undo_table()
        .lock()
        .get(token)
        .filter(|e| !e.group)
        .and_then(|e| e.targets.first().cloned())
}

/// Every `spawn_id` a still-open undo token would stop (a single kill's one,
/// or a group's members); empty once it fired or was cancelled.
pub fn undo_targets(token: &str) -> Vec<String> {
    undo_table()
        .lock()
        .get(token)
        .map(|e| e.targets.clone())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// the boot sweep's stop group (A29.3)
// ---------------------------------------------------------------------------

/// Hears the window of an armed [`StopGroup`] (the boot wiring publishes it
/// as `BootStatus.recovery`).
pub trait StopGroupObserver: Send + Sync {
    /// The window opened. Called before the token can fire or be cancelled.
    fn started(&self, window: &crate::contract::store::RecoveryWindow);
    /// The window ended: the stops fired (`fired`), or the undo cancelled
    /// every stop still pending (`!fired`). Called once.
    fn ended(&self, token: &crate::contract::ids::UndoToken, fired: bool);
}

/// The stops one boot sweep schedules, collected so that they share **one**
/// undo token and one window (A29.3): a crashed instance's session children
/// (§10.3) and the previous instance's quit survivors (§10.2 step 6). The
/// headless-lifetime kill is not a member (it keeps its own toast and token).
///
/// Nothing is signalled until [`StopGroup::arm`] has run and its window has
/// passed without an undo; every stop is identity-fenced at execution by
/// `kill_tree`, and a vendor CLI is never escalated to SIGKILL.
#[derive(Debug, Default)]
pub struct StopGroup {
    stops: Vec<(String, KillOpts)>,
}

impl StopGroup {
    pub fn new() -> StopGroup {
        StopGroup::default()
    }

    /// Add one stop (a spawn id already in the group is not added twice).
    pub fn push(&mut self, spawn_id: impl Into<String>, opts: KillOpts) {
        let spawn_id = spawn_id.into();
        if !self.stops.iter().any(|(id, _)| *id == spawn_id) {
            self.stops.push((spawn_id, opts));
        }
    }

    pub fn len(&self) -> usize {
        self.stops.len()
    }

    pub fn is_empty(&self) -> bool {
        self.stops.is_empty()
    }

    /// The spawn ids collected so far, in order.
    pub fn spawn_ids(&self) -> Vec<String> {
        self.stops.iter().map(|(id, _)| id.clone()).collect()
    }

    /// Arm the group: one token, one timer for every stop. `None` (and
    /// nothing armed) for an empty group. The window's `until` is wall
    /// clock. `observer` hears `started` before the token exists in the undo
    /// table, then exactly one `ended`: when the window passes (the stops
    /// fire right after) or when `undo_cancel` takes the token in time — in
    /// which case no stop of the group is ever signalled.
    pub fn arm(
        self,
        sup: Arc<Supervisor>,
        window: Duration,
        observer: Option<Arc<dyn StopGroupObserver>>,
    ) -> Option<crate::contract::store::RecoveryWindow> {
        if self.stops.is_empty() {
            return None;
        }
        let undo = crate::contract::ids::UndoToken::mint();
        let token = undo.as_str().to_string();
        let recovery = crate::contract::store::RecoveryWindow {
            stopping: self.stops.len() as u32,
            undo_token: undo.clone(),
            until: now_ms() + window.as_millis() as i64,
        };
        if let Some(o) = &observer {
            o.started(&recovery);
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let on_cancel: Option<Box<dyn FnOnce() + Send>> = observer.clone().map(|o| {
            let undo = undo.clone();
            Box::new(move || o.ended(&undo, false)) as Box<dyn FnOnce() + Send>
        });
        undo_table().lock().insert(
            token.clone(),
            ScheduledKill {
                targets: self.spawn_ids(),
                group: true,
                cancelled: cancelled.clone(),
                on_cancel,
            },
        );
        tracing::info!(
            target: "supervise",
            stops = self.stops.len(),
            window_ms = window.as_millis() as u64,
            "boot sweep stops scheduled behind one undo window"
        );

        let stops = self.stops;
        spawn_task(async move {
            tokio::time::sleep(window).await;
            // Same two guards as a single kill: the entry must still be in the
            // table (an undo removes it under the lock) *and* not flagged.
            let still_armed = {
                let mut table = undo_table().lock();
                match table.remove(&token) {
                    Some(entry) => !entry.cancelled.load(Ordering::SeqCst),
                    None => false,
                }
            };
            if !still_armed || cancelled.load(Ordering::SeqCst) {
                return;
            }
            // The window ended: the dialog closes as the stops start.
            if let Some(o) = &observer {
                o.ended(&undo, true);
            }
            for (spawn_id, opts) in stops {
                let sup = sup.clone();
                spawn_task(async move {
                    let report = kill_tree(&sup, &spawn_id, opts).await;
                    tracing::info!(
                        target: "supervise",
                        spawn_id = %spawn_id,
                        requested = report.requested.len(),
                        survivors = report.survivors.len(),
                        refused = ?report.refused_reason,
                        "grouped scheduled termination fired"
                    );
                    resolve_pending(&spawn_id);
                    emit_registry_change(&sup, std::slice::from_ref(&spawn_id));
                });
            }
        });
        Some(recovery)
    }
}

/// Spawn `task` on whichever runtime is available (an ambient tokio one during
/// commands and tests, Tauri's global runtime during boot).
fn spawn_task<F>(task: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn(task);
        }
        Err(_) => {
            tauri::async_runtime::spawn(task);
        }
    }
}

/// Arm an automatic termination behind an undo window and return its token.
///
/// The kill itself happens when the window closes, so an undo that arrives in
/// time genuinely prevents a signal rather than apologising for one.
pub fn schedule_kill(
    sup: Arc<Supervisor>,
    spawn_id: impl Into<String>,
    window: Duration,
    opts: KillOpts,
) -> String {
    let spawn_id = spawn_id.into();
    // The contract's `UndoToken` shape (`u_<32 hex>`, §3.2).
    let token = crate::contract::ids::UndoToken::mint().into_string();
    let cancelled = Arc::new(AtomicBool::new(false));
    undo_table().lock().insert(
        token.clone(),
        ScheduledKill {
            targets: vec![spawn_id.clone()],
            group: false,
            cancelled: cancelled.clone(),
            on_cancel: None,
        },
    );

    let task_token = token.clone();
    spawn_task(async move {
        tokio::time::sleep(window).await;
        // Two guards, because the cancel can land between the sleep waking and
        // this line: the entry must still be in the table *and* not flagged.
        let still_armed = {
            let mut table = undo_table().lock();
            match table.remove(&task_token) {
                Some(entry) => !entry.cancelled.load(Ordering::SeqCst),
                None => false,
            }
        };
        if !still_armed || cancelled.load(Ordering::SeqCst) {
            return;
        }
        let report = kill_tree(&sup, &spawn_id, opts).await;
        tracing::info!(
            target: "supervise",
            spawn_id = %spawn_id,
            requested = report.requested.len(),
            survivors = report.survivors.len(),
            refused = ?report.refused_reason,
            "scheduled termination fired"
        );
        resolve_pending(&spawn_id);
        emit_registry_change(&sup, std::slice::from_ref(&spawn_id));
    });

    token
}

// ---------------------------------------------------------------------------
// small helpers
// ---------------------------------------------------------------------------

fn emit_registry_change(sup: &Supervisor, changed: &[String]) {
    if changed.is_empty() {
        return;
    }
    if let Some(bus) = sup.events() {
        bus.emit(
            PROC_REGISTRY,
            ProcRegistryEvent {
                changed_spawn_ids: changed.to_vec(),
            },
        );
    }
}

fn emit_toast(sup: &Supervisor, toast: ToastEvent) {
    if let Some(bus) = sup.events() {
        bus.emit(events::TOAST, toast);
    }
}

/// `realpath` where possible; the raw path otherwise. `/tmp` and `/private/tmp`
/// are the same directory on macOS and a string comparison would say otherwise.
fn canonical(path: &str) -> PathBuf {
    let p = Path::new(path);
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

async fn mark_state(sup: &Supervisor, spawn_id: &str, state: ProcState, note: &str) {
    let id = spawn_id.to_string();
    let note = note.to_string();
    let result = sup
        .db()
        .write_durable(move |conn| {
            dao::proc::mark_state(conn, &id, state, Some(&note))?;
            Ok(())
        })
        .await;
    if let Err(err) = result {
        tracing::error!(
            target: "supervise",
            spawn_id,
            error = %err,
            "could not record a reaper state transition"
        );
    }
}

/// The row's process is gone: `exited` with an `exited_at`, plus the note that
/// says the reaper — not the owner — closed it.
async fn mark_reaped(sup: &Supervisor, spawn_id: &str) {
    let id = spawn_id.to_string();
    let at = now_ms();
    let result = sup
        .db()
        .write_durable(move |conn| {
            let tx = conn.unchecked_transaction()?;
            dao::proc::mark_exited(&tx, &id, None, None, at)?;
            dao::proc::mark_state(&tx, &id, ProcState::Exited, Some(note::REAPED))?;
            tx.commit()?;
            Ok(())
        })
        .await;
    if let Err(err) = result {
        tracing::error!(target: "supervise", spawn_id, error = %err, "could not close a reaped row");
    }
}

/// Take a live row over into this instance.
async fn adopt_row(
    sup: &Supervisor,
    spawn_id: &str,
    pid: i32,
    pgid: Option<i32>,
    proc_start_ts: Option<i64>,
    note: &str,
) {
    let id = spawn_id.to_string();
    let instance = sup.app_instance_id().to_string();
    let note = note.to_string();
    let result = sup
        .db()
        .write_durable(move |conn| {
            dao::proc::adopt(
                conn,
                &id,
                &instance,
                pid as i64,
                pgid.map(i64::from),
                proc_start_ts,
                Some(&note),
            )?;
            Ok(())
        })
        .await;
    if let Err(err) = result {
        tracing::error!(target: "supervise", spawn_id, error = %err, "could not adopt a row");
    }
}

async fn set_keep_alive(sup: &Supervisor, spawn_id: &str, keep: bool) {
    let id = spawn_id.to_string();
    let result = sup
        .db()
        .write_durable(move |conn| {
            dao::proc::set_keep_alive(conn, &id, keep)?;
            Ok(())
        })
        .await;
    if let Err(err) = result {
        tracing::error!(target: "supervise", spawn_id, error = %err, "could not set keepAlive");
    }
}

/// How long this *run* has been going, in seconds.
///
/// The registry's own `started_at` leads, because that is the moment the work
/// began and it is what `maxHeadlessLifetimeMinutes` is a deadline on. The
/// kernel's `etime` is the fallback for a row we adopted rather than started
/// (it knows the process's age, but not when the run it belongs to began).
fn age_secs(row: &ProcRow, snap: &Snapshot, now: i64) -> u64 {
    if let Some(started) = row.started_at {
        return ((now - started).max(0) / 1000) as u64;
    }
    if let Some(ps) = row.pid.and_then(|p| snap.get(p as i32)) {
        return ps.elapsed_secs;
    }
    ((now - row.planned_at).max(0) / 1000) as u64
}

fn rss_bytes_of(row: &ProcRow, snap: &Snapshot) -> u64 {
    row.pid
        .and_then(|p| snap.get(p as i32))
        .map(PsRow::rss_bytes)
        .unwrap_or(0)
}

impl RecoveredProc {
    /// Project a registry row into a recovery-sheet row.
    ///
    /// `cwd` comes from the registry column, never from `libproc`: the sheet
    /// wants "where we started it", and a syscall per row is exactly what
    /// DESIGN §8-5 tells the reaper not to do.
    pub fn of(row: &ProcRow, snap: &Snapshot, now: i64) -> RecoveredProc {
        let joined = row.argv_vec().join(" ");
        RecoveredProc {
            spawn_id: row.spawn_id.clone(),
            kind: row.kind,
            vendor: row.vendor.clone(),
            pid: row.pid.map(|p| p as i32),
            argv_masked: procscan::argv_display(&joined),
            cwd: row.cwd.clone(),
            worktree: row.worktree_id.clone(),
            rss_bytes: rss_bytes_of(row, snap),
            age_secs: age_secs(row, snap, now),
            keep_alive: row.keep_alive,
            suggested: RecoveryAction::Terminate,
        }
    }
}

// ---------------------------------------------------------------------------
// 1–5. the boot sweep
// ---------------------------------------------------------------------------

/// The boot sweep (DESIGN §8-5). Runs before the window exists.
///
/// Takes an `Arc` because an automatic termination outlives the call: the kill
/// is scheduled behind the undo window and needs an owned handle. Every call
/// site passes `&supervisor` and reads identically.
pub async fn sweep_startup(sup: &Arc<Supervisor>) -> SweepReport {
    sweep_startup_with(sup, SweepOpts::default()).await
}

/// [`sweep_startup`] with the undo window and kill grace made explicit. The
/// session stops it schedules form one group under one undo token, armed
/// here ([`SweepReport::recovery`]).
pub async fn sweep_startup_with(sup: &Arc<Supervisor>, opts: SweepOpts) -> SweepReport {
    let mut group = StopGroup::new();
    let mut report = sweep_startup_into(sup, opts, &mut group).await;
    report.recovery = group.arm(sup.clone(), opts.undo_window, None);
    report
}

/// The boot sweep, adding every session stop it schedules to `group`
/// instead of arming it (A29.3: the caller adds the quit survivors and arms
/// one window for all). The headless-lifetime kill is armed here, with its
/// own toast and token, as before.
pub async fn sweep_startup_into(sup: &Arc<Supervisor>, opts: SweepOpts, group: &mut StopGroup) -> SweepReport {
    let mut report = SweepReport::default();
    let me = sup.app_instance_id().to_string();
    let current_boot = sup.current_boot_id().to_string();

    let instances = match sup.db().read(dao::app_instance::list_open).await {
        Ok(rows) => rows,
        Err(err) => {
            // A sweep that cannot read the registry reports nothing and lets the
            // app boot. It must never be the reason the window does not open.
            tracing::error!(target: "supervise", error = %err, "boot sweep could not read app_instance");
            publish(sup, &report);
            return report;
        }
    };

    // ---- step 2: classify the instances that never closed.
    let mut crashed: Vec<AppInstanceRow> = Vec::new();
    for inst in instances {
        if inst.id == me {
            continue;
        }
        if inst.boot_id != current_boot {
            // ★ The reboot branch. Every pid this instance recorded belongs to a
            //   previous boot session and means nothing now, so the rows are
            //   closed without a fence check and without a signal.
            let id = inst.id.clone();
            let closed = sup
                .db()
                .write_durable(move |conn| {
                    let n = dao::proc::mark_live_rows_of_instance(
                        conn,
                        &id,
                        ProcState::Lost,
                        note::REBOOT,
                    )?;
                    dao::app_instance::close(conn, &id, now_ms())?;
                    Ok(n)
                })
                .await;
            match closed {
                Ok(n) => {
                    report.reboot_lost += n as u32;
                    tracing::info!(
                        target: "supervise",
                        app_instance = %inst.id,
                        boot_id = %inst.boot_id,
                        rows = n,
                        "rows from an earlier boot marked lost; no fences, no signals"
                    );
                }
                Err(err) => tracing::error!(
                    target: "supervise",
                    app_instance = %inst.id,
                    error = %err,
                    "could not close an instance from an earlier boot"
                ),
            }
            continue;
        }

        if is_live_sibling(&inst) {
            // A second copy of the app is running. Its rows are *its* business;
            // the single-instance lock (T29) decides what happens to this one.
            tracing::warn!(
                target: "supervise",
                app_instance = %inst.id,
                pid = inst.pid,
                "another instance of this app is alive; leaving its rows alone"
            );
            continue;
        }

        crashed.push(inst);
    }

    if crashed.is_empty() {
        publish(sup, &report);
        return report;
    }

    // Everything below needs to look at live processes. Without a snapshot we
    // cannot fence anything, and a row we cannot fence is a row we must not
    // touch — the next boot will try again.
    let snap = match ps_snapshot() {
        Ok(snap) => snap,
        Err(err) => {
            tracing::error!(
                target: "supervise",
                error = %err,
                "boot sweep has no process snapshot; crashed instances are left for the next boot"
            );
            publish(sup, &report);
            return report;
        }
    };
    let now = now_ms();

    // ---- steps 3 and 4, per crashed instance.
    let mut candidates: Vec<ProcRow> = Vec::new();
    // A41.3: processes found only by a dead instance's marker. Listed, never
    // stopped automatically.
    let mut orphans: Vec<ProcRow> = Vec::new();
    // Rows that belong to a Gilde session (`parent_uid`, contract §3.8).
    let mut with_parent: HashMap<String, String> = HashMap::new();
    for inst in &crashed {
        let instance_id = inst.id.clone();
        let rows = match sup
            .db()
            .read(move |conn| dao::proc::rows_of_instance(conn, &instance_id))
            .await
        {
            Ok(rows) => rows,
            Err(err) => {
                tracing::error!(
                    target: "supervise",
                    app_instance = %inst.id,
                    error = %err,
                    "could not read the rows of a crashed instance"
                );
                continue;
            }
        };

        let instance_id = inst.id.clone();
        if let Ok(parents) = sup
            .db()
            .read(move |conn| dao::proc::parents_of_instance(conn, &instance_id))
            .await
        {
            with_parent.extend(parents);
        }

        for row in rows.into_iter().filter(|r| r.state.is_live()) {
            match row.state {
                ProcState::Planned => {
                    match adopt_planned_row(sup, &row, &snap).await {
                        PlannedOutcome::Adopted(adopted) => {
                            report.adopted += 1;
                            candidates.push(adopted);
                        }
                        PlannedOutcome::Lost => report.lost += 1,
                    }
                }
                // `adopted` is treated exactly like `running`: it is a live row
                // whose owning instance has crashed, and leaving it alone would
                // make "was adopted once" a permanent hiding place.
                ProcState::Running | ProcState::Adopted => match verify(&row, &snap) {
                    Fence::Gone => {
                        mark_reaped(sup, &row.spawn_id).await;
                        report.exited += 1;
                    }
                    Fence::Mismatch(reason) => {
                        tracing::warn!(
                            target: "supervise",
                            spawn_id = %row.spawn_id,
                            pid = ?row.pid,
                            reason = reason.as_str(),
                            "identity fence refused a row; it will not be signalled"
                        );
                        mark_state(sup, &row.spawn_id, ProcState::Lost, reason.as_str()).await;
                        report.lost += 1;
                    }
                    Fence::Verified => candidates.push(row),
                },
                // Unreachable: `is_live()` admits exactly the three above.
                _ => {}
            }
        }

        // ---- step 4: untracked orphans of this dead instance (best-effort).
        orphans.extend(untracked_orphans(sup, &inst.id, &snap).await);
    }

    // ---- the policy for verified live orphans (DESIGN §8-5).
    let kill_opts = KillOpts::default().grace(opts.kill_grace);
    let max_headless_secs =
        u64::from(sup.config().get().supervision.max_headless_lifetime_minutes) * 60;

    for row in candidates {
        let age = age_secs(&row, &snap, now);

        if row.keep_alive {
            // Deliberately outlives the app. Adopted, badged "detached", never
            // quietly killed.
            adopt_row(
                sup,
                &row.spawn_id,
                row.pid.unwrap_or(0) as i32,
                row.pgid.map(|p| p as i32),
                row.proc_start_ts,
                note::DETACHED,
            )
            .await;
            report.adopted += 1;
            continue;
        }

        if row.kind == ProcKind::Headless && max_headless_secs > 0 && age > max_headless_secs {
            // Every legitimate deadline has demonstrably passed. Auto-terminate,
            // but behind a window the user can close.
            let dry = kill_tree(sup, &row.spawn_id, kill_opts.dry_run(true)).await;
            let token = schedule_kill(
                sup.clone(),
                row.spawn_id.clone(),
                opts.undo_window,
                kill_opts,
            );
            mark_state(
                sup,
                &row.spawn_id,
                row.state,
                note::HEADLESS_LIFETIME,
            )
            .await;
            emit_toast(
                sup,
                ToastEvent {
                    level: ToastLevel::Warn,
                    key: toast_key::HEADLESS_TERMINATED.to_string(),
                    params: {
                        let mut m = serde_json::Map::new();
                        m.insert(
                            "name".into(),
                            serde_json::Value::from(
                                row.vendor.clone().unwrap_or_else(|| row.kind.to_string()),
                            ),
                        );
                        m.insert("minutes".into(), serde_json::Value::from(age / 60));
                        m
                    },
                    action_id: None,
                    undo_token: Some(token),
                },
            );
            report.auto_terminated.push(dry);
            continue;
        }

        let is_session = row.session_id.as_deref().is_some_and(|s| !s.is_empty())
            || with_parent.contains_key(&row.spawn_id);
        if row.kind.is_session_kind() && is_session {
            // Contract §10.3: a crashed instance's Conversation child has lost
            // its stdin, a PTY child its master; neither is adopted as live.
            // Fenced SIGTERM (SIGHUP first for terminals) after the undo
            // window; a vendor CLI is never escalated to SIGKILL, and one that
            // survives becomes a `GildeCrashLeftover` for the Clean-up sheet.
            let hup = matches!(row.kind, ProcKind::Pty | ProcKind::Pane | ProcKind::Attach);
            let kopts = kill_opts.hup_first(hup).grace(Duration::from_millis(1_500));
            let dry = kill_tree(sup, &row.spawn_id, kopts.dry_run(true)).await;
            // A29.3: one group, one token — armed by the caller.
            group.push(row.spawn_id.clone(), kopts);
            mark_state(sup, &row.spawn_id, row.state, note::CRASH_LEFTOVER).await;
            note_crash_leftover(&row, with_parent.get(&row.spawn_id));
            report.auto_terminated.push(dry);
            continue;
        }

        note_crash_leftover(&row, with_parent.get(&row.spawn_id));
        report.pending.push(RecoveredProc::of(&row, &snap, now));
    }

    // A41.3: an orphan found only by the dead instance's marker is a Leftover
    // `GildeCrashLeftover` (the Recovered sheet, then Clean-up) — never a
    // stop of its own, not even past the headless lifetime: Gilde did not
    // start it as anything it can name.
    for row in orphans {
        note_crash_leftover(&row, None);
        report.pending.push(RecoveredProc::of(&row, &snap, now));
    }

    // A29.3: a row the sweep neither stops nor adopts is never dropped
    // silently — it is a `GildeCrashLeftover` for the Clean-up sheet (A43.1:
    // from the first scan, `note_crash_leftover` above).

    // ---- step 5: close the crashed instances.
    for inst in &crashed {
        let id = inst.id.clone();
        let at = now_ms();
        if let Err(err) = sup
            .db()
            .write_durable(move |conn| {
                dao::app_instance::close(conn, &id, at)?;
                Ok(())
            })
            .await
        {
            tracing::error!(
                target: "supervise",
                app_instance = %inst.id,
                error = %err,
                "could not close a crashed instance"
            );
        }
    }

    tracing::info!(
        target: "supervise",
        reboot_lost = report.reboot_lost,
        adopted = report.adopted,
        exited = report.exited,
        lost = report.lost,
        auto_terminated = report.auto_terminated.len(),
        pending = report.pending.len(),
        "boot sweep complete"
    );

    publish(sup, &report);
    report
}

static CRASH_LEFTOVERS: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
static QUIT_SURVIVORS: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();

fn leftover_set(
    cell: &'static OnceLock<Mutex<std::collections::HashSet<String>>>,
) -> &'static Mutex<std::collections::HashSet<String>> {
    cell.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

/// Spawn ids of session processes a crashed instance left behind (this run's
/// boot sweep scheduled their termination; contract §10.3).
pub fn crash_leftover_spawn_ids() -> std::collections::HashSet<String> {
    leftover_set(&CRASH_LEFTOVERS).lock().clone()
}

/// A43.1: one crash leftover the boot sweep proved, by identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrashLeftover {
    pub spawn_id: String,
    pub pid: i32,
    pub start_sec: Option<i64>,
    /// The Gilde session that owned the row (`parent_uid`, else `session_id`).
    pub session_uid: Option<String>,
}

static CRASH_LEFTOVER_IDS: OnceLock<Mutex<HashMap<String, CrashLeftover>>> = OnceLock::new();

fn crash_leftover_ids() -> &'static Mutex<HashMap<String, CrashLeftover>> {
    CRASH_LEFTOVER_IDS.get_or_init(Default::default)
}

/// Record a row the boot sweep proved to be a crash leftover (A29.3, A43.1).
fn note_crash_leftover(row: &ProcRow, parent_uid: Option<&String>) {
    leftover_set(&CRASH_LEFTOVERS).lock().insert(row.spawn_id.clone());
    let Some(pid) = row.pid.map(|p| p as i32).filter(|p| *p > 0) else { return };
    let session_uid = parent_uid.cloned().or_else(|| row.session_id.clone()).filter(|s| !s.is_empty());
    record_crash_leftover(CrashLeftover { spawn_id: row.spawn_id.clone(), pid, start_sec: row.proc_start_ts, session_uid });
}

/// Record a proven crash leftover (the boot sweep; tests that model a sweep
/// that already ran).
pub fn record_crash_leftover(leftover: CrashLeftover) {
    leftover_set(&CRASH_LEFTOVERS).lock().insert(leftover.spawn_id.clone());
    crash_leftover_ids().lock().insert(leftover.spawn_id.clone(), leftover);
}

/// A43.1: whether `(pid, start_sec)` is a crash leftover the boot sweep proved
/// for the Gilde session `uid` — known from the sweep at once, before any
/// health scan published it.
pub fn is_own_crash_leftover(uid: &str, pid: i32, start_sec: i64) -> bool {
    crash_leftover_ids().lock().values().any(|c| {
        c.pid == pid
            && c.session_uid.as_deref() == Some(uid)
            && c.start_sec.is_none_or(|s| (s - start_sec).abs() <= 1)
    })
}

/// Spawn ids of the previous instance's quit survivors (§10.2 step 6).
pub fn quit_survivor_spawn_ids() -> std::collections::HashSet<String> {
    leftover_set(&QUIT_SURVIVORS).lock().clone()
}

static LEAD_GONE_SURVIVORS: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();

/// A40.3: record a hosted pane whose processes survived its lead's exit
/// (PaneHost `lead_gone`, HUP then TERM, never KILL). The health scan shows
/// its row as Leftover `TeammateOfGoneLead` instead of a live process of this
/// instance.
pub fn note_lead_gone_survivor(spawn_id: &str) {
    leftover_set(&LEAD_GONE_SURVIVORS).lock().insert(spawn_id.to_string());
}

/// Spawn ids recorded by [`note_lead_gone_survivor`] this run.
pub fn lead_gone_survivor_spawn_ids() -> std::collections::HashSet<String> {
    leftover_set(&LEAD_GONE_SURVIVORS).lock().clone()
}

/// The counters of a [`SweepReport`], for logs and the boot report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SweepCounts {
    pub reboot_lost: u32,
    pub adopted: u32,
    pub exited: u32,
    pub lost: u32,
    pub auto_terminated: u32,
    pub pending: u32,
}

impl SweepCounts {
    pub fn of(report: &SweepReport) -> SweepCounts {
        SweepCounts {
            reboot_lost: report.reboot_lost,
            adopted: report.adopted,
            exited: report.exited,
            lost: report.lost,
            auto_terminated: report.auto_terminated.len() as u32,
            pending: report.pending.len() as u32,
        }
    }
}

/// Contract §10.2 step 6: the vendor processes an earlier instance recorded as
/// quit survivors (rows left `running` with [`note::QUIT_SURVIVOR`] under an
/// ended instance) are identity-fenced; a live one gets a fenced SIGTERM after
/// the undo window — never SIGKILL — and one that still survives stays visible
/// as Leftover `QuitSurvivor`. Returns how many terminations were scheduled
/// (one group under one undo token, armed here).
pub async fn sweep_quit_survivors(sup: &Arc<Supervisor>, window: Duration) -> u32 {
    let mut group = StopGroup::new();
    let n = sweep_quit_survivors_into(sup, &mut group).await;
    group.arm(sup.clone(), window, None);
    n
}

/// [`sweep_quit_survivors`], adding each stop to `group` instead of arming
/// it (A29.3: the boot sweep arms one window for every stop it schedules).
pub async fn sweep_quit_survivors_into(sup: &Arc<Supervisor>, group: &mut StopGroup) -> u32 {
    let me = sup.app_instance_id().to_string();
    let rows = match sup
        .db()
        .read(move |conn| {
            let mut out = Vec::new();
            for inst in dao::app_instance::list_all(conn)? {
                if inst.id == me || inst.ended_at.is_none() {
                    continue;
                }
                for row in dao::proc::rows_of_instance(conn, &inst.id)? {
                    if row.state.is_live() && row.note.as_deref() == Some(note::QUIT_SURVIVOR) {
                        out.push(row);
                    }
                }
            }
            Ok(out)
        })
        .await
    {
        Ok(rows) => rows,
        Err(err) => {
            tracing::warn!(target: "supervise", error = %err, "could not read quit survivors");
            return 0;
        }
    };
    if rows.is_empty() {
        return 0;
    }
    let Ok(snap) = ps_snapshot() else {
        return 0;
    };
    let mut scheduled = 0u32;
    for row in rows {
        match verify(&row, &snap) {
            Fence::Gone => mark_reaped(sup, &row.spawn_id).await,
            Fence::Mismatch(reason) => {
                mark_state(sup, &row.spawn_id, ProcState::Lost, reason.as_str()).await
            }
            Fence::Verified => {
                let hup = matches!(row.kind, ProcKind::Pty | ProcKind::Pane | ProcKind::Attach);
                let opts = KillOpts::default()
                    .grace(Duration::from_millis(1_500))
                    .hup_first(hup);
                group.push(row.spawn_id.clone(), opts);
                leftover_set(&QUIT_SURVIVORS).lock().insert(row.spawn_id.clone());
                scheduled += 1;
            }
        }
    }
    scheduled
}

/// Store the pending set and announce it. Always runs, even for an empty sweep:
/// a `count: 0` is what tells the frontend it may stop showing the sheet.
fn publish(sup: &Supervisor, report: &SweepReport) {
    set_pending(report.pending.clone());
    if let Some(bus) = sup.events() {
        bus.emit(
            RECOVERY_PENDING,
            RecoveryPendingEvent {
                count: report.pending_count(),
            },
        );
    }
}

/// Whether `inst` is a second copy of this app that is running right now.
///
/// Errs towards "yes": a live pid we cannot identify is left alone, which costs
/// one more boot before its rows are cleaned up, whereas guessing "no" would let
/// us reap the rows of a running sibling and kill its children.
fn is_live_sibling(inst: &AppInstanceRow) -> bool {
    let pid = inst.pid as i32;
    if pid <= 1 || pid == std::process::id() as i32 {
        // Our own pid on somebody else's row means the number was recycled: the
        // instance that wrote it is long gone.
        return false;
    }
    if !is_alive(pid) {
        return false;
    }
    match (procscan::exe_path_of(pid), std::env::current_exe().ok()) {
        (Some(theirs), Some(ours)) => {
            let theirs = canonical(&theirs);
            theirs == canonical(&ours.to_string_lossy())
        }
        // Unreadable either way — assume it is ours and keep our hands off it.
        _ => true,
    }
}

// ---------------------------------------------------------------------------
// 3. planned-row adoption
// ---------------------------------------------------------------------------

#[allow(clippy::large_enum_variant)]
enum PlannedOutcome {
    Adopted(ProcRow),
    Lost,
}

/// Close the one window in which a process of ours can exist without a recorded
/// pid: between `plan_spawn`'s INSERT and `mark_running` (DESIGN §8-3).
///
/// A candidate must match **all** of `argv_sig`, `cwd` and
/// `start_time >= planned_at`, plus the `GILDE_SPAWN_ID` marker when the
/// environment block can be read at all. Exactly one match adopts; zero or two
/// or more is `lost` — an ambiguous row is never signalled, because the whole
/// point of the fences is that we would not know which of the two we hit.
async fn adopt_planned_row(sup: &Supervisor, row: &ProcRow, snap: &Snapshot) -> PlannedOutcome {
    let matches = planned_row_candidates(row, snap);

    match matches.as_slice() {
        [pid] => {
            let pid = *pid;
            let start = procscan::start_time_of(pid);
            adopt_row(
                sup,
                &row.spawn_id,
                pid,
                pgid_of(pid),
                start,
                note::PLANNED_ADOPTED,
            )
            .await;
            tracing::info!(
                target: "supervise",
                spawn_id = %row.spawn_id,
                pid,
                "planned row adopted"
            );
            let mut adopted = row.clone();
            adopted.state = ProcState::Adopted;
            adopted.pid = Some(pid as i64);
            adopted.pgid = pgid_of(pid).map(i64::from);
            adopted.proc_start_ts = start;
            adopted.app_instance = sup.app_instance_id().to_string();
            PlannedOutcome::Adopted(adopted)
        }
        [] => {
            mark_state(sup, &row.spawn_id, ProcState::Lost, note::NO_CANDIDATE).await;
            PlannedOutcome::Lost
        }
        many => {
            tracing::warn!(
                target: "supervise",
                spawn_id = %row.spawn_id,
                candidates = many.len(),
                "ambiguous planned-row candidates; marking lost and signalling nothing"
            );
            mark_state(sup, &row.spawn_id, ProcState::Lost, note::AMBIGUOUS).await;
            PlannedOutcome::Lost
        }
    }
}

/// Live pids that match a `planned` row on every fence DESIGN §8-5 lists.
///
/// Exposed (`pub`) so the ambiguity rule can be tested without a database.
pub fn planned_row_candidates(row: &ProcRow, snap: &Snapshot) -> Vec<i32> {
    let argv = row.argv_vec();
    let Some(argv0) = argv.first().cloned() else {
        return Vec::new();
    };
    let want_cwd = canonical(&row.cwd);
    // `planned_at` is millis, kernel start times are whole seconds.
    let floor = row.planned_at / 1000;
    let me = std::process::id() as i32;
    let ancestors = snap.ancestors_of(me);

    let mut out: Vec<i32> = Vec::new();
    for ps in snap.rows() {
        if ps.pid <= 1 || ps.pid == me || ancestors.contains(&ps.pid) {
            continue;
        }
        // Cheap pre-filter on the snapshot's own command line, so the expensive
        // per-pid probes below run for a handful of processes, not 800.
        if !ps.args.starts_with(argv0.as_str()) {
            continue;
        }
        let Some(start) = procscan::start_time_of(ps.pid) else {
            continue;
        };
        if start < floor {
            continue;
        }
        let Some(live) = live_argv(ps.pid) else {
            continue;
        };
        if argv_sig_of(&live) != row.argv_sig {
            continue;
        }
        match procscan::cwd_of(ps.pid) {
            Some(cwd) => {
                if canonical(&cwd.to_string_lossy()) != want_cwd {
                    continue;
                }
            }
            // No cwd, no match: it is one of three required fences, not a bonus.
            None => continue,
        }
        // The env marker is checked only when the block is readable at all.
        // DESIGN §8-4 documents an unreadable environment as a normal path.
        if let Some(env) = procscan::environ_of(ps.pid) {
            if !env.is_empty() {
                let needle = format!("{ENV_SPAWN_ID}={}", row.env_marker);
                if !env.iter().any(|e| e == &needle) {
                    continue;
                }
            }
        }
        out.push(ps.pid);
    }
    out.sort_unstable();
    out.dedup();
    out
}

// ---------------------------------------------------------------------------
// 4. untracked orphans
// ---------------------------------------------------------------------------

/// Live processes stamped with a dead instance's `GILDE_APP_INSTANCE` and claimed
/// by no live registry row — the net under the sidecar's `host.spawned`.
///
/// **Best-effort by construction.** It depends entirely on reading another
/// process's environment block, which macOS refuses often enough that DESIGN
/// §8-4 tells us to document it rather than rely on it. An empty result is the
/// normal case, not a failure.
///
/// A discovered process is *materialised* into the registry (a planned row that
/// is immediately marked running against the live pid) so that everything
/// downstream — the fences, `kill_tree`, the monitor — treats it like any other
/// row instead of needing a second, weaker code path.
async fn untracked_orphans(sup: &Supervisor, dead_instance: &str, snap: &Snapshot) -> Vec<ProcRow> {
    let pids = snap.with_env(ENV_APP_INSTANCE, dead_instance);
    if pids.is_empty() {
        return Vec::new();
    }
    let me = std::process::id() as i32;
    let ancestors = snap.ancestors_of(me);
    let mut out = Vec::new();

    for pid in pids {
        if pid <= 1 || pid == me || ancestors.contains(&pid) {
            continue;
        }
        // Already covered by a live row (its own, or one adopted a moment ago).
        match sup.db().read(move |conn| dao::proc::by_pid(conn, pid as i64)).await {
            Ok(rows) if !rows.is_empty() => continue,
            Ok(_) => {}
            Err(err) => {
                tracing::warn!(target: "supervise", pid, error = %err, "could not check a pid against the registry");
                continue;
            }
        }
        let Some(argv) = live_argv(pid) else { continue };
        let Some(cwd) = procscan::cwd_of(pid) else { continue };

        let plan = SpawnPlan::new(ProcKind::Headless, argv, cwd);
        // Already running: registered, not launched (no allowlist check).
        let guard = match sup.plan_existing(plan).await {
            Ok(guard) => guard,
            Err(err) => {
                tracing::warn!(target: "supervise", pid, error = %err, "could not register an untracked orphan");
                continue;
            }
        };
        let spawn_id = guard.spawn_id().to_string();
        if let Err(err) = guard.mark_running(pid, pgid_of(pid).unwrap_or(0)) {
            tracing::warn!(target: "supervise", pid, error = %err, "could not record an untracked orphan");
            continue;
        }
        mark_state(sup, &spawn_id, ProcState::Running, note::UNTRACKED).await;

        let id = spawn_id.clone();
        match sup.db().read(move |conn| dao::proc::get(conn, &id)).await {
            Ok(Some(row)) => {
                tracing::warn!(
                    target: "supervise",
                    pid,
                    spawn_id = %spawn_id,
                    dead_instance,
                    "adopted an untracked orphan of a dead instance"
                );
                out.push(row);
            }
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(target: "supervise", pid, error = %err, "could not read back an orphan row")
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// generation sweep
// ---------------------------------------------------------------------------

/// Fence-and-kill every live row of a dead sidecar generation (DESIGN §8-5).
///
/// `keep_alive` rows are left alone — they are the ones the user asked to
/// outlive us. Runs **before the replacement sidecar can take work**, which is
/// what stops a crash loop from stacking orphans; `SidecarManager` awaits it
/// through [`ReaperSweeper`].
pub async fn sweep_generation(sup: &Supervisor, dead_gen: i64) -> Vec<KillReport> {
    let rows = match sup
        .db()
        .read(move |conn| dao::proc::live_rows_of_generation(conn, dead_gen))
        .await
    {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(
                target: "supervise",
                generation = dead_gen,
                error = %err,
                "generation sweep could not read the registry"
            );
            return Vec::new();
        }
    };

    let opts = KillOpts::from_config(&sup.config().get().supervision);
    let mut reports = Vec::new();
    let mut changed = Vec::new();

    for row in rows {
        if row.keep_alive {
            tracing::info!(
                target: "supervise",
                spawn_id = %row.spawn_id,
                generation = dead_gen,
                "keepAlive row survives its generation"
            );
            continue;
        }
        if row.state == ProcState::Planned {
            // No pid was ever recorded and the sidecar that would have recorded
            // one is dead. There is nothing to fence, so there is nothing to
            // signal — the row is closed, not killed.
            mark_state(sup, &row.spawn_id, ProcState::Lost, note::GENERATION).await;
            changed.push(row.spawn_id.clone());
            continue;
        }
        let report = kill_tree(sup, &row.spawn_id, opts).await;
        changed.push(row.spawn_id.clone());
        reports.push(report);
    }

    tracing::info!(
        target: "supervise",
        generation = dead_gen,
        killed = reports.len(),
        "generation sweep complete"
    );
    emit_registry_change(sup, &changed);
    reports
}

/// The [`crate::sidecar::GenerationSweeper`] the sidecar manager awaits before
/// letting a replacement take work.
pub struct ReaperSweeper {
    sup: Arc<Supervisor>,
}

impl ReaperSweeper {
    pub fn new(sup: Arc<Supervisor>) -> Arc<ReaperSweeper> {
        Arc::new(ReaperSweeper { sup })
    }
}

impl crate::sidecar::GenerationSweeper for ReaperSweeper {
    fn sweep(
        &self,
        dead_generation: i64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        let sup = self.sup.clone();
        Box::pin(async move {
            sweep_generation(&sup, dead_generation).await;
        })
    }
}

// ---------------------------------------------------------------------------
// periodic reconcile
// ---------------------------------------------------------------------------

/// Liveness, RSS and idle for every live registry row (DESIGN §8-5).
///
/// One `ps` snapshot, no `cwd` lookups, and `gilde://proc/registry` only when
/// something actually changed — a reconcile that fires every 30 seconds must
/// cost nothing when nothing is happening.
pub async fn reconcile(sup: &Supervisor) -> ReconcileReport {
    let mut report = ReconcileReport {
        at: now_ms(),
        ..ReconcileReport::default()
    };

    let rows = match sup.db().read(|conn| dao::proc::live_rows(conn, None)).await {
        Ok(rows) => rows,
        Err(err) => {
            tracing::warn!(target: "supervise", error = %err, "reconcile could not read the registry");
            return report;
        }
    };
    if rows.is_empty() {
        return report;
    }
    let snap = match ps_snapshot() {
        Ok(snap) => snap,
        Err(err) => {
            tracing::warn!(target: "supervise", error = %err, "reconcile has no process snapshot");
            return report;
        }
    };

    for row in rows {
        let Some(pid) = row.pid.map(|p| p as i32).filter(|p| *p > 0) else {
            continue;
        };
        report.checked += 1;

        match verify(&row, &snap) {
            Fence::Gone => {
                mark_reaped(sup, &row.spawn_id).await;
                report.exited += 1;
                report.changed.push(row.spawn_id.clone());
                report.rows.push(ReconciledRow {
                    spawn_id: row.spawn_id.clone(),
                    pid,
                    alive: false,
                    rss_bytes: 0,
                    idle_secs: 0,
                });
            }
            Fence::Mismatch(reason) => {
                // The pid is alive but is not the process we recorded. Closing
                // the row is the whole point: a mismatched row must never keep
                // a recycled pid in the "ours" set.
                //
                // The one exception is a matter of log level, not of policy:
                // a `--version` probe that finished while this very reconcile
                // was running is refused for a reason nobody can act on
                // (BACKLOG M1.5 #5, see `mismatch_is_a_finished_probe`).
                if mismatch_is_a_finished_probe(&row, reason, is_alive(pid)) {
                    tracing::debug!(
                        target: "supervise",
                        spawn_id = %row.spawn_id,
                        pid,
                        reason = reason.as_str(),
                        "reconcile: a version probe finished mid-sweep; its row is closed unverified"
                    );
                } else {
                    tracing::warn!(
                        target: "supervise",
                        spawn_id = %row.spawn_id,
                        pid,
                        reason = reason.as_str(),
                        "reconcile: identity fence refused a live row"
                    );
                }
                mark_state(sup, &row.spawn_id, ProcState::Lost, reason.as_str()).await;
                report.changed.push(row.spawn_id.clone());
                report.rows.push(ReconciledRow {
                    spawn_id: row.spawn_id.clone(),
                    pid,
                    alive: false,
                    rss_bytes: 0,
                    idle_secs: 0,
                });
            }
            Fence::Verified => {
                let rss = rss_bytes_of(&row, &snap);
                report.rss_bytes = report.rss_bytes.saturating_add(rss);
                report.rows.push(ReconciledRow {
                    spawn_id: row.spawn_id.clone(),
                    pid,
                    alive: true,
                    rss_bytes: rss,
                    idle_secs: idle_secs(sup, &row, report.at).await,
                });
            }
        }
    }

    emit_registry_change(sup, &report.changed);
    report
}

/// The one identity mismatch that is not worth a WARN (BACKLOG M1.5 #5).
///
/// Every boot logged `reconcile: identity fence refused a live row … reason=argv`.
/// The row was always the same shape — `kind = 'probe'`, the `<agent> --version`
/// detection spawn (T09) or the sidecar runtime self-test — and the registry
/// showed it exiting within ~100 ms of being started, milliseconds before or
/// after the warning:
///
/// ```text
/// probe | codex | 16201 | argv | ["…/bin/codex","--version"] | started 765.224 | exited 765.307
/// WARN  reconcile: identity fence refused a live row  pid=16201 reason=argv  @ 765.321
/// ```
///
/// [`reconcile`] takes **one** `ps` snapshot and then fences every row against
/// it, so a process this short-lived can be in the snapshot and gone by the time
/// its own row is checked. `LiveProbe::is_alive` still answers "alive" from that
/// snapshot, so the row is not `Gone`; and with the process really gone,
/// `KERN_PROCARGS2` and the environment block are unreadable, so fence 2 and
/// fence 3 both abstain and [`verify_with`](crate::supervise::identity::verify)
/// fails closed as `argv`. That is the correct, conservative verdict — closing
/// the row is right, and no signal is ever sent on a mismatch — but it is a race
/// with our own exiting child, not a recycled pid, and there is nothing an
/// operator can do about it.
///
/// The predicate is deliberately narrow: only `argv` (the fail-closed reason),
/// only a `probe` row (the only kind that is short-lived by construction), and
/// only when the pid is **provably gone by now**. A genuinely recycled pid is
/// still alive on the re-check and still logs WARN.
fn mismatch_is_a_finished_probe(row: &ProcRow, reason: MismatchReason, alive_now: bool) -> bool {
    reason == MismatchReason::Argv && row.kind == ProcKind::Probe && !alive_now
}

/// Idle seconds for a row: real I/O for a PTY session, wall clock otherwise.
///
/// `last_io_at` is written by the PTY reader thread and by sidecar events
/// (DESIGN §8-8), so this is "nothing happened", not "the clock moved".
async fn idle_secs(sup: &Supervisor, row: &ProcRow, now: i64) -> u64 {
    let since = match &row.session_id {
        Some(id) => {
            let id = id.clone();
            match sup.db().read(move |conn| dao::pty_session::get(conn, &id)).await {
                Ok(Some(session)) => session.last_io_at.unwrap_or(session.opened_at),
                _ => row.started_at.unwrap_or(row.planned_at),
            }
        }
        None => row.started_at.unwrap_or(row.planned_at),
    };
    ((now - since).max(0) / 1000) as u64
}

/// Whichever runtime the reconcile ticker landed on. Two variants because the
/// two runtimes hand back two unrelated `JoinHandle` types.
enum LoopTask {
    Tokio(tokio::task::JoinHandle<()>),
    Tauri(tauri::async_runtime::JoinHandle<()>),
}

impl LoopTask {
    fn abort(&self) {
        match self {
            LoopTask::Tokio(handle) => handle.abort(),
            LoopTask::Tauri(handle) => handle.abort(),
        }
    }
}

/// The 30-second reconcile ticker plus the window-focus trigger.
pub struct ReconcileLoop {
    poke: Arc<tokio::sync::Notify>,
    handle: Mutex<Option<LoopTask>>,
}

impl ReconcileLoop {
    /// Run a reconcile now — what a window-focus event does.
    pub fn poke(&self) {
        self.poke.notify_one();
    }

    /// Stop the ticker. Idempotent.
    pub fn stop(&self) {
        if let Some(handle) = self.handle.lock().take() {
            handle.abort();
        }
    }
}

impl Drop for ReconcileLoop {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Start the periodic reconcile. Ticks every [`RECONCILE_INTERVAL`] and
/// whenever [`ReconcileLoop::poke`] is called.
pub fn spawn_reconcile_loop(sup: Arc<Supervisor>, interval: Duration) -> Arc<ReconcileLoop> {
    let poke = Arc::new(tokio::sync::Notify::new());
    let task_poke = poke.clone();
    let handle = match tokio::runtime::Handle::try_current() {
        Ok(rt) => LoopTask::Tokio(rt.spawn(reconcile_loop(sup, task_poke, interval))),
        Err(_) => LoopTask::Tauri(tauri::async_runtime::spawn(reconcile_loop(
            sup, task_poke, interval,
        ))),
    };
    Arc::new(ReconcileLoop {
        poke,
        handle: Mutex::new(Some(handle)),
    })
}

async fn reconcile_loop(sup: Arc<Supervisor>, poke: Arc<tokio::sync::Notify>, interval: Duration) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick completes immediately; the boot sweep just ran, so skip it.
    ticker.tick().await;
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = poke.notified() => {}
        }
        let report = reconcile(&sup).await;
        if !report.changed.is_empty() {
            tracing::debug!(
                target: "supervise",
                checked = report.checked,
                exited = report.exited,
                changed = report.changed.len(),
                "reconcile"
            );
        }
    }
}

/// Start the reconcile loop and wire the window-focus trigger to it.
///
/// The focus listener lives here rather than in `lib.rs` so that the whole of
/// DESIGN §8-5's "30초 주기 + 포커스" is one call at the boot site.
pub fn install_reconcile(app: &tauri::AppHandle, sup: Arc<Supervisor>) -> Arc<ReconcileLoop> {
    use tauri::Manager as _;

    let loop_handle = spawn_reconcile_loop(sup, RECONCILE_INTERVAL);
    if let Some(window) = app.get_webview_window("main") {
        let on_focus = loop_handle.clone();
        window.on_window_event(move |event| {
            if matches!(event, tauri::WindowEvent::Focused(true)) {
                on_focus.poke();
            }
        });
    } else {
        tracing::debug!(target: "supervise", "no main window yet; reconcile runs on its timer only");
    }
    loop_handle
}

// ---------------------------------------------------------------------------
// resolving the recovery sheet
// ---------------------------------------------------------------------------

/// Apply the user's decisions. One [`KillOutcome`] per decision, in order.
///
/// *Keep* and *Adopt* both leave the process running and both return an honest
/// "no signal was sent" report rather than a bare success. They differ in what
/// happens next: an adopted row is managed by this instance like any other,
/// while a kept row also gets `keep_alive = 1` so the next boot adopts it again
/// instead of asking a second time.
pub async fn resolve(sup: &Supervisor, decisions: &[RecoveryDecision]) -> Vec<KillOutcome> {
    let opts = KillOpts::from_config(&sup.config().get().supervision);
    let mut out = Vec::with_capacity(decisions.len());
    let mut changed = Vec::new();

    for decision in decisions {
        let spawn_id = decision.spawn_id().to_string();
        let report = match decision.action() {
            RecoveryAction::Terminate => kill_tree(sup, &spawn_id, opts).await,
            RecoveryAction::Keep | RecoveryAction::Adopt => {
                let keep = decision.action() == RecoveryAction::Keep;
                // The user decided: kept or adopted is no longer a leftover.
                leftover_set(&CRASH_LEFTOVERS).lock().remove(&spawn_id);
                let row = sup.row(&spawn_id).await.ok().flatten();
                match row {
                    Some(row) => {
                        if keep {
                            set_keep_alive(sup, &spawn_id, true).await;
                        }
                        adopt_row(
                            sup,
                            &spawn_id,
                            row.pid.unwrap_or(0) as i32,
                            row.pgid.map(|p| p as i32),
                            row.proc_start_ts,
                            if keep { note::KEPT } else { note::CRASH_ADOPTED },
                        )
                        .await;
                        KillReport::refused(
                            Some(spawn_id.clone()),
                            "unfenced",
                            if keep { note::KEPT } else { note::CRASH_ADOPTED },
                        )
                    }
                    None => KillReport::refused(
                        Some(spawn_id.clone()),
                        "notFound",
                        "notFound",
                    ),
                }
            }
        };
        resolve_pending(&spawn_id);
        changed.push(spawn_id.clone());
        out.push(KillOutcome {
            target: KillTarget::spawn(spawn_id),
            report,
        });
    }

    emit_registry_change(sup, &changed);
    if let Some(bus) = sup.events() {
        bus.emit(
            RECOVERY_PENDING,
            RecoveryPendingEvent {
                count: pending().len() as u32,
            },
        );
    }
    out
}

// ---------------------------------------------------------------------------
// shutdown
// ---------------------------------------------------------------------------

/// End this instance's own children on the way out.
///
/// macOS has no `PR_SET_PDEATHSIG`: nothing in the kernel ends our children when
/// we die, and `kill_on_drop` is banned (M1-PLAN T19.4). Without this, a
/// `SIGTERM` to the host leaves every PTY and headless child running until the
/// next boot's sweep finds them.
///
/// Two kinds of row are spared:
///
///   * `keep_alive` rows — that flag exists precisely to outlive us;
///   * `kind = 'sidecar'` — signalling `gilde-node` from here would look like a
///     crash to `SidecarManager` and start its **restart** path mid-shutdown.
///     The sidecar is taken down deliberately by `RunEvent::Exit`, which runs
///     immediately after this sweep on the way out.
pub async fn shutdown_sweep(sup: &Supervisor) -> Vec<KillReport> {
    shutdown_sweep_within(sup, SHUTDOWN_SWEEP_BUDGET).await
}

/// [`shutdown_sweep`] inside `budget` (A40.1, A40.2).
///
/// * A row the quit coordinator handed over to the next boot (live, note
///   [`note::QUIT_SURVIVOR`]) is **left as is**: signalling it again would
///   rewrite it to `kill_incomplete`, and the next boot's
///   [`sweep_quit_survivors_into`] would never find it (§10.2 step 6).
/// * Every other row is signalled **concurrently**, so the sweep takes about
///   one kill however many rows there are; a kill still running when the
///   budget ends is abandoned (its signals are sent; the next boot owns the
///   rest) and the sweep returns, so the caller can close the instance.
pub async fn shutdown_sweep_within(sup: &Supervisor, budget: Duration) -> Vec<KillReport> {
    let deadline = tokio::time::Instant::now() + budget;
    let me = sup.app_instance_id().to_string();
    let rows = match sup
        .db()
        .read(move |conn| dao::proc::rows_of_instance(conn, &me))
        .await
    {
        Ok(rows) => rows,
        Err(err) => {
            tracing::warn!(target: "supervise", error = %err, "shutdown sweep could not read the registry");
            return Vec::new();
        }
    };

    // About a third of the budget per wait (TERM grace, then the KILL check).
    let grace = Duration::from_millis(1_500).min(budget / 3);
    let opts = KillOpts::from_config(&sup.config().get().supervision).grace(grace);
    let mut targets = Vec::new();
    for row in rows {
        if !row.state.is_live() || row.keep_alive || row.kind == ProcKind::Sidecar {
            continue;
        }
        if is_handed_over_quit_survivor(&row) {
            continue;
        }
        if row.state == ProcState::Planned {
            mark_state(sup, &row.spawn_id, ProcState::Lost, note::SHUTDOWN).await;
            continue;
        }
        targets.push(row.spawn_id);
    }
    let kills = targets.iter().map(|id| kill_tree(sup, id, opts)).collect();
    let (reports, unfinished) = join_until(kills, deadline).await;
    if unfinished > 0 {
        tracing::warn!(target: "supervise", unfinished, "shutdown sweep reached its budget; the next boot owns the rest");
    }
    reports
}

/// A40.1: a row the quit coordinator left `running` with the quit-survivor
/// note for the next boot's sweep.
pub fn is_handed_over_quit_survivor(row: &ProcRow) -> bool {
    row.state.is_live() && row.note.as_deref() == Some(note::QUIT_SURVIVOR)
}

/// Drive `futs` concurrently until all are done or `deadline` passes. Returns
/// the finished outputs (in completion order) and how many were abandoned.
async fn join_until<F: std::future::Future>(futs: Vec<F>, deadline: tokio::time::Instant) -> (Vec<F::Output>, usize) {
    use std::future::Future as _;
    use std::task::Poll;
    let mut pending: Vec<Option<std::pin::Pin<Box<F>>>> = futs.into_iter().map(|f| Some(Box::pin(f))).collect();
    let mut done = Vec::with_capacity(pending.len());
    let timer = tokio::time::sleep_until(deadline);
    tokio::pin!(timer);
    std::future::poll_fn(|cx| {
        for slot in pending.iter_mut() {
            if let Some(fut) = slot {
                if let Poll::Ready(out) = fut.as_mut().poll(cx) {
                    done.push(out);
                    *slot = None;
                }
            }
        }
        if pending.iter().all(Option::is_none) || timer.as_mut().poll(cx).is_ready() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
    let unfinished = pending.iter().filter(|f| f.is_some()).count();
    (done, unfinished)
}

/// Install `SIGTERM` / `SIGINT` handling for the host process.
///
/// A signal is a *request to quit*, so it is routed through Tauri's own exit
/// path (`AppHandle::exit`), which runs `RunEvent::Exit` and therefore the
/// sidecar shutdown that already lives there. Before that, this instance's own
/// children are ended and its `app_instance` row is closed, so the next boot
/// sees a clean shutdown instead of a crash with orphans.
///
/// A second signal while the first is still draining exits immediately: a user
/// pressing ⌃C twice means it.
pub fn install_signal_handlers(app: &tauri::AppHandle, sup: Arc<Supervisor>) {
    use tokio::signal::unix::{signal, SignalKind};

    let app = app.clone();
    spawn_task(async move {
        let (mut term, mut int) = match (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) {
            (Ok(term), Ok(int)) => (term, int),
            _ => {
                tracing::warn!(target: "supervise", "could not install signal handlers");
                return;
            }
        };

        tokio::select! {
            _ = term.recv() => tracing::info!(target: "supervise", "SIGTERM received; shutting down"),
            _ = int.recv() => tracing::info!(target: "supervise", "SIGINT received; shutting down"),
        }

        // A second signal must not be swallowed by the drain below.
        let hard = tokio::spawn(async move {
            tokio::select! {
                _ = term.recv() => {}
                _ = int.recv() => {}
            }
            tracing::warn!(target: "supervise", "second signal; exiting immediately");
            std::process::exit(130);
        });

        // A signal is a request to quit: the exit path runs the quit of
        // contract §10.2 (record first, bounded stop, never SIGKILL a vendor
        // CLI, then `ended_at`), so nothing is swept here.
        let _ = &sup;
        crate::lifecycle::quit::authorize_exit();
        app.exit(0);
        // Keep the second-signal guard alive until the process is gone.
        let _ = hard.await;
    });
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::models::argv_signature;

    fn planned_row(argv: &[&str], cwd: &str, planned_at: i64) -> ProcRow {
        let argv: Vec<String> = argv.iter().map(|a| (*a).to_string()).collect();
        ProcRow {
            spawn_id: "sp-1".into(),
            app_instance: "inst-old".into(),
            generation: 0,
            kind: ProcKind::Pty,
            vendor: Some("claude".into()),
            session_id: None,
            run_id: None,
            state: ProcState::Planned,
            pid: None,
            pgid: None,
            proc_start_ts: None,
            argv: serde_json::to_string(&argv).expect("argv json"),
            argv_sig: argv_signature(&argv),
            cwd: cwd.into(),
            env_marker: "sp-1".into(),
            config_dir: None,
            worktree_id: Some("wt-1".into()),
            keep_alive: false,
            planned_at,
            started_at: None,
            exited_at: None,
            exit_code: None,
            exit_signal: None,
            note: None,
        }
    }

    fn ps_row(pid: i32, args: &str, elapsed: u64, rss_kb: u64) -> PsRow {
        PsRow {
            pid,
            ppid: 1,
            pgid: pid,
            rss_kb,
            cpu_pct: 0.0,
            elapsed_secs: elapsed,
            user: "tester".into(),
            args: args.into(),
        }
    }

    /// BACKLOG M1.5 #5. The row in the WARN was always a finished `--version`
    /// probe; everything else that fails a fence still deserves the warning.
    #[test]
    fn only_a_finished_version_probe_downgrades_the_fence_warning() {
        let mut probe = planned_row(&["/usr/bin/codex", "--version"], "/", 1_000);
        probe.kind = ProcKind::Probe;
        probe.state = ProcState::Running;
        probe.pid = Some(16_201);

        assert!(
            mismatch_is_a_finished_probe(&probe, MismatchReason::Argv, false),
            "a probe that is already gone is the known-benign race"
        );
        assert!(
            !mismatch_is_a_finished_probe(&probe, MismatchReason::Argv, true),
            "a pid that is still alive is a real mismatch and stays a warning"
        );
        assert!(
            !mismatch_is_a_finished_probe(&probe, MismatchReason::StartTime, false),
            "pid reuse is never benign"
        );
        for reason in [
            MismatchReason::Uid,
            MismatchReason::EnvMarker,
            MismatchReason::NoPid,
        ] {
            assert!(
                !mismatch_is_a_finished_probe(&probe, reason, false),
                "{reason:?} is not the fail-closed argv case"
            );
        }

        // A pty session that fails the same fence is a session the user can see
        // and has to be told about, however dead its pid is.
        let mut session = probe.clone();
        session.kind = ProcKind::Pty;
        assert!(!mismatch_is_a_finished_probe(
            &session,
            MismatchReason::Argv,
            false
        ));
    }

    #[test]
    fn a_planned_row_with_no_matching_command_line_has_no_candidates() {
        let row = planned_row(&["/usr/bin/true", "--flag"], "/tmp", 1_000);
        let snap = Snapshot::from_rows(vec![ps_row(4242, "/usr/bin/false --flag", 10, 100)]);
        assert!(planned_row_candidates(&row, &snap).is_empty());
    }

    #[test]
    fn a_planned_row_never_matches_a_process_older_than_the_plan() {
        // `planned_at` far in the future: no live process can have started after
        // it, so the fence must reject every candidate on the machine.
        let exe = std::env::current_exe().expect("current exe");
        let row = planned_row(
            &[&exe.to_string_lossy()],
            "/tmp",
            (now_ms() + 3_600_000).max(0),
        );
        let snap = ps_snapshot().expect("snapshot");
        assert!(
            planned_row_candidates(&row, &snap).is_empty(),
            "a process cannot have started after the row that planned it"
        );
    }

    #[test]
    fn our_own_pid_on_a_stale_instance_row_is_not_a_live_sibling() {
        let inst = AppInstanceRow {
            id: "inst-old".into(),
            pid: std::process::id() as i64,
            boot_id: "boot".into(),
            version: "0.0.0".into(),
            started_at: 1,
            ended_at: None,
        };
        assert!(
            !is_live_sibling(&inst),
            "a recycled pid that is now ours means the writer is long gone"
        );
    }

    #[test]
    fn a_dead_pid_is_not_a_live_sibling() {
        let inst = AppInstanceRow {
            id: "inst-old".into(),
            // pid 0 is never a process we could confuse ourselves with.
            pid: 0,
            boot_id: "boot".into(),
            version: "0.0.0".into(),
            started_at: 1,
            ended_at: None,
        };
        assert!(!is_live_sibling(&inst));
    }

    #[test]
    fn the_undo_table_cancels_once_and_only_once() {
        let token = "tok-test-1";
        undo_table().lock().insert(
            token.to_string(),
            ScheduledKill {
                targets: vec!["sp-1".into()],
                group: false,
                cancelled: Arc::new(AtomicBool::new(false)),
                on_cancel: None,
            },
        );
        assert!(undo_is_pending(token));
        assert_eq!(undo_target(token).as_deref(), Some("sp-1"));
        assert_eq!(undo_targets(token), vec!["sp-1".to_string()]);
        assert!(undo_cancel(token));
        assert!(!undo_is_pending(token));
        assert!(!undo_cancel(token), "a token may only be spent once");
        assert!(!undo_cancel("never-existed"));
    }

    #[test]
    fn a_group_token_names_every_member_and_its_hook_runs_once_on_the_undo() {
        let token = "tok-test-group";
        let hits = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let h = hits.clone();
        undo_table().lock().insert(
            token.to_string(),
            ScheduledKill {
                targets: vec!["sp-a".into(), "sp-b".into()],
                group: true,
                cancelled: Arc::new(AtomicBool::new(false)),
                on_cancel: Some(Box::new(move || {
                    h.fetch_add(1, Ordering::SeqCst);
                })),
            },
        );
        assert_eq!(undo_target(token), None, "a group has no single target");
        assert_eq!(undo_targets(token), vec!["sp-a".to_string(), "sp-b".to_string()]);
        assert!(undo_cancel(token));
        assert!(!undo_cancel(token));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert!(undo_targets(token).is_empty());
    }

    #[test]
    fn a_stop_group_keeps_each_spawn_id_once_in_order() {
        let mut g = StopGroup::new();
        assert!(g.is_empty());
        g.push("sp-1", KillOpts::default());
        g.push("sp-2", KillOpts::default());
        g.push("sp-1", KillOpts::default().grace(Duration::from_millis(5)));
        assert_eq!(g.len(), 2);
        assert_eq!(g.spawn_ids(), vec!["sp-1".to_string(), "sp-2".to_string()]);
    }

    #[test]
    fn recovered_proc_serializes_to_the_ipc_contract() {
        let mut row = planned_row(&["/bin/claude", "--token", "s3cret"], "/tmp/wt", 1_000);
        row.pid = Some(4242);
        row.argv = serde_json::to_string(&vec![
            "/bin/claude".to_string(),
            "--token".to_string(),
            "«redacted»".to_string(),
        ])
        .expect("argv json");
        let snap = Snapshot::from_rows(vec![ps_row(4242, "/bin/claude", 90, 2_048)]);

        let recovered = RecoveredProc::of(&row, &snap, now_ms());
        assert_eq!(
            recovered.age_secs, 90,
            "with no started_at, the age falls back to the kernel's own etime"
        );
        assert_eq!(recovered.rss_bytes, 2_048 * 1024);
        assert!(!recovered.argv_masked.contains("s3cret"));

        let v = serde_json::to_value(&recovered).expect("json");
        assert_eq!(v["spawnId"], serde_json::json!("sp-1"));
        assert_eq!(v["kind"], serde_json::json!("pty"));
        assert_eq!(v["vendor"], serde_json::json!("claude"));
        assert_eq!(v["worktreeId"], serde_json::json!("wt-1"));
        assert_eq!(v["cwd"], serde_json::json!("/tmp/wt"));
        assert_eq!(v["keepAlive"], serde_json::json!(false));
        assert_eq!(v["suggested"], serde_json::json!("terminate"));
        assert!(v["argvDisplay"].is_string());
        assert!(v.get("argvMasked").is_none(), "the wire name is argvDisplay");
    }

    #[test]
    fn recovery_decisions_use_the_ipc_wire_shape() {
        let decision: RecoveryDecision =
            serde_json::from_value(serde_json::json!({ "action": "terminate", "spawnId": "sp-9" }))
                .expect("deserialize");
        assert_eq!(decision.spawn_id(), "sp-9");
        assert_eq!(decision.action(), RecoveryAction::Terminate);

        let keep = RecoveryDecision::Keep {
            spawn_id: "sp-1".into(),
        };
        let v = serde_json::to_value(&keep).expect("json");
        assert_eq!(v, serde_json::json!({ "action": "keep", "spawnId": "sp-1" }));

        let adopt: RecoveryDecision =
            serde_json::from_value(serde_json::json!({ "action": "adopt", "spawnId": "x" }))
                .expect("deserialize");
        assert_eq!(adopt.action(), RecoveryAction::Adopt);
    }

    #[test]
    fn a_sweep_report_says_when_it_did_nothing() {
        let empty = SweepReport::default();
        assert!(!empty.touched_anything());
        assert_eq!(empty.pending_count(), 0);

        let v = serde_json::to_value(&empty).expect("json");
        assert_eq!(v["rebootLost"], serde_json::json!(0));
        assert_eq!(v["autoTerminated"], serde_json::json!([]));
    }

    #[test]
    fn the_reconcile_report_carries_no_cwd() {
        let report = ReconcileReport {
            checked: 1,
            exited: 0,
            changed: vec![],
            rows: vec![ReconciledRow {
                spawn_id: "sp-1".into(),
                pid: 42,
                alive: true,
                rss_bytes: 1024,
                idle_secs: 3,
            }],
            rss_bytes: 1024,
            at: 1,
        };
        let v = serde_json::to_value(&report).expect("json");
        assert_eq!(v["rows"][0]["idleSecs"], serde_json::json!(3));
        assert!(
            v["rows"][0].get("cwd").is_none(),
            "reconcile must not resolve a cwd for rows nobody is rendering"
        );
    }
}
