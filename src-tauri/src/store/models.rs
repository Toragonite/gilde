//! Row structs for every `state.db` table, plus the small enums that are stored
//! as TEXT and the `argv_signature` helper used by the PID registry.
//!
//! Every struct mirrors the column list of DESIGN §8-2 / §10-4 one for one.
//! Serde representations are camelCase because these rows cross the IPC border
//! (process monitor, dispatch board, recovery sheet).

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::Row;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A TEXT column held a value this build does not know.
#[derive(Debug, thiserror::Error)]
#[error("unknown `{column}` value in state.db: {value:?}")]
pub struct UnknownVariant {
    pub column: &'static str,
    pub value: String,
}

/// Rows that can be built from a `rusqlite::Row`.
///
/// Implementations read by column *name*, never by index, so that adding a
/// column in a later forward migration cannot silently shift a field.
pub trait FromRow: Sized {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self>;
}

macro_rules! text_enum {
    (
        $(#[$meta:meta])*
        $name:ident, $column:literal {
            $( $variant:ident => $db:literal ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "camelCase")]
        pub enum $name {
            $( $variant ),+
        }

        impl $name {
            /// The exact string stored in SQLite.
            pub const fn as_db_str(self) -> &'static str {
                match self {
                    $( $name::$variant => $db ),+
                }
            }

            pub fn from_db_str(s: &str) -> Option<Self> {
                match s {
                    $( $db => Some($name::$variant), )+
                    _ => None,
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_db_str())
            }
        }

        impl ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
                Ok(ToSqlOutput::from(self.as_db_str()))
            }
        }

        impl FromSql for $name {
            fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
                let s = value.as_str()?;
                $name::from_db_str(s).ok_or_else(|| {
                    FromSqlError::Other(Box::new(UnknownVariant {
                        column: $column,
                        value: s.to_string(),
                    }))
                })
            }
        }
    };
}

text_enum! {
    /// `proc_registry.state` (DESIGN §8-2).
    ProcState, "proc_registry.state" {
        Planned => "planned",
        Running => "running",
        Exited => "exited",
        Killed => "killed",
        KillIncomplete => "kill_incomplete",
        Lost => "lost",
        Adopted => "adopted",
    }
}

impl ProcState {
    /// States in which the process may still be alive.
    pub const LIVE: [ProcState; 3] = [ProcState::Planned, ProcState::Running, ProcState::Adopted];
    /// States in which the row is finished and eligible for retention pruning.
    pub const TERMINAL: [ProcState; 4] = [
        ProcState::Exited,
        ProcState::Killed,
        ProcState::KillIncomplete,
        ProcState::Lost,
    ];

    pub const fn is_live(self) -> bool {
        matches!(
            self,
            ProcState::Planned | ProcState::Running | ProcState::Adopted
        )
    }

    pub const fn is_terminal(self) -> bool {
        !self.is_live()
    }
}

text_enum! {
    /// `proc_registry.kind` (DESIGN §8-2).
    ProcKind, "proc_registry.kind" {
        Pty => "pty",
        Headless => "headless",
        Sidecar => "sidecar",
        Gateway => "gateway",
        Probe => "probe",
        Git => "git",
    }
}

text_enum! {
    /// `worktree.origin`.
    WorktreeOrigin, "worktree.origin" {
        Created => "created",
        Adopted => "adopted",
    }
}

text_enum! {
    /// `worktree_alias.kind`.
    AliasKind, "worktree_alias.kind" {
        Symlink => "symlink",
        Adopted => "adopted",
    }
}

text_enum! {
    /// `account.source`.
    AccountSource, "account.source" {
        Cco => "cco",
        Manual => "manual",
        Ambient => "ambient",
    }
}

text_enum! {
    /// `conversation.resumability` (M2 slot, DESIGN §11-2).
    Resumability, "conversation.resumability" {
        NativeResume => "native_resume",
        TranscriptOnly => "transcript_only",
        HandoffOnly => "handoff_only",
    }
}

/// `sha256(cmd || '\0' || args.join('\0'))` — DESIGN §8-2 / §8-4 fence 2.
///
/// `argv[0]` is the command; every execution uses an absolute binary path, so
/// this is an exact comparison rather than a heuristic.
pub fn argv_signature<S: AsRef<str>>(argv: &[S]) -> String {
    let mut hasher = Sha256::new();
    for (i, part) in argv.iter().enumerate() {
        if i > 0 {
            hasher.update(b"\0");
        }
        hasher.update(part.as_ref().as_bytes());
    }
    hex::encode(hasher.finalize())
}

