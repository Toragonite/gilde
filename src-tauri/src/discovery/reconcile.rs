//! The discovery engine (contract §5.1, §5.9): pass 1 (B7), the libproc scan
//! cadence (3 s frontmost / 15 s otherwise, and immediately on a registry or
//! teams event, wake and focus), debounced FSEvents, kqueue exits, and the
//! reconcile — every 60 s, on focus and on wake: a listing + `stat` of every
//! root, head/tail only for changed files.
//!
//! The engine never mutates World: it sends [`EvidenceUpdate`]s through
//! `EvidenceSink`, and only when a fact changed (change-driven World, §11.2).
//! Team files go to a [`TeamObserver`] (World's Claude native adapter).
//!
//! Failure rules (§5.8): `EPERM`/`EACCES` on a root makes that source
//! `PermissionDenied` — **not empty**: the last known facts are kept, the
//! affected sessions get `SourceDenied(true)` (World shows them `Likely` with
//! "Can't read updates right now."), and liveness (kill/libproc) is unaffected,
//! so no session is ever marked Ended because a read failed.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::contract::activity::{ProcessFacts, RegistryFacts, TeamFileFacts, TranscriptFacts};
use crate::contract::common::{EpochMs, Vendor};
use crate::contract::display::TextCap;
use crate::contract::ids::{AccountId, ProcessKey};
use crate::contract::process::ProcRole;
use crate::contract::services::{
    EvidenceSink, EvidenceUpdate, IdentityHint, ProcSnapshot, SessionMetaFacts, SourceFacts,
};
use crate::contract::session::{BinaryFingerprint, BinaryRef, Origin};
use crate::contract::store::{SourceKind, SourceState, SourceStatus};

use super::claude::jobs::{self, JobState};
use super::claude::registry::{self, RegKind, Verdict};
use super::claude::teams::{self, InProcessMeta, InboxCache, LeadJoin, LiveLeadCandidate, LiveTeammate, TasksRead, TeamDirRead};
use super::claude::transcripts::{self, HeadMeta, TailMeta, TailMode};
use super::fsevents::{FsSignal, FsWatch, RootTag, WatchSpec};
use super::history::{HistoryService, HistorySources};
use super::kqueue::{KqEvent, KqWatcher, Track};
use super::procs::{
    claude_origin, other_vendor_origin, BundleInfo, Libproc, NsBundleInfo, OriginInputs, ProcSource, ScannedProc,
    Scanner, SCAN_BACKGROUND_MS, SCAN_FOCUSED_MS, UNREGISTERED_GRACE_MS,
};
use super::safe_read::{self, Backoff, FileId, ReadError, Root};
use super::{
    abs_path, agy, cursor, display_text, fail_closed_redactor, location, now_up_ms, now_wall_ms, orchestrator,
    real_abs_path, siblings, OwnProcesses, Redactor,
};

/// The reconcile interval (§5.9).
pub const RECONCILE_MS: u64 = 60_000;
/// Per-path FSEvents debounce floor (§5.9).
pub const DEBOUNCE_MS: u64 = 250;
/// The kqueue token of `$HOME` and of the orchestrator dir.
const TOKEN_HOME: u64 = 1;
const TOKEN_ORCH: u64 = 2;

// ---------------------------------------------------------------------------
// configuration
// ---------------------------------------------------------------------------

/// A Claude or Codex account to watch (config dir realpath).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountSpec {
    pub id: AccountId,
    pub vendor: Vendor,
    pub config_dir: PathBuf,
    /// A `~/.claude-*` sibling found by discovery (read-only, C7).
    pub discovered: bool,
}

impl AccountSpec {
    /// An account for `config_dir` (realpath'd when it exists).
    pub fn new(vendor: Vendor, config_dir: &Path) -> AccountSpec {
        let real = std::fs::canonicalize(config_dir).unwrap_or_else(|_| config_dir.to_path_buf());
        AccountSpec {
            id: AccountId::derive(vendor, &real.to_string_lossy()),
            vendor,
            config_dir: real,
            discovered: false,
        }
    }
}

/// A pane teammate process of a team.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveMember {
    pub agent_id: String,
    pub agent_name: String,
    pub process: ProcessKey,
    pub pid: i32,
    /// From the teammate's own transcript head (`agentName` + `teamName`).
    pub session_id: Option<String>,
}

/// One team dir as World's Claude native adapter needs it (§5.2.4).
#[derive(Debug, Clone, PartialEq)]
pub struct TeamObservation {
    pub account: AccountId,
    pub dir: TeamDirRead,
    pub join: LeadJoin,
    pub lead_alive: bool,
    pub live_members: Vec<LiveMember>,
    pub tasks: TasksRead,
    pub in_process: Vec<InProcessMeta>,
    /// A team dir with no live lead and no live member creates no team row
    /// (diagnostics only, §5.2.4).
    pub visible: bool,
    pub at: EpochMs,
}

/// Where team files go (World's Claude native adapter, T02).
pub trait TeamObserver: Send + Sync {
    fn team(&self, obs: TeamObservation);
    /// The dir disappeared (`Team.ended_at`).
    fn team_gone(&self, account: &AccountId, dir_name: &str, at: EpochMs);
}

pub struct NoTeamObserver;

impl TeamObserver for NoTeamObserver {
    fn team(&self, _obs: TeamObservation) {}
    fn team_gone(&self, _account: &AccountId, _dir_name: &str, _at: EpochMs) {}
}

/// Called with every scan's snapshot.
pub type SnapshotHook = Arc<dyn Fn(&ProcSnapshot) + Send + Sync>;
/// Called when the orchestrator's presence changes.
pub type PresenceHook = Arc<dyn Fn(bool) + Send + Sync>;

/// Everything the engine needs.
pub struct DiscoveryConfig {
    pub home: PathBuf,
    /// `GILDE_HOME` (the SQLite copies go to `<app_data>/tmp/vendorcopy`).
    pub app_data: PathBuf,
    /// Claude and Codex accounts (from settings; siblings are added here).
    pub accounts: Vec<AccountSpec>,
    /// `~/.cursor`.
    pub cursor_dir: Option<PathBuf>,
    /// `~/.gemini/antigravity-cli`.
    pub agy_dir: Option<PathBuf>,
    pub source: Arc<dyn ProcSource>,
    pub redact: Redactor,
    pub own: Arc<dyn OwnProcesses>,
    pub bundles: Arc<dyn BundleInfo>,
    pub teams: Arc<dyn TeamObserver>,
    pub history: Option<Arc<HistoryService>>,
    /// Every scan's snapshot (T09's procscan/watchdog: one scan, two consumers).
    pub on_snapshot: Option<SnapshotHook>,
    /// Orchestrator presence changes (the sidecar `orch.*` observer, T08).
    pub on_orchestrator: Option<PresenceHook>,
    /// Restrict argv reads to these pids (tests and benchmarks).
    pub scan_scope: Option<HashSet<i32>>,
    /// Start FSEvents + kqueue watchers.
    pub watch: bool,
    /// Registry torn-read retry (75 ms) and team config retries (50/100/200 ms).
    pub registry_retry: Duration,
    pub team_retries: Vec<Duration>,
}

impl DiscoveryConfig {
    /// The defaults for a `$HOME`: `~/.claude` (+ `CLAUDE_CONFIG_DIR` when
    /// Gilde was started with it), `CODEX_HOME` or `~/.codex`, `~/.cursor`,
    /// `~/.gemini/antigravity-cli`; the real libproc; the fail-closed
    /// redactor until T09's is wired. `own` is required (A11.3): discovery is
    /// never started without Gilde's own-process lookup, so Gilde's sessions
    /// are never classified as external.
    pub fn for_home(home: &Path, app_data: &Path, own: Arc<dyn OwnProcesses>) -> DiscoveryConfig {
        let mut accounts = vec![AccountSpec::new(Vendor::Claude, &home.join(".claude"))];
        if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
            let a = AccountSpec::new(Vendor::Claude, Path::new(&dir));
            if !accounts.iter().any(|x| x.config_dir == a.config_dir) {
                accounts.push(a);
            }
        }
        let codex = std::env::var_os("CODEX_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".codex"));
        accounts.push(AccountSpec::new(Vendor::Codex, &codex));
        DiscoveryConfig {
            home: home.to_path_buf(),
            app_data: app_data.to_path_buf(),
            accounts,
            cursor_dir: Some(home.join(".cursor")),
            agy_dir: Some(home.join(".gemini/antigravity-cli")),
            source: Arc::new(Libproc::new()),
            redact: fail_closed_redactor(),
            own,
            bundles: Arc::new(NsBundleInfo::default()),
            teams: Arc::new(NoTeamObserver),
            history: None,
            on_snapshot: None,
            on_orchestrator: None,
            scan_scope: None,
            watch: true,
            registry_retry: registry::TORN_RETRY,
            team_retries: teams::TORN_RETRIES.to_vec(),
        }
    }
}

/// Counters for `app_diagnostics` (never content, never paths).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiscoveryDiagnostics {
    pub scans: u64,
    pub reconciles: u64,
    pub procs: u32,
    pub candidates: u32,
    pub live_sessions: u32,
    pub registry_live: u32,
    pub registry_stale: u32,
    pub registry_weak: u32,
    pub registry_unknown_format: u32,
    pub registry_spares: u32,
    pub registry_torn: u64,
    pub teams_seen: u32,
    pub teams_hidden: u32,
    pub team_torn: u64,
    pub refused_opens: u64,
    pub eacces: u64,
    pub non_utf8: u64,
    pub fs_rescans: u64,
    pub kq_tracked: u32,
    pub kq_full: u64,
    pub transcripts_scanned: u64,
    pub skipped_backoff: u64,
    pub orchestrator_present: bool,
    pub sibling_accounts: u32,
    pub evidence_sent: u64,
    /// Team observations handed to the team observer (only changed ones).
    pub team_observations: u64,
    /// Inbox files read and parsed / served unchanged from the cache.
    pub inbox_reads: u64,
    pub inbox_cache_hits: u64,
    /// Inbox entries older than the newest 5,000 of their file (A33.5(d)).
    pub inbox_entries_dropped: u64,
    /// The team observer runs on its own thread (A33.5(d)), not on the
    /// discovery thread.
    pub team_observer_queued: bool,
}

