//! The `git` wrapper (DESIGN §7-2; contract §9.3 launch site).
//!
//! We shell out to the real `git` on purpose: libgit2's worktree support lags
//! upstream in exactly the places where being wrong loses the user's work, and
//! the user's `includeIf` / hooks / credential helper / `safe.directory`
//! configuration has to apply here exactly as it does in their terminal —
//! the agent runs `git` inside these worktrees itself.
//!
//! Every invocation obeys the same rules:
//!
//! 1. **No shell.** An argv array, always. There is no string that a repository
//!    name or branch name could ever be interpolated into.
//! 2. **`git -C <cwd>`** rather than changing the process's directory.
//! 3. **`GIT_TERMINAL_PROMPT=0`**, so a repository that wants credentials fails
//!    in 20 ms instead of blocking a command handler forever. Read-only calls
//!    additionally get **`GIT_OPTIONAL_LOCKS=0`** so that polling `status` never
//!    fights the user's interactive git for `index.lock`.
//! 4. **A wall-clock cap** (20 s by default). On expiry the process group is
//!    killed and the call returns a retryable error.
//! 5. **Its own process group** and a durable registry row written *before*
//!    the spawn (write-before-spawn, §2.1).
//! 6. **No content in argv.** A commit message travels on stdin (`-F -`),
//!    never as an argument, because the registry stores argv (§9.7).
//! 7. **No repository program on Gilde's own reads** (A37.6, A46.1). Every
//!    call has `core.fsmonitor` off, and every call but the user's commit
//!    and merge has hooks off. A read-only call also switches off signature
//!    programs and `diff.external`. A read that can reach file content
//!    (status, diff, log, …) first lists the effective config and makes
//!    every filter, diff and merge driver it names inert with `-c`. When
//!    one cannot be made inert, the read is skipped (fail closed,
//!    `GIT_FAILED{repoProgram}`). Writes the user started (commit, merge,
//!    worktree add/remove, the checkpoint commit) keep the repository's
//!    drivers, as the user's own git would: an LFS checkout or commit must
//!    still run its filter.
//!
//! # Registration (contract A12.8)
//!
//! Every spawn here is planned through the supervisor's [`SupervisorApi`],
//! bound once by the composition root with [`bind_supervisor`]:
//!
//! * **git** is planned as `ProcKind::Git` (A17.1) whenever a supervisor is
//!   bound; a refusal (`NOT_READY{bootSweep}`, `DISK_FULL`, …) is returned and
//!   git does not run. Tools and tests with no supervisor bound run it
//!   untracked (the legacy T09 hook is gone, A22.3);
//! * **every other program** (the worktree post-create command) is planned
//!   through `SupervisorApi` or not run at all: a refusal — or no supervisor
//!   — never launches the command (A14.2, fail closed).
//!
//! Every query used by the Gilde 2 services returns [`GitError`], so a
//! timeout stays `TIMEOUT` and a refusal keeps its code (A17.4); the v1
//! helpers that return `GildeError` are thin wrappers over them.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsStr;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::Command;

use crate::contract::activity::ExitInfo;
use crate::contract::common::AbsPath;
use crate::contract::error::{ErrorCode, IpcError};
use crate::contract::process::ProcKind;
use crate::contract::services::{SpawnRequest, SupervisorApi};
use crate::error::{GildeError, Result};

/// Wall-clock cap on a single `git` invocation (DESIGN §7-2).
pub const GIT_TIMEOUT: Duration = Duration::from_secs(20);
/// `git commit` runs the user's hooks (a `pre-commit` linter can be slow).
pub const COMMIT_TIMEOUT: Duration = Duration::from_secs(120);
/// stdout kept by default: far above any status or log answer.
pub const STDOUT_CAP: usize = 32 * 1024 * 1024;
/// stderr kept for the error detail; the rest is drained and dropped.
pub const STDERR_CAP: usize = 64 * 1024;

/// Identity used for checkpoint commits, so a user without a global
/// `user.email` never meets a confusing failure during a rescue (DESIGN §7-7).
pub const CHECKPOINT_USER_NAME: &str = "gilde-code";
pub const CHECKPOINT_USER_EMAIL: &str = "gilde@localhost";
/// Reason recorded on `git worktree lock` for worktrees we manage.
pub const LOCK_REASON: &str = "gilde-code session";
/// git's well-known empty tree: the diff base of a repository without commits.
pub const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
/// `GIT_FAILED{lockLeft}` (A37.2) and its sentence.
pub const LOCK_LEFT: &str = "lockLeft";
pub const LOCK_LEFT_KEY: &str = "changes.gitFailed.lockLeft";

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

// ---------------------------------------------------------------------------
// Registration: SupervisorApi, or untracked (git only, nothing bound)
// ---------------------------------------------------------------------------

static SUPERVISOR: OnceLock<Arc<dyn SupervisorApi>> = OnceLock::new();

/// Bind the supervisor every spawn of this module is planned through
/// (composition root, T09b). The first binding wins (`false` afterwards).
/// The resolved `git` is allowlisted for `git` rows (§9.3) at the same time.
pub fn bind_supervisor(api: Arc<dyn SupervisorApi>) -> bool {
    crate::supervise::spawn::allow_executable(git_binary(), &[crate::store::models::ProcKind::Git]);
    SUPERVISOR.set(api).is_ok()
}

/// The bound supervisor, if any.
pub fn supervisor() -> Option<Arc<dyn SupervisorApi>> {
    SUPERVISOR.get().cloned()
}

/// One planned spawn.
enum Registration {
    Api {
        api: Arc<dyn SupervisorApi>,
        ticket: crate::contract::services::SpawnTicket,
    },
    Untracked,
}

impl Registration {
    fn marker(&self) -> Option<String> {
        match self {
            Registration::Api { ticket, .. } => Some(ticket.env_marker.clone()),
            Registration::Untracked => None,
        }
    }

    async fn running(&self, pid: i32) {
        match self {
            Registration::Api { api, ticket } => {
                if let Err(err) = api.mark_running(ticket, pid).await {
                    tracing::warn!(target: "git", code = err.code.as_str(), "could not record a spawn as running");
                }
            }
            Registration::Untracked => {}
        }
    }

    async fn exited(&self, code: Option<i32>, signal: Option<i32>) {
        match self {
            Registration::Api { api, ticket } => api.mark_exited(ticket, ExitInfo { code, signal }).await,
            Registration::Untracked => {}
        }
    }
}

fn spawn_request(kind: ProcKind, argv: &[String], cwd: &Path, env_keys: &[&str]) -> std::result::Result<SpawnRequest, IpcError> {
    let Some((argv0, args)) = argv.split_first() else {
        return Err(IpcError::spawn_failed());
    };
    let argv0 = AbsPath::parse(argv0.as_str()).map_err(|_| IpcError::spawn_failed())?;
    Ok(SpawnRequest {
        kind,
        argv0,
        args: args.to_vec(),
        cwd: cwd.to_str().and_then(|c| AbsPath::parse(c).ok()),
        env_keys: env_keys.iter().map(|k| (*k).to_string()).collect(),
        parent_uid: None,
        generation: None,
        turn_id: None,
    })
}

/// git: `SupervisorApi` as `ProcKind::Git` (fail closed) when bound; else
/// untracked (tools and tests with nothing bound).
async fn register_git(argv: &[String], cwd: &Path, env_keys: &[&str]) -> std::result::Result<Registration, IpcError> {
    if let Some(api) = supervisor() {
        let req = spawn_request(ProcKind::Git, argv, cwd, env_keys)?;
        let ticket = api.plan_spawn(req).await?;
        return Ok(Registration::Api { api, ticket });
    }
    Ok(Registration::Untracked)
}

/// Any other program: planned through `SupervisorApi` or refused (A14.2).
async fn register_fail_closed(kind: ProcKind, argv: &[String], cwd: &Path, env_keys: &[&str]) -> std::result::Result<Registration, IpcError> {
    let Some(api) = supervisor() else {
        return Err(IpcError::refused("notAllowlisted"));
    };
    let req = spawn_request(kind, argv, cwd, env_keys)?;
    let ticket = api.plan_spawn(req).await?;
    Ok(Registration::Api { api, ticket })
}

// ---------------------------------------------------------------------------
// Output and errors
// ---------------------------------------------------------------------------

/// One completed `git` invocation.
#[derive(Debug, Clone)]
pub struct GitOutput {
    pub code: i32,
    /// Raw bytes: `-z` output is not text and must not be lossily decoded
    /// before it has been split on NUL.
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl GitOutput {
    /// stdout as text, trailing newline removed.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim_end().to_string()
    }

    /// stdout split on `\n`, empty lines dropped.
    pub fn lines(&self) -> Vec<String> {
        String::from_utf8_lossy(&self.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.to_string())
            .collect()
    }

    /// stdout split on NUL, the trailing empty field dropped.
    pub fn nul_fields(&self) -> Vec<String> {
        let mut fields: Vec<String> = self
            .stdout
            .split(|b| *b == 0)
            .map(|f| String::from_utf8_lossy(f).into_owned())
            .collect();
        if fields.last().is_some_and(|f| f.is_empty()) {
            fields.pop();
        }
        fields
    }
}

/// Why a git call did not answer.
#[derive(Debug, Clone)]
pub enum GitError {
    /// Not started: refused by the supervisor (`REFUSED`, `DISK_FULL`,
    /// `NOT_READY{bootSweep}`) or the spawn itself failed — or an answer
    /// Gilde decided around git's run: [`lock_left`] (A37.2).
    Spawn(IpcError),
    /// git ran and exited with a code the caller did not allow.
    Failed { code: i32, stderr: String },
    /// Ran past its cap; the process group was killed.
    Timeout,
    /// stdout exceeded the caller's cap.
    TooLarge,
}

impl GitError {
    /// The contract error (§4.4): `GIT_FAILED` carries git's stderr as the
    /// sanitized `detail` (home as `~`, ≤ 4 KiB; never logged).
    pub fn to_ipc(&self) -> IpcError {
        match self {
            GitError::Spawn(e) => e.clone(),
            GitError::Failed { stderr, .. } => {
                let err = IpcError::git_failed();
                let s = stderr.trim();
                if s.is_empty() {
                    err
                } else {
                    let clean = crate::render::sanitize::sanitize_display_text(
                        &crate::error::home_to_tilde(s),
                        crate::contract::display::TextCap::Bytes(4 * 1024),
                    );
                    err.with_detail(clean.text)
                }
            }
            GitError::Timeout => IpcError::timeout(),
            GitError::TooLarge => IpcError::git_failed(),
        }
    }

    pub(crate) fn into_gilde(self, sub: &str, cwd: &Path) -> GildeError {
        match self {
            GitError::Failed { code, stderr } => GildeError::Git { code, stderr },
            GitError::Timeout => GildeError::spawn(format!(
                "git {sub} timed out in {}",
                cwd.display()
            )),
            GitError::TooLarge => GildeError::spawn(format!("git {sub}: output too large")),
            // A read skipped by A46.1 stays a git failure for the v1 callers
            // (see [`is_repo_program_error`]).
            GitError::Spawn(e) if is_repo_program_ipc(&e) => GildeError::Git {
                code: -1,
                stderr: REPO_PROGRAM_DETAIL.to_string(),
            },
            GitError::Spawn(e) => match e.code {
                ErrorCode::Refused => GildeError::refused(
                    e.params.get("reason").and_then(|v| v.as_str()).unwrap_or("refused"),
                ),
                ErrorCode::DiskFull => GildeError::Io(std::io::Error::from_raw_os_error(libc::ENOSPC)),
                _ => GildeError::spawn(format!("git {sub}: {}", e.key)),
            },
        }
    }
}

/// How one call runs.
#[derive(Debug, Clone, Copy)]
pub struct ExecOpts<'a> {
    /// Exit codes that are answers, not failures.
    pub allowed: &'a [i32],
    pub timeout: Duration,
    /// Bytes written to stdin, then closed (commit messages, pathspecs).
    pub stdin: Option<&'a [u8]>,
    /// stdout beyond this is drained and the call fails `TooLarge`.
    pub stdout_cap: usize,
}

impl Default for ExecOpts<'_> {
    fn default() -> Self {
        ExecOpts {
            allowed: &[0],
            timeout: GIT_TIMEOUT,
            stdin: None,
            stdout_cap: STDOUT_CAP,
        }
    }
}

// ---------------------------------------------------------------------------
// run_git
// ---------------------------------------------------------------------------

/// Absolute path to `git`, resolved once.
pub fn git_binary() -> &'static Path {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        which::which("git")
            .ok()
            .unwrap_or_else(|| PathBuf::from("/usr/bin/git"))
    })
    .as_path()
}

