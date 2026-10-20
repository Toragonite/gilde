//! `state.db` — connection, forward-only migrations, and typed DAOs.
//!
//! # Threading contract
//!
//! One `rusqlite::Connection` lives behind a mutex inside [`Db`]. `Connection`
//! is synchronous and blocking, so **every** access goes through
//! [`Db::read`] / [`Db::write`] / [`Db::write_durable`], which hop onto
//! `tokio::task::spawn_blocking`. The mutex is never locked in an async
//! context, and no guard is ever held across an `.await`, because the guard is
//! taken and dropped entirely inside the blocking closure.
//!
//! # Durability
//!
//! WAL, `busy_timeout=5000`, `foreign_keys=ON`, `synchronous=NORMAL`.
//! [`Db::write_durable`] raises `synchronous=FULL` for the duration of one
//! closure and lowers it again afterwards (including on panic). That is the
//! PID-registry path only: the write-before-spawn row must survive a hard
//! crash, which is the foundation of every recovery path (DESIGN §8-2).
//!
//! # Boot never fails
//!
//! A corrupt database is quarantined to `state.db.corrupt-<ts>` and a fresh one
//! is created; [`Db::recovered_from_corruption`] then reports `true` so the UI
//! can toast where the old file went. Losing history is acceptable; refusing to
//! boot is not — at that moment the app's job is to help the user find and kill
//! orphan processes, and that only needs a live scan (DESIGN §10-4).
//!
//! # Time
//!
//! Every `*_at` column is **epoch milliseconds** ([`now_ms`]). The single
//! exception is `proc_registry.proc_start_ts`, which is the kernel start time
//! in **seconds** because that is the unit `libproc` reports.

pub mod dao;
pub mod migrations;
pub mod models;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use rusqlite::Connection;

use crate::error::{GildeError, Result};

pub use migrations::SchemaError;
pub use models::RetentionReport;

/// Epoch milliseconds. The unit of every `*_at` column.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// How long `PRAGMA integrity_check` may run at boot before we give up and
/// assume the file is fine (DESIGN §10-4).
const INTEGRITY_BUDGET: Duration = Duration::from_secs(2);
const BUSY_TIMEOUT: Duration = Duration::from_millis(5_000);

/// The `state.db` handle. Cheap to clone; all clones share one connection.
#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
    path: Option<PathBuf>,
    recovered_from_corruption: bool,
    quarantined_to: Option<PathBuf>,
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db")
            .field("path", &self.path)
            .field("recovered_from_corruption", &self.recovered_from_corruption)
            .finish()
    }
}

impl Db {
    /// Open (creating if needed), verify, and migrate `state.db`.
    ///
    /// Never fails because of a damaged file: a failed `integrity_check` moves
    /// the old database aside and starts a new one. It *does* fail on a schema
    /// this build cannot understand — see [`SchemaError`].
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut recovered = false;
        let mut quarantined_to = None;

        let mut conn = match open_configured(path) {
            Ok(c) => c,
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "state.db would not open; quarantining");
                quarantined_to = Some(quarantine(path)?);
                recovered = true;
                open_configured(path)?
            }
        };

        match integrity_check(&conn) {
            Integrity::Ok => {}
            Integrity::BudgetExceeded => {
                tracing::warn!(
                    budget_ms = INTEGRITY_BUDGET.as_millis(),
                    "integrity_check exceeded its budget; assuming the database is intact"
                );
            }
            Integrity::Corrupt(detail) => {
                tracing::error!(path = %path.display(), detail, "state.db failed integrity_check");
                drop(conn);
                quarantined_to = Some(quarantine(path)?);
                recovered = true;
                conn = open_configured(path)?;
            }
        }

        migrations::apply_all(&conn, now_ms())?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            path: Some(path.to_path_buf()),
            recovered_from_corruption: recovered,
            quarantined_to,
        })
    }

    /// An in-memory database with the schema applied. Tests only.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        configure(&conn)?;
        migrations::apply_all(&conn, now_ms())?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            path: None,
            recovered_from_corruption: false,
            quarantined_to: None,
        })
    }

    /// True when the previous `state.db` was damaged and had to be replaced.
    /// The UI shows a toast pointing at [`Db::quarantined_to`].
    pub fn recovered_from_corruption(&self) -> bool {
        self.recovered_from_corruption
    }

    /// Where the damaged database was moved, if any.
    pub fn quarantined_to(&self) -> Option<&Path> {
        self.quarantined_to.as_deref()
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Schema version currently in force.
    pub fn schema_version(&self) -> Result<u32> {
        let conn = self.conn.lock();
        migrations::current_version(&conn)
    }

    /// Run a read on the blocking pool.
    pub async fn read<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = self.conn.clone();
        join(tokio::task::spawn_blocking(move || {
            let guard = conn.lock();
            f(&guard)
        }))
        .await
    }

    /// Run a write on the blocking pool at `synchronous=NORMAL`.
    pub async fn write<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = self.conn.clone();
        join(tokio::task::spawn_blocking(move || {
            let guard = conn.lock();
            f(&guard)
        }))
        .await
    }

    /// Run a write with `synchronous=FULL` in force, restoring `NORMAL`
    /// afterwards even if the closure panics.
    ///
    /// Reserved for PID-registry writes (DESIGN §8-2): roughly 1–3 ms of extra
    /// fsync per spawn, paid deliberately so that a write-before-spawn row
    /// survives a hard crash. Do not "optimize" this by batching — batching
    /// destroys the property it exists for.
    pub async fn write_durable<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = self.conn.clone();
        join(tokio::task::spawn_blocking(move || {
            let guard = conn.lock();
            let _full = SyncFullGuard::enter(&guard)?;
            f(&guard)
        }))
        .await
    }

    /// Synchronous escape hatch for boot-time code that already owns a blocking
    /// thread (the reaper's startup sweep). Never call this from async code.
    pub fn blocking<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T>,
    {
        let guard = self.conn.lock();
        f(&guard)
    }
}

