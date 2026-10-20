//! The durable PID registry: `Supervisor`, `SpawnPlan`, `SpawnGuard`
//! (DESIGN §8-2, §8-3, §6-1).
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
use crate::events::bus::EventBus;
use crate::store::models::{
    argv_signature, AppInstanceRow, NewPlannedProc, ProcKind, ProcRow, ProcState,
};
use crate::store::{dao, now_ms, Db};
use crate::supervise::identity::ENV_SPAWN_ID;
use crate::supervise::procscan::{mask_argv, start_time_of, REDACTED};

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

// ---------------------------------------------------------------------------
// SpawnPlan
// ---------------------------------------------------------------------------

/// Everything the registry needs to describe a child *before* it exists.
///
/// `argv` is the **raw** argv, `argv[0]` being the absolute binary path. The
/// masking happens inside [`Supervisor::plan_spawn`]; callers must not mask
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
    events: Option<EventBus>,
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
        })
    }

    /// Attach the event bus. Separate from [`Supervisor::open_instance`]
    /// because the supervisor is created during boot, before the window (and
    /// therefore the bus) exists.
    pub fn with_events(mut self, events: EventBus) -> Supervisor {
        self.events = Some(events);
        self
    }

    pub fn set_events(&mut self, events: EventBus) {
        self.events = Some(events);
    }

    pub fn events(&self) -> Option<&EventBus> {
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

    /// Write the row, then hand back the guard that owns it (DESIGN §8-3).
    ///
    /// The INSERT and the `argv_sig` correction go in one transaction under
    /// `synchronous=FULL`, so the row that reaches the disk is either complete
    /// or absent.
    pub async fn plan_spawn(&self, plan: SpawnPlan) -> Result<SpawnGuard> {
        if plan.argv.is_empty() {
            return Err(GildeError::spawn("a spawn plan needs at least argv[0]"));
        }

        let spawn_id = Uuid::now_v7().to_string();
        // Fence 2 hashes the *unmasked* argv; the stored `argv` is masked. See
        // the module header for why the two must disagree.
        let raw_sig = argv_signature(&plan.argv);
        let masked = mask_argv_vec(&plan.argv);

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
        self.db
            .write_durable(move |conn| {
                let tx = conn.unchecked_transaction()?;
                dao::proc::insert_planned(&tx, &planned)?;
                tx.execute(
                    "UPDATE proc_registry SET argv_sig = ?2 WHERE spawn_id = ?1",
                    rusqlite::params![row_id, raw_sig],
                )?;
                tx.commit()?;
                Ok(())
            })
            .await?;

        tracing::debug!(
            target: "supervise",
            spawn_id = %spawn_id,
            kind = %plan.kind,
            cwd = %plan.cwd.display(),
            "spawn planned"
        );

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
            match (account_env_key(&account.vendor), &account.config_dir) {
                (Some(key), Some(dir)) => {
                    env.insert(key.to_string(), dir.clone());
                }
                // The ambient slot has no directory of its own, and a vendor
                // without directory-based switching has no key to set. Neither
                // is an error.
                _ => {}
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

/// Mask an argv **element-wise** for storage in `proc_registry.argv`.
///
/// [`mask_argv`] works on one string, which covers `--token=<secret>` and
/// `Bearer <secret>` when both halves live in the same element. A CLI just as
/// often takes `--token <secret>` as two separate arguments, and after
/// `Vec<String>` splitting there is no single string for the regex to see —
/// hence the extra "previous element was a credential flag" rule here.
pub fn mask_argv_vec(argv: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(argv.len());
    let mut redact_next = false;
    for arg in argv {
        if redact_next {
            out.push(REDACTED.to_string());
            redact_next = false;
            continue;
        }
        redact_next = is_credential_flag(arg);
        out.push(mask_argv(arg));
    }
    out
}

/// A bare `--token` / `-p`-style flag whose *value* is the next argument.
/// Only exact flag names count: a `--token=x` form is already handled by the
/// regex, and matching prefixes would redact unrelated arguments.
fn is_credential_flag(arg: &str) -> bool {
    let name = arg
        .strip_prefix("--")
        .or_else(|| arg.strip_prefix('-'))
        .unwrap_or("");
    if name.is_empty() || arg == "--" {
        return false;
    }
    matches!(
        name.to_ascii_lowercase().as_str(),
        "api-key" | "api_key" | "apikey" | "token" | "password" | "secret"
    )
}

// ---------------------------------------------------------------------------
// git spawn hook — DESIGN §8-1 "맨몸 spawn은 금지다"
// ---------------------------------------------------------------------------

/// [`SpawnGuard`] as the ticket `worktree::git` hands its spawns.
///
/// The trait methods swallow database errors on purpose: a registry write that
/// fails must not turn a working `git status` into a user-visible failure. The
/// failure is logged, and the row stays in whatever state it reached — which
/// the reaper then resolves, exactly as it does after a crash.
impl crate::worktree::git::SpawnTicket for SpawnGuard {
    fn env_marker(&self) -> String {
        SpawnGuard::env_marker(self).to_string()
    }

    fn mark_running(&self, pid: i32, pgid: i32) {
        if let Err(err) = SpawnGuard::mark_running(self, pid, pgid) {
            tracing::warn!(
                target: "supervise",
                spawn_id = %self.spawn_id,
                error = %err,
                "could not record a git spawn as running"
            );
        }
    }

    fn mark_exited(&self, code: Option<i32>, signal: Option<i32>) {
        if let Err(err) = SpawnGuard::mark_exited(self, code, signal) {
            tracing::warn!(
                target: "supervise",
                spawn_id = %self.spawn_id,
                error = %err,
                "could not record a git spawn as exited"
            );
        }
    }
}

/// The write-before-spawn hook `worktree::git` calls for every `git` process
/// and for the worktree post-create command.
pub struct RegistrySpawnHook {
    supervisor: Arc<Supervisor>,
}

impl RegistrySpawnHook {
    pub fn new(supervisor: Arc<Supervisor>) -> RegistrySpawnHook {
        RegistrySpawnHook { supervisor }
    }
}

impl crate::worktree::git::SpawnHook for RegistrySpawnHook {
    fn plan<'a>(
        &'a self,
        kind: &'a str,
        argv: &'a [String],
        cwd: &'a Path,
    ) -> crate::worktree::git::BoxFuture<'a, Option<Box<dyn crate::worktree::git::SpawnTicket>>>
    {
        Box::pin(async move {
            let kind = ProcKind::from_db_str(kind).unwrap_or(ProcKind::Probe);
            let plan = SpawnPlan::new(kind, argv.to_vec(), cwd);
            match self.supervisor.plan_spawn(plan).await {
                Ok(guard) => Some(Box::new(guard) as Box<dyn crate::worktree::git::SpawnTicket>),
                Err(err) => {
                    // Refusing to run `git` because the registry is unwritable
                    // would make a database problem look like a git problem.
                    // The spawn proceeds untracked and says so, loudly.
                    tracing::error!(
                        target: "supervise",
                        error = %err,
                        kind = %kind,
                        "could not plan a git spawn; it will run untracked"
                    );
                    None
                }
            }
        })
    }
}

/// Route `worktree::git`'s spawns through the registry. Idempotent — the first
/// installation wins (`false` on a later call).
pub fn install_git_spawn_hook(supervisor: Arc<Supervisor>) -> bool {
    crate::worktree::git::install_spawn_hook(Arc::new(RegistrySpawnHook::new(supervisor)))
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
        assert!(row.argv.contains(REDACTED));
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
    fn mask_argv_vec_redacts_both_the_joined_and_the_split_forms() {
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
        assert_eq!(masked[3], REDACTED, "the value after a bare flag is the secret");
        assert_eq!(masked[4], "--");
        assert_eq!(
            masked[5], "write a haiku",
            "the user's prompt is not a secret and is not masked"
        );
    }

    #[test]
    fn mask_argv_vec_leaves_ordinary_arguments_alone() {
        let plain = argv(&["/usr/bin/git", "-C", "/repo", "status", "--porcelain=v2"]);
        assert_eq!(mask_argv_vec(&plain), plain);
    }

    #[test]
    fn credential_flags_are_matched_exactly() {
        assert!(is_credential_flag("--token"));
        assert!(is_credential_flag("--API-KEY"));
        assert!(is_credential_flag("-password"));
        assert!(!is_credential_flag("--tokenizer"));
        assert!(!is_credential_flag("--"));
        assert!(!is_credential_flag("token"));
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
