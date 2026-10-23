//! Codex rollouts, thread index and processes (contract §5.3) [R2 Q1–Q4].
//!
//! * Rollouts `<CODEX_HOME or ~/.codex>/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`:
//!   line 1 `session_meta` (`id, cwd, source, originator, cli_version,
//!   timestamp`), the tail for the last item time.
//! * Index: `state_5.sqlite` through the SQLite rule, at most every 30 s when
//!   its size/mtime or its `-wal` changed: `SELECT id, cwd, title,
//!   first_user_message, created_at_ms, updated_at_ms, source, archived FROM
//!   threads` (`title`/`first_user_message` are **[content]**, memory only).
//!   `auth.json` is never opened.
//! * Processes: `app-server` (a VS Code child) → VsCode origin, thread unknown;
//!   `codex resume <id>` → that id; plain TUI → cwd only. An unmapped live
//!   session is Status unknown{NoThreadMap}; mapping heuristics are not shipped.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::contract::common::EpochMs;
use crate::discovery::claude::transcripts::parse_iso8601_ms;
use crate::discovery::is_vendor_uuid;
use crate::discovery::safe_read::{self, EntryKind, FileId, ReadError, Root};
use crate::discovery::sqlite::{self, SqliteError};

pub const ROLLOUT_HEAD_CAP: usize = 64 * 1024;
pub const ROLLOUT_TAIL_CAP: usize = 256 * 1024;
/// The index is re-queried at most every 30 s (§5.3).
pub const INDEX_MIN_INTERVAL_MS: u64 = 30_000;

/// `rollout-<ts>-<uuid>.jsonl` → the uuid.
pub fn rollout_id(name: &str) -> Option<String> {
    let stem = name.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
    if stem.len() < 37 {
        return None;
    }
    let id = &stem[stem.len() - 36..];
    (is_vendor_uuid(id) && stem.as_bytes()[stem.len() - 37] == b'-').then(|| id.to_string())
}

/// Line 1 of a rollout.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RolloutHead {
    pub id: String,
    pub cwd: Option<String>,
    pub source: Option<String>,
    pub originator: Option<String>,
    pub cli_version: Option<String>,
    pub started_at: Option<EpochMs>,
}

#[derive(Deserialize)]
struct RawLine {
    #[serde(rename = "type")]
    kind: Option<String>,
    timestamp: Option<String>,
    payload: Option<RawMeta>,
}

#[derive(Deserialize)]
struct RawMeta {
    id: Option<String>,
    cwd: Option<String>,
    source: Option<serde_json::Value>,
    originator: Option<String>,
    cli_version: Option<String>,
    timestamp: Option<String>,
}

/// Parse a `session_meta` line.
pub fn parse_session_meta(line: &[u8]) -> Option<RolloutHead> {
    let r: RawLine = serde_json::from_slice(line).ok()?;
    if r.kind.as_deref() != Some("session_meta") {
        return None;
    }
    let p = r.payload?;
    let id = p.id.filter(|i| is_vendor_uuid(i))?;
    let source = match p.source {
        Some(serde_json::Value::String(s)) => Some(s),
        Some(serde_json::Value::Object(_)) => Some("other".to_string()),
        _ => None,
    };
    Some(RolloutHead {
        id,
        cwd: p.cwd.filter(|c| c.len() <= 4096),
        source: source.filter(|s| s.len() <= 32),
        originator: p.originator.filter(|s| s.len() <= 64),
        cli_version: p.cli_version.filter(|s| s.len() <= 32),
        started_at: p.timestamp.or(r.timestamp).as_deref().and_then(parse_iso8601_ms),
    })
}

#[derive(Deserialize)]
struct TsOnly {
    timestamp: Option<String>,
}

/// Read a rollout's head (line 1) and the last item time (tail).
pub fn read_rollout(root: &Root, path: &Path) -> Result<(Option<RolloutHead>, Option<EpochMs>, FileId), ReadError> {
    let f = safe_read::open_in(root, path)?;
    let head = f.read_range(0, ROLLOUT_HEAD_CAP)?;
    let meta = memchr::memchr(b'\n', &head).and_then(|nl| parse_session_meta(&head[..nl]));
    let size = f.id.size;
    let start = size.saturating_sub(ROLLOUT_TAIL_CAP as u64);
    let tail = f.read_range(start, (size - start) as usize)?;
    let mut last = None;
    let complete = match memchr::memrchr(b'\n', &tail) {
        Some(nl) => &tail[..nl],
        None => &tail[..0],
    };
    for line in complete.rsplit(|b| *b == b'\n') {
        if line.is_empty() {
            continue;
        }
        if let Ok(t) = serde_json::from_slice::<TsOnly>(line) {
            if let Some(ts) = t.timestamp.as_deref().and_then(parse_iso8601_ms) {
                last = Some(ts);
                break;
            }
        }
    }
    Ok((meta, last, f.id))
}

