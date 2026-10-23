//! Forward-only migrations.
//!
//! Rules (DESIGN §10-4):
//!
//! * Every migration runs inside a transaction and is recorded in
//!   `schema_migrations(version, applied_at, checksum)`.
//! * Re-running the set is a no-op.
//! * A checksum mismatch on an already-applied migration aborts boot with a
//!   typed error instead of silently running against a schema we do not
//!   understand.
//! * Nothing is ever edited in place: a change means a new numbered migration.

use rusqlite::{Connection, OptionalExtension};
use sha2::{Digest, Sha256};

use crate::error::{GildeError, Result};

/// One forward step. `sql` is executed verbatim with `execute_batch`.
#[derive(Debug, Clone, Copy)]
pub struct Migration {
    pub version: u32,
    pub name: &'static str,
    pub sql: &'static str,
}

impl Migration {
    /// `sha256` of the SQL text, hex encoded. Recorded on apply and compared on
    /// every later boot.
    pub fn checksum(&self) -> String {
        hex::encode(Sha256::digest(self.sql.as_bytes()))
    }
}

/// Why the schema could not be brought up to date. Boot must stop on these.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SchemaError {
    #[error(
        "migration {version} ({name}) was applied with a different definition \
         (recorded checksum {recorded}, this build has {expected}); \
         the database was written by an incompatible build"
    )]
    ChecksumMismatch {
        version: u32,
        name: &'static str,
        recorded: String,
        expected: String,
    },
    #[error(
        "state.db is at schema version {found} but this build only knows up to {known}; \
         it was written by a newer build"
    )]
    FromTheFuture { found: u32, known: u32 },
}

/// A schema problem is reported to the rest of the app as `GildeError::Db`, but the
/// typed value stays available through [`inspect`] for callers that need to
/// distinguish "corrupt file" from "incompatible build".
impl From<SchemaError> for GildeError {
    fn from(e: SchemaError) -> Self {
        GildeError::Db(e.to_string())
    }
}

/// The migration table itself is bootstrapped outside the numbered set.
const BOOTSTRAP_SQL: &str = "
CREATE TABLE IF NOT EXISTS schema_migrations (
  version    INTEGER PRIMARY KEY,
  applied_at INTEGER NOT NULL,
  checksum   TEXT    NOT NULL,
  name       TEXT    NOT NULL DEFAULT ''
);
";

/// Migration 1 — DESIGN §8-2 (PID registry) + §10-4 (every other table)
/// + §11-2 (the two empty chat-import tables).
///
/// Do not edit. Add migration 2 instead.
const M0001_INITIAL: &str = r#"
-- ---------------------------------------------------------------- §8-2
CREATE TABLE app_instance (
  id           TEXT PRIMARY KEY,          -- uuid v7
  pid          INTEGER NOT NULL,
  boot_id      TEXT NOT NULL,             -- kern.bootsessionuuid
  version      TEXT NOT NULL,
  started_at   INTEGER NOT NULL,
  ended_at     INTEGER                    -- NULL => crashed or still running
);
CREATE INDEX ix_app_instance_open ON app_instance(started_at) WHERE ended_at IS NULL;

CREATE TABLE proc_registry (
  spawn_id      TEXT PRIMARY KEY,         -- uuid v7 (time ordered)
  app_instance  TEXT NOT NULL REFERENCES app_instance(id),
  generation    INTEGER NOT NULL,         -- sidecar generation; host spawns are 0
  kind          TEXT NOT NULL,            -- 'pty'|'headless'|'sidecar'|'gateway'|'probe'|'git'
  vendor        TEXT,                     -- claude|codex|cursor|gemini|NULL
  session_id    TEXT,
  run_id        TEXT,
  state         TEXT NOT NULL,            -- 'planned'|'running'|'exited'|'killed'
                                          -- |'kill_incomplete'|'lost'|'adopted'
  pid           INTEGER,
  pgid          INTEGER,
  proc_start_ts INTEGER,                  -- kernel start time (s) — pid reuse fence
  argv          TEXT NOT NULL,            -- JSON array, masked
  argv_sig      TEXT NOT NULL,            -- sha256(cmd || '\0' || args.join('\0'))
  cwd           TEXT NOT NULL,
  env_marker    TEXT NOT NULL,            -- GILDE_SPAWN_ID value stamped on the child
  config_dir    TEXT,
  worktree_id   TEXT,
  keep_alive    INTEGER NOT NULL DEFAULT 0,
  planned_at    INTEGER NOT NULL,
  started_at    INTEGER,
  exited_at     INTEGER,
  exit_code     INTEGER,
  exit_signal   INTEGER,
  note          TEXT
);
CREATE INDEX ix_proc_state ON proc_registry(state, app_instance);
CREATE INDEX ix_proc_run   ON proc_registry(run_id)      WHERE run_id IS NOT NULL;
CREATE INDEX ix_proc_wt    ON proc_registry(worktree_id) WHERE worktree_id IS NOT NULL;
CREATE INDEX ix_proc_pid   ON proc_registry(pid)         WHERE pid IS NOT NULL;
CREATE INDEX ix_proc_gen   ON proc_registry(generation, state);