// ---------------------------------------------------------------------------
// app_instance
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInstanceRow {
    pub id: String,
    pub pid: i64,
    pub boot_id: String,
    pub version: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
}

impl FromRow for AppInstanceRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            pid: row.get("pid")?,
            boot_id: row.get("boot_id")?,
            version: row.get("version")?,
            started_at: row.get("started_at")?,
            ended_at: row.get("ended_at")?,
        })
    }
}

// ---------------------------------------------------------------------------
// proc_registry
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcRow {
    pub spawn_id: String,
    pub app_instance: String,
    pub generation: i64,
    pub kind: ProcKind,
    pub vendor: Option<String>,
    pub session_id: Option<String>,
    pub run_id: Option<String>,
    pub state: ProcState,
    pub pid: Option<i64>,
    pub pgid: Option<i64>,
    pub proc_start_ts: Option<i64>,
    /// JSON array, already masked (DESIGN §8-6).
    pub argv: String,
    pub argv_sig: String,
    pub cwd: String,
    pub env_marker: String,
    pub config_dir: Option<String>,
    pub worktree_id: Option<String>,
    pub keep_alive: bool,
    pub planned_at: i64,
    pub started_at: Option<i64>,
    pub exited_at: Option<i64>,
    pub exit_code: Option<i64>,
    pub exit_signal: Option<i64>,
    pub note: Option<String>,
}

impl ProcRow {
    /// The stored `argv` column parsed back into a vector.
    ///
    /// Returns an empty vector when the column is not a JSON array of strings;
    /// a malformed row must never take down a monitor refresh.
    pub fn argv_vec(&self) -> Vec<String> {
        serde_json::from_str(&self.argv).unwrap_or_default()
    }
}

impl FromRow for ProcRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            spawn_id: row.get("spawn_id")?,
            app_instance: row.get("app_instance")?,
            generation: row.get("generation")?,
            kind: row.get("kind")?,
            vendor: row.get("vendor")?,
            session_id: row.get("session_id")?,
            run_id: row.get("run_id")?,
            state: row.get("state")?,
            pid: row.get("pid")?,
            pgid: row.get("pgid")?,
            proc_start_ts: row.get("proc_start_ts")?,
            argv: row.get("argv")?,
            argv_sig: row.get("argv_sig")?,
            cwd: row.get("cwd")?,
            env_marker: row.get("env_marker")?,
            config_dir: row.get("config_dir")?,
            worktree_id: row.get("worktree_id")?,
            keep_alive: row.get::<_, i64>("keep_alive")? != 0,
            planned_at: row.get("planned_at")?,
            started_at: row.get("started_at")?,
            exited_at: row.get("exited_at")?,
            exit_code: row.get("exit_code")?,
            exit_signal: row.get("exit_signal")?,
            note: row.get("note")?,
        })
    }
}

/// Everything `SpawnGuard::plan_spawn` needs to write the write-before-spawn row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewPlannedProc {
    pub spawn_id: String,
    pub app_instance: String,
    pub generation: i64,
    pub kind: ProcKind,
    pub vendor: Option<String>,
    pub session_id: Option<String>,
    pub run_id: Option<String>,
    /// argv[0] is the absolute command path; already masked.
    pub argv: Vec<String>,
    pub cwd: String,
    pub config_dir: Option<String>,
    pub worktree_id: Option<String>,
    pub keep_alive: bool,
    pub planned_at: i64,
    pub note: Option<String>,
}

impl NewPlannedProc {
    pub fn new(
        spawn_id: impl Into<String>,
        app_instance: impl Into<String>,
        kind: ProcKind,
        argv: Vec<String>,
        cwd: impl Into<String>,
        planned_at: i64,
    ) -> Self {
        Self {
            spawn_id: spawn_id.into(),
            app_instance: app_instance.into(),
            generation: 0,
            kind,
            vendor: None,
            session_id: None,
            run_id: None,
            argv,
            cwd: cwd.into(),
            config_dir: None,
            worktree_id: None,
            keep_alive: false,
            planned_at,
            note: None,
        }
    }
}

// ---------------------------------------------------------------------------
// project / worktree
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRow {
    pub id: String,
    pub path: String,
    pub canonical_path: String,
    pub repo_slug: String,
    pub name: String,
    pub color: Option<String>,
    pub default_base_ref: String,
    pub added_at: i64,
    pub last_opened_at: Option<i64>,
}

