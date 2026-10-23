//! Compat and canaries (contract §3.10) [C2, A1].
//!
//! Owner: T21. The `mod` lines below are pre-declared by T01 and frozen
//! after wave 0 (contract §15.1, F-18): a new module is a contract change.
//!
//! One [`ClaudeCompat`] per installed Claude version, from four inputs in
//! order — a later input may only **lower** a state, and only the shipped
//! table reaches `Verified`:
//!
//! 1. the shipped table ([`table`], `resources/claude-compat.json`);
//! 2. the static literal canary ([`canary`]) over each binary, keyed by its
//!    fingerprint `(size, mtime, inode)`; a version's canary is the minimum
//!    over its binaries (F-22);
//! 3. the runtime `initialize` handshake ([`CompatSink::handshake`]);
//! 4. runtime deviations ([`CompatSink::deviation`]).
//!
//! **Keyed on the binary actually spawned** [A1]: [`CompatApi::for_binary`]
//! takes the `BinaryRef` resolved at spawn and returns an owned record; the
//! session keeps that copy for its whole life. A replaced binary has a new
//! fingerprint (and usually a new version), so the next spawn gets a fresh
//! scan and record, and nothing a running session holds is touched.
//!
//! Work off the boot path (§2.4 B10): scans run one at a time on a single
//! worker thread at `QOS_CLASS_UTILITY`; [`CompatService::new`] reads the
//! cached rows once and scans nothing.

pub mod canary;
pub mod features;
pub mod notices;
pub mod sink;
pub mod table;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::sync::oneshot;

use crate::contract::common::{CompatState, Notice};
use crate::contract::compat::{
    below_minimum, parse_version, CanaryResult, ClaudeCompat, CompatCapability, CompatSource,
};
use crate::contract::services::{CompatApi, CompatSink};
use crate::contract::session::{BinaryFingerprint, BinaryRef};

use canary::{CanaryLiterals, CanarySet, ScanOutcome, Scanner, SetStatus};
use features::AppFeatures;
use sink::{ObservedRow, ObservedStore, RuntimeEvidence};
use table::{ShippedTable, Version};

/// A bundled data file failed its parse or lint (a build defect), or the
/// service could not start.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct CompatDataError(String);

impl CompatDataError {
    pub fn new(message: impl Into<String>) -> CompatDataError {
        CompatDataError(message.into())
    }

    pub fn message(&self) -> &str {
        &self.0
    }
}

/// How long a spawn waits for the static canary of a version never scanned
/// (§6.0 *Binary resolution*); past it the spawn proceeds with `Unknown`.
pub const SPAWN_WAIT: Duration = Duration::from_secs(1);

/// Hard stop for one scan. The budget is ≤ 1 s per binary (§11.1); this only
/// ends a stalled read, which then counts as `Unknown`, never `Failed` (PM-43).
pub const SCAN_DEADLINE: Duration = Duration::from_secs(10);

/// The version of the record returned for a binary whose `--version` could
/// not be read: below the minimum (Terminal mode), never listed.
pub const UNKNOWN_VERSION: &str = "0.0.0";

/// Note on that record.
pub const NOTE_VERSION_UNREADABLE: &str = "claude --version could not be read";

/// Called after any record may have changed (a scan finished, evidence
/// arrived); the owner of the store publisher re-reads [`CompatApi::list`]
/// and emits `Compat{list}` (§4.3).
pub type ChangeListener = Arc<dyn Fn() + Send + Sync>;

pub struct CompatConfig {
    pub table: ShippedTable,
    pub literals: CanaryLiterals,
    pub app_features: AppFeatures,
    pub spawn_wait: Duration,
    pub scan_deadline: Duration,
    /// `claude_compat_observed`; `None` = memory only.
    pub store: Option<Arc<dyn ObservedStore>>,
}