-- ---------------------------------------------------------------- §10-4
CREATE TABLE project (
  id               TEXT PRIMARY KEY,
  path             TEXT NOT NULL,
  canonical_path   TEXT NOT NULL,
  repo_slug        TEXT NOT NULL,
  name             TEXT NOT NULL,
  color            TEXT,
  default_base_ref TEXT NOT NULL DEFAULT 'HEAD',
  added_at         INTEGER NOT NULL,
  last_opened_at   INTEGER
);
CREATE UNIQUE INDEX ux_project_canonical ON project(canonical_path);

CREATE TABLE worktree (
  id              TEXT PRIMARY KEY,
  project_id      TEXT NOT NULL REFERENCES project(id) ON DELETE CASCADE,
  slug            TEXT NOT NULL,
  branch          TEXT NOT NULL,
  path            TEXT NOT NULL,
  base_sha        TEXT,
  origin          TEXT NOT NULL DEFAULT 'created',  -- 'created'|'adopted'
  created_at      INTEGER NOT NULL,
  removed_at      INTEGER,
  locked          INTEGER NOT NULL DEFAULT 0,
  verified_at     INTEGER,
  post_create_cmd TEXT,
  cached_dirty    INTEGER NOT NULL DEFAULT 0,
  cached_added    INTEGER NOT NULL DEFAULT 0,
  cached_removed  INTEGER NOT NULL DEFAULT 0,
  cached_ahead    INTEGER NOT NULL DEFAULT 0,
  cached_behind   INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX ix_worktree_project ON worktree(project_id);
CREATE UNIQUE INDEX ux_worktree_live_path ON worktree(path) WHERE removed_at IS NULL;

CREATE TABLE worktree_alias (
  id          TEXT PRIMARY KEY,
  worktree_id TEXT NOT NULL REFERENCES worktree(id) ON DELETE CASCADE,
  provider    TEXT NOT NULL,
  alias_path  TEXT NOT NULL,
  target_path TEXT NOT NULL,
  kind        TEXT NOT NULL,                -- 'symlink'|'adopted'
  created_at  INTEGER NOT NULL
);
CREATE INDEX ix_alias_worktree ON worktree_alias(worktree_id);
CREATE UNIQUE INDEX ux_alias_path ON worktree_alias(alias_path);

CREATE TABLE account (
  id              TEXT PRIMARY KEY,
  vendor          TEXT NOT NULL,
  label           TEXT NOT NULL,
  config_dir      TEXT,
  model_hint      TEXT,
  source          TEXT NOT NULL,            -- 'cco'|'manual'|'ambient'
  enabled         INTEGER NOT NULL DEFAULT 1,
  is_default      INTEGER NOT NULL DEFAULT 0,
  imported_at     INTEGER NOT NULL,
  last_seen_at    INTEGER,
  last_quota_json TEXT,
  last_quota_at   INTEGER
);
CREATE INDEX ix_account_vendor ON account(vendor);
CREATE UNIQUE INDEX ux_account_config_dir ON account(config_dir) WHERE config_dir IS NOT NULL;

CREATE TABLE pty_session (
  id          TEXT PRIMARY KEY,
  spawn_id    TEXT,
  project_id  TEXT REFERENCES project(id)  ON DELETE SET NULL,
  worktree_id TEXT REFERENCES worktree(id) ON DELETE SET NULL,
  agent_kind  TEXT NOT NULL,
  account_id  TEXT REFERENCES account(id)  ON DELETE SET NULL,
  title       TEXT NOT NULL,
  cwd         TEXT NOT NULL,
  cols        INTEGER NOT NULL,
  rows        INTEGER NOT NULL,
  argv        TEXT NOT NULL,                -- JSON array, masked
  opened_at   INTEGER NOT NULL,
  closed_at   INTEGER,
  exit_code   INTEGER,
  exit_signal INTEGER,
  last_io_at  INTEGER,
  last_status TEXT,
  detached    INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX ix_pty_open     ON pty_session(opened_at)   WHERE closed_at IS NULL;
CREATE INDEX ix_pty_worktree ON pty_session(worktree_id) WHERE worktree_id IS NOT NULL;
CREATE INDEX ix_pty_spawn    ON pty_session(spawn_id)    WHERE spawn_id IS NOT NULL;

CREATE TABLE dispatch_task (
  id                TEXT PRIMARY KEY,
  project_id        TEXT REFERENCES project(id) ON DELETE SET NULL,
  title             TEXT NOT NULL,
  prompt            TEXT NOT NULL,
  task_slug         TEXT NOT NULL,
  base_ref          TEXT,
  worktree_strategy TEXT NOT NULL,
  permission_mode   TEXT,
  timeout_ms        INTEGER,
  created_at        INTEGER NOT NULL,
  finished_at       INTEGER,
  status            TEXT NOT NULL
);
CREATE INDEX ix_task_created ON dispatch_task(created_at);
CREATE INDEX ix_task_project ON dispatch_task(project_id) WHERE project_id IS NOT NULL;

CREATE TABLE dispatch_run (
  id              TEXT PRIMARY KEY,
  task_id         TEXT NOT NULL REFERENCES dispatch_task(id) ON DELETE CASCADE,
  vendor          TEXT NOT NULL,
  model_requested TEXT,
  model_actual    TEXT,
  account_id      TEXT REFERENCES account(id)  ON DELETE SET NULL,
  worktree_id     TEXT REFERENCES worktree(id) ON DELETE SET NULL,
  spawn_id        TEXT,
  run_id          TEXT,                       -- vendor/mesh run id
  conversation_id TEXT,
  status          TEXT NOT NULL,
  note            TEXT,
  route_note      TEXT,
  text            TEXT,                       -- or a runs/<run_id>.txt pointer
  usage_json      TEXT,
  cost_usd        REAL,
  started_at      INTEGER,
  finished_at     INTEGER,
  error           TEXT
);
CREATE INDEX ix_run_task    ON dispatch_run(task_id);
CREATE INDEX ix_run_status  ON dispatch_run(status);
CREATE INDEX ix_run_started ON dispatch_run(started_at);

CREATE TABLE run_event (
  id      INTEGER PRIMARY KEY AUTOINCREMENT,
  run_id  TEXT NOT NULL REFERENCES dispatch_run(id) ON DELETE CASCADE,
  at      INTEGER NOT NULL,
  kind    TEXT NOT NULL,
  payload TEXT
);
CREATE INDEX ix_run_event_run ON run_event(run_id, at);
CREATE INDEX ix_run_event_at  ON run_event(at);

CREATE TABLE proc_sample (
  at        INTEGER NOT NULL,
  scope     TEXT NOT NULL,                   -- 'session'|'run'|'proc'|'global'
  scope_id  TEXT,
  rss_bytes INTEGER NOT NULL,
  cpu_pct   REAL NOT NULL
);
CREATE INDEX ix_proc_sample_at    ON proc_sample(at);
CREATE INDEX ix_proc_sample_scope ON proc_sample(scope, scope_id, at);

CREATE TABLE agent_binary (
  kind             TEXT PRIMARY KEY,
  abs_path         TEXT,
  version          TEXT,
  detected_at      INTEGER NOT NULL,
  path_fingerprint TEXT,
  verified_range   TEXT,
  ok               INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE event_log (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  at          INTEGER NOT NULL,
  category    TEXT NOT NULL,
  level       TEXT NOT NULL,
  subject_id  TEXT,
  message     TEXT NOT NULL,
  detail_json TEXT
);
CREATE INDEX ix_event_log_at      ON event_log(at);
CREATE INDEX ix_event_log_subject ON event_log(subject_id) WHERE subject_id IS NOT NULL;

CREATE TABLE settings_kv (
  key        TEXT PRIMARY KEY,
  value_json TEXT NOT NULL
);

-- ---------------------------------------------------------------- §11-2
-- Empty in M1. Created now so chat import attaches without a migration.
CREATE TABLE conversation (
  id                TEXT PRIMARY KEY,
  source            TEXT NOT NULL,           -- AgentKind
  source_session_id TEXT NOT NULL,           -- composerId / sessionId / threadId
  source_locator    TEXT NOT NULL,           -- file path or "sqlite:<db>#<key>"
  workspace_path    TEXT,
  worktree_id       TEXT REFERENCES worktree(id) ON DELETE SET NULL,
  title             TEXT,
  created_at        INTEGER NOT NULL,
  updated_at        INTEGER NOT NULL,
  model             TEXT,
  mode              TEXT,
  message_count     INTEGER NOT NULL DEFAULT 0,
  resumability      TEXT NOT NULL,           -- 'native_resume'|'transcript_only'|'handoff_only'
  cost_cents        INTEGER,
  imported_at       INTEGER NOT NULL,
  schema_variant    TEXT NOT NULL            -- 'cursor-legacy'|'cursor-3x'|'cc-2.1'|...
);
CREATE UNIQUE INDEX ux_conversation_source ON conversation(source, source_session_id);
CREATE INDEX ix_conversation_updated ON conversation(updated_at);

CREATE TABLE message (
  id              TEXT PRIMARY KEY,
  conversation_id TEXT NOT NULL REFERENCES conversation(id) ON DELETE CASCADE,
  seq             INTEGER NOT NULL,
  parent_id       TEXT REFERENCES message(id) ON DELETE SET NULL,  -- branching DAG
  role            TEXT NOT NULL,             -- 'user'|'assistant'|'system'|'tool'
  blocks          TEXT NOT NULL,             -- JSON array of Block
  files_touched   TEXT,                      -- JSON array of paths
  model           TEXT,
  usage_json      TEXT,
  timestamp       INTEGER NOT NULL,
  raw             TEXT                       -- lossless original record
);
CREATE INDEX ix_message_conversation ON message(conversation_id, seq);
CREATE INDEX ix_message_parent ON message(parent_id) WHERE parent_id IS NOT NULL;
"#;

/// Migration v2 "gilde2" (contract §3.9) — **metadata only**.
///
/// Drops the reserved content tables, adds the Gilde 2 metadata tables, and
/// extends three existing ones. Notes against the contract's SQL:
///
/// * `account.label`, `account.is_default` and `account.source` already exist
///   since v1 (`label` NOT NULL), so only the two quota columns are added —
///   re-adding a column is an error in SQLite;
/// * `app_instance.window_visible_at` is the B5 timestamp T-boot-2 compares
///   every `proc_registry.planned_at` against (§2.4);
/// * `proc_registry.parent_uid` is the owning session of a row (§3.8: rows
///   "carry `parent_uid` where a session owns them");
/// * `pty_session.title` (a v1 column that could hold terminal titles, i.e.
///   content) is dropped with the content tables under `secure_delete`, so
///   the rewritten rows leave no title bytes behind; PTY titles live in
///   memory only (A12.4). [`apply_through`] checkpoints the WAL right after
///   this migration so no older frame keeps them either.
const M0002_GILDE2: &str = r#"
-- ---------------------------------------------------------------- §3.9
-- The reserved content tables and the v1 PTY title column go, and their
-- freed bytes are overwritten (A12.4).
PRAGMA secure_delete = ON;
DROP TABLE IF EXISTS message;
DROP TABLE IF EXISTS conversation;
ALTER TABLE pty_session DROP COLUMN title;
PRAGMA secure_delete = OFF;

CREATE TABLE session_meta (
  uid               TEXT PRIMARY KEY,
  vendor            TEXT NOT NULL CHECK (vendor IN ('claude','codex','cursor','agy')),
  account_id        TEXT,
  vendor_session_id TEXT,
  project_id        TEXT, location_path TEXT, cwd TEXT, transcript_path TEXT,
  origin_last       TEXT, control_mode_last TEXT, permission_mode_last TEXT, model_last TEXT,
  cli_path_last     TEXT, cli_version_last TEXT,
  team_key          TEXT, agent_id TEXT, teammate_name TEXT, parent_session_id TEXT,
  parent_uid        TEXT,
  first_seen_at     INTEGER NOT NULL, started_at INTEGER, ended_at INTEGER,
  last_activity_at  INTEGER, last_seen_at INTEGER,
  usage_day TEXT, usage_in INTEGER, usage_out INTEGER, usage_cached INTEGER, cost_micros INTEGER,
  absent_since      INTEGER,
  updated_at        INTEGER NOT NULL,
  UNIQUE (vendor, account_id, vendor_session_id)
);
CREATE INDEX ix_session_meta_absent ON session_meta(absent_since) WHERE absent_since IS NOT NULL;
CREATE INDEX ix_session_meta_project ON session_meta(project_id) WHERE project_id IS NOT NULL;

CREATE TABLE history_index (
  path TEXT PRIMARY KEY, vendor TEXT NOT NULL, account_id TEXT, vendor_session_id TEXT,
  size INTEGER NOT NULL, mtime_ms INTEGER NOT NULL, inode INTEGER NOT NULL,
  cwd TEXT, git_branch TEXT, entrypoint TEXT, is_teammate INTEGER NOT NULL DEFAULT 0,
  team_name TEXT, teammate_name TEXT, continued_in TEXT, turns INTEGER,
  first_row_at INTEGER, last_row_at INTEGER, scanned_at INTEGER NOT NULL
);
CREATE INDEX ix_history_index_session ON history_index(vendor, vendor_session_id);

CREATE TABLE quit_record (
  id TEXT PRIMARY KEY,
  cause TEXT NOT NULL CHECK (cause IN ('quit','crash')),
  created_at INTEGER NOT NULL,
  resolved_at INTEGER
);
CREATE INDEX ix_quit_record_open ON quit_record(created_at) WHERE resolved_at IS NULL;

CREATE TABLE quit_record_item (
  record_id TEXT NOT NULL REFERENCES quit_record(id) ON DELETE CASCADE,
  uid TEXT NOT NULL, control_mode TEXT NOT NULL, permission_mode TEXT, model TEXT,
  was_waiting TEXT CHECK (was_waiting IN ('approval','answer')),
  team_lead INTEGER NOT NULL DEFAULT 0,
  survivor INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (record_id, uid)
);

CREATE TABLE claude_compat_observed (
  version TEXT NOT NULL, conversation TEXT, pane_hosting TEXT,
  source TEXT NOT NULL,
  binary_fingerprint TEXT,
  static_result TEXT, handshake_ok INTEGER, features_json TEXT,
  note TEXT, observed_at INTEGER NOT NULL,
  PRIMARY KEY (version, binary_fingerprint)
);

CREATE TABLE project_prefs (
  project_id TEXT PRIMARY KEY,
  last_vendor TEXT, last_account_id TEXT, last_model TEXT, last_control_mode TEXT,
  last_location_kind TEXT,
  updated_at INTEGER NOT NULL
);

ALTER TABLE account ADD COLUMN quota_json TEXT;
ALTER TABLE account ADD COLUMN quota_at INTEGER;

ALTER TABLE app_instance ADD COLUMN window_visible_at INTEGER;

ALTER TABLE proc_registry ADD COLUMN parent_uid TEXT;
CREATE INDEX ix_proc_parent ON proc_registry(parent_uid) WHERE parent_uid IS NOT NULL;
"#;

/// The ordered, forward-only set. Append only.
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "initial",
        sql: M0001_INITIAL,
    },
    Migration {
        version: 2,
        name: "gilde2",
        sql: M0002_GILDE2,
    },
];

/// Migrations that remove content (A12.4): the WAL is checkpointed and
/// truncated right after them.
const SCRUBBING: &[u32] = &[2];

/// Highest version this build knows about.
pub fn latest_version() -> u32 {
    MIGRATIONS.last().map(|m| m.version).unwrap_or(0)
}

/// Version recorded in the database (0 when the table does not exist yet).
pub fn current_version(conn: &Connection) -> Result<u32> {
    let exists: Option<String> = conn
        .query_row(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='schema_migrations'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if exists.is_none() {
        return Ok(0);
    }
    let v: Option<i64> = conn.query_row("SELECT MAX(version) FROM schema_migrations", [], |r| {
        r.get(0)
    })?;
    Ok(v.unwrap_or(0) as u32)
}

/// Look for a schema problem without changing anything.
///
/// Returns the typed [`SchemaError`] when the database was written by an
/// incompatible build. `Ok(None)` means the schema is either current or simply
/// behind and safe to migrate forward.
///
/// Assumes `schema_migrations` exists (see [`apply_all`], which bootstraps it).
pub fn inspect(conn: &Connection) -> Result<Option<SchemaError>> {
    let known = latest_version();
    let found = current_version(conn)?;
    if found > known {
        return Ok(Some(SchemaError::FromTheFuture { found, known }));
    }

    for m in MIGRATIONS {
        let recorded: Option<String> = conn
            .query_row(
                "SELECT checksum FROM schema_migrations WHERE version = ?1",
                [m.version],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(recorded) = recorded {
            let expected = m.checksum();
            if recorded != expected {
                return Ok(Some(SchemaError::ChecksumMismatch {
                    version: m.version,
                    name: m.name,
                    recorded,
                    expected,
                }));
            }
        }
    }
    Ok(None)
}

/// Bring the schema up to `latest_version()`.
///
/// Returns the version in force afterwards. Re-running is a no-op: already
/// applied migrations are only checksum-verified, never re-executed.
pub fn apply_all(conn: &Connection, now: i64) -> Result<u32> {
    apply_through(conn, now, latest_version())
}

/// [`apply_all`], stopping after `max_version` — the v1→v2 migration test
/// builds a v1 database with it. Boot always calls [`apply_all`].
pub fn apply_through(conn: &Connection, now: i64, max_version: u32) -> Result<u32> {
    conn.execute_batch(BOOTSTRAP_SQL)?;

    if let Some(problem) = inspect(conn)? {
        return Err(problem.into());
    }

    let applied: Vec<u32> = {
        let mut stmt = conn.prepare("SELECT version FROM schema_migrations")?;
        let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row? as u32);
        }
        out
    };

    let mut scrubbed = false;
    for m in MIGRATIONS {
        if applied.contains(&m.version) || m.version > max_version {
            continue;
        }
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(m.sql)?;
        tx.execute(
            "INSERT INTO schema_migrations (version, applied_at, checksum, name)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![m.version, now, m.checksum(), m.name],
        )?;
        tx.commit()?;
        scrubbed |= SCRUBBING.contains(&m.version);
        tracing::info!(version = m.version, name = m.name, "applied migration");
    }
    if scrubbed {
        // The removed content may still sit in older WAL frames: copy the
        // zeroed pages into the main file and truncate the WAL (A12.4).
        // A rollback-journal or in-memory database answers without effect.
        if let Err(err) = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(())) {
            tracing::warn!(error = %err, "could not checkpoint the WAL after a scrubbing migration");
        }
    }

    current_version(conn)
}