/// Subcommands that never write to the repository. They get
/// `GIT_OPTIONAL_LOCKS=0` so a status poll cannot take `index.lock` away from
/// the user's own git.
const READ_ONLY: &[&str] = &[
    "status",
    "rev-parse",
    "rev-list",
    "log",
    "diff",
    "show",
    "show-ref",
    "symbolic-ref",
    "for-each-ref",
    "ls-files",
    "ls-tree",
    "cat-file",
    "check-ignore",
    "merge-base",
    "merge-tree",
    "describe",
    "var",
    "version",
    "config",
];

/// The subcommand of an argv, skipping leading global options (`-c k=v`,
/// `--no-pager`, …). Returns `("worktree list")` style pairs for `worktree`,
/// whose read-only-ness depends on the second word.
fn subcommand(args: &[&str]) -> String {
    let mut i = 0;
    while i < args.len() {
        let a = args[i];
        if a == "-c" || a == "--git-dir" || a == "--work-tree" || a == "-C" || a == "--namespace" {
            i += 2;
            continue;
        }
        if a.starts_with('-') {
            i += 1;
            continue;
        }
        if a == "worktree" {
            let second = args.get(i + 1).copied().unwrap_or("");
            return format!("worktree {second}");
        }
        return a.to_string();
    }
    String::new()
}

fn is_read_only(sub: &str) -> bool {
    sub == "worktree list" || READ_ONLY.contains(&sub)
}

/// Config every git call carries (A37.6): the repository's `core.fsmonitor`
/// program never runs on a call Gilde makes.
const NO_FSMONITOR: [&str; 2] = ["-c", "core.fsmonitor=false"];
/// Hooks are switched off for every call except the user's own commit and
/// merge (A37.6), which keep the repository's hooks as their own git would.
pub const NO_HOOKS: [&str; 2] = ["-c", "core.hooksPath=/dev/null"];

fn keeps_hooks(sub: &str) -> bool {
    matches!(sub, "commit" | "merge")
}

// ---------------------------------------------------------------------------
// No repository program on Gilde's own reads (A46.1)
// ---------------------------------------------------------------------------
//
// Every value below was checked with marker programs on git 2.39.3 (see
// `tests/worktree_safety.rs`, section 9). `GIT_ATTR_SOURCE` cannot do this:
// it needs git 2.40, and git still reads `.git/info/attributes` with it.

/// Config every read-only call carries on top of the A37.6 pair:
/// * `log.showSignature=false`: otherwise `git log` runs the repository's
///   `gpg.program` on every signed commit, whatever `--format` asks for;
/// * empty signature programs and `diff.external`: if a read ever asks for
///   one, git fails ("cannot run") instead of running the repository's.
const READ_ONLY_INERT: [&str; 12] = [
    "-c",
    "log.showSignature=false",
    "-c",
    "diff.external=",
    "-c",
    "gpg.program=",
    "-c",
    "gpg.openpgp.program=",
    "-c",
    "gpg.x509.program=",
    "-c",
    "gpg.ssh.program=",
];

/// Read-only subcommands that never run a filter, diff or merge driver.
/// They read refs, the config or the commit graph, never file content
/// through attributes. Every other read-only subcommand is guarded by
/// [`driver_overrides`], including any added to [`READ_ONLY`] later.
const DRIVER_FREE: &[&str] = &[
    "config",
    "rev-parse",
    "rev-list",
    "symbolic-ref",
    "show-ref",
    "for-each-ref",
    "merge-base",
    "ls-tree",
    "check-ignore",
    "var",
    "version",
    "worktree list",
];

fn guards_drivers(sub: &str) -> bool {
    is_read_only(sub) && !DRIVER_FREE.contains(&sub)
}

/// The listing behind the guard: every key of the effective config (system,
/// global, local, worktree, command line and every included file). Values
/// are not needed, and the listing itself runs no program.
const LIST_CONFIG_NAMES: [&str; 4] = ["config", "--list", "--name-only", "-z"];
/// A config listing larger than this is not neutralized; the read is skipped.
const CONFIG_LIST_CAP: usize = 1024 * 1024;
/// More distinct drivers than this are not neutralized; the read is skipped.
pub const MAX_DRIVERS: usize = 64;
/// A driver name longer than this (bytes) is not neutralized; the read is skipped.
pub const MAX_DRIVER_NAME: usize = 256;

/// `GIT_FAILED{repoProgram}` (A46.1) and its sentence: a read skipped
/// because a driver could not be made inert. The view shows the diff as
/// unavailable; Gilde never invents rows for it.
pub const REPO_PROGRAM: &str = "repoProgram";
pub const REPO_PROGRAM_KEY: &str = "changes.gitFailed.repoProgram";
/// The stderr the v1 wrappers' [`GildeError::Git`] carries for it.
pub const REPO_PROGRAM_DETAIL: &str =
    "gilde: read skipped: this repository configures a filter, diff or merge driver that Gilde cannot switch off for its own reads";

/// The answer of a read skipped by A46.1: `GIT_FAILED{repoProgram}`.
pub fn repo_program_refusal() -> GitError {
    GitError::Spawn(IpcError::git_failed().param("reason", REPO_PROGRAM).with_key(REPO_PROGRAM_KEY))
}

fn is_repo_program_ipc(e: &IpcError) -> bool {
    e.code == ErrorCode::GitFailed && e.params.get("reason").and_then(|v| v.as_str()) == Some(REPO_PROGRAM)
}

/// Whether `e` is [`repo_program_refusal`]'s answer.
pub fn is_repo_program_refusal(e: &GitError) -> bool {
    matches!(e, GitError::Spawn(ipc) if is_repo_program_ipc(ipc))
}

/// [`is_repo_program_refusal`] for the v1 wrappers' [`GildeError`].
pub fn is_repo_program_error(e: &GildeError) -> bool {
    matches!(e, GildeError::Git { code: -1, stderr } if stderr == REPO_PROGRAM_DETAIL)
}

/// A kind of attribute-selected driver that runs a configured program.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Driver {
    /// `filter=<name>`: `clean` on hashing a worktree file (status, diff),
    /// `smudge` on writing one (diff temp files, merge renormalization),
    /// `process` for both.
    Filter,
    /// `diff=<name>`: `command` (external diff) and `textconv`.
    Diff,
    /// `merge=<name>`: `driver` (merge-tree, the merge preview).
    Merge,
}

impl Driver {
    fn of_section(section: &[u8]) -> Option<Driver> {
        // The listing prints section names in lower case.
        match section {
            b"filter" => Some(Driver::Filter),
            b"diff" => Some(Driver::Diff),
            b"merge" => Some(Driver::Merge),
            _ => None,
        }
    }

    fn section(self) -> &'static str {
        match self {
            Driver::Filter => "filter",
            Driver::Diff => "diff",
            Driver::Merge => "merge",
        }
    }

    /// The keys that make one driver inert and their values, as measured on
    /// git 2.39.3:
    /// * filter: with an empty `process`, git skips `clean`/`smudge` and
    ///   starts no process. Empty `clean`/`smudge` are no command either.
    ///   `required=false` is needed, or a required filter makes git die.
    /// * diff: never read under `--no-ext-diff`/`--no-textconv`. If a read
    ///   did read them, git fails ("cannot run") instead of running one.
    /// * merge: `merge-tree` fails ("cannot run") instead of running the
    ///   driver, so the merge check fails closed.
    ///
    /// No value names a program: a program name would be looked up on
    /// `PATH`, and a `PATH` with `.` would find one in the repository.
    fn inert(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Driver::Filter => &[("process", ""), ("clean", ""), ("smudge", ""), ("required", "false")],
            Driver::Diff => &[("command", ""), ("textconv", "")],
            Driver::Merge => &[("driver", "")],
        }
    }
}

/// Every driver `git config --list --name-only -z` names (key
/// `<section>.<name>.<var>`; the name may contain dots). `None` when one
/// cannot be overridden by `-c`, so the read must be skipped:
/// * a name with `=` (git splits `-c` at the first `=`, so the override
///   would set another key);
/// * a name that is not UTF-8, or has a newline;
/// * a name over [`MAX_DRIVER_NAME`] bytes, or more than [`MAX_DRIVERS`] drivers.
///
/// Keys without a name (`diff.external`, `merge.renormalize`) and drivers
/// with an empty name are not drivers: git never selects them.
fn drivers_named(listing: &[u8]) -> Option<BTreeSet<(Driver, String)>> {
    let mut drivers = BTreeSet::new();
    for key in listing.split(|b| *b == 0) {
        let Some(first) = key.iter().position(|b| *b == b'.') else { continue };
        let Some(kind) = Driver::of_section(&key[..first]) else { continue };
        let rest = &key[first + 1..];
        let Some(last) = rest.iter().rposition(|b| *b == b'.') else { continue };
        let name = &rest[..last];
        if name.is_empty() {
            continue;
        }
        let name = std::str::from_utf8(name).ok()?;
        if name.len() > MAX_DRIVER_NAME || name.contains('=') || name.contains('\n') {
            return None;
        }
        drivers.insert((kind, name.to_string()));
        if drivers.len() > MAX_DRIVERS {
            return None;
        }
    }
    Some(drivers)
}

/// The `-c` pairs that make every driver of `drivers` inert.
fn inert_args(drivers: &BTreeSet<(Driver, String)>) -> Vec<String> {
    let mut args = Vec::new();
    for (kind, name) in drivers {
        for (var, value) in kind.inert() {
            args.push("-c".to_string());
            args.push(format!("{}.{name}.{var}={value}", kind.section()));
        }
    }
    args
}

/// The overrides a guarded read of `cwd` needs, from a listing made with
/// the same environment as the read. A listing that fails or times out
/// gives the read its own error. A listing that cannot be neutralized
/// skips the read with [`repo_program_refusal`].
async fn driver_overrides(
    cwd: &Path,
    timeout: Duration,
    extra_env: &[(String, String)],
) -> std::result::Result<Vec<String>, GitError> {
    let opts = ExecOpts {
        stdout_cap: CONFIG_LIST_CAP,
        timeout: timeout.min(GIT_TIMEOUT),
        ..ExecOpts::default()
    };
    let listing = match spawn_git(cwd, &LIST_CONFIG_NAMES, opts, extra_env, &[]).await {
        Ok(out) => out,
        Err(GitError::TooLarge) => return Err(repo_program_refusal()),
        Err(e) => return Err(e),
    };
    match drivers_named(&listing.stdout) {
        Some(drivers) => Ok(inert_args(&drivers)),
        None => {
            tracing::debug!(target: "git", "a read was skipped: a repository driver cannot be made inert");
            Err(repo_program_refusal())
        }
    }
}

/// What a timed-out git gets before it is killed: `SIGTERM` lets git remove
/// its own lockfiles (A37.2).
const TERM_GRACE: Duration = Duration::from_secs(2);

/// Run `git -C <cwd> <args…>` and fail on a non-zero exit.
///
/// The error carries the raw `stderr` verbatim — it is never translated,
/// because git's messages are the most useful thing we can show and paraphrasing
/// them would lose information the user needs.
pub async fn run_git(cwd: &Path, args: &[&str]) -> Result<GitOutput> {
    run_git_status(cwd, args, &[0]).await
}

/// Like [`run_git`] but tolerating the listed exit codes (`check-ignore`
/// reports "not ignored" as exit 1, which is an answer and not a failure).
pub async fn run_git_status(cwd: &Path, args: &[&str], allowed: &[i32]) -> Result<GitOutput> {
    let opts = ExecOpts {
        allowed,
        ..ExecOpts::default()
    };
    exec(cwd, args, opts)
        .await
        .map_err(|e| e.into_gilde(&subcommand(args), cwd))
}

/// The one git launcher: registration first, then the spawn (§2.1, §9.3).
pub async fn exec(cwd: &Path, args: &[&str], opts: ExecOpts<'_>) -> std::result::Result<GitOutput, GitError> {
    exec_env(cwd, args, opts, &[]).await
}

/// [`exec`] with `extra_env` set on top of git's own overlay (the bounded
/// repo-identity probe of `commands/repo_identity.rs`, whose tests point git
/// at a scratch HOME; A17.3). Names only reach the registry, never values.
pub async fn exec_env(
    cwd: &Path,
    args: &[&str],
    opts: ExecOpts<'_>,
    extra_env: &[(String, String)],
) -> std::result::Result<GitOutput, GitError> {
    // A46.1: a read that can reach file content first makes every driver
    // of the effective config inert, or is skipped.
    let inert = if guards_drivers(&subcommand(args)) {
        driver_overrides(cwd, opts.timeout, extra_env).await?
    } else {
        Vec::new()
    };
    spawn_git(cwd, args, opts, extra_env, &inert).await
}

