//! `<cfg>/sessions/<pid>.json` and the liveness rule (contract §5.2.1).
//!
//! * Only names matching `^\d+\.json$` are opened; the `<pid>.<hex>.key`
//!   files beside them never are (name filter first, then the never-open
//!   filter of `safe_read`) [C6].
//! * Allowlisted fields only: unknown fields are ignored; a wrong type or an
//!   unknown `status`/`kind` value makes the record `unknownFormat` (Status
//!   unknown, never guessed, §3.1 rule 4). `messagingSocketPath`,
//!   `hostSessionId`, `bridgeSessionId`, `tempo/state/detail/needs` are not
//!   deserialized.
//! * A torn read (the CLI rewrites in place) is retried once after 75 ms.
//! * Liveness: all four checks or the entry is `stale` and hidden — the file
//!   parses with `pid` = file-name pid and `peerProtocol == 1`; `kill(pid, 0)`;
//!   kernel start ≤ `startedAt/1000 + 5` and equal to `procStart` (±1 s);
//!   the executable is `claude`/`claude.exe`/`…/native-binary/claude`.
//!   Sockets, team dirs, `status` and transcript mtime never make a session live.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::contract::activity::RegStatus;
use crate::contract::common::EpochMs;
use crate::discovery::is_vendor_uuid;
use crate::discovery::procs::{Alive, ProcSource};
use crate::discovery::safe_read::{self, EntryKind, FileId, ReadError, Root};

/// Registry file cap (§5.8): over it → `unknownFormat`.
pub const REGISTRY_CAP: u64 = 64 * 1024;
/// `sessions/` entries kept (newest by mtime) (§5.8).
pub const SESSIONS_ENTRIES_CAP: usize = 4_096;
/// The one torn-read retry (§5.8) [R2 E32].
pub const TORN_RETRY: Duration = Duration::from_millis(75);
/// `startedAt/1000 + 5` (§5.2.1 rule 3).
pub const START_SLACK_SECS: i64 = 5;

/// Registry `kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegKind {
    /// Also `-p` stream-json sessions: never classifies Gilde's own [R6 §6].
    Interactive,
    Bg,
}

/// The allowlisted, validated registry record. `name` is **[content]** when
/// the user set it (memory only).
#[derive(Debug, Clone, PartialEq)]
pub struct RegistryRecord {
    pub pid: i32,
    pub session_id: String,
    pub cwd: Option<String>,
    pub started_at: Option<EpochMs>,
    /// `procStart` parsed (UTC seconds).
    pub proc_start: Option<i64>,
    pub version: Option<String>,
    pub peer_protocol: Option<i64>,
    pub kind: Option<RegKind>,
    pub entrypoint: Option<String>,
    pub status: Option<RegStatus>,
    pub status_updated_at: Option<EpochMs>,
    pub waiting_for: Option<String>,
    /// The user-set session name (`nameSource` ≠ `derived`) — **[content]**.
    pub user_name: Option<String>,
    pub job_id: Option<String>,
    pub spare: bool,
}

/// Why a record is `unknownFormat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatError {
    Parse,
    TooLarge,
    PidMismatch,
    PeerProtocol,
    SessionId,
    ProcStart,
    UnknownValue,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawEntry {
    pid: i64,
    session_id: String,
    cwd: Option<String>,
    started_at: Option<i64>,
    proc_start: Option<String>,
    version: Option<String>,
    peer_protocol: Option<i64>,
    kind: Option<String>,
    entrypoint: Option<String>,
    status: Option<String>,
    status_updated_at: Option<i64>,
    waiting_for: Option<String>,
    name: Option<String>,
    name_source: Option<String>,
    job_id: Option<String>,
    spare: Option<bool>,
}

fn small(s: Option<String>, max: usize) -> Option<String> {
    s.filter(|v| !v.is_empty() && v.len() <= max)
}

