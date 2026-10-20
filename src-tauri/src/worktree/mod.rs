//! Worktree lifecycle (DESIGN §7).
//!
//! Everything in this module is shaped by one invariant from DESIGN §0:
//! **no operation that can lose the user's work happens without an explicit
//! choice.** Concretely:
//!
//! * `git worktree remove --force` appears nowhere in this crate, and
//!   `tests/worktree_safety.rs` greps the source to keep it that way.
//! * `git branch -d` / `-D` appear nowhere either. An unmerged `gilde/*` branch is
//!   cheap and recoverable; a deleted one is not.
//! * A dirty tree can only be removed through `CommitThenRemove` or
//!   `ArchiveThenRemove`, never through `KeepBranch`.
//! * A worktree with a live registry process is refused in **every** mode.
//! * `prune` reports before it acts, and never deletes a directory git forgot.
//!
//! We also never run `git checkout` in the user's own checkout (DESIGN §7-4):
//! "switching worktree" is a UI concept. N live agents must not have the ground
//! moved under them.

pub mod alias; // T14  provider path alias layer
pub mod git;
pub mod naming;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{GildeError, Result};
use crate::events::{ToastEvent, ToastLevel, WorktreeChangedEvent, WORKTREE_CHANGED};
use crate::paths::AppPaths;
use crate::store::models::{
    ProcState, WorktreeOrigin, WorktreeRow, WorktreeStatsCache,
};
use crate::store::{dao, now_ms, Db};

pub use git::{
    GitOutput, GitWorktreeEntry, NumStat, StatusEntry, StatusKind, StatusV2, BoxFuture,
};

/// Message used for the rescue commit made by `CommitThenRemove`.
pub const CHECKPOINT_MESSAGE: &str = "gilde: checkpoint before worktree removal";
/// Filesystem-watch debounce (DESIGN §7-5).
pub const WATCH_DEBOUNCE: Duration = Duration::from_secs(2);
/// Never recompute a worktree's stats more often than this.
pub const STATS_MIN_INTERVAL: Duration = Duration::from_secs(2);
/// Directory names whose contents never mean "the worktree changed".
pub const WATCH_IGNORED: &[&str] = &[".git", "node_modules", "target", "dist"];
/// Wall-clock cap on the user's post-create command.
pub const POST_CREATE_TIMEOUT: Duration = Duration::from_secs(300);
/// Bytes of post-create output kept for the toast.
pub const POST_CREATE_TAIL: usize = 4096;

// ---------------------------------------------------------------------------
// Wire types (mirrored by `src/lib/ipc.ts`, owned by T04)
// ---------------------------------------------------------------------------

/// A worktree as the frontend sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Worktree {
    pub id: String,
    pub project_id: String,
    pub slug: String,
    pub branch: String,
    pub path: String,
    pub base_sha: String,
    pub origin: WorktreeOrigin,
    pub created_at: i64,
    pub removed_at: Option<i64>,
    pub locked: bool,
    pub verified_at: Option<i64>,
    pub post_create_cmd: Option<String>,
}

impl From<WorktreeRow> for Worktree {
    fn from(row: WorktreeRow) -> Self {
        Worktree {
            id: row.id,
            project_id: row.project_id,
            slug: row.slug,
            branch: row.branch,
            path: row.path,
            base_sha: row.base_sha.unwrap_or_default(),
            origin: row.origin,
            created_at: row.created_at,
            removed_at: row.removed_at,
            locked: row.locked,
            verified_at: row.verified_at,
            post_create_cmd: row.post_create_cmd,
        }
    }
}

impl Worktree {
    pub fn path_buf(&self) -> PathBuf {
        PathBuf::from(&self.path)
    }
}

/// `worktree_create` request (DESIGN §3).
///
/// `aliasProviders` is carried as raw JSON: the provider vocabulary belongs to
/// `worktree::alias` (T14), which registers itself through [`install_alias_hook`]
/// rather than being a compile-time dependency of the lifecycle.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateWorktreeReq {
    pub project_id: String,
    pub task_name: String,
    #[serde(default)]
    pub base_ref: Option<String>,
    #[serde(default)]
    pub alias_providers: Vec<Value>,
    #[serde(default)]
    pub launch: Option<Value>,
    /// DECISIONS §M1 세부 기본값: shipped in M1 and run in the new worktree.
    #[serde(default)]
    pub post_create_cmd: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeStats {
    pub dirty_files: u32,
    pub staged: u32,
    pub untracked: u32,
    pub conflicted: u32,
    pub added: u64,
    pub removed: u64,
    pub ahead: u32,
    pub behind: u32,
    pub last_commit_at: Option<i64>,
    pub last_commit_subject: Option<String>,
}

/// `Force` deliberately does not exist (DESIGN §0 invariant 5, §7-7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RemoveMode {
    KeepBranch,
    ArchiveThenRemove,
    CommitThenRemove,
}