impl CompatConfig {
    /// The bundled data files with the contract's timings.
    pub fn bundled(store: Option<Arc<dyn ObservedStore>>) -> CompatConfig {
        CompatConfig {
            table: ShippedTable::bundled(),
            literals: CanaryLiterals::bundled(),
            app_features: AppFeatures::bundled(),
            spawn_wait: SPAWN_WAIT,
            scan_deadline: SCAN_DEADLINE,
            store,
        }
    }
}

/// The `CompatApi` + `CompatSink` provider (T21 → T02, T05, T07, T09, T10).
pub struct CompatService {
    shared: Arc<Shared>,
    jobs: mpsc::Sender<Job>,
}

struct Shared {
    table: ShippedTable,
    scanner: Scanner,
    sets_hash: String,
    app: AppFeatures,
    spawn_wait: Duration,
    scan_deadline: Duration,
    store: Option<Arc<dyn ObservedStore>>,
    state: Mutex<State>,
    listener: Mutex<Option<ChangeListener>>,
    scans: AtomicU64,
}

#[derive(Default)]
struct State {
    /// Every version seen (resolved, scanned, reported or restored).
    versions: BTreeMap<Version, RuntimeEvidence>,
    binaries: HashMap<BinaryFingerprint, BinaryEntry>,
}

struct BinaryEntry {
    realpath: Option<PathBuf>,
    version: Option<Version>,
    scan: ScanState,
    persisted: bool,
}

enum ScanState {
    Queued(Vec<oneshot::Sender<()>>),
    Done(ScanOutcome),
}

enum Job {
    Scan {
        fp: BinaryFingerprint,
        path: PathBuf,
    },
    Save(Box<ObservedRow>),
    Barrier(mpsc::Sender<()>),
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn version_text(v: Version) -> String {
    format!("{}.{}.{}", v.0, v.1, v.2)
}

/// The canary of one version: the minimum over its binaries (F-22).
struct CanarySummary {
    result: CanaryResult,
    sets: BTreeMap<CanarySet, SetStatus>,
}

impl CanarySummary {
    fn of(scans: &[&ScanOutcome]) -> CanarySummary {
        let mut sets = BTreeMap::new();
        for set in CanarySet::ALL {
            let statuses: Vec<SetStatus> = scans.iter().map(|s| s.set_status(set)).collect();
            let status = if statuses.contains(&SetStatus::Missing) {
                SetStatus::Missing
            } else if statuses.is_empty() || statuses.contains(&SetStatus::Unknown) {
                SetStatus::Unknown
            } else {
                SetStatus::Present
            };
            sets.insert(set, status);
        }
        let mut missing: BTreeSet<String> = BTreeSet::new();
        let mut unknown = false;
        for s in scans {
            match s.to_result() {
                CanaryResult::Missing { literals } => missing.extend(literals),
                CanaryResult::Unknown => unknown = true,
                CanaryResult::Pass | CanaryResult::NotRun => {}
            }
        }
        let result = if scans.is_empty() {
            CanaryResult::NotRun
        } else if !missing.is_empty() {
            CanaryResult::Missing {
                literals: missing.into_iter().collect(),
            }
        } else if unknown {
            CanaryResult::Unknown
        } else {
            CanaryResult::Pass
        };
        CanarySummary { result, sets }
    }

