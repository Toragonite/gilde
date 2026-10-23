//! Shared helpers of the discovery tests (T03). Include from a test with
//! `#[path = "fixtures/vendor/support.rs"] mod support;`.
//!
//! Every tree is a temp dir: no test ever reads a real vendor directory.

#![allow(dead_code)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gilde_code_lib::contract::account::Account;
use gilde_code_lib::contract::common::Vendor;
use gilde_code_lib::contract::ids::AccountId;
use gilde_code_lib::contract::services::{EvidenceSink, EvidenceUpdate, LocationFacts, SourceFacts};
use gilde_code_lib::contract::store::SourceStatus;
use gilde_code_lib::discovery::history::{HistoryService, HistorySources, MemoryUids};
use gilde_code_lib::discovery::procs::{FakeBundles, FakeProc, FakeProcs};
use gilde_code_lib::discovery::{AccountSpec, DiscoveryConfig, TeamObservation, TeamObserver};

/// The canary in every content field of the fixtures.
pub const CANARY: &str = "CANARY-7f3a-CONTENT";
pub const UID: u32 = 501;

pub fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/vendor")
}

/// A fixture file with `{{NAME}}` placeholders replaced.
pub fn template(rel: &str, vars: &[(&str, &str)]) -> String {
    let mut s = std::fs::read_to_string(fixture_dir().join(rel)).unwrap_or_else(|e| panic!("fixture {rel}: {e}"));
    for (k, v) in vars {
        s = s.replace(&format!("{{{{{k}}}}}"), v);
    }
    s
}

/// A deterministic canonical UUID.
pub fn uuid(n: u32) -> String {
    format!("{n:08x}-0000-4000-8000-{n:012x}")
}