impl FromRow for ProjectRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            path: row.get("path")?,
            canonical_path: row.get("canonical_path")?,
            repo_slug: row.get("repo_slug")?,
            name: row.get("name")?,
            color: row.get("color")?,
            default_base_ref: row.get("default_base_ref")?,
            added_at: row.get("added_at")?,
            last_opened_at: row.get("last_opened_at")?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeRow {
    pub id: String,
    pub project_id: String,
    pub slug: String,
    pub branch: String,
    pub path: String,
    pub base_sha: Option<String>,
    pub origin: WorktreeOrigin,
    pub created_at: i64,
    pub removed_at: Option<i64>,
    pub locked: bool,
    pub verified_at: Option<i64>,
    pub post_create_cmd: Option<String>,
    pub cached_dirty: i64,
    pub cached_added: i64,
    pub cached_removed: i64,
    pub cached_ahead: i64,
    pub cached_behind: i64,
}

impl FromRow for WorktreeRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            project_id: row.get("project_id")?,
            slug: row.get("slug")?,
            branch: row.get("branch")?,
            path: row.get("path")?,
            base_sha: row.get("base_sha")?,
            origin: row.get("origin")?,
            created_at: row.get("created_at")?,
            removed_at: row.get("removed_at")?,
            locked: row.get::<_, i64>("locked")? != 0,
            verified_at: row.get("verified_at")?,
            post_create_cmd: row.get("post_create_cmd")?,
            cached_dirty: row.get("cached_dirty")?,
            cached_added: row.get("cached_added")?,
            cached_removed: row.get("cached_removed")?,
            cached_ahead: row.get("cached_ahead")?,
            cached_behind: row.get("cached_behind")?,
        })
    }
}

/// The five cached git counters written by `worktree_stats` (DESIGN §7-5).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeStatsCache {
    pub dirty: i64,
    pub added: i64,
    pub removed: i64,
    pub ahead: i64,
    pub behind: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeAliasRow {
    pub id: String,
    pub worktree_id: String,
    pub provider: String,
    pub alias_path: String,
    pub target_path: String,
    pub kind: AliasKind,
    pub created_at: i64,
}

impl FromRow for WorktreeAliasRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            worktree_id: row.get("worktree_id")?,
            provider: row.get("provider")?,
            alias_path: row.get("alias_path")?,
            target_path: row.get("target_path")?,
            kind: row.get("kind")?,
            created_at: row.get("created_at")?,
        })
    }
}

// ---------------------------------------------------------------------------
// pty_session
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PtySessionRow {
    pub id: String,
    pub spawn_id: Option<String>,
    pub project_id: Option<String>,
    pub worktree_id: Option<String>,
    pub agent_kind: String,
    pub account_id: Option<String>,
    pub title: String,
    pub cwd: String,
    pub cols: i64,
    pub rows: i64,
    /// JSON array, already masked.
    pub argv: String,
    pub opened_at: i64,
    pub closed_at: Option<i64>,
    pub exit_code: Option<i64>,
    pub exit_signal: Option<i64>,
    pub last_io_at: Option<i64>,
    pub last_status: Option<String>,
    pub detached: bool,
}

impl FromRow for PtySessionRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            spawn_id: row.get("spawn_id")?,
            project_id: row.get("project_id")?,
            worktree_id: row.get("worktree_id")?,
            agent_kind: row.get("agent_kind")?,
            account_id: row.get("account_id")?,
            title: row.get("title")?,
            cwd: row.get("cwd")?,
            cols: row.get("cols")?,
            rows: row.get("rows")?,
            argv: row.get("argv")?,
            opened_at: row.get("opened_at")?,
            closed_at: row.get("closed_at")?,
            exit_code: row.get("exit_code")?,
            exit_signal: row.get("exit_signal")?,
            last_io_at: row.get("last_io_at")?,
            last_status: row.get("last_status")?,
            detached: row.get::<_, i64>("detached")? != 0,
        })
    }
}

// ---------------------------------------------------------------------------
// dispatch
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DispatchTaskRow {
    pub id: String,
    pub project_id: Option<String>,
    pub title: String,
    pub prompt: String,
    pub task_slug: String,
    pub base_ref: Option<String>,
    pub worktree_strategy: String,
    pub permission_mode: Option<String>,
    pub timeout_ms: Option<i64>,
    pub created_at: i64,
    pub finished_at: Option<i64>,
    pub status: String,
}

