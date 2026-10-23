//! Worktree lifecycle (DESIGN §7).
//!
//! Everything in this module is shaped by one invariant from DESIGN §0:
//! **no operation that can lose the user's work happens without an explicit
//! choice.** Concretely:
//!
//! * `git worktree remove --force` exists exactly once, in
//!   [`git::remove_worktree_acknowledged`], called only when every loss was
//!   listed and acknowledged (A36.4); `tests/worktree_safety.rs` greps the
//!   source to keep it that way. Gilde never deletes a worktree directory
//!   itself and never runs a repo-wide `git worktree prune` (A36.5).
//! * If Gilde cannot list a loss precisely it refuses with a reason and
//!   deletes nothing: commits on no branch are a listed loss; an operation
//!   in progress, nested git content, a worktree Gilde did not create and a
//!   lock Gilde did not set are refusals (A36, A37.3).
//! * `git branch -d` / `-D` appear nowhere either. An unmerged `gilde/*` branch is
//!   cheap and recoverable; a deleted one is not.
//! * A dirty tree can only be removed through `CommitThenRemove` or
//!   `ArchiveThenRemove`, never through `KeepBranch`.
//! * A worktree with a live registry process is refused in **every** mode.
//! * `prune` reports, and never deletes a directory or an admin entry.
//!
//! We also never run `git checkout` in the user's own checkout (DESIGN §7-4):
//! "switching worktree" is a UI concept. N live agents must not have the ground
//! moved under them.
//!
//! # Gilde 2 (contract §4.2.2, §4.2.5, A6; owner T11)
//!
//! [`GitService`] is the `GitApi`: the Changes view (status, numstat, hunk
//! names, made-by), commit, merge and discard behind single-use confirm
//! tokens, and `create_worktree` for `session_start{NewWorktree}`. A
//! session's place comes from World ([`PlaceSource`]) and is re-identified
//! from disk (§5.6); nothing is persisted. New worktrees go to
//! `<repo>/../<repo>-wt/<slug>` — or, only with the LP-40 app flag
//! [`LP40_FLAG`], to `<repo>/.gilde/worktrees/<slug>` with `/.gilde/` in
//! `.git/info/exclude` (A6). [`WorktreeManager`] is the kept v1 lifecycle
//! (`lib.rs`, the v1 gates and their tests); its post-create command is
//! planned through `SupervisorApi` and fails closed (A12.8, A14.2).

pub mod alias; // T14  provider path alias layer
pub mod git;
pub mod naming;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{GildeError, Result};
use crate::events::{ToastEvent, ToastLevel, WorktreeChangedEvent};
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
    /// git knows the worktree, the directory is gone → reported only (A36.5).
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