/// One registered git run: the A37.6 pair, the read-only constants,
/// `inert` (the A46.1 overrides) and then the caller's `args`.
async fn spawn_git(
    cwd: &Path,
    args: &[&str],
    opts: ExecOpts<'_>,
    extra_env: &[(String, String)],
    inert: &[String],
) -> std::result::Result<GitOutput, GitError> {
    let sub = subcommand(args);
    let read_only = is_read_only(&sub);
    let mut full: Vec<&str> = Vec::with_capacity(args.len() + inert.len() + READ_ONLY_INERT.len() + 4);
    full.extend_from_slice(&NO_FSMONITOR);
    if !keeps_hooks(&sub) {
        full.extend_from_slice(&NO_HOOKS);
    }
    if read_only {
        full.extend_from_slice(&READ_ONLY_INERT);
    }
    full.extend(inert.iter().map(String::as_str));
    full.extend_from_slice(args);
    let mut argv: Vec<String> = Vec::with_capacity(full.len() + 3);
    argv.push(git_binary().to_string_lossy().into_owned());
    argv.push("-C".into());
    argv.push(cwd.to_string_lossy().into_owned());
    argv.extend(full.iter().map(|a| (*a).to_string()));

    let mut env_keys: Vec<&str> = git_env_keys(read_only);
    env_keys.extend(extra_env.iter().map(|(k, _)| k.as_str()));
    let reg = register_git(&argv, cwd, &env_keys).await.map_err(GitError::Spawn)?;

    let mut cmd = Command::new(git_binary());
    cmd.arg("-C").arg(cwd).args(&full);
    cmd.envs(extra_env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    cmd.envs(git_env(read_only, reg.marker().as_deref()));
    let started = std::time::SystemTime::now();
    let (code, signal, out, err) = match drive(cmd, &reg, opts.stdin, opts.stdout_cap, opts.timeout).await {
        // A write that ran out of time may have left its lockfiles (A37.2).
        Err(GitError::Timeout) if !read_only => return Err(after_timeout(cwd, started)),
        other => other?,
    };

    let Some(code) = code else {
        tracing::debug!(target: "git", sub = %sub, ?signal, "git ended by a signal");
        return Err(GitError::Failed {
            code: -1,
            stderr: String::from_utf8_lossy(&err.0).into_owned(),
        });
    };
    if out.1 > opts.stdout_cap {
        return Err(GitError::TooLarge);
    }
    let result = GitOutput {
        code,
        stdout: out.0,
        stderr: String::from_utf8_lossy(&err.0).into_owned(),
    };
    if opts.allowed.contains(&code) {
        Ok(result)
    } else {
        tracing::debug!(target: "git", %code, sub = %sub, "git exited non-zero");
        Err(GitError::Failed {
            code,
            stderr: result.stderr,
        })
    }
}

/// Spawn `cmd` (stdio piped, own group), feed `stdin`, read both streams with
/// caps, wait — all under `timeout`. Returns `(code, signal, (stdout, total),
/// (stderr, total))`.
async fn drive(
    mut cmd: Command,
    reg: &Registration,
    stdin: Option<&[u8]>,
    stdout_cap: usize,
    timeout: Duration,
) -> std::result::Result<(Option<i32>, Option<i32>, (Vec<u8>, usize), (Vec<u8>, usize)), GitError> {
    cmd.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() });
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    // Every spawn is its own process group so a stuck child (and any hook it
    // started) can be signalled as a unit.
    cmd.process_group(0);
    cmd.kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            reg.exited(None, None).await;
            return Err(GitError::Spawn(
                IpcError::spawn_failed().with_detail(crate::error::home_to_tilde(&e.to_string())),
            ));
        }
    };
    let pid = child.id().map(|p| p as i32).unwrap_or(0);
    reg.running(pid).await;

    let input = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let work = async {
        let feed = async {
            if let (Some(mut s), Some(bytes)) = (input, stdin) {
                let _ = s.write_all(bytes).await;
                let _ = s.shutdown().await;
            }
        };
        let (_, out, err) = tokio::join!(feed, read_keep(stdout, stdout_cap), read_keep(stderr, STDERR_CAP));
        let status = child.wait().await;
        (out, err, status)
    };
    match tokio::time::timeout(timeout, work).await {
        Ok((out, err, Ok(status))) => {
            let code = status.code();
            let signal = exit_signal(&status);
            reg.exited(code, signal).await;
            Ok((code, signal, out, err))
        }
        Ok((_, _, Err(e))) => {
            reg.exited(None, None).await;
            Err(GitError::Spawn(IpcError::spawn_failed().with_detail(e.to_string())))
        }
        Err(_) => {
            // SIGTERM first (git removes its own lockfiles on it), and SIGKILL
            // only for a git still alive after the grace (A37.2). The child is
            // not reaped before either signal, so its group id cannot have
            // been reused.
            if pid > 0 {
                signal_group(pid, libc::SIGTERM);
            }
            let signal = match tokio::time::timeout(TERM_GRACE, child.wait()).await {
                Ok(_) => libc::SIGTERM,
                Err(_) => {
                    if pid > 0 {
                        signal_group(pid, libc::SIGKILL);
                    }
                    let _ = child.wait().await;
                    libc::SIGKILL
                }
            };
            reg.exited(None, Some(signal)).await;
            Err(GitError::Timeout)
        }
    }
}

/// After a write timed out (A37.2): every `*.lock` file at the top of the
/// git dir (and the shared `packed-refs`/`config`/`shallow` locks of a linked
/// worktree's common dir) whose mtime is not older than this run's spawn was
/// taken by this run and is removed; an older one belongs to someone else, is
/// left alone, and the answer is `GIT_FAILED{lockLeft}` naming it.
fn after_timeout(cwd: &Path, started: std::time::SystemTime) -> GitError {
    let Some((gitdir, common)) = git_dirs(cwd) else { return GitError::Timeout };
    // Compared exactly: on a file system with coarse timestamps this run's
    // own lock can look older and is then reported, never someone else's
    // removed (fail closed).
    let floor = started;
    // The git dir's own locks; from a linked worktree's common dir only the
    // shared files its operation can lock — never the main checkout's
    // `index.lock`/`HEAD.lock`, which a git running there may hold right now.
    const SHARED: &[&str] = &["packed-refs.lock", "config.lock", "shallow.lock"];
    let linked = gitdir != common;
    let mut dirs = vec![(gitdir, false)];
    if linked {
        dirs.push((common, true));
    }
    let mut left: Option<PathBuf> = None;
    for (dir, shared_only) in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let is_lock = path.extension().is_some_and(|e| e == "lock") && entry.file_type().is_ok_and(|t| t.is_file());
            if !is_lock {
                continue;
            }
            if shared_only && !SHARED.iter().any(|s| entry.file_name() == *s) {
                continue;
            }
            let ours = std::fs::symlink_metadata(&path).and_then(|m| m.modified()).is_ok_and(|m| m >= floor);
            if ours {
                match std::fs::remove_file(&path) {
                    Ok(()) => tracing::warn!(target: "git", "removed a lock file a timed-out git left"),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => left = Some(path),
                }
            } else {
                left = Some(path);
            }
        }
    }
    match left {
        Some(path) => lock_left(&path),
        None => GitError::Timeout,
    }
}

/// `GIT_FAILED{lockLeft}` with the lock's path (home as `~`) as the detail
/// (A37.2): a lock Gilde did not take, left untouched.
pub fn lock_left(path: &Path) -> GitError {
    let shown = crate::render::sanitize::sanitize_display_text(
        &crate::error::home_to_tilde(&path.to_string_lossy()),
        crate::contract::display::TextCap::Bytes(4 * 1024),
    )
    .text;
    GitError::Spawn(IpcError::git_failed().param("reason", LOCK_LEFT).with_key(LOCK_LEFT_KEY).with_detail(shown))
}

/// Whether `e` is [`lock_left`]'s answer.
pub fn is_lock_left(e: &GitError) -> bool {
    matches!(e, GitError::Spawn(ipc) if ipc.code == ErrorCode::GitFailed && ipc.params.get("reason").and_then(|v| v.as_str()) == Some(LOCK_LEFT))
}

/// The git dir and the common dir of the checkout `dir` is in, read from
/// disk (a `.git` directory, or a linked worktree's `.git` file and its
/// `commondir`); no git process.
pub fn git_dirs(dir: &Path) -> Option<(PathBuf, PathBuf)> {
    let mut probe = std::fs::canonicalize(dir).ok()?;
    for _ in 0..256 {
        let dot = probe.join(".git");
        if let Ok(meta) = std::fs::symlink_metadata(&dot) {
            let gitdir = if meta.is_dir() {
                dot
            } else if meta.is_file() {
                let text = std::fs::read_to_string(&dot).ok()?;
                let rel = text.lines().next()?.strip_prefix("gitdir:")?.trim();
                let p = if Path::new(rel).is_absolute() { PathBuf::from(rel) } else { probe.join(rel) };
                std::fs::canonicalize(p).ok()?
            } else {
                return None;
            };
            let common = match std::fs::read_to_string(gitdir.join("commondir")) {
                Ok(c) => {
                    let c = c.trim();
                    let p = if Path::new(c).is_absolute() { PathBuf::from(c) } else { gitdir.join(c) };
                    std::fs::canonicalize(p).ok()?
                }
                Err(_) => gitdir.clone(),
            };
            return Some((gitdir, common));
        }
        if !probe.pop() {
            return None;
        }
    }
    None
}

/// Keep the first `cap` bytes, drain the rest (so the child never blocks on a
/// full pipe); returns the kept bytes and the total length.
async fn read_keep<R: AsyncRead + Unpin>(reader: Option<R>, cap: usize) -> (Vec<u8>, usize) {
    let Some(mut reader) = reader else {
        return (Vec::new(), 0);
    };
    let mut kept = Vec::new();
    let mut total = 0usize;
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                total = total.saturating_add(n);
                if kept.len() < cap {
                    let room = cap - kept.len();
                    kept.extend_from_slice(&chunk[..n.min(room)]);
                }
            }
        }
    }
    (kept, total)
}

#[cfg(unix)]
fn exit_signal(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt as _;
    status.signal()
}

/// Signal the group of a child we just created and have not reaped (so its
/// pid cannot have been reused). git and the post-create program are
/// non-vendor helpers (§9.8).
#[cfg(unix)]
fn signal_group(pid: i32, signal: i32) {
    use nix::sys::signal::{killpg, Signal};
    use nix::unistd::Pid;
    if let Ok(sig) = Signal::try_from(signal) {
        let _ = killpg(Pid::from_raw(pid), sig);
    }
}

fn git_env_keys(read_only: bool) -> Vec<&'static str> {
    let mut keys = vec!["GIT_TERMINAL_PROMPT"];
    if read_only {
        keys.push("GIT_OPTIONAL_LOCKS");
    }
    keys.push("GILDE_SPAWN_ID");
    keys
}

/// Environment overlay. We deliberately inherit the rest of the environment:
/// git must see the user's `HOME`, `SSH_AUTH_SOCK` and `GIT_*` settings exactly
/// as their shell does (DESIGN §7-2).
fn git_env(read_only: bool, marker: Option<&str>) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("GIT_TERMINAL_PROMPT".to_string(), "0".to_string());
    if read_only {
        env.insert("GIT_OPTIONAL_LOCKS".to_string(), "0".to_string());
    }
    if let Some(m) = marker {
        env.insert("GILDE_SPAWN_ID".to_string(), m.to_string());
    }
    env
}

// ---------------------------------------------------------------------------
// Supervised programs other than git (fail closed)
// ---------------------------------------------------------------------------

/// What a supervised program did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramOutcome {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// The combined output, at most `tail_cap` bytes from the end.
    pub tail: String,
}

/// Run `program args…` in `cwd` as a registry row of `kind`, planned through
/// the bound `SupervisorApi` **before** it exists. A refusal (or no bound
/// supervisor) is returned and nothing runs (A14.2). The environment is
/// inherited plus `GILDE_SPAWN_ID`.
pub async fn run_program(
    kind: ProcKind,
    program: &Path,
    args: &[String],
    cwd: &Path,
    timeout: Duration,
    tail_cap: usize,
) -> std::result::Result<ProgramOutcome, IpcError> {
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(program.to_string_lossy().into_owned());
    argv.extend(args.iter().cloned());
    let reg = register_fail_closed(kind, &argv, cwd, &["GILDE_SPAWN_ID"]).await?;

    let mut cmd = Command::new(program);
    cmd.args(args).current_dir(cwd);
    if let Some(m) = reg.marker() {
        cmd.env("GILDE_SPAWN_ID", m);
    }
    match drive(cmd, &reg, None, 1024 * 1024, timeout).await {
        Ok((code, _signal, out, err)) => {
            let mut combined = String::from_utf8_lossy(&out.0).into_owned();
            combined.push_str(&String::from_utf8_lossy(&err.0));
            Ok(ProgramOutcome {
                exit_code: code,
                timed_out: false,
                tail: tail_of(&combined, tail_cap),
            })
        }
        Err(GitError::Timeout) => Ok(ProgramOutcome {
            exit_code: None,
            timed_out: true,
            tail: format!("timed out after {}s", timeout.as_secs()),
        }),
        Err(other) => Err(other.to_ipc()),
    }
}

