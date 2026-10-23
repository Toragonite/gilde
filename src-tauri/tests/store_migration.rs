//! `state.db` migration v2 (contract §3.9) and the schema privacy scan
//! (T-priv-1), plus the registry half of T-priv-argv (§9.7).
//!
//! Owner: T09.

mod fixtures;

use std::sync::Arc;

use gilde_code_lib::config::ConfigStore;
use gilde_code_lib::store::migrations::{self, DROPPED_IN_V2, EXPECTED_TABLES, LEGACY_TABLES};
use gilde_code_lib::store::{dao, Db};
use gilde_code_lib::supervise::registry::Supervisor;
use rusqlite::Connection;

fn columns(conn: &Connection, table: &str) -> Vec<(String, bool)> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .expect("table_info");
    stmt.query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, i64>(5)? > 0)))
        .expect("rows")
        .map(|r| r.expect("row"))
        .collect()
}

fn tables(conn: &Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .expect("tables");
    stmt.query_map([], |r| r.get::<_, String>(0))
        .expect("rows")
        .map(|r| r.expect("row"))
        .collect()
}

/// The T-priv-1 rule (§3.9): a column whose name is, or has an `_`-separated
/// part that is, one of the content/secret words, or that ends in `_name` /
/// `_title` — other than primary/foreign keys ending in `_key`/`_id`, primary
/// keys, and the allowlist `teammate_name`, `team_name`, `label`.
fn is_banned(column: &str, primary_key: bool) -> bool {
    const WORDS: [&str; 17] = [
        "title", "text", "prompt", "content", "body", "preview", "summary", "subject", "blocks",
        "raw", "token", "secret", "key", "name", "argv", "command", "env",
    ];
    const ALLOW: [&str; 3] = ["teammate_name", "team_name", "label"];
    if ALLOW.contains(&column) || primary_key {
        return false;
    }
    if column.ends_with("_key") || column.ends_with("_id") {
        return false;
    }
    column.ends_with("_name")
        || column.ends_with("_title")
        || column.split('_').any(|part| WORDS.contains(&part))
}

/// Columns the scan reports that the contract itself keeps, each with why.
const EXEMPT: &[(&str, &str, &str)] = &[
    ("proc_registry", "argv", "contract §9.7: the redacted argv (realpath + redacted args) [F-03]"),
    ("proc_registry", "argv_sig", "sha256 of the argv, the identity fence; no argv text"),
    ("proc_registry", "env_marker", "the GILDE_SPAWN_ID value Gilde minted; no environment value"),
    ("pty_session", "argv", "kept PTY lane (T06): stored through the same redaction"),
    ("project", "name", "the folder's basename (metadata, §3.4.2)"),
];