// ---------------------------------------------------------------------------
// the engine
// ---------------------------------------------------------------------------

struct AcctState {
    spec: AccountSpec,
    registry: HashMap<i32, (FileId, Verdict)>,
    registry_denied: bool,
    teams_denied: bool,
    transcripts_denied: bool,
    truncated: bool,
    /// Transcripts whose read hit `EACCES`/`EPERM` in the current registry
    /// pass (the file, its project dir or the config root) — A33.4.
    denied_transcripts: HashSet<PathBuf>,
    /// A transcript search in the current pass could not look everywhere
    /// (`projects/` or a project dir unreadable).
    search_denied: bool,
}

impl AcctState {
    fn new(spec: AccountSpec) -> AcctState {
        AcctState {
            spec,
            registry: HashMap::new(),
            registry_denied: false,
            teams_denied: false,
            transcripts_denied: false,
            truncated: false,
            denied_transcripts: HashSet::new(),
            search_denied: false,
        }
    }

    /// `<cfg>/<sub>` as a root. `Err(PermissionDenied)` = the config root (or
    /// a dir above it) cannot be searched — never "missing" (A33.4, §5.8).
    fn root(&self, sub: &str) -> Result<Root, ReadError> {
        Root::new(&self.spec.config_dir.join(sub))
    }
}

/// A live session we emitted `Alive` for.
#[derive(Debug, Clone)]
struct Tracked {
    hint: IdentityHint,
}

#[derive(Debug, Clone)]
struct TranscriptCache {
    id: FileId,
    head: HeadMeta,
    tail: TailMeta,
}

fn hint_key(h: &IdentityHint) -> String {
    format!(
        "{:?}|{}|{}|{}",
        h.vendor,
        h.account_id.as_ref().map(|a| a.as_str()).unwrap_or(""),
        h.vendor_session_id.as_deref().unwrap_or(""),
        h.process.as_ref().map(|p| p.as_str()).unwrap_or("")
    )
}

fn fact_kind(f: &SourceFacts) -> &'static str {
    match f {
        SourceFacts::Gilde(_) => "gilde",
        SourceFacts::Process(_) => "process",
        SourceFacts::Stream(_) => "stream",
        SourceFacts::Registry(_) => "registry",
        SourceFacts::Transcript(_) => "transcript",
        SourceFacts::Team(_) => "team",
        SourceFacts::Pty(_) => "pty",
        SourceFacts::Meta(_) => "meta",
        SourceFacts::SourceDenied(_) => "denied",
        // Discovery sends no spend (A10.4: T04/T05/T08 are the producers).
        SourceFacts::Usage(_) => "usage",
    }
}

fn digest(f: &SourceFacts) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    format!("{f:?}").hash(&mut h);
    h.finish()
}

/// Memory figures in 10 MB steps (< 1 GB) / 100 MB steps (≥ 1 GB) (§11.2).
pub fn quantize_memory(bytes: u64) -> u64 {
    const MB: u64 = 1_000_000;
    let step = if bytes < 1_000 * MB { 10 * MB } else { 100 * MB };
    (bytes / step) * step
}

/// The start of the local calendar day containing `now` (ms).
pub fn local_day_start(now: EpochMs) -> EpochMs {
    let secs = (now / 1000) as libc::time_t;
    // SAFETY: zeroed `tm` is a valid out-parameter; localtime_r/mktime are
    // re-entrant libc calls.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&secs, &mut tm).is_null() {
            return now - now.rem_euclid(86_400_000);
        }
        tm.tm_hour = 0;
        tm.tm_min = 0;
        tm.tm_sec = 0;
        tm.tm_isdst = -1;
        let t = libc::mktime(&mut tm);
        if t < 0 {
            return now - now.rem_euclid(86_400_000);
        }
        t as i64 * 1000
    }
}

/// The synchronous engine (one thread owns it; tests drive it directly).
pub struct Engine {
    cfg: DiscoveryConfig,
    sink: Arc<dyn EvidenceSink>,
    scanner: Scanner,
    accounts: Vec<AcctState>,
    tracked: HashMap<(i32, i64), Tracked>,
    emitted: HashMap<(String, &'static str), u64>,
    locations: HashSet<String>,
    teams: HashMap<(usize, String), TeamDirRead>,
    team_obs: HashMap<(usize, String), u64>,
    inboxes: InboxCache,
    transcript_paths: HashMap<String, PathBuf>,
    transcripts: HashMap<PathBuf, TranscriptCache>,
    sources: HashMap<(Vendor, SourceKind), SourceStatus>,
    sources_sent: HashMap<(Vendor, SourceKind), SourceStatus>,
    siblings: BTreeSet<PathBuf>,
    orchestrator_present: Option<bool>,
    jobs: HashMap<String, JobState>,
    history_only_sent: HashSet<String>,
    /// A Claude process exited: re-read the team dirs at once (a lead that
    /// died leaves its dir behind; `Team.ended_at`, §5.2.4).
    teams_dirty: bool,
    frontmost: bool,
    last_scan_up: Option<u64>,
    last_reconcile_up: Option<u64>,
    pending: HashMap<(RootTag, PathBuf), u64>,
    backoff: Backoff,
    diag: DiscoveryDiagnostics,
    snapshot: Option<ProcSnapshot>,
    kq: Option<KqWatcher>,
    fs: Option<FsWatch>,
    watch_specs: Vec<WatchSpec>,
    tx: Option<Sender<Cmd>>,
}

impl Engine {
    pub fn new(cfg: DiscoveryConfig, sink: Arc<dyn EvidenceSink>) -> Engine {
        let mut scanner = Scanner::new(cfg.source.clone(), cfg.redact.clone(), cfg.own.clone(), cfg.bundles.clone());
        scanner.set_scope(cfg.scan_scope.clone());
        let accounts = cfg.accounts.iter().cloned().map(AcctState::new).collect();
        super::sqlite::cleanup_leftovers(&cfg.app_data);
        Engine {
            cfg,
            sink,
            scanner,
            accounts,
            tracked: HashMap::new(),
            emitted: HashMap::new(),
            locations: HashSet::new(),
            teams: HashMap::new(),
            team_obs: HashMap::new(),
            inboxes: InboxCache::default(),
            transcript_paths: HashMap::new(),
            transcripts: HashMap::new(),
            sources: HashMap::new(),
            sources_sent: HashMap::new(),
            siblings: BTreeSet::new(),
            orchestrator_present: None,
            jobs: HashMap::new(),
            history_only_sent: HashSet::new(),
            teams_dirty: false,
            frontmost: true,
            last_scan_up: None,
            last_reconcile_up: None,
            pending: HashMap::new(),
            backoff: Backoff::default(),
            diag: DiscoveryDiagnostics::default(),
            snapshot: None,
            kq: None,
            fs: None,
            watch_specs: Vec::new(),
            tx: None,
        }
    }

    pub fn diagnostics(&self) -> DiscoveryDiagnostics {
        let mut d = self.diag.clone();
        let s = safe_read::stats();
        d.refused_opens = s.refused;
        d.eacces = s.eacces;
        d.non_utf8 = s.non_utf8;
        d.kq_tracked = self.kq.as_ref().map(|k| k.tracked_count() as u32).unwrap_or(0);
        d.live_sessions = self.tracked.len() as u32;
        d.skipped_backoff = self.backoff.skipped_total;
        d.inbox_reads = self.inboxes.reads;
        d.inbox_cache_hits = self.inboxes.hits;
        d.inbox_entries_dropped = self.inboxes.dropped;
        d
    }

    pub fn last_snapshot(&self) -> Option<&ProcSnapshot> {
        self.snapshot.as_ref()
    }

    pub fn scanner(&self) -> &Scanner {
        &self.scanner
    }

    pub fn accounts(&self) -> Vec<AccountSpec> {
        self.accounts.iter().map(|a| a.spec.clone()).collect()
    }

    pub fn set_frontmost(&mut self, frontmost: bool) {
        self.frontmost = frontmost;
    }

    // -- emission ------------------------------------------------------------

    fn emit(&mut self, hint: &IdentityHint, facts: SourceFacts) {
        let key = (hint_key(hint), fact_kind(&facts));
        let d = digest(&facts);
        if self.emitted.get(&key) == Some(&d) {
            return;
        }
        self.emitted.insert(key, d);
        self.diag.evidence_sent += 1;
        self.sink.evidence(EvidenceUpdate { hint: hint.clone(), facts, at: now_wall_ms() });
    }

    fn forget(&mut self, hint: &IdentityHint) {
        let k = hint_key(hint);
        self.emitted.retain(|(h, _), _| h != &k);
    }

    fn location_for(&mut self, cwd: &str) {
        if self.locations.contains(cwd) {
            return;
        }
        if let Some(facts) = location::identify(Path::new(cwd), &self.cfg.home).and_then(|i| i.facts()) {
            self.sink.location(facts);
        }
        self.locations.insert(cwd.to_string());
    }

    fn set_source(&mut self, vendor: Vendor, source: SourceKind, state: SourceState, hidden_stale: u32, indexed: u32, truncated: bool) {
        let st = SourceStatus {
            vendor,
            source,
            state,
            last_reconcile_at: Some(now_wall_ms()),
            hidden_stale,
            indexed,
            truncated,
        };
        self.sources.insert((vendor, source), st);
    }

    fn flush_sources(&mut self) {
        let mut out = Vec::new();
        for (k, v) in &self.sources {
            let changed = self.sources_sent.get(k).is_none_or(|old| {
                old.state != v.state || old.hidden_stale != v.hidden_stale || old.indexed != v.indexed || old.truncated != v.truncated
            });
            if changed {
                out.push((*k, v.clone()));
            }
        }
        for (k, v) in out {
            self.sink.source_status(v.clone());
            self.sources_sent.insert(k, v);
        }
    }