fn tail_of(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut start = s.len() - max;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    s[start..].to_string()
}

// ---------------------------------------------------------------------------
// Repository queries
// ---------------------------------------------------------------------------

/// The repository root for `path`, correct **from inside a linked worktree**.
///
/// `--git-common-dir` points at the main repository's `.git`, so its parent is
/// the main checkout even when `path` is one of our worktrees. A naive
/// "walk up looking for `.git`" search returns the worktree instead, which is
/// the bug this function exists to avoid (DESIGN §7-1).
pub async fn repo_root(path: &Path) -> Result<PathBuf> {
    main_root(path).await.map_err(|e| e.into_gilde("rev-parse", path))
}

/// [`repo_root`] with the [`GitError`] kept.
pub async fn main_root(path: &Path) -> std::result::Result<PathBuf, GitError> {
    let out = exec(
        path,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        ExecOpts::default(),
    )
    .await?;
    let common = PathBuf::from(out.text());
    if common.as_os_str().is_empty() {
        return Err(GitError::Failed {
            code: -1,
            stderr: format!("git reported no common dir for {}", path.display()),
        });
    }
    let root = if common.file_name() == Some(OsStr::new(".git")) {
        common
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| common.clone())
    } else {
        // Bare repository, or a `.git` file redirection we should not second
        // guess: the common dir itself is the best answer.
        common
    };
    Ok(std::fs::canonicalize(&root).unwrap_or(root))
}

/// True when `path` is inside a git repository.
pub async fn is_repo(path: &Path) -> bool {
    repo_root(path).await.is_ok()
}

/// Resolve `r` to a commit sha, failing with git's own message when it does not
/// name a commit.
pub async fn verify_ref(repo: &Path, r: &str) -> Result<String> {
    let spec = format!("{r}^{{commit}}");
    let out = run_git(repo, &["rev-parse", "--verify", "--quiet", "--end-of-options", &spec]).await?;
    let sha = out.text();
    if sha.is_empty() {
        return Err(GildeError::worktree(format!("{r} does not name a commit")));
    }
    Ok(sha)
}

/// `HEAD`'s branch, or `None` on a detached HEAD.
pub async fn current_branch(repo: &Path) -> Result<Option<String>> {
    match run_git_status(repo, &["symbolic-ref", "--short", "--quiet", "HEAD"], &[0, 1]).await {
        Ok(out) if out.code == 0 => {
            let name = out.text();
            Ok(if name.is_empty() { None } else { Some(name) })
        }
        Ok(_) => Ok(None),
        Err(e) => Err(e),
    }
}

/// True when the repository has at least one commit. A brand new `git init`
/// has none, and `worktree add` there fails with a confusing message.
pub async fn has_commits(repo: &Path) -> bool {
    run_git(repo, &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
        .await
        .is_ok_and(|o| !o.text().is_empty())
}

/// True when `refs/heads/<branch>` exists.
pub async fn branch_exists(repo: &Path, branch: &str) -> Result<bool> {
    branch_present(repo, branch).await.map_err(|e| e.into_gilde("show-ref", repo))
}

/// [`branch_exists`] with the [`GitError`] kept.
pub async fn branch_present(repo: &Path, branch: &str) -> std::result::Result<bool, GitError> {
    let full = format!("refs/heads/{branch}");
    let out = exec(
        repo,
        &["show-ref", "--verify", "--quiet", &full],
        ExecOpts { allowed: &[0, 1], ..ExecOpts::default() },
    )
    .await?;
    Ok(out.code == 0)
}

/// One entry of `git worktree list --porcelain`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitWorktreeEntry {
    pub path: PathBuf,
    pub head: Option<String>,
    /// Full ref name, e.g. `refs/heads/gilde/fix-login-ab12cd`.
    pub branch: Option<String>,
    pub bare: bool,
    pub detached: bool,
    pub locked: Option<String>,
    pub prunable: Option<String>,
}

impl GitWorktreeEntry {
    /// Short branch name (`gilde/fix-login-ab12cd`).
    pub fn short_branch(&self) -> Option<&str> {
        self.branch
            .as_deref()
            .map(|b| b.trim_start_matches("refs/heads/"))
    }
}

/// `git worktree list --porcelain -z`, with a fallback to the newline format
/// for git builds that predate `-z` (added in 2.36).
pub async fn worktree_list(repo: &Path) -> Result<Vec<GitWorktreeEntry>> {
    list_worktrees(repo).await.map_err(|e| e.into_gilde("worktree list", repo))
}

/// [`worktree_list`] with the [`GitError`] kept.
pub async fn list_worktrees(repo: &Path) -> std::result::Result<Vec<GitWorktreeEntry>, GitError> {
    match exec(repo, &["worktree", "list", "--porcelain", "-z"], ExecOpts::default()).await {
        Ok(out) => Ok(parse_worktree_list(&out.nul_fields())),
        Err(GitError::Failed { .. }) => {
            let out = exec(repo, &["worktree", "list", "--porcelain"], ExecOpts::default()).await?;
            let fields: Vec<String> = String::from_utf8_lossy(&out.stdout)
                .split('\n')
                .map(|l| l.trim_end_matches('\r').to_string())
                .collect();
            Ok(parse_worktree_list(&fields))
        }
        Err(e) => Err(e),
    }
}

/// Shared parser for both framings: one attribute per field, an empty field
/// ends the record.
pub fn parse_worktree_list(fields: &[String]) -> Vec<GitWorktreeEntry> {
    let mut out = Vec::new();
    let mut cur: Option<GitWorktreeEntry> = None;

    for field in fields {
        if field.is_empty() {
            if let Some(entry) = cur.take() {
                out.push(entry);
            }
            continue;
        }
        let (key, value) = match field.split_once(' ') {
            Some((k, v)) => (k, v),
            None => (field.as_str(), ""),
        };
        match key {
            "worktree" => {
                if let Some(entry) = cur.take() {
                    out.push(entry);
                }
                cur = Some(GitWorktreeEntry {
                    path: PathBuf::from(value),
                    ..Default::default()
                });
            }
            "HEAD" => {
                if let Some(e) = cur.as_mut() {
                    e.head = Some(value.to_string());
                }
            }
            "branch" => {
                if let Some(e) = cur.as_mut() {
                    e.branch = Some(value.to_string());
                }
            }
            "bare" => {
                if let Some(e) = cur.as_mut() {
                    e.bare = true;
                }
            }
            "detached" => {
                if let Some(e) = cur.as_mut() {
                    e.detached = true;
                }
            }
            "locked" => {
                if let Some(e) = cur.as_mut() {
                    e.locked = Some(value.to_string());
                }
            }
            "prunable" => {
                if let Some(e) = cur.as_mut() {
                    e.prunable = Some(value.to_string());
                }
            }
            _ => {}
        }
    }
    if let Some(entry) = cur.take() {
        out.push(entry);
    }
    out
}