impl RemoveMode {
    pub fn as_key(self) -> &'static str {
        match self {
            RemoveMode::KeepBranch => "keepBranch",
            RemoveMode::ArchiveThenRemove => "archiveThenRemove",
            RemoveMode::CommitThenRemove => "commitThenRemove",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemovePreflight {
    pub dirty_files: u32,
    pub untracked: u32,
    pub unmerged_commits: u32,
    pub live_sessions: Vec<String>,
    pub running_runs: Vec<String>,
    pub alias_links: Vec<String>,
    pub foreign_pids: Vec<i32>,
    pub allowed_modes: Vec<RemoveMode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoveOutcome {
    pub mode: RemoveMode,
    pub removed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_sha: Option<String>,
    /// The branch is never deleted by this app.
    pub branch_kept: String,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PruneReport {
    /// git knows the worktree, the directory is gone → `git worktree prune`.
    pub git_only: Vec<String>,
    /// The directory exists but git forgot it → reported, **never** deleted.
    pub disk_only: Vec<String>,
    /// A row with nothing behind it → the row is dropped.
    pub db_only: Vec<String>,
    pub healthy: Vec<String>,
}

/// Result of the per-project post-create command (DECISIONS §M1 세부 기본값).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PostCreateResult {
    pub cmd: String,
    pub exit_code: Option<i32>,
    pub tail: String,
}

// ---------------------------------------------------------------------------
// Typed refusals
// ---------------------------------------------------------------------------

/// Refusals the UI has to render as a *choice*, not as a stack trace.
///
/// These become `GildeErrorPayload { code: "REFUSED", key: "error.worktree.<x>",
/// params }` at the command boundary — `error.rs` is single-owner (T01) and has
/// no structured worktree variant, so the payload is built in
/// `commands/worktree.rs` instead of adding an `GildeError` variant.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorktreeError {
    /// The most common create failure in practice, and one git really enforces.
    #[error("branch {branch} is already checked out at {existing_path}")]
    BranchInUse {
        branch: String,
        existing_path: String,
    },
    #[error("{count} live session(s) in this worktree")]
    SessionsLive { sessions: Vec<String>, count: usize },
    #[error("{count} foreign process(es) are using this worktree")]
    ForeignProcessInside { pids: Vec<i32>, count: usize },
    #[error("the worktree has uncommitted work")]
    Dirty { dirty_files: u32, untracked: u32 },
    #[error("mode {mode} is not available here")]
    ModeNotAllowed {
        mode: &'static str,
        allowed: Vec<RemoveMode>,
    },
    #[error("{path} already exists")]
    PathOccupied { path: String },
    #[error("the repository has no commits yet")]
    EmptyRepository,
    #[error("{path} is not a git repository")]
    NotARepository { path: String },
    #[error("the worktree is locked")]
    Locked,
}

impl WorktreeError {
    /// i18n key. Must exist in both `src/i18n/ko.ts` and `src/i18n/en.ts`.
    pub fn key(&self) -> &'static str {
        match self {
            WorktreeError::BranchInUse { .. } => "error.worktree.branchInUse",
            WorktreeError::SessionsLive { .. } => "error.worktree.sessionsLive",
            WorktreeError::ForeignProcessInside { .. } => "error.worktree.foreignProcessInside",
            WorktreeError::Dirty { .. } => "error.worktree.dirty",
            WorktreeError::ModeNotAllowed { .. } => "error.worktree.modeNotAllowed",
            WorktreeError::PathOccupied { .. } => "error.worktree.pathOccupied",
            WorktreeError::EmptyRepository => "error.worktree.emptyRepository",
            WorktreeError::NotARepository { .. } => "error.worktree.notARepository",
            WorktreeError::Locked => "error.worktree.locked",
        }
    }

    /// Stable machine token, also the suffix of [`WorktreeError::key`].
    pub fn reason(&self) -> &'static str {
        self.key().rsplit('.').next().unwrap_or("worktree")
    }

    /// Interpolation parameters for [`WorktreeError::key`].
    pub fn params(&self) -> serde_json::Map<String, Value> {
        let mut m = serde_json::Map::new();
        match self {
            WorktreeError::BranchInUse {
                branch,
                existing_path,
            } => {
                m.insert("branch".into(), Value::from(branch.clone()));
                m.insert("existingPath".into(), Value::from(existing_path.clone()));
            }
            WorktreeError::SessionsLive { sessions, count } => {
                m.insert("count".into(), Value::from(*count));
                m.insert(
                    "sessions".into(),
                    Value::from(sessions.iter().map(|s| Value::from(s.clone())).collect::<Vec<_>>()),
                );
            }
            WorktreeError::ForeignProcessInside { pids, count } => {
                m.insert("count".into(), Value::from(*count));
                m.insert(
                    "pids".into(),
                    Value::from(pids.iter().map(|p| Value::from(*p)).collect::<Vec<_>>()),
                );
            }
            WorktreeError::Dirty {
                dirty_files,
                untracked,
            } => {
                m.insert("dirtyFiles".into(), Value::from(*dirty_files));
                m.insert("untracked".into(), Value::from(*untracked));
            }
            WorktreeError::ModeNotAllowed { mode, allowed } => {
                m.insert("mode".into(), Value::from(*mode));
                m.insert(
                    "allowed".into(),
                    Value::from(
                        allowed
                            .iter()
                            .map(|a| Value::from(a.as_key()))
                            .collect::<Vec<_>>(),
                    ),
                );
            }
            WorktreeError::PathOccupied { path } | WorktreeError::NotARepository { path } => {
                m.insert("path".into(), Value::from(path.clone()));
            }
            WorktreeError::EmptyRepository | WorktreeError::Locked => {}
        }
        m
    }
}

impl From<WorktreeError> for GildeError {
    fn from(e: WorktreeError) -> GildeError {
        GildeError::Refused {
            reason: e.reason().to_string(),
        }
    }
}

/// Either a typed refusal or an ordinary failure.
#[derive(Debug, thiserror::Error)]
pub enum WorktreeFailure {
    #[error(transparent)]
    Refused(#[from] WorktreeError),
    #[error(transparent)]
    Other(#[from] GildeError),
}

impl From<std::io::Error> for WorktreeFailure {
    fn from(e: std::io::Error) -> Self {
        WorktreeFailure::Other(GildeError::Io(e))
    }
}

impl From<WorktreeFailure> for GildeError {
    fn from(f: WorktreeFailure) -> GildeError {
        match f {
            WorktreeFailure::Refused(e) => e.into(),
            WorktreeFailure::Other(e) => e,
        }
    }
}

/// Result of the typed lifecycle entry points.
pub type WtResult<T> = std::result::Result<T, WorktreeFailure>;

// ---------------------------------------------------------------------------
// Hooks — seams for sibling tasks, so nobody has to edit this file
// ---------------------------------------------------------------------------

/// Provider alias layer (T14). Registered once at boot.
pub trait AliasHook: Send + Sync + 'static {
    /// Create/verify the requested provider aliases for a new worktree.
    fn ensure<'a>(
        &'a self,
        worktree: &'a Worktree,
        repo: &'a Path,
        providers: &'a [Value],
    ) -> BoxFuture<'a, Vec<Value>>;

    /// Remove only the symlinks we own. Returns the paths actually removed.
    fn remove_owned<'a>(&'a self, worktree_id: &'a str) -> BoxFuture<'a, Vec<String>>;
}

static ALIAS_HOOK: OnceLock<Arc<dyn AliasHook>> = OnceLock::new();

/// Install the alias layer. The first installation wins.
pub fn install_alias_hook(hook: Arc<dyn AliasHook>) -> bool {
    ALIAS_HOOK.set(hook).is_ok()
}

fn alias_hook() -> Option<&'static Arc<dyn AliasHook>> {
    ALIAS_HOOK.get()
}