    // -- the scan --------------------------------------------------------------

    /// One libproc scan: snapshot, exits, process-only sessions.
    pub fn tick(&mut self) {
        self.tick_at(now_wall_ms(), now_up_ms());
    }

    /// [`Engine::tick`] at a given wall / uptime clock (tests drive the 10 s
    /// unregistered grace with it).
    pub fn tick_at(&mut self, now: EpochMs, up: u64) {
        let out = self.scanner.scan(now, up);
        self.diag.scans += 1;
        self.diag.procs = out.snapshot.procs.len() as u32;
        self.diag.candidates = self.scanner.procs().filter(|p| p.candidate).count() as u32;
        for gone in &out.vanished {
            self.process_gone(gone.0, gone.1, now);
        }
        if let Some(cb) = &self.cfg.on_snapshot {
            cb(&out.snapshot);
        }
        self.snapshot = Some(out.snapshot);
        self.last_scan_up = Some(up);
        self.registry_liveness();
        self.process_sessions(up);
        self.publish_live();
        if self.teams_dirty {
            self.teams_dirty = false;
            for idx in 0..self.accounts.len() {
                if self.accounts[idx].spec.vendor == Vendor::Claude {
                    self.read_teams(idx, None);
                }
            }
            self.flush_sources();
        }
    }

    fn process_gone(&mut self, pid: i32, start: i64, now: EpochMs) {
        if let Some(t) = self.tracked.remove(&(pid, start)) {
            if t.hint.vendor == Some(Vendor::Claude) {
                self.teams_dirty = true;
            }
            if let Some(kq) = &self.kq {
                kq.untrack(pid);
            }
            let hint = t.hint.clone();
            self.forget(&hint);
            self.emit(&hint, SourceFacts::Process(ProcessFacts::Gone { at: now }));
        }
    }

    /// A kqueue `NOTE_EXIT` (immediate; the scan would see it later).
    pub fn on_exit(&mut self, pid: i32, start: i64) {
        self.process_gone(pid, start, now_wall_ms());
    }

    fn track(&mut self, pid: i32, start: i64, hint: &IdentityHint) {
        self.tracked.insert((pid, start), Tracked { hint: hint.clone() });
        if let Some(kq) = &self.kq {
            let src = self.cfg.source.clone();
            if kq.track(pid, start, |p| src.bsd_info(p).map(|b| b.start_sec)) == Track::Full {
                self.diag.kq_full += 1;
            }
        }
    }

    fn alive_facts(p: &ScannedProc) -> SourceFacts {
        SourceFacts::Process(ProcessFacts::Alive { pid: p.pid, start_sec: p.start_sec, cpu_active_at: p.cpu_active_at })
    }

    fn subtree_rss(&self, pid: i32) -> u64 {
        let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
        let mut rss: HashMap<i32, u64> = HashMap::new();
        for p in self.scanner.procs() {
            children.entry(p.ppid).or_default().push(p.pid);
            rss.insert(p.pid, p.rss_bytes);
        }
        let mut total = 0u64;
        let mut stack = vec![pid];
        let mut seen = HashSet::new();
        while let Some(x) = stack.pop() {
            if !seen.insert(x) {
                continue;
            }
            total = total.saturating_add(*rss.get(&x).unwrap_or(&0));
            if let Some(c) = children.get(&x) {
                stack.extend(c.iter().copied());
            }
        }
        total
    }

    fn binary_ref(exec_path: &str, version: Option<&str>) -> Option<BinaryRef> {
        let real = std::fs::canonicalize(exec_path).ok()?;
        let st = safe_read::lstat(&real).ok()?;
        Some(BinaryRef {
            realpath: abs_path(&real)?,
            fingerprint: BinaryFingerprint { size: st.id.size, mtime_ms: st.id.mtime_ms, inode: st.id.inode },
            version: version.filter(|v| is_semver(v)).map(str::to_string),
            pidpath: abs_path(Path::new(exec_path)),
        })
    }

    /// Re-check registry liveness for entries whose process vanished.
    fn registry_liveness(&mut self) {
        let source = self.cfg.source.clone();
        for acct in &mut self.accounts {
            for (pid, (_, v)) in acct.registry.iter_mut() {
                if let Verdict::Live { start_sec, .. } = v {
                    let alive = source.bsd_info(*pid).map(|b| b.start_sec) == Some(*start_sec);
                    if !alive {
                        *v = Verdict::Stale { why: registry::StaleWhy::NotAlive };
                    }
                }
            }
        }
    }

    fn registered_pids(&self) -> HashSet<i32> {
        let mut s = HashSet::new();
        for a in &self.accounts {
            for (pid, (_, v)) in &a.registry {
                if matches!(v, Verdict::Live { .. } | Verdict::UnknownFormat { live: Some(_), .. } | Verdict::Spare) {
                    s.insert(*pid);
                }
            }
        }
        s
    }

    /// The account whose `teams/<team>` exists.
    fn account_of_team(&self, team: &str) -> Option<usize> {
        self.accounts
            .iter()
            .position(|a| a.spec.vendor == Vendor::Claude && a.spec.config_dir.join("teams").join(team).is_dir())
            // A denied config root hides the dir: the last known account
            // keeps the teammate's identity (A33.4, last facts kept).
            .or_else(|| self.teams.keys().filter(|(_, d)| d == team).map(|(i, _)| *i).min())
    }

    fn member_backend(&self, acct: usize, team: &str, agent_id: &str) -> Option<String> {
        let read = self.teams.get(&(acct, team.to_string()))?;
        read.config.as_ref()?.members.iter().find(|m| m.agent_id == agent_id)?.backend_type.clone()
    }

    fn codex_account(&self) -> Option<AccountId> {
        self.accounts.iter().find(|a| a.spec.vendor == Vendor::Codex).map(|a| a.spec.id.clone())
    }

    /// Sessions known from processes only: teammates, unregistered Claude
    /// processes (after 10 s), codex/cursor/agy processes, own processes.
    fn process_sessions(&mut self, now_up: u64) {
        let registered = self.registered_pids();
        let procs: Vec<ScannedProc> = self.scanner.procs().filter(|p| p.candidate).cloned().collect();
        for p in procs {
            if registered.contains(&p.pid) {
                continue; // the registry path owns it
            }
            let Some(vendor) = p.vendor else { continue };
            if !matches!(p.role, Some(ProcRole::Session) | Some(ProcRole::Teammate)) {
                continue; // helpers, pane servers: not sessions [F-47]
            }
            let ancestry = p.ancestry.clone().unwrap_or_default();
            let other_gilde = p.own.is_none() && p.gilde_app_instance == Some(true);
            let mut hint = IdentityHint {
                vendor: Some(vendor),
                process: Some(p.key.clone()),
                gilde_spawn_id: p.own.as_ref().map(|o| o.spawn_id.clone()),
                ..IdentityHint::default()
            };
            let mut meta = SessionMetaFacts {
                cwd: p.cwd.as_deref().and_then(|c| real_abs_path(Path::new(c))),
                owner: ancestry.owner.clone(),
                model: p.labels.model.clone(),
                ..SessionMetaFacts::default()
            };
            let mut teams_denied = false;
            match vendor {
                Vendor::Claude if p.role == Some(ProcRole::Teammate) => {
                    let Some(t) = p.labels.teammate.clone() else { continue };
                    let team = t.team_name.clone().unwrap_or_default();
                    let acct = self.account_of_team(&team);
                    let backend = acct.and_then(|a| self.member_backend(a, &team, t.agent_id.as_deref().unwrap_or("")));
                    if let Some(a) = acct {
                        let acct_id = self.accounts[a].spec.id.clone();
                        hint.account_id = Some(acct_id.clone());
                        if let (Some(h), Some(name)) = (&self.cfg.history, t.agent_name.as_deref()) {
                            hint.vendor_session_id = h.teammate_session(&acct_id, &team, name);
                        }
                    }
                    meta.origin = Some(claude_origin(OriginInputs {
                        teammate: true,
                        member_backend: backend.as_deref(),
                        ..OriginInputs::default()
                    }));
                    meta.teammate = Some(t);
                    teams_denied = acct.is_some_and(|a| self.accounts[a].teams_denied);
                }
                Vendor::Claude => {
                    if p.own.is_none() && now_up.saturating_sub(p.first_seen_up_ms) < UNREGISTERED_GRACE_MS {
                        continue; // it may be starting (§5.2.2)
                    }
                    meta.origin = Some(claude_origin(OriginInputs {
                        other_gilde,
                        cco_team_member: p.cco_team_member,
                        terminal_ancestor: ancestry.terminal_ancestor,
                        ..OriginInputs::default()
                    }));
                }
                Vendor::Codex => {
                    hint.account_id = self.codex_account();
                    hint.vendor_session_id = p.labels.resume.clone();
                    meta.origin = Some(if p.labels.app_server && !other_gilde {
                        Origin::VsCode
                    } else {
                        other_vendor_origin(&ancestry, other_gilde)
                    });
                    if let (Some(h), Some(id)) = (&self.cfg.history, &p.labels.resume) {
                        if let Some(row) = h.row_of(Vendor::Codex, id) {
                            meta.transcript_path = abs_path(&row.path);
                        }
                    }
                }
                Vendor::Cursor => {
                    hint.account_id = Some(AccountId::derive_default_login(Vendor::Cursor));
                    let chats = self.cfg.cursor_dir.as_ref().map(|d| d.join("chats"));
                    hint.vendor_session_id = p.labels.resume.clone().filter(|id| {
                        chats.as_ref().zip(p.cwd.as_deref()).is_some_and(|(c, cwd)| cursor::chat_exists(c, cwd, id))
                    });
                    meta.origin = Some(other_vendor_origin(&ancestry, other_gilde));
                }
                Vendor::Agy => {
                    hint.account_id = Some(AccountId::derive_default_login(Vendor::Agy));
                    hint.vendor_session_id = p.labels.conversation.clone().filter(|id| {
                        self.cfg.agy_dir.as_ref().is_some_and(|d| agy::conversation_exists(d, id))
                    });
                    meta.origin = Some(other_vendor_origin(&ancestry, other_gilde));
                }
            }
            if let Some(path) = p.exec_path.as_deref() {
                meta.binary = Self::binary_ref(path, None);
            }
            meta.memory_bytes = Some(quantize_memory(self.subtree_rss(p.pid)));
            if let Some(c) = p.cwd.clone() {
                self.location_for(&c);
            }
            self.emit(&hint, Self::alive_facts(&p));
            self.emit(&hint, SourceFacts::Meta(Box::new(meta)));
            if teams_denied || self.emitted.contains_key(&(hint_key(&hint), "denied")) {
                self.emit(&hint, SourceFacts::SourceDenied(teams_denied));
            }
            self.track(p.pid, p.start_sec, &hint);
        }
    }