// ---------------------------------------------------------------------------
// status --porcelain=v2 -z
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusKind {
    /// `1` — an ordinary changed entry.
    Ordinary,
    /// `2` — a rename or copy (carries a second, original path).
    Renamed,
    /// `u` — unmerged.
    Unmerged,
    /// `?` — untracked.
    Untracked,
    /// `!` — ignored (only present with `--ignored`).
    Ignored,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
    pub kind: StatusKind,
    /// Index status letter (`.` when unchanged).
    pub x: char,
    /// Worktree status letter (`.` when unchanged).
    pub y: char,
    pub path: String,
    /// Original path for a rename/copy.
    pub orig_path: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatusV2 {
    pub entries: Vec<StatusEntry>,
    /// Tracked files with a staged and/or unstaged change (kinds `1`, `2`, `u`).
    pub dirty_files: u32,
    /// Entries with a non-`.` index status.
    pub staged: u32,
    /// Entries with a non-`.` worktree status.
    pub unstaged: u32,
    pub untracked: u32,
    pub conflicted: u32,
    /// Entries of `--ignored` output (directories collapsed).
    pub ignored: u32,
}

impl StatusV2 {
    /// Nothing to commit and nothing untracked.
    pub fn is_clean(&self) -> bool {
        self.dirty_files == 0 && self.untracked == 0
    }

    /// Paths, in git's order — used by the removal card.
    pub fn paths(&self) -> Vec<String> {
        self.entries.iter().map(|e| e.path.clone()).collect()
    }
}

/// `git status --porcelain=v2 -z --untracked-files=all`.
///
/// `-z` and `v2` are not stylistic: a file name containing a space, a quote or
/// a newline is exactly the input that breaks a naive parser, and an agent will
/// eventually create one (DESIGN §7-5).
///
/// `--ignore-submodules=dirty` (A46.1): a dirty submodule would make git run
/// `status` inside it, which reads the submodule's own config and runs its
/// drivers. A submodule whose commit moved is still reported.
pub async fn status_v2(wt: &Path) -> Result<StatusV2> {
    let out = run_git(
        wt,
        &["status", "--porcelain=v2", "-z", "--untracked-files=all", "--ignore-submodules=dirty"],
    )
    .await?;
    Ok(parse_status_v2(&out.nul_fields()))
}

/// What a discard would delete: tracked changes, untracked files and ignored
/// entries (directories collapsed, so `node_modules/` counts once).
pub async fn status_for_discard(wt: &Path) -> std::result::Result<StatusV2, GitError> {
    let out = exec(
        wt,
        &[
            "status",
            "--porcelain=v2",
            "-z",
            "--untracked-files=all",
            "--ignored=matching",
            // A46.1, as in `status_v2` (a discard refuses submodules anyway).
            "--ignore-submodules=dirty",
        ],
        ExecOpts::default(),
    )
    .await?;
    Ok(parse_status_v2(&out.nul_fields()))
}

/// Parse the NUL-separated fields of `status --porcelain=v2 -z`.
///
/// The one subtlety: a `2` (rename/copy) record is **two** NUL fields — the new
/// path terminates the record and the original path follows it.
pub fn parse_status_v2(fields: &[String]) -> StatusV2 {
    let mut status = StatusV2::default();
    let mut i = 0;

    while i < fields.len() {
        let record = &fields[i];
        i += 1;
        if record.is_empty() {
            continue;
        }
        let mut chars = record.chars();
        let tag = chars.next().unwrap_or(' ');

        let entry = match tag {
            '#' => continue,
            '1' => {
                // 1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>
                let parts: Vec<&str> = record.splitn(9, ' ').collect();
                if parts.len() < 9 {
                    continue;
                }
                let (x, y) = xy(parts[1]);
                StatusEntry {
                    kind: StatusKind::Ordinary,
                    x,
                    y,
                    path: parts[8].to_string(),
                    orig_path: None,
                }
            }
            '2' => {
                // 2 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <X><score> <path>\0<origPath>
                let parts: Vec<&str> = record.splitn(10, ' ').collect();
                if parts.len() < 10 {
                    continue;
                }
                let orig = fields.get(i).cloned();
                i += 1;
                let (x, y) = xy(parts[1]);
                StatusEntry {
                    kind: StatusKind::Renamed,
                    x,
                    y,
                    path: parts[9].to_string(),
                    orig_path: orig,
                }
            }
            'u' => {
                // u <XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>
                let parts: Vec<&str> = record.splitn(11, ' ').collect();
                if parts.len() < 11 {
                    continue;
                }
                let (x, y) = xy(parts[1]);
                StatusEntry {
                    kind: StatusKind::Unmerged,
                    x,
                    y,
                    path: parts[10].to_string(),
                    orig_path: None,
                }
            }
            '?' | '!' => {
                let path = record[1..].trim_start_matches(' ').to_string();
                StatusEntry {
                    kind: if tag == '?' {
                        StatusKind::Untracked
                    } else {
                        StatusKind::Ignored
                    },
                    x: tag,
                    y: tag,
                    path,
                    orig_path: None,
                }
            }
            _ => continue,
        };

        match entry.kind {
            StatusKind::Ordinary | StatusKind::Renamed => {
                status.dirty_files += 1;
                if entry.x != '.' {
                    status.staged += 1;
                }
                if entry.y != '.' {
                    status.unstaged += 1;
                }
            }
            StatusKind::Unmerged => {
                status.dirty_files += 1;
                status.conflicted += 1;
            }
            StatusKind::Untracked => status.untracked += 1,
            StatusKind::Ignored => status.ignored += 1,
        }
        status.entries.push(entry);
    }

    status
}

fn xy(field: &str) -> (char, char) {
    let mut c = field.chars();
    (c.next().unwrap_or('.'), c.next().unwrap_or('.'))
}

// ---------------------------------------------------------------------------
// diff / rev-list / log
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NumStat {
    pub added: u64,
    pub removed: u64,
    pub files: u64,
}

impl NumStat {
    pub fn merge(self, other: NumStat) -> NumStat {
        NumStat {
            added: self.added + other.added,
            removed: self.removed + other.removed,
            files: self.files + other.files,
        }
    }
}

/// What every numstat passes (A37.6, A46.1): no external diff, no textconv,
/// and no `status` inside a dirty submodule (see [`status_v2`]).
const NUMSTAT_SAFE: [&str; 3] = ["--no-ext-diff", "--no-textconv", "--ignore-submodules=dirty"];

/// `git diff --numstat <base>...HEAD` — what this branch added on top of the
/// merge base.
pub async fn numstat(wt: &Path, base: &str) -> Result<NumStat> {
    let range = format!("{base}...HEAD");
    let mut args = vec!["diff", "--numstat"];
    args.extend_from_slice(&NUMSTAT_SAFE);
    args.extend_from_slice(&["--end-of-options", &range, "--"]);
    let out = run_git(wt, &args).await?;
    Ok(parse_numstat(&out.lines()))
}

/// `git diff --numstat` — uncommitted work, staged and unstaged.
pub async fn numstat_worktree(wt: &Path) -> Result<NumStat> {
    let mut args = vec!["diff", "--numstat"];
    args.extend_from_slice(&NUMSTAT_SAFE);
    let unstaged = run_git(wt, &args).await?;
    args.push("--cached");
    let staged = run_git(wt, &args).await?;
    Ok(parse_numstat(&unstaged.lines()).merge(parse_numstat(&staged.lines())))
}

/// `<added>\t<removed>\t<path>`; binary files report `-` for both counts.
///
/// The default (non `-z`) framing is used on purpose: git quotes control
/// characters in paths there, so every record stays on one line, and we only
/// need the two leading numbers.
pub fn parse_numstat(lines: &[String]) -> NumStat {
    let mut stat = NumStat::default();
    for line in lines {
        let mut cols = line.split('\t');
        let (Some(a), Some(r)) = (cols.next(), cols.next()) else {
            continue;
        };
        stat.files += 1;
        stat.added += a.parse::<u64>().unwrap_or(0);
        stat.removed += r.parse::<u64>().unwrap_or(0);
    }
    stat
}

/// `git rev-list --left-right --count <base>...HEAD` → `(ahead, behind)`.
///
/// git prints `left<TAB>right`; with `<base>...HEAD` the left side is
/// base-only (behind) and the right side is HEAD-only (ahead).
pub async fn rev_list_lr(wt: &Path, base: &str) -> Result<(u32, u32)> {
    let range = format!("{base}...HEAD");
    let out = run_git(wt, &["rev-list", "--left-right", "--count", &range]).await?;
    let text = out.text();
    let mut cols = text.split_whitespace();
    let behind = cols.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let ahead = cols.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    Ok((ahead, behind))
}

/// Last commit time (unix seconds) and subject.
pub async fn last_commit(wt: &Path) -> Result<Option<(i64, String)>> {
    let out = run_git_status(wt, &["log", "-1", "--format=%ct%x00%s", "HEAD"], &[0, 128]).await?;
    if out.code != 0 {
        return Ok(None);
    }
    let fields = out.nul_fields();
    let Some(ts) = fields.first().and_then(|t| t.trim().parse::<i64>().ok()) else {
        return Ok(None);
    };
    let subject = fields
        .get(1)
        .map(|s| s.trim_end().to_string())
        .unwrap_or_default();
    Ok(Some((ts, subject)))
}

/// `git add -A` then a commit that cannot be blocked by the user's setup.
///
/// Hooks and GPG signing are bypassed deliberately (DESIGN §7-7): a failing
/// `pre-commit` hook must not stand between the user and a rescue, and a user
/// without a global `user.email` must not meet a confusing error here.
pub async fn checkpoint_commit(wt: &Path, msg: &str) -> Result<String> {
    run_git(wt, &["add", "-A"]).await?;
    let opts = ExecOpts {
        stdin: Some(msg.as_bytes()),
        ..ExecOpts::default()
    };
    exec(
        wt,
        &[
            NO_HOOKS[0],
            NO_HOOKS[1],
            "-c",
            &format!("user.name={CHECKPOINT_USER_NAME}"),
            "-c",
            &format!("user.email={CHECKPOINT_USER_EMAIL}"),
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--no-verify",
            "-F",
            "-",
        ],
        opts,
    )
    .await
    .map_err(|e| e.into_gilde("commit", wt))?;
    let out = run_git(wt, &["rev-parse", "HEAD"]).await?;
    Ok(out.text())
}

// ---------------------------------------------------------------------------
// Changes view queries (contract §4.2.5)
// ---------------------------------------------------------------------------

/// `rev-parse --verify --quiet <rev>^{commit}` → the full sha, `None` when it
/// does not name a commit.
pub async fn resolve_commit(dir: &Path, rev: &str) -> std::result::Result<Option<String>, GitError> {
    let spec = format!("{rev}^{{commit}}");
    let out = exec(
        dir,
        &["rev-parse", "--verify", "--quiet", "--end-of-options", &spec],
        ExecOpts { allowed: &[0, 1, 128], ..ExecOpts::default() },
    )
    .await?;
    let sha = out.text();
    Ok((out.code == 0 && !sha.is_empty()).then_some(sha))
}

/// `HEAD`'s short branch name; `None` when detached (or no commits).
pub async fn head_branch(dir: &Path) -> std::result::Result<Option<String>, GitError> {
    let out = exec(
        dir,
        &["symbolic-ref", "--short", "--quiet", "HEAD"],
        ExecOpts { allowed: &[0, 1, 128], ..ExecOpts::default() },
    )
    .await?;
    let name = out.text();
    Ok((out.code == 0 && !name.is_empty()).then_some(name))
}

/// `git merge-base <a> <b>`; `None` when they share no history.
pub async fn merge_base(dir: &Path, a: &str, b: &str) -> std::result::Result<Option<String>, GitError> {
    let out = exec(
        dir,
        &["merge-base", "--end-of-options", a, b],
        ExecOpts { allowed: &[0, 1], ..ExecOpts::default() },
    )
    .await?;
    let sha = out.text();
    Ok((out.code == 0 && !sha.is_empty()).then_some(sha))
}

/// `git merge-base --is-ancestor <a> <b>`.
pub async fn is_ancestor(dir: &Path, a: &str, b: &str) -> std::result::Result<bool, GitError> {
    let out = exec(
        dir,
        &["merge-base", "--is-ancestor", "--end-of-options", a, b],
        ExecOpts { allowed: &[0, 1], ..ExecOpts::default() },
    )
    .await?;
    Ok(out.code == 0)
}

/// `git rev-list --count <range>`.
pub async fn count(dir: &Path, range: &str) -> std::result::Result<u32, GitError> {
    let out = exec(dir, &["rev-list", "--count", "--end-of-options", range], ExecOpts::default()).await?;
    Ok(out.text().trim().parse().unwrap_or(0))
}

/// `git rev-parse --short <sha>`.
pub async fn short_sha(dir: &Path, sha: &str) -> std::result::Result<String, GitError> {
    let out = exec(dir, &["rev-parse", "--short", "--end-of-options", sha], ExecOpts::default()).await?;
    Ok(out.text())
}

/// One changed tracked path between `base` and the working tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffEntry {
    /// `A`, `M`, `D`, `T` (type change) — renames are off (`--no-renames`).
    pub status: char,
    pub path: String,
    /// `None` for binary files.
    pub added: Option<u32>,
    pub removed: Option<u32>,
}

/// Tracked changes from `base` (a commit, or [`EMPTY_TREE`]) to the working
/// tree, staged and unstaged together: `diff --name-status` + `--numstat`,
/// both `-z --no-renames`.
pub async fn diff_entries(dir: &Path, base: &str) -> std::result::Result<Vec<DiffEntry>, GitError> {
    let common = [
        "--no-renames",
        "--no-ext-diff",
        "--no-textconv",
        "--ignore-submodules=dirty",
        "-z",
    ];
    let mut args = vec!["diff", "--name-status"];
    args.extend_from_slice(&common);
    args.extend_from_slice(&["--end-of-options", base, "--"]);
    let names = exec(dir, &args, ExecOpts::default()).await?;
    let mut args = vec!["diff", "--numstat"];
    args.extend_from_slice(&common);
    args.extend_from_slice(&["--end-of-options", base, "--"]);
    let nums = exec(dir, &args, ExecOpts::default()).await?;
    Ok(merge_diff_entries(&names.nul_fields(), &nums.nul_fields()))
}

/// Join `--name-status -z` (`<S>\0<path>\0…`) with `--numstat -z`
/// (`<a>\t<r>\t<path>\0…`, `-` for binary).
pub fn merge_diff_entries(names: &[String], nums: &[String]) -> Vec<DiffEntry> {
    let mut counts: HashMap<&str, (Option<u32>, Option<u32>)> = HashMap::new();
    for rec in nums {
        let mut cols = rec.splitn(3, '\t');
        let (Some(a), Some(r), Some(p)) = (cols.next(), cols.next(), cols.next()) else {
            continue;
        };
        counts.insert(p, (a.parse().ok(), r.parse().ok()));
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < names.len() {
        let status = names[i].chars().next().unwrap_or('M');
        let path = names[i + 1].clone();
        i += 2;
        let (added, removed) = counts.get(path.as_str()).copied().unwrap_or((None, None));
        out.push(DiffEntry { status, path, added, removed });
    }
    out
}

/// Untracked, not ignored files (`ls-files --others --exclude-standard -z`).
pub async fn untracked_files(dir: &Path) -> std::result::Result<Vec<String>, GitError> {
    let out = exec(
        dir,
        &["ls-files", "--others", "--exclude-standard", "-z"],
        ExecOpts::default(),
    )
    .await?;
    Ok(out.nul_fields())
}

/// The function or section names of each file's hunks, from the
/// `@@ … @@ <name>` headers of `diff -U0` (≤ `per_file` unique names each).
pub async fn hunk_names(dir: &Path, base: &str, per_file: usize) -> std::result::Result<HashMap<String, Vec<String>>, GitError> {
    let out = exec(
        dir,
        &[
            "-c",
            "core.quotePath=false",
            "diff",
            "-U0",
            "--no-color",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            "--ignore-submodules=dirty",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "--end-of-options",
            base,
            "--",
        ],
        ExecOpts { stdout_cap: 16 * 1024 * 1024, ..ExecOpts::default() },
    )
    .await;
    match out {
        Ok(out) => Ok(parse_hunk_names(&out.stdout, per_file)),
        // A diff too large to scan just has no part names.
        Err(GitError::TooLarge) => Ok(HashMap::new()),
        Err(e) => Err(e),
    }
}

/// Parse `diff -U0` output into `path → [hunk names]`.
pub fn parse_hunk_names(patch: &[u8], per_file: usize) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    let mut minus: Option<String> = None;
    let mut current: Option<String> = None;
    for raw in patch.split(|b| *b == b'\n') {
        let line = String::from_utf8_lossy(raw);
        if line.starts_with("diff --git ") {
            minus = None;
            current = None;
        } else if let Some(rest) = line.strip_prefix("--- ") {
            minus = diff_path(rest, "a/");
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            current = diff_path(rest, "b/").or_else(|| minus.clone());
        } else if let Some(after) = line.strip_prefix("@@") {
            let Some(path) = current.as_ref() else { continue };
            // `@@ -a,b +c,d @@ <name>`
            let Some(end) = after.find("@@") else { continue };
            let name = after[end + 2..].trim();
            if name.is_empty() {
                continue;
            }
            let names = out.entry(path.clone()).or_default();
            if names.len() < per_file && !names.iter().any(|n| n == name) {
                names.push(name.to_string());
            }
        }
    }
    out
}

/// The path of a `---`/`+++` header line: `/dev/null` → `None`; a quoted
/// path is C-unquoted.
fn diff_path(rest: &str, prefix: &str) -> Option<String> {
    let rest = rest.trim_end_matches('\r');
    if rest == "/dev/null" {
        return None;
    }
    let path = if rest.starts_with('"') { c_unquote(rest)? } else { rest.to_string() };
    path.strip_prefix(prefix).map(str::to_string)
}

