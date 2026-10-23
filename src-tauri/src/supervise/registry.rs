//! The durable PID registry: `Supervisor`, `SpawnPlan`, `SpawnGuard`
//! (DESIGN §8-2, §8-3, §6-1), and the [`SupervisorApi`] every Gilde 2 task
//! spawns through (contract §2.1, §3.8, §9.3, §9.8; owner T09).
//!
//! Gilde 2 additions:
//!
//! * **the spawn gate** (§2.4 B6): every plan waits for the boot sweep;
//! * **memory-only mode** (§2.4): no durable row, no spawn — `DISK_FULL`; a
//!   durable write that fails with `ENOSPC`/read-only enters the mode;
//! * **the launch allowlist** (§9.3): `SupervisorApi::plan_spawn` refuses an
//!   `argv[0]` that is not allowlisted for its kind (`REFUSED{notAllowlisted}`);
//! * **one redaction** (§9.7, F-03): `proc_registry.argv` holds the redacted
//!   argv (`supervise::redact`); the signature still hashes the raw argv;
//! * `parent_uid` (§3.8): the owning session, written with the planned row.
//!
//! **Write before spawn.** A row describing the child is committed with
//! `synchronous=FULL` *before* the process exists, so that a hard crash between
//! the two can never produce a process nobody knows about. The fsync costs
//! roughly 1–3 ms per spawn; DESIGN §8-2 calls that a deliberate price and
//! forbids batching it away, because the durability of that one row is what
//! every recovery path is built on.
//!
//! The lifecycle:
//!
//! ```text
//! plan_spawn(plan)                → INSERT state='planned', env_marker=spawn_id  [fsync]
//! guard.env_marker()              → the value to stamp into GILDE_SPAWN_ID
//! guard.mark_running(pid, pgid)   → kernel start time via libproc, state='running'
//! guard.mark_exited(code, signal) → state='exited'
//! Drop while armed && !running    → state='lost', note='spawn failed or aborted'
//! ```
//!
//! The window between the INSERT and `mark_running` is the only moment a
//! process of ours can exist without a recorded pid. It is not wished away: the
//! reaper's *planned-row adoption* (DESIGN §8-5) closes it by matching live
//! processes against `argv_sig` + `cwd` + `planned_at`.
//!
//! ## Why `argv` and `argv_sig` disagree on purpose
//!
//! The `argv` column is stored **masked** (DESIGN §8-2: "JSON 배열, 마스킹 후"),
//! because the monitor panel and `event_log` read it and an argv can carry a
//! token. The `argv_sig` column must instead hash the **unmasked** argv: fence 2
//! compares it against the live process's real command line, which of course
//! still contains the secret. Storing the masked hash would silently disable
//! fence 2 for exactly the commands that matter most.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::accounts::AccountSlot;
use crate::config::ConfigStore;
use crate::error::{GildeError, Result};
use crate::events::Events;
use crate::store::models::{
    argv_signature, AppInstanceRow, NewPlannedProc, ProcKind, ProcRow, ProcState,
};
use crate::store::{dao, now_ms, Db};
use crate::contract::activity::ExitInfo;
use crate::contract::ids::ProcessKey;
use crate::contract::services::{
    ProcSnapshot, SignalOutcome, SignalPlan, SpawnRequest, SpawnTicket, SupervisorApi,
};
use crate::contract::IpcError;
use crate::lifecycle::memory_only::{MemoryOnly, MemoryOnlyReason};
use crate::lifecycle::spawn_gate::SpawnGate;
use crate::supervise::identity::ENV_SPAWN_ID;
use crate::supervise::procscan::{start_time_of, ScanHub};

/// Environment variable carrying the id of the app instance that started a
/// child. The untracked-orphan sweep (DESIGN §8-5 step 4) is built on it.
pub const ENV_APP_INSTANCE: &str = "GILDE_APP_INSTANCE";

/// `note` written on a row whose guard died before the process started.
pub const NOTE_SPAWN_ABORTED: &str = "spawn failed or aborted";

/// Locale of last resort. A Finder launch inherits no locale at all, and a CLI
/// without one mangles UTF-8 (DESIGN §6-1).
pub const DEFAULT_LANG: &str = "en_US.UTF-8";

/// Terminal identity we present to every child, interactive or not.
pub const TERM_VALUE: &str = "xterm-256color";
pub const COLORTERM_VALUE: &str = "truecolor";
pub const TERM_PROGRAM_VALUE: &str = "gilde-code";

/// Proxy variables copied through when the ambient environment has them.
pub const PROXY_KEYS: [&str; 8] = [
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
];

/// Keys `agents.envPassthrough` may never set: they are this app's identity
/// markers and the whole supervision story depends on them being ours.
pub const RESERVED_ENV_KEYS: [&str; 2] = [ENV_SPAWN_ID, ENV_APP_INSTANCE];

/// The account directory variable each vendor reads (DECISIONS §우선순위 2).
///
/// `None` for vendors with no directory-based account switching in M1
/// (`cursorAgent`, `gemini`) — the slot exists in the model, the env key does
/// not exist in the CLI.
pub fn account_env_key(vendor: &str) -> Option<&'static str> {
    match vendor {
        "claude" => Some("CLAUDE_CONFIG_DIR"),
        "codex" => Some("CODEX_HOME"),
        "agy" | "antigravity" => Some("AGY_HOME"),
        _ => None,
    }
}

/// A39.1(b): whether a registry row is one of Gilde's own vendor sessions.
/// Such a row is a vendor CLI for the kill policy (§9.8 [C5]) and for quit
/// survivor recording (§10.2) **whatever its exec path classifies as** — an
/// unknown install layout never makes Gilde SIGKILL its own session:
///
/// * a row whose `vendor` was recorded at plan time (the classifier knew the
///   binary, or the command a hosted pane runs);
/// * every Conversation, `claude attach`, sidecar turn (and the legacy
///   `headless` turn) and vendor-binary probe (`vendorcli`) row: those kinds
///   launch nothing but a resolved vendor binary (§3.8, §9.3, A13);
/// * a PTY or hosted-pane row that a Gilde session owns (`owned_by_session`:
///   `parent_uid`, or the older `session_id`): Gilde 2 opens PTYs only for a
///   session's agent and for a lead's pane members.
///
/// Sidecar, probe, git and IDE rows are helpers and keep the TERM → KILL
/// escalation.
pub fn is_own_vendor_row(kind: ProcKind, vendor: Option<&str>, owned_by_session: bool) -> bool {
    if vendor.is_some_and(|v| !v.is_empty()) {
        return true;
    }
    match kind {
        ProcKind::Convo | ProcKind::Attach | ProcKind::Turn | ProcKind::Headless | ProcKind::Vendorcli => true,
        ProcKind::Pty | ProcKind::Pane => owned_by_session,
        ProcKind::Sidecar | ProcKind::Gateway | ProcKind::Probe | ProcKind::Git | ProcKind::Ide => false,
    }
}

// ---------------------------------------------------------------------------
// SpawnPlan
// ---------------------------------------------------------------------------

/// Everything the registry needs to describe a child *before* it exists.
///
/// `argv` is the **raw** argv, `argv[0]` being the absolute binary path. The
/// masking happens inside `SupervisorApi::plan_spawn`; callers must not mask
/// first or fence 2 will hash the wrong thing.
#[derive(Debug, Clone, PartialEq)]
pub struct SpawnPlan {
    pub kind: ProcKind,
    pub vendor: Option<String>,
    pub session_id: Option<String>,
    pub run_id: Option<String>,
    /// Sidecar generation. Host spawns are `0`.
    pub generation: i64,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub config_dir: Option<PathBuf>,
    pub worktree_id: Option<String>,
    /// The owning session (contract §3.8), stored in `proc_registry.parent_uid`.
    pub parent_uid: Option<String>,
}

impl Default for SpawnPlan {
    fn default() -> Self {
        SpawnPlan {
            kind: ProcKind::Probe,
            vendor: None,
            session_id: None,
            run_id: None,
            generation: 0,
            argv: Vec::new(),
            cwd: PathBuf::from("/"),
            config_dir: None,
            worktree_id: None,
            parent_uid: None,
        }
    }
}