    fn publish_live(&mut self) {
        let Some(h) = &self.cfg.history else { return };
        let mut live = HashSet::new();
        for t in self.tracked.values() {
            if let (Some(v), Some(sid)) = (t.hint.vendor, t.hint.vendor_session_id.clone()) {
                live.insert((v, sid));
            }
        }
        h.set_live(live);
    }

    // -- the registry ------------------------------------------------------------

    /// Read every registry file of one account (listing + changed files).
    fn read_registry(&mut self, idx: usize) {
        let root = match self.accounts[idx].root("sessions") {
            Ok(r) => r,
            Err(ReadError::PermissionDenied) => {
                // The config root is unreadable: keep the last records (A33.4).
                self.accounts[idx].registry_denied = true;
                return;
            }
            Err(_) => {
                self.accounts[idx].registry_denied = false;
                return;
            }
        };
        let (files, truncated) = match registry::list_registry(&root) {
            Ok(v) => v,
            Err(ReadError::PermissionDenied) => {
                self.accounts[idx].registry_denied = true; // keep the last records
                return;
            }
            Err(_) => return,
        };
        self.accounts[idx].registry_denied = false;
        self.accounts[idx].truncated = truncated;
        let up = now_up_ms();
        let present: HashSet<i32> = files.iter().map(|f| f.pid).collect();
        self.accounts[idx].registry.retain(|pid, _| present.contains(pid));
        for f in files {
            let unchanged = self.accounts[idx].registry.get(&f.pid).is_some_and(|(id, _)| *id == f.id);
            if unchanged {
                continue;
            }
            self.read_registry_file(idx, &root, &f.path, f.pid, up);
        }
    }

    fn read_registry_file(&mut self, idx: usize, root: &Root, path: &Path, pid: i32, up: u64) {
        if self.backoff.skipping(path, up) {
            return;
        }
        match registry::read_registry(root, path, pid, self.cfg.registry_retry) {
            Ok((parsed, id)) => {
                self.backoff.ok(path);
                if matches!(parsed, Err(registry::FormatError::Parse)) {
                    self.diag.registry_torn += 1;
                }
                let v = registry::verdict(parsed, pid, self.cfg.source.as_ref());
                self.accounts[idx].registry.insert(pid, (id, v));
            }
            Err(ReadError::NotFound) => {
                self.accounts[idx].registry.remove(&pid);
            }
            Err(ReadError::PermissionDenied) => self.accounts[idx].registry_denied = true,
            Err(e) if e.is_transient() => {
                self.backoff.fail(path, up);
            }
            Err(_) => {}
        }
    }

    fn transcript_for(&mut self, idx: usize, sid: &str) -> Option<PathBuf> {
        if let Some(p) = self.transcript_paths.get(sid).cloned() {
            match safe_read::lstat(&p) {
                Ok(st) if st.kind == safe_read::EntryKind::File => return Some(p),
                // Its project dir (or the config root) is unreadable: the
                // transcript is still there as far as we know (A33.4).
                Err(ReadError::PermissionDenied) => {
                    self.accounts[idx].denied_transcripts.insert(p.clone());
                    return Some(p);
                }
                _ => {}
            }
        }
        let projects = match self.accounts[idx].root("projects") {
            Ok(r) => r,
            Err(ReadError::PermissionDenied) => {
                self.accounts[idx].search_denied = true;
                return None;
            }
            Err(_) => return None,
        };
        let (found, denied) = transcripts::find_transcript_checked(&projects, sid);
        self.accounts[idx].search_denied |= denied && found.is_none();
        let p = found?;
        self.transcript_paths.insert(sid.to_string(), p.clone());
        Some(p)
    }

    fn scan_transcript(&mut self, idx: usize, path: &Path) -> Option<TranscriptCache> {
        let denied = |e: &mut Engine| {
            e.accounts[idx].denied_transcripts.insert(path.to_path_buf());
            e.transcripts.get(path).cloned()
        };
        let projects = match self.accounts[idx].root("projects") {
            Ok(r) => r,
            Err(ReadError::PermissionDenied) => return denied(self),
            Err(_) => return None,
        };
        let id = match safe_read::lstat(path) {
            Ok(st) => st.id,
            Err(ReadError::PermissionDenied) => return denied(self),
            Err(_) => return None,
        };
        if let Some(c) = self.transcripts.get(path) {
            if c.id == id {
                return Some(c.clone());
            }
        }
        let f = match safe_read::open_in(&projects, path) {
            Ok(f) => f,
            Err(ReadError::PermissionDenied) => return denied(self),
            Err(_) => return None,
        };
        let head = transcripts::scan_head(&f).ok()?;
        let tail = transcripts::scan_tail(&f, TailMode::Live).ok()?;
        self.diag.transcripts_scanned += 1;
        let c = TranscriptCache { id: f.id, head, tail };
        if self.transcripts.len() > 512 {
            self.transcripts.clear();
        }
        self.transcripts.insert(path.to_path_buf(), c.clone());
        Some(c)
    }

    /// Evidence for every registry entry of one account.
    fn registry_evidence(&mut self, idx: usize) {
        let acct_id = self.accounts[idx].spec.id.clone();
        let denied = self.accounts[idx].registry_denied;
        // Transcript denials are recomputed every pass, so access coming
        // back clears them (A33.4).
        self.accounts[idx].denied_transcripts.clear();
        self.accounts[idx].search_denied = false;
        let entries: Vec<(i32, Verdict)> = self.accounts[idx].registry.iter().map(|(p, (_, v))| (*p, v.clone())).collect();
        let (mut live, mut stale, mut weak, mut unknown, mut spares) = (0u32, 0u32, 0u32, 0u32, 0u32);
        for (pid, v) in entries {
            match v {
                Verdict::Live { record, weak: w, start_sec, exec_path } => {
                    live += 1;
                    weak += u32::from(w);
                    self.live_registry_evidence(idx, &acct_id, pid, start_sec, &exec_path, &record, denied);
                }
                Verdict::UnknownFormat { live: Some((start_sec, exec_path)), .. } => {
                    unknown += 1;
                    let p = self.scanner.get(pid, start_sec).cloned();
                    let hint = IdentityHint {
                        vendor: Some(Vendor::Claude),
                        account_id: Some(acct_id.clone()),
                        process: Some(ProcessKey::new(pid as u32, start_sec.max(0) as u64)),
                        gilde_spawn_id: p.as_ref().and_then(|p| p.own.as_ref()).map(|o| o.spawn_id.clone()),
                        ..IdentityHint::default()
                    };
                    self.emit(&hint, SourceFacts::Process(ProcessFacts::Alive {
                        pid,
                        start_sec,
                        cpu_active_at: p.as_ref().and_then(|p| p.cpu_active_at),
                    }));
                    self.emit(&hint, SourceFacts::Registry(RegistryFacts {
                        status: None,
                        waiting_for: None,
                        status_updated_at: None,
                        peer_protocol_ok: false,
                    }));
                    let meta = SessionMetaFacts {
                        cwd: p.as_ref().and_then(|p| p.cwd.as_deref()).and_then(|c| real_abs_path(Path::new(c))),
                        binary: Self::binary_ref(&exec_path, None),
                        origin: Some(Origin::Unknown),
                        ..SessionMetaFacts::default()
                    };
                    self.emit(&hint, SourceFacts::Meta(Box::new(meta)));
                    self.track(pid, start_sec, &hint);
                }
                Verdict::UnknownFormat { live: None, .. } => unknown += 1,
                Verdict::Stale { .. } => stale += 1,
                Verdict::Spare => spares += 1,
            }
        }
        let a = &mut self.accounts[idx];
        a.transcripts_denied = a.search_denied || !a.denied_transcripts.is_empty();
        self.diag.registry_live += live;
        self.diag.registry_stale += stale;
        self.diag.registry_weak += weak;
        self.diag.registry_unknown_format += unknown;
        self.diag.registry_spares += spares;
    }

    /// Registry evidence for every Claude account, with the aggregated
    /// `Registry` source status (any account denied → `PermissionDenied`).
    fn registry_evidence_all(&mut self) {
        self.diag.registry_live = 0;
        self.diag.registry_stale = 0;
        self.diag.registry_weak = 0;
        self.diag.registry_unknown_format = 0;
        self.diag.registry_spares = 0;
        let mut any = false;
        for idx in 0..self.accounts.len() {
            if self.accounts[idx].spec.vendor == Vendor::Claude {
                any = true;
                self.registry_evidence(idx);
            }
        }
        if any {
            let denied = self.accounts.iter().any(|a| a.registry_denied);
            let truncated = self.accounts.iter().any(|a| a.truncated);
            let state = if denied { SourceState::PermissionDenied } else { SourceState::Ok };
            let (stale, live) = (self.diag.registry_stale, self.diag.registry_live);
            self.set_source(Vendor::Claude, SourceKind::Registry, state, stale, live, truncated);
            self.transcripts_source();
        }
    }