    fn status(&self, set: CanarySet) -> SetStatus {
        self.sets.get(&set).copied().unwrap_or(SetStatus::Unknown)
    }
}

impl Shared {
    /// Compose a record from the four inputs (§3.10).
    fn compose(
        &self,
        version: &str,
        v: Version,
        scans: &[&ScanOutcome],
        rt: &RuntimeEvidence,
    ) -> ClaudeCompat {
        let entry = self.table.get_exact(v);
        let summary = CanarySummary::of(scans);

        // 1. the shipped table: the only way to Verified
        let mut conversation = if entry.is_some_and(|e| e.conversation_verified) {
            CompatState::Verified
        } else {
            CompatState::Unverified
        };
        let mut pane_hosting = if entry.is_some_and(|e| e.pane_hosting_verified) {
            CompatState::Verified
        } else {
            CompatState::Unverified
        };
        let mut source = if entry.is_some() {
            CompatSource::Shipped
        } else {
            CompatSource::StaticCanary
        };

        // 2. the static canary: a missing pinned entry fails its capability
        if summary.status(CanarySet::Conversation) == SetStatus::Missing {
            conversation = CompatState::Failed;
            source = CompatSource::StaticCanary;
        }
        if summary.status(CanarySet::PaneHosting) == SetStatus::Missing {
            pane_hosting = CompatState::Failed;
            source = CompatSource::StaticCanary;
        }

        // 3. the handshake
        if rt.handshake_ok == Some(false) {
            conversation = CompatState::Failed;
            source = CompatSource::Handshake;
        }

        // 4. deviations
        if rt.conversation_failed {
            conversation = CompatState::Failed;
        }
        if rt.pane_hosting_failed {
            pane_hosting = CompatState::Failed;
        }
        if rt.deviated() {
            source = CompatSource::Observed;
        }

        let inherit = summary.result == CanaryResult::Pass && !rt.deviated();
        let team_store = match summary.status(CanarySet::TeamStore) {
            SetStatus::Present => Some(true),
            SetStatus::Missing => Some(false),
            SetStatus::Unknown => None,
        };
        ClaudeCompat {
            version: version.to_string(),
            conversation,
            pane_hosting,
            source,
            static_canary: summary.result,
            handshake_ok: rt.handshake_ok,
            below_minimum: below_minimum(version),
            note: rt.note().cloned(),
            features: features::effective(&self.table, v, inherit, team_store, &rt.features_off),
        }
    }

    /// The record of version `v` as the state holds it now.
    fn record(&self, st: &State, v: Version) -> ClaudeCompat {
        let scans: Vec<&ScanOutcome> = st
            .binaries
            .values()
            .filter(|b| b.version == Some(v))
            .filter_map(|b| match &b.scan {
                ScanState::Done(o) => Some(o),
                ScanState::Queued(_) => None,
            })
            .collect();
        let empty = RuntimeEvidence::default();
        let rt = st.versions.get(&v).unwrap_or(&empty);
        self.compose(&version_text(v), v, &scans, rt)
    }

    fn known(&self, st: &State, v: Version) -> bool {
        st.versions.contains_key(&v) || self.table.get_exact(v).is_some()
    }

    fn version_has_final_scan(st: &State, v: Version) -> bool {
        st.binaries
            .values()
            .any(|b| b.version == Some(v) && matches!(&b.scan, ScanState::Done(o) if o.is_final()))
    }

    fn changed(&self) {
        let listener = self.listener.lock().clone();
        if let Some(listener) = listener {
            listener();
        }
    }

    fn save(&self, row: &ObservedRow) {
        if let Some(store) = &self.store {
            if let Err(e) = store.save(row) {
                tracing::warn!(error = %e, "claude_compat_observed write failed; compat stays in memory");
            }
        }
    }