impl FromRow for DispatchTaskRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            project_id: row.get("project_id")?,
            title: row.get("title")?,
            prompt: row.get("prompt")?,
            task_slug: row.get("task_slug")?,
            base_ref: row.get("base_ref")?,
            worktree_strategy: row.get("worktree_strategy")?,
            permission_mode: row.get("permission_mode")?,
            timeout_ms: row.get("timeout_ms")?,
            created_at: row.get("created_at")?,
            finished_at: row.get("finished_at")?,
            status: row.get("status")?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DispatchRunRow {
    pub id: String,
    pub task_id: String,
    pub vendor: String,
    pub model_requested: Option<String>,
    pub model_actual: Option<String>,
    pub account_id: Option<String>,
    pub worktree_id: Option<String>,
    pub spawn_id: Option<String>,
    /// Vendor/mesh run id (distinct from our primary key).
    pub run_id: Option<String>,
    pub conversation_id: Option<String>,
    pub status: String,
    pub note: Option<String>,
    pub route_note: Option<String>,
    /// Result text, or a `runs/<run_id>.txt` pointer once it exceeds 4 MiB.
    pub text: Option<String>,
    pub usage_json: Option<String>,
    pub cost_usd: Option<f64>,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub error: Option<String>,
}

impl FromRow for DispatchRunRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            task_id: row.get("task_id")?,
            vendor: row.get("vendor")?,
            model_requested: row.get("model_requested")?,
            model_actual: row.get("model_actual")?,
            account_id: row.get("account_id")?,
            worktree_id: row.get("worktree_id")?,
            spawn_id: row.get("spawn_id")?,
            run_id: row.get("run_id")?,
            conversation_id: row.get("conversation_id")?,
            status: row.get("status")?,
            note: row.get("note")?,
            route_note: row.get("route_note")?,
            text: row.get("text")?,
            usage_json: row.get("usage_json")?,
            cost_usd: row.get("cost_usd")?,
            started_at: row.get("started_at")?,
            finished_at: row.get("finished_at")?,
            error: row.get("error")?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunEventRow {
    pub id: i64,
    pub run_id: String,
    pub at: i64,
    pub kind: String,
    pub payload: Option<String>,
}

impl FromRow for RunEventRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            run_id: row.get("run_id")?,
            at: row.get("at")?,
            kind: row.get("kind")?,
            payload: row.get("payload")?,
        })
    }
}

// ---------------------------------------------------------------------------
// accounts / binaries / logs / samples
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountRow {
    pub id: String,
    pub vendor: String,
    pub label: String,
    pub config_dir: Option<String>,
    pub model_hint: Option<String>,
    pub source: AccountSource,
    pub enabled: bool,
    pub is_default: bool,
    pub imported_at: i64,
    pub last_seen_at: Option<i64>,
    pub last_quota_json: Option<String>,
    pub last_quota_at: Option<i64>,
}

impl FromRow for AccountRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            vendor: row.get("vendor")?,
            label: row.get("label")?,
            config_dir: row.get("config_dir")?,
            model_hint: row.get("model_hint")?,
            source: row.get("source")?,
            enabled: row.get::<_, i64>("enabled")? != 0,
            is_default: row.get::<_, i64>("is_default")? != 0,
            imported_at: row.get("imported_at")?,
            last_seen_at: row.get("last_seen_at")?,
            last_quota_json: row.get("last_quota_json")?,
            last_quota_at: row.get("last_quota_at")?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentBinaryRow {
    pub kind: String,
    pub abs_path: Option<String>,
    pub version: Option<String>,
    pub detected_at: i64,
    pub path_fingerprint: Option<String>,
    pub verified_range: Option<String>,
    pub ok: bool,
}

impl FromRow for AgentBinaryRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            kind: row.get("kind")?,
            abs_path: row.get("abs_path")?,
            version: row.get("version")?,
            detected_at: row.get("detected_at")?,
            path_fingerprint: row.get("path_fingerprint")?,
            verified_range: row.get("verified_range")?,
            ok: row.get::<_, i64>("ok")? != 0,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventLogRow {
    pub id: i64,
    pub at: i64,
    pub category: String,
    pub level: String,
    pub subject_id: Option<String>,
    pub message: String,
    pub detail_json: Option<String>,
}

impl FromRow for EventLogRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            at: row.get("at")?,
            category: row.get("category")?,
            level: row.get("level")?,
            subject_id: row.get("subject_id")?,
            message: row.get("message")?,
            detail_json: row.get("detail_json")?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcSampleRow {
    pub at: i64,
    pub scope: String,
    pub scope_id: Option<String>,
    pub rss_bytes: i64,
    pub cpu_pct: f64,
}

