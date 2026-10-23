//! Background jobs (contract §5.2.6) — **frozen** for 2.1.307 [R6 §6 LP-22].
//!
//! `kind:"bg"` registry entries are Background sessions, `jobId ==
//! sessionId[:8]`. Extra state comes from `<cfg>/jobs/<jobId>/state.json`
//! (≤ 64 KiB, allowlist `{state, sessionId}`; the `name` key is a session name
//! and is not read) and `<cfg>/daemon/roster.json` (allowlist
//! `{jobs[].{id, state}}`). The format is not a stable interface: any
//! mismatch means "ignore". `daemon/control.key` is never opened, and Gilde
//! never runs `claude agents --json` (it writes `.claude.json` + `backups/`).

use std::path::Path;

use serde::Deserialize;

use crate::discovery::is_vendor_uuid;
use crate::discovery::safe_read::{self, ReadError, Root};

pub const JOB_STATE_CAP: u64 = 64 * 1024;
pub const ROSTER_CAP: u64 = 256 * 1024;

/// A job's allowlisted state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobState {
    /// e.g. `running`, `blocked`, `done`, `failed`, `stopped`.
    pub state: String,
    pub session_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawJob {
    state: String,
    session_id: Option<String>,
}

/// `jobId` = 8 lowercase hex (`sessionId[:8]`).
pub fn is_job_id(s: &str) -> bool {
    s.len() == 8 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Parse `jobs/<id>/state.json`; any mismatch → `None` (ignored).
pub fn parse_job_state(bytes: &[u8], job_id: &str) -> Option<JobState> {
    let r: RawJob = serde_json::from_slice(bytes).ok()?;
    if r.state.is_empty() || r.state.len() > 32 || !r.state.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') {
        return None;
    }
    let session_id = r.session_id.filter(|s| is_vendor_uuid(s));
    if let Some(sid) = &session_id {
        if !sid.starts_with(job_id) {
            return None;
        }
    }
    Some(JobState { state: r.state, session_id })
}

/// Read one job's state.
pub fn read_job_state(cfg: &Root, job_id: &str) -> Result<Option<JobState>, ReadError> {
    if !is_job_id(job_id) {
        return Ok(None);
    }
    let path = cfg.join(Path::new("jobs").join(job_id).join("state.json"));
    match safe_read::read_file(cfg, &path, JOB_STATE_CAP) {
        Ok((bytes, _)) => Ok(parse_job_state(&bytes, job_id)),
        Err(ReadError::NotFound) | Err(ReadError::TooLarge { .. }) => Ok(None),
        Err(e) => Err(e),
    }
}

/// One roster row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterJob {
    pub id: String,
    pub state: String,
}

#[derive(Deserialize)]
struct RawRoster {
    jobs: Vec<RawRosterJob>,
}

#[derive(Deserialize)]
struct RawRosterJob {
    id: String,
    state: String,
}

/// Parse `daemon/roster.json` (allowlist `jobs[].{id, state}`).
pub fn parse_roster(bytes: &[u8]) -> Option<Vec<RosterJob>> {
    let r: RawRoster = serde_json::from_slice(bytes).ok()?;
    Some(
        r.jobs
            .into_iter()
            .filter(|j| is_job_id(&j.id) && !j.state.is_empty() && j.state.len() <= 32)
            .map(|j| RosterJob { id: j.id, state: j.state })
            .collect(),
    )
}

/// Read `daemon/roster.json` (never `daemon/control.key`).
pub fn read_roster(cfg: &Root) -> Result<Option<Vec<RosterJob>>, ReadError> {
    let path = cfg.join("daemon/roster.json");
    match safe_read::read_file(cfg, &path, ROSTER_CAP) {
        Ok((bytes, _)) => Ok(parse_roster(&bytes)),
        Err(ReadError::NotFound) | Err(ReadError::TooLarge { .. }) => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_state_allowlist() {
        let s = parse_job_state(
            br#"{"backend":"daemon","state":"done","sessionId":"04b2b152-0000-4000-8000-000000000000","name":"CANARY","providerEnv":{"K":"V"}}"#,
            "04b2b152",
        )
        .unwrap();
        assert_eq!(s.state, "done");
        assert!(!format!("{s:?}").contains("CANARY"));
        assert!(parse_job_state(br#"{"state":"done","sessionId":"ffffffff-0000-4000-8000-000000000000"}"#, "04b2b152").is_none());
        assert!(parse_job_state(br#"{"state":7}"#, "04b2b152").is_none());
        assert!(is_job_id("04b2b152"));
        assert!(!is_job_id("04B2B152"));
        assert_eq!(parse_roster(br#"{"proto":1,"jobs":[{"id":"04b2b152","state":"running","x":1},{"id":"bad","state":"x"}]}"#).unwrap().len(), 1);
    }
}