impl SpawnPlan {
    /// The minimum a plan needs: what kind of process, its argv and its cwd.
    pub fn new(kind: ProcKind, argv: Vec<String>, cwd: impl Into<PathBuf>) -> SpawnPlan {
        SpawnPlan {
            kind,
            argv,
            cwd: cwd.into(),
            ..SpawnPlan::default()
        }
    }

    pub fn vendor(mut self, vendor: impl Into<String>) -> SpawnPlan {
        self.vendor = Some(vendor.into());
        self
    }

    pub fn session(mut self, session_id: impl Into<String>) -> SpawnPlan {
        self.session_id = Some(session_id.into());
        self
    }

    pub fn run(mut self, run_id: impl Into<String>) -> SpawnPlan {
        self.run_id = Some(run_id.into());
        self
    }

    pub fn generation(mut self, generation: i64) -> SpawnPlan {
        self.generation = generation;
        self
    }

    pub fn worktree(mut self, worktree_id: impl Into<String>) -> SpawnPlan {
        self.worktree_id = Some(worktree_id.into());
        self
    }

    pub fn config_dir(mut self, dir: impl Into<PathBuf>) -> SpawnPlan {
        self.config_dir = Some(dir.into());
        self
    }

    pub fn parent(mut self, uid: impl Into<String>) -> SpawnPlan {
        self.parent_uid = Some(uid.into());
        self
    }
}

// ---------------------------------------------------------------------------
// SpawnGuard
// ---------------------------------------------------------------------------

const ARMED: u8 = 0;
const RUNNING: u8 = 1;
const FINISHED: u8 = 2;

/// A committed `planned` row, waiting for the process it describes.
///
/// Dropping it without [`SpawnGuard::mark_running`] marks the row `lost`. That
/// is the whole design: an abandoned guard is indistinguishable from a spawn
/// that failed, and both must leave a row that says so rather than a `planned`
/// row that the next boot's reaper will spend effort trying to adopt.
pub struct SpawnGuard {
    db: Db,
    spawn_id: String,
    state: AtomicU8,
}

impl std::fmt::Debug for SpawnGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpawnGuard")
            .field("spawn_id", &self.spawn_id)
            .field("state", &self.state.load(Ordering::Relaxed))
            .finish()
    }
}

impl SpawnGuard {
    fn new(db: Db, spawn_id: String) -> SpawnGuard {
        SpawnGuard {
            db,
            spawn_id,
            state: AtomicU8::new(ARMED),
        }
    }

    /// The value to stamp into `GILDE_SPAWN_ID` on the child. Identical to
    /// [`SpawnGuard::spawn_id`]: one secret, one row, one process.
    pub fn env_marker(&self) -> &str {
        &self.spawn_id
    }

    pub fn spawn_id(&self) -> &str {
        &self.spawn_id
    }

    /// The child exists. Records its pid, its process group and — the fence
    /// that makes pid reuse detectable — the **kernel** start time.
    ///
    /// Deliberately synchronous: it is called immediately after `spawn()` on
    /// whatever thread did the spawning (including a PTY reader thread and
    /// `Drop`), and an `async` signature there would mean either a runtime
    /// handle in every caller or a window where the pid is known but unrecorded.
    pub fn mark_running(&self, pid: i32, pgid: i32) -> Result<()> {
        let proc_start_ts = start_time_of(pid);
        if proc_start_ts.is_none() {
            // Either the child died within microseconds, or libproc refused.
            // Fence 1 will refuse to verify the row, which is the safe outcome.
            tracing::warn!(
                target: "supervise",
                spawn_id = %self.spawn_id,
                pid,
                "no kernel start time for a just-spawned child; the row cannot pass fence 1"
            );
        }
        let spawn_id = self.spawn_id.clone();
        let at = now_ms();
        let pgid = if pgid > 0 { Some(pgid as i64) } else { None };
        self.db.blocking(move |conn| {
            dao::proc::mark_running(conn, &spawn_id, pid as i64, pgid, proc_start_ts, at)?;
            Ok(())
        })?;
        self.state.store(RUNNING, Ordering::SeqCst);
        Ok(())
    }

    /// The child is gone and we know how. Idempotent from the caller's point of
    /// view: a second call simply rewrites the same terminal row.
    pub fn mark_exited(&self, code: Option<i32>, signal: Option<i32>) -> Result<()> {
        let spawn_id = self.spawn_id.clone();
        let at = now_ms();
        let code = code.map(i64::from);
        let signal = signal.map(i64::from);
        self.db.blocking(move |conn| {
            dao::proc::mark_exited(conn, &spawn_id, code, signal, at)?;
            Ok(())
        })?;
        self.state.store(FINISHED, Ordering::SeqCst);
        Ok(())
    }

    /// Give up on this row *without* a process ever having existed — the spawn
    /// failed, or the caller aborted before spawning. Equivalent to dropping
    /// the guard, but with a reason of your own.
    pub fn abandon(self, note: &str) {
        self.close_lost(note);
        self.state.store(FINISHED, Ordering::SeqCst);
    }

    fn close_lost(&self, note: &str) {
        let spawn_id = self.spawn_id.clone();
        let note = note.to_string();
        let result = self.db.blocking(move |conn| {
            dao::proc::mark_state(conn, &spawn_id, ProcState::Lost, Some(&note))?;
            Ok(())
        });
        if let Err(err) = result {
            tracing::error!(
                target: "supervise",
                spawn_id = %self.spawn_id,
                error = %err,
                "could not mark an abandoned spawn row as lost"
            );
        }
    }
}

impl Drop for SpawnGuard {
    fn drop(&mut self) {
        if self.state.load(Ordering::SeqCst) == ARMED {
            self.close_lost(NOTE_SPAWN_ABORTED);
        }
    }
}

// ---------------------------------------------------------------------------
// environment composition — DESIGN §6-1
// ---------------------------------------------------------------------------

/// The ambient half of a child's environment.
///
/// Every field is explicit so that a child's environment is **the same whether
/// the app was started from Finder, from `cargo tauri dev`, or from a terminal**
/// — the single most common source of "it works for me" in agent tooling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvBase {
    /// The resolved `PATH` (T09's login-shell probe), not the app's own.
    pub path: String,
    pub home: PathBuf,
    pub user: Option<String>,
    pub logname: Option<String>,
    pub shell: Option<String>,
    pub tmpdir: Option<String>,
    pub lang: Option<String>,
    pub lc_all: Option<String>,
    pub ssh_auth_sock: Option<String>,
    /// Proxy variables that were present in the ambient environment.
    pub proxies: BTreeMap<String, String>,
}

impl EnvBase {
    /// A base with nothing but a `PATH` and a `HOME`.
    pub fn new(path: impl Into<String>, home: impl Into<PathBuf>) -> EnvBase {
        EnvBase {
            path: path.into(),
            home: home.into(),
            user: None,
            logname: None,
            shell: None,
            tmpdir: None,
            lang: None,
            lc_all: None,
            ssh_auth_sock: None,
            proxies: BTreeMap::new(),
        }
    }

    /// Fill the optional fields from this process's environment, keeping
    /// `resolved_path` as the `PATH` (ours is the wrong one — a GUI app's `PATH`
    /// is `/usr/bin:/bin:/usr/sbin:/sbin`).
    pub fn from_process_env(resolved_path: impl Into<String>) -> EnvBase {
        let home = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/"));
        let mut base = EnvBase::new(resolved_path, home);
        base.user = std::env::var("USER").ok();
        base.logname = std::env::var("LOGNAME").ok().or_else(|| base.user.clone());
        base.shell = std::env::var("SHELL").ok();
        base.tmpdir = std::env::var("TMPDIR").ok();
        base.lang = std::env::var("LANG").ok();
        base.lc_all = std::env::var("LC_ALL").ok();
        base.ssh_auth_sock = std::env::var("SSH_AUTH_SOCK").ok();
        for key in PROXY_KEYS {
            if let Ok(value) = std::env::var(key) {
                base.proxies.insert(key.to_string(), value);
            }
        }
        base
    }
}

// ---------------------------------------------------------------------------
// Supervisor
// ---------------------------------------------------------------------------

