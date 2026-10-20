//! The `git` wrapper (DESIGN §7-2).
//!
//! We shell out to the real `git` on purpose: libgit2's worktree support lags
//! upstream in exactly the places where being wrong loses the user's work, and
//! the user's `includeIf` / hooks / credential helper / `safe.directory`
//! configuration has to apply here exactly as it does in their terminal —
//! the agent runs `git` inside these worktrees itself.
//!
//! Every invocation obeys the same five rules:
//!
//! 1. **No shell.** An argv array, always. There is no string that a repository
//!    name or branch name could ever be interpolated into.
//! 2. **`git -C <cwd>`** rather than changing the process's directory.
//! 3. **`GIT_TERMINAL_PROMPT=0`**, so a repository that wants credentials fails
//!    in 20 ms instead of blocking a command handler forever. Read-only calls
//!    additionally get **`GIT_OPTIONAL_LOCKS=0`** so that polling `status` never
//!    fights the user's interactive git for `index.lock`.
//! 4. **A 20 second cap.** On expiry the process group is killed and the call
//!    returns a retryable error.
//! 5. **Its own process group** (`process_group(0)`), and — once T08 installs a
//!    hook via [`install_spawn_hook`] — a durable registry row written *before*
//!    the spawn. `git` is not exempt from the write-before-spawn rule.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::process::Command;

use crate::error::{GildeError, Result};

/// Wall-clock cap on a single `git` invocation (DESIGN §7-2).
pub const GIT_TIMEOUT: Duration = Duration::from_secs(20);

/// Identity used for checkpoint commits, so a user without a global
/// `user.email` never meets a confusing failure during a rescue (DESIGN §7-7).
pub const CHECKPOINT_USER_NAME: &str = "gilde-code";
pub const CHECKPOINT_USER_EMAIL: &str = "gilde@localhost";
/// Reason recorded on `git worktree lock` for worktrees we manage.
pub const LOCK_REASON: &str = "gilde-code session";

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

// ---------------------------------------------------------------------------
// Spawn hook — the seam T08 wires `SpawnGuard` into
// ---------------------------------------------------------------------------

/// A single armed registry row, handed back by [`SpawnHook::plan`].
///
/// T08's `SpawnGuard` implements this. Dropping the ticket without
/// [`SpawnTicket::mark_running`] must leave the row `state='lost'`, which is
/// exactly `SpawnGuard`'s own `Drop` contract.
pub trait SpawnTicket: Send + Sync {
    /// Value to stamp into `GILDE_SPAWN_ID` on the child.
    fn env_marker(&self) -> String;
    fn mark_running(&self, pid: i32, pgid: i32);
    fn mark_exited(&self, code: Option<i32>, signal: Option<i32>);
}

/// Pre-spawn registration. Installed once at boot by T08/T29.
pub trait SpawnHook: Send + Sync + 'static {
    /// `kind` is a `proc_registry.kind` value — `"git"` for git invocations and
    /// `"probe"` for the post-create command.
    fn plan<'a>(
        &'a self,
        kind: &'a str,
        argv: &'a [String],
        cwd: &'a Path,
    ) -> BoxFuture<'a, Option<Box<dyn SpawnTicket>>>;
}

static SPAWN_HOOK: OnceLock<Arc<dyn SpawnHook>> = OnceLock::new();

/// Install the write-before-spawn hook. Idempotent: the first installation wins
/// and later calls report `false` rather than swapping a live hook.
pub fn install_spawn_hook(hook: Arc<dyn SpawnHook>) -> bool {
    SPAWN_HOOK.set(hook).is_ok()
}

fn spawn_hook() -> Option<&'static Arc<dyn SpawnHook>> {
    SPAWN_HOOK.get()
}

/// Register a spawn with the hook, if one is installed. Used by every spawn in
/// this module and by the worktree post-create command.
pub async fn plan_spawn(kind: &str, argv: &[String], cwd: &Path) -> Option<Box<dyn SpawnTicket>> {
    match spawn_hook() {
        Some(hook) => hook.plan(kind, argv, cwd).await,
        None => None,
    }
}

// ---------------------------------------------------------------------------
// Output
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
    "describe",
    "var",
    "version",
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

/// Run `git -C <cwd> <args…>` and fail on a non-zero exit.
///
/// The error carries the raw `stderr` verbatim — it is never translated,
/// because git's messages are the most useful thing we can show and paraphrasing
/// them would lose information the user needs.
pub async fn run_git(cwd: &Path, args: &[&str]) -> Result<GitOutput> {
    let out = run_git_status(cwd, args, &[0]).await?;
    Ok(out)
}