/// One rollout file found by the walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolloutFile {
    pub path: PathBuf,
    pub id: String,
    pub file: FileId,
}

/// Walk `sessions/YYYY/MM/DD/` (depth 3) for rollouts. Returns files and
/// whether a listing cap was hit.
pub fn list_rollouts(sessions: &Root, cap: usize) -> Result<(Vec<RolloutFile>, bool), ReadError> {
    let mut out = Vec::new();
    let mut truncated = false;
    let digits = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_digit());
    let years = safe_read::list_dir(sessions, sessions.path(), safe_read::DIR_ENTRIES_CAP)?;
    truncated |= years.truncated;
    for y in years.entries.iter().filter(|e| e.kind == EntryKind::Dir && digits(&e.name, 4)) {
        let yd = sessions.join(&y.name);
        let Ok(months) = safe_read::list_dir(sessions, &yd, safe_read::DIR_ENTRIES_CAP) else { continue };
        for m in months.entries.iter().filter(|e| e.kind == EntryKind::Dir && digits(&e.name, 2)) {
            let md = yd.join(&m.name);
            let Ok(days) = safe_read::list_dir(sessions, &md, safe_read::DIR_ENTRIES_CAP) else { continue };
            for d in days.entries.iter().filter(|e| e.kind == EntryKind::Dir && digits(&e.name, 2)) {
                let dd = md.join(&d.name);
                let Ok(files) = safe_read::list_dir(sessions, &dd, safe_read::DIR_ENTRIES_CAP) else { continue };
                truncated |= files.truncated;
                for f in files.entries.iter().filter(|e| e.kind == EntryKind::File) {
                    let Some(id) = rollout_id(&f.name) else { continue };
                    let path = dd.join(&f.name);
                    if let Ok(st) = safe_read::lstat(&path) {
                        out.push(RolloutFile { path, id, file: st.id });
                    }
                }
            }
        }
    }
    if out.len() > cap {
        out.sort_by_key(|f| std::cmp::Reverse(f.file.mtime_ms));
        out.truncate(cap);
        truncated = true;
    }
    Ok((out, truncated))
}

/// One `threads` row. `title` and `first_user_message` are **[content]**.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ThreadRow {
    pub id: String,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub first_user_message: Option<String>,
    pub created_at: Option<EpochMs>,
    pub updated_at: Option<EpochMs>,
    pub source: Option<String>,
    pub archived: bool,
}

/// `SELECT … FROM threads` through the SQLite rule.
pub fn query_threads(root: &Root, db: &Path, app_data: &Path) -> Result<Vec<ThreadRow>, SqliteError> {
    sqlite::with_vendor_db(root, db, app_data, |c| {
        let mut st = c.prepare(
            "SELECT id, cwd, title, first_user_message, created_at_ms, updated_at_ms, source, archived FROM threads",
        )?;
        let rows = st.query_map([], |r| {
            Ok(ThreadRow {
                id: r.get::<_, String>(0)?,
                cwd: r.get::<_, Option<String>>(1)?,
                title: r.get::<_, Option<String>>(2)?,
                first_user_message: r.get::<_, Option<String>>(3)?,
                created_at: r.get::<_, Option<i64>>(4)?,
                updated_at: r.get::<_, Option<i64>>(5)?,
                source: r.get::<_, Option<String>>(6)?,
                archived: r.get::<_, Option<i64>>(7)?.unwrap_or(0) != 0,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollout_names_and_meta() {
        assert_eq!(
            rollout_id("rollout-2026-10-21T13-25-05-0e2c0b1a-6b7d-4c51-9a8e-1f2e3d4c5b6a.jsonl").as_deref(),
            Some("0e2c0b1a-6b7d-4c51-9a8e-1f2e3d4c5b6a")
        );
        assert_eq!(rollout_id("rollout-x.jsonl"), None);
        let m = parse_session_meta(br#"{"timestamp":"2026-10-21T04:25:05.000Z","type":"session_meta","payload":{"id":"0e2c0b1a-6b7d-4c51-9a8e-1f2e3d4c5b6a","cwd":"/w","source":"vscode","originator":"codex_vscode","cli_version":"0.50.0","instructions":"CANARY","timestamp":"2026-10-21T04:25:05.000Z"}}"#).unwrap();
        assert_eq!(m.source.as_deref(), Some("vscode"));
        assert!(m.started_at.is_some());
        assert!(!format!("{m:?}").contains("CANARY"));
        assert!(parse_session_meta(br#"{"type":"event_msg"}"#).is_none());
    }
}