    fn run_scan(&self, fp: BinaryFingerprint, path: &Path) {
        self.scans.fetch_add(1, Ordering::Relaxed);
        let started = Instant::now();
        let outcome = self.scanner.scan_path(path, Some(&fp), self.scan_deadline);
        let kind = match &outcome {
            ScanOutcome::Scanned { missing } if missing.is_empty() => "pass",
            ScanOutcome::Scanned { .. } => "missing",
            ScanOutcome::Unknown(r) => r.as_str(),
        };
        // Never a path (it may sit under the home dir) and never content.
        tracing::info!(
            fingerprint = %fp.key(),
            result = kind,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "static canary"
        );
        let mut save = None;
        let waiters = {
            let mut st = self.state.lock();
            match st.binaries.get_mut(&fp) {
                Some(b) => {
                    if let Some(v) = b.version {
                        save = sink::canary_row(
                            &version_text(v),
                            &fp,
                            &outcome,
                            &self.sets_hash,
                            now_ms(),
                        );
                        b.persisted = save.is_some();
                    }
                    match std::mem::replace(&mut b.scan, ScanState::Done(outcome)) {
                        ScanState::Queued(w) => w,
                        ScanState::Done(_) => Vec::new(),
                    }
                }
                None => Vec::new(),
            }
        };
        for w in waiters {
            let _ = w.send(());
        }
        if let Some(row) = save {
            self.save(&row);
        }
        self.changed();
    }
}

fn worker(shared: Arc<Shared>, jobs: mpsc::Receiver<Job>) {
    canary::enter_utility_qos();
    while let Ok(job) = jobs.recv() {
        match job {
            Job::Scan { fp, path } => shared.run_scan(fp, &path),
            Job::Save(row) => shared.save(&row),
            Job::Barrier(done) => {
                let _ = done.send(());
            }
        }
    }
}

impl CompatService {
    /// Load the cached rows (one small table read) and start the canary
    /// worker. Scans nothing: boot schedules the canaries (B10).
    pub fn new(cfg: CompatConfig) -> Result<CompatService, CompatDataError> {
        let scanner = Scanner::new(&cfg.literals)?;
        let sets_hash = cfg.literals.hash().to_string();
        let mut state = State::default();
        if let Some(store) = &cfg.store {
            match store.load() {
                Ok(rows) => restore(&mut state, rows, &sets_hash),
                Err(e) => {
                    tracing::warn!(error = %e, "claude_compat_observed could not be read; starting empty")
                }
            }
        }
        let shared = Arc::new(Shared {
            table: cfg.table,
            scanner,
            sets_hash,
            app: cfg.app_features,
            spawn_wait: cfg.spawn_wait,
            scan_deadline: cfg.scan_deadline,
            store: cfg.store,
            state: Mutex::new(state),
            listener: Mutex::new(None),
            scans: AtomicU64::new(0),
        });
        let (tx, rx) = mpsc::channel();
        let for_worker = shared.clone();
        std::thread::Builder::new()
            .name("gilde-compat-canary".into())
            .spawn(move || worker(for_worker, rx))
            .map_err(|e| CompatDataError::new(format!("could not start the canary worker: {e}")))?;
        Ok(CompatService { shared, jobs: tx })
    }

    /// The bundled data files; `store` = `state.db` in the app.
    pub fn bundled(
        store: Option<Arc<dyn ObservedStore>>,
    ) -> Result<CompatService, CompatDataError> {
        CompatService::new(CompatConfig::bundled(store))
    }

    /// Register the change listener (replaces an earlier one).
    pub fn set_on_change(&self, listener: ChangeListener) {
        *self.shared.listener.lock() = Some(listener);
    }

    /// Block until every job queued so far has run (tests, quit). `false` on
    /// timeout. Never call it from the async runtime's own threads.
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let (tx, rx) = mpsc::channel();
        if self.jobs.send(Job::Barrier(tx)).is_err() {
            return false;
        }
        rx.recv_timeout(timeout).is_ok()
    }

    /// Scans started since the service was built (diagnostics, tests).
    pub fn scans_started(&self) -> u64 {
        self.shared.scans.load(Ordering::Relaxed)
    }

    /// The record for any version string, known or not (an unparseable one
    /// is treated as below the minimum).
    fn record_any(&self, version: &str) -> ClaudeCompat {
        match parse_version(version) {
            Some(v) => self.shared.record(&self.shared.state.lock(), v),
            None => {
                self.shared
                    .compose(UNKNOWN_VERSION, (0, 0, 0), &[], &RuntimeEvidence::default())
            }
        }
    }