/// Parse one registry file's bytes. `file_pid` is the pid in the file name.
pub fn parse_registry(bytes: &[u8], file_pid: i32) -> Result<RegistryRecord, FormatError> {
    let raw: RawEntry = serde_json::from_slice(bytes).map_err(|_| FormatError::Parse)?;
    if raw.pid != i64::from(file_pid) {
        return Err(FormatError::PidMismatch);
    }
    let status = match raw.status.as_deref() {
        None => None,
        Some("busy") => Some(RegStatus::Busy),
        Some("idle") => Some(RegStatus::Idle),
        Some("waiting") => Some(RegStatus::Waiting),
        Some(_) => return Err(FormatError::UnknownValue),
    };
    let kind = match raw.kind.as_deref() {
        None => None,
        Some("interactive") => Some(RegKind::Interactive),
        Some("bg") => Some(RegKind::Bg),
        Some(_) => return Err(FormatError::UnknownValue),
    };
    let proc_start = match raw.proc_start.as_deref() {
        None => None,
        Some(s) => Some(parse_proc_start(s).ok_or(FormatError::ProcStart)?),
    };
    let user_name = match raw.name_source.as_deref() {
        Some("derived") | None => None,
        Some(_) => small(raw.name, 512),
    };
    let rec = RegistryRecord {
        pid: file_pid,
        session_id: raw.session_id,
        cwd: small(raw.cwd, 4096),
        started_at: raw.started_at.filter(|v| *v > 0),
        proc_start,
        version: small(raw.version, 32),
        peer_protocol: raw.peer_protocol,
        kind,
        entrypoint: small(raw.entrypoint, 64),
        status,
        status_updated_at: raw.status_updated_at.filter(|v| *v > 0),
        waiting_for: small(raw.waiting_for, 128),
        user_name,
        job_id: small(raw.job_id, 64),
        spare: raw.spare.unwrap_or(false),
    };
    if !is_vendor_uuid(&rec.session_id) {
        return Err(FormatError::SessionId);
    }
    Ok(rec)
}

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

fn proc_start_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r"^(Mon|Tue|Wed|Thu|Fri|Sat|Sun) (Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec) +(\d{1,2}) (\d{2}):(\d{2}):(\d{2}) (\d{4})$",
        )
        .expect("the procStart grammar compiles")
    })
}

/// `procStart`: `^(Mon|…|Sun) (Jan|…|Dec) +(\d{1,2}) (\d{2}):(\d{2}):(\d{2}) (\d{4})$`,
/// English names, **UTC**, day space-padded ("Oct  1") [R2 E7]. Returns UTC seconds.
pub fn parse_proc_start(s: &str) -> Option<i64> {
    let c = proc_start_re().captures(s)?;
    let num = |i: usize| c.get(i).and_then(|m| m.as_str().parse::<u32>().ok());
    let month = MONTHS.iter().position(|m| *m == &c[2])? as u32 + 1;
    let (d, h, mi, se) = (num(3)?, num(4)?, num(5)?, num(6)?);
    let y = i64::from(num(7)?);
    if !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let days = days_from_civil(y, month, d);
    Some(days * 86_400 + i64::from(h) * 3600 + i64::from(mi) * 60 + i64::from(se))
}

/// Days since 1970-01-01 for a proleptic Gregorian date (H. Hinnant).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Why a parsed entry is hidden.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleWhy {
    NotAlive,
    /// `EPERM`: another user — excluded, never signalled.
    OtherUser,
    StartMismatch,
    NotClaude,
    /// The `pid` field differs from the file-name pid (A33.5(a)): the file
    /// lies about which process it describes, so it fails liveness whatever
    /// process runs under either pid — hidden and counted in `hiddenStale`,
    /// never attributed to a live process.
    PidMismatch,
}

/// The liveness verdict of one registry file.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// All four checks passed. `weak` = `procStart` absent (diagnostics).
    Live { record: Box<RegistryRecord>, weak: bool, start_sec: i64, exec_path: String },
    /// Hidden; diagnostics count it.
    Stale { why: StaleWhy },
    /// A pre-warmed spare (`spare: true`): hidden.
    Spare,
    /// The file could not be trusted; `live` = the file-name pid passes the
    /// process checks (then: Status unknown{UnknownPeerProtocol}).
    UnknownFormat { why: FormatError, live: Option<(i64, String)> },
}