/// Owner of the app instance row and the only door to `plan_spawn`.
///
/// Cheap to clone through an `Arc`; one instance per running app.
pub struct Supervisor {
    db: Db,
    boot_id: String,
    app_instance_id: String,
    version: String,
    config: Arc<ConfigStore>,
    events: Option<Events>,
    gate: Arc<SpawnGate>,
    memory_only: Arc<MemoryOnly>,
    scans: Arc<ScanHub>,
    /// Guards of rows planned through [`SupervisorApi::plan_spawn`], keyed by
    /// spawn id, until the caller reports the exit.
    tickets: parking_lot::Mutex<std::collections::HashMap<String, SpawnGuard>>,
    /// Leads whose teammates run in PaneHost (A11.3 `hosted_leads`), noted
    /// by the composition root when the sessions manager reports a hosted
    /// lead started, removed when it exits.
    hosted: parking_lot::Mutex<Vec<crate::discovery::HostedLead>>,
}

impl std::fmt::Debug for Supervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Supervisor")
            .field("app_instance_id", &self.app_instance_id)
            .field("boot_id", &self.boot_id)
            .field("version", &self.version)
            .finish()
    }
}

impl Supervisor {
    /// `sysctl -n kern.bootsessionuuid`, read through `sysctlbyname(3)` — no
    /// subprocess, so this needs no guard of its own.
    ///
    /// It is the fence that makes every pid from a previous boot meaningless:
    /// the startup sweep marks those rows `lost` **without a fence check and
    /// without a single signal** (DESIGN §8-2).
    pub fn boot_id() -> Result<String> {
        dao::app_instance::current_boot_id()
    }

    /// Record this run of the app and take ownership of its rows.
    pub async fn open_instance(
        db: &Db,
        config: Arc<ConfigStore>,
        version: impl Into<String>,
    ) -> Result<Supervisor> {
        let boot_id = Supervisor::boot_id()?;
        let version = version.into();
        let row = AppInstanceRow {
            id: Uuid::now_v7().to_string(),
            pid: std::process::id() as i64,
            boot_id: boot_id.clone(),
            version: version.clone(),
            started_at: now_ms(),
            ended_at: None,
        };
        let app_instance_id = row.id.clone();
        db.write_durable(move |conn| dao::app_instance::insert(conn, &row))
            .await?;

        tracing::info!(
            target: "supervise",
            app_instance = %app_instance_id,
            boot_id = %boot_id,
            "app instance opened"
        );

        Ok(Supervisor {
            db: db.clone(),
            boot_id,
            app_instance_id,
            version,
            config,
            events: None,
            // Tests and tools run no boot sweep: open unless the boot path
            // installs its own closed gate (`with_spawn_gate`).
            gate: SpawnGate::opened(),
            memory_only: MemoryOnly::new(),
            scans: ScanHub::new(),
            tickets: parking_lot::Mutex::new(std::collections::HashMap::new()),
            hosted: parking_lot::Mutex::new(Vec::new()),
        })
    }

    /// A hosted lead started (PaneHost hosts its teammates): discovery's
    /// `Hosted` team join reads it through [`crate::discovery::OwnProcesses`].
    pub fn note_hosted_lead(&self, pid: i32, spawned_at: i64, config_dir: Option<String>) {
        let mut hosted = self.hosted.lock();
        hosted.retain(|h| h.pid != pid);
        hosted.push(crate::discovery::HostedLead {
            pid,
            spawned_at,
            config_dir: config_dir.unwrap_or_default(),
        });
    }

    /// A hosted lead exited.
    pub fn drop_hosted_lead(&self, pid: i32) {
        self.hosted.lock().retain(|h| h.pid != pid);
    }

    /// The boot path's gate (closed until the boot sweep opens it, §2.4 B6).
    pub fn with_spawn_gate(mut self, gate: Arc<SpawnGate>) -> Supervisor {
        self.gate = gate;
        self
    }

    /// The process-wide memory-only state (§2.4).
    pub fn with_memory_only(mut self, mode: Arc<MemoryOnly>) -> Supervisor {
        self.memory_only = mode;
        self
    }

    /// The shared discovery scan (§2.2.1 `procscan.rs`).
    pub fn with_scan_hub(mut self, hub: Arc<ScanHub>) -> Supervisor {
        self.scans = hub;
        self
    }

    pub fn spawn_gate_handle(&self) -> &Arc<SpawnGate> {
        &self.gate
    }

    pub fn memory_only_state(&self) -> &Arc<MemoryOnly> {
        &self.memory_only
    }

    pub fn scan_hub(&self) -> &Arc<ScanHub> {
        &self.scans
    }

    /// B5: record when the window became visible (T-boot-2 compares every
    /// `planned_at` of this instance against it).
    pub async fn mark_window_visible(&self, at: i64) -> Result<()> {
        let id = self.app_instance_id.clone();
        self.db
            .write_durable(move |conn| {
                dao::app_instance::set_window_visible(conn, &id, at)?;
                Ok(())
            })
            .await
    }

    /// A durable write failed: classify it, enter memory-only mode when it is
    /// a full or read-only disk, and say whether it did.
    fn note_write_failure(&self, err: &GildeError) -> bool {
        let reason = match err {
            GildeError::Io(io) => Some(MemoryOnlyReason::of_io(io)).filter(|r| *r != MemoryOnlyReason::Unwritable),
            GildeError::Db(msg) => MemoryOnlyReason::of_db_message(msg),
            _ => None,
        };
        match reason {
            Some(r) => {
                self.memory_only.enter(r);
                true
            }
            None => false,
        }
    }

    /// Attach the event bus. Separate from [`Supervisor::open_instance`]
    /// because the supervisor is created during boot, before the window (and
    /// therefore the bus) exists.
    pub fn with_events(mut self, events: Events) -> Supervisor {
        self.events = Some(events);
        self
    }

    pub fn set_events(&mut self, events: Events) {
        self.events = Some(events);
    }

