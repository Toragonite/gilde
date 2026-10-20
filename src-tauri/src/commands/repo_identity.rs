//! `repo_identity` — which REPOSITORY a folder belongs to, worktree-aware.
//!
//! The Team view groups teams by repository and filters them to "this
//! workspace". A team's lead runs in a folder that may be the main checkout, a
//! subfolder of it, or a LINKED git worktree of it; its members each run in a
//! linked worktree under the Orchestrator's data root; and the same project may
//! be cloned twice. A path-prefix comparison gets every one of those wrong, so
//! identity is git's own answer:
//!
//!   1. **Same common dir ⇒ same repository.** `git rev-parse --git-common-dir`
//!      (absolute, canonicalized) is the main repository's `.git` from the main
//!      checkout, from any subfolder and from every linked worktree alike.
//!   2. **Same normalized `origin` ⇒ same repository**, so separate clones of one
//!      project collapse. Normalization drops the scheme, the user and the port,
//!      lowercases the host, turns `git@host:path` into `host/path`, and strips a
//!      trailing `/` and `.git`. Without an origin, rule 1's identity is the key.
//!
//! Read-only and bounded. Every git call: an argv (never a shell), `git -C`,
//! `GIT_OPTIONAL_LOCKS=0` + `GIT_TERMINAL_PROMPT=0`, its own process group, a
//! [`GIT_DEADLINE`] after which the group is killed, and the write-before-spawn
//! hook like every other git spawn (`worktree::git::plan_spawn`). Answers are
//! cached per path for [`CACHE_TTL`]; an answer that could not be read in time
//! (`unknown`) is never cached, so the next call tries again.
//!
//! How a path fails, outside the happy path (each is an ANSWER, never an error):
//!
//!   * not absolute, empty, over 4 KiB, or holding a NUL → `notRepo`, no git run;
//!   * does not exist (a removed worktree) → `gone`, no git run;
//!   * exists but is not a directory, or git says it is not in a repository →
//!     `notRepo`;
//!   * git could not be spawned, or did not answer within the deadline →
//!     `unknown` (the UI keeps its provisional per-path grouping).
//!
//! Only a call with more than [`PATHS_MAX`] paths is refused (`REFUSED paths`):
//! the frontend sends them in chunks.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::task::JoinSet;

use crate::error::{CmdResult, GildeError, GildeErrorPayload};
use crate::worktree::git::{git_binary, plan_spawn};

/// Wall-clock cap on one git call here. These are local, read-only queries
/// that answer in milliseconds; a slow one is a stuck filesystem, and the UI
/// must not wait on it.
pub const GIT_DEADLINE: Duration = Duration::from_secs(3);
/// How long an answer is reused. Short: a worktree removed a minute ago should
/// read `gone` soon, and the frontend asks again every half minute.
pub const CACHE_TTL: Duration = Duration::from_secs(20);
/// Paths per call; more is refused rather than silently cut.
pub const PATHS_MAX: usize = 256;
/// Longest path accepted (bytes).
const PATH_MAX_BYTES: usize = 4096;
/// Paths probed at the same time.
const PARALLEL: usize = 8;
/// Entries kept; past this the cache is emptied (it only saves git calls).
const CACHE_MAX: usize = 2048;

/// What a path turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RepoKind {
    /// Inside a git repository (main checkout, subfolder or linked worktree).
    Repo,
    /// Exists but is not in a repository (or is not a directory, or is not a
    /// usable absolute path).
    NotRepo,
    /// Does not exist.
    Gone,
    /// git did not answer in time or could not be run: ask again later.
    Unknown,
}

/// One path's repository identity, as the Team view groups by it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoIdentity {
    /// The path as asked (the answer's join key).
    pub path: String,
    pub kind: RepoKind,
    /// The grouping key: `origin:<normalized url>`, else `dir:<common dir>`;
    /// `path:<path>` when the path is not (known to be) in a repository.
    pub key: String,
    /// Absolute, canonical `--git-common-dir`.
    pub common_dir: Option<String>,
    /// The main checkout: the common dir's parent when it is named `.git`,
    /// else the common dir itself (a bare repository). The label's source.
    pub root: Option<String>,
    /// `--show-toplevel` of THIS path's checkout (a linked worktree's own root).
    pub toplevel: Option<String>,
    /// The path is inside a linked worktree (its git dir is not the common dir).
    pub linked_worktree: bool,
    /// `HEAD`'s short branch name; `None` when detached.
    pub branch: Option<String>,
    /// The normalized `remote.origin.url`, when there is one.
    pub origin: Option<String>,
}

