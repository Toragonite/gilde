//! Agy summaries and processes (contract §5.5) [R2 Q1, Q2].
//!
//! * `~/.gemini/antigravity-cli/conversation_summaries.db` through the SQLite
//!   rule: `SELECT id, title, workspace_uris, last_modified_time,
//!   last_user_input_time, status, killed, not_fully_idle FROM
//!   conversation_summaries` — never `preview`; `title` is **[content]**.
//! * `cache/last_conversations.json` (cwd → id) read as JSON, in memory.
//! * `conversations/` is listed for names only (the resume check, §3.2).
//! * `presence/*.lock` existence is **not** liveness; `log/` is never watched;
//!   `oauth_creds.json` and `antigravity-oauth-token*` are never opened.
//! * Processes: `agy --conversation <id>` → that id; otherwise cwd only →
//!   Status unknown{ProcessOnly}. `AGY_HOME` is never set (agy ignores it).

use std::collections::{HashMap, HashSet};
use std::path::Path;

use rusqlite::types::Value as SqlValue;

use crate::contract::common::EpochMs;
use crate::discovery::claude::transcripts::parse_iso8601_ms;
use crate::discovery::is_vendor_uuid;
use crate::discovery::safe_read::{self, EntryKind, ReadError, Root};
use crate::discovery::sqlite::{self, SqliteError};

pub const LAST_CONVERSATIONS_CAP: u64 = 1024 * 1024;

/// One summary row. `title` is **[content]**.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SummaryRow {
    pub id: String,
    pub title: Option<String>,
    /// Workspace paths from the `file:` URIs.
    pub workspaces: Vec<String>,
    pub last_modified: Option<EpochMs>,
    pub last_user_input: Option<EpochMs>,
    pub status: Option<String>,
    pub killed: Option<bool>,
    pub not_fully_idle: Option<bool>,
}

/// A time column: integer s / ms, or an ISO string.
fn time_of(v: SqlValue) -> Option<EpochMs> {
    match v {
        SqlValue::Integer(n) if n > 100_000_000_000 => Some(n),
        SqlValue::Integer(n) if n > 0 => Some(n * 1000),
        SqlValue::Real(f) if f > 0.0 => Some(if f > 1e11 { f as i64 } else { (f * 1000.0) as i64 }),
        SqlValue::Text(t) => parse_iso8601_ms(&t),
        _ => None,
    }
}

fn bool_of(v: SqlValue) -> Option<bool> {
    match v {
        SqlValue::Integer(n) => Some(n != 0),
        _ => None,
    }
}

fn text_of(v: SqlValue) -> Option<String> {
    match v {
        SqlValue::Text(t) => Some(t),
        _ => None,
    }
}

/// `["file:///a/b", …]` → paths.
pub fn workspace_paths(raw: &str) -> Vec<String> {
    let Ok(list) = serde_json::from_str::<Vec<String>>(raw) else { return Vec::new() };
    list.iter()
        .filter_map(|u| url::Url::parse(u).ok())
        .filter(|u| u.scheme() == "file")
        .filter_map(|u| u.to_file_path().ok())
        .filter_map(|p| p.to_str().map(str::to_string))
        .take(16)
        .collect()
}

/// Query the summaries index.
pub fn query_summaries(root: &Root, db: &Path, app_data: &Path) -> Result<Vec<SummaryRow>, SqliteError> {
    sqlite::with_vendor_db(root, db, app_data, |c| {
        let mut st = c.prepare(
            "SELECT id, title, workspace_uris, last_modified_time, last_user_input_time, status, killed, not_fully_idle FROM conversation_summaries",
        )?;
        let rows = st.query_map([], |r| {
            Ok(SummaryRow {
                id: text_of(r.get(0)?).unwrap_or_default(),
                title: text_of(r.get(1)?),
                workspaces: text_of(r.get(2)?).map(|w| workspace_paths(&w)).unwrap_or_default(),
                last_modified: time_of(r.get(3)?),
                last_user_input: time_of(r.get(4)?),
                status: text_of(r.get(5)?).filter(|s| s.len() <= 64),
                killed: bool_of(r.get(6)?),
                not_fully_idle: bool_of(r.get(7)?),
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            let row = row?;
            if is_vendor_uuid(&row.id) {
                out.push(row);
            }
        }
        Ok(out)
    })
}

/// `cache/last_conversations.json` (cwd → id), in memory.
pub fn read_last_conversations(agy: &Root) -> Result<HashMap<String, String>, ReadError> {
    let path = agy.join("cache/last_conversations.json");
    let (bytes, _) = match safe_read::read_file(agy, &path, LAST_CONVERSATIONS_CAP) {
        Ok(v) => v,
        Err(ReadError::NotFound) => return Ok(HashMap::new()),
        Err(e) => return Err(e),
    };
    let map: HashMap<String, serde_json::Value> = serde_json::from_slice(&bytes).unwrap_or_default();
    Ok(map
        .into_iter()
        .filter_map(|(cwd, v)| {
            let id = match v {
                serde_json::Value::String(s) => s,
                serde_json::Value::Object(o) => o.get("id").and_then(|x| x.as_str()).map(str::to_string)?,
                _ => return None,
            };
            is_vendor_uuid(&id).then_some((cwd, id))
        })
        .collect())
}

/// `conversations/` names (no content).
pub fn list_conversations(agy: &Root) -> HashSet<String> {
    let dir = agy.join("conversations");
    let Ok(l) = safe_read::list_dir(agy, &dir, safe_read::DIR_ENTRIES_CAP) else { return HashSet::new() };
    l.entries
        .into_iter()
        .filter(|e| e.kind == EntryKind::File)
        .filter_map(|e| e.name.strip_suffix(".db").map(str::to_string))
        .filter(|id| is_vendor_uuid(id))
        .collect()
}

/// The resume check of §3.2: a UUID whose `conversations/<id>.db` exists.
pub fn conversation_exists(agy_dir: &Path, id: &str) -> bool {
    is_vendor_uuid(id)
        && safe_read::lstat(&agy_dir.join("conversations").join(format!("{id}.db"))).is_ok_and(|s| s.kind == EntryKind::File)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris_and_times() {
        assert_eq!(workspace_paths(r#"["file:///Users/u/p%20q","https://x"]"#), vec!["/Users/u/p q".to_string()]);
        assert_eq!(time_of(SqlValue::Integer(1_792_558_922)), Some(1_792_558_922_000));
        assert_eq!(time_of(SqlValue::Integer(1_792_558_922_000)), Some(1_792_558_922_000));
        assert_eq!(time_of(SqlValue::Null), None);
    }
}
