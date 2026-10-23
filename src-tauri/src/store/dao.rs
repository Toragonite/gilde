//! Typed queries over `state.db`.
//!
//! Every function here takes a `&Connection` and is therefore **synchronous**.
//! Callers must reach them through [`crate::store::Db::read`] /
//! [`crate::store::Db::write`] / [`crate::store::Db::write_durable`], which run
//! the closure on `tokio::task::spawn_blocking`. Nothing in this module opens a
//! connection or blocks a runtime thread on its own.
//!
//! All `*_at` columns are **epoch milliseconds** (see [`crate::store::now_ms`]).
//! The single exception is `proc_registry.proc_start_ts`, which is the kernel
//! start time in **seconds** because that is what `libproc` reports.

use rusqlite::{params, Connection, OptionalExtension, Params};

use crate::error::Result;
use crate::store::models::*;

// ---------------------------------------------------------------------------
// small helpers
// ---------------------------------------------------------------------------

fn query_rows<T: FromRow, P: Params>(conn: &Connection, sql: &str, args: P) -> Result<Vec<T>> {
    let mut stmt = conn.prepare(sql)?;
    let mapped = stmt.query_map(args, |r| T::from_row(r))?;
    let mut out = Vec::new();
    for row in mapped {
        out.push(row?);
    }
    Ok(out)
}

fn query_opt<T: FromRow, P: Params>(conn: &Connection, sql: &str, args: P) -> Result<Option<T>> {
    let mut stmt = conn.prepare(sql)?;
    let row = stmt.query_row(args, |r| T::from_row(r)).optional()?;
    Ok(row)
}

const fn b(v: bool) -> i64 {
    if v {
        1
    } else {
        0
    }
}

// ---------------------------------------------------------------------------
// app_instance
// ---------------------------------------------------------------------------

pub mod app_instance {
    use super::*;