/// "Which processes are using this directory?" — replaceable by the real
/// process scanner (T07/T12) so the preflight and the Process Monitor agree.
pub trait ForeignProcessProbe: Send + Sync + 'static {
    fn pids_using(&self, path: &Path) -> Vec<i32>;
}

static FOREIGN_PROBE: OnceLock<Arc<dyn ForeignProcessProbe>> = OnceLock::new();

pub fn install_foreign_probe(probe: Arc<dyn ForeignProcessProbe>) -> bool {
    FOREIGN_PROBE.set(probe).is_ok()
}

/// Default probe: `proc_listpidspath`, which reports every process holding a
/// reference (cwd included) below `path`. A superset of "cwd inside" — and the
/// safe direction to err in, since the answer only ever *adds* a warning.
#[cfg(target_os = "macos")]
fn default_foreign_pids(path: &Path) -> Vec<i32> {
    use libproc::processes::{pids_by_type_and_path, ProcFilter};
    match pids_by_type_and_path(ProcFilter::All, path, false, true) {
        Ok(pids) => pids.into_iter().map(|p| p as i32).collect(),
        Err(err) => {
            tracing::debug!(target: "worktree", path = %path.display(), error = %err, "foreign pid probe failed");
            Vec::new()
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn default_foreign_pids(_path: &Path) -> Vec<i32> {
    Vec::new()
}

fn probe_foreign_pids(path: &Path) -> Vec<i32> {
    let mut pids = match FOREIGN_PROBE.get() {
        Some(probe) => probe.pids_using(path),
        None => default_foreign_pids(path),
    };
    let me = std::process::id() as i32;
    pids.retain(|p| *p != me);
    pids.sort_unstable();
    pids.dedup();
    pids
}

// ---------------------------------------------------------------------------
// WorktreeManager
// ---------------------------------------------------------------------------

/// Optional sink for `gilde://worktree/changed`. Held as an option so the manager
/// is fully usable in tests without a `tauri::AppHandle`.
pub type EventSink = crate::events::bus::EventBus;

struct WatchHandle {
    _debouncer: notify_debouncer_full::Debouncer<
        notify::RecommendedWatcher,
        notify_debouncer_full::RecommendedCache,
    >,
}

pub struct WorktreeManager {
    me: Weak<WorktreeManager>,
    db: Db,
    paths: Arc<AppPaths>,
    bus: Option<EventSink>,
    watchers: Mutex<HashMap<String, WatchHandle>>,
    last_stats: Mutex<HashMap<String, Instant>>,
}

impl std::fmt::Debug for WorktreeManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorktreeManager")
            .field("watchers", &self.watchers.lock().len())
            .finish()
    }
}

impl WorktreeManager {
    pub fn new(db: Db, paths: Arc<AppPaths>, bus: Option<EventSink>) -> Arc<Self> {
        Arc::new_cyclic(|me| WorktreeManager {
            me: me.clone(),
            db,
            paths,
            bus,
            watchers: Mutex::new(HashMap::new()),
            last_stats: Mutex::new(HashMap::new()),
        })
    }

    fn arc(&self) -> Option<Arc<Self>> {
        self.me.upgrade()
    }

    pub fn paths(&self) -> &AppPaths {
        &self.paths
    }

    // -- rows ---------------------------------------------------------------

    async fn worktree_row(&self, id: &str) -> Result<WorktreeRow> {
        let id = id.to_string();
        self.db
            .read(move |conn| dao::worktree::get(conn, &id))
            .await?
            .ok_or_else(|| GildeError::not_found("worktree"))
    }

    async fn project_row(&self, id: &str) -> Result<crate::store::models::ProjectRow> {
        let id = id.to_string();
        self.db
            .read(move |conn| dao::project::get(conn, &id))
            .await?
            .ok_or_else(|| GildeError::not_found("project"))
    }

    /// Live worktrees of a project.
    pub async fn list(&self, project_id: &str) -> Result<Vec<Worktree>> {
        let project_id = project_id.to_string();
        let rows = self
            .db
            .read(move |conn| dao::worktree::list_by_project(conn, &project_id, false))
            .await?;
        Ok(rows.into_iter().map(Worktree::from).collect())
    }

    pub async fn get(&self, id: &str) -> Result<Worktree> {
        Ok(Worktree::from(self.worktree_row(id).await?))
    }

    // -- create -------------------------------------------------------------

    /// Create a worktree. See [`WorktreeManager::create_typed`]; refusals are
    /// flattened into [`GildeError::Refused`] here.
    pub async fn create(&self, req: CreateWorktreeReq) -> Result<Worktree> {
        self.create_typed(req).await.map_err(GildeError::from)
    }

    /// DESIGN §7-3, preflight first and every failure typed.
    pub async fn create_typed(&self, req: CreateWorktreeReq) -> WtResult<Worktree> {
        let project = self.project_row(&req.project_id).await?;
        let repo = PathBuf::from(&project.canonical_path);

        if !git::is_repo(&repo).await {
            return Err(WorktreeError::NotARepository {
                path: repo.display().to_string(),
            }
            .into());
        }
        if !git::has_commits(&repo).await {
            return Err(WorktreeError::EmptyRepository.into());
        }

        // base_ref: caller → project default → the repository's current branch.
        let base_ref = match req.base_ref.as_deref().filter(|r| !r.trim().is_empty()) {
            Some(r) => r.to_string(),
            None => {
                let stored = project.default_base_ref.trim();
                if stored.is_empty() || stored == "HEAD" {
                    git::current_branch(&repo)
                        .await?
                        .unwrap_or_else(|| "HEAD".to_string())
                } else {
                    stored.to_string()
                }
            }
        };
        let base_sha = git::verify_ref(&repo, &base_ref).await?;

        let slug = naming::task_slug(&req.task_name);
        let branch = naming::branch_name(&slug);
        let repo_slug = if project.repo_slug.is_empty() {
            naming::repo_slug(&repo)
        } else {
            project.repo_slug.clone()
        };
        let path = self.paths.worktrees_root().join(&repo_slug).join(&slug);

        if path.exists() {
            return Err(WorktreeError::PathOccupied {
                path: path.display().to_string(),
            }
            .into());
        }

        // "branch already checked out in another worktree" is a real git
        // failure mode and the single most common create error in practice.
        if let Some(existing) = branch_in_use(&repo, &branch).await? {
            return Err(WorktreeError::BranchInUse {
                branch: branch.clone(),
                existing_path: existing.display().to_string(),
            }
            .into());
        }

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let branch_exists = git::branch_exists(&repo, &branch).await?;
        let base = if branch_exists {
            None
        } else {
            Some(base_sha.as_str())
        };
        git::worktree_add(&repo, &path, &branch, base).await?;

        // Locked while a session may live in it (DESIGN §7-3). Removal unlocks
        // deliberately; that is not the same thing as `--force`.
        if let Err(err) = git::worktree_lock(&path, true, git::LOCK_REASON).await {
            tracing::warn!(target: "worktree", error = %err, "worktree lock failed");
        }

        let now = now_ms();
        let row = WorktreeRow {
            id: uuid::Uuid::now_v7().to_string(),
            project_id: project.id.clone(),
            slug: slug.clone(),
            branch: branch.clone(),
            path: path.display().to_string(),
            base_sha: Some(base_sha.clone()),
            origin: WorktreeOrigin::Created,
            created_at: now,
            removed_at: None,
            locked: true,
            verified_at: Some(now),
            post_create_cmd: req
                .post_create_cmd
                .clone()
                .filter(|c| !c.trim().is_empty()),
            cached_dirty: 0,
            cached_added: 0,
            cached_removed: 0,
            cached_ahead: 0,
            cached_behind: 0,
        };
        let insert = row.clone();
        self.db
            .write(move |conn| dao::worktree::insert(conn, &insert))
            .await?;

        let worktree = Worktree::from(row);

        if !req.alias_providers.is_empty() {
            if let Some(hook) = alias_hook() {
                let outcomes = hook.ensure(&worktree, &repo, &req.alias_providers).await;
                tracing::info!(target: "worktree", count = outcomes.len(), "alias outcomes");
            } else {
                tracing::debug!(target: "worktree", "alias providers requested but no alias hook installed");
            }
        }

        self.start_watch(&worktree.id, &path);

        if let Some(cmd) = worktree.post_create_cmd.clone() {
            match self.run_post_create(&path, &cmd).await {
                Ok(result) => self.report_post_create(&worktree.id, &result),
                Err(err) => {
                    tracing::warn!(target: "worktree", error = %err, "post-create command failed to start");
                    self.toast(
                        ToastLevel::Warn,
                        "worktree.postCreate.failed",
                        &[("cmd", Value::from(cmd)), ("detail", Value::from(err.to_string()))],
                    );
                }
            }
        }

        Ok(worktree)
    }

    // -- stats --------------------------------------------------------------

    /// DESIGN §7-5. Everything here runs with `GIT_OPTIONAL_LOCKS=0`.
    pub async fn stats(&self, id: &str) -> Result<WorktreeStats> {
        let row = self.worktree_row(id).await?;
        let path = PathBuf::from(&row.path);
        if !path.exists() {
            return Err(GildeError::not_found(format!("worktree path {}", row.path)));
        }

        let status = git::status_v2(&path).await?;
        let base = row.base_sha.clone().unwrap_or_default();

        let (ahead, behind) = if base.is_empty() {
            (0, 0)
        } else {
            git::rev_list_lr(&path, &base).await.unwrap_or((0, 0))
        };
        let committed = if base.is_empty() {
            NumStat::default()
        } else {
            git::numstat(&path, &base).await.unwrap_or_default()
        };
        let uncommitted = git::numstat_worktree(&path).await.unwrap_or_default();
        let diff = committed.merge(uncommitted);
        let last = git::last_commit(&path).await.unwrap_or(None);

        let stats = WorktreeStats {
            dirty_files: status.dirty_files,
            staged: status.staged,
            untracked: status.untracked,
            conflicted: status.conflicted,
            added: diff.added,
            removed: diff.removed,
            ahead,
            behind,
            last_commit_at: last.as_ref().map(|(ts, _)| ts * 1000),
            last_commit_subject: last.map(|(_, s)| s),
        };

        let cache = WorktreeStatsCache {
            dirty: i64::from(stats.dirty_files),
            added: stats.added as i64,
            removed: stats.removed as i64,
            ahead: i64::from(stats.ahead),
            behind: i64::from(stats.behind),
        };
        let cache_id = row.id.clone();
        let _ = self
            .db
            .write(move |conn| dao::worktree::update_stats_cache(conn, &cache_id, &cache))
            .await;

        Ok(stats)
    }

    /// Recompute stats and publish `gilde://worktree/changed`, rate limited to one
    /// pass per [`STATS_MIN_INTERVAL`] per worktree.
    pub async fn refresh_and_publish(&self, id: &str) {
        {
            let mut last = self.last_stats.lock();
            if let Some(at) = last.get(id) {
                if at.elapsed() < STATS_MIN_INTERVAL {
                    return;
                }
            }
            last.insert(id.to_string(), Instant::now());
        }
        match self.stats(id).await {
            Ok(stats) => {
                if let Some(bus) = &self.bus {
                    bus.emit(
                        WORKTREE_CHANGED,
                        WorktreeChangedEvent {
                            worktree_id: id.to_string(),
                            dirty: stats.dirty_files,
                            added: stats.added,
                            removed: stats.removed,
                            ahead: stats.ahead,
                            behind: stats.behind,
                            at: now_ms(),
                        },
                    );
                }
            }
            Err(err) => {
                tracing::debug!(target: "worktree", worktree_id = id, error = %err, "stats refresh failed")
            }
        }
    }

    // -- preflight ----------------------------------------------------------

    /// Collect every fact the removal card needs before anything is destroyed.
    pub async fn remove_preflight(&self, id: &str) -> Result<RemovePreflight> {
        let row = self.worktree_row(id).await?;
        let path = PathBuf::from(&row.path);

        let status = if path.exists() {
            git::status_v2(&path).await.unwrap_or_default()
        } else {
            StatusV2::default()
        };

        let unmerged_commits = match (path.exists(), row.base_sha.as_deref()) {
            (true, Some(base)) if !base.is_empty() => {
                git::rev_list_lr(&path, base).await.unwrap_or((0, 0)).0
            }
            _ => 0,
        };

        let live_sessions = self.live_sessions(id).await?;
        let running_runs = self.running_runs(id).await?;
        let alias_links = self.alias_links(id).await?;
        let foreign_pids = self.foreign_pids(id, &path).await?;

        let dirty = status.dirty_files > 0 || status.untracked > 0;
        let allowed_modes = if !live_sessions.is_empty() {
            // Guard 1 blocks every mode, so offer none.
            Vec::new()
        } else if dirty {
            vec![RemoveMode::CommitThenRemove, RemoveMode::ArchiveThenRemove]
        } else {
            vec![RemoveMode::KeepBranch, RemoveMode::ArchiveThenRemove]
        };

        Ok(RemovePreflight {
            dirty_files: status.dirty_files,
            untracked: status.untracked,
            unmerged_commits,
            live_sessions,
            running_runs,
            alias_links,
            foreign_pids,
            allowed_modes,
        })
    }

    async fn live_sessions(&self, worktree_id: &str) -> Result<Vec<String>> {
        let id = worktree_id.to_string();
        let rows = self
            .db
            .read(move |conn| dao::proc::by_worktree(conn, &id))
            .await?;
        Ok(rows
            .into_iter()
            .filter(|r| r.state == ProcState::Running)
            .map(|r| r.session_id.unwrap_or(r.spawn_id))
            .collect())
    }

    async fn running_runs(&self, worktree_id: &str) -> Result<Vec<String>> {
        let id = worktree_id.to_string();
        self.db
            .read(move |conn| {
                let mut out = Vec::new();
                for status in ["queued", "spawning", "running"] {
                    for run in dao::dispatch::runs_by_status(conn, status)? {
                        if run.worktree_id.as_deref() == Some(id.as_str()) {
                            out.push(run.id);
                        }
                    }
                }
                Ok(out)
            })
            .await
    }

    async fn alias_links(&self, worktree_id: &str) -> Result<Vec<String>> {
        let id = worktree_id.to_string();
        let rows = self
            .db
            .read(move |conn| dao::worktree_alias::list_by_worktree(conn, &id))
            .await?;
        Ok(rows.into_iter().map(|r| r.alias_path).collect())
    }

    /// Processes using the worktree that are **not** ours.
    async fn foreign_pids(&self, worktree_id: &str, path: &Path) -> Result<Vec<i32>> {
        if !path.exists() {
            return Ok(Vec::new());
        }
        let id = worktree_id.to_string();
        let ours: HashSet<i32> = self
            .db
            .read(move |conn| dao::proc::by_worktree(conn, &id))
            .await?
            .into_iter()
            .filter_map(|r| r.pid.map(|p| p as i32))
            .collect();

        let path = path.to_path_buf();
        let mut pids =
            tokio::task::spawn_blocking(move || probe_foreign_pids(&path))
                .await
                .unwrap_or_default();
        pids.retain(|p| !ours.contains(p));
        Ok(pids)
    }

    // -- remove -------------------------------------------------------------

    pub async fn remove(&self, id: &str, mode: RemoveMode) -> Result<RemoveOutcome> {
        self.remove_typed(id, mode).await.map_err(GildeError::from)
    }

    /// DESIGN §7-7. The guard set is enforced here, not in the UI.
    pub async fn remove_typed(&self, id: &str, mode: RemoveMode) -> WtResult<RemoveOutcome> {
        let row = self.worktree_row(id).await?;
        if row.removed_at.is_some() {
            return Err(WorktreeFailure::Other(GildeError::not_found(
                "worktree (already removed)",
            )));
        }
        let project = self.project_row(&row.project_id).await?;
        let repo = PathBuf::from(&project.canonical_path);
        let path = PathBuf::from(&row.path);
        let mut warnings: Vec<String> = Vec::new();

        // guard 1 — live registry processes block every mode.
        let live = self.live_sessions(id).await?;
        if !live.is_empty() {
            return Err(WorktreeError::SessionsLive {
                count: live.len(),
                sessions: live,
            }
            .into());
        }

        // guard 2 — foreign processes are reported by the preflight; reaching
        // `remove` means the user saw the card and chose to continue. We record
        // it as a warning rather than silently pretending it did not happen.
        let foreign = self.foreign_pids(id, &path).await?;
        if !foreign.is_empty() {
            warnings.push(format!(
                "foreign processes still using the worktree: {foreign:?}"
            ));
        }

        // guard 3 — a dirty tree cannot be dropped with KeepBranch.
        let status = if path.exists() {
            git::status_v2(&path).await.unwrap_or_default()
        } else {
            StatusV2::default()
        };
        let dirty = !status.is_clean();
        if dirty && mode == RemoveMode::KeepBranch {
            return Err(WorktreeError::Dirty {
                dirty_files: status.dirty_files,
                untracked: status.untracked,
            }
            .into());
        }
        if !dirty && mode == RemoveMode::CommitThenRemove {
            return Err(WorktreeError::ModeNotAllowed {
                mode: RemoveMode::CommitThenRemove.as_key(),
                allowed: vec![RemoveMode::KeepBranch, RemoveMode::ArchiveThenRemove],
            }
            .into());
        }

        let mut commit_sha = None;
        let mut archive_path = None;

        match mode {
            RemoveMode::CommitThenRemove => {
                commit_sha = Some(git::checkpoint_commit(&path, CHECKPOINT_MESSAGE).await?);
            }
            RemoveMode::ArchiveThenRemove => {
                let dest = self.archive_destination(&project.repo_slug, &row.slug);
                copy_tree_excluding_git(&path, &dest)?;
                archive_path = Some(dest.display().to_string());
            }
            RemoveMode::KeepBranch => {}
        }

        // The alias layer removes only symlinks it owns (T14).
        if let Some(hook) = alias_hook() {
            let removed = hook.remove_owned(id).await;
            if !removed.is_empty() {
                tracing::info!(target: "worktree", count = removed.len(), "removed owned aliases");
            }
        }

        self.stop_watch(id);

        let removed;
        if path.exists() {
            // Unlock first: `git worktree remove` refuses a locked worktree and
            // the alternative is `--force`, which this codebase does not have.
            if let Err(err) = git::worktree_lock(&path, false, "").await {
                warnings.push(format!("unlock failed: {err}"));
            }
            match git::worktree_remove(&repo, &path).await {
                Ok(()) => removed = true,
                // `git worktree remove` refuses a tree with modified or
                // untracked files. In `ArchiveThenRemove` that is exactly the
                // expected state, and the user's work is already copied out —
                // so we delete the directory ourselves and let `prune` clean
                // up git's bookkeeping. This is the *only* path that removes a
                // dirty directory, it happens only after a verified archive,
                // and it is reported in `warnings`.
                Err(err) if mode == RemoveMode::ArchiveThenRemove => {
                    let archived = archive_path
                        .as_deref()
                        .map(|p| Path::new(p).exists())
                        .unwrap_or(false);
                    if !archived {
                        return Err(WorktreeFailure::Other(err));
                    }
                    warnings.push(format!(
                        "git declined to remove the dirty worktree ({err}); the archive is complete, so the directory was deleted directly"
                    ));
                    std::fs::remove_dir_all(&path)?;
                    removed = true;
                }
                Err(err) => return Err(WorktreeFailure::Other(err)),
            }
        } else {
            warnings.push("worktree directory was already gone".to_string());
            removed = true;
        }

        if let Err(err) = git::worktree_prune(&repo).await {
            warnings.push(format!("prune failed: {err}"));
        }

        let id_owned = id.to_string();
        let at = now_ms();
        self.db
            .write(move |conn| dao::worktree::mark_removed(conn, &id_owned, at).map(|_| ()))
            .await?;

        Ok(RemoveOutcome {
            mode,
            removed,
            archive_path,
            commit_sha,
            // The app never deletes a branch (DESIGN §7-7).
            branch_kept: row.branch,
            warnings,
        })
    }

    fn archive_destination(&self, repo_slug: &str, slug: &str) -> PathBuf {
        let ts = now_ms();
        self.paths
            .archive_root()
            .join(repo_slug)
            .join(format!("{slug}-{ts}"))
    }

    // -- prune --------------------------------------------------------------

    /// Compare git, disk and the database, then act only where DESIGN §7-8 says
    /// it is safe: prune git's bookkeeping, drop empty rows, and **report**
    /// disk-only directories without deleting them.
    pub async fn prune(&self, project_id: &str) -> Result<PruneReport> {
        let project = self.project_row(project_id).await?;
        let repo = PathBuf::from(&project.canonical_path);
        let repo_slug = if project.repo_slug.is_empty() {
            naming::repo_slug(&repo)
        } else {
            project.repo_slug.clone()
        };

        let git_entries = git::worktree_list(&repo).await.unwrap_or_default();
        let git_paths: HashSet<PathBuf> = git_entries
            .iter()
            .filter(|e| e.path != repo)
            .map(|e| e.path.clone())
            .collect();

        let root = self.paths.worktrees_root().join(&repo_slug);
        let mut disk_paths: HashSet<PathBuf> = HashSet::new();
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    disk_paths.insert(entry.path());
                }
            }
        }

        let pid = project_id.to_string();
        let rows = self
            .db
            .read(move |conn| dao::worktree::list_by_project(conn, &pid, false))
            .await?;

        let mut report = PruneReport::default();

        for entry in &git_entries {
            if entry.path == repo {
                continue;
            }
            if !entry.path.exists() {
                report.git_only.push(entry.path.display().to_string());
            }
        }

        for dir in &disk_paths {
            if !git_paths.contains(dir) {
                report.disk_only.push(dir.display().to_string());
            }
        }

        // Disk truth beats a stored flag: a row whose directory is gone is
        // dropped, whether or not git still holds bookkeeping for it. A stale
        // flag must never resurrect a false "unmerged work" warning.
        let mut db_only_ids = Vec::new();
        for row in &rows {
            let p = PathBuf::from(&row.path);
            if !p.exists() {
                report.db_only.push(row.path.clone());
                db_only_ids.push(row.id.clone());
            } else if git_paths.contains(&p) {
                report.healthy.push(row.path.clone());
            }
        }

        if !report.git_only.is_empty() {
            // Bookkeeping only: `git worktree prune` never touches a directory.
            if let Err(err) = git::worktree_prune(&repo).await {
                tracing::warn!(target: "worktree", error = %err, "git worktree prune failed");
            }
        }

        if !db_only_ids.is_empty() {
            let at = now_ms();
            let ids = db_only_ids.clone();
            self.db
                .write(move |conn| {
                    for id in &ids {
                        dao::worktree::mark_removed(conn, id, at)?;
                    }
                    Ok(())
                })
                .await?;
            for id in &db_only_ids {
                self.stop_watch(id);
            }
        }

        report.git_only.sort();
        report.disk_only.sort();
        report.db_only.sort();
        report.healthy.sort();
        Ok(report)
    }

    // -- lock ---------------------------------------------------------------

    pub async fn set_locked(&self, id: &str, locked: bool) -> Result<()> {
        let row = self.worktree_row(id).await?;
        let path = PathBuf::from(&row.path);
        if path.exists() {
            git::worktree_lock(&path, locked, git::LOCK_REASON).await?;
        }
        let id = row.id.clone();
        self.db
            .write(move |conn| dao::worktree::set_locked(conn, &id, locked).map(|_| ()))
            .await
    }

    // -- watching -----------------------------------------------------------

    /// Start (or restart) the debounced filesystem watch for one worktree.
    ///
    /// Blind timer polling is deliberately absent: a 200 ms `git status` poll
    /// across eight worktrees is measurable CPU, and background noise is the
    /// thing this app exists to remove (DESIGN §7-5).
    pub fn start_watch(&self, worktree_id: &str, path: &Path) {
        if !path.exists() || self.arc().is_none() {
            return;
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();

        let debouncer = match notify_debouncer_full::new_debouncer(
            WATCH_DEBOUNCE,
            None,
            move |result: notify_debouncer_full::DebounceEventResult| {
                if let Ok(events) = result {
                    if events.iter().any(|e| e.paths.iter().any(|p| is_interesting(p))) {
                        let _ = tx.send(());
                    }
                }
            },
        ) {
            Ok(d) => d,
            Err(err) => {
                tracing::warn!(target: "worktree", error = %err, "could not create worktree watcher");
                return;
            }
        };

        let mut debouncer = debouncer;
        if let Err(err) = debouncer.watch(path, notify::RecursiveMode::Recursive) {
            tracing::warn!(target: "worktree", path = %path.display(), error = %err, "could not watch worktree");
            return;
        }

        let id = worktree_id.to_string();
        let weak = self.me.clone();
        let task = async move {
            while rx.recv().await.is_some() {
                let Some(manager) = weak.upgrade() else { break };
                manager.refresh_and_publish(&id).await;
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(task);
            }
            Err(_) => {
                tauri::async_runtime::spawn(task);
            }
        }

        self.watchers.lock().insert(
            worktree_id.to_string(),
            WatchHandle {
                _debouncer: debouncer,
            },
        );
    }

    /// Drop the watcher (and, with it, the pump task's sender).
    pub fn stop_watch(&self, worktree_id: &str) {
        self.watchers.lock().remove(worktree_id);
        self.last_stats.lock().remove(worktree_id);
    }

    pub fn watch_count(&self) -> usize {
        self.watchers.lock().len()
    }

    /// Re-arm watches for every live worktree. Called at boot (T29).
    pub async fn resume_watches(&self) -> Result<usize> {
        let rows = self.db.read(dao::worktree::list_live).await?;
        let mut started = 0;
        for row in rows {
            let path = PathBuf::from(&row.path);
            if path.exists() {
                self.start_watch(&row.id, &path);
                started += 1;
            }
        }
        Ok(started)
    }

    // -- post-create command ------------------------------------------------

    /// Run the per-project post-create command in the new worktree.
    ///
    /// The command string is split with `shell-words`, **not** handed to a
    /// shell. Spawning a shell with a `-c` string is forbidden in this tree
    /// (`scripts/lint-forbidden.sh` enforces it); an argv array is the only
    /// form that cannot be turned into an injection.
    pub async fn run_post_create(&self, cwd: &Path, cmd: &str) -> Result<PostCreateResult> {
        let argv = shell_words::split(cmd)
            .map_err(|e| GildeError::config(format!("post-create command is not parseable: {e}")))?;
        let Some((program, args)) = argv.split_first() else {
            return Err(GildeError::config("post-create command is empty"));
        };

        let resolved = if program.contains('/') {
            PathBuf::from(program)
        } else {
            which::which(program)
                .map_err(|e| GildeError::spawn(format!("post-create command {program}: {e}")))?
        };

        let full: Vec<String> = std::iter::once(resolved.to_string_lossy().into_owned())
            .chain(args.iter().cloned())
            .collect();
        let ticket = git::plan_spawn("probe", &full, cwd).await;

        let mut command = tokio::process::Command::new(&resolved);
        command.args(args);
        command.current_dir(cwd);
        command.stdin(Stdio::null());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        command.process_group(0);
        command.kill_on_drop(true);
        if let Some(t) = ticket.as_deref() {
            command.env("GILDE_SPAWN_ID", t.env_marker());
        }

        let child = command
            .spawn()
            .map_err(|e| GildeError::spawn(format!("post-create command: {e}")))?;
        let pid = child.id().map(|p| p as i32);
        if let (Some(t), Some(pid)) = (ticket.as_deref(), pid) {
            t.mark_running(pid, pid);
        }

        let output = match tokio::time::timeout(POST_CREATE_TIMEOUT, child.wait_with_output()).await
        {
            Ok(res) => res.map_err(|e| GildeError::spawn(format!("post-create command: {e}")))?,
            Err(_) => {
                if let Some(pid) = pid {
                    #[cfg(unix)]
                    {
                        use nix::sys::signal::{killpg, Signal};
                        use nix::unistd::Pid;
                        let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
                    }
                }
                if let Some(t) = ticket.as_deref() {
                    t.mark_exited(None, Some(libc::SIGKILL));
                }
                return Ok(PostCreateResult {
                    cmd: cmd.to_string(),
                    exit_code: None,
                    tail: format!(
                        "timed out after {}s",
                        POST_CREATE_TIMEOUT.as_secs()
                    ),
                });
            }
        };
        if let Some(t) = ticket.as_deref() {
            t.mark_exited(output.status.code(), None);
        }

        let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
        combined.push_str(&String::from_utf8_lossy(&output.stderr));
        Ok(PostCreateResult {
            cmd: cmd.to_string(),
            exit_code: output.status.code(),
            tail: tail_of(&combined, POST_CREATE_TAIL),
        })
    }

    fn report_post_create(&self, worktree_id: &str, result: &PostCreateResult) {
        let ok = result.exit_code == Some(0);
        tracing::info!(
            target: "worktree",
            worktree_id,
            cmd = %result.cmd,
            exit_code = ?result.exit_code,
            "post-create command finished"
        );
        self.toast(
            if ok { ToastLevel::Success } else { ToastLevel::Warn },
            if ok {
                "worktree.postCreate.ok"
            } else {
                "worktree.postCreate.failed"
            },
            &[
                ("worktreeId", Value::from(worktree_id)),
                ("cmd", Value::from(result.cmd.clone())),
                (
                    "exitCode",
                    result.exit_code.map(Value::from).unwrap_or(Value::Null),
                ),
                ("tail", Value::from(result.tail.clone())),
            ],
        );
    }

    fn toast(&self, level: ToastLevel, key: &str, params: &[(&str, Value)]) {
        let Some(bus) = &self.bus else { return };
        let mut toast = ToastEvent::new(level, key);
        for (k, v) in params {
            toast.params.insert((*k).to_string(), v.clone());
        }
        bus.emit(crate::events::TOAST, toast);
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Where `branch` is currently checked out, if anywhere.
///
/// git enforces "one worktree per branch"; discovering that *before* calling
/// `worktree add` is what lets the UI offer "open the existing worktree"
/// instead of surfacing a raw git failure (DESIGN §7-3).
pub async fn branch_in_use(repo: &Path, branch: &str) -> Result<Option<PathBuf>> {
    let entries = git::worktree_list(repo).await?;
    Ok(entries
        .into_iter()
        .find(|e| e.short_branch() == Some(branch))
        .map(|e| e.path))
}

/// True when a changed path is worth recomputing stats for.
fn is_interesting(path: &Path) -> bool {
    !path.components().any(|c| {
        let s = c.as_os_str().to_string_lossy();
        WATCH_IGNORED.contains(&s.as_ref())
    })
}

/// Last `max` bytes of `s`, cut on a character boundary.
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

/// Copy `src` to `dest`, skipping `.git` (DESIGN §7-7, `ArchiveThenRemove`).
///
/// The archive is a rescue copy of the user's *files*; the git metadata is
/// still in the repository and copying a linked worktree's `.git` file would
/// produce a broken half-repository.
pub fn copy_tree_excluding_git(src: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    let mut stack = vec![(src.to_path_buf(), dest.to_path_buf())];

    while let Some((from, to)) = stack.pop() {
        std::fs::create_dir_all(&to)?;
        for entry in std::fs::read_dir(&from)? {
            let entry = entry?;
            let name = entry.file_name();
            if name == std::ffi::OsStr::new(".git") {
                continue;
            }
            let src_path = entry.path();
            let dest_path = to.join(&name);
            let meta = entry.metadata()?;
            if meta.is_symlink() {
                let target = std::fs::read_link(&src_path)?;
                let _ = std::fs::remove_file(&dest_path);
                #[cfg(unix)]
                std::os::unix::fs::symlink(&target, &dest_path)?;
            } else if meta.is_dir() {
                stack.push((src_path, dest_path));
            } else {
                std::fs::copy(&src_path, &dest_path)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_mode_wire_names_match_the_ipc_contract() {
        for (mode, key) in [
            (RemoveMode::KeepBranch, "keepBranch"),
            (RemoveMode::ArchiveThenRemove, "archiveThenRemove"),
            (RemoveMode::CommitThenRemove, "commitThenRemove"),
        ] {
            assert_eq!(serde_json::to_value(mode).unwrap(), Value::from(key));
            assert_eq!(mode.as_key(), key);
            assert_eq!(
                serde_json::from_value::<RemoveMode>(Value::from(key)).unwrap(),
                mode
            );
        }
        // `Force` must not be expressible.
        assert!(serde_json::from_value::<RemoveMode>(Value::from("force")).is_err());
    }

    #[test]
    fn worktree_serializes_in_the_documented_shape() {
        let wt = Worktree {
            id: "w1".into(),
            project_id: "p1".into(),
            slug: "fix-login-ab12cd".into(),
            branch: "gilde/fix-login-ab12cd".into(),
            path: "/tmp/x".into(),
            base_sha: "abc".into(),
            origin: WorktreeOrigin::Created,
            created_at: 1,
            removed_at: None,
            locked: true,
            verified_at: Some(2),
            post_create_cmd: Some("npm ci".into()),
        };
        let v = serde_json::to_value(&wt).unwrap();
        assert_eq!(v["projectId"], "p1");
        assert_eq!(v["baseSha"], "abc");
        assert_eq!(v["origin"], "created");
        assert_eq!(v["postCreateCmd"], "npm ci");
        assert_eq!(v["removedAt"], Value::Null);
    }

    #[test]
    fn create_request_accepts_the_frontend_shape() {
        let req: CreateWorktreeReq = serde_json::from_value(serde_json::json!({
            "projectId": "p1",
            "taskName": "fix login",
            "aliasProviders": [{ "provider": "claude-w" }],
            "postCreateCmd": "npm ci"
        }))
        .unwrap();
        assert_eq!(req.project_id, "p1");
        assert_eq!(req.task_name, "fix login");
        assert_eq!(req.alias_providers.len(), 1);
        assert_eq!(req.post_create_cmd.as_deref(), Some("npm ci"));
        assert!(req.base_ref.is_none());
    }

    #[test]
    fn preflight_and_stats_serialize_in_camel_case() {
        let v = serde_json::to_value(RemovePreflight {
            dirty_files: 2,
            untracked: 1,
            unmerged_commits: 3,
            live_sessions: vec!["s1".into()],
            running_runs: vec![],
            alias_links: vec!["/repo/.claude/worktrees/x".into()],
            foreign_pids: vec![42],
            allowed_modes: vec![RemoveMode::CommitThenRemove],
        })
        .unwrap();
        assert_eq!(v["dirtyFiles"], 2);
        assert_eq!(v["unmergedCommits"], 3);
        assert_eq!(v["liveSessions"][0], "s1");
        assert_eq!(v["foreignPids"][0], 42);
        assert_eq!(v["allowedModes"][0], "commitThenRemove");

        let v = serde_json::to_value(WorktreeStats {
            dirty_files: 1,
            staged: 1,
            untracked: 0,
            conflicted: 0,
            added: 10,
            removed: 2,
            ahead: 1,
            behind: 0,
            last_commit_at: Some(5),
            last_commit_subject: Some("init".into()),
        })
        .unwrap();
        assert_eq!(v["dirtyFiles"], 1);
        assert_eq!(v["lastCommitAt"], 5);
        assert_eq!(v["lastCommitSubject"], "init");
    }

    #[test]
    fn worktree_errors_carry_structured_params() {
        let e = WorktreeError::BranchInUse {
            branch: "gilde/x".into(),
            existing_path: "/wt/x".into(),
        };
        assert_eq!(e.key(), "error.worktree.branchInUse");
        assert_eq!(e.reason(), "branchInUse");
        assert_eq!(e.params()["branch"], "gilde/x");
        assert_eq!(e.params()["existingPath"], "/wt/x");

        let e = WorktreeError::SessionsLive {
            sessions: vec!["s1".into(), "s2".into()],
            count: 2,
        };
        assert_eq!(e.params()["count"], 2);
        let err: GildeError = e.into();
        assert_eq!(err.code(), "REFUSED");

        let e = WorktreeError::Dirty {
            dirty_files: 3,
            untracked: 1,
        };
        assert_eq!(e.params()["dirtyFiles"], 3);
    }

    #[test]
    fn watch_filter_skips_noise_directories() {
        assert!(is_interesting(Path::new("/wt/src/main.rs")));
        assert!(!is_interesting(Path::new("/wt/.git/index")));
        assert!(!is_interesting(Path::new("/wt/node_modules/a/b.js")));
        assert!(!is_interesting(Path::new("/wt/target/debug/x")));
        assert!(!is_interesting(Path::new("/wt/dist/bundle.js")));
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
    fn archive_copy_skips_the_git_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("wt");
        std::fs::create_dir_all(src.join(".git/objects")).unwrap();
        std::fs::create_dir_all(src.join("src")).unwrap();
        std::fs::write(src.join("src/main.rs"), b"fn main() {}").unwrap();
        std::fs::write(src.join(".git/HEAD"), b"ref: refs/heads/main").unwrap();

        let dest = tmp.path().join("archive/wt-1");
        copy_tree_excluding_git(&src, &dest).unwrap();

        assert!(dest.join("src/main.rs").exists());
        assert!(!dest.join(".git").exists(), ".git must not be archived");
    }
}