    /// The aggregated `Transcripts` source status (any account denied →
    /// `PermissionDenied`).
    fn transcripts_source(&mut self) {
        let denied = self.accounts.iter().any(|a| a.transcripts_denied);
        let (total, _) = self.cfg.history.as_ref().map(|h| h.coverage()).unwrap_or((0, 0));
        let trunc = self.cfg.history.as_ref().is_some_and(|h| h.truncated());
        let st = if denied { SourceState::PermissionDenied } else { SourceState::Ok };
        self.set_source(Vendor::Claude, SourceKind::Transcripts, st, 0, total, trunc);
    }

    #[allow(clippy::too_many_arguments)]
    fn live_registry_evidence(
        &mut self,
        idx: usize,
        acct_id: &AccountId,
        pid: i32,
        start_sec: i64,
        exec_path: &str,
        rec: &registry::RegistryRecord,
        denied: bool,
    ) {
        let p = self.scanner.get(pid, start_sec).cloned();
        let own = p.as_ref().and_then(|p| p.own.clone()).or_else(|| self.cfg.own.lookup(pid, start_sec));
        let hint = IdentityHint {
            vendor: Some(Vendor::Claude),
            account_id: Some(acct_id.clone()),
            vendor_session_id: Some(rec.session_id.clone()),
            process: Some(ProcessKey::new(pid as u32, start_sec.max(0) as u64)),
            gilde_spawn_id: own.as_ref().map(|o| o.spawn_id.clone()),
        };
        let search_before = self.accounts[idx].search_denied;
        self.accounts[idx].search_denied = false;
        let transcript = self.transcript_for(idx, &rec.session_id);
        let cache = transcript.as_ref().and_then(|t| self.scan_transcript(idx, t));
        // This session's own transcript could not be read (or searched).
        let transcript_denied = match &transcript {
            Some(t) => self.accounts[idx].denied_transcripts.contains(t),
            None => self.accounts[idx].search_denied,
        };
        self.accounts[idx].search_denied |= search_before;
        let ancestry = p.as_ref().and_then(|p| p.ancestry.clone()).unwrap_or_default();
        let other_gilde = own.is_none()
            && (p.as_ref().and_then(|p| p.gilde_app_instance) == Some(true) || rec.entrypoint.as_deref() == Some("gilde"));
        let origin = claude_origin(OriginInputs {
            teammate: false,
            member_backend: None,
            other_gilde,
            kind_bg: rec.kind == Some(RegKind::Bg),
            entrypoint: rec.entrypoint.as_deref(),
            cco_team_member: p.as_ref().and_then(|p| p.cco_team_member),
            terminal_ancestor: ancestry.terminal_ancestor,
        });
        let cwd_raw = cache.as_ref().and_then(|c| c.head.cwd.clone()).or_else(|| rec.cwd.clone());
        let title = rec
            .user_name
            .clone()
            .or_else(|| cache.as_ref().and_then(|c| c.tail.user_name.clone()))
            .or_else(|| cache.as_ref().and_then(|c| c.tail.custom_title.clone()))
            .or_else(|| cache.as_ref().and_then(|c| c.tail.ai_title.clone()))
            .or_else(|| match cache.as_ref().map(|c| &c.head.first_prompt) {
                Some(transcripts::FirstPrompt::Found(fp)) => Some(fp.chars().take(60).collect()),
                _ => None,
            })
            .and_then(|t| display_text(&t, TextCap::Chars(120)));
        let meta = SessionMetaFacts {
            cwd: cwd_raw.as_deref().and_then(|c| real_abs_path(Path::new(c))),
            started_at: rec.started_at,
            model: p.as_ref().and_then(|p| p.labels.model.clone()),
            permission_mode: cache.as_ref().and_then(|c| c.tail.permission_mode.or(c.head.permission_mode)),
            origin: Some(origin),
            owner: ancestry.owner.clone(),
            binary: Self::binary_ref(exec_path, rec.version.as_deref()),
            claude_version: rec.version.clone().filter(|v| is_semver(v)),
            transcript_path: transcript.as_deref().and_then(abs_path),
            title,
            teammate: None,
            memory_bytes: Some(quantize_memory(self.subtree_rss(pid))),
        };
        if let Some(c) = &cwd_raw {
            self.location_for(c);
        }
        self.emit(&hint, SourceFacts::Process(ProcessFacts::Alive {
            pid,
            start_sec,
            cpu_active_at: p.as_ref().and_then(|p| p.cpu_active_at),
        }));
        self.emit(&hint, SourceFacts::Registry(RegistryFacts {
            status: rec.status,
            waiting_for: rec.waiting_for.clone(),
            status_updated_at: rec.status_updated_at,
            peer_protocol_ok: true,
        }));
        self.emit(&hint, SourceFacts::Meta(Box::new(meta)));
        if let Some(c) = cache {
            let mut facts: TranscriptFacts = c.tail.facts.clone();
            facts.last_row_at = c.tail.last_row_at;
            self.emit(&hint, SourceFacts::Transcript(facts));
        }
        let denied_any = denied || transcript_denied;
        if denied_any || self.emitted.contains_key(&(hint_key(&hint), "denied")) {
            self.emit(&hint, SourceFacts::SourceDenied(denied_any));
        }
        if rec.kind == Some(RegKind::Bg) {
            if let (Some(job), Ok(root)) = (rec.job_id.clone(), self.accounts[idx].root("")) {
                if let Ok(Some(js)) = jobs::read_job_state(&root, &job) {
                    self.jobs.insert(job, js);
                }
            }
        }
        self.track(pid, start_sec, &hint);
    }

    // -- teams ---------------------------------------------------------------------

    fn live_teammates(&self) -> Vec<(LiveTeammate, ScannedProc)> {
        self.scanner
            .procs()
            .filter(|p| p.role == Some(ProcRole::Teammate))
            .filter_map(|p| {
                let t = p.labels.teammate.as_ref()?;
                Some((
                    LiveTeammate {
                        team_name: t.team_name.clone()?,
                        agent_id: t.agent_id.clone()?,
                        parent_session_id: t.parent_session_id.clone(),
                        pid: p.pid,
                    },
                    p.clone(),
                ))
            })
            .collect()
    }

    fn live_leads(&self, idx: usize) -> Vec<LiveLeadCandidate> {
        self.accounts[idx]
            .registry
            .iter()
            .filter_map(|(pid, (_, v))| match v {
                Verdict::Live { record, .. } => Some(LiveLeadCandidate {
                    session_id: record.session_id.clone(),
                    pid: *pid,
                    started_at: record.started_at,
                    cwd: record.cwd.as_deref().map(|c| {
                        std::fs::canonicalize(c).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| c.to_string())
                    }),
                }),
                _ => None,
            })
            .collect()
    }

    /// Read every team dir of one account (or only `only`).
    fn read_teams(&mut self, idx: usize, only: Option<&str>) {
        let teams_root = match self.accounts[idx].root("teams") {
            Ok(r) => r,
            Err(ReadError::PermissionDenied) => {
                // The config root is unreadable: teams keep their last state,
                // none ends because a read failed (A33.4, PM-19).
                self.accounts[idx].teams_denied = true;
                self.teams_source();
                return;
            }
            Err(_) => {
                self.accounts[idx].teams_denied = false;
                self.teams_gone_all(idx, only);
                self.teams_source();
                return;
            }
        };
        let dirs: Vec<String> = match safe_read::list_dir(&teams_root, teams_root.path(), safe_read::DIR_ENTRIES_CAP) {
            Ok(l) => {
                self.accounts[idx].teams_denied = false;
                l.entries.into_iter().filter(|e| e.kind == safe_read::EntryKind::Dir).map(|e| e.name).collect()
            }
            Err(ReadError::PermissionDenied) => {
                self.accounts[idx].teams_denied = true;
                self.teams_source();
                return;
            }
            Err(_) => Vec::new(),
        };
        let present: HashSet<String> = dirs.iter().cloned().collect();
        let known: Vec<String> = self.teams.keys().filter(|(i, _)| *i == idx).map(|(_, d)| d.clone()).collect();
        let now = now_wall_ms();
        for d in known {
            if !present.contains(&d) && only.is_none_or(|o| o == d) {
                self.teams.remove(&(idx, d.clone()));
                self.team_obs.remove(&(idx, d.clone()));
                self.cfg.teams.team_gone(&self.accounts[idx].spec.id, &d, now);
            }
        }
        let teammates = self.live_teammates();
        let leads = self.live_leads(idx);
        let hosted: Vec<(i32, EpochMs, Option<String>)> =
            self.cfg.own.hosted_leads().into_iter().map(|h| (h.pid, h.spawned_at, Some(h.config_dir))).collect();
        let tasks_root = self.accounts[idx].root("tasks").ok();
        let mut seen = 0u32;
        let mut hidden = 0u32;
        for dir in dirs {
            if only.is_some_and(|o| o != dir) {
                continue;
            }
            let prev = self.teams.get(&(idx, dir.clone())).cloned();
            let (read, inbox_ids) = match teams::read_team_dir_cached(&teams_root, &dir, prev.as_ref(), &self.cfg.team_retries, &mut self.inboxes) {
                Ok(r) => r,
                Err(ReadError::PermissionDenied) => {
                    self.accounts[idx].teams_denied = true;
                    continue;
                }
                Err(_) => continue,
            };
            self.diag.team_torn += u64::from(read.torn);
            let tm: Vec<LiveTeammate> = teammates.iter().map(|(t, _)| t.clone()).collect();
            let join = teams::join_lead(&dir, read.config.as_ref(), &tm, &leads, &hosted);
            let lead_alive = join.lead_pid.is_some_and(|p| self.cfg.source.bsd_info(p).is_some());
            let tasks = tasks_root
                .as_ref()
                .and_then(|r| teams::read_tasks(r, &r.join(&dir)).ok())
                .unwrap_or_default();
            let in_process = match (&join.lead_session_id, self.accounts[idx].root("projects").ok()) {
                (Some(sid), Some(projects)) => self
                    .transcript_for(idx, sid)
                    .map(|t| teams::read_inprocess(&projects, &t))
                    .unwrap_or_default(),
                _ => Vec::new(),
            };
            let acct_id = self.accounts[idx].spec.id.clone();
            let live_members: Vec<LiveMember> = teammates
                .iter()
                .filter(|(t, _)| t.team_name == dir)
                .map(|(t, p)| {
                    let name = p.labels.teammate.as_ref().and_then(|l| l.agent_name.clone()).unwrap_or_default();
                    LiveMember {
                        agent_id: t.agent_id.clone(),
                        session_id: self.cfg.history.as_ref().and_then(|h| h.teammate_session(&acct_id, &dir, &name)),
                        agent_name: name,
                        process: p.key.clone(),
                        pid: p.pid,
                    }
                })
                .collect();
            let visible = lead_alive || !live_members.is_empty();
            seen += 1;
            hidden += u32::from(!visible);
            // S4 facts for live pane members.
            self.member_facts(&acct_id, &read, &live_members);
            let obs = TeamObservation {
                account: acct_id,
                dir: read.clone(),
                join,
                lead_alive,
                live_members,
                tasks,
                in_process,
                visible,
                at: now,
            };
            // The change digest names each inbox by its file identity, never
            // by its entries (a 10 k inbox costs no formatting, A33.5(d)).
            let d = {
                let mut h = std::collections::hash_map::DefaultHasher::new();
                format!(
                    "{:?}{:?}{}{}{}{:?}{:?}{:?}{:?}{:?}{}",
                    obs.dir.config,
                    obs.dir.config_id,
                    obs.dir.roster_stale,
                    obs.dir.torn,
                    obs.dir.inboxes_skipped,
                    inbox_ids,
                    obs.join,
                    obs.live_members,
                    obs.tasks,
                    obs.in_process,
                    obs.lead_alive
                )
                .hash(&mut h);
                h.finish()
            };
            self.teams.insert((idx, dir.clone()), read);
            if self.team_obs.get(&(idx, dir.clone())) != Some(&d) {
                self.team_obs.insert((idx, dir.clone()), d);
                self.diag.team_observations += 1;
                self.cfg.teams.team(obs);
            }
        }
        if only.is_none() {
            self.diag.teams_seen = seen;
            self.diag.teams_hidden = hidden;
        }
        self.teams_source();
    }