    pub fn insert(conn: &Connection, row: &AppInstanceRow) -> Result<()> {
        conn.execute(
            "INSERT INTO app_instance (id, pid, boot_id, version, started_at, ended_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                row.id,
                row.pid,
                row.boot_id,
                row.version,
                row.started_at,
                row.ended_at
            ],
        )?;
        Ok(())
    }

    pub fn close(conn: &Connection, id: &str, ended_at: i64) -> Result<bool> {
        let n = conn.execute(
            "UPDATE app_instance SET ended_at = ?2 WHERE id = ?1 AND ended_at IS NULL",
            params![id, ended_at],
        )?;
        Ok(n > 0)
    }

    pub fn get(conn: &Connection, id: &str) -> Result<Option<AppInstanceRow>> {
        query_opt(conn, "SELECT * FROM app_instance WHERE id = ?1", params![id])
    }

    /// Instances that never recorded an `ended_at` — still running, or crashed.
    pub fn list_open(conn: &Connection) -> Result<Vec<AppInstanceRow>> {
        query_rows(
            conn,
            "SELECT * FROM app_instance WHERE ended_at IS NULL ORDER BY started_at",
            params![],
        )
    }

    pub fn list_all(conn: &Connection) -> Result<Vec<AppInstanceRow>> {
        query_rows(conn, "SELECT * FROM app_instance ORDER BY started_at", params![])
    }

    /// `kern.bootsessionuuid` — the fence that makes every PID from a previous
    /// boot meaningless (DESIGN §8-2).
    ///
    /// Read through `sysctlbyname(3)`; no subprocess, so this needs no
    /// `SpawnGuard`.
    /// B5 (contract §2.4): when the window became visible. T-boot-2 asserts no
    /// `proc_registry` row of this instance was planned before it.
    pub fn set_window_visible(conn: &Connection, id: &str, at: i64) -> Result<bool> {
        let n = conn.execute(
            "UPDATE app_instance SET window_visible_at = ?2 WHERE id = ?1 AND window_visible_at IS NULL",
            params![id, at],
        )?;
        Ok(n > 0)
    }

    pub fn window_visible_at(conn: &Connection, id: &str) -> Result<Option<i64>> {
        let v: Option<Option<i64>> = conn
            .query_row(
                "SELECT window_visible_at FROM app_instance WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(v.flatten())
    }

    #[cfg(target_os = "macos")]
    pub fn current_boot_id() -> Result<String> {
        // 36 chars + NUL; a generous buffer costs nothing.
        let mut buf = [0u8; 128];
        let mut len: libc::size_t = buf.len();
        let name = c"kern.bootsessionuuid";
        let rc = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                buf.as_mut_ptr().cast::<libc::c_void>(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(len.min(buf.len()));
        Ok(String::from_utf8_lossy(&buf[..end]).into_owned())
    }

    #[cfg(not(target_os = "macos"))]
    pub fn current_boot_id() -> Result<String> {
        // Only macOS is supported in M1; a stable placeholder keeps the reaper's
        // "different boot" branch from firing on unsupported platforms.
        Ok("unsupported-platform".to_string())
    }
}

// ---------------------------------------------------------------------------
// proc_registry
// ---------------------------------------------------------------------------

pub mod proc {
    use super::*;

    const LIVE_STATES: &str = "('planned','running','adopted')";

    /// Write-before-spawn (DESIGN §8-3). `env_marker` is the spawn id itself.
    pub fn insert_planned(conn: &Connection, p: &NewPlannedProc) -> Result<()> {
        let argv_json = serde_json::to_string(&p.argv).unwrap_or_else(|_| "[]".to_string());
        let sig = argv_signature(&p.argv);
        conn.execute(
            "INSERT INTO proc_registry
               (spawn_id, app_instance, generation, kind, vendor, session_id, run_id,
                state, pid, pgid, proc_start_ts, argv, argv_sig, cwd, env_marker,
                config_dir, worktree_id, keep_alive, planned_at, started_at, exited_at,
                exit_code, exit_signal, note)
             VALUES
               (?1, ?2, ?3, ?4, ?5, ?6, ?7,
                'planned', NULL, NULL, NULL, ?8, ?9, ?10, ?11,
                ?12, ?13, ?14, ?15, NULL, NULL,
                NULL, NULL, ?16)",
            params![
                p.spawn_id,
                p.app_instance,
                p.generation,
                p.kind,
                p.vendor,
                p.session_id,
                p.run_id,
                argv_json,
                sig,
                p.cwd,
                p.spawn_id, // env_marker
                p.config_dir,
                p.worktree_id,
                b(p.keep_alive),
                p.planned_at,
                p.note,
            ],
        )?;
        Ok(())
    }

    pub fn mark_running(
        conn: &Connection,
        spawn_id: &str,
        pid: i64,
        pgid: Option<i64>,
        proc_start_ts: Option<i64>,
        started_at: i64,
    ) -> Result<bool> {
        let n = conn.execute(
            "UPDATE proc_registry
                SET state = 'running', pid = ?2, pgid = ?3, proc_start_ts = ?4, started_at = ?5
              WHERE spawn_id = ?1",
            params![spawn_id, pid, pgid, proc_start_ts, started_at],
        )?;
        Ok(n > 0)
    }

    pub fn mark_exited(
        conn: &Connection,
        spawn_id: &str,
        exit_code: Option<i64>,
        exit_signal: Option<i64>,
        exited_at: i64,
    ) -> Result<bool> {
        let n = conn.execute(
            "UPDATE proc_registry
                SET state = 'exited', exit_code = ?2, exit_signal = ?3, exited_at = ?4
              WHERE spawn_id = ?1",
            params![spawn_id, exit_code, exit_signal, exited_at],
        )?;
        Ok(n > 0)
    }

    /// Force a state. `note` records *why* — every non-obvious transition
    /// (`lost`, `kill_incomplete`, `adopted`) must carry one.
    pub fn mark_state(
        conn: &Connection,
        spawn_id: &str,
        state: ProcState,
        note: Option<&str>,
    ) -> Result<bool> {
        let n = conn.execute(
            "UPDATE proc_registry
                SET state = ?2, note = COALESCE(?3, note)
              WHERE spawn_id = ?1",
            params![spawn_id, state, note],
        )?;
        Ok(n > 0)
    }

    /// Mark every row of `app_instance` that is still live as `state`, without
    /// touching the processes. Used by the reboot branch of the startup sweep.
    pub fn mark_live_rows_of_instance(
        conn: &Connection,
        app_instance: &str,
        state: ProcState,
        note: &str,
    ) -> Result<u64> {
        let sql = format!(
            "UPDATE proc_registry SET state = ?2, note = ?3
              WHERE app_instance = ?1 AND state IN {LIVE_STATES}"
        );
        let n = conn.execute(&sql, params![app_instance, state, note])?;
        Ok(n as u64)
    }

    /// Planned-row adoption / re-parenting after a crash (DESIGN §8-5).
    pub fn adopt(
        conn: &Connection,
        spawn_id: &str,
        app_instance: &str,
        pid: i64,
        pgid: Option<i64>,
        proc_start_ts: Option<i64>,
        note: Option<&str>,
    ) -> Result<bool> {
        let n = conn.execute(
            "UPDATE proc_registry
                SET state = 'adopted', app_instance = ?2, pid = ?3, pgid = ?4,
                    proc_start_ts = ?5, note = COALESCE(?6, note)
              WHERE spawn_id = ?1",
            params![spawn_id, app_instance, pid, pgid, proc_start_ts, note],
        )?;
        Ok(n > 0)
    }

    pub fn set_keep_alive(conn: &Connection, spawn_id: &str, keep_alive: bool) -> Result<bool> {
        let n = conn.execute(
            "UPDATE proc_registry SET keep_alive = ?2 WHERE spawn_id = ?1",
            params![spawn_id, b(keep_alive)],
        )?;
        Ok(n > 0)
    }

    pub fn get(conn: &Connection, spawn_id: &str) -> Result<Option<ProcRow>> {
        query_opt(
            conn,
            "SELECT * FROM proc_registry WHERE spawn_id = ?1",
            params![spawn_id],
        )
    }

    /// Rows that may still have a live process (`planned`/`running`/`adopted`).
    ///
    /// `exclude_app_instance` drops the caller's own instance, which is what the
    /// reaper wants: everything left belongs to a crashed or foreign instance.
    pub fn live_rows(
        conn: &Connection,
        exclude_app_instance: Option<&str>,
    ) -> Result<Vec<ProcRow>> {
        match exclude_app_instance {
            Some(me) => query_rows(
                conn,
                &format!(
                    "SELECT * FROM proc_registry
                      WHERE state IN {LIVE_STATES} AND app_instance <> ?1
                      ORDER BY spawn_id"
                ),
                params![me],
            ),
            None => query_rows(
                conn,
                &format!(
                    "SELECT * FROM proc_registry
                      WHERE state IN {LIVE_STATES}
                      ORDER BY spawn_id"
                ),
                params![],
            ),
        }
    }

    /// Every row that belongs to one instance, in spawn order.
    pub fn rows_of_instance(conn: &Connection, app_instance: &str) -> Result<Vec<ProcRow>> {
        query_rows(
            conn,
            "SELECT * FROM proc_registry WHERE app_instance = ?1 ORDER BY spawn_id",
            params![app_instance],
        )
    }

    /// Rows of one sidecar generation (host spawns are generation 0).
    pub fn rows_of_generation(conn: &Connection, generation: i64) -> Result<Vec<ProcRow>> {
        query_rows(
            conn,
            "SELECT * FROM proc_registry WHERE generation = ?1 ORDER BY spawn_id",
            params![generation],
        )
    }

    /// Live rows of one sidecar generation — the generation sweep input.
    pub fn live_rows_of_generation(conn: &Connection, generation: i64) -> Result<Vec<ProcRow>> {
        query_rows(
            conn,
            &format!(
                "SELECT * FROM proc_registry
                  WHERE generation = ?1 AND state IN {LIVE_STATES}
                  ORDER BY spawn_id"
            ),
            params![generation],
        )
    }

    pub fn by_run_id(conn: &Connection, run_id: &str) -> Result<Vec<ProcRow>> {
        query_rows(
            conn,
            "SELECT * FROM proc_registry WHERE run_id = ?1 ORDER BY spawn_id",
            params![run_id],
        )
    }

    pub fn by_worktree(conn: &Connection, worktree_id: &str) -> Result<Vec<ProcRow>> {
        query_rows(
            conn,
            "SELECT * FROM proc_registry WHERE worktree_id = ?1 ORDER BY spawn_id",
            params![worktree_id],
        )
    }

    pub fn by_session(conn: &Connection, session_id: &str) -> Result<Vec<ProcRow>> {
        query_rows(
            conn,
            "SELECT * FROM proc_registry WHERE session_id = ?1 ORDER BY spawn_id",
            params![session_id],
        )
    }

    /// Live rows whose pid matches — the pid-reuse fence needs the candidate set.
    pub fn by_pid(conn: &Connection, pid: i64) -> Result<Vec<ProcRow>> {
        query_rows(
            conn,
            &format!(
                "SELECT * FROM proc_registry
                  WHERE pid = ?1 AND state IN {LIVE_STATES}
                  ORDER BY spawn_id"
            ),
            params![pid],
        )
    }

    /// The owning session of a row (contract §3.8), written in the same
    /// transaction as the planned row.
    pub fn set_parent_uid(conn: &Connection, spawn_id: &str, uid: &str) -> Result<bool> {
        let n = conn.execute(
            "UPDATE proc_registry SET parent_uid = ?2 WHERE spawn_id = ?1",
            params![spawn_id, uid],
        )?;
        Ok(n > 0)
    }

    pub fn parent_uid(conn: &Connection, spawn_id: &str) -> Result<Option<String>> {
        let v: Option<Option<String>> = conn
            .query_row(
                "SELECT parent_uid FROM proc_registry WHERE spawn_id = ?1",
                [spawn_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(v.flatten())
    }

    /// `spawn_id → parent_uid` for every row of `app_instance` that has one.
    pub fn parents_of_instance(
        conn: &Connection,
        app_instance: &str,
    ) -> Result<Vec<(String, String)>> {
        let mut stmt = conn.prepare(
            "SELECT spawn_id, parent_uid FROM proc_registry
              WHERE app_instance = ?1 AND parent_uid IS NOT NULL",
        )?;
        let rows = stmt.query_map([app_instance], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Every row whose `planned_at` is earlier than `at` for `app_instance`
    /// (T-boot-2's assertion, also logged by the boot timer).
    pub fn planned_before(conn: &Connection, app_instance: &str, at: i64) -> Result<Vec<ProcRow>> {
        query_rows(
            conn,
            "SELECT * FROM proc_registry WHERE app_instance = ?1 AND planned_at < ?2
              ORDER BY planned_at",
            params![app_instance, at],
        )
    }
}

// ---------------------------------------------------------------------------
// project
// ---------------------------------------------------------------------------

pub mod project {
    use super::*;

    /// Insert or update by primary key; `canonical_path` stays unique.
    pub fn upsert(conn: &Connection, row: &ProjectRow) -> Result<()> {
        conn.execute(
            "INSERT INTO project
               (id, path, canonical_path, repo_slug, name, color, default_base_ref,
                added_at, last_opened_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(id) DO UPDATE SET
               path = excluded.path,
               canonical_path = excluded.canonical_path,
               repo_slug = excluded.repo_slug,
               name = excluded.name,
               color = excluded.color,
               default_base_ref = excluded.default_base_ref,
               last_opened_at = excluded.last_opened_at",
            params![
                row.id,
                row.path,
                row.canonical_path,
                row.repo_slug,
                row.name,
                row.color,
                row.default_base_ref,
                row.added_at,
                row.last_opened_at,
            ],
        )?;
        Ok(())
    }

    pub fn get(conn: &Connection, id: &str) -> Result<Option<ProjectRow>> {
        query_opt(conn, "SELECT * FROM project WHERE id = ?1", params![id])
    }

    pub fn get_by_canonical_path(conn: &Connection, path: &str) -> Result<Option<ProjectRow>> {
        query_opt(
            conn,
            "SELECT * FROM project WHERE canonical_path = ?1",
            params![path],
        )
    }

    pub fn list(conn: &Connection) -> Result<Vec<ProjectRow>> {
        query_rows(
            conn,
            "SELECT * FROM project ORDER BY COALESCE(last_opened_at, added_at) DESC",
            params![],
        )
    }

    pub fn touch_opened(conn: &Connection, id: &str, at: i64) -> Result<bool> {
        let n = conn.execute(
            "UPDATE project SET last_opened_at = ?2 WHERE id = ?1",
            params![id, at],
        )?;
        Ok(n > 0)
    }

    pub fn remove(conn: &Connection, id: &str) -> Result<bool> {
        let n = conn.execute("DELETE FROM project WHERE id = ?1", [id])?;
        Ok(n > 0)
    }
}

// ---------------------------------------------------------------------------
// worktree
// ---------------------------------------------------------------------------

pub mod worktree {
    use super::*;

    pub fn insert(conn: &Connection, row: &WorktreeRow) -> Result<()> {
        conn.execute(
            "INSERT INTO worktree
               (id, project_id, slug, branch, path, base_sha, origin, created_at,
                removed_at, locked, verified_at, post_create_cmd,
                cached_dirty, cached_added, cached_removed, cached_ahead, cached_behind)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
            params![
                row.id,
                row.project_id,
                row.slug,
                row.branch,
                row.path,
                row.base_sha,
                row.origin,
                row.created_at,
                row.removed_at,
                b(row.locked),
                row.verified_at,
                row.post_create_cmd,
                row.cached_dirty,
                row.cached_added,
                row.cached_removed,
                row.cached_ahead,
                row.cached_behind,
            ],
        )?;
        Ok(())
    }

    pub fn get(conn: &Connection, id: &str) -> Result<Option<WorktreeRow>> {
        query_opt(conn, "SELECT * FROM worktree WHERE id = ?1", params![id])
    }

    pub fn get_by_path(conn: &Connection, path: &str) -> Result<Option<WorktreeRow>> {
        query_opt(
            conn,
            "SELECT * FROM worktree WHERE path = ?1 AND removed_at IS NULL",
            params![path],
        )
    }

    pub fn list_live(conn: &Connection) -> Result<Vec<WorktreeRow>> {
        query_rows(
            conn,
            "SELECT * FROM worktree WHERE removed_at IS NULL ORDER BY created_at",
            params![],
        )
    }

    pub fn list_by_project(
        conn: &Connection,
        project_id: &str,
        include_removed: bool,
    ) -> Result<Vec<WorktreeRow>> {
        if include_removed {
            query_rows(
                conn,
                "SELECT * FROM worktree WHERE project_id = ?1 ORDER BY created_at",
                params![project_id],
            )
        } else {
            query_rows(
                conn,
                "SELECT * FROM worktree
                  WHERE project_id = ?1 AND removed_at IS NULL ORDER BY created_at",
                params![project_id],
            )
        }
    }

    pub fn mark_removed(conn: &Connection, id: &str, removed_at: i64) -> Result<bool> {
        let n = conn.execute(
            "UPDATE worktree SET removed_at = ?2 WHERE id = ?1",
            params![id, removed_at],
        )?;
        Ok(n > 0)
    }

    pub fn set_locked(conn: &Connection, id: &str, locked: bool) -> Result<bool> {
        let n = conn.execute(
            "UPDATE worktree SET locked = ?2 WHERE id = ?1",
            params![id, b(locked)],
        )?;
        Ok(n > 0)
    }

    pub fn set_verified(conn: &Connection, id: &str, verified_at: i64) -> Result<bool> {
        let n = conn.execute(
            "UPDATE worktree SET verified_at = ?2 WHERE id = ?1",
            params![id, verified_at],
        )?;
        Ok(n > 0)
    }

    pub fn set_post_create_cmd(conn: &Connection, id: &str, cmd: Option<&str>) -> Result<bool> {
        let n = conn.execute(
            "UPDATE worktree SET post_create_cmd = ?2 WHERE id = ?1",
            params![id, cmd],
        )?;
        Ok(n > 0)
    }

    /// Cache the five counters `worktree_stats` computed (DESIGN §7-5).
    pub fn update_stats_cache(
        conn: &Connection,
        id: &str,
        stats: &WorktreeStatsCache,
    ) -> Result<bool> {
        let n = conn.execute(
            "UPDATE worktree
                SET cached_dirty = ?2, cached_added = ?3, cached_removed = ?4,
                    cached_ahead = ?5, cached_behind = ?6
              WHERE id = ?1",
            params![
                id,
                stats.dirty,
                stats.added,
                stats.removed,
                stats.ahead,
                stats.behind
            ],
        )?;
        Ok(n > 0)
    }
}

// ---------------------------------------------------------------------------
// worktree_alias
// ---------------------------------------------------------------------------

pub mod worktree_alias {
    use super::*;

    pub fn insert(conn: &Connection, row: &WorktreeAliasRow) -> Result<()> {
        conn.execute(
            "INSERT INTO worktree_alias
               (id, worktree_id, provider, alias_path, target_path, kind, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                row.id,
                row.worktree_id,
                row.provider,
                row.alias_path,
                row.target_path,
                row.kind,
                row.created_at,
            ],
        )?;
        Ok(())
    }

    pub fn list_by_worktree(conn: &Connection, worktree_id: &str) -> Result<Vec<WorktreeAliasRow>> {
        query_rows(
            conn,
            "SELECT * FROM worktree_alias WHERE worktree_id = ?1 ORDER BY created_at",
            params![worktree_id],
        )
    }

    pub fn get_by_alias_path(conn: &Connection, alias_path: &str) -> Result<Option<WorktreeAliasRow>> {
        query_opt(
            conn,
            "SELECT * FROM worktree_alias WHERE alias_path = ?1",
            params![alias_path],
        )
    }

    pub fn delete(conn: &Connection, id: &str) -> Result<bool> {
        let n = conn.execute("DELETE FROM worktree_alias WHERE id = ?1", [id])?;
        Ok(n > 0)
    }

    pub fn delete_for_worktree(conn: &Connection, worktree_id: &str) -> Result<u64> {
        let n = conn.execute(
            "DELETE FROM worktree_alias WHERE worktree_id = ?1",
            [worktree_id],
        )?;
        Ok(n as u64)
    }
}

// ---------------------------------------------------------------------------
// pty_session
// ---------------------------------------------------------------------------

pub mod pty_session {
    use super::*;

    pub fn insert(conn: &Connection, row: &PtySessionRow) -> Result<()> {
        conn.execute(
            // No title: the column is gone since v2 (A12.4).
            "INSERT INTO pty_session
               (id, spawn_id, project_id, worktree_id, agent_kind, account_id, cwd,
                cols, rows, argv, opened_at, closed_at, exit_code, exit_signal,
                last_io_at, last_status, detached)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
            params![
                row.id,
                row.spawn_id,
                row.project_id,
                row.worktree_id,
                row.agent_kind,
                row.account_id,
                row.cwd,
                row.cols,
                row.rows,
                row.argv,
                row.opened_at,
                row.closed_at,
                row.exit_code,
                row.exit_signal,
                row.last_io_at,
                row.last_status,
                b(row.detached),
            ],
        )?;
        Ok(())
    }

    pub fn get(conn: &Connection, id: &str) -> Result<Option<PtySessionRow>> {
        query_opt(conn, "SELECT * FROM pty_session WHERE id = ?1", params![id])
    }

    pub fn list_open(conn: &Connection) -> Result<Vec<PtySessionRow>> {
        query_rows(
            conn,
            "SELECT * FROM pty_session WHERE closed_at IS NULL ORDER BY opened_at",
            params![],
        )
    }

    pub fn list_by_worktree(conn: &Connection, worktree_id: &str) -> Result<Vec<PtySessionRow>> {
        query_rows(
            conn,
            "SELECT * FROM pty_session WHERE worktree_id = ?1 ORDER BY opened_at",
            params![worktree_id],
        )
    }

    pub fn mark_closed(
        conn: &Connection,
        id: &str,
        closed_at: i64,
        exit_code: Option<i64>,
        exit_signal: Option<i64>,
    ) -> Result<bool> {
        let n = conn.execute(
            "UPDATE pty_session
                SET closed_at = ?2, exit_code = ?3, exit_signal = ?4
              WHERE id = ?1",
            params![id, closed_at, exit_code, exit_signal],
        )?;
        Ok(n > 0)
    }

    /// Last byte seen in either direction — the input to idle detection.
    pub fn touch_io(conn: &Connection, id: &str, at: i64) -> Result<bool> {
        let n = conn.execute(
            "UPDATE pty_session SET last_io_at = ?2 WHERE id = ?1",
            params![id, at],
        )?;
        Ok(n > 0)
    }

    pub fn set_status(conn: &Connection, id: &str, status: &str) -> Result<bool> {
        let n = conn.execute(
            "UPDATE pty_session SET last_status = ?2 WHERE id = ?1",
            params![id, status],
        )?;
        Ok(n > 0)
    }

    pub fn set_detached(conn: &Connection, id: &str, detached: bool) -> Result<bool> {
        let n = conn.execute(
            "UPDATE pty_session SET detached = ?2 WHERE id = ?1",
            params![id, b(detached)],
        )?;
        Ok(n > 0)
    }
}

// ---------------------------------------------------------------------------
// dispatch (task / run / run_event)
// ---------------------------------------------------------------------------

pub mod dispatch {
    use super::*;

    pub fn task_insert(conn: &Connection, row: &DispatchTaskRow) -> Result<()> {
        conn.execute(
            "INSERT INTO dispatch_task
               (id, project_id, title, prompt, task_slug, base_ref, worktree_strategy,
                permission_mode, timeout_ms, created_at, finished_at, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                row.id,
                row.project_id,
                row.title,
                row.prompt,
                row.task_slug,
                row.base_ref,
                row.worktree_strategy,
                row.permission_mode,
                row.timeout_ms,
                row.created_at,
                row.finished_at,
                row.status,
            ],
        )?;
        Ok(())
    }

    pub fn task_get(conn: &Connection, id: &str) -> Result<Option<DispatchTaskRow>> {
        query_opt(conn, "SELECT * FROM dispatch_task WHERE id = ?1", params![id])
    }

    pub fn task_list(conn: &Connection, limit: i64) -> Result<Vec<DispatchTaskRow>> {
        query_rows(
            conn,
            "SELECT * FROM dispatch_task ORDER BY created_at DESC LIMIT ?1",
            params![limit],
        )
    }

    pub fn task_set_status(conn: &Connection, id: &str, status: &str) -> Result<bool> {
        let n = conn.execute(
            "UPDATE dispatch_task SET status = ?2 WHERE id = ?1",
            params![id, status],
        )?;
        Ok(n > 0)
    }

    pub fn task_finish(
        conn: &Connection,
        id: &str,
        status: &str,
        finished_at: i64,
    ) -> Result<bool> {
        let n = conn.execute(
            "UPDATE dispatch_task SET status = ?2, finished_at = ?3 WHERE id = ?1",
            params![id, status, finished_at],
        )?;
        Ok(n > 0)
    }

    pub fn run_insert(conn: &Connection, row: &DispatchRunRow) -> Result<()> {
        conn.execute(
            "INSERT INTO dispatch_run
               (id, task_id, vendor, model_requested, model_actual, account_id, worktree_id,
                spawn_id, run_id, conversation_id, status, note, route_note, text,
                usage_json, cost_usd, started_at, finished_at, error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
            params![
                row.id,
                row.task_id,
                row.vendor,
                row.model_requested,
                row.model_actual,
                row.account_id,
                row.worktree_id,
                row.spawn_id,
                row.run_id,
                row.conversation_id,
                row.status,
                row.note,
                row.route_note,
                row.text,
                row.usage_json,
                row.cost_usd,
                row.started_at,
                row.finished_at,
                row.error,
            ],
        )?;
        Ok(())
    }

    pub fn run_get(conn: &Connection, id: &str) -> Result<Option<DispatchRunRow>> {
        query_opt(conn, "SELECT * FROM dispatch_run WHERE id = ?1", params![id])
    }

    pub fn runs_of_task(conn: &Connection, task_id: &str) -> Result<Vec<DispatchRunRow>> {
        query_rows(
            conn,
            "SELECT * FROM dispatch_run WHERE task_id = ?1 ORDER BY id",
            params![task_id],
        )
    }

    pub fn runs_by_status(conn: &Connection, status: &str) -> Result<Vec<DispatchRunRow>> {
        query_rows(
            conn,
            "SELECT * FROM dispatch_run WHERE status = ?1 ORDER BY id",
            params![status],
        )
    }

    pub fn run_update_status(
        conn: &Connection,
        id: &str,
        status: &str,
        note: Option<&str>,
    ) -> Result<bool> {
        let n = conn.execute(
            "UPDATE dispatch_run SET status = ?2, note = COALESCE(?3, note) WHERE id = ?1",
            params![id, status, note],
        )?;
        Ok(n > 0)
    }

    pub fn run_set_spawn(conn: &Connection, id: &str, spawn_id: Option<&str>) -> Result<bool> {
        let n = conn.execute(
            "UPDATE dispatch_run SET spawn_id = ?2 WHERE id = ?1",
            params![id, spawn_id],
        )?;
        Ok(n > 0)
    }

    pub fn run_set_started(conn: &Connection, id: &str, started_at: i64) -> Result<bool> {
        let n = conn.execute(
            "UPDATE dispatch_run SET started_at = ?2 WHERE id = ?1",
            params![id, started_at],
        )?;
        Ok(n > 0)
    }

    /// Terminal update for one run. `text` may be a `runs/<run_id>.txt` pointer
    /// once the verbatim result exceeds 4 MiB (DESIGN §10-2).
    #[allow(clippy::too_many_arguments)]
    pub fn run_finish(
        conn: &Connection,
        id: &str,
        status: &str,
        text: Option<&str>,
        usage_json: Option<&str>,
        cost_usd: Option<f64>,
        model_actual: Option<&str>,
        error: Option<&str>,
        finished_at: i64,
    ) -> Result<bool> {
        let n = conn.execute(
            "UPDATE dispatch_run
                SET status = ?2, text = ?3, usage_json = ?4, cost_usd = ?5,
                    model_actual = COALESCE(?6, model_actual), error = ?7, finished_at = ?8
              WHERE id = ?1",
            params![
                id,
                status,
                text,
                usage_json,
                cost_usd,
                model_actual,
                error,
                finished_at
            ],
        )?;
        Ok(n > 0)
    }

    /// Mirror of one stream event. The file on disk is the record of truth;
    /// this row exists so the UI can query without reading `stream.jsonl`.
    pub fn run_event_append(
        conn: &Connection,
        run_id: &str,
        at: i64,
        kind: &str,
        payload: Option<&str>,
    ) -> Result<i64> {
        conn.execute(
            "INSERT INTO run_event (run_id, at, kind, payload) VALUES (?1, ?2, ?3, ?4)",
            params![run_id, at, kind, payload],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn run_events(conn: &Connection, run_id: &str, limit: i64) -> Result<Vec<RunEventRow>> {
        query_rows(
            conn,
            "SELECT * FROM run_event WHERE run_id = ?1 ORDER BY at, id LIMIT ?2",
            params![run_id, limit],
        )
    }
}

// ---------------------------------------------------------------------------
// account
// ---------------------------------------------------------------------------

pub mod account {
    use super::*;

    /// Idempotent import key is the config dir (DESIGN §10-5): the same
    /// `~/.claude-<name>` directory is always the same account slot.
    ///
    /// Rows whose `config_dir` is NULL (the synthetic `ambient` account) are
    /// matched by `id` instead. The stored `id`, `imported_at` and the
    /// user-owned `enabled` / `is_default` flags survive a re-import.
    pub fn upsert_by_config_dir(conn: &Connection, row: &AccountRow) -> Result<AccountRow> {
        let existing: Option<AccountRow> = match row.config_dir.as_deref() {
            Some(dir) => query_opt(
                conn,
                "SELECT * FROM account WHERE config_dir = ?1",
                params![dir],
            )?,
            None => query_opt(conn, "SELECT * FROM account WHERE id = ?1", params![row.id])?,
        };

        match existing {
            Some(prev) => {
                conn.execute(
                    "UPDATE account
                        SET vendor = ?2, label = ?3, model_hint = ?4, source = ?5,
                            last_seen_at = ?6
                      WHERE id = ?1",
                    params![
                        prev.id,
                        row.vendor,
                        row.label,
                        row.model_hint,
                        row.source,
                        row.last_seen_at,
                    ],
                )?;
                Ok(get(conn, &prev.id)?.unwrap_or(prev))
            }
            None => {
                conn.execute(
                    "INSERT INTO account
                       (id, vendor, label, config_dir, model_hint, source, enabled, is_default,
                        imported_at, last_seen_at, last_quota_json, last_quota_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                    params![
                        row.id,
                        row.vendor,
                        row.label,
                        row.config_dir,
                        row.model_hint,
                        row.source,
                        b(row.enabled),
                        b(row.is_default),
                        row.imported_at,
                        row.last_seen_at,
                        row.last_quota_json,
                        row.last_quota_at,
                    ],
                )?;
                Ok(row.clone())
            }
        }
    }

    pub fn get(conn: &Connection, id: &str) -> Result<Option<AccountRow>> {
        query_opt(conn, "SELECT * FROM account WHERE id = ?1", params![id])
    }

    pub fn list(conn: &Connection) -> Result<Vec<AccountRow>> {
        query_rows(
            conn,
            "SELECT * FROM account ORDER BY vendor, is_default DESC, label",
            params![],
        )
    }

    pub fn list_by_vendor(conn: &Connection, vendor: &str) -> Result<Vec<AccountRow>> {
        query_rows(
            conn,
            "SELECT * FROM account WHERE vendor = ?1 ORDER BY is_default DESC, label",
            params![vendor],
        )
    }

    pub fn set_enabled(conn: &Connection, id: &str, enabled: bool) -> Result<bool> {
        let n = conn.execute(
            "UPDATE account SET enabled = ?2 WHERE id = ?1",
            params![id, b(enabled)],
        )?;
        Ok(n > 0)
    }

    /// Exactly one default per vendor.
    pub fn set_default(conn: &Connection, id: &str) -> Result<bool> {
        let tx = conn.unchecked_transaction()?;
        let vendor: Option<String> = tx
            .query_row("SELECT vendor FROM account WHERE id = ?1", [id], |r| r.get(0))
            .optional()?;
        let Some(vendor) = vendor else {
            return Ok(false);
        };
        tx.execute(
            "UPDATE account SET is_default = 0 WHERE vendor = ?1",
            [&vendor],
        )?;
        tx.execute("UPDATE account SET is_default = 1 WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(true)
    }

    pub fn touch_seen(conn: &Connection, id: &str, at: i64) -> Result<bool> {
        let n = conn.execute(
            "UPDATE account SET last_seen_at = ?2 WHERE id = ?1",
            params![id, at],
        )?;
        Ok(n > 0)
    }

    /// M2 quota slot — stored verbatim, never interpreted in M1.
    pub fn set_quota(conn: &Connection, id: &str, quota_json: &str, at: i64) -> Result<bool> {
        let n = conn.execute(
            "UPDATE account SET last_quota_json = ?2, last_quota_at = ?3 WHERE id = ?1",
            params![id, quota_json, at],
        )?;
        Ok(n > 0)
    }
}

// ---------------------------------------------------------------------------
// agent_binary
// ---------------------------------------------------------------------------

pub mod agent_binary {
    use super::*;

    pub fn upsert(conn: &Connection, row: &AgentBinaryRow) -> Result<()> {
        conn.execute(
            "INSERT INTO agent_binary
               (kind, abs_path, version, detected_at, path_fingerprint, verified_range, ok)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(kind) DO UPDATE SET
               abs_path = excluded.abs_path,
               version = excluded.version,
               detected_at = excluded.detected_at,
               path_fingerprint = excluded.path_fingerprint,
               verified_range = excluded.verified_range,
               ok = excluded.ok",
            params![
                row.kind,
                row.abs_path,
                row.version,
                row.detected_at,
                row.path_fingerprint,
                row.verified_range,
                b(row.ok),
            ],
        )?;
        Ok(())
    }

    pub fn get(conn: &Connection, kind: &str) -> Result<Option<AgentBinaryRow>> {
        query_opt(conn, "SELECT * FROM agent_binary WHERE kind = ?1", params![kind])
    }

    pub fn list(conn: &Connection) -> Result<Vec<AgentBinaryRow>> {
        query_rows(conn, "SELECT * FROM agent_binary ORDER BY kind", params![])
    }
}

// ---------------------------------------------------------------------------
// event_log
// ---------------------------------------------------------------------------

pub mod event_log {
    use super::*;

    /// The kill audit trail (DESIGN §10-4). Never drop a row on the floor for
    /// convenience — a kill with no `event_log` line is an unexplained kill.
    pub fn append(
        conn: &Connection,
        at: i64,
        category: &str,
        level: &str,
        subject_id: Option<&str>,
        message: &str,
        detail_json: Option<&str>,
    ) -> Result<i64> {
        conn.execute(
            "INSERT INTO event_log (at, category, level, subject_id, message, detail_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![at, category, level, subject_id, message, detail_json],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn recent(conn: &Connection, limit: i64) -> Result<Vec<EventLogRow>> {
        query_rows(
            conn,
            "SELECT * FROM event_log ORDER BY at DESC, id DESC LIMIT ?1",
            params![limit],
        )
    }

    pub fn by_subject(conn: &Connection, subject_id: &str, limit: i64) -> Result<Vec<EventLogRow>> {
        query_rows(
            conn,
            "SELECT * FROM event_log WHERE subject_id = ?1 ORDER BY at DESC, id DESC LIMIT ?2",
            params![subject_id, limit],
        )
    }

    /// Drop rows older than `before`, then trim to the newest `max_rows`.
    pub fn prune(conn: &Connection, before: i64, max_rows: i64) -> Result<u64> {
        let mut deleted = conn.execute("DELETE FROM event_log WHERE at < ?1", [before])? as u64;
        deleted += conn.execute(
            "DELETE FROM event_log
              WHERE id NOT IN (SELECT id FROM event_log ORDER BY at DESC, id DESC LIMIT ?1)",
            [max_rows],
        )? as u64;
        Ok(deleted)
    }
}

// ---------------------------------------------------------------------------
// proc_sample
// ---------------------------------------------------------------------------

pub mod proc_sample {
    use super::*;

    pub fn insert(
        conn: &Connection,
        at: i64,
        scope: &str,
        scope_id: Option<&str>,
        rss_bytes: i64,
        cpu_pct: f64,
    ) -> Result<()> {
        conn.execute(
            "INSERT INTO proc_sample (at, scope, scope_id, rss_bytes, cpu_pct)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![at, scope, scope_id, rss_bytes, cpu_pct],
        )?;
        Ok(())
    }

    /// Sparkline backfill: `[from, to)` for one scope, oldest first.
    pub fn range(
        conn: &Connection,
        scope: &str,
        scope_id: Option<&str>,
        from: i64,
        to: i64,
    ) -> Result<Vec<ProcSampleRow>> {
        match scope_id {
            Some(id) => query_rows(
                conn,
                "SELECT * FROM proc_sample
                  WHERE scope = ?1 AND scope_id = ?2 AND at >= ?3 AND at < ?4
                  ORDER BY at",
                params![scope, id, from, to],
            ),
            None => query_rows(
                conn,
                "SELECT * FROM proc_sample
                  WHERE scope = ?1 AND scope_id IS NULL AND at >= ?2 AND at < ?3
                  ORDER BY at",
                params![scope, from, to],
            ),
        }
    }

    pub fn prune(conn: &Connection, before: i64) -> Result<u64> {
        let n = conn.execute("DELETE FROM proc_sample WHERE at < ?1", [before])?;
        Ok(n as u64)
    }
}

// ---------------------------------------------------------------------------
// settings_kv
// ---------------------------------------------------------------------------

pub mod settings_kv {
    use super::*;

    /// Returns `None` both when the key is absent and when the stored text is
    /// not valid JSON — a corrupted panel layout must not block a boot.
    pub fn get(conn: &Connection, key: &str) -> Result<Option<serde_json::Value>> {
        let raw: Option<String> = conn
            .query_row("SELECT value_json FROM settings_kv WHERE key = ?1", [key], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(raw.and_then(|s| match serde_json::from_str(&s) {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!(key, error = %e, "settings_kv value is not valid JSON; ignoring");
                None
            }
        }))
    }

    pub fn set(conn: &Connection, key: &str, value: &serde_json::Value) -> Result<()> {
        conn.execute(
            "INSERT INTO settings_kv (key, value_json) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value_json = excluded.value_json",
            params![key, value.to_string()],
        )?;
        Ok(())
    }

    pub fn delete(conn: &Connection, key: &str) -> Result<bool> {
        let n = conn.execute("DELETE FROM settings_kv WHERE key = ?1", [key])?;
        Ok(n > 0)
    }

    pub fn all(conn: &Connection) -> Result<Vec<(String, String)>> {
        let mut stmt = conn.prepare("SELECT key, value_json FROM settings_kv ORDER BY key")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// quit_record / quit_record_item (contract §3.9, §10.2–§10.3)
// ---------------------------------------------------------------------------

pub mod quit_record {
    use super::*;

    /// Unresolved records expire after 7 days.
    pub const EXPIRY_MS: i64 = 7 * 24 * 60 * 60 * 1000;

    /// The record and every item in **one** transaction. Callers run it through
    /// `Db::write_durable` (`synchronous=FULL`, §3.9 *Durability*).
    pub fn insert(conn: &Connection, record: &QuitRecordRow, items: &[QuitItemRow]) -> Result<()> {
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO quit_record (id, cause, created_at, resolved_at) VALUES (?1, ?2, ?3, ?4)",
            params![record.id, record.cause, record.created_at, record.resolved_at],
        )?;
        for item in items {
            tx.execute(
                "INSERT OR REPLACE INTO quit_record_item
                   (record_id, uid, control_mode, permission_mode, model, was_waiting, team_lead, survivor)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    record.id,
                    item.uid,
                    item.control_mode,
                    item.permission_mode,
                    item.model,
                    item.was_waiting,
                    b(item.team_lead),
                    b(item.survivor),
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get(conn: &Connection, id: &str) -> Result<Option<QuitRecordRow>> {
        query_opt(conn, "SELECT * FROM quit_record WHERE id = ?1", [id])
    }

    /// Unresolved, unexpired records, newest first.
    pub fn list_open(conn: &Connection, now: i64) -> Result<Vec<QuitRecordRow>> {
        query_rows(
            conn,
            "SELECT * FROM quit_record WHERE resolved_at IS NULL AND created_at >= ?1
              ORDER BY created_at DESC",
            [now - EXPIRY_MS],
        )
    }

    /// Items in quit order (insertion order).
    pub fn items(conn: &Connection, record_id: &str) -> Result<Vec<QuitItemRow>> {
        query_rows(
            conn,
            "SELECT * FROM quit_record_item WHERE record_id = ?1 ORDER BY rowid",
            [record_id],
        )
    }

    pub fn mark_survivor(conn: &Connection, record_id: &str, uid: &str) -> Result<bool> {
        let n = conn.execute(
            "UPDATE quit_record_item SET survivor = 1 WHERE record_id = ?1 AND uid = ?2",
            params![record_id, uid],
        )?;
        Ok(n > 0)
    }

    /// Continued or dismissed: the record is deleted (its items cascade).
    pub fn delete(conn: &Connection, id: &str) -> Result<bool> {
        conn.execute("DELETE FROM quit_record_item WHERE record_id = ?1", [id])?;
        let n = conn.execute("DELETE FROM quit_record WHERE id = ?1", [id])?;
        Ok(n > 0)
    }

    /// Remove one item (it was continued); deletes the record when it empties.
    pub fn remove_item(conn: &Connection, record_id: &str, uid: &str) -> Result<bool> {
        let n = conn.execute(
            "DELETE FROM quit_record_item WHERE record_id = ?1 AND uid = ?2",
            params![record_id, uid],
        )?;
        let left: i64 = conn.query_row(
            "SELECT COUNT(*) FROM quit_record_item WHERE record_id = ?1",
            [record_id],
            |r| r.get(0),
        )?;
        if left == 0 {
            conn.execute("DELETE FROM quit_record WHERE id = ?1", [record_id])?;
        }
        Ok(n > 0)
    }
}

// ---------------------------------------------------------------------------
// claude_compat_observed (contract §3.9, §3.10) — the table; T21 owns the rules
// ---------------------------------------------------------------------------

pub mod compat_observed {
    use super::*;

    /// Upsert one observation keyed `(version, binary_fingerprint)` [F-22].
    pub fn upsert(conn: &Connection, row: &CompatObservedRow) -> Result<()> {
        conn.execute(
            "INSERT INTO claude_compat_observed
               (version, conversation, pane_hosting, source, binary_fingerprint, static_result,
                handshake_ok, features_json, note, observed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT (version, binary_fingerprint) DO UPDATE SET
               conversation = excluded.conversation, pane_hosting = excluded.pane_hosting,
               source = excluded.source, static_result = excluded.static_result,
               handshake_ok = excluded.handshake_ok, features_json = excluded.features_json,
               note = excluded.note, observed_at = excluded.observed_at",
            params![
                row.version,
                row.conversation,
                row.pane_hosting,
                row.source,
                row.binary_fingerprint,
                row.static_result,
                row.handshake_ok.map(b),
                row.features_json,
                row.note,
                row.observed_at,
            ],
        )?;
        Ok(())
    }

    pub fn list(conn: &Connection) -> Result<Vec<CompatObservedRow>> {
        query_rows(
            conn,
            "SELECT * FROM claude_compat_observed ORDER BY version, binary_fingerprint",
            [],
        )
    }
}

// ---------------------------------------------------------------------------
// UI state (`settings_kv` under `ui.*`, contract §3.9, §4.2.1 `ui_state_set`)
// ---------------------------------------------------------------------------

pub mod ui_state {
    use super::*;

    /// The `settings_kv` key prefix for UI state.
    pub const PREFIX: &str = "ui.";

    pub fn set(conn: &Connection, key: &str, value: &serde_json::Value) -> Result<()> {
        settings_kv::set(conn, &format!("{PREFIX}{key}"), value)
    }

    /// Every `ui.*` entry, keyed without the prefix.
    pub fn all(conn: &Connection) -> Result<Vec<(String, serde_json::Value)>> {
        let mut stmt = conn.prepare(
            "SELECT key, value_json FROM settings_kv WHERE key LIKE 'ui.%' ORDER BY key",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (key, json) = row?;
            if let Ok(value) = serde_json::from_str(&json) {
                out.push((key[PREFIX.len()..].to_string(), value));
            }
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// retention (DESIGN §10-4, run once at boot)
// ---------------------------------------------------------------------------

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// `run_event`: 30 days **or** the most recent 200 runs, whichever keeps less.
pub const RUN_EVENT_MAX_AGE_MS: i64 = 30 * DAY_MS;
pub const RUN_EVENT_MAX_RUNS: i64 = 200;
/// `proc_sample`: 24 hours (≈17k rows/day at a 5 s interval).
pub const PROC_SAMPLE_MAX_AGE_MS: i64 = DAY_MS;
/// Terminal `proc_registry` rows: 7 days.
pub const PROC_ROW_MAX_AGE_MS: i64 = 7 * DAY_MS;
/// `event_log`: 30 days or 50k rows.
pub const EVENT_LOG_MAX_AGE_MS: i64 = 30 * DAY_MS;
pub const EVENT_LOG_MAX_ROWS: i64 = 50_000;

/// Apply the boot-time retention policy. `dispatch_task` / `dispatch_run` are
/// deliberately never auto-deleted in M1.
pub fn run_retention(conn: &Connection, now: i64) -> Result<RetentionReport> {
    let tx = conn.unchecked_transaction()?;

    // `run_event` is legacy since Gilde 2 (contract §3.9): never read or
    // written, so retention leaves it alone too.
    let run_events_deleted = 0u64;

    // Unresolved quit records expire after 7 days (contract §3.9, §10.2).
    tx.execute(
        "DELETE FROM quit_record_item WHERE record_id IN
           (SELECT id FROM quit_record WHERE resolved_at IS NULL AND created_at < ?1)",
        [now - quit_record::EXPIRY_MS],
    )?;
    tx.execute(
        "DELETE FROM quit_record WHERE resolved_at IS NULL AND created_at < ?1",
        [now - quit_record::EXPIRY_MS],
    )?;

    let proc_samples_deleted = tx.execute(
        "DELETE FROM proc_sample WHERE at < ?1",
        [now - PROC_SAMPLE_MAX_AGE_MS],
    )? as u64;

    let proc_rows_deleted = tx.execute(
        "DELETE FROM proc_registry
          WHERE state IN ('exited','killed','kill_incomplete','lost')
            AND COALESCE(exited_at, started_at, planned_at) < ?1",
        [now - PROC_ROW_MAX_AGE_MS],
    )? as u64;

    let mut event_log_deleted = tx.execute(
        "DELETE FROM event_log WHERE at < ?1",
        [now - EVENT_LOG_MAX_AGE_MS],
    )? as u64;
    event_log_deleted += tx.execute(
        "DELETE FROM event_log
          WHERE id NOT IN (SELECT id FROM event_log ORDER BY at DESC, id DESC LIMIT ?1)",
        [EVENT_LOG_MAX_ROWS],
    )? as u64;

    tx.commit()?;
    Ok(RetentionReport {
        run_events_deleted,
        proc_samples_deleted,
        proc_rows_deleted,
        event_log_deleted,
    })
}
