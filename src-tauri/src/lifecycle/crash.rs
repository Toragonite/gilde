//! Crash recovery (§10.1, §10.3) and the launch kind of `app_bootstrap`.
//!
//! Owner: T09.
//!
//! A previous instance that never closed its `app_instance` row crashed (or
//! was killed, or the machine lost power): the single-instance lock is held by
//! now, so no open row can belong to a running sibling of this `GILDE_HOME`.
//! Its registry rows that were Gilde sessions (`convo`, `pty`, `pane`,
//! `attach`, `turn` with a session id) become a `quit_record{cause:'crash'}` —
//! unless that instance wrote a quit record of its own before it died — so
//! "Continue all" works exactly as after a quit (§10.1). An item whose crash
//! leftover is still alive is kept: Continue waits for it (§6.5 rule 1, F-12).
//!
//! The record's id is minted while `setup()` runs (B2–B4: reads only, a few
//! ms) so that `app_bootstrap.launch` can name it from memory at B5; the boot
//! sweep (B6) writes it.

use std::collections::{HashMap, HashSet};

use rusqlite::Connection;

use crate::contract::ids::QuitRecordId;
use crate::contract::ipc::LaunchInfo;
use crate::error::Result;
use crate::lifecycle::quit::{control_mode_of_kind, session_of};
use crate::store::dao;
use crate::store::models::{QuitItemRow, CAUSE_QUIT};

/// "Restore" within this long after the last clean exit, Home after it [M2].
pub const RESTORE_WINDOW_MS: i64 = 8 * 60 * 60 * 1000;

/// One crash record to write in B6.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrashPlan {
    pub instance_id: String,
    pub record_id: String,
    pub items: Vec<QuitItemRow>,
}

/// Plan the crash records for every open `app_instance` (all of them crashed:
/// this process holds the instance lock and has not inserted its own row).
pub fn plan_crash_records(conn: &Connection) -> Result<Vec<CrashPlan>> {
    let mut plans = Vec::new();
    for inst in dao::app_instance::list_open(conn)? {
        if wrote_quit_record_after(conn, inst.started_at)? {
            continue;
        }
        let rows = dao::proc::rows_of_instance(conn, &inst.id)?;
        let parents: HashMap<String, String> =
            dao::proc::parents_of_instance(conn, &inst.id)?.into_iter().collect();
        let mut seen = HashSet::new();
        let mut items = Vec::new();
        let mut rows = rows;
        rows.sort_by_key(|r| r.planned_at);
        for row in rows.iter().filter(|r| r.kind.is_session_kind()) {
            let Some(uid) = session_of(row, &parents) else { continue };
            if !seen.insert(uid.clone()) {
                continue;
            }
            let mut item = QuitItemRow {
                uid: uid.clone(),
                control_mode: control_mode_of_kind(row.kind).to_string(),
                permission_mode: None,
                model: None,
                was_waiting: None,
                team_lead: false,
                survivor: false,
            };
            enrich_from_session_meta(conn, &mut item)?;
            items.push(item);
        }
        if items.is_empty() {
            continue;
        }
        plans.push(CrashPlan {
            instance_id: inst.id.clone(),
            record_id: QuitRecordId::mint().into_string(),
            items,
        });
    }
    Ok(plans)
}

/// Whether a quit record was written after `since` (an instance that quit
/// cleanly up to the exit and died on the way out).
fn wrote_quit_record_after(conn: &Connection, since: i64) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM quit_record WHERE cause = ?1 AND created_at >= ?2",
        rusqlite::params![CAUSE_QUIT, since],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// The control mode, permission mode and model a session last ran with, when