impl FromRow for ProcSampleRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            at: row.get("at")?,
            scope: row.get("scope")?,
            scope_id: row.get("scope_id")?,
            rss_bytes: row.get("rss_bytes")?,
            cpu_pct: row.get("cpu_pct")?,
        })
    }
}

// ---------------------------------------------------------------------------
// M2 slots — created empty in M1 (DESIGN §11-2)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationRow {
    pub id: String,
    pub source: String,
    pub source_session_id: String,
    pub source_locator: String,
    pub workspace_path: Option<String>,
    pub worktree_id: Option<String>,
    pub title: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub model: Option<String>,
    pub mode: Option<String>,
    pub message_count: i64,
    pub resumability: Resumability,
    pub cost_cents: Option<i64>,
    pub imported_at: i64,
    pub schema_variant: String,
}

impl FromRow for ConversationRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            source: row.get("source")?,
            source_session_id: row.get("source_session_id")?,
            source_locator: row.get("source_locator")?,
            workspace_path: row.get("workspace_path")?,
            worktree_id: row.get("worktree_id")?,
            title: row.get("title")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
            model: row.get("model")?,
            mode: row.get("mode")?,
            message_count: row.get("message_count")?,
            resumability: row.get("resumability")?,
            cost_cents: row.get("cost_cents")?,
            imported_at: row.get("imported_at")?,
            schema_variant: row.get("schema_variant")?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageRow {
    pub id: String,
    pub conversation_id: String,
    pub seq: i64,
    /// Claude Code `parentUuid` — preserves the branching DAG.
    pub parent_id: Option<String>,
    pub role: String,
    /// JSON array of `Block`.
    pub blocks: String,
    /// JSON array of paths.
    pub files_touched: Option<String>,
    pub model: Option<String>,
    pub usage_json: Option<String>,
    pub timestamp: i64,
    /// Lossless original record.
    pub raw: Option<String>,
}

impl FromRow for MessageRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            conversation_id: row.get("conversation_id")?,
            seq: row.get("seq")?,
            parent_id: row.get("parent_id")?,
            role: row.get("role")?,
            blocks: row.get("blocks")?,
            files_touched: row.get("files_touched")?,
            model: row.get("model")?,
            usage_json: row.get("usage_json")?,
            timestamp: row.get("timestamp")?,
            raw: row.get("raw")?,
        })
    }
}

// ---------------------------------------------------------------------------
// retention
// ---------------------------------------------------------------------------

/// What `dao::run_retention` deleted, for the boot log line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetentionReport {
    pub run_events_deleted: u64,
    pub proc_samples_deleted: u64,
    pub proc_rows_deleted: u64,
    pub event_log_deleted: u64,
}

impl RetentionReport {
    pub fn total(&self) -> u64 {
        self.run_events_deleted
            + self.proc_samples_deleted
            + self.proc_rows_deleted
            + self.event_log_deleted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_enum_roundtrips_through_db_strings() {
        assert_eq!(ProcState::KillIncomplete.as_db_str(), "kill_incomplete");
        assert_eq!(
            ProcState::from_db_str("kill_incomplete"),
            Some(ProcState::KillIncomplete)
        );
        assert_eq!(ProcState::from_db_str("nope"), None);
        assert_eq!(ProcKind::Pty.to_string(), "pty");
        assert_eq!(
            Resumability::from_db_str("native_resume"),
            Some(Resumability::NativeResume)
        );
    }

    #[test]
    fn live_and_terminal_partition_all_states() {
        for st in ProcState::LIVE {
            assert!(st.is_live() && !st.is_terminal());
        }
        for st in ProcState::TERMINAL {
            assert!(st.is_terminal() && !st.is_live());
        }
        // Adopted is live, so LIVE + TERMINAL covers every variant exactly once.
        assert_eq!(ProcState::LIVE.len() + ProcState::TERMINAL.len(), 7);
    }

    #[test]
    fn argv_signature_is_nul_separated_and_order_sensitive() {
        let a = argv_signature(&["/bin/echo", "a", "b"]);
        let b = argv_signature(&["/bin/echo", "b", "a"]);
        assert_ne!(a, b);
        // "a\0b" must not collide with the single argument "a\0b".
        let joined = argv_signature(&["/bin/echo"]);
        assert_ne!(a, joined);
        assert_eq!(a.len(), 64);
        assert_eq!(a, argv_signature(&["/bin/echo".to_string(), "a".into(), "b".into()]));
    }
}