    pub fn events(&self) -> Option<&Events> {
        self.events.as_ref()
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    pub fn app_instance_id(&self) -> &str {
        &self.app_instance_id
    }

    pub fn current_boot_id(&self) -> &str {
        &self.boot_id
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn config(&self) -> &Arc<ConfigStore> {
        &self.config
    }

    /// Close this instance's row. Called on a clean shutdown; an instance whose
    /// `ended_at` is still NULL at the next boot is by definition a crash.
    pub async fn close_instance(&self) -> Result<()> {
        let id = self.app_instance_id.clone();
        let at = now_ms();
        self.db
            .write_durable(move |conn| {
                dao::app_instance::close(conn, &id, at)?;
                Ok(())
            })
            .await
    }

    /// `SupervisorApi::plan_spawn` with the row's guard handed to the caller
    /// instead of kept until the exit is reported (the registry's own tests
    /// and fixtures that drive the row by hand). The same checks, in the same
    /// order: the A13.1/A15.1 ids, the boot gate, the §9.3 allowlist,
    /// memory-only mode; then the durable row.
    pub async fn plan_guarded(&self, req: SpawnRequest) -> std::result::Result<SpawnGuard, IpcError> {
        // A13.1: a malformed request is refused before anything waits or is
        // planned.
        validate_spawn_ids(req.kind, req.generation, req.turn_id.as_deref())?;
        self.gate.wait().await?;
        // The same check `plan_spawn_now` applies again (A12.8).
        if !crate::supervise::spawn::is_allowed(Path::new(req.argv0.as_str()), req.kind) {
            return Err(IpcError::refused(REASON_NOT_ALLOWLISTED));
        }
        if self.memory_only.is_active() || self.db.is_memory_only() {
            return Err(IpcError::disk_full());
        }
        let mut argv = Vec::with_capacity(req.args.len() + 1);
        argv.push(req.argv0.as_str().to_string());
        argv.extend(req.args.iter().cloned());
        let cwd = req
            .cwd
            .as_ref()
            .map(|c| PathBuf::from(c.as_str()))
            .unwrap_or_else(|| PathBuf::from("/"));
        // A39.1(a): the shared classifier over the whole argv; a hosted pane
        // (`/bin/sh -c <command>`) records the vendor its command runs.
        let vendor = crate::supervise::procscan::vendor_of_argv(req.argv0.as_str(), &argv).or_else(|| {
            if req.kind != crate::contract::process::ProcKind::Pane {
                return None;
            }
            let command = req.args.iter().skip_while(|a| a.as_str() != "-c").nth(1)?;
            crate::agents::classify::vendor_of_command(command).map(crate::contract::common::Vendor::as_str)
        });
        let mut plan = SpawnPlan::new(req.kind.into(), argv, cwd);
        if let Some(vendor) = vendor {
            plan = plan.vendor(vendor);
        }
        if let Some(uid) = &req.parent_uid {
            plan = plan.parent(uid.as_str());
        }
        // A13.1: the generation and the turn id live in the row, so the
        // dead-generation sweep (`live_rows_of_generation`) finds the spawn.
        if let Some(generation) = req.generation {
            plan = plan.generation(generation);
        }
        if let Some(turn_id) = &req.turn_id {
            plan = plan.run(turn_id.clone());
        }
        self.plan_spawn_now(plan).await.map_err(|e| IpcError::from(&e))
    }

    /// The registry's own unit tests plan rows directly (gate, allowlist,
    /// row); everything else plans through `SupervisorApi::plan_spawn`
    /// (A12.8: the legacy public `plan_spawn` is gone, A22.3).
    #[cfg(test)]
    async fn plan_spawn(&self, plan: SpawnPlan) -> Result<SpawnGuard> {
        if plan.argv.is_empty() {
            return Err(GildeError::spawn("a spawn plan needs at least argv[0]"));
        }
        // §2.4 B6: nothing spawns before the boot sweep opened the gate.
        if self.gate.wait_for(crate::lifecycle::spawn_gate::GATE_WAIT).await.is_err() {
            return Err(GildeError::Refused {
                reason: crate::lifecycle::spawn_gate::REASON_BOOT_SWEEP.to_string(),
            });
        }
        self.plan_spawn_now(plan).await
    }

    /// Register a process that is **already running** (the reaper's adoption
    /// of an untracked orphan): the same row as `SupervisorApi::plan_spawn`,
    /// but no gate wait and no allowlist check, because nothing is launched —
    /// refusing would only leave a live process unsupervised.
    pub async fn plan_existing(&self, plan: SpawnPlan) -> Result<SpawnGuard> {
        if plan.argv.is_empty() {
            return Err(GildeError::spawn("a spawn plan needs at least argv[0]"));
        }
        // A41.3: observing a process that already runs is a registry write,
        // not a spawn, so it never waits on the spawn gate — the boot sweep
        // registers orphans while the gate it opens is still closed.
        self.write_plan(plan).await
    }

    /// `SupervisorApi::plan_spawn` after the gate: the §9.3 allowlist, then
    /// the row (DESIGN §8-3: the INSERT and the `argv_sig` correction go in
    /// one transaction under `synchronous=FULL`, so the row that reaches the
    /// disk is either complete or absent).
    async fn plan_spawn_now(&self, plan: SpawnPlan) -> Result<SpawnGuard> {
        check_allowlist(&plan)?;
        check_plan_ids(&plan)?;
        self.write_plan(plan).await
    }

    /// Write the planned row (memory-only mode refuses).
    async fn write_plan(&self, plan: SpawnPlan) -> Result<SpawnGuard> {
        if plan.argv.is_empty() {
            return Err(GildeError::spawn("a spawn plan needs at least argv[0]"));
        }
        // §2.4: a spawn needs a durable row; memory-only mode has none.
        if self.memory_only.is_active() || self.db.is_memory_only() {
            return Err(GildeError::Io(std::io::Error::from_raw_os_error(libc::ENOSPC)));
        }

        let spawn_id = Uuid::now_v7().to_string();
        // Fence 2 hashes the *unmasked* argv; the stored `argv` is redacted
        // (contract §9.7). See the module header for why the two must disagree.
        let raw_sig = argv_signature(&plan.argv);
        let masked = mask_argv_vec(&plan.argv);
        let parent_uid = plan.parent_uid.clone();

        let planned = NewPlannedProc {
            spawn_id: spawn_id.clone(),
            app_instance: self.app_instance_id.clone(),
            generation: plan.generation,
            kind: plan.kind,
            vendor: plan.vendor.clone(),
            session_id: plan.session_id.clone(),
            run_id: plan.run_id.clone(),
            argv: masked,
            cwd: plan.cwd.to_string_lossy().into_owned(),
            config_dir: plan
                .config_dir
                .as_ref()
                .map(|d| d.to_string_lossy().into_owned()),
            worktree_id: plan.worktree_id.clone(),
            keep_alive: false,
            planned_at: now_ms(),
            note: None,
        };

        let row_id = spawn_id.clone();
        let written = self
            .db
            .write_durable(move |conn| {
                let tx = conn.unchecked_transaction()?;
                dao::proc::insert_planned(&tx, &planned)?;
                tx.execute(
                    "UPDATE proc_registry SET argv_sig = ?2 WHERE spawn_id = ?1",
                    rusqlite::params![row_id, raw_sig],
                )?;
                if let Some(uid) = &parent_uid {
                    dao::proc::set_parent_uid(&tx, &row_id, uid)?;
                }
                tx.commit()?;
                Ok(())
            })
            .await;
        if let Err(err) = written {
            if self.note_write_failure(&err) {
                return Err(GildeError::Io(std::io::Error::from_raw_os_error(libc::ENOSPC)));
            }
            return Err(err);
        }

        tracing::debug!(
            target: "supervise",
            spawn_id = %spawn_id,
            kind = %plan.kind,
            cwd = %plan.cwd.display(),
            "spawn planned"
        );

        Ok(SpawnGuard::new(self.db.clone(), spawn_id))
    }

    /// A synchronous plan for the `ps`/`lsof` fallbacks and the PATH probe
    /// (`probe` rows, contract §3.8). No gate: the boot sweep's own identity
    /// checks may need it before the gate opens.
    pub fn plan_spawn_blocking(&self, plan: SpawnPlan) -> Result<SpawnGuard> {
        if plan.argv.is_empty() {
            return Err(GildeError::spawn("a spawn plan needs at least argv[0]"));
        }
        check_allowlist(&plan)?;
        if self.memory_only.is_active() || self.db.is_memory_only() {
            return Err(GildeError::Io(std::io::Error::from_raw_os_error(libc::ENOSPC)));
        }
        let spawn_id = Uuid::now_v7().to_string();
        let raw_sig = argv_signature(&plan.argv);
        let planned = NewPlannedProc {
            spawn_id: spawn_id.clone(),
            app_instance: self.app_instance_id.clone(),
            generation: plan.generation,
            kind: plan.kind,
            vendor: plan.vendor.clone(),
            session_id: plan.session_id.clone(),
            run_id: plan.run_id.clone(),
            argv: mask_argv_vec(&plan.argv),
            cwd: plan.cwd.to_string_lossy().into_owned(),
            config_dir: None,
            worktree_id: None,
            keep_alive: false,
            planned_at: now_ms(),
            note: None,
        };
        let row_id = spawn_id.clone();
        self.db.blocking(move |conn| {
            conn.pragma_update(None, "synchronous", "FULL")?;
            let result = (|| -> Result<()> {
                let tx = conn.unchecked_transaction()?;
                dao::proc::insert_planned(&tx, &planned)?;
                tx.execute(
                    "UPDATE proc_registry SET argv_sig = ?2 WHERE spawn_id = ?1",
                    rusqlite::params![row_id, raw_sig],
                )?;
                tx.commit()?;
                Ok(())
            })();
            let _ = conn.pragma_update(None, "synchronous", "NORMAL");
            result
        })?;
        Ok(SpawnGuard::new(self.db.clone(), spawn_id))
    }

    /// Read one registry row back.
    pub async fn row(&self, spawn_id: &str) -> Result<Option<ProcRow>> {
        let spawn_id = spawn_id.to_string();
        self.db
            .read(move |conn| dao::proc::get(conn, &spawn_id))
            .await
    }

    /// The environment a child gets — an **allowlist**, never an inheritance
    /// (DESIGN §6-1). Anything not listed here does not reach the child.
    ///
    /// Order of precedence, lowest first:
    ///
    /// 1. the base allowlist (`PATH`, `HOME`, locale, terminal identity, …);
    /// 2. proxy variables;
    /// 3. `agents.envPassthrough` — the user's escape hatch. It **adds** keys
    ///    the allowlist does not carry and never rewrites one that is already
    ///    composed, so the result stays a function of this call's arguments
    ///    rather than of whatever the app happened to inherit. (The default
    ///    config lists `SSH_AUTH_SOCK` and the proxies, which the base already
    ///    covers; those entries are therefore no-ops rather than a silent
    ///    override of the caller's [`EnvBase`].)
    /// 4. the account directory variable for `account`'s vendor;
    /// 5. `GILDE_SPAWN_ID` / `GILDE_APP_INSTANCE`, which nothing may override.
    pub fn compose_env(
        &self,
        base: EnvBase,
        account: Option<&AccountSlot>,
        marker: &str,
    ) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();

        env.insert("PATH".to_string(), base.path);
        env.insert("HOME".to_string(), base.home.to_string_lossy().into_owned());

        let user = base.user.clone().or_else(|| base.logname.clone());
        let logname = base.logname.clone().or_else(|| base.user.clone());
        if let Some(user) = user {
            env.insert("USER".to_string(), user);
        }
        if let Some(logname) = logname {
            env.insert("LOGNAME".to_string(), logname);
        }
        if let Some(shell) = base.shell {
            env.insert("SHELL".to_string(), shell);
        }
        if let Some(tmpdir) = base.tmpdir {
            env.insert("TMPDIR".to_string(), tmpdir);
        }

        // Locale: a Finder launch has none, and a CLI without one breaks UTF-8.
        match (base.lang, base.lc_all) {
            (None, None) => {
                env.insert("LANG".to_string(), DEFAULT_LANG.to_string());
            }
            (lang, lc_all) => {
                if let Some(lang) = lang {
                    env.insert("LANG".to_string(), lang);
                }
                if let Some(lc_all) = lc_all {
                    env.insert("LC_ALL".to_string(), lc_all);
                }
            }
        }

        env.insert("TERM".to_string(), TERM_VALUE.to_string());
        env.insert("COLORTERM".to_string(), COLORTERM_VALUE.to_string());
        env.insert("TERM_PROGRAM".to_string(), TERM_PROGRAM_VALUE.to_string());

        if let Some(sock) = base.ssh_auth_sock {
            env.insert("SSH_AUTH_SOCK".to_string(), sock);
        }
        for (key, value) in base.proxies {
            env.insert(key, value);
        }

        let reserved: BTreeSet<&str> = RESERVED_ENV_KEYS.iter().copied().collect();
        for key in self.config.get().agents.env_passthrough {
            if reserved.contains(key.as_str()) {
                tracing::warn!(
                    target: "supervise",
                    key = %key,
                    "ignoring envPassthrough entry: it is one of this app's identity markers"
                );
                continue;
            }
            if env.contains_key(&key) {
                continue;
            }
            if let Ok(value) = std::env::var(&key) {
                env.insert(key, value);
            }
        }

        if let Some(account) = account {
            // The ambient slot has no directory of its own, and a vendor
            // without directory-based switching has no key to set. Neither
            // is an error.
            if let (Some(key), Some(dir)) = (account_env_key(&account.vendor), &account.config_dir) {
                env.insert(key.to_string(), dir.clone());
            }
        }

        env.insert(ENV_SPAWN_ID.to_string(), marker.to_string());
        env.insert(ENV_APP_INSTANCE.to_string(), self.app_instance_id.clone());

        env
    }
}

// ---------------------------------------------------------------------------
// argv masking
// ---------------------------------------------------------------------------

/// The stored form of an argv: the one redaction of contract §9.7
/// ([`crate::supervise::redact::redact_argv`]). Kept under its old name for
/// the call sites that already use it.
pub fn mask_argv_vec(argv: &[String]) -> Vec<String> {
    crate::supervise::redact::redact_argv(argv)
}

// ---------------------------------------------------------------------------
// discovery::OwnProcesses (A11.3)
// ---------------------------------------------------------------------------

/// Gilde's own processes as the registry knows them (A11.3): discovery asks
/// before it classifies, so a process Gilde started is never "external".
/// Called on the discovery thread (a plain thread, never a runtime worker),
/// so the registry is read synchronously; a read error is "not ours".
impl crate::discovery::OwnProcesses for Supervisor {
    fn lookup(&self, pid: i32, start_sec: i64) -> Option<crate::discovery::OwnMatch> {
        if pid <= 0 {
            return None;
        }
        let rows = self
            .db
            .blocking(|conn| dao::proc::by_pid(conn, i64::from(pid)))
            .ok()?;
        rows.into_iter()
            .find(|r| r.proc_start_ts.is_some_and(|s| (s - start_sec).abs() <= 1))
            .map(|r| crate::discovery::OwnMatch {
                this_instance: r.app_instance == self.app_instance_id,
                spawn_id: r.spawn_id,
            })
    }