/// `session_meta` knows them (metadata only, §3.9).
fn enrich_from_session_meta(conn: &Connection, item: &mut QuitItemRow) -> Result<()> {
    use rusqlite::OptionalExtension as _;
    type MetaRow = (Option<String>, Option<String>, Option<String>, Option<String>, Option<String>);
    let row: Option<MetaRow> = conn
        .query_row(
            "SELECT control_mode_last, permission_mode_last, model_last, team_key, agent_id
               FROM session_meta WHERE uid = ?1",
            [&item.uid],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    if let Some((mode, perm, model, team_key, agent_id)) = row {
        if let Some(mode) = mode.filter(|m| matches!(m.as_str(), "conversation" | "terminal" | "turnBased")) {
            item.control_mode = mode;
        }
        item.permission_mode = perm;
        item.model = model;
        // A session with a team and no agent id of its own is the team's lead.
        item.team_lead = team_key.is_some() && agent_id.is_none();
    }
    Ok(())
}

/// `app_bootstrap.launch` (§4.2.1) [ux-ia §2.7, M2, M10].
pub fn launch_info(conn: &Connection, crash_plans: &[CrashPlan], now: i64) -> Result<LaunchInfo> {
    if let Some(first) = crash_plans.first() {
        if let Ok(id) = QuitRecordId::parse(first.record_id.clone()) {
            let count: usize = crash_plans.iter().map(|p| p.items.len()).sum();
            return Ok(LaunchInfo::AfterCrash {
                record_id: id,
                count: count as u32,
            });
        }
    }
    for record in dao::quit_record::list_open(conn, now)? {
        let items = dao::quit_record::items(conn, &record.id)?;
        if items.is_empty() {
            continue;
        }
        if let Ok(id) = QuitRecordId::parse(record.id.clone()) {
            let count = items.len() as u32;
            return Ok(if record.cause == CAUSE_QUIT {
                LaunchInfo::AfterQuit { record_id: id, count }
            } else {
                LaunchInfo::AfterCrash { record_id: id, count }
            });
        }
    }
    let instances = dao::app_instance::list_all(conn)?;
    if instances.is_empty() {
        return Ok(LaunchInfo::FirstLaunch);
    }
    let last_end = instances.iter().filter_map(|i| i.ended_at).max();
    Ok(match last_end {
        Some(end) if now - end > RESTORE_WINDOW_MS => LaunchInfo::HomeAfter8h,
        _ => LaunchInfo::Restore,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::models::{AppInstanceRow, NewPlannedProc, ProcKind};
    use crate::store::Db;

    fn db() -> Db {
        Db::open_in_memory().expect("db")
    }

    fn instance(conn: &Connection, id: &str, started: i64, ended: Option<i64>) {
        dao::app_instance::insert(
            conn,
            &AppInstanceRow {
                id: id.into(),
                pid: 1,
                boot_id: "b".into(),
                version: "t".into(),
                started_at: started,
                ended_at: ended,
            },
        )
        .unwrap();
    }

    fn row(conn: &Connection, inst: &str, spawn: &str, kind: ProcKind, session: Option<&str>, parent: Option<&str>) {
        let mut p = NewPlannedProc::new(spawn, inst, kind, vec!["/x/claude".into()], "/tmp", 10);
        p.session_id = session.map(String::from);
        dao::proc::insert_planned(conn, &p).unwrap();
        if let Some(uid) = parent {
            dao::proc::set_parent_uid(conn, spawn, uid).unwrap();
        }
    }

    const U1: &str = "s_01890a5d-ac96-774b-bcce-b302099a8057";
    const U2: &str = "s_01890a5d-ac96-774b-bcce-b302099a8058";

    #[test]
    fn a_crashed_instance_becomes_one_record_per_distinct_session() {
        let db = db();
        db.blocking(|conn| {
            instance(conn, "dead", 100, None);
            row(conn, "dead", "a", ProcKind::Convo, None, Some(U1));
            row(conn, "dead", "b", ProcKind::Pane, None, Some(U1));
            row(conn, "dead", "c", ProcKind::Pty, Some(U2), None);
            row(conn, "dead", "d", ProcKind::Sidecar, None, None);
            row(conn, "dead", "e", ProcKind::Git, Some("x"), None);
            let plans = plan_crash_records(conn)?;
            assert_eq!(plans.len(), 1);
            let uids: Vec<&str> = plans[0].items.iter().map(|i| i.uid.as_str()).collect();
            assert_eq!(uids, vec![U1, U2]);
            assert_eq!(plans[0].items[0].control_mode, "conversation");
            assert_eq!(plans[0].items[1].control_mode, "terminal");
            match launch_info(conn, &plans, 1_000)? {
                LaunchInfo::AfterCrash { count, record_id } => {
                    assert_eq!(count, 2);
                    assert_eq!(record_id.as_str(), plans[0].record_id);
                }
                other => panic!("{other:?}"),
            }
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn launch_kinds() {
        let db = db();
        db.blocking(|conn| {
            assert_eq!(launch_info(conn, &[], 1_000)?, LaunchInfo::FirstLaunch);
            instance(conn, "old", 100, Some(200));
            assert_eq!(launch_info(conn, &[], 300)?, LaunchInfo::Restore);
            assert_eq!(
                launch_info(conn, &[], 200 + RESTORE_WINDOW_MS + 1)?,
                LaunchInfo::HomeAfter8h
            );
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn an_instance_that_wrote_a_quit_record_gets_no_crash_record() {
        let db = db();
        db.blocking(|conn| {
            instance(conn, "dying", 100, None);
            row(conn, "dying", "a", ProcKind::Convo, None, Some(U1));
            let rec = crate::store::models::QuitRecordRow {
                id: QuitRecordId::mint().into_string(),
                cause: CAUSE_QUIT.into(),
                created_at: 150,
                resolved_at: None,
            };
            let item = QuitItemRow {
                uid: U1.into(),
                control_mode: "conversation".into(),
                permission_mode: None,
                model: None,
                was_waiting: Some("approval".into()),
                team_lead: false,
                survivor: false,
            };
            dao::quit_record::insert(conn, &rec, &[item])?;
            assert!(plan_crash_records(conn)?.is_empty());
            match launch_info(conn, &[], 1_000)? {
                LaunchInfo::AfterQuit { count, .. } => assert_eq!(count, 1),
                other => panic!("{other:?}"),
            }
            Ok(())
        })
        .unwrap();
    }
}