    /// Queue a scan of `binary` unless one is done or queued; returns a
    /// waiter when `wait` and the scan is still to come.
    fn request_scan(
        &self,
        st: &mut State,
        binary: &BinaryRef,
        version: Option<Version>,
        wait: bool,
    ) -> Option<oneshot::Receiver<()>> {
        let fp = binary.fingerprint;
        let path = PathBuf::from(binary.realpath.as_str());
        // A new fingerprint at the same path replaces what was there [A1]:
        // forget that path's pending or retryable entries (final scans of the
        // old bytes stay: they are evidence for their version).
        st.binaries.retain(|k, b| {
            *k == fp
                || b.realpath.as_deref() != Some(path.as_path())
                || matches!(&b.scan, ScanState::Done(o) if o.is_final())
        });
        let mut needs_job = false;
        let entry = st.binaries.entry(fp).or_insert_with(|| {
            needs_job = true;
            BinaryEntry {
                realpath: None,
                version: None,
                scan: ScanState::Queued(Vec::new()),
                persisted: false,
            }
        });
        entry.realpath = Some(path.clone());
        if version.is_some() && entry.version != version {
            entry.version = version;
            entry.persisted = false;
        }
        if let ScanState::Done(o) = &entry.scan {
            if !o.is_final() {
                entry.scan = ScanState::Queued(Vec::new());
                needs_job = true;
            }
        }
        // A finished scan that just learned its version is persisted now.
        let mut save = None;
        if let (ScanState::Done(o), Some(v), false) = (&entry.scan, entry.version, entry.persisted)
        {
            save = sink::canary_row(&version_text(v), &fp, o, &self.shared.sets_hash, now_ms());
            entry.persisted = save.is_some();
        }
        let waiter = match (&mut entry.scan, wait) {
            (ScanState::Queued(waiters), true) => {
                let (tx, rx) = oneshot::channel();
                waiters.push(tx);
                Some(rx)
            }
            _ => None,
        };
        if let Some(row) = save {
            let _ = self.jobs.send(Job::Save(Box::new(row)));
        }
        if needs_job {
            let _ = self.jobs.send(Job::Scan { fp, path });
        }
        waiter
    }

    /// Record runtime evidence for a version, persist it, tell the listener.
    fn runtime(&self, version: &str, apply: impl FnOnce(&mut RuntimeEvidence) -> bool) {
        let Some(v) = parse_version(version) else {
            tracing::warn!("compat evidence for an unparseable version ignored");
            return;
        };
        let row = {
            let mut st = self.shared.state.lock();
            let rt = st.versions.entry(v).or_default();
            if !apply(rt) {
                return;
            }
            sink::version_row(&version_text(v), rt, now_ms())
        };
        if self.shared.store.is_some() {
            let _ = self.jobs.send(Job::Save(Box::new(row)));
        }
        self.shared.changed();
    }
}

/// Rebuild the state from `claude_compat_observed` rows.
fn restore(st: &mut State, rows: Vec<ObservedRow>, sets_hash: &str) {
    for row in rows {
        let Some(v) = parse_version(&row.version) else {
            continue;
        };
        let rt = st.versions.entry(v).or_default();
        if row.binary_fingerprint == sink::VERSION_ROW {
            *rt = sink::runtime_from_row(&row);
            continue;
        }
        let Some(fp) = sink::parse_fingerprint_key(&row.binary_fingerprint) else {
            continue;
        };
        // A scan made with another entry list is stale: the binary is scanned
        // again the next time it is seen.
        let Some(outcome) = sink::scan_from_row(&row, sets_hash) else {
            continue;
        };
        st.binaries.insert(
            fp,
            BinaryEntry {
                realpath: None,
                version: Some(v),
                scan: ScanState::Done(outcome),
                persisted: true,
            },
        );
    }
}