/// Like [`run_git`] but tolerating the listed exit codes (`check-ignore`
/// reports "not ignored" as exit 1, which is an answer and not a failure).
pub async fn run_git_status(cwd: &Path, args: &[&str], allowed: &[i32]) -> Result<GitOutput> {
    let sub = subcommand(args);
    let mut argv: Vec<String> = Vec::with_capacity(args.len() + 3);
    argv.push(git_binary().to_string_lossy().into_owned());
    argv.push("-C".into());
    argv.push(cwd.to_string_lossy().into_owned());
    argv.extend(args.iter().map(|a| (*a).to_string()));

    let ticket = plan_spawn("git", &argv, cwd).await;

    let mut cmd = Command::new(git_binary());
    cmd.arg("-C").arg(cwd).args(args);
    cmd.envs(git_env(is_read_only(&sub), ticket.as_deref()));
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    // Every spawn is its own process group so a stuck git (and any hook it
    // started) can be signalled as a unit.
    cmd.process_group(0);
    cmd.kill_on_drop(true);

    let child = cmd
        .spawn()
        .map_err(|e| GildeError::spawn(format!("git {sub}: {e}")))?;
    let pid = child.id().map(|p| p as i32);
    if let (Some(t), Some(pid)) = (ticket.as_deref(), pid) {
        t.mark_running(pid, pid);
    }

    let output = match tokio::time::timeout(GIT_TIMEOUT, child.wait_with_output()).await {
        Ok(res) => res.map_err(|e| GildeError::spawn(format!("git {sub}: {e}")))?,
        Err(_) => {
            if let Some(pid) = pid {
                kill_group(pid);
            }
            if let Some(t) = ticket.as_deref() {
                t.mark_exited(None, Some(libc::SIGKILL));
            }
            return Err(GildeError::spawn(format!(
                "git {sub} timed out after {}s in {}",
                GIT_TIMEOUT.as_secs(),
                cwd.display()
            )));
        }
    };

    let code = output.status.code().unwrap_or(-1);
    if let Some(t) = ticket.as_deref() {
        t.mark_exited(output.status.code(), exit_signal(&output.status));
    }

    let result = GitOutput {
        code,
        stdout: output.stdout,
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    };

    if allowed.contains(&code) {
        Ok(result)
    } else {
        tracing::debug!(target: "git", %code, sub = %sub, cwd = %cwd.display(), "git exited non-zero");
        Err(GildeError::Git {
            code,
            stderr: result.stderr,
        })
    }
}

#[cfg(unix)]
fn exit_signal(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt as _;
    status.signal()
}

#[cfg(unix)]
fn kill_group(pid: i32) {
    use nix::sys::signal::{killpg, Signal};
    use nix::unistd::Pid;
    let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
}