async fn join<T>(handle: tokio::task::JoinHandle<Result<T>>) -> Result<T> {
    match handle.await {
        Ok(res) => res,
        Err(err) => Err(GildeError::internal(format!("database task failed: {err}"))),
    }
}

/// Raises `synchronous=FULL` and lowers it back to `NORMAL` on drop.
struct SyncFullGuard<'a> {
    conn: &'a Connection,
}

impl<'a> SyncFullGuard<'a> {
    fn enter(conn: &'a Connection) -> Result<Self> {
        conn.pragma_update(None, "synchronous", "FULL")?;
        Ok(Self { conn })
    }
}

impl Drop for SyncFullGuard<'_> {
    fn drop(&mut self) {
        if let Err(err) = self.conn.pragma_update(None, "synchronous", "NORMAL") {
            tracing::error!(error = %err, "could not restore synchronous=NORMAL");
        }
    }
}

fn open_configured(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    configure(&conn)?;
    Ok(conn)
}

fn configure(conn: &Connection) -> Result<()> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    // journal_mode returns a row, so it cannot go through pragma_update.
    // In-memory databases answer "memory" and that is fine.
    let mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
    if mode.to_ascii_lowercase() != "wal" && mode.to_ascii_lowercase() != "memory" {
        tracing::warn!(mode, "journal_mode is not WAL");
    }
    conn.pragma_update(None, "foreign_keys", true)?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(())
}

enum Integrity {
    Ok,
    /// The check did not finish inside the budget. Treated as "assume intact":
    /// a slow check is not evidence of damage, and destroying a large healthy
    /// database because it took three seconds to verify would be far worse than
    /// the risk of running on it.
    BudgetExceeded,
    Corrupt(String),
}