#[async_trait]
impl CompatApi for CompatService {
    fn list(&self) -> Vec<ClaudeCompat> {
        let st = self.shared.state.lock();
        let versions: BTreeSet<Version> = self
            .shared
            .table
            .entries()
            .map(|(v, _)| *v)
            .chain(st.versions.keys().copied())
            .collect();
        versions
            .into_iter()
            .map(|v| self.shared.record(&st, v))
            .collect()
    }

    fn lookup(&self, version: &str) -> Option<ClaudeCompat> {
        let v = parse_version(version)?;
        let st = self.shared.state.lock();
        self.shared
            .known(&st, v)
            .then(|| self.shared.record(&st, v))
    }

    async fn for_binary(&self, binary: &BinaryRef) -> ClaudeCompat {
        let version = binary.version.as_deref().and_then(parse_version);
        let (waiter, created) = {
            let mut st = self.shared.state.lock();
            let created = match version {
                Some(v) if !st.versions.contains_key(&v) => {
                    st.versions.insert(v, RuntimeEvidence::default());
                    true
                }
                _ => false,
            };
            // Wait only for a version never scanned to a verdict (§6.0).
            let wait = version.is_none_or(|v| !Shared::version_has_final_scan(&st, v));
            (self.request_scan(&mut st, binary, version, wait), created)
        };
        if created {
            self.shared.changed();
        }
        if let Some(waiter) = waiter {
            // Timeout or a stopped worker: the spawn proceeds with `Unknown`.
            let _ = tokio::time::timeout(self.shared.spawn_wait, waiter).await;
        }

        let st = self.shared.state.lock();
        let own = match st.binaries.get(&binary.fingerprint).map(|b| &b.scan) {
            Some(ScanState::Done(o)) => Some(o),
            _ => None,
        };
        let mut record = match version {
            Some(v) => self.shared.record(&st, v),
            None => {
                let scans: Vec<&ScanOutcome> = own.into_iter().collect();
                let rt = RuntimeEvidence {
                    plain_note: Some(NOTE_VERSION_UNREADABLE.to_string()),
                    ..RuntimeEvidence::default()
                };
                self.shared.compose(UNKNOWN_VERSION, (0, 0, 0), &scans, &rt)
            }
        };
        if own.is_none() && record.static_canary == CanaryResult::NotRun {
            // The wait ran out: a timeout = Unknown = Unverified (§6.0).
            record.static_canary = CanaryResult::Unknown;
        }
        record
    }

    fn feature(&self, version: &str, flag: &str) -> bool {
        self.record_any(version).features.get(flag) == Some(&true)
    }

    fn app_feature(&self, flag: &str) -> bool {
        self.shared.app.get(flag)
    }

    fn notices(&self, version: &str) -> Vec<Notice> {
        notices::notices_for(&self.record_any(version))
    }

    fn schedule_canary(&self, binary: &BinaryRef) {
        let version = binary.version.as_deref().and_then(parse_version);
        let created = {
            let mut st = self.shared.state.lock();
            let created = match version {
                Some(v) if !st.versions.contains_key(&v) => {
                    st.versions.insert(v, RuntimeEvidence::default());
                    true
                }
                _ => false,
            };
            let _ = self.request_scan(&mut st, binary, version, false);
            created
        };
        if created {
            self.shared.changed();
        }
    }
}

impl CompatSink for CompatService {
    fn handshake(&self, version: &str, ok: bool, note: Option<String>) {
        self.runtime(version, |rt| rt.apply_handshake(ok, note.as_deref()));
    }

    fn deviation(&self, version: &str, capability: CompatCapability, note: String) {
        if let CompatCapability::Feature(flag) = &capability {
            if !features::is_claude_flag(flag) {
                tracing::warn!(flag = %flag, "deviation for a flag the §1.3 table does not name");
            }
        }
        tracing::info!(capability = ?capability, "compat deviation recorded");
        self.runtime(version, |rt| rt.apply_deviation(&capability, &note));
    }

    fn note(&self, version: &str, note: String) {
        self.runtime(version, |rt| rt.apply_note(&note));
    }
}