/// Days → (y, m, d) (H. Hinnant's civil_from_days).
fn civil(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `procStart` (UTC ctime, space-padded day) for kernel start `secs`.
pub fn ctime_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil(days);
    let wd = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"][days.rem_euclid(7) as usize];
    let mon = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"][m as usize - 1];
    format!("{wd} {mon} {d:>2} {:02}:{:02}:{:02} {y}", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// A fixture home: `<tmp>/home/.claude/{sessions,projects,teams,tasks,jobs,daemon}`.
pub struct Home {
    _root: tempfile::TempDir,
    pub home: PathBuf,
    pub claude: PathBuf,
    pub app_data: PathBuf,
}

impl Home {
    pub fn new() -> Home {
        let root = tempfile::Builder::new().prefix("gilde-disc-").tempdir().expect("temp root");
        let base = std::fs::canonicalize(root.path()).expect("canonical root");
        let home = base.join("home");
        let claude = home.join(".claude");
        for d in ["sessions", "projects", "teams", "tasks", "jobs", "daemon"] {
            std::fs::create_dir_all(claude.join(d)).expect("claude tree");
        }
        let app_data = home.join(".gilde");
        std::fs::create_dir_all(&app_data).expect("gilde home");
        Home { _root: root, home, claude, app_data }
    }

    pub fn workdir(&self, name: &str) -> PathBuf {
        let p = self.home.join("work").join(name);
        std::fs::create_dir_all(&p).expect("workdir");
        p
    }

    /// `<cfg>/sessions/<pid>.json` from the template.
    #[allow(clippy::too_many_arguments)]
    pub fn write_registry(&self, cfg: &Path, pid: i32, sid: &str, start_sec: i64, cwd: &Path, kind: &str, entrypoint: &str, status: &str, extra: &str) -> PathBuf {
        let body = template(
            "claude/registry/interactive.json",
            &[
                ("PID", &pid.to_string()),
                ("SID", sid),
                ("CWD", cwd.to_str().unwrap()),
                ("STARTED_AT", &(start_sec * 1000 + 400).to_string()),
                ("PROC_START", &ctime_utc(start_sec)),
                ("KIND", kind),
                ("ENTRYPOINT", entrypoint),
                ("STATUS", status),
                ("EXTRA", extra),
            ],
        );
        let p = cfg.join("sessions").join(format!("{pid}.json"));
        std::fs::write(&p, body).expect("registry");
        p
    }

    /// `<cfg>/projects/<slug>/<sid>.jsonl` from a transcript fixture.
    pub fn write_transcript(&self, cfg: &Path, slug: &str, sid: &str, fixture: &str, vars: &[(&str, &str)]) -> PathBuf {
        let mut all: Vec<(&str, &str)> = vec![("SID", sid)];
        all.extend_from_slice(vars);
        let body = template(fixture, &all);
        let dir = cfg.join("projects").join(slug);
        std::fs::create_dir_all(&dir).expect("slug dir");
        let p = dir.join(format!("{sid}.jsonl"));
        std::fs::write(&p, body).expect("transcript");
        p
    }

    /// A config for the engine over this home with fake processes.
    pub fn config(&self, procs: Arc<FakeProcs>, teams: Arc<dyn TeamObserver>, history: Option<Arc<HistoryService>>) -> DiscoveryConfig {
        let mut cfg = DiscoveryConfig::for_home(&self.home, &self.app_data, Arc::new(gilde_code_lib::discovery::NoOwnProcesses));
        cfg.accounts = vec![AccountSpec::new(Vendor::Claude, &self.claude), AccountSpec::new(Vendor::Codex, &self.home.join(".codex"))];
        cfg.source = procs;
        cfg.bundles = Arc::new(FakeBundles::default());
        cfg.teams = teams;
        cfg.history = history;
        cfg.watch = false;
        cfg.registry_retry = Duration::from_millis(1);
        cfg.team_retries = vec![Duration::ZERO; 3];
        cfg
    }

    pub fn history(&self) -> Arc<HistoryService> {
        HistoryService::new(
            HistorySources {
                claude: vec![(self.claude_account(), self.claude.clone())],
                codex: vec![],
                cursor_chats: Some(self.home.join(".cursor/chats")),
                agy_dir: Some(self.home.join(".gemini/antigravity-cli")),
                home: self.home.clone(),
                app_data: self.app_data.clone(),
            },
            Arc::new(MemoryUids::default()),
            None,
        )
    }

    pub fn claude_account(&self) -> AccountId {
        AccountSpec::new(Vendor::Claude, &self.claude).id
    }
}

/// A fake `claude` process.
pub fn claude_proc(start_sec: i64, ppid: i32, argv: &[&str], cwd: &Path) -> FakeProc {
    FakeProc {
        ppid,
        uid: UID,
        start_sec,
        comm: "claude".into(),
        exec_path: Some("/opt/fake/bin/claude.exe".into()),
        argv: Some(argv.iter().map(|s| s.to_string()).collect()),
        env_names: Some(vec!["PATH".into(), "HOME".into()]),
        cwd: Some(cwd.to_string_lossy().into_owned()),
        rss_bytes: 50_000_000,
        cpu_ns: 0,
        other_user: false,
    }
}

pub fn procs() -> Arc<FakeProcs> {
    let p = Arc::new(FakeProcs::new(UID));
    p.insert(1, FakeProc { ppid: 0, uid: 0, start_sec: 1, comm: "launchd".into(), exec_path: Some("/sbin/launchd".into()), ..FakeProc::default() });
    p
}

/// Records everything discovery sends.
#[derive(Default)]
pub struct Rec {
    pub evidence: Mutex<Vec<EvidenceUpdate>>,
    pub locations: Mutex<Vec<LocationFacts>>,
    pub statuses: Mutex<Vec<SourceStatus>>,
    pub accounts: Mutex<Vec<Account>>,
}

impl EvidenceSink for Rec {
    fn evidence(&self, update: EvidenceUpdate) {
        self.evidence.lock().unwrap().push(update);
    }
    fn location(&self, facts: LocationFacts) {
        self.locations.lock().unwrap().push(facts);
    }
    fn source_status(&self, status: SourceStatus) {
        self.statuses.lock().unwrap().push(status);
    }
    fn account(&self, account: Account) {
        self.accounts.lock().unwrap().push(account);
    }
}

impl Rec {
    pub fn all(&self) -> Vec<EvidenceUpdate> {
        self.evidence.lock().unwrap().clone()
    }

    pub fn clear(&self) {
        self.evidence.lock().unwrap().clear();
    }

    /// Facts sent for a vendor session id.
    pub fn for_sid(&self, sid: &str) -> Vec<SourceFacts> {
        self.all().into_iter().filter(|u| u.hint.vendor_session_id.as_deref() == Some(sid)).map(|u| u.facts).collect()
    }

    /// Facts sent for a pid (any hint carrying its process key).
    pub fn for_pid(&self, pid: i32) -> Vec<EvidenceUpdate> {
        let prefix = format!("x_{pid}_");
        self.all().into_iter().filter(|u| u.hint.process.as_ref().is_some_and(|p| p.as_str().starts_with(&prefix))).collect()
    }

    pub fn sids(&self) -> HashSet<String> {
        self.all().into_iter().filter_map(|u| u.hint.vendor_session_id).collect()
    }
}

/// Records team observations.
#[derive(Default)]
pub struct TeamsRec {
    pub obs: Mutex<Vec<TeamObservation>>,
    pub gone: Mutex<Vec<String>>,
}

impl TeamObserver for TeamsRec {
    fn team(&self, obs: TeamObservation) {
        self.obs.lock().unwrap().push(obs);
    }
    fn team_gone(&self, _account: &AccountId, dir_name: &str, _at: i64) {
        self.gone.lock().unwrap().push(dir_name.to_string());
    }
}

/// Every regular file under `root` with its (size, mtime) — the never-write check.
pub fn tree_state(root: &Path) -> Vec<(PathBuf, u64, i64)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(m) = std::fs::symlink_metadata(&p) else { continue };
            use std::os::unix::fs::MetadataExt;
            if m.is_dir() {
                stack.push(p.clone());
            }
            out.push((p, m.size(), m.mtime() * 1_000_000_000 + m.mtime_nsec()));
        }
    }
    out.sort();
    out
}

/// `chmod -R` (dirs keep `x` so they stay listable).
pub fn chmod_tree(root: &Path, file_mode: u32, dir_mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let mut stack = vec![root.to_path_buf()];
    let mut dirs = Vec::new();
    while let Some(d) = stack.pop() {
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                let p = e.path();
                let Ok(m) = std::fs::symlink_metadata(&p) else { continue };
                if m.is_dir() {
                    stack.push(p.clone());
                    dirs.push(p);
                } else if m.is_file() {
                    let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(file_mode));
                }
            }
        }
    }
    dirs.push(root.to_path_buf());
    for d in dirs.into_iter().rev() {
        let _ = std::fs::set_permissions(&d, std::fs::Permissions::from_mode(dir_mode));
    }
}