    /// The aggregated `Teams` and `Tasks` source statuses.
    fn teams_source(&mut self) {
        let denied = self.accounts.iter().any(|a| a.teams_denied);
        let state = if denied { SourceState::PermissionDenied } else { SourceState::Ok };
        let known = self.teams.len() as u32;
        self.set_source(Vendor::Claude, SourceKind::Teams, state.clone(), 0, known, false);
        self.set_source(Vendor::Claude, SourceKind::Tasks, state, 0, 0, false);
    }

    fn teams_gone_all(&mut self, idx: usize, only: Option<&str>) {
        let now = now_wall_ms();
        let known: Vec<String> = self.teams.keys().filter(|(i, _)| *i == idx).map(|(_, d)| d.clone()).collect();
        for d in known {
            if only.is_none_or(|o| o == d) {
                self.teams.remove(&(idx, d.clone()));
                self.team_obs.remove(&(idx, d.clone()));
                self.cfg.teams.team_gone(&self.accounts[idx].spec.id, &d, now);
            }
        }
    }

    /// S4 facts per live pane member: roster `isActive`, the lead inbox's
    /// `permission_request` attributed to it, its last `idle_notification`.
    fn member_facts(&mut self, acct: &AccountId, read: &TeamDirRead, live: &[LiveMember]) {
        let roster = read.config.as_ref().map(|c| c.members.clone()).unwrap_or_default();
        let lead_inbox = read.inboxes.get("team-lead").cloned().unwrap_or_default();
        for m in live {
            let row = roster.iter().find(|r| r.agent_id == m.agent_id);
            let perm = lead_inbox
                .iter()
                .filter_map(|e| e.frame.as_ref().map(|f| (e, f)))
                .filter(|(_, f)| f.kind == "permission_request" && f.agent_id.as_deref() == Some(m.agent_name.as_str()))
                .filter_map(|(e, f)| Some((f.timestamp.or(e.timestamp)?, f.tool_name.clone().unwrap_or_default())))
                .max_by_key(|(t, _)| *t);
            let idle = lead_inbox
                .iter()
                .filter(|e| e.from == m.agent_name)
                .filter_map(|e| e.frame.as_ref().map(|f| (e, f)))
                .filter(|(_, f)| f.kind == "idle_notification")
                .filter_map(|(e, f)| Some((f.timestamp.or(e.timestamp)?, f.idle_reason?)))
                .max_by_key(|(t, _)| *t);
            let facts = TeamFileFacts {
                is_active: row.and_then(|r| r.is_active),
                in_roster: row.is_some(),
                permission_request: perm,
                last_idle: idle,
            };
            let start = self.scanner.by_pid(m.pid).map(|p| p.start_sec).unwrap_or(0);
            let hint = IdentityHint {
                vendor: Some(Vendor::Claude),
                account_id: Some(acct.clone()),
                vendor_session_id: m.session_id.clone(),
                process: Some(ProcessKey::new(m.pid as u32, start.max(0) as u64)),
                gilde_spawn_id: self.scanner.by_pid(m.pid).and_then(|p| p.own.as_ref()).map(|o| o.spawn_id.clone()),
            };
            self.emit(&hint, SourceFacts::Team(facts));
        }
    }

    // -- reconcile ------------------------------------------------------------------

    /// Pass 1 (B7) and every reconcile: siblings, registries, transcripts of
    /// live sessions, teams/tasks, orchestrator presence, history-only
    /// sessions of today, source statuses.
    pub fn reconcile(&mut self) {
        self.diag.reconciles += 1;
        self.discover_siblings();
        if self.last_scan_up.is_none() {
            self.tick();
        }
        for idx in 0..self.accounts.len() {
            if self.accounts[idx].spec.vendor != Vendor::Claude {
                continue;
            }
            self.read_registry(idx);
        }
        self.scanner.refresh_own();
        self.tick_sessions_after_registry();
        for idx in 0..self.accounts.len() {
            if self.accounts[idx].spec.vendor == Vendor::Claude {
                self.read_teams(idx, None);
            }
        }
        self.orchestrator_check();
        self.history_today();
        self.history_sources();
        if let Some(h) = &self.cfg.history {
            if self.diag.reconciles > 1 {
                h.kick(); // the stat pass for new and changed files (B11 keeps it current)
            }
        }
        self.source_statuses();
        self.flush_sources();
        self.last_reconcile_up = Some(now_up_ms());
        if self.cfg.watch {
            self.ensure_watchers();
        }
    }

    fn tick_sessions_after_registry(&mut self) {
        self.registry_evidence_all();
        // Anything tracked under a process key that is no longer alive.
        let now = now_wall_ms();
        let dead: Vec<(i32, i64)> = self
            .tracked
            .keys()
            .filter(|(pid, start)| self.cfg.source.bsd_info(*pid).map(|b| b.start_sec) != Some(*start))
            .copied()
            .collect();
        for (pid, start) in dead {
            self.process_gone(pid, start, now);
        }
        self.process_sessions(now_up_ms());
        self.publish_live();
    }

    fn discover_siblings(&mut self) {
        let Ok(found) = siblings::discover(&self.cfg.home) else { return };
        for s in found {
            if self.siblings.contains(&s.path) {
                continue;
            }
            self.siblings.insert(s.path.clone());
            if self.accounts.iter().any(|a| a.spec.config_dir == s.path) {
                continue;
            }
            if let Some(acct) = siblings::account_of(&s) {
                let mut spec = AccountSpec::new(Vendor::Claude, &s.path);
                spec.discovered = true;
                self.accounts.push(AcctState::new(spec));
                self.sink.account(acct);
            }
        }
        self.diag.sibling_accounts = self.accounts.iter().filter(|a| a.spec.discovered).count() as u32;
    }

    fn orchestrator_check(&mut self) {
        let present = orchestrator::present(&self.cfg.home);
        if self.orchestrator_present != Some(present) {
            self.orchestrator_present = Some(present);
            self.diag.orchestrator_present = present;
            if let Some(cb) = &self.cfg.on_orchestrator {
                cb(present);
            }
            if let Some(kq) = &self.kq {
                if present {
                    let _ = kq.watch_dir(&orchestrator::root_of(&self.cfg.home), TOKEN_ORCH);
                } else {
                    kq.unwatch_dir(TOKEN_ORCH);
                }
            }
        }
    }

    fn history_sources(&mut self) {
        let Some(h) = &self.cfg.history else { return };
        let s = HistorySources {
            claude: self.accounts.iter().filter(|a| a.spec.vendor == Vendor::Claude).map(|a| (a.spec.id.clone(), a.spec.config_dir.clone())).collect(),
            codex: self.accounts.iter().filter(|a| a.spec.vendor == Vendor::Codex).map(|a| (a.spec.id.clone(), a.spec.config_dir.clone())).collect(),
            cursor_chats: self.cfg.cursor_dir.as_ref().map(|d| d.join("chats")),
            agy_dir: self.cfg.agy_dir.clone(),
            home: self.cfg.home.clone(),
            app_data: self.cfg.app_data.clone(),
        };
        if h.sources() != s {
            h.set_sources(s);
        }
    }