impl RepoIdentity {
    /// An identity that is the path alone (not a repository, gone, unknown).
    fn alone(path: &str, kind: RepoKind) -> Self {
        RepoIdentity {
            path: path.to_string(),
            kind,
            key: format!("path:{path}"),
            common_dir: None,
            root: None,
            toplevel: None,
            linked_worktree: false,
            branch: None,
            origin: None,
        }
    }
}

/// Environment for the probes. The app uses the default (the user's own git
/// configuration applies, as in their terminal); tests point git at a scratch
/// HOME with no global or system configuration.
#[derive(Debug, Clone)]
pub struct ProbeEnv {
    pub vars: Vec<(String, String)>,
    pub deadline: Duration,
}

impl Default for ProbeEnv {
    fn default() -> Self {
        ProbeEnv {
            vars: Vec::new(),
            deadline: GIT_DEADLINE,
        }
    }
}

// ---------------------------------------------------------------------------
// origin normalization
// ---------------------------------------------------------------------------

/// `remote.origin.url` → a comparable form, or `None` when there is nothing to
/// compare. `https://u@GitHub.com:443/a/b.git/`, `ssh://git@github.com/a/b` and
/// `git@github.com:a/b.git` all become `github.com/a/b`; a local path or
/// `file://` URL becomes `/the/path`.
pub fn normalize_origin(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() || s.len() > 2048 || s.chars().any(char::is_control) {
        return None;
    }
    let (host, path): (String, &str) = if let Some(idx) = s.find("://") {
        let scheme = &s[..idx];
        if scheme.is_empty()
            || !scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
        {
            return None;
        }
        let rest = &s[idx + 3..];
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        (strip_port(host).to_ascii_lowercase(), path)
    } else if let Some((authority, path)) = scp_like(s) {
        let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        (host.to_ascii_lowercase(), path)
    } else {
        (String::new(), s)
    };

    let mut segments: Vec<&str> = path.split('/').filter(|seg| !seg.is_empty()).collect();
    if let Some(last) = segments.last_mut() {
        if let Some(stem) = last.strip_suffix(".git") {
            *last = stem;
        }
    }
    segments.retain(|seg| !seg.is_empty());
    if segments.is_empty() {
        return None;
    }
    let joined = segments.join("/");
    Some(if host.is_empty() {
        format!("/{joined}")
    } else {
        format!("{host}/{joined}")
    })
}

/// git's scp-like syntax: `[user@]host:path`, i.e. a `:` with no `/` before it
/// (and not a one-letter drive such as `C:`).
fn scp_like(s: &str) -> Option<(&str, &str)> {
    let colon = s.find(':')?;
    if s[..colon].contains('/') || colon < 2 {
        return None;
    }
    Some((&s[..colon], &s[colon + 1..]))
}