/// Environment overlay. We deliberately inherit the rest of the environment:
/// git must see the user's `HOME`, `SSH_AUTH_SOCK` and `GIT_*` settings exactly
/// as their shell does (DESIGN §7-2).
fn git_env(read_only: bool, ticket: Option<&dyn SpawnTicket>) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("GIT_TERMINAL_PROMPT".to_string(), "0".to_string());
    if read_only {
        env.insert("GIT_OPTIONAL_LOCKS".to_string(), "0".to_string());
    }
    if let Some(t) = ticket {
        env.insert("GILDE_SPAWN_ID".to_string(), t.env_marker());
    }
    env
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
    let out = run_git(
        path,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .await?;
    let common = PathBuf::from(out.text());
    if common.as_os_str().is_empty() {
        return Err(GildeError::worktree(format!(
            "git reported no common dir for {}",
            path.display()
        )));
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
    let out = run_git(repo, &["rev-parse", "--verify", "--quiet", &spec]).await?;
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
    let full = format!("refs/heads/{branch}");
    let out = run_git_status(repo, &["show-ref", "--verify", "--quiet", &full], &[0, 1]).await?;
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
    match run_git(repo, &["worktree", "list", "--porcelain", "-z"]).await {
        Ok(out) => Ok(parse_worktree_list(&out.nul_fields())),
        Err(GildeError::Git { .. }) => {
            let out = run_git(repo, &["worktree", "list", "--porcelain"]).await?;
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
pub async fn status_v2(wt: &Path) -> Result<StatusV2> {
    let out = run_git(
        wt,
        &["status", "--porcelain=v2", "-z", "--untracked-files=all"],
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
                    x: '?',
                    y: '?',
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
            StatusKind::Ignored => {}
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

/// `git diff --numstat <base>...HEAD` — what this branch added on top of the
/// merge base.
pub async fn numstat(wt: &Path, base: &str) -> Result<NumStat> {
    let range = format!("{base}...HEAD");
    let out = run_git(wt, &["diff", "--numstat", &range]).await?;
    Ok(parse_numstat(&out.lines()))
}

/// `git diff --numstat` — uncommitted work, staged and unstaged.
pub async fn numstat_worktree(wt: &Path) -> Result<NumStat> {
    let unstaged = run_git(wt, &["diff", "--numstat"]).await?;
    let staged = run_git(wt, &["diff", "--numstat", "--cached"]).await?;
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
    run_git(
        wt,
        &[
            "-c",
            &format!("user.name={CHECKPOINT_USER_NAME}"),
            "-c",
            &format!("user.email={CHECKPOINT_USER_EMAIL}"),
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--no-verify",
            "-m",
            msg,
        ],
    )
    .await?;
    let out = run_git(wt, &["rev-parse", "HEAD"]).await?;
    Ok(out.text())
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
    let path_s = path.to_string_lossy().into_owned();
    match base {
        Some(base_sha) => {
            run_git(
                repo,
                &[
                    "worktree",
                    "add",
                    "--no-track",
                    "-b",
                    branch,
                    "--",
                    &path_s,
                    base_sha,
                ],
            )
            .await?;
        }
        None => {
            run_git(repo, &["worktree", "add", "--", &path_s, branch]).await?;
        }
    }
    Ok(())
}

/// `git worktree remove` — **never with `--force`** (DESIGN §0 invariant 5).
///
/// A locked worktree refuses removal, so callers unlock first; that unlock is a
/// deliberate, logged step and not a flag that silently discards work.
pub async fn worktree_remove(repo: &Path, path: &Path) -> Result<()> {
    let path_s = path.to_string_lossy().into_owned();
    run_git(repo, &["worktree", "remove", "--", &path_s]).await?;
    Ok(())
}

/// `git worktree prune` — removes administrative files for worktrees whose
/// directory is gone. It never touches a directory on disk.
pub async fn worktree_prune(repo: &Path) -> Result<()> {
    run_git(repo, &["worktree", "prune"]).await?;
    Ok(())
}

/// `git worktree lock|unlock`. Locking is idempotent: git reports "already
/// locked" as a failure that we treat as success.
pub async fn worktree_lock(wt: &Path, locked: bool, reason: &str) -> Result<()> {
    let path_s = wt.to_string_lossy().into_owned();
    let repo = repo_root(wt).await.unwrap_or_else(|_| wt.to_path_buf());
    let out = if locked {
        run_git_status(
            &repo,
            &["worktree", "lock", "--reason", reason, "--", &path_s],
            &[0, 128],
        )
        .await?
    } else {
        run_git_status(&repo, &["worktree", "unlock", "--", &path_s], &[0, 128]).await?
    };
    if out.code != 0 {
        let stderr = out.stderr.to_lowercase();
        let benign = stderr.contains("already locked") || stderr.contains("not locked");
        if !benign {
            return Err(GildeError::Git {
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
        assert_eq!(subcommand(&[]), "");
    }

    #[test]
    fn read_only_calls_get_optional_locks_off() {
        assert!(is_read_only("status"));
        assert!(is_read_only("worktree list"));
        assert!(!is_read_only("worktree remove"));
        assert!(!is_read_only("commit"));

        let env = git_env(true, None);
        assert_eq!(env.get("GIT_TERMINAL_PROMPT").map(String::as_str), Some("0"));
        assert_eq!(env.get("GIT_OPTIONAL_LOCKS").map(String::as_str), Some("0"));

        let env = git_env(false, None);
        assert_eq!(env.get("GIT_TERMINAL_PROMPT").map(String::as_str), Some("0"));
        assert!(env.get("GIT_OPTIONAL_LOCKS").is_none());
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
        assert_eq!(
            entries[1].locked.as_deref(),
            Some("gilde-code session")
        );
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
        ]);
        let status = parse_status_v2(&fields);

        assert_eq!(status.dirty_files, 4);
        assert_eq!(status.untracked, 1);
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
    fn git_binary_resolves() {
        assert!(git_binary().is_absolute(), "{:?}", git_binary());
    }
}