#[test]
fn v1_to_v2_on_a_real_file_keeps_metadata_and_drops_content_tables() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("state.db");
    {
        let conn = Connection::open(&path).expect("open");
        assert_eq!(migrations::apply_through(&conn, 1, 1).expect("v1"), 1);
        conn.execute_batch(
            "INSERT INTO conversation (id, source, source_session_id, source_locator, title,
               created_at, updated_at, resumability, imported_at, schema_variant)
             VALUES ('c1','claude','s','f','A CONFIDENTIAL TITLE',1,1,'native_resume',1,'cc-2.1');
             INSERT INTO message (id, conversation_id, seq, role, blocks, timestamp)
             VALUES ('m1','c1',0,'user','[\"A CONFIDENTIAL MESSAGE\"]',1);
             INSERT INTO app_instance (id, pid, boot_id, version, started_at, ended_at)
             VALUES ('i1', 1, 'b', 'old', 1, 2);
             INSERT INTO account (id, vendor, label, source, imported_at) VALUES ('a1','claude','Work','manual',1);",
        )
        .expect("v1 rows");
    }
    // Boot opens it: the migration runs once.
    let started = std::time::Instant::now();
    let db = Db::open(&path).expect("db open migrates");
    // §2.4 B2 budgets 200 ms; a functional test only asserts a 10× ceiling
    // (A10.7), the budget itself is a G5 benchmark.
    assert!(started.elapsed() < std::time::Duration::from_secs(2), "migration v2 within 10× its 200 ms budget");
    assert_eq!(db.schema_version().expect("version"), 2);
    db.blocking(|conn| {
        let names = tables(conn);
        for gone in DROPPED_IN_V2 {
            assert!(!names.iter().any(|t| t == gone), "{gone} must be dropped");
        }
        for t in EXPECTED_TABLES {
            assert!(names.iter().any(|n| n == t), "missing table {t}");
        }
        assert_eq!(names.len(), EXPECTED_TABLES.len(), "unexpected tables: {names:?}");
        let label: String = conn.query_row("SELECT label FROM account WHERE id='a1'", [], |r| r.get(0))?;
        assert_eq!(label, "Work");
        let ended: Option<i64> =
            conn.query_row("SELECT ended_at FROM app_instance WHERE id='i1'", [], |r| r.get(0))?;
        assert_eq!(ended, Some(2));
        let cols: Vec<String> = columns(conn, "proc_registry").into_iter().map(|c| c.0).collect();
        assert!(cols.contains(&"parent_uid".to_string()));
        let cols: Vec<String> = columns(conn, "app_instance").into_iter().map(|c| c.0).collect();
        assert!(cols.contains(&"window_visible_at".to_string()));
        Ok(())
    })
    .expect("checks");
    drop(db);
    // Nothing of the content survives in the file.
    let bytes = std::fs::read(&path).expect("read db file");
    let hay = String::from_utf8_lossy(&bytes);
    assert!(!hay.contains("A CONFIDENTIAL MESSAGE"), "message content left in the file");
    // Re-opening is a no-op.
    let db = Db::open(&path).expect("reopen");
    assert_eq!(db.schema_version().expect("version"), 2);
}

/// A12.4: a v1 file whose `pty_session.title` holds a value migrates to a
/// schema without the column, and the value is gone from the bytes of the
/// database file and its WAL (the v1 rows sit in WAL frames here, as they
/// would after a crash).
#[test]
fn v2_drops_the_pty_title_and_its_bytes() {
    const CANARY: &str = "A12-4 CANARY TERMINAL TITLE 5f1d";
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("state.db");
    let wal = dir.path().join("state.db-wal");

    // A v1 file in WAL mode; this connection stays open so the rows are only
    // in WAL frames when the app opens the file.
    let v1 = Connection::open(&path).expect("open");
    let mode: String = v1.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0)).expect("wal");
    assert_eq!(mode, "wal");
    assert_eq!(migrations::apply_through(&v1, 1, 1).expect("v1"), 1);
    v1.execute(
        "INSERT INTO pty_session (id, agent_kind, title, cwd, cols, rows, argv, opened_at)
         VALUES ('p1', 'claude', ?1, '/tmp', 80, 24, '[]', 1)",
        [CANARY],
    )
    .expect("v1 row");
    let in_wal = std::fs::read(&wal).expect("wal file");
    assert!(
        String::from_utf8_lossy(&in_wal).contains(CANARY),
        "the fixture puts the title into the WAL"
    );

    let started = std::time::Instant::now();
    let db = Db::open(&path).expect("db open migrates");
    assert!(started.elapsed() < std::time::Duration::from_secs(2), "within 10× the 200 ms budget");
    assert_eq!(db.schema_version().expect("version"), 2);
    db.blocking(|conn| {
        let cols: Vec<String> = columns(conn, "pty_session").into_iter().map(|c| c.0).collect();
        assert!(!cols.contains(&"title".to_string()), "the column is gone: {cols:?}");
        let cwd: String = conn.query_row("SELECT cwd FROM pty_session WHERE id='p1'", [], |r| r.get(0))?;
        assert_eq!(cwd, "/tmp", "the row's metadata is kept");
        let row = dao::pty_session::get(conn, "p1")?.expect("row");
        assert_eq!(row.title, "", "nothing reads a title back");
        // What any later writer leaves in the WAL is checkpointed too.
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        Ok(())
    })
    .expect("checks");
    drop(db);
    drop(v1);

    for file in [&path, &wal] {
        let Ok(bytes) = std::fs::read(file) else { continue };
        assert!(
            !String::from_utf8_lossy(&bytes).contains(CANARY),
            "the title survives in {}",
            file.display()
        );
    }
}