/// `host:22` → `host`; `[::1]:22` → `[::1]`.
fn strip_port(host: &str) -> &str {
    if host.starts_with('[') {
        return host.find(']').map_or(host, |i| &host[..=i]);
    }
    match host.rsplit_once(':') {
        Some((h, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => h,
        Some((h, "")) => h,
        _ => host,
    }
}

// ---------------------------------------------------------------------------
// git, read-only, bounded
// ---------------------------------------------------------------------------

struct GitAnswer {
    code: i32,
    stdout: Vec<u8>,
}

impl GitAnswer {
    fn lines(&self) -> Vec<String> {
        String::from_utf8_lossy(&self.stdout)
            .lines()
            .map(str::to_string)
            .filter(|l| !l.is_empty())
            .collect()
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim().to_string()
    }
}

/// `git -C <cwd> <args…>`; `None` when it could not be run or ran past the deadline.
async fn git_read(cwd: &Path, args: &[&str], env: &ProbeEnv) -> Option<GitAnswer> {
    let mut argv: Vec<String> = Vec::with_capacity(args.len() + 3);
    argv.push(git_binary().to_string_lossy().into_owned());
    argv.push("-C".into());
    argv.push(cwd.to_string_lossy().into_owned());
    argv.extend(args.iter().map(|a| (*a).to_string()));
    let ticket = plan_spawn("git", &argv, cwd).await;

    let mut cmd = Command::new(git_binary());
    cmd.arg("-C").arg(cwd).args(args);
    cmd.env("GIT_OPTIONAL_LOCKS", "0");
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    for (key, value) in &env.vars {
        cmd.env(key, value);
    }
    if let Some(t) = ticket.as_deref() {
        cmd.env("GILDE_SPAWN_ID", t.env_marker());
    }
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::null());
    cmd.process_group(0);
    cmd.kill_on_drop(true);

    let child = cmd.spawn().ok()?;
    let pid = child.id().map(|p| p as i32);
    if let (Some(t), Some(pid)) = (ticket.as_deref(), pid) {
        t.mark_running(pid, pid);
    }
    match tokio::time::timeout(env.deadline, child.wait_with_output()).await {
        Ok(Ok(output)) => {
            if let Some(t) = ticket.as_deref() {
                t.mark_exited(output.status.code(), exit_signal(&output.status));
            }
            Some(GitAnswer {
                code: output.status.code().unwrap_or(-1),
                stdout: output.stdout,
            })
        }
        Ok(Err(_)) => None,
        Err(_) => {
            if let Some(pid) = pid {
                kill_group(pid);
            }
            if let Some(t) = ticket.as_deref() {
                t.mark_exited(None, Some(libc::SIGKILL));
            }
            tracing::debug!(target: "git", cwd = %cwd.display(), "repo identity probe ran past its deadline");
            None
        }
    }
}

fn exit_signal(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt as _;
    status.signal()
}

fn kill_group(pid: i32) {
    use nix::sys::signal::{killpg, Signal};
    use nix::unistd::Pid;
    let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
}

async fn canonical(path: &str) -> PathBuf {
    let raw = PathBuf::from(path);
    tokio::fs::canonicalize(&raw).await.unwrap_or(raw)
}

/// One path's identity, without the cache.
pub async fn probe(path: &str, env: &ProbeEnv) -> RepoIdentity {
    if path.is_empty()
        || path.len() > PATH_MAX_BYTES
        || path.contains('\0')
        || !Path::new(path).is_absolute()
    {
        return RepoIdentity::alone(path, RepoKind::NotRepo);
    }
    let dir = Path::new(path);
    match tokio::fs::metadata(dir).await {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return RepoIdentity::alone(path, RepoKind::Gone)
        }
        Err(_) => return RepoIdentity::alone(path, RepoKind::NotRepo),
        Ok(meta) if !meta.is_dir() => return RepoIdentity::alone(path, RepoKind::NotRepo),
        Ok(_) => {}
    }

    // `--show-toplevel` fails in a bare repository and inside a `.git` dir, so
    // a failure is retried without it before the path is called "not a repo".
    const DIRS: [&str; 4] = ["rev-parse", "--path-format=absolute", "--git-common-dir", "--git-dir"];
    let with_top = [&DIRS[..], &["--show-toplevel"]].concat();
    let lines = match git_read(dir, &with_top, env).await {
        None => return RepoIdentity::alone(path, RepoKind::Unknown),
        Some(answer) if answer.code == 0 => answer.lines(),
        Some(_) => match git_read(dir, &DIRS, env).await {
            None => return RepoIdentity::alone(path, RepoKind::Unknown),
            Some(answer) if answer.code == 0 => answer.lines(),
            Some(_) => return RepoIdentity::alone(path, RepoKind::NotRepo),
        },
    };
    let (Some(common_raw), Some(git_dir_raw)) = (lines.first(), lines.get(1)) else {
        return RepoIdentity::alone(path, RepoKind::NotRepo);
    };
    let common = canonical(common_raw).await;
    let git_dir = canonical(git_dir_raw).await;
    let toplevel = match lines.get(2) {
        Some(top) => Some(canonical(top).await.to_string_lossy().into_owned()),
        None => None,
    };
    let root = if common.file_name() == Some(std::ffi::OsStr::new(".git")) {
        common.parent().map(Path::to_path_buf).unwrap_or_else(|| common.clone())
    } else {
        common.clone()
    };

    let branch = match git_read(dir, &["symbolic-ref", "--short", "--quiet", "HEAD"], env).await {
        Some(answer) if answer.code == 0 => Some(answer.text()).filter(|b| !b.is_empty()),
        _ => None,
    };
    let origin = match git_read(dir, &["config", "--get", "remote.origin.url"], env).await {
        Some(answer) if answer.code == 0 => normalize_origin(&answer.text()),
        _ => None,
    };

    let common_dir = common.to_string_lossy().into_owned();
    let key = match &origin {
        Some(url) => format!("origin:{url}"),
        None => format!("dir:{common_dir}"),
    };
    RepoIdentity {
        path: path.to_string(),
        kind: RepoKind::Repo,
        key,
        linked_worktree: git_dir != common,
        common_dir: Some(common_dir),
        root: Some(root.to_string_lossy().into_owned()),
        toplevel,
        branch,
        origin,
    }
}