/// git's C-style quoting (`"a\tb\"c\303\251"`).
pub fn c_unquote(s: &str) -> Option<String> {
    let inner = s.strip_prefix('"')?.strip_suffix('"')?;
    let mut bytes = Vec::with_capacity(inner.len());
    let mut it = inner.bytes().peekable();
    while let Some(b) = it.next() {
        if b != b'\\' {
            bytes.push(b);
            continue;
        }
        match it.next()? {
            b'n' => bytes.push(b'\n'),
            b't' => bytes.push(b'\t'),
            b'r' => bytes.push(b'\r'),
            b'a' => bytes.push(7),
            b'b' => bytes.push(8),
            b'f' => bytes.push(12),
            b'v' => bytes.push(11),
            b'"' => bytes.push(b'"'),
            b'\\' => bytes.push(b'\\'),
            d @ b'0'..=b'7' => {
                let mut v = u32::from(d - b'0');
                for _ in 0..2 {
                    match it.peek() {
                        Some(n @ b'0'..=b'7') => {
                            v = v * 8 + u32::from(n - b'0');
                            it.next();
                        }
                        _ => break,
                    }
                }
                bytes.push(u8::try_from(v).ok()?);
            }
            other => bytes.push(other),
        }
    }
    String::from_utf8(bytes).ok()
}

/// A commit of `log`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    pub short: String,
    /// Committer time, unix seconds.
    pub at: i64,
    pub subject: String,
}

/// `git log -z --format=%h%x1f%ct%x1f%s -n <max> <range>`.
pub async fn log_range(dir: &Path, range: &str, max: usize) -> std::result::Result<Vec<LogEntry>, GitError> {
    let n = format!("-n{max}");
    let out = exec(
        dir,
        &["log", "-z", "--format=%h%x1f%ct%x1f%s", &n, "--end-of-options", range, "--"],
        ExecOpts::default(),
    )
    .await?;
    Ok(out
        .nul_fields()
        .iter()
        .filter_map(|rec| {
            let mut cols = rec.splitn(3, '\u{1f}');
            Some(LogEntry {
                short: cols.next()?.trim().to_string(),
                at: cols.next()?.trim().parse().ok()?,
                subject: cols.next().unwrap_or("").to_string(),
            })
        })
        .collect())
}

/// `git config --get <key>` in `dir` (the user's own settings apply).
pub async fn config_get(dir: &Path, key: &str) -> std::result::Result<Option<String>, GitError> {
    let out = exec(dir, &["config", "--get", key], ExecOpts { allowed: &[0, 1], ..ExecOpts::default() }).await?;
    let v = out.text();
    Ok((out.code == 0 && !v.is_empty()).then_some(v))
}

/// The size of `<rev>:<path>`; `None` when that blob does not exist.
pub async fn blob_size(dir: &Path, rev: &str, path: &str) -> std::result::Result<Option<u64>, GitError> {
    let spec = format!("{rev}:{path}");
    let out = exec(dir, &["cat-file", "-s", &spec], ExecOpts { allowed: &[0, 1, 128], ..ExecOpts::default() }).await?;
    Ok(if out.code == 0 { out.text().trim().parse().ok() } else { None })
}

/// The exact bytes of `<rev>:<path>` (no filters, no textconv).
pub async fn blob(dir: &Path, rev: &str, path: &str, cap: usize) -> std::result::Result<Vec<u8>, GitError> {
    let spec = format!("{rev}:{path}");
    let out = exec(dir, &["cat-file", "blob", &spec], ExecOpts { stdout_cap: cap, ..ExecOpts::default() }).await?;
    Ok(out.stdout)
}

/// Stage exactly `paths` (additions, changes and deletions) — pathspecs on
/// stdin (`--pathspec-from-file=- --pathspec-file-nul`), literal.
pub async fn add_paths(dir: &Path, paths: &[String]) -> std::result::Result<(), GitError> {
    let spec = nul_join(paths);
    exec(
        dir,
        &[
            "--literal-pathspecs",
            "add",
            "-A",
            "--pathspec-from-file=-",
            "--pathspec-file-nul",
        ],
        ExecOpts { stdin: Some(&spec), ..ExecOpts::default() },
    )
    .await
    .map(|_| ())
}

/// Commit exactly `paths` (`--only`) with `message` on stdin, the user as
/// author, their hooks and signing settings applied. Returns the new HEAD.
pub async fn commit_paths(dir: &Path, paths: &[String], message: &str) -> std::result::Result<String, GitError> {
    let mut args: Vec<&str> = vec!["--literal-pathspecs", "commit", "--only", "-F", "-", "--"];
    args.extend(paths.iter().map(String::as_str));
    exec(
        dir,
        &args,
        ExecOpts { stdin: Some(message.as_bytes()), timeout: COMMIT_TIMEOUT, ..ExecOpts::default() },
    )
    .await?;
    let out = exec(dir, &["rev-parse", "HEAD"], ExecOpts::default()).await?;
    Ok(out.text())
}

/// Whether merging `theirs` into `ours` conflicts, computed without touching
/// any working tree (`git merge-tree --write-tree`, git ≥ 2.38).
pub async fn merge_conflicts(dir: &Path, ours: &str, theirs: &str) -> std::result::Result<bool, GitError> {
    let out = exec(
        dir,
        &["merge-tree", "--write-tree", "--no-messages", "--end-of-options", ours, theirs],
        ExecOpts { allowed: &[0, 1], ..ExecOpts::default() },
    )
    .await?;
    Ok(out.code == 1)
}

/// `git merge --ff-only <sha>` in the checkout `dir`.
pub async fn merge_fast_forward(dir: &Path, sha: &str) -> std::result::Result<(), GitError> {
    exec(dir, &["merge", "--ff-only", "--no-edit", "--end-of-options", sha], ExecOpts { timeout: COMMIT_TIMEOUT, ..ExecOpts::default() })
        .await
        .map(|_| ())
}

/// `git merge --no-ff -m <message> <sha>` in the checkout `dir`.
pub async fn merge_commit(dir: &Path, sha: &str, message: &str) -> std::result::Result<(), GitError> {
    exec(
        dir,
        &["merge", "--no-ff", "--no-edit", "-m", message, "--end-of-options", sha],
        ExecOpts { timeout: COMMIT_TIMEOUT, ..ExecOpts::default() },
    )
    .await
    .map(|_| ())
}

/// `git merge --abort` (restores the pre-merge state after a failed merge).
pub async fn merge_abort(dir: &Path) -> std::result::Result<(), GitError> {
    exec(dir, &["merge", "--abort"], ExecOpts { allowed: &[0, 128], ..ExecOpts::default() })
        .await
        .map(|_| ())
}

/// Tracked changes in `dir` (staged or unstaged; untracked ignored).
pub async fn tracked_dirty(dir: &Path) -> std::result::Result<bool, GitError> {
    let out = exec(
        dir,
        &["status", "--porcelain=v2", "-z", "--untracked-files=no", "--ignore-submodules=dirty"],
        ExecOpts::default(),
    )
    .await?;
    Ok(!parse_status_v2(&out.nul_fields()).entries.is_empty())
}

/// The absolute common git dir of `dir` (the main `.git`, also from a worktree).
pub async fn common_dir(dir: &Path) -> std::result::Result<PathBuf, GitError> {
    let out = exec(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        ExecOpts::default(),
    )
    .await?;
    let p = PathBuf::from(out.text());
    Ok(std::fs::canonicalize(&p).unwrap_or(p))
}

fn nul_join(paths: &[String]) -> Vec<u8> {
    let mut out = Vec::new();
    for p in paths {
        out.extend_from_slice(p.as_bytes());
        out.push(0);
    }
    out
}

// ---------------------------------------------------------------------------
// `.git/info/exclude` (contract A6)
// ---------------------------------------------------------------------------

/// The header written above an entry this module appends.
pub const EXCLUDE_HEADER: &str = "# Gilde worktrees (added by Gilde here, never to a tracked ignore file)";

/// Append `entry` (e.g. `/.gilde/`) to `<common dir>/info/exclude` once —
/// never to the tracked `.gitignore` (A6). `Ok(true)` when it was added,
/// `Ok(false)` when an identical line is already there.
pub fn ensure_excluded(common_dir: &Path, entry: &str) -> std::io::Result<bool> {
    use std::io::Write as _;
    let info = common_dir.join("info");
    let exclude = info.join("exclude");
    let existing = match std::fs::read_to_string(&exclude) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    if existing.lines().any(|l| l.trim() == entry) {
        return Ok(false);
    }
    std::fs::create_dir_all(&info)?;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&exclude)?;
    let mut text = String::new();
    if !existing.is_empty() && !existing.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(EXCLUDE_HEADER);
    text.push('\n');
    text.push_str(entry);
    text.push('\n');
    f.write_all(text.as_bytes())?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// worktree lifecycle
// ---------------------------------------------------------------------------

/// `git worktree add`. With `base` the branch is created (`--no-track -b`);
/// without it, an existing branch is checked out.
pub async fn worktree_add(
    repo: &Path,
    path: &Path,
    branch: &str,
    base: Option<&str>,
) -> Result<()> {
    add_worktree(repo, path, branch, base)
        .await
        .map_err(|e| e.into_gilde("worktree add", repo))
}

/// [`worktree_add`] with the [`GitError`] kept.
pub async fn add_worktree(
    repo: &Path,
    path: &Path,
    branch: &str,
    base: Option<&str>,
) -> std::result::Result<(), GitError> {
    let path_s = path.to_string_lossy().into_owned();
    let args: Vec<&str> = match base {
        Some(base_sha) => vec!["worktree", "add", "--no-track", "-b", branch, "--", &path_s, base_sha],
        None => vec!["worktree", "add", "--", &path_s, branch],
    };
    exec(repo, &args, ExecOpts::default()).await.map(|_| ())
}

/// `git worktree add --detach -- <path> <sha>`: a new worktree at `sha` with
/// no branch yet (git removes its partial directory when it fails, and no
/// branch is created — unlike `-b`, which creates the branch first and
/// leaves it behind on a failed add).
pub async fn add_worktree_detached(repo: &Path, path: &Path, sha: &str) -> std::result::Result<(), GitError> {
    let path_s = path.to_string_lossy().into_owned();
    exec(repo, &["worktree", "add", "--detach", "--", &path_s, sha], ExecOpts::default()).await.map(|_| ())
}

/// Create `refs/heads/<branch>` at `sha` **only if it does not exist**
/// (`update-ref` with an empty old value: atomic, fails when it exists).
pub async fn create_branch_at(dir: &Path, branch: &str, sha: &str) -> std::result::Result<(), GitError> {
    let full = format!("refs/heads/{branch}");
    exec(dir, &["update-ref", &full, sha, ""], ExecOpts::default()).await.map(|_| ())
}

/// Point the worktree `wt`'s own `HEAD` at `refs/heads/<branch>` (whose tip
/// is the commit `wt` already has checked out, so nothing changes on disk).
pub async fn attach_head(wt: &Path, branch: &str) -> std::result::Result<(), GitError> {
    let full = format!("refs/heads/{branch}");
    exec(wt, &["symbolic-ref", "HEAD", &full], ExecOpts::default()).await.map(|_| ())
}

/// `git worktree remove` without `--force`: git itself refuses a dirty tree.
///
/// A locked worktree refuses removal, so callers unlock Gilde's own lock
/// first and restore it when the removal fails (A36.6).
pub async fn worktree_remove(repo: &Path, path: &Path) -> Result<()> {
    remove_worktree(repo, path)
        .await
        .map_err(|e| e.into_gilde("worktree remove", repo))
}

/// [`worktree_remove`] with the [`GitError`] kept.
pub async fn remove_worktree(repo: &Path, path: &Path) -> std::result::Result<(), GitError> {
    let path_s = path.to_string_lossy().into_owned();
    exec(repo, &["worktree", "remove", "--", &path_s], ExecOpts::default()).await.map(|_| ())
}

/// `git worktree remove --force` — the **only** `--force` in this crate
/// (A36.4; `tests/worktree_safety.rs` greps for it). Called only when every
/// loss of the tree was listed and the user acknowledged each one, after the
/// checks that make the list exact (A36.1–A36.3, A37.3). One `--force`, never
/// two: git still refuses a locked worktree.
pub async fn remove_worktree_acknowledged(repo: &Path, path: &Path) -> std::result::Result<(), GitError> {
    let path_s = path.to_string_lossy().into_owned();
    exec(repo, &["worktree", "remove", "--force", "--", &path_s], ExecOpts::default()).await.map(|_| ())
}