    fn match_planned(&self, argv: &[String], cwd: Option<&str>) -> Option<String> {
        if argv.is_empty() {
            return None;
        }
        // The row keeps the signature of the unmasked argv (fence 2), so the
        // raw argv discovery read matches it exactly; the cwd must agree when
        // discovery knows it.
        let sig = crate::supervise::identity::argv_sig_of(argv);
        let me = self.app_instance_id.clone();
        let rows = self
            .db
            .blocking(|conn| dao::proc::planned_before(conn, &me, now_ms().saturating_add(1)))
            .ok()?;
        rows.into_iter()
            .rev()
            .find(|r| {
                r.state == ProcState::Planned
                    && r.pid.is_none()
                    && r.argv_sig == sig
                    && cwd.is_none_or(|c| c == r.cwd)
            })
            .map(|r| r.spawn_id)
    }

    fn hosted_leads(&self) -> Vec<crate::discovery::HostedLead> {
        let mut hosted = self.hosted.lock();
        hosted.retain(|h| crate::supervise::spawn::is_alive(h.pid));
        hosted.clone()
    }
}

// ---------------------------------------------------------------------------
// SupervisorApi (contract §2.1, §3.8, §9.3, §9.8)
// ---------------------------------------------------------------------------

/// The refusal reason of a launch outside the §9.3 allowlist.
pub const REASON_NOT_ALLOWLISTED: &str = "notAllowlisted";

/// §9.3 enforcement 1 for every launch the supervisor plans:
/// `REFUSED{notAllowlisted}` when `argv[0]` is not allowlisted for the plan's
/// kind. Shared by the blocking probe path and `SupervisorApi::plan_spawn`
/// (A12.8).
fn check_allowlist(plan: &SpawnPlan) -> Result<()> {
    let Some(argv0) = plan.argv.first() else {
        return Err(GildeError::spawn("a spawn plan needs at least argv[0]"));
    };
    if crate::supervise::spawn::is_allowed(Path::new(argv0), plan.kind) {
        Ok(())
    } else {
        tracing::warn!(target: "supervise", kind = %plan.kind, "launch refused: not allowlisted");
        Err(GildeError::Refused {
            reason: REASON_NOT_ALLOWLISTED.to_string(),
        })
    }
}

/// The longest turn id a `Turn` spawn may carry (A13.1).
pub const TURN_ID_MAX_BYTES: usize = 128;

/// A13.1 as amended by A15.1: `generation` is required (≥ 1) for `Sidecar`
/// and `Turn`, optional (≥ 1 when present) for `Vendorcli` — present exactly
/// when the sidecar spawns the probe, so the dead-generation sweep finds a
/// probe orphaned by a sidecar crash — and reserved for every other kind;
/// `turn_id` is required for `Turn` (≤ 128 bytes, no control characters) and
/// reserved otherwise. A violation is `INVALID_ARGUMENT{field:
/// "generation"|"turnId", rule}` (`range` for a `Vendorcli` generation < 1).
pub fn validate_spawn_ids(
    kind: crate::contract::process::ProcKind,
    generation: Option<i64>,
    turn_id: Option<&str>,
) -> std::result::Result<(), IpcError> {
    use crate::contract::process::ProcKind as K;
    let needs_generation = matches!(kind, K::Sidecar | K::Turn);
    let may_have_generation = kind == K::Vendorcli;
    match generation {
        Some(g) if needs_generation && g >= 1 => {}
        Some(_) if needs_generation => return Err(IpcError::invalid_argument("generation", "required")),
        None if needs_generation => return Err(IpcError::invalid_argument("generation", "required")),
        Some(g) if may_have_generation && g >= 1 => {}
        Some(_) if may_have_generation => return Err(IpcError::invalid_argument("generation", "range")),
        Some(_) => return Err(IpcError::invalid_argument("generation", "reserved")),
        None => {}
    }
    match (kind == K::Turn, turn_id) {
        (true, None | Some("")) => return Err(IpcError::invalid_argument("turnId", "required")),
        (true, Some(id)) if id.len() > TURN_ID_MAX_BYTES => {
            return Err(IpcError::invalid_argument("turnId", "maxLength"))
        }
        (true, Some(id)) if id.chars().any(char::is_control) => {
            return Err(IpcError::invalid_argument("turnId", "control"))
        }
        (false, Some(_)) => return Err(IpcError::invalid_argument("turnId", "reserved")),
        _ => {}
    }
    Ok(())
}

/// The required half of [`validate_spawn_ids`] for every plan (the legacy
/// path included): a `sidecar` or `turn` row without a generation (or a
/// `turn` row without a valid turn id) would escape the dead-generation
/// sweep. Legacy sidecar code still stamps generations on other kinds, so
/// "reserved" is enforced on the `SupervisorApi` path only (A13.1).
fn check_plan_ids(plan: &SpawnPlan) -> Result<()> {
    use crate::contract::process::ProcKind as K;
    let kind = match plan.kind {
        ProcKind::Sidecar => K::Sidecar,
        ProcKind::Turn => K::Turn,
        _ => return Ok(()),
    };
    let generation = (plan.generation != 0).then_some(plan.generation);
    let turn_id = if kind == K::Turn { plan.run_id.as_deref() } else { None };
    validate_spawn_ids(kind, generation, turn_id).map_err(|e| {
        GildeError::spawn(format!(
            "invalid {} ({})",
            e.params.get("field").and_then(|v| v.as_str()).unwrap_or("?"),
            e.params.get("rule").and_then(|v| v.as_str()).unwrap_or("?"),
        ))
    })
}

/// `x_<pid>_<start seconds>` → `(pid, start)`.
pub fn parse_process_key(key: &ProcessKey) -> Option<(i32, i64)> {
    let rest = key.as_str().strip_prefix("x_")?;
    let (pid, start) = rest.split_once('_')?;
    Some((pid.parse().ok()?, start.parse().ok()?))
}

impl Supervisor {
    /// The fenced signal behind [`SupervisorApi::signal`] and `health_stop`.
    ///
    /// A key that names a live registry row (same pid, start time ±1 s) goes
    /// through `kill_tree` and its three identity fences; any other process is
    /// signalled only if its kernel start time still equals the key's, through
    /// the bare-pid path (no group signal for a process we did not create).
    /// Vendor CLIs are never escalated to SIGKILL (`supervise::kill`).
    pub async fn signal_key(
        &self,
        target: &ProcessKey,
        plan: SignalPlan,
    ) -> std::result::Result<SignalOutcome, IpcError> {
        let Some((pid, start)) = parse_process_key(target) else {
            return Err(IpcError::not_found("process"));
        };
        // A42.1: a `SignalPlan` never escalates ("the plan type has no
        // SIGKILL", C5): fenced HUP/TERM, wait the grace, report survivors.
        let opts = match plan {
            SignalPlan::Term { grace_ms } => crate::supervise::kill::KillOpts::default()
                .grace(std::time::Duration::from_millis(u64::from(grace_ms))),
            SignalPlan::HupThenTerm { grace_ms } => crate::supervise::kill::KillOpts::default()
                .grace(std::time::Duration::from_millis(u64::from(grace_ms)))
                .hup_first(true),
        }
        .escalate(false);
        let rows = self
            .db
            .read(move |conn| dao::proc::by_pid(conn, i64::from(pid)))
            .await
            .map_err(|e| IpcError::from(&e))?;
        let row = rows.into_iter().find(|r| {
            r.state.is_live() && r.proc_start_ts.is_some_and(|s| (s - start).abs() <= 1)
        });
        let report = match row {
            Some(row) => crate::supervise::kill::kill_tree(self, &row.spawn_id, opts).await,
            // A42.1: a pid with no Gilde row is signalled alone.
            None => match start_time_of(pid) {
                Some(live) if (live - start).abs() <= 1 => {
                    crate::supervise::kill::kill_single_pid(self, pid, opts).await
                }
                Some(_) => return Err(IpcError::refused("startTimeChanged")),
                None => {
                    return Ok(SignalOutcome {
                        exited: true,
                        survivor: false,
                    })
                }
            },
        };
        if let Some(reason) = report.refused_reason.as_deref() {
            if reason == crate::supervise::kill::reason::GONE {
                return Ok(SignalOutcome {
                    exited: true,
                    survivor: false,
                });
            }
            return Err(IpcError::refused(reason));
        }
        Ok(SignalOutcome {
            exited: report.survivors.is_empty(),
            survivor: !report.survivors.is_empty(),
        })
    }
}

#[async_trait::async_trait]
impl SupervisorApi for Supervisor {
    async fn spawn_gate(&self) -> std::result::Result<(), IpcError> {
        self.gate.wait().await
    }