// ---------------------------------------------------------------------------
// cache + batch
// ---------------------------------------------------------------------------

/// Per-path answers with a time to live. `unknown` is never stored.
pub struct IdentityCache {
    ttl: Duration,
    entries: Mutex<HashMap<String, (Instant, RepoIdentity)>>,
}

impl IdentityCache {
    pub fn new(ttl: Duration) -> Self {
        IdentityCache {
            ttl,
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub fn get(&self, path: &str, now: Instant) -> Option<RepoIdentity> {
        let entries = self.entries.lock().ok()?;
        entries
            .get(path)
            .filter(|(at, _)| now.saturating_duration_since(*at) < self.ttl)
            .map(|(_, identity)| identity.clone())
    }

    pub fn put(&self, identity: &RepoIdentity, now: Instant) {
        if identity.kind == RepoKind::Unknown {
            return;
        }
        if let Ok(mut entries) = self.entries.lock() {
            if entries.len() >= CACHE_MAX {
                let ttl = self.ttl;
                entries.retain(|_, (at, _)| now.saturating_duration_since(*at) < ttl);
                if entries.len() >= CACHE_MAX {
                    entries.clear();
                }
            }
            entries.insert(identity.path.clone(), (now, identity.clone()));
        }
    }
}

fn global_cache() -> &'static IdentityCache {
    static CACHE: OnceLock<IdentityCache> = OnceLock::new();
    CACHE.get_or_init(|| IdentityCache::new(CACHE_TTL))
}

/// Every path's identity, in the order asked (duplicates answered once each),
/// at most [`PARALLEL`] probes at a time.
pub async fn identify_all(paths: &[String], env: &ProbeEnv, cache: &IdentityCache) -> Vec<RepoIdentity> {
    let now = Instant::now();
    let mut known: HashMap<String, RepoIdentity> = HashMap::new();
    let mut misses: Vec<String> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for path in paths {
        if !seen.insert(path.as_str()) {
            continue;
        }
        match cache.get(path, now) {
            Some(hit) => {
                known.insert(path.clone(), hit);
            }
            None => misses.push(path.clone()),
        }
    }

    let env = Arc::new(env.clone());
    let mut pending = misses.into_iter();
    let mut running: JoinSet<RepoIdentity> = JoinSet::new();
    loop {
        while running.len() < PARALLEL {
            let Some(path) = pending.next() else { break };
            let env = Arc::clone(&env);
            running.spawn(async move { probe(&path, &env).await });
        }
        match running.join_next().await {
            Some(Ok(identity)) => {
                cache.put(&identity, Instant::now());
                known.insert(identity.path.clone(), identity);
            }
            Some(Err(err)) => tracing::warn!(target: "git", error = %err, "a repo identity probe failed"),
            None => break,
        }
    }

    paths
        .iter()
        .map(|path| {
            known
                .get(path)
                .cloned()
                .unwrap_or_else(|| RepoIdentity::alone(path, RepoKind::Unknown))
        })
        .collect()
}

/// The repository identity of each path (Team view grouping and scope).
#[tauri::command]
pub async fn repo_identity(paths: Vec<String>) -> CmdResult<Vec<RepoIdentity>> {
    if paths.len() > PATHS_MAX {
        return Err(GildeErrorPayload::from(GildeError::refused("paths")));
    }
    Ok(identify_all(&paths, &ProbeEnv::default(), global_cache()).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- normalization ------------------------------------------------------

    #[test]
    fn origin_forms_of_one_repository_normalize_equal() {
        let want = Some("github.com/Acme/Widget".to_string());
        for raw in [
            "https://github.com/Acme/Widget.git",
            "https://github.com/Acme/Widget",
            "https://github.com/Acme/Widget/",
            "https://github.com/Acme/Widget.git/",
            "https://user:token@GitHub.com/Acme/Widget.git",
            "http://GITHUB.COM:443/Acme/Widget",
            "ssh://git@github.com/Acme/Widget.git",
            "ssh://git@github.com:22/Acme/Widget",
            "git@github.com:Acme/Widget.git",
            "git@GitHub.com:Acme/Widget",
            "github.com:Acme/Widget.git",
            "git://github.com/Acme/Widget",
            "  https://github.com//Acme/Widget.git  ",
        ] {
            assert_eq!(normalize_origin(raw), want, "{raw}");
        }
    }

    #[test]
    fn different_repositories_and_odd_values_stay_apart() {
        assert_ne!(
            normalize_origin("git@github.com:Acme/Widget.git"),
            normalize_origin("git@github.com:Acme/Gadget.git")
        );
        assert_ne!(
            normalize_origin("git@github.com:Acme/Widget.git"),
            normalize_origin("git@gitlab.com:Acme/Widget.git")
        );
        // The path keeps its case (only the host is case-insensitive).
        assert_ne!(
            normalize_origin("https://github.com/acme/widget"),
            normalize_origin("https://github.com/Acme/Widget")
        );
        assert_eq!(normalize_origin("/srv/git/widget.git"), Some("/srv/git/widget".into()));
        assert_eq!(normalize_origin("file:///srv/git/widget"), Some("/srv/git/widget".into()));
        assert_eq!(normalize_origin(""), None);
        assert_eq!(normalize_origin("   "), None);
        assert_eq!(normalize_origin("https://github.com/"), None);
        assert_eq!(normalize_origin("https://github.com/a\nb"), None);
        assert_eq!(normalize_origin("://x/y"), None);
        assert_eq!(strip_port("[::1]:22"), "[::1]");
    }

    // -- real repositories --------------------------------------------------

    /// A scratch world: git sees no global or system configuration, a scratch
    /// HOME, and never walks above the scratch root.
    struct World {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        env: ProbeEnv,
    }

    impl World {
        fn new() -> Self {
            let tmp = tempfile::tempdir().expect("tempdir");
            let root = std::fs::canonicalize(tmp.path()).expect("canonical tmp");
            let home = root.join("home");
            std::fs::create_dir_all(&home).unwrap();
            let vars = vec![
                ("GIT_CONFIG_GLOBAL".to_string(), "/dev/null".to_string()),
                ("GIT_CONFIG_NOSYSTEM".to_string(), "1".to_string()),
                ("HOME".to_string(), home.to_string_lossy().into_owned()),
                ("GIT_CEILING_DIRECTORIES".to_string(), root.to_string_lossy().into_owned()),
                ("GIT_AUTHOR_NAME".to_string(), "t".to_string()),
                ("GIT_AUTHOR_EMAIL".to_string(), "t@localhost".to_string()),
                ("GIT_COMMITTER_NAME".to_string(), "t".to_string()),
                ("GIT_COMMITTER_EMAIL".to_string(), "t@localhost".to_string()),
            ];
            World {
                _tmp: tmp,
                root,
                env: ProbeEnv {
                    vars,
                    deadline: Duration::from_secs(10),
                },
            }
        }

        fn git(&self, cwd: &Path, args: &[&str]) {
            let out = std::process::Command::new(git_binary())
                .arg("-C")
                .arg(cwd)
                .args(args)
                .envs(self.env.vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
                .env("GIT_TERMINAL_PROMPT", "0")
                .stdin(Stdio::null())
                .output()
                .expect("git runs");
            assert!(
                out.status.success(),
                "git {args:?} in {}: {}",
                cwd.display(),
                String::from_utf8_lossy(&out.stderr)
            );
        }

        fn repo(&self, rel: &str, origin: Option<&str>) -> PathBuf {
            let dir = self.root.join(rel);
            std::fs::create_dir_all(&dir).unwrap();
            self.git(&dir, &["init", "--quiet", "--initial-branch=main"]);
            self.git(&dir, &["commit", "--quiet", "--allow-empty", "-m", "init"]);
            if let Some(url) = origin {
                self.git(&dir, &["remote", "add", "origin", url]);
            }
            dir
        }

        fn worktree(&self, repo: &Path, rel: &str, branch: &str) -> PathBuf {
            let dir = self.root.join(rel);
            std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
            let target = dir.to_string_lossy().into_owned();
            self.git(repo, &["worktree", "add", "--quiet", "-b", branch, &target]);
            dir
        }
    }

    fn s(path: &Path) -> String {
        path.to_string_lossy().into_owned()
    }

    #[tokio::test]
    async fn main_checkout_linked_worktree_member_worktree_and_second_clone_are_one_repository() {
        let w = World::new();
        let main = w.repo("src/widget", Some("https://github.com/Acme/Widget.git"));
        let sub = main.join("pkg");
        std::fs::create_dir_all(&sub).unwrap();
        // (a) a lead session running inside a linked worktree of the repo
        let linked = w.worktree(&main, "elsewhere/widget-feature", "feature/x");
        // (b) a member worktree under the Orchestrator-like data root
        let member = w.worktree(
            &main,
            "data/.claude-code-orchestrator/worktrees/0a1b2c/demo-hi1/alice",
            "cco/demo-hi1/alice",
        );
        // (c) a second clone of the same project, origin written another way
        let clone = w.repo("other/widget", None);
        w.git(&clone, &["remote", "add", "origin", "git@GITHUB.com:Acme/Widget"]);

        let cache = IdentityCache::new(CACHE_TTL);
        let asked = vec![s(&main), s(&sub), s(&linked), s(&member), s(&clone)];
        let ids = identify_all(&asked, &w.env, &cache).await;
        assert_eq!(ids.len(), 5);
        for id in &ids {
            assert_eq!(id.kind, RepoKind::Repo, "{}", id.path);
            assert_eq!(id.key, "origin:github.com/Acme/Widget", "{}", id.path);
            assert_eq!(id.origin.as_deref(), Some("github.com/Acme/Widget"));
        }
        let [main_id, sub_id, linked_id, member_id, clone_id] = &ids[..] else { unreachable!() };
        assert_eq!(main_id.path, s(&main), "answers keep the asked path and order");

        // The main checkout and every worktree of it share the common dir and the root.
        let main_common = s(&main.join(".git"));
        for id in [main_id, sub_id, linked_id, member_id] {
            assert_eq!(id.common_dir.as_deref(), Some(main_common.as_str()), "{}", id.path);
            assert_eq!(id.root.as_deref(), Some(s(&main).as_str()), "{}", id.path);
        }
        assert!(!main_id.linked_worktree && !sub_id.linked_worktree);
        assert!(linked_id.linked_worktree && member_id.linked_worktree);
        assert_eq!(sub_id.toplevel.as_deref(), Some(s(&main).as_str()));
        assert_eq!(linked_id.toplevel.as_deref(), Some(s(&linked).as_str()));
        assert_eq!(main_id.branch.as_deref(), Some("main"));
        assert_eq!(linked_id.branch.as_deref(), Some("feature/x"));
        assert_eq!(member_id.branch.as_deref(), Some("cco/demo-hi1/alice"));

        // The clone is its own checkout (own common dir, own root) — joined only by origin.
        assert_eq!(clone_id.root.as_deref(), Some(s(&clone).as_str()));
        assert_ne!(clone_id.common_dir, main_id.common_dir);
        assert!(!clone_id.linked_worktree);
    }

    #[tokio::test]
    async fn without_an_origin_the_common_dir_is_the_identity() {
        let w = World::new();
        let solo = w.repo("solo", None);
        let wt = w.worktree(&solo, "solo-wt", "topic");
        let twin = w.repo("twin/solo", None);
        let cache = IdentityCache::new(CACHE_TTL);
        let ids = identify_all(&[s(&solo), s(&wt), s(&twin)], &w.env, &cache).await;
        let want = format!("dir:{}", s(&solo.join(".git")));
        assert_eq!(ids[0].key, want);
        assert_eq!(ids[1].key, want, "a linked worktree joins its repository");
        assert!(ids[1].linked_worktree);
        assert_ne!(ids[2].key, want, "a same-named repo with no shared origin stays apart");
        assert_eq!(ids[0].origin, None);
    }

    #[tokio::test]
    async fn a_plain_folder_a_deleted_path_a_file_and_bad_input_are_their_own_groups() {
        let w = World::new();
        let plain = w.root.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let deleted = w.root.join("deleted");
        std::fs::create_dir_all(&deleted).unwrap();
        std::fs::remove_dir(&deleted).unwrap();
        let file = w.root.join("file.txt");
        std::fs::write(&file, b"x").unwrap();
        let cache = IdentityCache::new(CACHE_TTL);
        let asked = vec![
            s(&plain),
            s(&deleted),
            s(&file),
            "relative/path".to_string(),
            String::new(),
            "/with\0nul".to_string(),
            "/".repeat(PATH_MAX_BYTES + 1),
        ];
        let ids = identify_all(&asked, &w.env, &cache).await;
        assert_eq!(ids[0].kind, RepoKind::NotRepo);
        assert_eq!(ids[0].key, format!("path:{}", s(&plain)));
        assert_eq!(ids[1].kind, RepoKind::Gone);
        assert_eq!(ids[1].key, format!("path:{}", s(&deleted)));
        for id in &ids[2..] {
            assert_eq!(id.kind, RepoKind::NotRepo, "{:?}", id.path.get(..20));
            assert!(id.common_dir.is_none() && id.root.is_none() && !id.linked_worktree);
        }
    }

    #[tokio::test]
    async fn a_removed_worktree_reads_gone_once_its_cached_answer_expires() {
        let w = World::new();
        let main = w.repo("r", None);
        let wt = w.worktree(&main, "wt", "t");
        let cache = IdentityCache::new(Duration::from_secs(600));
        let first = identify_all(&[s(&wt)], &w.env, &cache).await;
        assert_eq!(first[0].kind, RepoKind::Repo);
        std::fs::remove_dir_all(&wt).unwrap();
        let cached = identify_all(&[s(&wt)], &w.env, &cache).await;
        assert_eq!(cached[0].kind, RepoKind::Repo, "within the TTL the cached answer is reused");
        let fresh = identify_all(&[s(&wt)], &w.env, &IdentityCache::new(Duration::ZERO)).await;
        assert_eq!(fresh[0].kind, RepoKind::Gone, "past the TTL git is asked again");
    }

    #[tokio::test]
    async fn duplicates_are_answered_in_place_and_unknown_is_never_cached() {
        let w = World::new();
        let main = w.repo("dup", None);
        let cache = IdentityCache::new(CACHE_TTL);
        let ids = identify_all(&[s(&main), s(&main)], &w.env, &cache).await;
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0], ids[1]);

        let unknown = RepoIdentity::alone("/x", RepoKind::Unknown);
        cache.put(&unknown, Instant::now());
        assert!(cache.get("/x", Instant::now()).is_none());
    }

    #[tokio::test]
    async fn a_git_that_cannot_answer_in_time_is_unknown_not_not_a_repo() {
        let w = World::new();
        let main = w.repo("slow", None);
        let env = ProbeEnv {
            vars: w.env.vars.clone(),
            deadline: Duration::ZERO,
        };
        let id = probe(&s(&main), &env).await;
        assert_eq!(id.kind, RepoKind::Unknown);
        assert_eq!(id.key, format!("path:{}", s(&main)));
    }

    #[tokio::test]
    async fn more_than_the_cap_is_refused() {
        let paths = vec!["/".to_string(); PATHS_MAX + 1];
        let err = repo_identity(paths).await.expect_err("refused");
        assert_eq!(err.code, "REFUSED");
    }

    #[test]
    fn the_identity_crosses_ipc_in_camel_case() {
        let id = RepoIdentity::alone("/p", RepoKind::NotRepo);
        let json = serde_json::to_value(&id).unwrap();
        assert_eq!(json["kind"], "notRepo");
        assert_eq!(json["key"], "path:/p");
        assert!(json.get("linkedWorktree").is_some() && json.get("commonDir").is_some());
        let gone = serde_json::to_value(RepoKind::Gone).unwrap();
        assert_eq!(gone, "gone");
    }
}