#[test]
fn t_priv_1_no_content_or_secret_named_columns() {
    let db = Db::open_in_memory().expect("db");
    db.blocking(|conn| {
        let mut violations = Vec::new();
        for table in tables(conn) {
            if LEGACY_TABLES.contains(&table.as_str()) || table == "schema_migrations" {
                continue;
            }
            for (col, pk) in columns(conn, &table) {
                if !is_banned(&col, pk) {
                    continue;
                }
                if EXEMPT.iter().any(|(t, c, _)| *t == table && *c == col) {
                    continue;
                }
                violations.push(format!("{table}.{col}"));
            }
        }
        assert!(violations.is_empty(), "content- or secret-named columns: {violations:?}");
        Ok(())
    })
    .expect("scan");
}

#[test]
fn the_privacy_rule_itself() {
    assert!(is_banned("title", false));
    assert!(is_banned("first_prompt", false));
    assert!(is_banned("user_name", false));
    assert!(is_banned("page_title", false));
    assert!(!is_banned("teammate_name", false));
    assert!(!is_banned("team_key", false));
    assert!(!is_banned("vendor_session_id", false));
    assert!(!is_banned("label", false));
    assert!(!is_banned("key", true));
}

#[tokio::test]
async fn t_priv_argv_the_registry_stores_only_the_redacted_argv() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::open_in_memory().expect("db");
    let config = Arc::new(ConfigStore::open_at(&dir.path().join("config.json")).expect("config"));
    let sup = Supervisor::open_instance(&db, config, "0.0.0-test").await.expect("instance");
    let hostile: Vec<String> = [
        "/bin/sh",
        "-c",
        "cd /tmp && env AWS_SECRET_ACCESS_KEY=AKIAIOSFODNN7CANARYvalue ANTHROPIC_BASE_URL=https://canary.example \
         /x/claude --settings '{\"apiKeyHelper\":\"canary-helper\"}' -p 'canary prompt text'",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    // Through the one spawn path (A12.8/A22.3): `/bin/sh -c` is a pane.
    let guard = sup
        .plan_guarded(gilde_code_lib::contract::services::SpawnRequest {
            kind: gilde_code_lib::contract::process::ProcKind::Pane,
            argv0: gilde_code_lib::contract::common::AbsPath::parse(hostile[0].clone()).expect("abs"),
            args: hostile[1..].to_vec(),
            cwd: gilde_code_lib::contract::common::AbsPath::parse("/tmp").ok(),
            env_keys: Vec::new(),
            parent_uid: Some(
                gilde_code_lib::contract::ids::SessionUid::parse("s_01890a5d-ac96-774b-bcce-b302099a8057").expect("uid"),
            ),
            generation: None,
            turn_id: None,
        })
        .await
        .expect("plan");
    let id = guard.spawn_id().to_string();
    let row = db
        .read(move |conn| dao::proc::get(conn, &id))
        .await
        .expect("read")
        .expect("row");
    for canary in ["AKIAIOSFODNN7CANARYvalue", "canary.example", "canary-helper", "canary prompt text"] {
        assert!(!row.argv.contains(canary), "{canary} reached proc_registry.argv: {}", row.argv);
    }
    assert!(row.argv.contains("AWS_SECRET_ACCESS_KEY=***"), "the key name stays: {}", row.argv);
    let id = guard.spawn_id().to_string();
    let parent = db
        .read(move |conn| dao::proc::parent_uid(conn, &id))
        .await
        .expect("parent");
    assert_eq!(parent.as_deref(), Some("s_01890a5d-ac96-774b-bcce-b302099a8057"));
    guard.abandon("test");
}