    async fn plan_spawn(&self, req: SpawnRequest) -> std::result::Result<SpawnTicket, IpcError> {
        let guard = self.plan_guarded(req).await?;
        let ticket = SpawnTicket {
            spawn_id: guard.spawn_id().to_string(),
            env_marker: guard.env_marker().to_string(),
        };
        self.tickets.lock().insert(ticket.spawn_id.clone(), guard);
        Ok(ticket)
    }

    async fn mark_running(&self, ticket: &SpawnTicket, pid: i32) -> std::result::Result<(), IpcError> {
        let pgid = crate::supervise::spawn::pgid_of(pid).unwrap_or(pid);
        let tickets = self.tickets.lock();
        match tickets.get(&ticket.spawn_id) {
            Some(guard) => guard.mark_running(pid, pgid).map_err(|e| IpcError::from(&e)),
            None => Err(IpcError::not_found("spawn")),
        }
    }

    async fn mark_exited(&self, ticket: &SpawnTicket, exit: ExitInfo) {
        let guard = self.tickets.lock().remove(&ticket.spawn_id);
        let result = match guard {
            Some(guard) => guard.mark_exited(exit.code, exit.signal),
            None => {
                let id = ticket.spawn_id.clone();
                let at = now_ms();
                self.db
                    .write(move |conn| {
                        dao::proc::mark_exited(
                            conn,
                            &id,
                            exit.code.map(i64::from),
                            exit.signal.map(i64::from),
                            at,
                        )?;
                        Ok(())
                    })
                    .await
            }
        };
        if let Err(err) = result {
            tracing::warn!(target: "supervise", spawn_id = %ticket.spawn_id, error = %err, "could not record an exit");
        }
    }

    async fn signal(&self, target: &ProcessKey, plan: SignalPlan) -> std::result::Result<SignalOutcome, IpcError> {
        self.signal_key(target, plan).await
    }

    fn is_vendor_cli(&self, exec_path: &str) -> bool {
        crate::supervise::procscan::vendor_of_argv::<&str>(exec_path, &[]).is_some()
    }

    fn redact_argv(&self, argv: &[String]) -> Vec<String> {
        crate::supervise::redact::redact_argv(argv)
    }

    fn memory_only(&self) -> Option<String> {
        self.memory_only
            .reason_str()
            .or_else(|| self.db.is_memory_only().then(|| "unwritable".to_string()))
    }

    fn last_scan(&self) -> Option<ProcSnapshot> {
        self.scans.latest().map(|s| (*s).clone())
    }
}

/// [`crate::supervise::procscan::ProbeRegistrar`] over the registry: the
/// `ps`/`lsof` fallbacks become `probe` rows (contract §3.8).
pub struct ProbeHook {
    supervisor: Arc<Supervisor>,
}

impl ProbeHook {
    pub fn new(supervisor: Arc<Supervisor>) -> ProbeHook {
        ProbeHook { supervisor }
    }
}

struct ProbeGuardRow(SpawnGuard);

impl crate::supervise::procscan::ProbeRow for ProbeGuardRow {
    fn env_marker(&self) -> String {
        self.0.env_marker().to_string()
    }

    fn running(&self, pid: i32, pgid: i32) {
        let _ = self.0.mark_running(pid, pgid);
    }