/// Table names the full migration set creates. Used by the schema test and by
/// `app_diagnostics()`.
pub const EXPECTED_TABLES: &[&str] = &[
    "account",
    "agent_binary",
    "app_instance",
    "claude_compat_observed",
    "dispatch_run",
    "dispatch_task",
    "event_log",
    "history_index",
    "proc_registry",
    "proc_sample",
    "project",
    "project_prefs",
    "pty_session",
    "quit_record",
    "quit_record_item",
    "run_event",
    "schema_migrations",
    "session_meta",
    "settings_kv",
    "worktree",
    "worktree_alias",
];

/// Tables v1 created that v2 drops (the reserved content tables, §3.9).
pub const DROPPED_IN_V2: &[&str] = &["conversation", "message"];

/// Legacy tables Gilde 2 never reads or writes (§3.9): kept for the user's old
/// data until a Settings → Storage action removes them (v1.x). The schema
/// privacy scan (T-priv-1) reports their content columns separately.
pub const LEGACY_TABLES: &[&str] = &["dispatch_task", "dispatch_run", "run_event"];

/// Indexes DESIGN §8-2 names explicitly. Their absence is a schema bug.
pub const REQUIRED_PROC_INDEXES: &[&str] = &[
    "ix_proc_state",
    "ix_proc_run",
    "ix_proc_wt",
    "ix_proc_pid",
    "ix_proc_gen",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        conn
    }

    fn table_names(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap();
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
        rows.map(|r| r.unwrap())
            .filter(|n| !n.starts_with("sqlite_"))
            .collect()
    }

    #[test]
    fn the_migration_set_creates_every_table_and_index() {
        let conn = mem();
        assert_eq!(current_version(&conn).unwrap(), 0);
        assert_eq!(apply_all(&conn, 1_000).unwrap(), latest_version());

        let tables = table_names(&conn);
        for expected in EXPECTED_TABLES {
            assert!(
                tables.iter().any(|t| t == expected),
                "missing table {expected}; have {tables:?}"
            );
        }
        assert_eq!(tables.len(), EXPECTED_TABLES.len(), "unexpected extra tables");

        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='index'")
            .unwrap();
        let indexes: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        for ix in REQUIRED_PROC_INDEXES {
            assert!(indexes.iter().any(|i| i == ix), "missing index {ix}");
        }
    }

    #[test]
    fn reapplying_is_a_no_op() {
        let conn = mem();
        apply_all(&conn, 1_000).unwrap();
        let applied_at: i64 = conn
            .query_row("SELECT applied_at FROM schema_migrations WHERE version=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(applied_at, 1_000);

        // A second (and third) run must not rewrite the row or re-run the DDL.
        assert_eq!(apply_all(&conn, 2_000).unwrap(), latest_version());
        assert_eq!(apply_all(&conn, 3_000).unwrap(), latest_version());
        let applied_at: i64 = conn
            .query_row("SELECT applied_at FROM schema_migrations WHERE version=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(applied_at, 1_000, "applied_at must not be rewritten");
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, MIGRATIONS.len() as i64);
    }

    #[test]
    fn checksum_mismatch_is_a_typed_error() {
        let conn = mem();
        apply_all(&conn, 1_000).unwrap();
        conn.execute(
            "UPDATE schema_migrations SET checksum = 'tampered' WHERE version = 1",
            [],
        )
        .unwrap();

        assert!(matches!(
            inspect(&conn).unwrap(),
            Some(SchemaError::ChecksumMismatch { version: 1, .. })
        ));
        assert!(apply_all(&conn, 2_000).is_err(), "boot must abort");
    }

    #[test]
    fn a_newer_schema_version_aborts() {
        let conn = mem();
        apply_all(&conn, 1_000).unwrap();
        conn.execute(
            "INSERT INTO schema_migrations (version, applied_at, checksum, name)
             VALUES (999, 1, 'x', 'from-the-future')",
            [],
        )
        .unwrap();
        assert!(matches!(
            inspect(&conn).unwrap(),
            Some(SchemaError::FromTheFuture { found: 999, known: 2 })
        ));
        assert!(apply_all(&conn, 2_000).is_err(), "boot must abort");
    }

    #[test]
    fn v2_on_top_of_v1_drops_content_tables_and_keeps_rows() {
        let conn = mem();
        assert_eq!(apply_through(&conn, 1_000, 1).unwrap(), 1);
        conn.execute_batch(
            "INSERT INTO conversation (id, source, source_session_id, source_locator, title,
               created_at, updated_at, resumability, imported_at, schema_variant)
             VALUES ('c1','claude','s','f','secret title',1,1,'native_resume',1,'cc-2.1');
             INSERT INTO account (id, vendor, label, source, imported_at) VALUES ('a1','claude','Work','manual',1);",
        )
        .unwrap();
        assert_eq!(apply_all(&conn, 2_000).unwrap(), 2);
        let tables = table_names(&conn);
        for gone in DROPPED_IN_V2 {
            assert!(!tables.iter().any(|t| t == gone), "{gone} survived v2");
        }
        let label: String = conn
            .query_row("SELECT label FROM account WHERE id='a1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(label, "Work");
        let quota: Option<String> = conn
            .query_row("SELECT quota_json FROM account WHERE id='a1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(quota, None);
    }

    #[test]
    fn migration_versions_are_unique_and_ascending() {
        let mut prev = 0u32;
        for m in MIGRATIONS {
            assert!(m.version > prev, "migrations must ascend: {}", m.version);
            prev = m.version;
            assert_eq!(m.checksum().len(), 64);
        }
        assert_eq!(latest_version(), prev);
    }
}