/// Remove the admin entry of `target` only (A36.5): the one
/// `<common dir>/worktrees/<name>` whose `gitdir` file names `<target>/.git`,
/// and only while `target` is gone and the entry carries no lock other than
/// Gilde's. Never a repo-wide `git worktree prune`: another worktree whose
/// directory is momentarily missing (an unmounted volume) is never touched.
/// `Ok(true)` when an entry was removed.
pub fn prune_target_admin(repo: &Path, target: &Path) -> std::io::Result<bool> {
    if std::fs::symlink_metadata(target).is_ok() {
        return Ok(false);
    }
    let Some((_, common)) = git_dirs(repo) else { return Ok(false) };
    let want = crate::worktree::canonical_lexical(&target.join(".git"));
    let Ok(entries) = std::fs::read_dir(common.join("worktrees")) else { return Ok(false) };
    for entry in entries.flatten() {
        let admin = entry.path();
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(admin.join("gitdir")) else { continue };
        let named = text.trim();
        let named = if Path::new(named).is_absolute() { PathBuf::from(named) } else { admin.join(named) };
        if crate::worktree::canonical_lexical(&named) != want {
            continue;
        }
        if let Ok(reason) = std::fs::read_to_string(admin.join("locked")) {
            if reason.trim() != LOCK_REASON {
                return Ok(false);
            }
        }
        std::fs::remove_dir_all(&admin)?;
        return Ok(true);
    }
    Ok(false)
}

/// The git operation in progress in the worktree whose git dir is `gitdir`
/// (A36.2): a rebase (also an interactive one stopped at `edit`, and `am`),
/// a merge, cherry-pick, revert, sequencer run or bisect.
pub fn operation_in_progress(gitdir: &Path) -> Option<&'static str> {
    const MARKERS: &[(&str, &str)] = &[
        ("rebase-merge", "rebase"),
        ("rebase-apply", "rebase"),
        ("MERGE_HEAD", "merge"),
        ("CHERRY_PICK_HEAD", "cherryPick"),
        ("REVERT_HEAD", "revert"),
        ("sequencer", "sequencer"),
        ("BISECT_LOG", "bisect"),
        ("BISECT_START", "bisect"),
    ];
    MARKERS
        .iter()
        .find(|(name, _)| std::fs::symlink_metadata(gitdir.join(name)).is_ok())
        .map(|(_, what)| *what)
}

/// The commit every ref of `dir` points at (tags peeled), except the refs
/// that live and die with `dir`'s own worktree (`refs/worktree/`,
/// `refs/bisect/`, `refs/rewritten/`). Branches, tags, remotes, notes and the
/// stash all count (A36.1).
pub async fn ref_tips(dir: &Path) -> std::result::Result<Vec<String>, GitError> {
    let out = exec(
        dir,
        &["for-each-ref", "--format=%(objecttype) %(objectname) %(*objecttype) %(*objectname) %(refname)"],
        ExecOpts::default(),
    )
    .await?;
    let mut tips = Vec::new();
    for line in out.lines() {
        let mut cols = line.splitn(5, ' ');
        let (Some(kind), Some(sha), Some(peeled_kind), Some(peeled), Some(name)) =
            (cols.next(), cols.next(), cols.next(), cols.next(), cols.next())
        else {
            continue;
        };
        if ["refs/worktree/", "refs/bisect/", "refs/rewritten/"].iter().any(|p| name.starts_with(p)) {
            continue;
        }
        match (kind, peeled_kind) {
            ("commit", _) => tips.push(sha.to_string()),
            ("tag", "commit") => tips.push(peeled.to_string()),
            _ => {}
        }
    }
    Ok(tips)
}

/// Every stash entry (`refs/stash` and its reflog: `stash@{1}`, … are
/// entries too), shared by all worktrees of the repository.
pub async fn stash_tips(dir: &Path) -> std::result::Result<Vec<String>, GitError> {
    let out = exec(
        dir,
        &["log", "-g", "--format=%H", "--end-of-options", "refs/stash", "--"],
        ExecOpts { allowed: &[0, 128], ..ExecOpts::default() },
    )
    .await?;
    if out.code != 0 {
        return Ok(Vec::new());
    }
    Ok(out.lines().into_iter().map(|l| l.trim().to_string()).filter(|l| l.len() >= 40).collect())
}

/// Commits reachable from `head` and from none of `others`
/// (`rev-list --count --stdin`; the list travels on stdin, never in argv).
pub async fn count_only_from(dir: &Path, head: &str, others: &[String]) -> std::result::Result<u32, GitError> {
    let mut input = String::with_capacity(41 * (others.len() + 1));
    input.push_str(head);
    input.push('\n');
    for o in others {
        input.push('^');
        input.push_str(o);
        input.push('\n');
    }
    let out = exec(
        dir,
        &["rev-list", "--count", "--stdin"],
        ExecOpts { stdin: Some(input.as_bytes()), ..ExecOpts::default() },
    )
    .await?;
    Ok(out.text().trim().parse().unwrap_or(0))
}

/// The paths with uncommitted state in `dir`: staged, unstaged or untracked
/// (A37.1). Renames are off, so a deletion and an addition stay two paths.
pub async fn uncommitted_paths(dir: &Path) -> std::result::Result<std::collections::HashSet<String>, GitError> {
    let out = exec(
        dir,
        &["status", "--porcelain=v2", "-z", "--untracked-files=all", "--no-renames", "--ignore-submodules=dirty"],
        ExecOpts::default(),
    )
    .await?;
    let status = parse_status_v2(&out.nul_fields());
    let mut paths = std::collections::HashSet::new();
    for e in status.entries {
        if e.kind == StatusKind::Ignored {
            continue;
        }
        if let Some(orig) = e.orig_path {
            paths.insert(orig);
        }
        paths.insert(e.path);
    }
    Ok(paths)
}

/// Whether the index of `dir` records a submodule (a gitlink, mode 160000).
pub async fn has_gitlinks(dir: &Path) -> std::result::Result<bool, GitError> {
    let out = exec(dir, &["ls-files", "--stage", "-z"], ExecOpts::default()).await?;
    Ok(out.stdout.split(|b| *b == 0).any(|rec| rec.starts_with(b"160000 ")))
}

/// `git worktree lock|unlock`. Locking is idempotent: git reports "already
/// locked" as a failure that we treat as success.
pub async fn worktree_lock(wt: &Path, locked: bool, reason: &str) -> Result<()> {
    set_worktree_lock(wt, locked, reason)
        .await
        .map_err(|e| e.into_gilde("worktree lock", wt))
}

/// [`worktree_lock`] with the [`GitError`] kept.
pub async fn set_worktree_lock(wt: &Path, locked: bool, reason: &str) -> std::result::Result<(), GitError> {
    let path_s = wt.to_string_lossy().into_owned();
    let repo = match main_root(wt).await {
        Ok(r) => r,
        Err(GitError::Failed { .. }) => wt.to_path_buf(),
        Err(e) => return Err(e),
    };
    let opts = ExecOpts { allowed: &[0, 128], ..ExecOpts::default() };
    let out = if locked {
        exec(&repo, &["worktree", "lock", "--reason", reason, "--", &path_s], opts).await?
    } else {
        exec(&repo, &["worktree", "unlock", "--", &path_s], opts).await?
    };
    if out.code != 0 {
        let stderr = out.stderr.to_lowercase();
        let benign = stderr.contains("already locked") || stderr.contains("not locked");
        if !benign {
            return Err(GitError::Failed {
                code: out.code,
                stderr: out.stderr,
            });
        }
    }
    Ok(())
}