/// Process checks 2–4 for `pid` (and the start-time bound when known).
fn process_checks(
    procs: &dyn ProcSource,
    pid: i32,
    started_at: Option<EpochMs>,
    proc_start: Option<i64>,
) -> Result<(i64, String), StaleWhy> {
    match procs.alive(pid) {
        Alive::Yes => {}
        Alive::No => return Err(StaleWhy::NotAlive),
        Alive::OtherUser => return Err(StaleWhy::OtherUser),
    }
    let Some(info) = procs.bsd_info(pid) else { return Err(StaleWhy::NotAlive) };
    if let Some(started) = started_at {
        if info.start_sec > started / 1000 + START_SLACK_SECS {
            return Err(StaleWhy::StartMismatch);
        }
    }
    if let Some(ps) = proc_start {
        if (info.start_sec - ps).abs() > 1 {
            return Err(StaleWhy::StartMismatch);
        }
    }
    let exec = procs.exec_path(pid).ok_or(StaleWhy::NotAlive)?;
    if !is_claude_process(procs, pid, &exec) {
        return Err(StaleWhy::NotClaude);
    }
    Ok((info.start_sec, exec))
}

/// A47.1: whether `exec` (the **resolved** `proc_pidpath`) runs claude, by
/// the one shared classifier (A39.1(a)): a native installer's
/// `…/share/claude/versions/<semver>` is claude, and a `node`/`bun` is
/// claude only when its argv names claude's script.
fn is_claude_process(procs: &dyn ProcSource, pid: i32, exec: &str) -> bool {
    use crate::agents::classify::{vendor_of_exec_path, vendor_of_process};
    if let Some(vendor) = vendor_of_exec_path(exec) {
        return vendor == crate::contract::common::Vendor::Claude;
    }
    if !matches!(exec.rsplit('/').next(), Some("node" | "bun")) {
        return false;
    }
    // The raw argv is used only to classify; it is zeroed when `raw` drops.
    let raw = procs.args(pid);
    let argv: &[String] = raw.as_ref().and_then(|r| r.argv.as_deref()).unwrap_or(&[]);
    vendor_of_process(exec, argv) == Some(crate::contract::common::Vendor::Claude)
}

/// Apply the liveness rule to a parse result.
pub fn verdict(parsed: Result<RegistryRecord, FormatError>, file_pid: i32, procs: &dyn ProcSource) -> Verdict {
    match parsed {
        Ok(rec) if rec.spare => Verdict::Spare,
        Ok(rec) if rec.peer_protocol != Some(1) => Verdict::UnknownFormat {
            why: FormatError::PeerProtocol,
            live: process_checks(procs, file_pid, rec.started_at, rec.proc_start).ok(),
        },
        Ok(rec) => match process_checks(procs, rec.pid, rec.started_at, rec.proc_start) {
            Ok((start_sec, exec_path)) => {
                let weak = rec.proc_start.is_none();
                Verdict::Live { record: Box::new(rec), weak, start_sec, exec_path }
            }
            Err(why) => Verdict::Stale { why },
        },
        // A33.5(a): a lying `pid` is a liveness failure (stale, hidden), not
        // an unknown format: no process checks run for either pid.
        Err(FormatError::PidMismatch) => Verdict::Stale { why: StaleWhy::PidMismatch },
        Err(why) => Verdict::UnknownFormat { why, live: process_checks(procs, file_pid, None, None).ok() },
    }
}

/// One registry file in `sessions/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegFile {
    pub pid: i32,
    pub path: PathBuf,
    pub id: FileId,
}

/// `^\d+\.json$` → the pid.
pub fn registry_pid(name: &str) -> Option<i32> {
    let stem = name.strip_suffix(".json")?;
    if stem.is_empty() || stem.len() > 10 || !stem.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    stem.parse::<i32>().ok().filter(|p| *p > 0)
}

/// List the registry files of `<cfg>/sessions` (newest 4,096 by mtime).
/// Returns the files and whether the cap was hit.
pub fn list_registry(sessions: &Root) -> Result<(Vec<RegFile>, bool), ReadError> {
    let listing = safe_read::list_dir(sessions, sessions.path(), safe_read::DIR_ENTRIES_CAP)?;
    let mut files = Vec::new();
    for e in listing.entries {
        if e.kind != EntryKind::File {
            continue; // symlinks and dirs are skipped (§5.8)
        }
        let Some(pid) = registry_pid(&e.name) else { continue }; // `*.key` never matches
        let path = sessions.join(&e.name);
        if let Ok(st) = safe_read::lstat(&path) {
            if st.kind == EntryKind::File {
                files.push(RegFile { pid, path, id: st.id });
            }
        }
    }
    let mut truncated = listing.truncated;
    if files.len() > SESSIONS_ENTRIES_CAP {
        files.sort_by_key(|f| std::cmp::Reverse(f.id.mtime_ms));
        files.truncate(SESSIONS_ENTRIES_CAP);
        truncated = true;
    }
    Ok((files, truncated))
}