    /// Sessions that ended today and are not live: `HistoryOnly` evidence, so
    /// "◌ n ended today" is right after a late launch (§5.9 store membership).
    fn history_today(&mut self) {
        let Some(h) = self.cfg.history.clone() else { return };
        let since = local_day_start(now_wall_ms());
        let live: HashSet<String> = self.tracked.values().filter_map(|t| t.hint.vendor_session_id.clone()).collect();
        for row in h.rows_since(since) {
            let Some(sid) = row.vendor_session_id.clone() else { continue };
            if live.contains(&sid) || self.history_only_sent.contains(&sid) || row.is_teammate {
                continue;
            }
            self.history_only_sent.insert(sid.clone());
            let hint = IdentityHint {
                vendor: Some(row.vendor),
                account_id: row.account_id.clone(),
                vendor_session_id: Some(sid),
                ..IdentityHint::default()
            };
            let meta = SessionMetaFacts {
                cwd: row.cwd.as_deref().and_then(|c| real_abs_path(Path::new(c))),
                started_at: row.first_row_at,
                transcript_path: abs_path(&row.path),
                ..SessionMetaFacts::default()
            };
            if let Some(c) = row.cwd.clone() {
                self.location_for(&c);
            }
            self.emit(&hint, SourceFacts::Process(ProcessFacts::HistoryOnly));
            self.emit(&hint, SourceFacts::Meta(Box::new(meta)));
            let mut facts = transcripts::empty_facts();
            facts.last_row_at = row.last_row_at;
            self.emit(&hint, SourceFacts::Transcript(facts));
        }
    }

    fn source_statuses(&mut self) {
        let any_claude = self.accounts.iter().any(|a| a.spec.vendor == Vendor::Claude);
        if any_claude {
            self.transcripts_source();
            self.set_source(Vendor::Claude, SourceKind::Jobs, SourceState::Ok, 0, self.jobs.len() as u32, false);
        }
        self.set_source(Vendor::Claude, SourceKind::Processes, SourceState::Ok, 0, self.diag.candidates, false);
        if let Some(acct) = self.accounts.iter().find(|a| a.spec.vendor == Vendor::Codex) {
            let present = acct.spec.config_dir.join("sessions").is_dir();
            let st = if present { SourceState::Ok } else { SourceState::Unavailable { reason: "notInstalled".into() } };
            self.set_source(Vendor::Codex, SourceKind::Rollouts, st.clone(), 0, 0, false);
            self.set_source(Vendor::Codex, SourceKind::ThreadIndex, st, 0, 0, false);
        }
        if let Some(d) = &self.cfg.cursor_dir {
            let st = if d.join("chats").is_dir() { SourceState::Ok } else { SourceState::Unavailable { reason: "notInstalled".into() } };
            self.set_source(Vendor::Cursor, SourceKind::Chats, st, 0, 0, false);
        }
        if let Some(d) = &self.cfg.agy_dir {
            let st = if d.is_dir() { SourceState::Ok } else { SourceState::Unavailable { reason: "notInstalled".into() } };
            self.set_source(Vendor::Agy, SourceKind::Summaries, st, 0, 0, false);
        }
    }

    // -- FS signals -------------------------------------------------------------------

    /// Queue an FS signal (debounced per path; `Rescan` with an empty path).
    pub fn on_fs(&mut self, sig: FsSignal) {
        let now = now_up_ms();
        let (tag, path) = match sig {
            FsSignal::Changed { tag, path } => (tag, path),
            FsSignal::Rescan { tag } => {
                self.diag.fs_rescans += 1;
                (tag, PathBuf::new())
            }
        };
        let due = now + tag.latency_ms().max(DEBOUNCE_MS);
        self.pending.entry((tag, path)).or_insert(due);
    }

    /// The earliest pending deadline (uptime ms).
    pub fn next_pending(&self) -> Option<u64> {
        self.pending.values().copied().min()
    }

    /// Handle every pending FS signal whose window elapsed (`force` = all).
    pub fn flush_fs(&mut self, force: bool) {
        let now = now_up_ms();
        let due: Vec<(RootTag, PathBuf)> = self
            .pending
            .iter()
            .filter(|(_, d)| force || **d <= now)
            .map(|(k, _)| k.clone())
            .collect();
        if due.is_empty() {
            return;
        }
        let mut rescan_scan = false;
        let mut kick_history = false;
        for k in &due {
            self.pending.remove(k);
        }
        for (tag, path) in due {
            let rescan = path.as_os_str().is_empty();
            match tag {
                RootTag::ClaudeSessions(i) => {
                    let i = usize::from(i);
                    if i < self.accounts.len() {
                        if rescan {
                            self.read_registry(i);
                        } else if let (Some(pid), Ok(root)) = (
                            path.file_name().and_then(|n| n.to_str()).and_then(registry::registry_pid),
                            self.accounts[i].root("sessions"),
                        ) {
                            let up = now_up_ms();
                            self.read_registry_file(i, &root, &path, pid, up);
                        }
                        rescan_scan = true;
                    }
                }
                RootTag::ClaudeTeams(i) | RootTag::ClaudeTasks(i) => {
                    let i = usize::from(i);
                    let base = if matches!(tag, RootTag::ClaudeTeams(_)) { "teams" } else { "tasks" };
                    if i < self.accounts.len() {
                        let team = path
                            .strip_prefix(self.accounts[i].spec.config_dir.join(base))
                            .ok()
                            .and_then(|r| r.components().next())
                            .and_then(|c| c.as_os_str().to_str())
                            .map(str::to_string);
                        match team {
                            Some(t) if !rescan => self.read_teams(i, Some(&t)),
                            _ => self.read_teams(i, None),
                        }
                        rescan_scan = true;
                    }
                }
                RootTag::ClaudeProjects(i) => {
                    let i = usize::from(i);
                    if let Some(h) = &self.cfg.history {
                        if !rescan {
                            h.invalidate(&path);
                        }
                    }
                    if i < self.accounts.len() && !rescan {
                        if let Some(sid) = transcripts::session_id_of(&path) {
                            if self.cfg.history.as_ref().is_some_and(|h| h.row_of(Vendor::Claude, &sid).is_none()) {
                                kick_history = true; // a new transcript: index it (a teammate's session id)
                            }
                            self.transcript_paths.insert(sid, path.clone());
                        }
                    }
                }
                RootTag::ClaudeJobs(_) => {}
                RootTag::CodexSessions(_) | RootTag::CodexIndex(_) | RootTag::CursorChats | RootTag::AgyDir => {
                    if let Some(h) = &self.cfg.history {
                        if !rescan {
                            h.invalidate(&path);
                        }
                    }
                }
                RootTag::Home => {
                    self.discover_siblings();
                    self.orchestrator_check();
                    self.history_sources();
                }
            }
        }
        if rescan_scan {
            self.tick(); // immediately on a registry or teams event (§5.2.2)
        }
        if kick_history {
            if let Some(h) = &self.cfg.history {
                h.kick();
            }
        }
        self.registry_evidence_all();
        self.flush_sources();
    }

    // -- watchers -----------------------------------------------------------------------

    /// The FSEvents roots this engine watches (§5.9).
    pub fn watch_specs(&self) -> Vec<WatchSpec> {
        self.specs()
    }

    fn specs(&self) -> Vec<WatchSpec> {
        let mut v = Vec::new();
        for (i, a) in self.accounts.iter().enumerate() {
            let i = i as u16;
            let d = &a.spec.config_dir;
            match a.spec.vendor {
                Vendor::Claude => {
                    for (tag, sub) in [
                        (RootTag::ClaudeSessions(i), "sessions"),
                        (RootTag::ClaudeTeams(i), "teams"),
                        (RootTag::ClaudeProjects(i), "projects"),
                        (RootTag::ClaudeTasks(i), "tasks"),
                        (RootTag::ClaudeJobs(i), "jobs"),
                    ] {
                        if let Ok(p) = std::fs::canonicalize(d.join(sub)) {
                            v.push(WatchSpec { tag, path: p });
                        }
                    }
                }
                Vendor::Codex => {
                    if let Ok(p) = std::fs::canonicalize(d.join("sessions")) {
                        v.push(WatchSpec { tag: RootTag::CodexSessions(i), path: p });
                    }
                    if let Ok(p) = std::fs::canonicalize(d) {
                        v.push(WatchSpec { tag: RootTag::CodexIndex(i), path: p });
                    }
                }
                _ => {}
            }
        }
        if let Some(c) = self.cfg.cursor_dir.as_ref().and_then(|d| std::fs::canonicalize(d.join("chats")).ok()) {
            v.push(WatchSpec { tag: RootTag::CursorChats, path: c });
        }
        if let Some(a) = self.cfg.agy_dir.as_ref().and_then(|d| std::fs::canonicalize(d).ok()) {
            v.push(WatchSpec { tag: RootTag::AgyDir, path: a });
        }
        v
    }

    fn ensure_watchers(&mut self) {
        let Some(tx) = self.tx.clone() else { return };
        let specs = self.specs();
        if self.fs.is_none() || specs != self.watch_specs {
            let t = tx.clone();
            self.fs = Some(FsWatch::start(specs.clone(), Arc::new(move |s| {
                let _ = t.send(Cmd::Fs(s));
            })));
            self.watch_specs = specs;
        }
        if self.kq.is_none() {
            let t = tx.clone();
            if let Ok(kq) = KqWatcher::start(Arc::new(move |e| {
                let _ = t.send(Cmd::Kq(e));
            })) {
                let _ = kq.watch_dir(&self.cfg.home, TOKEN_HOME);
                if self.orchestrator_present == Some(true) {
                    let _ = kq.watch_dir(&orchestrator::root_of(&self.cfg.home), TOKEN_ORCH);
                }
                let src = self.cfg.source.clone();
                for (pid, start) in self.tracked.keys() {
                    kq.track(*pid, *start, |p| src.bsd_info(p).map(|b| b.start_sec));
                }
                self.kq = Some(kq);
            }
        }
    }

    fn on_kq(&mut self, ev: KqEvent) {
        match ev {
            KqEvent::Exit { pid, start_sec } => self.on_exit(pid, start_sec),
            KqEvent::Dir { .. } => self.on_fs(FsSignal::Changed { tag: RootTag::Home, path: self.cfg.home.clone() }),
        }
    }