fn integrity_check(conn: &Connection) -> Integrity {
    let handle = conn.get_interrupt_handle();
    let finished = Arc::new(AtomicBool::new(false));
    let flag = finished.clone();

    let watchdog = std::thread::spawn(move || {
        let deadline = Instant::now() + INTEGRITY_BUDGET;
        while Instant::now() < deadline {
            if flag.load(Ordering::SeqCst) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if flag.load(Ordering::SeqCst) {
            false
        } else {
            // Safe against a late fire: sqlite3_interrupt() is a no-op when no
            // statement is running and never affects statements started later.
            handle.interrupt();
            true
        }
    });

    // `integrity_check(1)` stops at the first problem — we only need to know
    // whether there is one.
    let result: rusqlite::Result<String> =
        conn.query_row("PRAGMA integrity_check(1)", [], |r| r.get(0));
    finished.store(true, Ordering::SeqCst);
    let interrupted = watchdog.join().unwrap_or(false);

    match result {
        Ok(text) if text.eq_ignore_ascii_case("ok") => Integrity::Ok,
        Ok(text) => Integrity::Corrupt(text),
        Err(_) if interrupted => Integrity::BudgetExceeded,
        Err(err) => Integrity::Corrupt(err.to_string()),
    }
}

/// Move a damaged database (and its WAL/SHM siblings) out of the way.
/// Returns the path the main file was moved to.
fn quarantine(path: &Path) -> Result<PathBuf> {
    let stamp = now_ms();
    let mut base = path.as_os_str().to_os_string();
    base.push(format!(".corrupt-{stamp}"));
    let target = PathBuf::from(base);

    if path.exists() {
        std::fs::rename(path, &target)?;
    }
    for suffix in ["-wal", "-shm"] {
        let mut from = path.as_os_str().to_os_string();
        from.push(suffix);
        let from = PathBuf::from(from);
        if from.exists() {
            let mut to = target.as_os_str().to_os_string();
            to.push(suffix);
            // A leftover WAL is not worth failing the boot over.
            let _ = std::fs::rename(&from, PathBuf::from(to));
        }
    }
    tracing::warn!(moved_to = %target.display(), "quarantined a damaged state.db");
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::dao::*;
    use super::models::*;
    use super::*;

    fn instance(id: &str, boot: &str) -> AppInstanceRow {
        AppInstanceRow {
            id: id.to_string(),
            pid: 4242,
            boot_id: boot.to_string(),
            version: "0.1.0".to_string(),
            started_at: 1_000,
            ended_at: None,
        }
    }

    fn planned(spawn_id: &str, instance_id: &str) -> NewPlannedProc {
        NewPlannedProc::new(
            spawn_id,
            instance_id,
            ProcKind::Headless,
            vec!["/usr/local/bin/claude".into(), "-p".into(), "hello".into()],
            "/tmp",
            10_000,
        )
    }

    // -- open / migrate -----------------------------------------------------

    #[test]
    fn in_memory_open_applies_the_schema() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.schema_version().unwrap(), migrations::latest_version());
        assert!(!db.recovered_from_corruption());
    }

    #[test]
    fn open_then_reopen_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");

        let db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), 1);
        let applied_at: i64 = db
            .blocking(|c| {
                Ok(c.query_row(
                    "SELECT applied_at FROM schema_migrations WHERE version = 1",
                    [],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        // Seed a row so we can prove the reopen did not recreate the file.
        db.blocking(|c| dao::app_instance::insert(c, &instance("i-1", "boot-a")))
            .unwrap();
        drop(db);

        let db2 = Db::open(&path).unwrap();
        assert_eq!(db2.schema_version().unwrap(), 1);
        assert!(!db2.recovered_from_corruption());
        let applied_at2: i64 = db2
            .blocking(|c| {
                Ok(c.query_row(
                    "SELECT applied_at FROM schema_migrations WHERE version = 1",
                    [],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(applied_at, applied_at2, "migration must not re-run");
        let rows = db2.blocking(|c| dao::app_instance::list_open(c)).unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn a_file_database_runs_in_wal_with_a_busy_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let db = Db::open(&path).unwrap();

        let mode: String = db
            .blocking(|c| Ok(c.query_row("PRAGMA journal_mode", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(mode.to_ascii_lowercase(), "wal");

        let fk: i64 = db
            .blocking(|c| Ok(c.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(fk, 1);

        let busy: i64 = db
            .blocking(|c| Ok(c.query_row("PRAGMA busy_timeout", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(busy, 5_000);

        let sync: i64 = db
            .blocking(|c| Ok(c.query_row("PRAGMA synchronous", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(sync, 1, "NORMAL outside the registry path");

        assert!(path.with_extension("db-wal").exists() || dir.path().join("state.db-wal").exists());
    }

    #[test]
    fn a_corrupt_file_is_quarantined_and_boot_still_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        // Not a SQLite file at all.
        std::fs::write(&path, b"this is not a database, not even close").unwrap();

        let db = Db::open(&path).unwrap();
        assert!(db.recovered_from_corruption());
        assert_eq!(db.schema_version().unwrap(), migrations::latest_version());
        let moved = db.quarantined_to().expect("quarantine path");
        assert!(moved.exists());
        assert!(moved.file_name().unwrap().to_string_lossy().contains(".corrupt-"));
        // The new database works.
        db.blocking(|c| dao::app_instance::insert(c, &instance("i-1", "boot-a")))
            .unwrap();
    }

    // -- proc_registry lifecycle -------------------------------------------

    #[test]
    fn proc_row_moves_planned_to_running_to_exited() {
        let db = Db::open_in_memory().unwrap();
        db.blocking(|c| {
            app_instance::insert(c, &instance("i-1", "boot-a"))?;
            proc::insert_planned(c, &planned("s-1", "i-1"))?;

            let row = proc::get(c, "s-1")?.expect("planned row");
            assert_eq!(row.state, ProcState::Planned);
            assert_eq!(row.pid, None, "a planned row has no pid yet");
            assert_eq!(row.env_marker, "s-1", "env marker is the spawn id");
            assert_eq!(
                row.argv_vec(),
                vec!["/usr/local/bin/claude", "-p", "hello"]
            );
            assert_eq!(
                row.argv_sig,
                argv_signature(&["/usr/local/bin/claude", "-p", "hello"])
            );

            assert!(proc::mark_running(c, "s-1", 777, Some(777), Some(1_700), 11_000)?);
            let row = proc::get(c, "s-1")?.unwrap();
            assert_eq!(row.state, ProcState::Running);
            assert_eq!(row.pid, Some(777));
            assert_eq!(row.pgid, Some(777));
            assert_eq!(row.proc_start_ts, Some(1_700));
            assert_eq!(row.started_at, Some(11_000));

            assert!(proc::mark_exited(c, "s-1", Some(0), None, 12_000)?);
            let row = proc::get(c, "s-1")?.unwrap();
            assert_eq!(row.state, ProcState::Exited);
            assert_eq!(row.exit_code, Some(0));
            assert_eq!(row.exited_at, Some(12_000));
            assert!(row.state.is_terminal());

            // Unknown spawn ids report "no such row" rather than erroring.
            assert!(!proc::mark_exited(c, "nope", Some(1), None, 1)?);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn instance_and_generation_filters_are_exact() {
        let db = Db::open_in_memory().unwrap();
        db.blocking(|c| {
            app_instance::insert(c, &instance("i-current", "boot-a"))?;
            app_instance::insert(c, &instance("i-crashed", "boot-a"))?;

            proc::insert_planned(c, &planned("s-mine", "i-current"))?;
            let mut theirs = planned("s-theirs", "i-crashed");
            theirs.generation = 3;
            proc::insert_planned(c, &theirs)?;
            let mut other_gen = planned("s-gen7", "i-crashed");
            other_gen.generation = 7;
            proc::insert_planned(c, &other_gen)?;

            // rows_of_instance is scoped to exactly one instance.
            let crashed = proc::rows_of_instance(c, "i-crashed")?;
            assert_eq!(crashed.len(), 2);
            assert!(crashed.iter().all(|r| r.app_instance == "i-crashed"));
            assert!(
                !crashed.iter().any(|r| r.spawn_id == "s-mine"),
                "the current instance's rows must not appear"
            );

            // live_rows excludes the current instance for the reaper.
            let live = proc::live_rows(c, Some("i-current"))?;
            assert_eq!(live.len(), 2);
            assert!(!live.iter().any(|r| r.app_instance == "i-current"));
            assert_eq!(proc::live_rows(c, None)?.len(), 3);

            // generation filtering is exact.
            assert_eq!(proc::rows_of_generation(c, 3)?.len(), 1);
            assert_eq!(proc::rows_of_generation(c, 3)?[0].spawn_id, "s-theirs");
            assert_eq!(proc::rows_of_generation(c, 7)?.len(), 1);
            assert_eq!(proc::rows_of_generation(c, 0)?.len(), 1);
            assert_eq!(proc::rows_of_generation(c, 99)?.len(), 0);

            // exited rows drop out of the live set but stay in the instance set.
            proc::mark_exited(c, "s-theirs", Some(0), None, 20_000)?;
            assert_eq!(proc::live_rows(c, Some("i-current"))?.len(), 1);
            assert_eq!(proc::rows_of_instance(c, "i-crashed")?.len(), 2);
            assert_eq!(proc::live_rows_of_generation(c, 3)?.len(), 0);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn keep_alive_run_and_worktree_lookups() {
        let db = Db::open_in_memory().unwrap();
        db.blocking(|c| {
            app_instance::insert(c, &instance("i-1", "boot-a"))?;
            let mut p = planned("s-1", "i-1");
            p.run_id = Some("run-9".into());
            p.worktree_id = Some("wt-9".into());
            p.session_id = Some("sess-9".into());
            proc::insert_planned(c, &p)?;

            assert_eq!(proc::by_run_id(c, "run-9")?.len(), 1);
            assert_eq!(proc::by_worktree(c, "wt-9")?.len(), 1);
            assert_eq!(proc::by_session(c, "sess-9")?.len(), 1);
            assert_eq!(proc::by_run_id(c, "run-x")?.len(), 0);

            assert!(!proc::get(c, "s-1")?.unwrap().keep_alive);
            assert!(proc::set_keep_alive(c, "s-1", true)?);
            assert!(proc::get(c, "s-1")?.unwrap().keep_alive);

            proc::mark_running(c, "s-1", 1234, Some(1234), Some(9), 1)?;
            assert_eq!(proc::by_pid(c, 1234)?.len(), 1);

            assert!(proc::mark_state(c, "s-1", ProcState::Lost, Some("reboot"))?);
            let row = proc::get(c, "s-1")?.unwrap();
            assert_eq!(row.state, ProcState::Lost);
            assert_eq!(row.note.as_deref(), Some("reboot"));
            assert_eq!(proc::by_pid(c, 1234)?.len(), 0, "lost rows are not live");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn foreign_keys_are_enforced() {
        let db = Db::open_in_memory().unwrap();
        db.blocking(|c| {
            // proc_registry.app_instance -> app_instance.id
            let err = proc::insert_planned(c, &planned("s-1", "no-such-instance")).unwrap_err();
            assert!(
                err.to_string().to_lowercase().contains("foreign key"),
                "expected a foreign key violation, got: {err}"
            );

            // worktree.project_id -> project.id
            let wt = WorktreeRow {
                id: "wt-1".into(),
                project_id: "no-such-project".into(),
                slug: "task".into(),
                branch: "gilde/task".into(),
                path: "/tmp/wt-1".into(),
                base_sha: None,
                origin: WorktreeOrigin::Created,
                created_at: 1,
                removed_at: None,
                locked: false,
                verified_at: None,
                post_create_cmd: None,
                cached_dirty: 0,
                cached_added: 0,
                cached_removed: 0,
                cached_ahead: 0,
                cached_behind: 0,
            };
            assert!(worktree::insert(c, &wt).is_err());
            Ok(())
        })
        .unwrap();
    }

    // -- projects / worktrees / aliases -------------------------------------

    #[test]
    fn project_worktree_alias_round_trip() {
        let db = Db::open_in_memory().unwrap();
        db.blocking(|c| {
            let proj = ProjectRow {
                id: "p-1".into(),
                path: "~/code/app".into(),
                canonical_path: "/Users/x/code/app".into(),
                repo_slug: "app".into(),
                name: "app".into(),
                color: Some("#7aa2f7".into()),
                default_base_ref: "HEAD".into(),
                added_at: 1,
                last_opened_at: None,
            };
            project::upsert(c, &proj)?;
            assert_eq!(project::list(c)?.len(), 1);
            assert_eq!(
                project::get_by_canonical_path(c, "/Users/x/code/app")?
                    .unwrap()
                    .id,
                "p-1"
            );
            assert!(project::touch_opened(c, "p-1", 500)?);
            assert_eq!(project::get(c, "p-1")?.unwrap().last_opened_at, Some(500));

            let wt = WorktreeRow {
                id: "wt-1".into(),
                project_id: "p-1".into(),
                slug: "fix-login".into(),
                branch: "gilde/fix-login".into(),
                path: "/Users/x/.gilde/worktrees/app/fix-login".into(),
                base_sha: Some("deadbeef".into()),
                origin: WorktreeOrigin::Created,
                created_at: 2,
                removed_at: None,
                locked: false,
                verified_at: None,
                post_create_cmd: Some("npm ci".into()),
                cached_dirty: 0,
                cached_added: 0,
                cached_removed: 0,
                cached_ahead: 0,
                cached_behind: 0,
            };
            worktree::insert(c, &wt)?;
            assert_eq!(worktree::list_live(c)?.len(), 1);
            assert_eq!(worktree::list_by_project(c, "p-1", false)?.len(), 1);

            worktree::update_stats_cache(
                c,
                "wt-1",
                &WorktreeStatsCache {
                    dirty: 3,
                    added: 120,
                    removed: 7,
                    ahead: 2,
                    behind: 1,
                },
            )?;
            let stored = worktree::get(c, "wt-1")?.unwrap();
            assert_eq!(stored.cached_added, 120);
            assert_eq!(stored.cached_behind, 1);
            assert_eq!(stored.post_create_cmd.as_deref(), Some("npm ci"));

            worktree_alias::insert(
                c,
                &WorktreeAliasRow {
                    id: "al-1".into(),
                    worktree_id: "wt-1".into(),
                    provider: "claude-w".into(),
                    alias_path: "/Users/x/code/app/.claude/worktrees/fix-login".into(),
                    target_path: stored.path.clone(),
                    kind: AliasKind::Symlink,
                    created_at: 3,
                },
            )?;
            assert_eq!(worktree_alias::list_by_worktree(c, "wt-1")?.len(), 1);

            // Cascade: removing the worktree removes its aliases.
            c.execute("DELETE FROM worktree WHERE id = 'wt-1'", [])?;
            assert_eq!(worktree_alias::list_by_worktree(c, "wt-1")?.len(), 0);

            assert!(worktree::mark_removed(c, "wt-1", 9).is_ok());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn accounts_upsert_by_config_dir_is_idempotent() {
        let db = Db::open_in_memory().unwrap();
        db.blocking(|c| {
            let mut acc = AccountRow {
                id: "a-1".into(),
                vendor: "claude".into(),
                label: "work-claude".into(),
                config_dir: Some("/Users/x/.claude-work-claude".into()),
                model_hint: None,
                source: AccountSource::Cco,
                enabled: true,
                is_default: false,
                imported_at: 1,
                last_seen_at: None,
                last_quota_json: None,
                last_quota_at: None,
            };
            account::upsert_by_config_dir(c, &acc)?;

            // Same config dir, different generated id => same slot, id preserved.
            acc.id = "a-2".into();
            acc.label = "renamed".into();
            acc.last_seen_at = Some(50);
            let merged = account::upsert_by_config_dir(c, &acc)?;
            assert_eq!(merged.id, "a-1");
            assert_eq!(merged.label, "renamed");
            assert_eq!(account::list(c)?.len(), 1);

            // The synthetic ambient account has no config dir.
            let ambient = AccountRow {
                id: "ambient".into(),
                vendor: "claude".into(),
                label: "ambient".into(),
                config_dir: None,
                source: AccountSource::Ambient,
                ..merged.clone()
            };
            account::upsert_by_config_dir(c, &ambient)?;
            account::upsert_by_config_dir(c, &ambient)?;
            assert_eq!(account::list(c)?.len(), 2);

            assert!(account::set_default(c, "a-1")?);
            assert!(account::get(c, "a-1")?.unwrap().is_default);
            assert!(account::set_default(c, "ambient")?);
            assert!(!account::get(c, "a-1")?.unwrap().is_default, "one default per vendor");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn pty_session_dispatch_and_agent_binary_round_trip() {
        let db = Db::open_in_memory().unwrap();
        db.blocking(|c| {
            pty_session::insert(
                c,
                &PtySessionRow {
                    id: "sess-1".into(),
                    spawn_id: Some("s-1".into()),
                    project_id: None,
                    worktree_id: None,
                    agent_kind: "claude".into(),
                    account_id: None,
                    title: "claude · fix-login".into(),
                    cwd: "/tmp".into(),
                    cols: 120,
                    rows: 40,
                    argv: "[\"/usr/local/bin/claude\"]".into(),
                    opened_at: 1,
                    closed_at: None,
                    exit_code: None,
                    exit_signal: None,
                    last_io_at: None,
                    last_status: None,
                    detached: false,
                },
            )?;
            assert_eq!(pty_session::list_open(c)?.len(), 1);
            assert!(pty_session::touch_io(c, "sess-1", 500)?);
            assert!(pty_session::set_status(c, "sess-1", "working")?);
            assert!(pty_session::set_detached(c, "sess-1", true)?);
            let row = pty_session::get(c, "sess-1")?.unwrap();
            assert_eq!(row.last_io_at, Some(500));
            assert_eq!(row.last_status.as_deref(), Some("working"));
            assert!(row.detached);
            assert!(pty_session::mark_closed(c, "sess-1", 900, Some(0), None)?);
            assert_eq!(pty_session::list_open(c)?.len(), 0);

            dispatch::task_insert(
                c,
                &DispatchTaskRow {
                    id: "t-1".into(),
                    project_id: None,
                    title: "t".into(),
                    prompt: "p".into(),
                    task_slug: "t".into(),
                    base_ref: None,
                    worktree_strategy: "per-target".into(),
                    permission_mode: None,
                    timeout_ms: None,
                    created_at: 1,
                    finished_at: None,
                    status: "running".into(),
                },
            )?;
            dispatch::run_insert(c, &run_row("r-1", 1))?;
            assert!(dispatch::run_set_spawn(c, "r-1", Some("s-1"))?);
            assert!(dispatch::run_update_status(c, "r-1", "running", Some("dispatched"))?);
            assert!(dispatch::run_finish(
                c,
                "r-1",
                "done",
                Some("result text"),
                Some("{\"input\":10}"),
                Some(0.42),
                Some("claude-opus-5"),
                None,
                999,
            )?);
            let run = dispatch::run_get(c, "r-1")?.unwrap();
            assert_eq!(run.status, "done");
            assert_eq!(run.text.as_deref(), Some("result text"));
            assert_eq!(run.model_actual.as_deref(), Some("claude-opus-5"));
            assert_eq!(run.cost_usd, Some(0.42));
            assert_eq!(run.note.as_deref(), Some("dispatched"));
            assert_eq!(dispatch::runs_by_status(c, "done")?.len(), 1);
            assert!(dispatch::task_finish(c, "t-1", "done", 1000)?);

            // Cascade: deleting the task removes runs, which removes run_events.
            dispatch::run_event_append(c, "r-1", 5, "text", None)?;
            assert_eq!(dispatch::run_events(c, "r-1", 10)?.len(), 1);
            c.execute("DELETE FROM dispatch_task WHERE id = 't-1'", [])?;
            assert_eq!(dispatch::run_events(c, "r-1", 10)?.len(), 0);
            assert!(dispatch::run_get(c, "r-1")?.is_none());

            agent_binary::upsert(
                c,
                &AgentBinaryRow {
                    kind: "claude".into(),
                    abs_path: Some("/usr/local/bin/claude".into()),
                    version: Some("2.1.0".into()),
                    detected_at: 1,
                    path_fingerprint: Some("abc".into()),
                    verified_range: None,
                    ok: true,
                },
            )?;
            agent_binary::upsert(
                c,
                &AgentBinaryRow {
                    kind: "claude".into(),
                    abs_path: Some("/opt/homebrew/bin/claude".into()),
                    version: Some("2.2.0".into()),
                    detected_at: 2,
                    path_fingerprint: Some("def".into()),
                    verified_range: None,
                    ok: true,
                },
            )?;
            assert_eq!(agent_binary::list(c)?.len(), 1, "upsert is keyed on kind");
            assert_eq!(
                agent_binary::get(c, "claude")?.unwrap().version.as_deref(),
                Some("2.2.0")
            );
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn settings_kv_and_event_log() {
        let db = Db::open_in_memory().unwrap();
        db.blocking(|c| {
            assert!(settings_kv::get(c, "layout")?.is_none());
            settings_kv::set(c, "layout", &serde_json::json!({ "sidebarWidth": 280 }))?;
            settings_kv::set(c, "layout", &serde_json::json!({ "sidebarWidth": 320 }))?;
            assert_eq!(
                settings_kv::get(c, "layout")?.unwrap()["sidebarWidth"],
                serde_json::json!(320)
            );
            assert_eq!(settings_kv::all(c)?.len(), 1);

            // Unparsable JSON is ignored, not fatal.
            c.execute(
                "INSERT INTO settings_kv (key, value_json) VALUES ('bad', '{oops')",
                [],
            )?;
            assert!(settings_kv::get(c, "bad")?.is_none());

            event_log::append(c, 10, "kill", "warn", Some("s-1"), "sent SIGTERM", None)?;
            event_log::append(c, 20, "kill", "info", Some("s-1"), "exited", None)?;
            assert_eq!(event_log::recent(c, 10)?.len(), 2);
            assert_eq!(event_log::by_subject(c, "s-1", 10)?[0].at, 20);
            Ok(())
        })
        .unwrap();
    }

    // -- retention ----------------------------------------------------------

    #[test]
    fn retention_applies_every_policy() {
        let db = Db::open_in_memory().unwrap();
        let now: i64 = 1_000 * DAY;
        db.blocking(move |c| {
            app_instance::insert(c, &instance("i-1", "boot-a"))?;

            // dispatch_task + 3 runs; run-old is outside the "recent runs" window.
            dispatch::task_insert(
                c,
                &DispatchTaskRow {
                    id: "t-1".into(),
                    project_id: None,
                    title: "task".into(),
                    prompt: "do it".into(),
                    task_slug: "task".into(),
                    base_ref: Some("HEAD".into()),
                    worktree_strategy: "per-target".into(),
                    permission_mode: Some("acceptEdits".into()),
                    timeout_ms: Some(45 * 60 * 1000),
                    created_at: now,
                    finished_at: None,
                    status: "running".into(),
                },
            )?;
            for (i, id) in ["run-a", "run-b"].iter().enumerate() {
                dispatch::run_insert(c, &run_row(id, now + i as i64))?;
            }
            // run_event rows: one fresh, one older than 30 days.
            dispatch::run_event_append(c, "run-a", now, "text", Some("{}"))?;
            dispatch::run_event_append(c, "run-a", now - 31 * DAY, "text", Some("{}"))?;

            // proc_sample: one fresh, one 25h old.
            proc_sample::insert(c, now, "session", Some("sess-1"), 100, 1.0)?;
            proc_sample::insert(c, now - 25 * 3600 * 1000, "session", Some("sess-1"), 100, 1.0)?;

            // proc_registry: an 8-day-old exited row and a live one.
            let mut old = planned("s-old", "i-1");
            old.planned_at = now - 9 * DAY;
            proc::insert_planned(c, &old)?;
            proc::mark_exited(c, "s-old", Some(0), None, now - 8 * DAY)?;
            proc::insert_planned(c, &planned("s-live", "i-1"))?;

            // event_log: one fresh, one 31 days old.
            event_log::append(c, now, "kill", "info", None, "fresh", None)?;
            event_log::append(c, now - 31 * DAY, "kill", "info", None, "stale", None)?;

            let report = run_retention(c, now)?;
            assert_eq!(report.run_events_deleted, 1);
            assert_eq!(report.proc_samples_deleted, 1);
            assert_eq!(report.proc_rows_deleted, 1);
            assert_eq!(report.event_log_deleted, 1);
            assert_eq!(report.total(), 4);

            assert_eq!(dispatch::run_events(c, "run-a", 100)?.len(), 1);
            assert_eq!(
                proc_sample::range(c, "session", Some("sess-1"), 0, now + 1)?.len(),
                1
            );
            assert!(proc::get(c, "s-old")?.is_none());
            assert!(proc::get(c, "s-live")?.is_some());
            assert_eq!(event_log::recent(c, 10)?.len(), 1);

            // dispatch_task / dispatch_run are never auto-deleted in M1.
            assert!(dispatch::task_get(c, "t-1")?.is_some());
            assert_eq!(dispatch::runs_of_task(c, "t-1")?.len(), 2);

            // Re-running is a no-op.
            let again = run_retention(c, now)?;
            assert_eq!(again.total(), 0);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn retention_trims_run_events_beyond_the_recent_run_window() {
        let db = Db::open_in_memory().unwrap();
        let now: i64 = 1_000 * DAY;
        db.blocking(move |c| {
            dispatch::task_insert(
                c,
                &DispatchTaskRow {
                    id: "t-1".into(),
                    project_id: None,
                    title: "task".into(),
                    prompt: "p".into(),
                    task_slug: "task".into(),
                    base_ref: None,
                    worktree_strategy: "shared".into(),
                    permission_mode: None,
                    timeout_ms: None,
                    created_at: now,
                    finished_at: None,
                    status: "done".into(),
                },
            )?;
            // 205 runs, each with one recent event. Only the newest 200 runs keep theirs.
            for i in 0..205i64 {
                let id = format!("run-{i:04}");
                dispatch::run_insert(c, &run_row(&id, now - (205 - i)))?;
                dispatch::run_event_append(c, &id, now, "text", None)?;
            }
            let report = run_retention(c, now)?;
            assert_eq!(report.run_events_deleted, 5);
            let remaining: i64 =
                c.query_row("SELECT COUNT(*) FROM run_event", [], |r| r.get(0))?;
            assert_eq!(remaining, 200);
            Ok(())
        })
        .unwrap();
    }

    const DAY: i64 = 24 * 60 * 60 * 1000;

    fn run_row(id: &str, started_at: i64) -> DispatchRunRow {
        DispatchRunRow {
            id: id.to_string(),
            task_id: "t-1".into(),
            vendor: "claude".into(),
            model_requested: None,
            model_actual: None,
            account_id: None,
            worktree_id: None,
            spawn_id: None,
            run_id: None,
            conversation_id: None,
            status: "done".into(),
            note: None,
            route_note: None,
            text: None,
            usage_json: None,
            cost_usd: None,
            started_at: Some(started_at),
            finished_at: Some(started_at),
            error: None,
        }
    }

    // -- async surface ------------------------------------------------------

    #[tokio::test]
    async fn read_write_and_durable_go_through_spawn_blocking() {
        let db = Db::open_in_memory().unwrap();

        db.write(|c| dao::app_instance::insert(c, &instance("i-1", "boot-a")))
            .await
            .unwrap();

        // The registry path: synchronous=FULL for the duration, and only that.
        db.write_durable(|c| {
            let level: i64 = c.query_row("PRAGMA synchronous", [], |r| r.get(0))?;
            assert_eq!(level, 2, "synchronous must be FULL inside write_durable");
            dao::proc::insert_planned(c, &planned("s-1", "i-1"))
        })
        .await
        .unwrap();

        let sync_after: i64 = db
            .read(|c| Ok(c.query_row("PRAGMA synchronous", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(sync_after, 1, "synchronous must be back to NORMAL");

        let rows = db.read(|c| dao::proc::live_rows(c, None)).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, ProcState::Planned);
    }

    #[tokio::test]
    async fn a_failing_closure_surfaces_its_error_and_keeps_the_db_usable() {
        let db = Db::open_in_memory().unwrap();
        let err = db
            .write(|c| dao::proc::insert_planned(c, &planned("s-1", "missing-instance")))
            .await
            .unwrap_err();
        assert!(err.to_string().to_lowercase().contains("foreign key"), "{err}");

        db.write(|c| dao::app_instance::insert(c, &instance("i-1", "boot-a")))
            .await
            .unwrap();
        assert_eq!(
            db.read(|c| dao::app_instance::list_open(c))
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn boot_id_is_readable_on_this_machine() {
        let boot = dao::app_instance::current_boot_id().unwrap();
        if cfg!(target_os = "macos") {
            assert!(
                boot.len() >= 36,
                "kern.bootsessionuuid should be a uuid, got {boot:?}"
            );
        }
    }
}