    fn exited(&self, code: Option<i32>, signal: Option<i32>) {
        let _ = self.0.mark_exited(code, signal);
    }
}

impl crate::supervise::procscan::ProbeRegistrar for ProbeHook {
    fn plan(&self, argv: &[String]) -> Option<Box<dyn crate::supervise::procscan::ProbeRow>> {
        let plan = SpawnPlan::new(ProcKind::Probe, argv.to_vec(), "/");
        match self.supervisor.plan_spawn_blocking(plan) {
            Ok(guard) => Some(Box::new(ProbeGuardRow(guard))),
            Err(err) => {
                tracing::debug!(target: "supervise", error = %err, "probe row not written; the fallback does not run");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::AccountSlot;
    use crate::store::models::AccountSource;

    struct Fixture {
        supervisor: Supervisor,
        db: Db,
        _dir: tempfile::TempDir,
    }

    async fn fixture() -> Fixture {
        // The argv[0]s these tests plan, allowlisted for their kinds (§9.3).
        for (path, kind) in [
            ("/bin/agent", ProcKind::Pty),
            ("/bin/echo", ProcKind::Probe),
            ("/usr/bin/git", ProcKind::Git),
            ("/bin/false", ProcKind::Probe),
        ] {
            crate::supervise::spawn::allow_executable(Path::new(path), &[kind]);
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::open_in_memory().expect("db");
        let config = Arc::new(ConfigStore::open_at(&dir.path().join("config.json")).expect("config"));
        let supervisor = Supervisor::open_instance(&db, config, "0.0.0-test")
            .await
            .expect("open_instance");
        Fixture {
            supervisor,
            db,
            _dir: dir,
        }
    }

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|p| (*p).to_string()).collect()
    }

    async fn row_of(db: &Db, spawn_id: &str) -> ProcRow {
        let id = spawn_id.to_string();
        db.read(move |conn| dao::proc::get(conn, &id))
            .await
            .expect("read")
            .expect("row exists")
    }

    #[tokio::test]
    async fn boot_id_is_readable_on_this_machine() {
        let boot_id = Supervisor::boot_id().expect("kern.bootsessionuuid");
        assert!(!boot_id.is_empty(), "boot id must not be empty");
        #[cfg(target_os = "macos")]
        {
            assert_eq!(boot_id.len(), 36, "kern.bootsessionuuid is a UUID: {boot_id}");
            assert_eq!(
                boot_id.matches('-').count(),
                4,
                "kern.bootsessionuuid is a UUID: {boot_id}"
            );
        }
    }

    #[tokio::test]
    async fn the_app_instance_row_records_the_boot_id_and_our_pid() {
        let f = fixture().await;
        let id = f.supervisor.app_instance_id().to_string();
        let row = f
            .db
            .read(move |conn| dao::app_instance::get(conn, &id))
            .await
            .expect("read")
            .expect("row exists");

        assert_eq!(row.boot_id, Supervisor::boot_id().expect("boot id"));
        assert_eq!(row.pid, std::process::id() as i64);
        assert_eq!(row.version, "0.0.0-test");
        assert!(row.ended_at.is_none(), "a fresh instance is still open");

        f.supervisor.close_instance().await.expect("close");
        let id = f.supervisor.app_instance_id().to_string();
        let row = f
            .db
            .read(move |conn| dao::app_instance::get(conn, &id))
            .await
            .expect("read")
            .expect("row exists");
        assert!(row.ended_at.is_some(), "a closed instance is not a crash");
    }

    #[tokio::test]
    async fn plan_spawn_writes_a_planned_row_before_the_process_exists() {
        let f = fixture().await;
        let plan = SpawnPlan::new(
            ProcKind::Pty,
            argv(&["/bin/agent", "--token", "sk-abcdefghijklmnopqrst", "chat"]),
            "/tmp/work",
        )
        .vendor("claude")
        .session("session-1")
        .worktree("wt-1");

        let guard = f.supervisor.plan_spawn(plan.clone()).await.expect("plan");
        let row = row_of(&f.db, guard.spawn_id()).await;

        assert_eq!(row.state, ProcState::Planned);
        assert!(row.pid.is_none(), "a planned row has no pid yet");
        assert_eq!(row.env_marker, guard.spawn_id());
        assert_eq!(row.env_marker, guard.env_marker());
        assert_eq!(row.cwd, "/tmp/work");
        assert_eq!(row.kind, ProcKind::Pty);
        assert_eq!(row.vendor.as_deref(), Some("claude"));
        assert_eq!(row.session_id.as_deref(), Some("session-1"));
        assert_eq!(row.worktree_id.as_deref(), Some("wt-1"));
        assert_eq!(row.app_instance, f.supervisor.app_instance_id());
        assert!(row.planned_at > 0);

        // The secret is not in the stored argv …
        assert!(
            !row.argv.contains("sk-abcdefghijklmnopqrst"),
            "argv is stored masked: {}",
            row.argv
        );
        assert!(row.argv.contains(crate::supervise::redact::REDACTED));
        // … but the signature is over the *raw* argv, or fence 2 could never
        // match the live process.
        assert_eq!(row.argv_sig, argv_signature(&plan.argv));

        guard.abandon("test");
    }

    #[tokio::test]
    async fn plan_spawn_refuses_an_empty_argv() {
        let f = fixture().await;
        let err = f
            .supervisor
            .plan_spawn(SpawnPlan::new(ProcKind::Git, Vec::new(), "/tmp"))
            .await
            .expect_err("an argv with no argv[0] is not a plan");
        assert_eq!(err.code(), "SPAWN");
    }

    #[tokio::test]
    async fn mark_running_records_the_kernel_start_time() {
        let f = fixture().await;
        let guard = f
            .supervisor
            .plan_spawn(SpawnPlan::new(
                ProcKind::Probe,
                argv(&["/bin/echo", "hi"]),
                "/tmp",
            ))
            .await
            .expect("plan");

        // Our own pid: alive, ours, and with a start time libproc will report.
        let me = std::process::id() as i32;
        guard.mark_running(me, me).expect("mark_running");

        let row = row_of(&f.db, guard.spawn_id()).await;
        assert_eq!(row.state, ProcState::Running);
        assert_eq!(row.pid, Some(me as i64));
        assert_eq!(row.pgid, Some(me as i64));
        assert!(row.started_at.is_some());
        let recorded = row.proc_start_ts.expect("kernel start time recorded");
        let expected = start_time_of(me).expect("libproc knows our start time");
        assert_eq!(recorded, expected);

        guard.mark_exited(Some(0), None).expect("mark_exited");
        let row = row_of(&f.db, guard.spawn_id()).await;
        assert_eq!(row.state, ProcState::Exited);
        assert_eq!(row.exit_code, Some(0));
        assert!(row.exited_at.is_some());
    }

    #[tokio::test]
    async fn a_guard_dropped_before_mark_running_marks_the_row_lost() {
        let f = fixture().await;
        let guard = f
            .supervisor
            .plan_spawn(SpawnPlan::new(
                ProcKind::Git,
                argv(&["/usr/bin/git", "status"]),
                "/tmp",
            ))
            .await
            .expect("plan");
        let spawn_id = guard.spawn_id().to_string();

        drop(guard);

        let row = row_of(&f.db, &spawn_id).await;
        assert_eq!(row.state, ProcState::Lost);
        assert_eq!(row.note.as_deref(), Some(NOTE_SPAWN_ABORTED));
    }

    #[tokio::test]
    async fn a_guard_dropped_after_mark_running_keeps_its_state() {
        let f = fixture().await;
        let guard = f
            .supervisor
            .plan_spawn(SpawnPlan::new(
                ProcKind::Git,
                argv(&["/usr/bin/git", "status"]),
                "/tmp",
            ))
            .await
            .expect("plan");
        let spawn_id = guard.spawn_id().to_string();
        let me = std::process::id() as i32;
        guard.mark_running(me, me).expect("mark_running");

        drop(guard);

        let row = row_of(&f.db, &spawn_id).await;
        assert_eq!(
            row.state,
            ProcState::Running,
            "a running row must not be clobbered by Drop"
        );
        assert_eq!(row.note, None);
    }

    #[tokio::test]
    async fn abandon_records_its_own_reason() {
        let f = fixture().await;
        let guard = f
            .supervisor
            .plan_spawn(SpawnPlan::new(
                ProcKind::Probe,
                argv(&["/bin/false"]),
                "/tmp",
            ))
            .await
            .expect("plan");
        let spawn_id = guard.spawn_id().to_string();
        guard.abandon("binary not executable");

        let row = row_of(&f.db, &spawn_id).await;
        assert_eq!(row.state, ProcState::Lost);
        assert_eq!(row.note.as_deref(), Some("binary not executable"));
    }

    // -- compose_env ------------------------------------------------------

    fn full_base() -> EnvBase {
        let mut base = EnvBase::new("/opt/homebrew/bin:/usr/bin", "/Users/tester");
        base.user = Some("tester".into());
        base.logname = Some("tester".into());
        base.shell = Some("/bin/zsh".into());
        base.tmpdir = Some("/var/folders/tmp/".into());
        base.lang = Some("ko_KR.UTF-8".into());
        base.ssh_auth_sock = Some("/private/tmp/ssh-agent.sock".into());
        base.proxies
            .insert("HTTPS_PROXY".into(), "http://127.0.0.1:8080".into());
        base
    }

    fn account(vendor: &str, dir: Option<&str>) -> AccountSlot {
        AccountSlot {
            id: "acct-1".into(),
            vendor: vendor.into(),
            label: "work".into(),
            config_dir: dir.map(|d| d.to_string()),
            model_hint: None,
            source: AccountSource::Manual,
            enabled: true,
            is_default: false,
            logged_in: true,
            last_seen_at: None,
            last_quota_at: None,
        }
    }

    #[tokio::test]
    async fn compose_env_builds_exactly_the_allowlist_and_nothing_else() {
        let f = fixture().await;
        // The shipped default lists SSH_AUTH_SOCK and the proxies; emptying it
        // makes this assertion about the allowlist itself rather than about
        // whatever this particular machine exports.
        f.supervisor
            .config()
            .patch(serde_json::json!({ "agents": { "envPassthrough": [] } }))
            .expect("patch");
        let env = f
            .supervisor
            .compose_env(full_base(), Some(&account("claude", Some("/Users/tester/.claude-work"))), "spawn-42");

        let keys: Vec<&str> = env.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec![
                // Sorted: `compose_env` returns a `BTreeMap`, so `GILDE_*` sits
                // between `COLORTERM` and `HOME`.
                "CLAUDE_CONFIG_DIR",
                "COLORTERM",
                "GILDE_APP_INSTANCE",
                "GILDE_SPAWN_ID",
                "HOME",
                "HTTPS_PROXY",
                "LANG",
                "LOGNAME",
                "PATH",
                "SHELL",
                "SSH_AUTH_SOCK",
                "TERM",
                "TERM_PROGRAM",
                "TMPDIR",
                "USER",
            ],
            "the environment is an allowlist, not an inheritance"
        );

        assert_eq!(env["PATH"], "/opt/homebrew/bin:/usr/bin");
        assert_eq!(env["HOME"], "/Users/tester");
        assert_eq!(env["USER"], "tester");
        assert_eq!(env["LOGNAME"], "tester");
        assert_eq!(env["SHELL"], "/bin/zsh");
        assert_eq!(env["TMPDIR"], "/var/folders/tmp/");
        assert_eq!(env["LANG"], "ko_KR.UTF-8");
        assert_eq!(env["TERM"], TERM_VALUE);
        assert_eq!(env["COLORTERM"], COLORTERM_VALUE);
        assert_eq!(env["TERM_PROGRAM"], TERM_PROGRAM_VALUE);
        assert_eq!(env["SSH_AUTH_SOCK"], "/private/tmp/ssh-agent.sock");
        assert_eq!(env["HTTPS_PROXY"], "http://127.0.0.1:8080");
        assert_eq!(env["CLAUDE_CONFIG_DIR"], "/Users/tester/.claude-work");
        assert_eq!(env["GILDE_SPAWN_ID"], "spawn-42");
        assert_eq!(env["GILDE_APP_INSTANCE"], f.supervisor.app_instance_id());
    }

    #[tokio::test]
    async fn compose_env_falls_back_to_a_utf8_locale_when_there_is_none() {
        let f = fixture().await;
        let base = EnvBase::new("/usr/bin", "/Users/tester");
        let env = f.supervisor.compose_env(base, None, "m");
        assert_eq!(env["LANG"], DEFAULT_LANG);
        assert!(!env.contains_key("LC_ALL"));

        let mut base = EnvBase::new("/usr/bin", "/Users/tester");
        base.lc_all = Some("C".into());
        let env = f.supervisor.compose_env(base, None, "m");
        assert_eq!(env["LC_ALL"], "C");
        assert!(
            !env.contains_key("LANG"),
            "an explicit LC_ALL is not silently paired with a default LANG"
        );
    }

    #[tokio::test]
    async fn compose_env_maps_each_vendor_to_its_account_variable() {
        let f = fixture().await;
        for (vendor, key) in [
            ("claude", "CLAUDE_CONFIG_DIR"),
            ("codex", "CODEX_HOME"),
            ("agy", "AGY_HOME"),
        ] {
            let env = f.supervisor.compose_env(
                EnvBase::new("/usr/bin", "/Users/tester"),
                Some(&account(vendor, Some("/dir"))),
                "m",
            );
            assert_eq!(env.get(key).map(String::as_str), Some("/dir"), "{vendor}");
        }

        // No directory-based switching, and the ambient slot: no key, no error.
        let env = f.supervisor.compose_env(
            EnvBase::new("/usr/bin", "/Users/tester"),
            Some(&account("gemini", Some("/dir"))),
            "m",
        );
        assert!(env.keys().all(|k| !k.ends_with("_HOME") && k != "CLAUDE_CONFIG_DIR"));

        let env = f.supervisor.compose_env(
            EnvBase::new("/usr/bin", "/Users/tester"),
            Some(&account("claude", None)),
            "m",
        );
        assert!(!env.contains_key("CLAUDE_CONFIG_DIR"));
    }

    #[tokio::test]
    async fn env_passthrough_adds_configured_keys_but_never_the_identity_markers() {
        let f = fixture().await;
        std::env::set_var("GILDE_TEST_PASSTHROUGH", "yes");
        f.supervisor
            .config()
            .patch(serde_json::json!({
                "agents": {
                    "envPassthrough": ["GILDE_TEST_PASSTHROUGH", "GILDE_SPAWN_ID", "GILDE_TEST_ABSENT"]
                }
            }))
            .expect("patch");

        let env = f
            .supervisor
            .compose_env(EnvBase::new("/usr/bin", "/Users/tester"), None, "the-marker");

        assert_eq!(env["GILDE_TEST_PASSTHROUGH"], "yes");
        assert!(
            !env.contains_key("GILDE_TEST_ABSENT"),
            "a passthrough key that is not in the ambient environment adds nothing"
        );
        assert_eq!(
            env["GILDE_SPAWN_ID"], "the-marker",
            "envPassthrough must not be able to forge the spawn marker"
        );
        std::env::remove_var("GILDE_TEST_PASSTHROUGH");
    }

    // -- masking ----------------------------------------------------------

    #[test]
    fn mask_argv_vec_is_the_contract_redaction() {
        let masked = mask_argv_vec(&argv(&[
            "/bin/agent",
            "--api-key=sk-abcdefghijklmnopqrst",
            "--token",
            "ghp_abcdefghijklmnopqrstuvwx",
            "--",
            "write a haiku",
        ]));
        assert_eq!(masked[0], "/bin/agent");
        assert!(!masked[1].contains("sk-abcdefghijklmnopqrst"), "{:?}", masked[1]);
        assert_eq!(masked[2], "--token");
        assert_eq!(masked[3], crate::supervise::redact::REDACTED, "the value after a secret flag");
        assert_eq!(masked[4], "--");
        assert_eq!(
            masked[5],
            crate::supervise::redact::PROMPT,
            "the user's prompt is content (contract §9.7 rule 5)"
        );
    }

    #[test]
    fn mask_argv_vec_leaves_ordinary_arguments_alone() {
        let plain = argv(&["/usr/bin/git", "-C", "/repo", "status", "--porcelain=v2"]);
        assert_eq!(mask_argv_vec(&plain), plain);
    }

    #[test]
    fn account_env_keys_cover_the_directory_based_vendors_only() {
        assert_eq!(account_env_key("claude"), Some("CLAUDE_CONFIG_DIR"));
        assert_eq!(account_env_key("codex"), Some("CODEX_HOME"));
        assert_eq!(account_env_key("agy"), Some("AGY_HOME"));
        assert_eq!(account_env_key("antigravity"), Some("AGY_HOME"));
        assert_eq!(account_env_key("cursorAgent"), None);
        assert_eq!(account_env_key("gemini"), None);
    }
}