    fn scan_interval(&self) -> u64 {
        if self.frontmost {
            SCAN_FOCUSED_MS
        } else {
            SCAN_BACKGROUND_MS
        }
    }
}

fn is_semver(v: &str) -> bool {
    let parts: Vec<&str> = v.split('.').collect();
    parts.len() == 3 && parts.iter().all(|p| !p.is_empty() && p.len() <= 6 && p.bytes().all(|b| b.is_ascii_digit()))
}

// ---------------------------------------------------------------------------
// the thread
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// the team observer's own thread (A33.5(d))
// ---------------------------------------------------------------------------

enum TeamItem {
    Team(Box<TeamObservation>),
    Gone { account: AccountId, dir: String, at: EpochMs },
}

#[derive(Default)]
struct TeamQueueState {
    items: VecDeque<(String, TeamItem)>,
    busy: bool,
    stop: bool,
}

/// Team observations wait here for the team thread, so World's adapter work
/// never runs on the discovery thread. A newer observation of the same team
/// replaces one still waiting (each is a full read of the dir); `team_gone`
/// keeps its order.
#[derive(Default)]
struct TeamQueue {
    state: Mutex<TeamQueueState>,
    cv: Condvar,
}

impl TeamQueue {
    fn push(&self, key: String, item: TeamItem) {
        let Ok(mut st) = self.state.lock() else { return };
        if let TeamItem::Team(obs) = item {
            let last = st.items.iter_mut().rev().find(|(k, _)| *k == key);
            match last {
                Some((_, TeamItem::Team(waiting))) => *waiting = obs,
                _ => st.items.push_back((key, TeamItem::Team(obs))),
            }
        } else {
            st.items.push_back((key, item));
        }
        self.cv.notify_all();
    }

    /// Wait until every waiting observation was applied.
    fn drain(&self, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        let Ok(mut st) = self.state.lock() else { return false };
        while !st.items.is_empty() || st.busy {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return false;
            }
            st = match self.cv.wait_timeout(st, left) {
                Ok((g, _)) => g,
                Err(_) => return false,
            };
        }
        true
    }

    fn stop(&self) {
        if let Ok(mut st) = self.state.lock() {
            st.stop = true;
            self.cv.notify_all();
        }
    }

    fn run(&self, inner: &dyn TeamObserver) {
        loop {
            let item = {
                let Ok(mut st) = self.state.lock() else { return };
                loop {
                    if st.stop {
                        return;
                    }
                    if let Some((_, item)) = st.items.pop_front() {
                        st.busy = true;
                        break item;
                    }
                    st = match self.cv.wait(st) {
                        Ok(g) => g,
                        Err(_) => return,
                    };
                }
            };
            match item {
                TeamItem::Team(obs) => inner.team(*obs),
                TeamItem::Gone { account, dir, at } => inner.team_gone(&account, &dir, at),
            }
            if let Ok(mut st) = self.state.lock() {
                st.busy = false;
                self.cv.notify_all();
            }
        }
    }
}

/// The `TeamObserver` the threaded service gives its engine.
struct QueuedTeams(Arc<TeamQueue>);

impl TeamObserver for QueuedTeams {
    fn team(&self, obs: TeamObservation) {
        let key = format!("{}|{}", obs.account.as_str(), obs.dir.dir_name);
        self.0.push(key, TeamItem::Team(Box::new(obs)));
    }
    fn team_gone(&self, account: &AccountId, dir_name: &str, at: EpochMs) {
        let key = format!("{}|{dir_name}", account.as_str());
        self.0.push(key, TeamItem::Gone { account: account.clone(), dir: dir_name.to_string(), at });
    }
}

enum Cmd {
    Fs(FsSignal),
    Kq(KqEvent),
    Frontmost(bool),
    Wake,
    Reconcile,
    SetAccounts(Vec<AccountSpec>),
    Sync(Sender<DiscoveryDiagnostics>),
    Stop,
}

#[derive(Default)]
struct Shared {
    snapshot: Mutex<Option<ProcSnapshot>>,
}

/// The running discovery service (one thread at QoS utility, plus the team
/// observer's thread).
pub struct Discovery {
    tx: Sender<Cmd>,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    teams: Arc<TeamQueue>,
    teams_thread: Option<JoinHandle<()>>,
}

impl Discovery {
    /// Start: pass 1 runs at once on the discovery thread (B7), the history
    /// background pass (B11) after it.
    pub fn start(mut cfg: DiscoveryConfig, sink: Arc<dyn EvidenceSink>) -> Discovery {
        let (tx, rx) = mpsc::channel::<Cmd>();
        let shared = Arc::new(Shared::default());
        let thread_shared = shared.clone();
        let thread_tx = tx.clone();
        // A33.5(d): World's team work runs on its own thread, off the
        // discovery thread's critical path.
        let teams = Arc::new(TeamQueue::default());
        let observer = std::mem::replace(&mut cfg.teams, Arc::new(QueuedTeams(teams.clone())));
        let queue = teams.clone();
        let teams_thread = std::thread::Builder::new()
            .name("gilde-teams".into())
            .spawn(move || {
                super::set_thread_qos(false);
                queue.run(observer.as_ref());
            })
            .expect("spawn the team observer thread");
        let thread = std::thread::Builder::new()
            .name("gilde-discovery".into())
            .spawn(move || {
                super::set_thread_qos(false);
                let mut engine = Engine::new(cfg, sink);
                engine.tx = Some(thread_tx);
                engine.diag.team_observer_queued = true;
                run(engine, rx, thread_shared);
            })
            .expect("spawn the discovery thread");
        Discovery { tx, shared, thread: Some(thread), teams, teams_thread: Some(teams_thread) }
    }

    /// Window frontmost (3 s cadence) or not (15 s); frontmost scans at once.
    pub fn set_frontmost(&self, frontmost: bool) {
        let _ = self.tx.send(Cmd::Frontmost(frontmost));
    }

    /// Wake or window focus: scan + reconcile now.
    pub fn wake(&self) {
        let _ = self.tx.send(Cmd::Wake);
    }

    pub fn reconcile_now(&self) {
        let _ = self.tx.send(Cmd::Reconcile);
    }

    /// Replace the watched accounts (settings changed).
    pub fn set_accounts(&self, accounts: Vec<AccountSpec>) {
        let _ = self.tx.send(Cmd::SetAccounts(accounts));
    }

    /// The latest scan (T09's `SupervisorApi::last_scan`).
    pub fn last_snapshot(&self) -> Option<ProcSnapshot> {
        self.shared.snapshot.lock().ok().and_then(|s| s.clone())
    }

    /// Wait until every queued command ran and the team observer applied
    /// what they produced; returns the diagnostics (the caller waits, never
    /// the discovery thread).
    pub fn sync(&self, timeout: Duration) -> Option<DiscoveryDiagnostics> {
        let start = std::time::Instant::now();
        let (tx, rx) = mpsc::channel();
        self.tx.send(Cmd::Sync(tx)).ok()?;
        let d = rx.recv_timeout(timeout).ok()?;
        self.teams.drain(timeout.saturating_sub(start.elapsed())).then_some(d)
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        let _ = self.tx.send(Cmd::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        self.teams.stop();
        if let Some(t) = self.teams_thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run(mut engine: Engine, rx: Receiver<Cmd>, shared: Arc<Shared>) {
    engine.reconcile();
    if let Ok(mut s) = shared.snapshot.lock() {
        *s = engine.snapshot.clone();
    }
    let history_thread = engine.cfg.history.as_ref().map(|h| h.spawn_fill());
    loop {
        let now = now_up_ms();
        let scan_due = engine.last_scan_up.map_or(now, |t| t + engine.scan_interval());
        let rec_due = engine.last_reconcile_up.map_or(now, |t| t + RECONCILE_MS);
        let mut next = scan_due.min(rec_due);
        if let Some(p) = engine.next_pending() {
            next = next.min(p);
        }
        let wait = Duration::from_millis(next.saturating_sub(now).max(1));
        match rx.recv_timeout(wait) {
            Ok(Cmd::Fs(s)) => engine.on_fs(s),
            Ok(Cmd::Kq(e)) => engine.on_kq(e),
            Ok(Cmd::Frontmost(f)) => {
                let was = engine.frontmost;
                engine.set_frontmost(f);
                if f && !was {
                    engine.tick();
                }
            }
            Ok(Cmd::Wake) => {
                engine.tick();
                engine.reconcile();
            }
            Ok(Cmd::Reconcile) => engine.reconcile(),
            Ok(Cmd::SetAccounts(a)) => {
                engine.accounts = a.into_iter().map(AcctState::new).collect();
                engine.siblings.clear();
                engine.reconcile();
            }
            Ok(Cmd::Sync(tx)) => {
                engine.flush_fs(true);
                let _ = tx.send(engine.diagnostics());
            }
            Ok(Cmd::Stop) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
        let now = now_up_ms();
        if engine.next_pending().is_some_and(|d| d <= now) {
            engine.flush_fs(false);
        }
        if engine.last_scan_up.is_none_or(|t| now >= t + engine.scan_interval()) {
            engine.tick();
        }
        if engine.last_reconcile_up.is_none_or(|t| now >= t + RECONCILE_MS) {
            engine.reconcile();
        }
        if let Ok(mut s) = shared.snapshot.lock() {
            *s = engine.snapshot.clone();
        }
    }
    if let Some(h) = &engine.cfg.history {
        h.stop();
    }
    drop(engine);
    if let Some(t) = history_thread {
        let _ = t.join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantize_and_day_start() {
        assert_eq!(quantize_memory(19_999_999), 10_000_000);
        assert_eq!(quantize_memory(1_234_000_000), 1_200_000_000);
        let now = now_wall_ms();
        let d = local_day_start(now);
        assert!(d <= now && now - d < 86_400_000 + 3_600_000);
        assert!(is_semver("2.1.306"));
        assert!(!is_semver("2.1"));
    }
}