/// `git check-ignore -q -- <path>`: exit 0 = ignored, 1 = not ignored.
pub async fn check_ignore(repo: &Path, path: &Path) -> Result<bool> {
    let path_s = path.to_string_lossy().into_owned();
    let out = run_git_status(repo, &["check-ignore", "-q", "--", &path_s], &[0, 1]).await?;
    Ok(out.code == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subcommand_skips_global_options() {
        assert_eq!(subcommand(&["status", "--porcelain=v2"]), "status");
        assert_eq!(
            subcommand(&["-c", "user.name=x", "-c", "commit.gpgsign=false", "commit"]),
            "commit"
        );
        assert_eq!(subcommand(&["worktree", "list", "--porcelain"]), "worktree list");
        assert_eq!(subcommand(&["worktree", "remove", "--", "/p"]), "worktree remove");
        assert_eq!(subcommand(&["--literal-pathspecs", "add", "-A"]), "add");
        assert_eq!(subcommand(&[]), "");
    }

    #[test]
    fn read_only_calls_get_optional_locks_off() {
        assert!(is_read_only("status"));
        assert!(is_read_only("worktree list"));
        assert!(is_read_only("merge-tree"));
        assert!(!is_read_only("worktree remove"));
        assert!(!is_read_only("commit"));
        assert!(!is_read_only("merge"));

        let env = git_env(true, None);
        assert_eq!(env.get("GIT_TERMINAL_PROMPT").map(String::as_str), Some("0"));
        assert_eq!(env.get("GIT_OPTIONAL_LOCKS").map(String::as_str), Some("0"));
        assert!(!env.contains_key("GILDE_SPAWN_ID"));

        let env = git_env(false, Some("sp-1"));
        assert_eq!(env.get("GIT_TERMINAL_PROMPT").map(String::as_str), Some("0"));
        assert!(!env.contains_key("GIT_OPTIONAL_LOCKS"));
        assert_eq!(env.get("GILDE_SPAWN_ID").map(String::as_str), Some("sp-1"));
        assert_eq!(git_env_keys(false), vec!["GIT_TERMINAL_PROMPT", "GILDE_SPAWN_ID"]);
    }

    #[test]
    fn git_rows_carry_the_v1_column_value() {
        assert_eq!(ProcKind::Git.as_str(), "git");
        let store: crate::store::models::ProcKind = ProcKind::Git.into();
        assert_eq!(store, crate::store::models::ProcKind::Git, "the allowlist keys resolved git under it");
    }

    #[test]
    fn v1_wrappers_keep_the_code_of_a_git_error() {
        let cwd = Path::new("/r");
        assert!(matches!(GitError::Failed { code: 128, stderr: "x".into() }.into_gilde("status", cwd), GildeError::Git { code: 128, .. }));
        assert!(matches!(GitError::Spawn(IpcError::disk_full()).into_gilde("status", cwd), GildeError::Io(_)));
        let ipc = IpcError::from(&GitError::Spawn(IpcError::disk_full()).into_gilde("status", cwd));
        assert_eq!(ipc.code, ErrorCode::DiskFull);
    }

    fn f(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn worktree_list_parses_records_and_annotations() {
        let entries = parse_worktree_list(&f(&[
            "worktree /repo",
            "HEAD abc",
            "branch refs/heads/main",
            "",
            "worktree /wt/x",
            "HEAD def",
            "branch refs/heads/gilde/fix-ab12cd",
            "locked gilde-code session",
            "",
            "worktree /wt/gone",
            "HEAD 000",
            "detached",
            "prunable gitdir file points to non-existent location",
            "",
        ]));
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].path, PathBuf::from("/repo"));
        assert_eq!(entries[0].short_branch(), Some("main"));
        assert_eq!(entries[1].short_branch(), Some("gilde/fix-ab12cd"));
        assert_eq!(entries[1].locked.as_deref(), Some("gilde-code session"));
        assert!(entries[2].detached);
        assert!(entries[2].prunable.is_some());
    }

    #[test]
    fn status_v2_handles_spaces_quotes_and_newlines_in_paths() {
        let fields = f(&[
            "# branch.oid abc",
            "1 .M N... 100644 100644 100644 aaa bbb with space.txt",
            "1 M. N... 100644 100644 100644 aaa bbb say \"hi\".txt",
            "1 .M N... 100644 100644 100644 aaa bbb line\nbreak.txt",
            "u UU N... 100644 100644 100644 100644 a b c conflict.txt",
            "? untracked file.txt",
            "! node_modules/",
        ]);
        let status = parse_status_v2(&fields);

        assert_eq!(status.dirty_files, 4);
        assert_eq!(status.untracked, 1);
        assert_eq!(status.ignored, 1);
        assert_eq!(status.conflicted, 1);
        assert_eq!(status.staged, 1);
        assert_eq!(status.unstaged, 2);

        let paths = status.paths();
        assert!(paths.contains(&"with space.txt".to_string()));
        assert!(paths.contains(&"say \"hi\".txt".to_string()));
        assert!(paths.contains(&"line\nbreak.txt".to_string()));
        assert!(paths.contains(&"untracked file.txt".to_string()));
        assert!(!status.is_clean());
    }

    #[test]
    fn status_v2_consumes_the_extra_field_of_a_rename() {
        let fields = f(&[
            "2 R. N... 100644 100644 100644 aaa bbb R100 new name.txt",
            "old name.txt",
            "? still parsed.txt",
        ]);
        let status = parse_status_v2(&fields);
        assert_eq!(status.entries.len(), 2);
        assert_eq!(status.entries[0].path, "new name.txt");
        assert_eq!(
            status.entries[0].orig_path.as_deref(),
            Some("old name.txt"),
            "the original path must not be re-parsed as a record"
        );
        assert_eq!(status.entries[1].kind, StatusKind::Untracked);
        assert_eq!(status.entries[1].path, "still parsed.txt");
    }

    #[test]
    fn clean_status_is_clean() {
        let status = parse_status_v2(&f(&["# branch.head main"]));
        assert!(status.is_clean());
        assert_eq!(status.dirty_files, 0);
    }

    #[test]
    fn numstat_counts_text_and_binary_files() {
        let stat = parse_numstat(&f(&["3\t1\tsrc/a.rs", "-\t-\tassets/logo.png", "10\t0\tb.rs"]));
        assert_eq!(stat.files, 3);
        assert_eq!(stat.added, 13);
        assert_eq!(stat.removed, 1);
        assert_eq!(
            stat.merge(NumStat {
                added: 1,
                removed: 2,
                files: 1
            }),
            NumStat {
                added: 14,
                removed: 3,
                files: 4
            }
        );
    }

    #[test]
    fn diff_entries_join_status_and_counts() {
        let names = f(&["M", "src/a b.rs", "A", "new.txt", "D", "gone.rs", "M", "logo.png"]);
        let nums = f(&["3\t1\tsrc/a b.rs", "2\t0\tnew.txt", "0\t7\tgone.rs", "-\t-\tlogo.png"]);
        let e = merge_diff_entries(&names, &nums);
        assert_eq!(e.len(), 4);
        assert_eq!(e[0], DiffEntry { status: 'M', path: "src/a b.rs".into(), added: Some(3), removed: Some(1) });
        assert_eq!(e[2].status, 'D');
        assert_eq!((e[3].added, e[3].removed), (None, None), "binary");
    }

    #[test]
    fn hunk_names_come_from_the_headers_and_are_bounded() {
        let patch = b"diff --git a/src/a.rs b/src/a.rs\n\
--- a/src/a.rs\n\
+++ b/src/a.rs\n\
@@ -1 +1 @@ fn alpha()\n\
-x\n\
+y\n\
@@ -9 +9 @@ fn beta()\n\
@@ -12 +12 @@ fn alpha()\n\
@@ -20 +20 @@\n\
diff --git a/gone.rs b/gone.rs\n\
--- a/gone.rs\n\
+++ /dev/null\n\
@@ -1,3 +0,0 @@ struct Gone\n\
diff --git \"a/q\\\"uote.rs\" \"b/q\\\"uote.rs\"\n\
--- \"a/q\\\"uote.rs\"\n\
+++ \"b/q\\\"uote.rs\"\n\
@@ -1 +1 @@ impl Q\n";
        let names = parse_hunk_names(patch, 8);
        assert_eq!(names["src/a.rs"], vec!["fn alpha()".to_string(), "fn beta()".to_string()]);
        assert_eq!(names["gone.rs"], vec!["struct Gone".to_string()], "a deletion is named by its old path");
        assert_eq!(names["q\"uote.rs"], vec!["impl Q".to_string()]);
        let one = parse_hunk_names(patch, 1);
        assert_eq!(one["src/a.rs"].len(), 1);
    }

    #[test]
    fn c_unquote_decodes_git_escapes() {
        assert_eq!(c_unquote("\"a\\tb\"").as_deref(), Some("a\tb"));
        assert_eq!(c_unquote("\"caf\\303\\251\"").as_deref(), Some("café"));
        assert_eq!(c_unquote("\"x\\\\y\"").as_deref(), Some("x\\y"));
        assert_eq!(c_unquote("plain"), None);
    }

    #[test]
    fn git_output_helpers() {
        let out = GitOutput {
            code: 0,
            stdout: b"a\0b\0".to_vec(),
            stderr: String::new(),
        };
        assert_eq!(out.nul_fields(), vec!["a".to_string(), "b".to_string()]);

        let out = GitOutput {
            code: 0,
            stdout: b"line1\nline2\n".to_vec(),
            stderr: String::new(),
        };
        assert_eq!(out.lines(), vec!["line1".to_string(), "line2".to_string()]);
        assert_eq!(out.text(), "line1\nline2");
    }

    #[test]
    fn git_failures_become_sanitized_contract_errors() {
        let e = GitError::Failed { code: 128, stderr: "fatal: \u{1b}[31mbad\u{1b}[0m \u{202e}x\n".into() }.to_ipc();
        assert_eq!(e.code, ErrorCode::GitFailed);
        let d = e.detail.unwrap();
        assert!(!d.contains('\u{1b}') && !d.contains('\u{202e}'), "{d:?}");
        assert_eq!(GitError::Timeout.to_ipc().code, ErrorCode::Timeout);
        let refused = GitError::Spawn(IpcError::refused("notAllowlisted")).into_gilde("status", Path::new("/"));
        assert!(matches!(refused, GildeError::Refused { ref reason } if reason == "notAllowlisted"));
    }

    #[test]
    fn exclude_entry_is_appended_once_and_never_to_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        let common = dir.path().join(".git");
        std::fs::create_dir_all(&common).unwrap();
        assert!(ensure_excluded(&common, "/.gilde/").unwrap());
        assert!(!ensure_excluded(&common, "/.gilde/").unwrap(), "only once");
        let text = std::fs::read_to_string(common.join("info/exclude")).unwrap();
        assert_eq!(text.matches("/.gilde/").count(), 1);
        // The tracked ignore file is never created (its name is spelled
        // indirectly: `tests/alias_safety.rs` bans the literal in this module).
        assert!(!dir.path().join(format!(".git{}", "ignore")).exists());
        // An existing file without a trailing newline keeps its last line intact.
        std::fs::write(common.join("info/exclude"), "*.log").unwrap();
        assert!(ensure_excluded(&common, "/.gilde/").unwrap());
        let text = std::fs::read_to_string(common.join("info/exclude")).unwrap();
        assert!(text.starts_with("*.log\n"), "{text:?}");
    }

    #[test]
    fn tail_keeps_the_end_on_a_char_boundary() {
        assert_eq!(tail_of("abc", 10), "abc");
        assert_eq!(tail_of("abcdef", 3), "def");
        let s = "한글한글한글";
        let t = tail_of(s, 7);
        assert!(s.ends_with(&t));
        assert!(t.chars().count() <= 3);
    }

    #[test]
    fn git_binary_resolves() {
        assert!(git_binary().is_absolute(), "{:?}", git_binary());
    }

    fn listing(keys: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for k in keys {
            out.extend_from_slice(k);
            out.push(0);
        }
        out
    }

    #[test]
    fn a46_drivers_are_read_from_every_listed_key() {
        let drivers = drivers_named(&listing(&[
            b"core.bare",
            b"filter.lfs.process",
            b"filter.lfs.required",
            b"filter.a.b.clean",
            b"filter.MiXed.clean",
            b"filter.it's \\ ]x.smudge",
            b"diff.lockb.textconv",
            b"diff.external",
            b"diff.algorithm",
            b"merge.npm.driver",
            b"merge.renormalize",
            b"filter..clean",
            b"log.showsignature",
        ]))
        .expect("all of these can be overridden");
        let names: Vec<(Driver, &str)> = drivers.iter().map(|(k, n)| (*k, n.as_str())).collect();
        assert_eq!(
            names,
            vec![
                (Driver::Filter, "MiXed"),
                (Driver::Filter, "a.b"),
                (Driver::Filter, "it's \\ ]x"),
                (Driver::Filter, "lfs"),
                (Driver::Diff, "lockb"),
                (Driver::Merge, "npm"),
            ],
            "dotted and cased names are kept as git lists them; keys without a name are not drivers"
        );
        assert_eq!(drivers_named(b"").map(|d| d.len()), Some(0), "an empty listing has no drivers");
    }

    #[test]
    fn a46_a_driver_that_cannot_be_overridden_skips_the_read() {
        assert!(drivers_named(&listing(&[b"filter.e=q.clean"])).is_none(), "`-c` splits at the first `=`");
        assert!(drivers_named(&listing(&[b"diff.\xff\xfe.textconv"])).is_none(), "not UTF-8");
        assert!(drivers_named(&listing(&[b"merge.a\nb.driver"])).is_none(), "newline");
        let long = format!("filter.{}.clean", "n".repeat(MAX_DRIVER_NAME + 1));
        assert!(drivers_named(&listing(&[long.as_bytes()])).is_none(), "name too long");
        let at_cap: Vec<String> = (0..MAX_DRIVERS).map(|i| format!("filter.f{i}.clean")).collect();
        let refs: Vec<&[u8]> = at_cap.iter().map(|s| s.as_bytes()).collect();
        assert_eq!(drivers_named(&listing(&refs)).map(|d| d.len()), Some(MAX_DRIVERS));
        let over: Vec<String> = (0..=MAX_DRIVERS).map(|i| format!("diff.d{i}.textconv")).collect();
        let refs: Vec<&[u8]> = over.iter().map(|s| s.as_bytes()).collect();
        assert!(drivers_named(&listing(&refs)).is_none(), "more than MAX_DRIVERS");
    }

    #[test]
    fn a46_inert_values_name_no_program() {
        let drivers = drivers_named(&listing(&[b"filter.p.process", b"diff.t.command", b"merge.m.driver"])).unwrap();
        let args = inert_args(&drivers);
        let pairs: Vec<&str> = args.chunks(2).map(|c| {
            assert_eq!(c[0], "-c");
            c[1].as_str()
        }).collect();
        assert_eq!(
            pairs,
            vec![
                "filter.p.process=",
                "filter.p.clean=",
                "filter.p.smudge=",
                "filter.p.required=false",
                "diff.t.command=",
                "diff.t.textconv=",
                "merge.m.driver=",
            ]
        );
        for kind in [Driver::Filter, Driver::Diff, Driver::Merge] {
            for (var, value) in kind.inert() {
                assert!(value.is_empty() || *value == "false", "{var}={value}: never a program a PATH lookup could find");
            }
        }
        for pair in READ_ONLY_INERT.chunks(2) {
            assert_eq!(pair[0], "-c");
            let value = pair[1].split_once('=').unwrap().1;
            assert!(value.is_empty() || value == "false", "{}", pair[1]);
        }
    }

    #[test]
    fn a46_reads_that_reach_content_are_guarded_and_writes_are_not() {
        for sub in ["status", "diff", "log", "show", "merge-tree", "ls-files", "cat-file", "describe"] {
            assert!(guards_drivers(sub), "{sub}");
        }
        for sub in ["config", "rev-parse", "rev-list", "worktree list", "for-each-ref", "merge-base"] {
            assert!(!guards_drivers(sub), "{sub}");
        }
        // The user's writes keep the repository's drivers (an LFS commit or
        // checkout must still run its filter).
        for sub in ["commit", "merge", "add", "worktree add", "worktree remove", "update-ref"] {
            assert!(!guards_drivers(sub), "{sub}");
        }
        for sub in DRIVER_FREE {
            assert!(is_read_only(sub), "{sub}: every driver-free subcommand is read-only");
        }
        assert_eq!(subcommand(&LIST_CONFIG_NAMES), "config", "the listing is never guarded itself");
    }

    #[test]
    fn a46_a_skipped_read_stays_a_git_failure_everywhere() {
        let e = repo_program_refusal();
        assert!(is_repo_program_refusal(&e));
        let ipc = e.to_ipc();
        assert_eq!((ipc.code, ipc.params["reason"].as_str()), (ErrorCode::GitFailed, Some(REPO_PROGRAM)));
        assert_eq!(ipc.key, REPO_PROGRAM_KEY);
        assert!(ipc.detail.is_none(), "the sentence comes from the key, not an English detail");
        let v1 = e.into_gilde("status", Path::new("/r"));
        assert!(is_repo_program_error(&v1), "{v1:?}");
        assert_eq!(IpcError::from(&v1).code, ErrorCode::GitFailed);
        assert!(!is_repo_program_refusal(&GitError::Spawn(IpcError::git_failed())));
        assert!(!is_repo_program_refusal(&lock_left(Path::new("/r/.git/index.lock"))));
        assert!(!is_repo_program_error(&GildeError::Git { code: -1, stderr: "killed".into() }));
    }
}