/// Where the manager reports `gilde://worktree/changed` and its toasts. Held
/// as an option so the manager is fully usable in tests without a
/// `tauri::AppHandle`.
pub trait WorktreeEvents: Send + Sync {
    fn changed(&self, event: WorktreeChangedEvent);
    fn toast(&self, toast: ToastEvent);
}

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
    events: Option<Arc<dyn WorktreeEvents>>,
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
    /// `events`: where `gilde://worktree/changed` and toasts go (the boot
    /// passes T09's `events::Events`; the legacy graph bus is gone, A22.3).
    pub fn new(db: Db, paths: Arc<AppPaths>, events: Option<Arc<dyn WorktreeEvents>>) -> Arc<Self> {
        Self::build(db, paths, events)
    }

    /// The manager with an explicit event sink (tests record what it emits).
    pub fn with_events(db: Db, paths: Arc<AppPaths>, events: Arc<dyn WorktreeEvents>) -> Arc<Self> {
        Self::build(db, paths, Some(events))
    }

    fn build(db: Db, paths: Arc<AppPaths>, events: Option<Arc<dyn WorktreeEvents>>) -> Arc<Self> {
        Arc::new_cyclic(|me| WorktreeManager {
            me: me.clone(),
            db,
            paths,
            events,
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
            tolerate(git::rev_list_lr(&path, &base).await)?
        };
        let committed = if base.is_empty() {
            NumStat::default()
        } else {
            tolerate(git::numstat(&path, &base).await)?
        };
        let uncommitted = tolerate(git::numstat_worktree(&path).await)?;
        let diff = committed.merge(uncommitted);
        let last = tolerate(git::last_commit(&path).await)?;

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
                if let Some(events) = &self.events {
                    events.changed(
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
            tolerate(git::status_v2(&path).await)?
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
            tolerate(git::status_v2(&path).await)?
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

        // The Gilde 2 rules hold here too (A36, A37.3, A38.7), before anything
        // is committed, copied or removed: never through a lock Gilde did not
        // set, never with an operation in progress or nested git content, and
        // never while HEAD holds commits no other ref keeps (this path has no
        // way to list them).
        let mut lock = LockState::Unlocked;
        let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if path.exists() {
            let entries = git::worktree_list(&repo).await?;
            let entry = entries.iter().skip(1).find(|e| canonical_lexical(&e.path) == canonical);
            match entry.and_then(|e| e.locked.as_deref()) {
                None => {}
                Some(reason) if reason == git::LOCK_REASON => lock = LockState::Gilde,
                Some(_) => return Err(WorktreeError::Locked.into()),
            }
            let gate = |e: git::GitError| WorktreeFailure::Other(e.into_gilde("worktree", &repo));
            if let Some(reason) = removal_blocker(&repo, &canonical, &entries).await.map_err(gate)? {
                return Err(WorktreeFailure::Other(GildeError::refused(reason)));
            }
            if unreachable_commits(&canonical, &entries).await.map_err(gate)? > 0 {
                return Err(WorktreeFailure::Other(GildeError::refused("unmergedCommits")));
            }
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
            // Gilde's own lock is released for the removal and restored when
            // it fails (A36.6); any other lock refused above.
            if lock == LockState::Gilde {
                git::worktree_lock(&path, false, "").await?;
            }
            // A dirty tree reaches here only as `ArchiveThenRemove` after a
            // verified archive — the one acknowledged loss of this path — and
            // only then is the removal forced (A36.4). git deletes it; there is
            // no fallback that deletes a directory itself.
            let archived = archive_path.as_deref().is_some_and(|p| Path::new(p).exists());
            let result = if dirty && mode == RemoveMode::ArchiveThenRemove && archived {
                git::remove_worktree_acknowledged(&repo, &path).await
            } else {
                git::remove_worktree(&repo, &path).await
            };
            if let Err(err) = result {
                restore_after_failed_remove(&repo, &canonical, lock).await;
                return Err(WorktreeFailure::Other(err.into_gilde("worktree remove", &repo)));
            }
            removed = true;
        } else {
            // Only this worktree's own admin entry, and only while its
            // directory is gone (A36.5) — never a repo-wide prune.
            match git::prune_target_admin(&repo, &path) {
                Ok(_) => {}
                Err(err) => warnings.push(format!("could not clean the admin entry: {}", err.kind())),
            }
            warnings.push("worktree directory was already gone".to_string());
            removed = true;
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
    /// it is safe: drop empty rows, and **report** git-only entries and
    /// disk-only directories without touching them (A36.5).
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

        // `git_only` is reported, never pruned (A36.5): a worktree whose
        // directory is missing may be on a volume that is not mounted right
        // now, and pruning it would lose its HEAD, index and the commits only
        // it references.

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
    ///
    /// The program is planned through the bound `SupervisorApi` before it
    /// exists (A12.8). A refusal — `REFUSED{notAllowlisted}` for any program
    /// the §9.3 allowlist does not name, the boot gate, memory-only mode — is
    /// returned and **nothing runs**; with no supervisor bound it is refused
    /// the same way (A14.2: fail closed). Gilde 2 offers no post-create
    /// command over IPC (§4.8), so in the app this path only ever refuses.
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

        let outcome = git::run_program(
            crate::contract::process::ProcKind::Probe,
            &resolved,
            args,
            cwd,
            POST_CREATE_TIMEOUT,
            POST_CREATE_TAIL,
        )
        .await
        .map_err(|e| ipc_to_gilde(&e))?;
        Ok(PostCreateResult {
            cmd: cmd.to_string(),
            exit_code: outcome.exit_code,
            tail: outcome.tail,
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
        let Some(events) = &self.events else { return };
        let mut toast = ToastEvent::new(level, key);
        for (k, v) in params {
            toast.params.insert((*k).to_string(), v.clone());
        }
        events.toast(toast);
    }
}

// ===========================================================================
// Gilde 2 — Changes, commit, merge, discard, new worktrees (contract §4.2.5,
// §4.2.2 `NewWorktree`, §9.4 diff base) through the existing gates
// ===========================================================================
//
// Nothing here keeps state on disk: a session's location comes from World,
// git answers every question, and a destructive step happens only behind a
// single-use confirm token whose digest is re-computed at execution
// (§4.1 rule 4). The v1 gates hold: no `--force`, no branch deletion, no
// `checkout` in the user's checkout, a live session blocks a discard.

/// The i18n reason of an action the owner of a watched session blocks (#108).
pub const REASON_OWNER_WRITING: &str = "changes.ownerWriting";
/// The i18n reason of a Discard that waits for the live session to stop.
pub const REASON_STOP_TO_DISCARD: &str = "changes.stopToDiscard";
/// The app flag that turns the LP-40 worktree root on (A6, §1.3 P41).
pub const LP40_FLAG: &str = "worktreesInRepo";
/// `ChangesView.files` and `commits` caps (§4.2.5).
pub const MAX_FILE_ROWS: usize = 2000;
pub const MAX_COMMIT_ROWS: usize = 200;
/// Hunk-header names per file (`FileRow.parts`, ≤ 8).
pub const MAX_PARTS: usize = 8;
/// Bytes of an untracked file read to count its lines; past the budget the
/// count is 0 (the row still shows).
const UNTRACKED_READ_CAP: u64 = 1024 * 1024;
const UNTRACKED_READ_BUDGET: u64 = 32 * 1024 * 1024;
/// The single-use confirm token lifetime (§4.1 rule 4).
const TOKEN_TTL: Duration = Duration::from_secs(60);
/// Other sessions of a location whose edits are attributed (`OtherSession`).
const MADE_BY_PEERS: usize = 16;

use crate::contract::common::{AbsPath, Vendor};
use crate::contract::content::ChangeKind;
use crate::contract::error::IpcError;
use crate::contract::ids::{ConfirmToken, LocationId, ProjectId, SessionUid};
use crate::contract::ipc::changes::{
    ActionState, ChangesActions, ChangesBase, ChangesGetReq, ChangesView, CommitRow, FileRow,
    GitAuthor, GitCommitPreviewReq, GitCommitPreviewRes, GitCommitReq, GitCommitRes,
    GitMergePreviewReq, GitMergePreviewRes, GitMergeReq, GitMergeRes, Loss, LossKind, MadeBy,
    MergeEffect, MergeKind, SharedFolder, WorktreeDiscardPreviewReq, WorktreeDiscardPreviewRes,
    WorktreeDiscardReq,
};
use crate::contract::project::{Location, Project, ProjectKind};
use crate::contract::services::{CompatApi, GitApi, LocationFacts, WorldApi};
use crate::contract::session::{ChangesCap, Control, Session};

/// An `IpcError` from a kept-v1 helper back into the internal error.
fn ipc_to_gilde(e: &IpcError) -> GildeError {
    match e.code {
        crate::contract::ErrorCode::Refused => GildeError::refused(
            e.params.get("reason").and_then(|v| v.as_str()).unwrap_or("refused"),
        ),
        crate::contract::ErrorCode::DiskFull => GildeError::Io(std::io::Error::from_raw_os_error(libc::ENOSPC)),
        _ => GildeError::spawn(e.key.clone()),
    }
}

fn git_err(e: git::GitError) -> IpcError {
    e.to_ipc()
}

/// The v1 reads treat a failed git call as an empty answer, except a read
/// that A46.1 skipped. Gilde could not read that tree without running the
/// repository's program, so the read never counts as "nothing changed"
/// (a clean tree for a removal, zero lines in the stats).
fn tolerate<T: Default>(answer: Result<T>) -> Result<T> {
    match answer {
        Err(e) if git::is_repo_program_error(&e) => Err(e),
        other => Ok(other.unwrap_or_default()),
    }
}

/// A git failure while creating a worktree (A17.4): git refusing or failing
/// is `REFUSED{worktreeGate}` with git's sanitized stderr as the detail; a
/// timeout stays `TIMEOUT` and a spawn refusal keeps its own code
/// (`NOT_READY{bootSweep}`, `DISK_FULL`, `SPAWN_FAILED`).
fn gate_err(e: git::GitError) -> IpcError {
    // A lock that a timed-out git left (A37.2) is a git failure too.
    let gated = matches!(e, git::GitError::Failed { .. } | git::GitError::TooLarge) || git::is_lock_left(&e);
    if !gated {
        return e.to_ipc();
    }
    let gate = IpcError::refused("worktreeGate");
    match e.to_ipc().detail {
        Some(d) => gate.with_detail(d),
        None => gate,
    }
}

// ---------------------------------------------------------------------------
// Places: where a session works (World is the source)
// ---------------------------------------------------------------------------

/// What World knows, in one read.
#[derive(Debug, Clone, Default)]
pub struct PlaceView {
    pub sessions: Vec<Session>,
    pub locations: Vec<Location>,
    pub projects: Vec<Project>,
}

/// The slice of World this module reads (a narrow seam so tests can supply
/// sessions and locations without a World).
pub trait PlaceSource: Send + Sync {
    fn session(&self, uid: &SessionUid) -> Option<Session>;
    fn view(&self) -> PlaceView;
    fn transcript(&self, uid: &SessionUid) -> Option<(AbsPath, Vendor)>;
    /// Tell World about a location Gilde created or the user picked (§5.6).
    fn location_seen(&self, facts: LocationFacts);
}

/// [`PlaceSource`] over the real World (T02).
pub struct WorldPlaces(pub Arc<dyn WorldApi>);

impl PlaceSource for WorldPlaces {
    fn session(&self, uid: &SessionUid) -> Option<Session> {
        self.0.session(uid)
    }

    fn view(&self) -> PlaceView {
        let snap = self.0.snapshot();
        PlaceView {
            sessions: snap.sessions,
            locations: snap.locations,
            projects: snap.projects,
        }
    }

    fn transcript(&self, uid: &SessionUid) -> Option<(AbsPath, Vendor)> {
        self.0.transcript(uid)
    }

    fn location_seen(&self, facts: LocationFacts) {
        self.0.sink().location(facts);
    }
}

/// One session's place (all paths canonical).
#[derive(Debug, Clone)]
pub struct Place {
    pub uid: SessionUid,
    pub vendor: Vendor,
    /// A watched session whose owner may still be writing (§4.2.5,
    /// `REFUSED{ownerAlive}`; ux-ia #108).
    pub owner_alive: bool,
    pub in_gilde: bool,
    pub location_id: LocationId,
    /// The worktree root, main checkout or folder.
    pub location: PathBuf,
    pub project_id: ProjectId,
    pub project_root: PathBuf,
    pub git: bool,
    /// A linked worktree (not the main checkout).
    pub worktree: bool,
    /// The other existing locations of the project.
    pub other_roots: Vec<PathBuf>,
}

/// Whether a session counts as running (In Gilde or Watching).
pub fn is_live(s: &Session) -> bool {
    matches!(s.control, Control::InGilde { .. } | Control::Watching { .. })
}

fn owner_alive(s: &Session) -> bool {
    matches!(s.control, Control::Watching { .. }) || s.caps.changes == ChangesCap::ReadOnly
}

/// Resolve `uid`'s place: World's location for the session, else its cwd,
/// re-identified from disk with the §5.6 rules (no git spawn).
pub fn place_of(src: &dyn PlaceSource, home: &Path, uid: &SessionUid) -> std::result::Result<(Place, PlaceView), IpcError> {
    let session = src.session(uid).ok_or_else(|| IpcError::not_found("session"))?;
    let view = src.view();
    let dir = session
        .location_id
        .as_ref()
        .and_then(|id| view.locations.iter().find(|l| &l.id == id))
        .map(|l| PathBuf::from(l.path.as_str()))
        .or_else(|| session.cwd.as_ref().map(|c| PathBuf::from(c.as_str())))
        .ok_or_else(|| IpcError::not_found("location"))?;
    let id = crate::discovery::location::identify(&dir, home).ok_or_else(|| IpcError::not_found("location"))?;
    let other_roots = view
        .locations
        .iter()
        .filter(|l| l.project_id == id.project_id && l.exists)
        .filter_map(|l| std::fs::canonicalize(l.path.as_str()).ok())
        .filter(|p| p != &id.location_path)
        .collect();
    let place = Place {
        uid: session.uid.clone(),
        vendor: session.vendor,
        owner_alive: owner_alive(&session),
        in_gilde: matches!(session.control, Control::InGilde { .. }),
        location_id: id.location_id.clone(),
        location: id.location_path.clone(),
        project_id: id.project_id.clone(),
        project_root: id.project_root.clone(),
        git: id.git,
        worktree: id.worktree,
        other_roots,
    };
    Ok((place, view))
}

/// The sessions whose place is `place`'s location: by World's location id;
/// a session without one by the §5.6 identity of its cwd (never by a path
/// prefix — an LP-40 worktree lives *inside* its main checkout).
fn sessions_here<'a>(view: &'a PlaceView, place: &'a Place, home: &'a Path) -> impl Iterator<Item = &'a Session> + 'a {
    view.sessions.iter().filter(move |s| {
        if s.merged_into.is_some() {
            return false;
        }
        match (&s.location_id, &s.cwd) {
            (Some(id), _) => id == &place.location_id,
            (None, Some(c)) => crate::discovery::location::identify(Path::new(c.as_str()), home)
                .is_some_and(|i| i.location_id == place.location_id),
            (None, None) => false,
        }
    })
}

// ---------------------------------------------------------------------------
// Made by: the session's own edit/write tool calls (§4.2.5, ux-ia §3.13)
// ---------------------------------------------------------------------------

/// Claude tools whose input names the file they change.
const CLAUDE_EDIT_TOOLS: &[&str] = &["Edit", "MultiEdit", "Write", "NotebookEdit"];
/// Bytes of one transcript read per call (the rest continues next call).
const MADE_BY_READ_PER_CALL: u64 = 64 * 1024 * 1024;
const MADE_BY_CHUNK: usize = 1024 * 1024;
/// A transcript line longer than this is skipped (its tail is never parsed).
const MADE_BY_MAX_LINE: usize = 16 * 1024 * 1024;
/// Transcripts indexed at once.
const MADE_BY_MAX_TRANSCRIPTS: usize = 256;

#[derive(Default)]
struct Scan {
    id: Option<crate::discovery::safe_read::FileId>,
    offset: u64,
    carry: Vec<u8>,
    skipping: bool,
    paths: HashSet<String>,
}

/// Incremental, memory-only index of the files each transcript's edit and
/// write tool calls named (content: never persisted or logged).
#[derive(Default)]
pub struct MadeByIndex {
    scans: Mutex<HashMap<PathBuf, Scan>>,
}

impl MadeByIndex {
    pub fn new() -> MadeByIndex {
        MadeByIndex::default()
    }

    /// The absolute paths `transcript`'s edit/write tool calls named
    /// (canonical where they exist; relative codex paths joined to `cwd`).
    /// `None` = unknown (unreadable, or a vendor with no tool events).
    /// Blocking: call from the blocking pool.
    pub fn edited(&self, transcript: &Path, vendor: Vendor, cwd: &Path) -> Option<HashSet<String>> {
        if !matches!(vendor, Vendor::Claude | Vendor::Codex) {
            return None;
        }
        let parent = transcript.parent()?;
        let root = crate::discovery::safe_read::Root::new(parent).ok()?;
        let file = crate::discovery::safe_read::open_in(&root, transcript).ok()?;
        let mut scans = self.scans.lock();
        if scans.len() >= MADE_BY_MAX_TRANSCRIPTS && !scans.contains_key(transcript) {
            // Memory only and cheap to rebuild: start over rather than grow.
            scans.clear();
        }
        let scan = scans.entry(transcript.to_path_buf()).or_default();
        let same = scan
            .id
            .is_some_and(|id| id.inode == file.id.inode && id.dev == file.id.dev && file.id.size >= scan.offset);
        if !same {
            *scan = Scan::default();
        }
        scan.id = Some(file.id);
        let end = file.id.size.min(scan.offset.saturating_add(MADE_BY_READ_PER_CALL));
        while scan.offset < end {
            let want = ((end - scan.offset) as usize).min(MADE_BY_CHUNK);
            let Ok(bytes) = file.read_range(scan.offset, want) else { break };
            if bytes.is_empty() {
                break;
            }
            scan.offset += bytes.len() as u64;
            let mut start = 0usize;
            for nl in memchr::memchr_iter(b'\n', &bytes) {
                if !scan.skipping {
                    scan.carry.extend_from_slice(&bytes[start..nl]);
                    let line = std::mem::take(&mut scan.carry);
                    collect_edited(&line, vendor, cwd, &mut scan.paths);
                }
                scan.carry.clear();
                scan.skipping = false;
                start = nl + 1;
            }
            if !scan.skipping {
                scan.carry.extend_from_slice(&bytes[start..]);
                if scan.carry.len() > MADE_BY_MAX_LINE {
                    scan.carry.clear();
                    scan.skipping = true;
                }
            }
        }
        Some(scan.paths.clone())
    }
}

/// Add the files one transcript line's edit/write calls name.
fn collect_edited(line: &[u8], vendor: Vendor, cwd: &Path, out: &mut HashSet<String>) {
    let hint: &[&[u8]] = match vendor {
        Vendor::Claude => &[b"\"tool_use\""],
        _ => &[b"apply_patch", b"FileChange", b"fileChange", b"file_change", b"*** "],
    };
    if !hint.iter().any(|h| memchr::memmem::find(line, h).is_some()) {
        return;
    }
    let Ok(v) = serde_json::from_slice::<Value>(line) else { return };
    let mut add = |p: &str| {
        let path = Path::new(p);
        let abs = if path.is_absolute() { path.to_path_buf() } else { cwd.join(path) };
        out.insert(canonical_lexical(&abs).to_string_lossy().into_owned());
    };
    match vendor {
        Vendor::Claude => {
            let blocks = v.pointer("/message/content").and_then(Value::as_array);
            for b in blocks.into_iter().flatten() {
                if b.get("type").and_then(Value::as_str) != Some("tool_use") {
                    continue;
                }
                let name = b.get("name").and_then(Value::as_str).unwrap_or("");
                if !CLAUDE_EDIT_TOOLS.contains(&name) {
                    continue;
                }
                let input = b.get("input");
                for key in ["file_path", "notebook_path"] {
                    if let Some(p) = input.and_then(|i| i.get(key)).and_then(Value::as_str) {
                        add(p);
                    }
                }
            }
        }
        _ => codex_paths(&v, &mut add, 0),
    }
}

/// codex rollouts: `FileChange` items (`changes` as a map of paths or a list
/// of `{path}`) and `apply_patch` texts (`*** Add/Update/Delete File:`,
/// `*** Move to:`), wherever they sit in the row.
fn codex_paths(v: &Value, add: &mut dyn FnMut(&str), depth: usize) {
    if depth > 8 {
        return;
    }
    match v {
        Value::Object(map) => {
            if let Some(changes) = map.get("changes") {
                match changes {
                    Value::Object(m) => m.keys().for_each(|k| add(k.as_str())),
                    Value::Array(list) => list
                        .iter()
                        .filter_map(|c| c.get("path").and_then(Value::as_str))
                        .for_each(&mut *add),
                    _ => {}
                }
            }
            for (_, child) in map {
                codex_paths(child, add, depth + 1);
            }
        }
        Value::Array(list) => list.iter().for_each(|c| codex_paths(c, add, depth + 1)),
        Value::String(s) if s.contains("*** ") => {
            // A function-call `arguments` string may itself be JSON.
            if s.trim_start().starts_with('{') {
                if let Ok(inner) = serde_json::from_str::<Value>(s) {
                    codex_paths(&inner, add, depth + 1);
                    return;
                }
            }
            for l in s.lines() {
                for prefix in ["*** Add File: ", "*** Update File: ", "*** Delete File: ", "*** Move to: "] {
                    if let Some(p) = l.strip_prefix(prefix) {
                        let p = p.trim();
                        if !p.is_empty() {
                            add(p);
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

/// `p` without `.`/`..` components, with its longest existing ancestor
/// canonicalized (so `/tmp` vs `/private/tmp` and symlinked homes agree).
pub fn canonical_lexical(p: &Path) -> PathBuf {
    let mut clean = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                clean.pop();
            }
            std::path::Component::CurDir => {}
            other => clean.push(other.as_os_str()),
        }
    }
    let mut tail = Vec::new();
    let mut probe = clean.clone();
    loop {
        if let Ok(real) = std::fs::canonicalize(&probe) {
            let mut out = real;
            for t in tail.iter().rev() {
                out.push(t);
            }
            return out;
        }
        match (probe.file_name().map(|n| n.to_os_string()), probe.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name);
                probe = parent.to_path_buf();
            }
            _ => return clean,
        }
    }
}

// ---------------------------------------------------------------------------
// confirm tokens (§4.1 rule 4)
// ---------------------------------------------------------------------------

struct Pending<T> {
    uid: SessionUid,
    data: T,
    at: Instant,
}

/// Single-use 60 s tokens bound to a session and a digest of the previewed
/// state. Every failure to redeem is `CONFLICT{changed}` (the documented value).
struct Tokens<T> {
    map: Mutex<HashMap<String, Pending<T>>>,
}

impl<T: Clone> Tokens<T> {
    fn new() -> Tokens<T> {
        Tokens { map: Mutex::new(HashMap::new()) }
    }

    fn issue(&self, uid: &SessionUid, data: T) -> ConfirmToken {
        let token = ConfirmToken::mint();
        let mut map = self.map.lock();
        map.retain(|_, p| p.at.elapsed() < TOKEN_TTL);
        map.insert(token.as_str().to_string(), Pending { uid: uid.clone(), data, at: Instant::now() });
        token
    }

    /// Take a live token of `uid` (A37.4: atomically single use — the caller
    /// takes it before its first `await`, so a second concurrent execution is
    /// `CONFLICT{changed}`). `check` sees the previewed data first, under the
    /// same lock; when it refuses (a request that does not match the preview,
    /// decided without touching anything) the token stays for a corrected call.
    fn take_if(
        &self,
        token: &ConfirmToken,
        uid: &SessionUid,
        check: impl FnOnce(&T) -> std::result::Result<(), IpcError>,
    ) -> std::result::Result<T, IpcError> {
        let mut map = self.map.lock();
        match map.get(token.as_str()) {
            Some(p) if p.at.elapsed() < TOKEN_TTL && &p.uid == uid => check(&p.data)?,
            _ => return Err(IpcError::conflict("changed")),
        }
        map.remove(token.as_str()).map(|p| p.data).ok_or_else(|| IpcError::conflict("changed"))
    }
}

fn digest(parts: &[String]) -> String {
    use sha2::{Digest as _, Sha256};
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update([0u8]);
    }
    hex::encode(h.finalize())
}

// ---------------------------------------------------------------------------
// GitService
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct CommitPending {
    /// The real paths of the previewed rows.
    paths: Vec<String>,
    /// Their wire paths (`pathRel`), for the check made before the token is taken.
    rels: Vec<String>,
    digest: String,
}

#[derive(Clone)]
struct MergePending {
    digest: String,
}

#[derive(Clone)]
struct DiscardPending {
    digest: String,
    /// The previewed losses, each of which must be acknowledged.
    loss_ids: Vec<String>,
}

/// Gilde's own lock on a worktree it is about to discard (A36.6, A37.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LockState {
    Unlocked,
    /// Locked with [`git::LOCK_REASON`]: released only for the removal and
    /// restored when it fails.
    Gilde,
}

/// Everything a discard decides on (preview and execution compute it alike).
struct DiscardState {
    losses: Vec<Loss>,
    digest: String,
    lock: LockState,
}

/// One row before it is sent: the real path git knows and its wire row.
#[derive(Debug, Clone)]
struct Row {
    real: String,
    row: FileRow,
    /// Staged, unstaged or untracked in the working tree — the rows a commit
    /// can take (A37.1). A change already committed on the branch is not.
    uncommitted: bool,
}

/// The diff base of a location (§9.4: merge-base with the branch's base for
/// worktrees, `HEAD` otherwise).
#[derive(Debug, Clone)]
struct Base {
    /// The commit the working tree is compared with (or the empty tree).
    rev: String,
    /// For a worktree whose main checkout is on a branch.
    branch: Option<BaseBranch>,
}

#[derive(Debug, Clone)]
struct BaseBranch {
    name: String,
    merge_base: String,
}

/// The `GitApi` (T11): Changes, commit, merge, discard, new worktrees.
pub struct GitService {
    src: Arc<dyn PlaceSource>,
    home: PathBuf,
    compat: Option<Arc<dyn CompatApi>>,
    made_by: Arc<MadeByIndex>,
    commits: Tokens<CommitPending>,
    merges: Tokens<MergePending>,
    discards: Tokens<DiscardPending>,
}

impl GitService {
    /// Over the real World (`home` = the user's home, the §5.6 walk boundary).
    pub fn new(world: Arc<dyn WorldApi>, home: PathBuf) -> GitService {
        GitService::with_source(Arc::new(WorldPlaces(world)), home)
    }

    pub fn with_source(src: Arc<dyn PlaceSource>, home: PathBuf) -> GitService {
        GitService {
            src,
            home,
            compat: None,
            made_by: Arc::new(MadeByIndex::new()),
            commits: Tokens::new(),
            merges: Tokens::new(),
            discards: Tokens::new(),
        }
    }

    /// The compat service whose app flag [`LP40_FLAG`] switches the worktree
    /// root (A6; absent = the fallback root).
    pub fn with_compat(mut self, compat: Arc<dyn CompatApi>) -> GitService {
        self.compat = Some(compat);
        self
    }

    pub fn place(&self, uid: &SessionUid) -> std::result::Result<(Place, PlaceView), IpcError> {
        place_of(self.src.as_ref(), &self.home, uid)
    }

    /// The worktree layout new worktrees get (A6).
    pub fn layout(&self) -> naming::WorktreeLayout {
        match &self.compat {
            Some(c) if c.app_feature(LP40_FLAG) => naming::WorktreeLayout::InRepo,
            _ => naming::WorktreeLayout::Sibling,
        }
    }

    async fn base_of(&self, place: &Place) -> std::result::Result<Base, IpcError> {
        let loc = &place.location;
        let head = git::resolve_commit(loc, "HEAD").await.map_err(git_err)?;
        let Some(head) = head else {
            return Ok(Base { rev: git::EMPTY_TREE.to_string(), branch: None });
        };
        if place.worktree {
            if let Some(name) = git::head_branch(&place.project_root).await.map_err(git_err)? {
                if let Some(mb) = git::merge_base(loc, "HEAD", &format!("refs/heads/{name}")).await.map_err(git_err)? {
                    return Ok(Base { rev: mb.clone(), branch: Some(BaseBranch { name, merge_base: mb }) });
                }
            }
        }
        Ok(Base { rev: head, branch: None })
    }

    /// The file rows of `place` against `base` (≤ 2000, sorted by path).
    async fn rows(&self, place: &Place, view: &PlaceView, base: &Base) -> std::result::Result<Vec<Row>, IpcError> {
        let loc = &place.location;
        let entries = git::diff_entries(loc, &base.rev).await.map_err(git_err)?;
        let untracked = git::untracked_files(loc).await.map_err(git_err)?;
        let parts = git::hunk_names(loc, &base.rev, MAX_PARTS).await.map_err(git_err)?;
        let open = git::uncommitted_paths(loc).await.map_err(git_err)?;
        let made_by = self.made_by_fn(place, view).await;

        let mut rows: Vec<Row> = Vec::with_capacity(entries.len() + untracked.len());
        for e in entries {
            let change = match e.status {
                'A' => ChangeKind::Added,
                'D' => ChangeKind::Deleted,
                _ => ChangeKind::Modified,
            };
            rows.push(Row {
                row: FileRow {
                    path_rel: display_rel(&e.path),
                    change,
                    added: e.added.unwrap_or(0),
                    removed: e.removed.unwrap_or(0),
                    parts: parts.get(&e.path).map(|p| sanitize_parts(p)).unwrap_or_default(),
                    made_by: made_by(&e.path),
                },
                uncommitted: open.contains(&e.path),
                real: e.path,
            });
        }
        let loc_owned = loc.clone();
        let names = untracked.clone();
        let counts = tokio::task::spawn_blocking(move || {
            let mut budget = UNTRACKED_READ_BUDGET;
            names.iter().map(|p| line_count(&loc_owned.join(p), &mut budget)).collect::<Vec<u32>>()
        })
        .await
        .unwrap_or_default();
        for (i, p) in untracked.into_iter().enumerate() {
            rows.push(Row {
                row: FileRow {
                    path_rel: display_rel(&p),
                    change: ChangeKind::Added,
                    added: counts.get(i).copied().unwrap_or(0),
                    removed: 0,
                    parts: Vec::new(),
                    made_by: made_by(&p),
                },
                uncommitted: true,
                real: p,
            });
        }
        rows.sort_by(|a, b| a.real.cmp(&b.real));
        // A path both in the diff and untracked keeps the uncommitted mark.
        rows.dedup_by(|later, kept| {
            let same = later.real == kept.real;
            if same {
                kept.uncommitted |= later.uncommitted;
            }
            same
        });
        rows.truncate(MAX_FILE_ROWS);
        Ok(rows)
    }

    /// `path_rel → MadeBy` for `place` (cursor/agy → `Unknown`).
    async fn made_by_fn(&self, place: &Place, view: &PlaceView) -> Box<dyn Fn(&str) -> MadeBy + Send + Sync> {
        if !matches!(place.vendor, Vendor::Claude | Vendor::Codex) {
            return Box::new(|_| MadeBy::Unknown);
        }
        let own = self.src.transcript(&place.uid);
        let mut peers: Vec<&Session> = sessions_here(view, place, &self.home)
            .filter(|s| s.uid != place.uid && matches!(s.vendor, Vendor::Claude | Vendor::Codex))
            .collect();
        peers.sort_by_key(|s| std::cmp::Reverse(s.last_activity_at.unwrap_or(0)));
        peers.truncate(MADE_BY_PEERS);
        let peer_transcripts: Vec<(SessionUid, (AbsPath, Vendor), PathBuf)> = peers
            .iter()
            .filter_map(|s| {
                let t = self.src.transcript(&s.uid)?;
                let cwd = s.cwd.as_ref().map(|c| PathBuf::from(c.as_str())).unwrap_or_else(|| place.location.clone());
                Some((s.uid.clone(), t, cwd))
            })
            .collect();
        let index = self.made_by.clone();
        let loc = place.location.clone();
        let (mine, others) = tokio::task::spawn_blocking(move || {
            let mine = own.and_then(|(p, v)| index.edited(Path::new(p.as_str()), v, &loc));
            let others: Vec<(SessionUid, HashSet<String>)> = peer_transcripts
                .into_iter()
                .filter_map(|(uid, (p, v), cwd)| Some((uid, index.edited(Path::new(p.as_str()), v, &cwd)?)))
                .collect();
            (mine, others)
        })
        .await
        .unwrap_or((None, Vec::new()));
        let fold = crate::ide::paths::case_insensitive(&place.location);
        let key = move |s: &str| if fold { s.to_lowercase() } else { s.to_string() };
        let mine: Option<HashSet<String>> = mine.map(|m| m.iter().map(|s| key(s)).collect());
        let others: Vec<(SessionUid, HashSet<String>)> =
            others.into_iter().map(|(u, set)| (u, set.iter().map(|s| key(s)).collect())).collect();
        let loc = place.location.clone();
        Box::new(move |rel: &str| {
            let abs = key(&loc.join(rel).to_string_lossy());
            if let Some(m) = &mine {
                if m.contains(&abs) {
                    return MadeBy::ThisSession;
                }
            }
            if let Some((uid, _)) = others.iter().find(|(_, set)| set.contains(&abs)) {
                return MadeBy::OtherSession { uid: uid.clone() };
            }
            if mine.is_some() {
                MadeBy::NotFromKnownSession
            } else {
                MadeBy::Unknown
            }
        })
    }

    /// Whether a live session works at or under `place`'s location (this one
    /// included): by World's location id, by that location's path, or by the
    /// session's cwd — in any location, so a session in a worktree nested
    /// inside this one counts too (A36.3).
    fn live_at_or_under(&self, view: &PlaceView, place: &Place) -> bool {
        let target = &place.location;
        let fold = crate::ide::paths::case_insensitive(target);
        let under = |p: &str| crate::ide::paths::within(&canonical_lexical(Path::new(p)), target, fold);
        view.sessions.iter().filter(|s| s.merged_into.is_none() && is_live(s)).any(|s| {
            let by_location = s.location_id.as_ref().is_some_and(|id| {
                id == &place.location_id || view.locations.iter().any(|l| &l.id == id && under(l.path.as_str()))
            });
            by_location || s.cwd.as_ref().is_some_and(|c| under(c.as_str()))
        })
    }

    fn require_worktree(place: &Place) -> std::result::Result<(), IpcError> {
        if !place.git {
            return Err(IpcError::unsupported("notGit"));
        }
        if !place.worktree {
            return Err(IpcError::unsupported("notAWorktree"));
        }
        Ok(())
    }

    /// Per-path state for the commit digest: change, counts and the working
    /// file's `(size, mtime, mode)`.
    fn row_state(place: &Place, row: &Row) -> String {
        let meta = std::fs::symlink_metadata(place.location.join(&row.real))
            .map(|m| {
                use std::os::unix::fs::MetadataExt as _;
                format!("{}:{}.{}:{:o}", m.size(), m.mtime(), m.mtime_nsec(), m.mode())
            })
            .unwrap_or_else(|_| "absent".into());
        format!("{}|{:?}|{}|{}|{meta}", row.real, row.row.change, row.row.added, row.row.removed)
    }

    /// The merge of the worktree's HEAD into the main checkout's branch:
    /// `(effect, digest, base branch, worktree commit)`. A worktree whose
    /// HEAD is unborn has nothing to merge: up to date, no commit.
    async fn merge_state(&self, place: &Place) -> std::result::Result<(MergeEffect, String, String, Option<String>), IpcError> {
        let main = &place.project_root;
        let branch = git::head_branch(main).await.map_err(git_err)?.ok_or_else(|| IpcError::refused("noBaseBranch"))?;
        let ours = git::resolve_commit(main, "HEAD").await.map_err(git_err)?.ok_or_else(|| IpcError::refused("noBaseBranch"))?;
        let Some(theirs) = git::resolve_commit(&place.location, "HEAD").await.map_err(git_err)? else {
            let effect = MergeEffect { kind: MergeKind::UpToDate, commits: 0, files_changed: 0 };
            let d = digest(&[place.location.to_string_lossy().into_owned(), branch.clone(), ours, "unborn".into()]);
            return Ok((effect, d, branch, None));
        };
        let kind = if git::is_ancestor(main, &theirs, &ours).await.map_err(git_err)? {
            MergeKind::UpToDate
        } else if git::is_ancestor(main, &ours, &theirs).await.map_err(git_err)? {
            MergeKind::FastForward
        } else {
            MergeKind::MergeCommit
        };
        let commits = git::count(main, &format!("{ours}..{theirs}")).await.map_err(git_err)?;
        let files = git::exec(
            main,
            &["diff", "--name-only", "-z", "--no-renames", "--no-ext-diff", "--no-textconv", "--end-of-options", &format!("{ours}...{theirs}"), "--"],
            git::ExecOpts::default(),
        )
        .await
        .map_err(git_err)?
        .nul_fields()
        .len() as u32;
        let effect = MergeEffect { kind, commits, files_changed: files };
        let d = digest(&[place.location.to_string_lossy().into_owned(), branch.clone(), ours.clone(), theirs.clone(), format!("{kind:?}")]);
        Ok((effect, d, branch, Some(theirs)))
    }

    /// The gates every merge passes (preview and execution): the main
    /// checkout has no uncommitted tracked change, and the merge does not
    /// conflict (checked without touching any working tree).
    async fn merge_gates(place: &Place, effect: &MergeEffect, theirs: Option<&str>) -> std::result::Result<(), IpcError> {
        if git::tracked_dirty(&place.project_root).await.map_err(git_err)? {
            return Err(IpcError::refused("mainDirty"));
        }
        if let (MergeKind::MergeCommit, Some(theirs)) = (effect.kind, theirs) {
            if git::merge_conflicts(&place.project_root, "HEAD", theirs).await.map_err(git_err)? {
                return Err(IpcError::refused("conflicts"));
            }
        }
        Ok(())
    }

    /// What a discard of `place` would lose — or why it cannot be listed
    /// exactly, in which case it refuses and deletes nothing (A36, A37.3).
    /// Preview and execution run exactly this.
    async fn discard_state(&self, place: &Place, view: &PlaceView) -> std::result::Result<DiscardState, IpcError> {
        Self::require_worktree(place)?;
        if place.owner_alive {
            return Err(IpcError::refused("ownerAlive"));
        }
        if self.live_at_or_under(view, place) {
            return Err(IpcError::refused("sessionsLive").with_key(REASON_STOP_TO_DISCARD));
        }
        let (entry, entries) = self.check_linked_worktree(place).await?;
        let loc = &place.location;

        // A37.3: only a worktree Gilde made, and never through a lock Gilde
        // did not set.
        if !Self::gilde_created(place, &entry).await? {
            return Err(keyed_refusal(REFUSED_NOT_GILDE_CREATED));
        }
        let lock = match entry.locked.as_deref() {
            None => LockState::Unlocked,
            Some(reason) if reason == git::LOCK_REASON => LockState::Gilde,
            Some(_) => return Err(keyed_refusal(REFUSED_LOCKED_BY_USER)),
        };

        // A36.2, A36.3: what a list of losses cannot describe.
        if let Some(reason) = removal_blocker(&place.project_root, loc, &entries).await.map_err(git_err)? {
            return Err(keyed_refusal(reason));
        }

        let status = git::status_for_discard(loc).await.map_err(git_err)?;
        let tracked = status.dirty_files;
        let untracked = status.untracked + status.ignored;
        let unreachable = unreachable_commits(loc, &entries).await.map_err(git_err)?;
        let mut losses = Vec::new();
        if tracked > 0 {
            losses.push(Loss { id: "uncommittedChanges".into(), kind: LossKind::UncommittedChanges, count: tracked });
        }
        if untracked > 0 {
            losses.push(Loss { id: "untrackedFiles".into(), kind: LossKind::UntrackedFiles, count: untracked });
        }
        if unreachable > 0 {
            losses.push(Loss { id: "unmergedCommits".into(), kind: LossKind::UnmergedCommits, count: unreachable });
        }

        // A37.5: the digest covers HEAD (sha and branch vs detached), the lock,
        // every loss and the state on disk of each dirty, untracked or ignored
        // entry, so editing an already-dirty file after the preview is a change.
        let head = git::resolve_commit(loc, "HEAD").await.map_err(git_err)?.unwrap_or_default();
        let mut parts = vec![
            loc.to_string_lossy().into_owned(),
            head,
            entry.branch.clone().unwrap_or_else(|| "detached".into()),
            format!("{lock:?}"),
            format!("{tracked}:{untracked}:{unreachable}"),
        ];
        let loc_owned = loc.clone();
        let records: Vec<(String, String)> =
            status.entries.iter().map(|e| (format!("{}{}", e.x, e.y), e.path.clone())).collect();
        let mut stated = tokio::task::spawn_blocking(move || {
            records
                .into_iter()
                .map(|(xy, path)| format!("{xy}{path}|{}", stat_state(&loc_owned.join(&path))))
                .collect::<Vec<String>>()
        })
        .await
        .map_err(|_| IpcError::internal("discard-state"))?;
        stated.sort();
        parts.extend(stated);
        Ok(DiscardState { losses, digest: digest(&parts), lock })
    }

    /// The worktree is a linked worktree git knows for this repository —
    /// never the main checkout, `$HOME` or `/` (the last line before a
    /// directory is deleted). Returns its entry and the whole list.
    async fn check_linked_worktree(
        &self,
        place: &Place,
    ) -> std::result::Result<(git::GitWorktreeEntry, Vec<git::GitWorktreeEntry>), IpcError> {
        let loc = &place.location;
        let home = std::fs::canonicalize(&self.home).unwrap_or_else(|_| self.home.clone());
        if loc == &place.project_root
            || place.project_root.starts_with(loc)
            || loc == &home
            || loc.parent().is_none()
            || !std::fs::symlink_metadata(loc.join(".git")).is_ok_and(|m| m.is_file())
        {
            return Err(IpcError::unsupported("notAWorktree"));
        }
        let entries = git::list_worktrees(&place.project_root).await.map_err(git_err)?;
        let entry = entries
            .iter()
            .skip(1)
            .find(|e| std::fs::canonicalize(&e.path).is_ok_and(|p| &p == loc))
            .cloned()
            .ok_or_else(|| IpcError::unsupported("notAWorktree"))?;
        Ok((entry, entries))
    }

    /// A17.7's one rule, with the branch git reports for the worktree. A
    /// detached HEAD (an agent checked out a commit, a rebase is running) is
    /// judged by the branch Gilde created it with — `gilde/<directory name>`
    /// — when that branch still exists.
    async fn gilde_created(place: &Place, entry: &git::GitWorktreeEntry) -> std::result::Result<bool, IpcError> {
        let branch = match entry.short_branch() {
            Some(b) => Some(b.to_string()),
            None => match place.location.file_name().and_then(|n| n.to_str()) {
                Some(slug) => {
                    let b = naming::branch_name(slug);
                    git::branch_present(&place.project_root, &b).await.map_err(git_err)?.then_some(b)
                }
                None => None,
            },
        };
        Ok(naming::is_gilde_created(&place.project_root, &place.location, branch.as_deref()))
    }

    /// HEAD's sha and branch (`detached` when it has none), for the commit
    /// digest (A37.5): a commit or a checkout after the preview is a change.
    async fn head_state(place: &Place) -> std::result::Result<String, IpcError> {
        let loc = &place.location;
        let head = git::resolve_commit(loc, "HEAD").await.map_err(git_err)?.unwrap_or_default();
        let branch = git::head_branch(loc).await.map_err(git_err)?.unwrap_or_else(|| "detached".into());
        Ok(format!("{head}|{branch}"))
    }

    /// The base side of `ide_open{Diff}` for `rel` (relative to the session's
    /// location): `Ok(None)` when the file did not exist at the base.
    pub async fn diff_base(&self, uid: &SessionUid, rel: &str, cap: usize) -> std::result::Result<Option<Vec<u8>>, IpcError> {
        let (place, _) = self.place(uid)?;
        if !place.git {
            return Err(IpcError::unsupported("diffNotSupportedByEditor"));
        }
        let base = self.base_of(&place).await?;
        if base.rev == git::EMPTY_TREE {
            return Ok(None);
        }
        match git::blob_size(&place.location, &base.rev, rel).await.map_err(git_err)? {
            None => Ok(None),
            Some(size) if size as usize > cap => Err(IpcError::unsupported("diffNotSupportedByEditor")),
            Some(_) => Ok(Some(git::blob(&place.location, &base.rev, rel, cap).await.map_err(git_err)?)),
        }
    }
}

// ---------------------------------------------------------------------------
// What a removal cannot list (A36, A37.3) — shared by discard and the v1 path
// ---------------------------------------------------------------------------

/// `REFUSED{operationInProgress}` (A36.2).
pub const REFUSED_OPERATION_IN_PROGRESS: &str = "operationInProgress";
/// `REFUSED{nestedRepository}` (A36.3).
pub const REFUSED_NESTED_REPOSITORY: &str = "nestedRepository";
/// `REFUSED{notGildeCreated}` (A37.3).
pub const REFUSED_NOT_GILDE_CREATED: &str = "notGildeCreated";
/// `REFUSED{lockedByUser}` (A37.3).
pub const REFUSED_LOCKED_BY_USER: &str = "lockedByUser";
/// Directory entries a nested-repository scan visits before it gives up and
/// refuses (it cannot say there is none).
const NESTED_SCAN_MAX: usize = 1_000_000;

/// A refusal with its own sentence, `changes.refused.<reason>` (A38.7).
fn keyed_refusal(reason: &str) -> IpcError {
    IpcError::refused(reason).with_key(&format!("changes.refused.{reason}"))
}

fn commit_digest(head: &str, states: &[String]) -> String {
    let mut parts = Vec::with_capacity(states.len() + 1);
    parts.push(head.to_string());
    parts.extend(states.iter().cloned());
    digest(&parts)
}

/// `(size, mtime, mode)` of one path, never following a symlink.
fn stat_state(path: &Path) -> String {
    std::fs::symlink_metadata(path)
        .map(|m| {
            use std::os::unix::fs::MetadataExt as _;
            format!("{}:{}.{}:{:o}", m.size(), m.mtime(), m.mtime_nsec(), m.mode())
        })
        .unwrap_or_else(|_| "absent".into())
}

/// Why removing the linked worktree `target` of `repo` could lose what no
/// list describes (A36.2, A36.3), or `None`:
/// * an operation in progress in its own git dir → `operationInProgress`;
/// * another worktree registered under it, a `.git` file or directory (or a
///   bare repository) anywhere below its root, a submodule in its index or
///   a submodule store in its git dir → `nestedRepository`.
async fn removal_blocker(
    repo: &Path,
    target: &Path,
    entries: &[git::GitWorktreeEntry],
) -> std::result::Result<Option<&'static str>, git::GitError> {
    let Some((gitdir, _)) = git::git_dirs(target) else {
        return Err(git::GitError::Failed { code: -1, stderr: "the worktree's git dir could not be read".into() });
    };
    if git::operation_in_progress(&gitdir).is_some() {
        return Ok(Some(REFUSED_OPERATION_IN_PROGRESS));
    }
    let fold = crate::ide::paths::case_insensitive(target);
    let nested_worktree = entries.iter().any(|e| {
        let p = canonical_lexical(&e.path);
        p != target && p != repo && crate::ide::paths::within(&p, target, fold)
    });
    if nested_worktree {
        return Ok(Some(REFUSED_NESTED_REPOSITORY));
    }
    let root = target.to_path_buf();
    let modules = gitdir.join("modules");
    let found = tokio::task::spawn_blocking(move || {
        std::fs::symlink_metadata(&modules).is_ok() || nested_git_below(&root, NESTED_SCAN_MAX)
    })
    .await
    .unwrap_or(true);
    if found || git::has_gitlinks(target).await? {
        return Ok(Some(REFUSED_NESTED_REPOSITORY));
    }
    Ok(None)
}

/// Whether anything git-shaped lies below `root`: an entry named `.git`
/// (file, directory or symlink) other than the root's own, or a directory
/// that is a bare repository (`HEAD` + `objects/` + `refs/`). Symlinks are
/// never followed. An unreadable directory or more than `max` entries is
/// answered `true`: the scan cannot say there is nothing.
pub fn nested_git_below(root: &Path, max: usize) -> bool {
    let mut stack = vec![(root.to_path_buf(), true)];
    let mut seen = 0usize;
    while let Some((dir, is_root)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { return true };
        let (mut head, mut objects, mut refs) = (false, false, false);
        for entry in entries {
            let Ok(entry) = entry else { return true };
            seen += 1;
            if seen > max {
                return true;
            }
            let name = entry.file_name();
            let Ok(kind) = entry.file_type() else { return true };
            if name == ".git" {
                if is_root {
                    continue;
                }
                return true;
            }
            match name.to_str() {
                Some("HEAD") if kind.is_file() => head = true,
                Some("objects") if kind.is_dir() => objects = true,
                Some("refs") if kind.is_dir() => refs = true,
                _ => {}
            }
            if kind.is_dir() {
                stack.push((entry.path(), false));
            }
        }
        if head && objects && refs && !is_root {
            return true;
        }
    }
    false
}

/// Commits reachable from `target`'s HEAD and from no other ref: other
/// branches, tags, remotes, the stash (every entry) and the other worktrees'
/// HEADs (A36.1). The kept branch is a ref, so its own commits do not count.
async fn unreachable_commits(target: &Path, entries: &[git::GitWorktreeEntry]) -> std::result::Result<u32, git::GitError> {
    let Some(head) = git::resolve_commit(target, "HEAD").await? else { return Ok(0) };
    let mut others = git::ref_tips(target).await?;
    others.extend(git::stash_tips(target).await?);
    for e in entries {
        if canonical_lexical(&e.path) != target {
            if let Some(h) = e.head.as_ref().filter(|h| !h.is_empty()) {
                others.push(h.clone());
            }
        }
    }
    others.sort();
    others.dedup();
    git::count_only_from(target, &head, &others).await
}

/// After `git worktree remove` failed (A36.4–A36.6): when git removed the
/// directory but not its admin entry, that one entry is cleaned (target
/// only); otherwise Gilde's lock is put back exactly as it was.
async fn restore_after_failed_remove(repo: &Path, target: &Path, lock: LockState) {
    if std::fs::symlink_metadata(target).is_err() {
        if let Err(err) = git::prune_target_admin(repo, target) {
            tracing::warn!(target: "worktree", kind = ?err.kind(), "could not clean the removed worktree's admin entry");
        }
        return;
    }
    if lock == LockState::Gilde {
        if let Err(err) = git::set_worktree_lock(target, true, git::LOCK_REASON).await {
            tracing::warn!(target: "worktree", code = err.to_ipc().code.as_str(), "could not restore the worktree lock");
        }
    }
}

/// A git path as the wire shows it (sanitized; identical for ordinary names).
fn display_rel(p: &str) -> String {
    crate::render::sanitize::sanitize_display_text(p, crate::contract::display::TextCap::Bytes(4096)).text
}

fn sanitize_parts(parts: &[String]) -> Vec<String> {
    parts
        .iter()
        .map(|p| crate::render::sanitize::sanitize_display_text(p, crate::contract::display::TextCap::Chars(120)).text)
        .filter(|p| !p.is_empty())
        .take(MAX_PARTS)
        .collect()
}

/// Lines of an untracked file (binary or over the caps → 0). Never follows a
/// symlink out of the worktree.
fn line_count(path: &Path, budget: &mut u64) -> u32 {
    use std::io::Read as _;
    let Ok(meta) = std::fs::symlink_metadata(path) else { return 0 };
    if !meta.is_file() || meta.len() > UNTRACKED_READ_CAP || meta.len() > *budget {
        return 0;
    }
    *budget -= meta.len();
    let mut buf = Vec::with_capacity(meta.len() as usize);
    if std::fs::File::open(path).and_then(|f| f.take(UNTRACKED_READ_CAP).read_to_end(&mut buf)).is_err() {
        return 0;
    }
    if buf[..buf.len().min(8000)].contains(&0) {
        return 0;
    }
    let n = memchr::memchr_iter(b'\n', &buf).count();
    let last = usize::from(!buf.is_empty() && !buf.ends_with(b"\n"));
    u32::try_from(n + last).unwrap_or(u32::MAX)
}

#[async_trait::async_trait]
impl GitApi for GitService {
    async fn changes(&self, req: &ChangesGetReq) -> std::result::Result<ChangesView, IpcError> {
        let (place, view) = self.place(&req.uid)?;
        if !place.git {
            return Err(IpcError::unsupported("notGit"));
        }
        let base = self.base_of(&place).await?;
        let rows = self.rows(&place, &view, &base).await?;
        let loc = &place.location;

        let (changes_base, commits) = match &base.branch {
            Some(b) => {
                let newer = git::count(loc, &format!("{}..refs/heads/{}", b.merge_base, b.name)).await.map_err(git_err)?;
                let short = git::short_sha(loc, &b.merge_base).await.map_err(git_err)?;
                let log = git::log_range(loc, &format!("{}..HEAD", b.merge_base), MAX_COMMIT_ROWS).await.map_err(git_err)?;
                let commits = log
                    .into_iter()
                    .map(|c| CommitRow {
                        commit_short: c.short,
                        subject: crate::render::sanitize::sanitize_display_text(&c.subject, crate::contract::display::TextCap::Chars(200)).text,
                        at: c.at.saturating_mul(1000),
                    })
                    .collect();
                (Some(ChangesBase { branch: crate::render::sanitize::sanitize_display_text(&b.name, crate::contract::display::TextCap::Chars(255)).text, commit_short: short, newer_on_main: newer }), commits)
            }
            None => (None, Vec::new()),
        };

        let others = sessions_here(&view, &place, &self.home).filter(|s| s.uid != place.uid && is_live(s)).count() as u32;
        let shared_folder = (others > 0).then_some(SharedFolder { other_live_sessions: others });

        let owner = place.owner_alive;
        let disabled_owner = || ActionState::Disabled { reason: REASON_OWNER_WRITING.to_string() };
        // A37.1: Commit only when something is uncommitted (the rows also show
        // what the branch already committed against the merge base).
        let commit = if owner {
            disabled_owner()
        } else if !rows.iter().any(|r| r.uncommitted) {
            ActionState::Hidden
        } else {
            ActionState::Enabled
        };
        let merge = match &base.branch {
            Some(b) if place.worktree => {
                if owner {
                    disabled_owner()
                } else if git::count(loc, &format!("refs/heads/{}..HEAD", b.name)).await.map_err(git_err)? == 0 {
                    ActionState::Hidden
                } else {
                    ActionState::Enabled
                }
            }
            _ => ActionState::Hidden,
        };
        let discard = if !place.worktree {
            ActionState::Hidden
        } else if owner {
            disabled_owner()
        } else if self.live_at_or_under(&view, &place) {
            ActionState::Disabled { reason: REASON_STOP_TO_DISCARD.to_string() }
        } else {
            ActionState::Enabled
        };

        Ok(ChangesView {
            location: place.location_id.clone(),
            base: changes_base,
            files: rows.into_iter().map(|r| r.row).collect(),
            commits,
            shared_folder,
            actions: ChangesActions { commit, merge, discard },
        })
    }

    async fn commit_preview(&self, req: &GitCommitPreviewReq) -> std::result::Result<GitCommitPreviewRes, IpcError> {
        let (place, view) = self.place(&req.uid)?;
        if !place.git {
            return Err(IpcError::unsupported("notGit"));
        }
        if place.owner_alive {
            return Err(IpcError::refused("ownerAlive"));
        }
        let base = self.base_of(&place).await?;
        let rows = self.rows(&place, &view, &base).await?;
        let mut chosen: Vec<&Row> = Vec::with_capacity(req.paths.len());
        for (i, p) in req.paths.iter().enumerate() {
            match rows.iter().find(|r| &r.row.path_rel == p) {
                // A37.1: a row the branch already committed (a committed
                // deletion above all) is listed by the Changes view but is
                // not offered: it is never passed to git as a pathspec.
                Some(r) if r.uncommitted => chosen.push(r),
                Some(_) => {}
                None => return Err(IpcError::invalid_argument(&format!("paths[{i}]"), "path")),
            }
        }
        chosen.sort_by(|a, b| a.real.cmp(&b.real));
        let name = git::config_get(&place.location, "user.name").await.map_err(git_err)?.unwrap_or_default();
        let email = git::config_get(&place.location, "user.email").await.map_err(git_err)?.unwrap_or_default();
        let states: Vec<String> = chosen.iter().map(|r| Self::row_state(&place, r)).collect();
        let pending = CommitPending {
            paths: chosen.iter().map(|r| r.real.clone()).collect(),
            rels: chosen.iter().map(|r| r.row.path_rel.clone()).collect(),
            digest: commit_digest(&Self::head_state(&place).await?, &states),
        };
        let token = self.commits.issue(&place.uid, pending);
        Ok(GitCommitPreviewRes {
            files: chosen.iter().map(|r| r.row.clone()).collect(),
            author: GitAuthor {
                name: crate::render::sanitize::sanitize_display_text(&name, crate::contract::display::TextCap::Chars(255)).text,
                email: crate::render::sanitize::sanitize_display_text(&email, crate::contract::display::TextCap::Chars(255)).text,
            },
            confirm_token: token,
        })
    }

    async fn commit(&self, req: &GitCommitReq) -> std::result::Result<GitCommitRes, IpcError> {
        let (place, view) = self.place(&req.uid)?;
        if place.owner_alive {
            return Err(IpcError::refused("ownerAlive"));
        }
        // A37.4: a path outside the preview is refused without an `await`
        // (the token stays); otherwise the token is taken before any.
        let pending = self.commits.take_if(&req.confirm_token, &place.uid, |p| {
            match req.paths.iter().position(|path| !p.rels.contains(path)) {
                Some(i) => Err(IpcError::invalid_argument(&format!("paths[{i}]"), "path")),
                None => Ok(()),
            }
        })?;
        let base = self.base_of(&place).await?;
        let rows = self.rows(&place, &view, &base).await?;
        let mut real = Vec::with_capacity(req.paths.len());
        for p in &req.paths {
            match rows.iter().find(|r| &r.row.path_rel == p).filter(|r| r.uncommitted && pending.paths.contains(&r.real)) {
                Some(r) => real.push(r.real.clone()),
                // Previewed, but no longer uncommitted (or gone): the state changed.
                None => return Err(IpcError::conflict("changed")),
            }
        }
        let states: Vec<String> = pending
            .paths
            .iter()
            .map(|p| match rows.iter().find(|r| &r.real == p) {
                Some(r) => Self::row_state(&place, r),
                None => format!("{p}|gone"),
            })
            .collect();
        if commit_digest(&Self::head_state(&place).await?, &states) != pending.digest {
            return Err(IpcError::conflict("changed"));
        }
        git::add_paths(&place.location, &real).await.map_err(git_err)?;
        let sha = git::commit_paths(&place.location, &real, &req.message).await.map_err(git_err)?;
        tracing::info!(target: "worktree", files = real.len(), "commit made through the Changes view");
        Ok(GitCommitRes { commit: sha })
    }

    async fn merge_preview(&self, req: &GitMergePreviewReq) -> std::result::Result<GitMergePreviewRes, IpcError> {
        let (place, _) = self.place(&req.uid)?;
        Self::require_worktree(&place)?;
        if place.owner_alive {
            return Err(IpcError::refused("ownerAlive"));
        }
        let (effect, d, _, theirs) = self.merge_state(&place).await?;
        Self::merge_gates(&place, &effect, theirs.as_deref()).await?;
        let token = self.merges.issue(&place.uid, MergePending { digest: d });
        Ok(GitMergePreviewRes { effect, confirm_token: token })
    }

    async fn merge(&self, req: &GitMergeReq) -> std::result::Result<GitMergeRes, IpcError> {
        let (place, _) = self.place(&req.uid)?;
        Self::require_worktree(&place)?;
        if place.owner_alive {
            return Err(IpcError::refused("ownerAlive"));
        }
        let pending = self.merges.take_if(&req.confirm_token, &place.uid, |_| Ok(()))?;
        let (effect, d, branch, theirs) = self.merge_state(&place).await?;
        if d != pending.digest {
            return Err(IpcError::conflict("changed"));
        }
        Self::merge_gates(&place, &effect, theirs.as_deref()).await?;
        let main = &place.project_root;
        match (effect.kind, theirs) {
            (MergeKind::UpToDate, _) | (_, None) => {}
            (MergeKind::FastForward, Some(theirs)) => git::merge_fast_forward(main, &theirs).await.map_err(git_err)?,
            (MergeKind::MergeCommit, Some(theirs)) => {
                let wt_branch = git::head_branch(&place.location).await.map_err(git_err)?;
                let message = match wt_branch {
                    Some(b) => format!("Merge branch '{b}' into {branch}"),
                    None => format!("Merge commit '{}' into {branch}", &theirs[..theirs.len().min(12)]),
                };
                if let Err(e) = git::merge_commit(main, &theirs, &message).await {
                    // Never leave the user's checkout half-merged.
                    let _ = git::merge_abort(main).await;
                    return Err(match &e {
                        git::GitError::Failed { stderr, .. } if stderr.contains("CONFLICT") => IpcError::refused("conflicts"),
                        _ => e.to_ipc(),
                    });
                }
            }
        }
        let head = git::resolve_commit(main, "HEAD").await.map_err(git_err)?.unwrap_or_default();
        tracing::info!(target: "worktree", kind = ?effect.kind, "merge made through the Changes view");
        Ok(GitMergeRes { merge_commit: head })
    }

    async fn discard_preview(&self, req: &WorktreeDiscardPreviewReq) -> std::result::Result<WorktreeDiscardPreviewRes, IpcError> {
        let (place, view) = self.place(&req.uid)?;
        let state = self.discard_state(&place, &view).await?;
        let pending = DiscardPending { digest: state.digest, loss_ids: state.losses.iter().map(|l| l.id.clone()).collect() };
        let token = self.discards.issue(&place.uid, pending);
        Ok(WorktreeDiscardPreviewRes { losses: state.losses, branch_kept: true, confirm_token: token })
    }

    /// The branch is always kept: the app never deletes one (DESIGN §7-7,
    /// A17.5 — the request has no such field). git removes the worktree —
    /// with its one `--force` only when every listed loss was acknowledged —
    /// and any failure leaves the tree to git's own refusal and Gilde's lock
    /// as it was (A36.4, A36.6): there is no fallback that deletes a
    /// directory, and no repo-wide prune (A36.5).
    async fn discard(&self, req: &WorktreeDiscardReq) -> std::result::Result<(), IpcError> {
        // A37.4: taken before the first `await`. A loss the preview listed
        // and the request did not acknowledge is refused first, without an
        // `await`, and keeps the token.
        let pending = self.discards.take_if(&req.confirm_token, &req.uid, |p| {
            match p.loss_ids.iter().find(|id| !req.accepted_losses.contains(id)) {
                Some(id) => {
                    tracing::info!(target: "worktree", loss = %id, "discard refused: a loss was not acknowledged");
                    Err(IpcError::refused("unacknowledgedLoss"))
                }
                None => Ok(()),
            }
        })?;
        let (place, view) = self.place(&req.uid)?;
        let state = self.discard_state(&place, &view).await?;
        if state.digest != pending.digest {
            return Err(IpcError::conflict("changed"));
        }
        if state.losses.iter().any(|l| !req.accepted_losses.contains(&l.id)) {
            return Err(IpcError::refused("unacknowledgedLoss"));
        }

        let repo = &place.project_root;
        let loc = &place.location;
        if state.lock == LockState::Gilde {
            git::set_worktree_lock(loc, false, "").await.map_err(git_err)?;
        }
        let removed = if state.losses.is_empty() {
            git::remove_worktree(repo, loc).await
        } else {
            git::remove_worktree_acknowledged(repo, loc).await
        };
        if let Err(e) = removed {
            restore_after_failed_remove(repo, loc, state.lock).await;
            return Err(e.to_ipc());
        }
        tracing::info!(target: "worktree", losses = state.losses.len(), "worktree discarded; branch kept");
        Ok(())
    }

    async fn create_worktree(&self, project: &ProjectId, slug: &str, base: Option<&str>) -> std::result::Result<LocationFacts, IpcError> {
        if !naming::is_worktree_slug(slug) {
            return Err(IpcError::invalid_argument("location.slug", if slug.is_empty() { "required" } else { "pattern" }));
        }
        let base = base.filter(|b| !b.is_empty());
        if let Some(b) = base {
            if !crate::contract::ipc::sessions::is_base_ref(b) {
                return Err(IpcError::invalid_argument("location.base", "pattern"));
            }
        }
        let view = self.src.view();
        let proj = view.projects.iter().find(|p| &p.id == project).ok_or_else(|| IpcError::not_found("project"))?;
        if proj.kind != ProjectKind::Git {
            return Err(IpcError::refused("worktreeGate"));
        }
        let root = std::fs::canonicalize(proj.root.as_str()).map_err(|_| IpcError::not_found("project"))?;
        let gate = |detail: Option<String>| {
            let e = IpcError::refused("worktreeGate");
            match detail {
                Some(d) => e.with_detail(d),
                None => e,
            }
        };
        let id = crate::discovery::location::identify(&root, &self.home).ok_or_else(|| gate(None))?;
        if !id.git || id.worktree || &id.project_id != project {
            return Err(gate(None));
        }
        let base_sha = match base {
            Some(b) => git::resolve_commit(&root, b).await.map_err(gate_err)?.ok_or_else(|| gate(None))?,
            None => git::resolve_commit(&root, "HEAD").await.map_err(gate_err)?.ok_or_else(|| gate(None))?,
        };
        let layout = self.layout();
        let path = naming::worktree_path(layout, &root, slug).ok_or_else(|| gate(None))?;
        if std::fs::symlink_metadata(&path).is_ok() {
            return Err(IpcError::conflict("worktreeExists"));
        }
        let branch = naming::branch_name(slug);
        if git::branch_present(&root, &branch).await.map_err(gate_err)? {
            return Err(IpcError::conflict("worktreeExists"));
        }
        if layout == naming::WorktreeLayout::InRepo {
            // Before the directory exists, so it never shows as untracked (A6).
            let common = git::common_dir(&root).await.map_err(gate_err)?;
            git::ensure_excluded(&common, naming::IN_REPO_EXCLUDE).map_err(|e| IpcError::from(&GildeError::Io(e)))?;
        }
        // Detached first, then the branch (only if it does not exist), then
        // HEAD onto it: a failure at any step leaves no stray `gilde/` branch
        // that would block the slug (a failed `worktree add -b` keeps the
        // branch it created), and the app never deletes a branch to clean up.
        git::add_worktree_detached(&root, &path, &base_sha).await.map_err(gate_err)?;
        let attached = match git::create_branch_at(&path, &branch, &base_sha).await {
            Ok(()) => git::attach_head(&path, &branch).await.map_err(gate_err),
            Err(git::GitError::Failed { .. }) if git::branch_present(&root, &branch).await.unwrap_or(false) => {
                Err(IpcError::conflict("worktreeExists"))
            }
            Err(e) => Err(gate_err(e)),
        };
        if let Err(e) = attached {
            // The fresh worktree is pristine: a plain remove (never `--force`).
            if let Err(err) = git::remove_worktree(&root, &path).await {
                tracing::warn!(target: "worktree", code = err.to_ipc().code.as_str(), "could not remove a half-made worktree");
            }
            return Err(e);
        }
        // Locked while a session may live in it (DESIGN §7-3).
        if let Err(err) = git::set_worktree_lock(&path, true, git::LOCK_REASON).await {
            tracing::warn!(target: "worktree", code = err.to_ipc().code.as_str(), "worktree lock failed");
        }
        let facts = crate::discovery::location::identify(&path, &self.home)
            .and_then(|i| i.facts())
            .ok_or_else(|| IpcError::internal("worktree-identity"))?;
        self.src.location_seen(facts.clone());
        Ok(facts)
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