/// Read and parse one registry file, retrying a torn read once after 75 ms.
/// `Err` is a read failure (permission, gone); parse problems are `Ok(Err(_))`.
pub fn read_registry(
    sessions: &Root,
    path: &Path,
    file_pid: i32,
    retry: Duration,
) -> Result<(Result<RegistryRecord, FormatError>, FileId), ReadError> {
    let attempt = || -> Result<(Result<RegistryRecord, FormatError>, FileId), ReadError> {
        match safe_read::read_file(sessions, path, REGISTRY_CAP) {
            Ok((bytes, id)) => Ok((parse_registry(&bytes, file_pid), id)),
            Err(ReadError::TooLarge { size }) => Ok((
                Err(FormatError::TooLarge),
                FileId { size, ..FileId::default() },
            )),
            Err(e) => Err(e),
        }
    };
    let first = attempt()?;
    if matches!(first.0, Err(FormatError::Parse)) {
        std::thread::sleep(retry);
        return attempt();
    }
    Ok(first)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::procs::{FakeProc, FakeProcs};

    const SID: &str = "0e2c0b1a-6b7d-4c51-9a8e-1f2e3d4c5b6a";

    fn entry(pid: i32, extra: &str) -> String {
        format!(
            r#"{{"pid":{pid},"sessionId":"{SID}","cwd":"/w","startedAt":1792558922000,"procStart":"Wed Oct 21 04:25:05 2026","version":"2.1.306","peerProtocol":1,"peerFeatures":["notify_idle"],"kind":"interactive","entrypoint":"cli","pidDomain":"darwin","messagingSocketPath":"/tmp/cc-socks/{pid}.sock","name":"x","nameSource":"derived","status":"idle","statusUpdatedAt":1792558930000{extra}}}"#
        )
    }

    fn fake_claude(pid: i32, start: i64) -> FakeProcs {
        let f = FakeProcs::new(501);
        f.insert(pid, FakeProc { ppid: 1, uid: 501, start_sec: start, comm: "claude".into(), exec_path: Some("/opt/c/bin/claude.exe".into()), ..Default::default() });
        f
    }

    #[test]
    fn proc_start_grammar() {
        assert_eq!(parse_proc_start("Thu Jan  1 00:00:00 1970"), Some(0));
        assert_eq!(parse_proc_start("Wed Oct 21 04:25:05 2026"), Some(days_from_civil(2026, 10, 21) * 86_400 + 4 * 3600 + 25 * 60 + 5));
        assert_eq!(parse_proc_start("Thu Oct  1 13:00:00 2026"), Some(days_from_civil(2026, 10, 1) * 86_400 + 13 * 3600));
        for (i, m) in MONTHS.iter().enumerate() {
            let s = format!("Mon {m} 15 01:02:03 2024");
            assert_eq!(parse_proc_start(&s), Some(days_from_civil(2024, i as u32 + 1, 15) * 86_400 + 3723), "{s}");
        }
        assert_eq!(parse_proc_start("Thu Oct 1 13:00:00 2026"), parse_proc_start("Thu Oct  1 13:00:00 2026"), "` +` allows one space");
        for bad in ["thu Oct  1 13:00:00 2026", "Thu Okt  1 13:00:00 2026", "Thu Oct 01 13:00 2026", "Thu Oct  1 13:00:00 26", "Thu Oct  1 13:00:00 2026 ", "", "Thu Oct 32 13:00:00 2026", "Thu  Oct 1 13:00:00 2026", "Thu Oct 1 13:00:00 2026\n", "Thu Oct 123 13:00:00 2026"] {
            assert_eq!(parse_proc_start(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn parse_allowlist_and_unknown_values() {
        let r = parse_registry(entry(52146, "").as_bytes(), 52146).unwrap();
        assert_eq!(r.status, Some(RegStatus::Idle));
        assert_eq!(r.kind, Some(RegKind::Interactive));
        assert_eq!(r.user_name, None, "derived names are not user names");
        let user = entry(52146, "").replace(r#""nameSource":"derived""#, r#""nameSource":"user""#);
        assert_eq!(parse_registry(user.as_bytes(), 52146).unwrap().user_name.as_deref(), Some("x"));
        assert_eq!(parse_registry(entry(52146, "").as_bytes(), 9).unwrap_err(), FormatError::PidMismatch);
        let busy_new = entry(1, "").replace(r#""status":"idle""#, r#""status":"compacting""#);
        assert_eq!(parse_registry(busy_new.as_bytes(), 1).unwrap_err(), FormatError::UnknownValue);
        let wrong_type = entry(1, "").replace(r#""startedAt":1792558922000"#, r#""startedAt":"x""#);
        assert_eq!(parse_registry(wrong_type.as_bytes(), 1).unwrap_err(), FormatError::Parse);
        let bad_id = entry(1, "").replace(SID, "not-a-uuid");
        assert_eq!(parse_registry(bad_id.as_bytes(), 1).unwrap_err(), FormatError::SessionId);
        assert_eq!(parse_registry(br#"{"pid":1,"sessionId""#, 1).unwrap_err(), FormatError::Parse);
        assert_eq!(registry_pid("52146.json"), Some(52146));
        assert_eq!(registry_pid("52146.0a1b.key"), None);
        assert_eq!(registry_pid("x.json"), None);
        assert_eq!(registry_pid("0.json"), None);
    }

    #[test]
    fn liveness_each_check_fails_alone() {
        let ps = parse_proc_start("Wed Oct 21 04:25:05 2026").unwrap();
        let rec = || parse_registry(entry(77, "").as_bytes(), 77);
        assert!(matches!(verdict(rec(), 77, &fake_claude(77, ps)), Verdict::Live { weak: false, .. }));
        // 1. peerProtocol ≠ 1 → unknownFormat, live process reported
        let p2 = entry(77, "").replace(r#""peerProtocol":1"#, r#""peerProtocol":2"#);
        match verdict(parse_registry(p2.as_bytes(), 77), 77, &fake_claude(77, ps)) {
            Verdict::UnknownFormat { why: FormatError::PeerProtocol, live: Some(_) } => {}
            v => panic!("{v:?}"),
        }
        // 2. dead pid
        assert_eq!(verdict(rec(), 77, &FakeProcs::new(501)), Verdict::Stale { why: StaleWhy::NotAlive });
        // 2'. another user (EPERM)
        let other = fake_claude(77, ps);
        other.table.lock().unwrap().get_mut(&77).unwrap().other_user = true;
        assert_eq!(verdict(rec(), 77, &other), Verdict::Stale { why: StaleWhy::OtherUser });
        // 3. reused pid (start time differs from procStart)
        assert_eq!(verdict(rec(), 77, &fake_claude(77, ps + 3600)), Verdict::Stale { why: StaleWhy::StartMismatch });
        // 3'. procStart absent: the startedAt bound alone, flagged weak
        let weak = entry(77, "").replace(r#""procStart":"Wed Oct 21 04:25:05 2026","#, "");
        assert!(matches!(verdict(parse_registry(weak.as_bytes(), 77), 77, &fake_claude(77, ps)), Verdict::Live { weak: true, .. }));
        assert_eq!(
            verdict(parse_registry(weak.as_bytes(), 77), 77, &fake_claude(77, 1792558922 + 60)),
            Verdict::Stale { why: StaleWhy::StartMismatch }
        );
        // 4. not a claude executable
        let compiler = FakeProcs::new(501);
        compiler.insert(77, FakeProc { ppid: 1, uid: 501, start_sec: ps, exec_path: Some("/usr/bin/cc".into()), ..Default::default() });
        assert_eq!(verdict(rec(), 77, &compiler), Verdict::Stale { why: StaleWhy::NotClaude });
        // 1'. `pid` ≠ file-name pid: stale whatever runs under either pid (A33.5(a))
        let lying = entry(78, "");
        assert_eq!(verdict(parse_registry(lying.as_bytes(), 77), 77, &fake_claude(77, ps)), Verdict::Stale { why: StaleWhy::PidMismatch });
        assert_eq!(verdict(parse_registry(lying.as_bytes(), 77), 77, &fake_claude(78, ps)), Verdict::Stale { why: StaleWhy::PidMismatch });
        assert_eq!(verdict(parse_registry(lying.as_bytes(), 77), 77, &FakeProcs::new(501)), Verdict::Stale { why: StaleWhy::PidMismatch });
        // spare
        let spare = entry(77, r#","spare":true"#);
        assert_eq!(verdict(parse_registry(spare.as_bytes(), 77), 77, &fake_claude(77, ps)), Verdict::Spare);
    }
}
