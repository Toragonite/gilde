//! The history index, the memory-only title cache and previews (contract
//! §5.9, §4.2.8) [C10].
//!
//! * **Index** (metadata only, [`IndexRow`]): one row per transcript /
//!   rollout / cursor chat / agy summary — `stat` of every file, head/tail
//!   only for new or changed files (`(size, mtime, inode)`). Optionally
//!   persisted through an [`IndexStore`] (`state.db` `history_index`, §3.9);
//!   no title, prompt or name column is ever written. Every store write goes
//!   through the one **index writer** thread (QoS utility, A51.4): the fill,
//!   the on-demand loads and the stat pass only queue paths, and no caller
//!   ever waits for SQLite.
//! * **Title cache** (**[content]**, memory only, rebuilt each launch, never
//!   written to disk): filled in two orders at once — the rows of the page
//!   History is showing are read first, on demand; a background pass at QoS
//!   background (≤ 25 % of one core) fills the rest.
//! * `history_query` searches the cache (NFC-normalized, Korean-safe matcher
//!   ported from the palette), reports `coverage.titlesLoaded / total`, pages
//!   with an opaque keyset cursor, and a newer `queryId` supersedes older ones.
//! * cursor rows are "{vendor} 대화 · {date}" (`label: vendorDate`, no title);
//!   cursor and agy rows are `titleOnly` (no preview).
//!
//! Every title, prompt and reply that leaves this module passes the one
//! display-text boundary (`discovery::display_text`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use unicode_normalization::UnicodeNormalization;

use crate::contract::common::{EpochMs, Vendor};
use crate::contract::display::TextCap;
use crate::contract::ids::{AccountId, HistoryCursor, ProjectId, SessionUid};
use crate::contract::ipc::{
    HistoryCoverage, HistoryLabel, HistoryPreviewRes, HistoryQueryReq, HistoryQueryRes, HistoryRow,
};
use crate::contract::services::IdentityKey;
use crate::contract::session::{ContinueOption, Origin};
use crate::contract::IpcError;
use crate::discovery::claude::transcripts::{self, FirstPrompt, TailMode};
use crate::discovery::safe_read::{self, EntryKind, FileId, Root};
use crate::discovery::{agy, codex, cursor, display_text, is_vendor_uuid, location, now_wall_ms};

/// Files indexed (newest by mtime) (§5.8).
pub const INDEX_CAP: usize = 20_000;
/// Rows loaded on demand by one query before it answers with what it has.
pub const ON_DEMAND_MAX: usize = 300;
/// The History title cap (Session titles are ≤ 120 chars, §3.4.4).
pub const TITLE_CHARS: usize = 120;
/// First prompt as a title: 60 chars (§3.4.4).
pub const PROMPT_TITLE_CHARS: usize = 60;
/// Preview fields: ≤ 400 chars (§4.2.8).
pub const PREVIEW_CHARS: usize = 400;
/// The index writer writes at most this many queued paths per batch, one
/// `BEGIN IMMEDIATE` transaction each (A51.4; T48b's 256 held the write
/// lock too long).
pub const INDEX_BATCH_ROWS: usize = 64;
/// … and a queued path waits at most this long for its batch (A51.4) —
/// plus the kernel's timer leeway: macOS coalesces a utility-QoS thread's
/// timed wait by up to 75 ms (`kern.timer_coalesce_tier3_ns_max`).
pub const INDEX_BATCH_INTERVAL: Duration = Duration::from_secs(1);
/// The index connection's `busy_timeout`, the store's (`store/mod.rs`, A51.3).
const INDEX_BUSY_TIMEOUT: Duration = Duration::from_millis(5_000);

/// What kind of file a row comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RowKind {
    Transcript,
    Rollout,
    CursorChat,
    AgySummary,
}

/// One index row (**[meta]** only).
#[derive(Debug, Clone, PartialEq)]
pub struct IndexRow {
    /// The file (agy rows: `<summaries db>#<id>`).
    pub path: PathBuf,
    pub kind: RowKind,
    pub vendor: Vendor,
    pub account_id: Option<AccountId>,
    pub vendor_session_id: Option<String>,
    pub file: FileId,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub entrypoint: Option<String>,
    pub is_teammate: bool,
    pub team_name: Option<String>,
    pub teammate_name: Option<String>,
    pub continued_in: Option<String>,
    pub turns: Option<u32>,
    pub first_row_at: Option<EpochMs>,
    pub last_row_at: Option<EpochMs>,
    pub scanned_at: EpochMs,
    /// Head/tail metadata loaded for the current `file` identity.
    pub loaded: bool,
}

impl IndexRow {
    fn new(path: PathBuf, kind: RowKind, vendor: Vendor, account_id: Option<AccountId>, sid: Option<String>, file: FileId) -> IndexRow {
        IndexRow {
            path,
            kind,
            vendor,
            account_id,
            vendor_session_id: sid,
            file,
            cwd: None,
            git_branch: None,
            entrypoint: None,
            is_teammate: false,
            team_name: None,
            teammate_name: None,
            continued_in: None,
            turns: None,
            first_row_at: None,
            last_row_at: None,
            scanned_at: 0,
            loaded: false,
        }
    }

    fn key(&self) -> Option<IdentityKey> {
        Some(IdentityKey {
            vendor: self.vendor,
            account_id: self.account_id.clone(),
            vendor_session_id: self.vendor_session_id.clone()?,
        })
    }
}

/// The memory-only cache entry (**[content]**).
#[derive(Debug, Clone, Default)]
struct Titles {
    /// The display title (sanitized, ≤ 120 chars).
    title: Option<String>,
    /// For the matcher: NFC + lowercase of `title`.
    folded: String,
    loaded: bool,
}

/// Where the index looks (set by the engine; accounts change at runtime).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistorySources {
    /// `(account, realpath of the config dir)`.
    pub claude: Vec<(AccountId, PathBuf)>,
    /// `(account, realpath of CODEX_HOME)`.
    pub codex: Vec<(AccountId, PathBuf)>,
    pub cursor_chats: Option<PathBuf>,
    pub agy_dir: Option<PathBuf>,
    pub home: PathBuf,
    pub app_data: PathBuf,
}

/// Session uids for history rows (World mints and persists them in
/// `session_meta`, §3.3.2; the wiring passes World's resolver).
pub trait UidResolver: Send + Sync {
    fn uid_for(&self, key: &IdentityKey) -> SessionUid;
    fn key_of(&self, uid: &SessionUid) -> Option<IdentityKey>;
}

/// An in-memory resolver with ephemeral uids — a **test double only**
/// (A11.4): the app wires World's resolver, so it is absent from builds
/// without `test-fixtures`.
#[cfg(any(test, feature = "test-fixtures"))]
#[derive(Default)]
pub struct MemoryUids {
    inner: Mutex<(HashMap<IdentityKey, SessionUid>, HashMap<SessionUid, IdentityKey>)>,
}

#[cfg(any(test, feature = "test-fixtures"))]
impl UidResolver for MemoryUids {
    fn uid_for(&self, key: &IdentityKey) -> SessionUid {
        let mut g = self.inner.lock().expect("uid map");
        if let Some(u) = g.0.get(key) {
            return u.clone();
        }
        let u = SessionUid::mint();
        g.0.insert(key.clone(), u.clone());
        g.1.insert(u.clone(), key.clone());
        u
    }
    fn key_of(&self, uid: &SessionUid) -> Option<IdentityKey> {
        self.inner.lock().ok()?.1.get(uid).cloned()
    }
}

/// Persistence of index metadata (`state.db` `history_index`).
///
/// In the app only the index writer thread calls the write methods (A51.4).
pub trait IndexStore: Send + Sync {
    fn load(&self) -> Vec<IndexRow>;
    fn upsert(&self, rows: &[IndexRow]);
    fn remove(&self, paths: &[PathBuf]);
    /// One batch of the index writer: every path of `remove` is deleted,
    /// then every row of `upsert` is written — in **one** transaction where
    /// the store has transactions ([`SqliteIndexStore`] does). The default
    /// is the two calls in that order.
    fn write(&self, remove: &[PathBuf], upsert: &[IndexRow]) {
        self.remove(remove);
        self.upsert(upsert);
    }
}

// ---------------------------------------------------------------------------
// the index writer (A51.4)
// ---------------------------------------------------------------------------

/// What the index writer does to one queued path. A batch applies, in its
/// one transaction, every remove first and then every upsert, which is the
/// order of the calls for each path: removed then loaded again ends up
/// stored (if still loaded at flush time), loaded then removed ends up gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PathOp {
    remove: bool,
    upsert: bool,
}

/// The index writer's queue. Each path is queued at most once, at the
/// position (and time) of its first call; a later call on a queued path
/// updates its [`PathOp`] in place. So the queue holds at most one entry per
/// path the index knows, and paths taken into a batch are queued afresh —
/// behind that batch — by a later call.
struct Pending {
    order: VecDeque<(PathBuf, Instant)>,
    ops: HashMap<PathBuf, PathOp>,
    /// [`INDEX_BATCH_INTERVAL`] (a test may shorten it).
    interval: Duration,
    /// Passes that may still queue rows ([`Producing`]): a stopping writer
    /// drains only once there are none.
    producers: usize,
    stopping: bool,
    /// The writer thread is gone: nothing is queued any more.
    exited: bool,
}

struct WriterShared {
    pending: Mutex<Pending>,
    wake: Condvar,
}

impl WriterShared {
    /// The queue is never left inconsistent by a panic (no code that can
    /// panic runs while it is locked), so a poisoned lock is still sound.
    fn lock(&self) -> MutexGuard<'_, Pending> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The one thread that writes the index store (A51.4): batches of
/// ≤ [`INDEX_BATCH_ROWS`] queued paths, a path at most
/// [`INDEX_BATCH_INTERVAL`] after it was queued, each batch one
/// [`IndexStore::write`] (one `BEGIN IMMEDIATE` transaction). The rows are
/// taken from the in-memory index at flush time, and a row no longer loaded
/// is not written (T48b's rule: never an older copy over a newer one). It
/// runs at QoS utility, so a busy Mac does not throttle it while it holds the
/// write lock, as it did the QoS-background fill before A51.5 (the priority
/// inversion of A51.4); callers only queue paths and never wait for it.
struct IndexWriter {
    shared: Arc<WriterShared>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

/// Held by a pass that queues rows (a [`HistoryService::fill`], the
/// background thread of [`HistoryService::spawn_fill`]). A stopping writer
/// drains and exits only once none is held, so the row in hand when `stop`
/// came, and the stat pass's removals, are written too.
struct Producing<'a>(&'a WriterShared);

impl Drop for Producing<'_> {
    fn drop(&mut self) {
        let mut p = self.0.lock();
        p.producers = p.producers.saturating_sub(1);
        let last = p.producers == 0;
        drop(p);
        if last {
            self.0.wake.notify_all();
        }
    }
}

impl IndexWriter {
    /// Start the writer thread over the service's in-memory index and its
    /// store. A thread that cannot be spawned leaves the index memory-only
    /// (logged), never a panic.
    fn start(state: Arc<Mutex<State>>, store: Arc<dyn IndexStore>) -> Option<IndexWriter> {
        let shared = Arc::new(WriterShared {
            pending: Mutex::new(Pending {
                order: VecDeque::new(),
                ops: HashMap::new(),
                interval: INDEX_BATCH_INTERVAL,
                producers: 0,
                stopping: false,
                exited: false,
            }),
            wake: Condvar::new(),
        });
        let s = shared.clone();
        match std::thread::Builder::new().name("gilde-history-writer".into()).spawn(move || run_writer(&s, &state, store.as_ref())) {
            Ok(h) => Some(IndexWriter { shared, thread: Mutex::new(Some(h)) }),
            Err(err) => {
                tracing::warn!(target: "discovery", error = %err, "history index writer not started; the index stays in memory");
                None
            }
        }
    }

    /// Queue `paths` for an upsert (`remove == false`) or a removal. Never
    /// waits for SQLite: the writer holds this lock only to take a batch.
    /// After the writer has exited (stopped and drained), nothing is queued.
    fn queue<'a>(&self, paths: impl IntoIterator<Item = &'a Path>, remove: bool) {
        let mut p = self.shared.lock();
        if p.exited {
            return;
        }
        let was_empty = p.order.is_empty();
        let now = Instant::now();
        for path in paths {
            let q = &mut *p;
            match q.ops.get_mut(path) {
                Some(op) if remove => *op = PathOp { remove: true, upsert: false },
                Some(op) => op.upsert = true,
                None => {
                    q.ops.insert(path.to_path_buf(), PathOp { remove, upsert: !remove });
                    q.order.push_back((path.to_path_buf(), now));
                }
            }
        }
        // Wake the writer only when it has something new to time or a full
        // batch: an idle writer sleeps without a timeout.
        let wake = (was_empty && !p.order.is_empty()) || p.order.len() >= INDEX_BATCH_ROWS;
        drop(p);
        if wake {
            self.shared.wake.notify_all();
        }
    }

    fn producing(&self) -> Producing<'_> {
        self.shared.lock().producers += 1;
        Producing(&self.shared)
    }

    /// Stop: the writer writes everything queued — as soon as no
    /// [`Producing`] pass is left — then exits.
    fn stop(&self) {
        self.shared.lock().stopping = true;
        self.shared.wake.notify_all();
    }

    /// Wait until the writer thread has exited (after [`Self::stop`]).
    fn join(&self) {
        let handle = self.thread.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(h) = handle {
            // A writer that died by a panic is joined too, and marked gone,
            // so nothing waits for it and nothing is queued for it.
            let _ = h.join();
            self.shared.lock().exited = true;
            self.shared.wake.notify_all();
        } else {
            // Another caller is joining: wait for the same exit.
            let mut p = self.shared.lock();
            while !p.exited {
                p = self.shared.wake.wait(p).unwrap_or_else(PoisonError::into_inner);
            }
        }
    }

    #[cfg(any(test, feature = "test-fixtures"))]
    fn set_interval(&self, interval: Duration) {
        self.shared.lock().interval = interval;
        self.shared.wake.notify_all();
    }
}

/// The writer thread: QoS utility, then batches until stopped and drained.
fn run_writer(shared: &WriterShared, state: &Mutex<State>, store: &dyn IndexStore) {
    crate::discovery::set_thread_qos(false);
    let mut p = shared.lock();
    loop {
        let draining = p.stopping && p.producers == 0;
        let Some(&(_, first)) = p.order.front() else {
            if draining {
                p.exited = true;
                drop(p);
                shared.wake.notify_all();
                return;
            }
            p = shared.wake.wait(p).unwrap_or_else(PoisonError::into_inner);
            continue;
        };
        let waited = first.elapsed();
        if !draining && p.order.len() < INDEX_BATCH_ROWS && waited < p.interval {
            let rest = p.interval - waited;
            p = shared.wake.wait_timeout(p, rest).map(|(g, _)| g).unwrap_or_else(|e| e.into_inner().0);
            continue;
        }
        let q = &mut *p;
        let n = q.order.len().min(INDEX_BATCH_ROWS);
        let batch: Vec<(PathBuf, PathOp)> = q.order.drain(..n).filter_map(|(path, _)| q.ops.remove(&path).map(|op| (path, op))).collect();
        drop(p);
        write_batch(state, store, batch);
        p = shared.lock();
    }
}

/// One batch: the queued upserts read from the in-memory index *now* (a row
/// no longer there, or no longer loaded, is not written), then one
/// [`IndexStore::write`]. A store that panics loses this batch, logged, and
/// the writer goes on.
fn write_batch(state: &Mutex<State>, store: &dyn IndexStore, batch: Vec<(PathBuf, PathOp)>) {
    let mut remove = Vec::new();
    let mut upsert = Vec::new();
    {
        let st = state.lock().unwrap_or_else(PoisonError::into_inner);
        for (path, op) in batch {
            if op.upsert {
                if let Some(r) = st.rows.get(&path).filter(|r| r.loaded) {
                    upsert.push(r.clone());
                }
            }
            if op.remove {
                remove.push(path);
            }
        }
    }
    if remove.is_empty() && upsert.is_empty() {
        return;
    }
    let rows = remove.len() + upsert.len();
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| store.write(&remove, &upsert))).is_err() {
        tracing::warn!(target: "discovery", rows, "history index write panicked; the batch is skipped");
    }
}

/// A token bucket for the `query` rate class (20/s).
struct Bucket {
    tokens: f64,
    at: Instant,
}

impl Bucket {
    const RATE: f64 = 20.0;
    const BURST: f64 = 20.0;

    fn take(&mut self) -> Result<(), u64> {
        let now = Instant::now();
        let dt = now.duration_since(self.at).as_secs_f64();
        self.at = now;
        self.tokens = (self.tokens + dt * Self::RATE).min(Self::BURST);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            Ok(())
        } else {
            Err((((1.0 - self.tokens) / Self::RATE) * 1000.0).ceil() as u64)
        }
    }
}

#[derive(Default)]
struct State {
    rows: HashMap<PathBuf, IndexRow>,
    titles: HashMap<PathBuf, Titles>,
    /// `(vendor, vendor session id)` → path.
    by_sid: HashMap<(Vendor, String), PathBuf>,
    truncated: bool,
}

/// Counters of one refresh.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RefreshStats {
    pub total: u32,
    pub loaded: u32,
    pub removed: u32,
    pub truncated: bool,
}

/// The history service (shared by the engine, the background pass and the
/// `history_*` commands).
pub struct HistoryService {
    /// Shared with the index writer, which reads rows at flush time.
    state: Arc<Mutex<State>>,
    sources: RwLock<HistorySources>,
    resolver: Arc<dyn UidResolver>,
    /// The one writer of the index store (A51.4); `None` when memory-only.
    writer: Option<IndexWriter>,
    live: Mutex<HashSet<(Vendor, String)>>,
    bucket: Mutex<Bucket>,
    latest_query: Mutex<Option<String>>,
    projects: Mutex<HashMap<String, Option<ProjectId>>>,
    codex_threads: Mutex<HashMap<String, codex::ThreadRow>>,
    agy_rows: Mutex<HashMap<String, agy::SummaryRow>>,
    stop: AtomicBool,
    /// Wakes the background thread for a refresh (reconcile, FS events).
    kick: (Mutex<bool>, Condvar),
    /// Test-only (A51.1): called by [`HistoryService::fill`] once per row,
    /// inside the row's measured work.
    #[cfg(any(test, feature = "test-fixtures"))]
    fill_row_hook: Mutex<Option<FillRowHook>>,
}

/// The writer drains what is queued and exits; its thread is joined.
impl Drop for HistoryService {
    fn drop(&mut self) {
        if let Some(w) = &self.writer {
            w.stop();
            w.join();
        }
    }
}

/// A closure [`HistoryService::fill`] calls once per row, with the row's
/// path, inside the work it measures — a **test hook only** (A51.1): absent
/// from builds without `test-fixtures`.
#[cfg(any(test, feature = "test-fixtures"))]
pub type FillRowHook = Arc<dyn Fn(&Path) + Send + Sync>;

/// The calling thread's CPU time, or `None` when the clock cannot be read.
///
/// The History fill rests by this clock, not by wall time (A51.1): a
/// low-QoS thread on a busy Mac mostly *waits* (scheduler starvation,
/// throttled I/O), and resting 3× the wall time multiplied that waiting into
/// rest — ≈ 79 ms of wall time for ≈ 1 ms of CPU per row, a fill at 0.3 % of
/// a core. CPU time ≤ wall time, so resting 3× it still holds the fill to
/// ≤ 25 % of one core.
fn thread_cpu_time() -> Option<Duration> {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `ts` is a valid out-parameter.
    if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) } != 0 {
        return None;
    }
    Some(Duration::new(u64::try_from(ts.tv_sec).ok()?, u32::try_from(ts.tv_nsec).ok()?))
}

/// The fill's rest after one row: 3× the row's thread CPU time (work 1,
/// rest 3: ≤ 25 % of a core). When either CPU reading is missing (or the
/// clock went backwards) it rests 3× the row's wall time instead, so the
/// fill never spins.
fn fill_rest(cpu_before: Option<Duration>, cpu_after: Option<Duration>, wall: Duration) -> Duration {
    let spent = match (cpu_before, cpu_after) {
        (Some(a), Some(b)) => b.checked_sub(a).unwrap_or(wall),
        _ => wall,
    };
    spent * 3
}

/// The `HistoryService` as Tauri managed state (`commands/history.rs`).
pub struct HistoryState(pub Arc<HistoryService>);

fn fold(s: &str) -> String {
    s.nfc().collect::<String>().to_lowercase()
}

/// The palette's Korean-safe matcher (`palette/commands.ts`): subsequence by
/// code point with bonuses for word starts, adjacency and position zero.
pub fn fuzzy_score(query: &str, candidate: &str) -> Option<i32> {
    let needles: Vec<char> = query.chars().filter(|c| !c.is_whitespace()).collect();
    let hay: Vec<char> = candidate.chars().collect();
    if needles.is_empty() {
        return Some(0);
    }
    if needles.len() > hay.len() {
        return None;
    }
    let mut score: i32 = 0;
    let mut cursor = 0usize;
    let mut previous: isize = -2;
    for n in needles {
        let found = (cursor..hay.len()).find(|&i| hay[i] == n)? as isize;
        if found == previous + 1 {
            score += 10;
        }
        if found == 0 {
            score += 12;
        } else if matches!(hay[(found - 1) as usize], ' ' | '-' | '_' | '/' | '.' | ':' | '·') {
            score += 8;
        }
        score -= ((found - previous - 1).min(6)) as i32;
        previous = found;
        cursor = found as usize + 1;
    }
    score -= (hay.len() as i32 / 8).min(8);
    Some(score)
}

fn path_hash(p: &Path) -> String {
    crate::contract::ids::hash16(p.as_os_str().as_encoded_bytes())
}

fn query_hash(req: &HistoryQueryReq) -> String {
    let f = &req.filters;
    let s = format!(
        "{}\0{:?}\0{:?}\0{:?}\0{:?}\0{:?}\0{:?}",
        fold(&req.text),
        f.project_id.as_ref().map(|p| p.as_str()),
        f.vendor,
        f.from,
        f.to,
        f.account_id.as_ref().map(|a| a.as_str()),
        f.origin
    );
    crate::contract::ids::hash16(s.as_bytes())[..8].to_string()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CursorPos {
    score: i32,
    mtime: i64,
    phash: String,
}

fn encode_cursor(qh: &str, pos: &CursorPos) -> Option<HistoryCursor> {
    let raw = format!("v1|{qh}|{}|{}|{}", pos.score, pos.mtime, pos.phash);
    HistoryCursor::parse(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw)).ok()
}

fn decode_cursor(c: &HistoryCursor, qh: &str) -> Option<CursorPos> {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(c.as_str()).ok()?;
    let s = String::from_utf8(bytes).ok()?;
    let mut it = s.split('|');
    if it.next()? != "v1" || it.next()? != qh {
        return None;
    }
    Some(CursorPos { score: it.next()?.parse().ok()?, mtime: it.next()?.parse().ok()?, phash: it.next()?.to_string() })
}

/// Order: score desc, mtime desc, path hash asc.
fn before(a: &CursorPos, b: &CursorPos) -> bool {
    (b.score, b.mtime, std::cmp::Reverse(&b.phash)) < (a.score, a.mtime, std::cmp::Reverse(&a.phash))
}

fn origin_of(row: &IndexRow) -> Origin {
    match row.vendor {
        Vendor::Claude if row.is_teammate => Origin::TerminalTeam,
        Vendor::Claude => match row.entrypoint.as_deref() {
            Some("claude-vscode") => Origin::VsCode,
            Some("sdk-cli") => Origin::Headless,
            Some("cli") => Origin::Terminal,
            _ => Origin::Unknown,
        },
        Vendor::Codex => match row.entrypoint.as_deref() {
            Some("vscode") => Origin::VsCode,
            Some("exec") => Origin::Headless,
            Some("cli") => Origin::Terminal,
            _ => Origin::Unknown,
        },
        Vendor::Cursor | Vendor::Agy => Origin::Terminal,
    }
}

impl HistoryService {
    /// The service over `store`'s rows. With a store it starts the index
    /// writer thread (A51.4), which the service owns: [`Self::stop`] drains
    /// and ends it, and dropping the service joins it. Without a store
    /// (memory-only) there is no writer and nothing is queued.
    pub fn new(sources: HistorySources, resolver: Arc<dyn UidResolver>, store: Option<Arc<dyn IndexStore>>) -> Arc<HistoryService> {
        let mut st = State::default();
        if let Some(store) = &store {
            for r in store.load() {
                if let Some(sid) = &r.vendor_session_id {
                    st.by_sid.insert((r.vendor, sid.clone()), r.path.clone());
                }
                st.rows.insert(r.path.clone(), r);
            }
        }
        let state = Arc::new(Mutex::new(st));
        let writer = store.and_then(|s| IndexWriter::start(state.clone(), s));
        Arc::new(HistoryService {
            state,
            sources: RwLock::new(sources),
            resolver,
            writer,
            live: Mutex::new(HashSet::new()),
            bucket: Mutex::new(Bucket { tokens: Bucket::BURST, at: Instant::now() }),
            latest_query: Mutex::new(None),
            projects: Mutex::new(HashMap::new()),
            codex_threads: Mutex::new(HashMap::new()),
            agy_rows: Mutex::new(HashMap::new()),
            stop: AtomicBool::new(false),
            kick: (Mutex::new(false), Condvar::new()),
            #[cfg(any(test, feature = "test-fixtures"))]
            fill_row_hook: Mutex::new(None),
        })
    }

    pub fn set_sources(&self, s: HistorySources) {
        if let Ok(mut g) = self.sources.write() {
            *g = s;
        }
    }

    pub fn sources(&self) -> HistorySources {
        self.sources.read().map(|g| g.clone()).unwrap_or_default()
    }

    /// The running `(vendor, session id)` set (from the engine).
    pub fn set_live(&self, live: HashSet<(Vendor, String)>) {
        if let Ok(mut g) = self.live.lock() {
            *g = live;
        }
    }

    /// Stop the background pass (after the row in hand) and the index
    /// writer: the writer writes everything queued, once no fill pass and no
    /// background thread is left to queue more, then exits (A51.4). Rows
    /// loaded on demand after that stay in memory only.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.kick();
        if let Some(w) = &self.writer {
            w.stop();
        }
    }

    /// [`Self::stop`], then wait until the index writer has written its
    /// queue and exited (at once when memory-only). The background thread of
    /// [`Self::spawn_fill`] ends with this, so joining that thread after
    /// `stop` waits for the drain too. Never call it from inside a fill pass
    /// (a fill row hook): the writer drains only after that pass ends.
    pub fn stop_and_drain(&self) {
        self.stop();
        if let Some(w) = &self.writer {
            w.join();
        }
    }

    /// Queue `path` for the index writer (nothing when memory-only).
    fn queue_upsert(&self, path: &Path) {
        if let Some(w) = &self.writer {
            w.queue([path], false);
        }
    }

    /// Ask the background thread for a refresh (the stat pass + fill).
    pub fn kick(&self) {
        let (m, c) = &self.kick;
        if let Ok(mut k) = m.lock() {
            *k = true;
        }
        c.notify_one();
    }

    /// `(rows, titles loaded)`.
    pub fn coverage(&self) -> (u32, u32) {
        let st = self.state.lock().expect("history state");
        let loaded = st.titles.values().filter(|t| t.loaded).count() as u32;
        (st.rows.len() as u32, loaded)
    }

    pub fn truncated(&self) -> bool {
        self.state.lock().map(|s| s.truncated).unwrap_or(false)
    }

    /// A copy of the row of a vendor session id.
    pub fn row_of(&self, vendor: Vendor, sid: &str) -> Option<IndexRow> {
        let st = self.state.lock().ok()?;
        let p = st.by_sid.get(&(vendor, sid.to_string()))?;
        st.rows.get(p).cloned()
    }

    /// The newest teammate transcript of `team` / `agent` in `account`.
    pub fn teammate_session(&self, account: &AccountId, team: &str, agent: &str) -> Option<String> {
        let st = self.state.lock().ok()?;
        st.rows
            .values()
            .filter(|r| r.vendor == Vendor::Claude && r.account_id.as_ref() == Some(account))
            .filter(|r| r.team_name.as_deref() == Some(team) && r.teammate_name.as_deref() == Some(agent))
            .max_by_key(|r| r.file.mtime_ms)
            .and_then(|r| r.vendor_session_id.clone())
    }

    /// Rows whose last activity is at or after `since` (the "ended today" set).
    pub fn rows_since(&self, since: EpochMs) -> Vec<IndexRow> {
        let st = self.state.lock().expect("history state");
        st.rows.values().filter(|r| r.last_row_at.is_some_and(|t| t >= since)).cloned().collect()
    }

    pub fn project_of(&self, cwd: &str) -> Option<ProjectId> {
        if let Some(p) = self.projects.lock().ok()?.get(cwd) {
            return p.clone();
        }
        let home = self.sources().home;
        let p = location::identify(Path::new(cwd), &home).map(|i| i.project_id);
        if let Ok(mut m) = self.projects.lock() {
            if m.len() > 4096 {
                m.clear();
            }
            m.insert(cwd.to_string(), p.clone());
        }
        p
    }

    // -----------------------------------------------------------------------
    // the stat pass
    // -----------------------------------------------------------------------

    /// Stat every source file; new/changed files lose their `loaded` flag
    /// (their head/tail is read by the fill pass or on demand); gone files are
    /// removed. Agy and codex index rows come from their SQLite indexes.
    pub fn refresh_index(&self) -> RefreshStats {
        let src = self.sources();
        let mut found: Vec<IndexRow> = Vec::new();
        let mut truncated = false;
        for (acct, cfg) in &src.claude {
            let projects = cfg.join("projects");
            let Ok(root) = Root::new(&projects) else { continue };
            let Ok(slugs) = safe_read::list_dir(&root, root.path(), safe_read::DIR_ENTRIES_CAP) else { continue };
            truncated |= slugs.truncated;
            for slug in slugs.entries.iter().filter(|e| e.kind == EntryKind::Dir) {
                let dir = root.join(&slug.name);
                let Ok(files) = safe_read::list_dir(&root, &dir, safe_read::DIR_ENTRIES_CAP) else { continue };
                truncated |= files.truncated;
                for f in files.entries.iter().filter(|e| e.kind == EntryKind::File) {
                    let path = dir.join(&f.name);
                    let Some(sid) = transcripts::session_id_of(&path) else { continue };
                    if let Ok(st) = safe_read::lstat(&path) {
                        found.push(IndexRow::new(path, RowKind::Transcript, Vendor::Claude, Some(acct.clone()), Some(sid), st.id));
                    }
                }
            }
        }
        for (acct, home) in &src.codex {
            let sessions = home.join("sessions");
            if let Ok(root) = Root::new(&sessions) {
                if let Ok((files, t)) = codex::list_rollouts(&root, INDEX_CAP) {
                    truncated |= t;
                    for f in files {
                        found.push(IndexRow::new(f.path, RowKind::Rollout, Vendor::Codex, Some(acct.clone()), Some(f.id), f.file));
                    }
                }
            }
            let db = home.join("state_5.sqlite");
            if let Ok(root) = Root::new(home) {
                if let Ok(rows) = codex::query_threads(&root, &db, &src.app_data) {
                    if let Ok(mut m) = self.codex_threads.lock() {
                        *m = rows.into_iter().map(|r| (r.id.clone(), r)).collect();
                    }
                }
            }
        }
        if let Some(chats) = &src.cursor_chats {
            if let Ok(root) = Root::new(chats) {
                if let Ok((files, t)) = cursor::list_chats(&root, INDEX_CAP) {
                    truncated |= t;
                    let acct = AccountId::derive_default_login(Vendor::Cursor);
                    for f in files {
                        found.push(IndexRow::new(f.meta_path, RowKind::CursorChat, Vendor::Cursor, Some(acct.clone()), Some(f.chat_id), f.file));
                    }
                }
            }
        }
        if let Some(dir) = &src.agy_dir {
            let db = dir.join("conversation_summaries.db");
            if let (Ok(root), Ok(st)) = (Root::new(dir), safe_read::lstat(&db)) {
                if let Ok(rows) = agy::query_summaries(&root, &db, &src.app_data) {
                    let acct = AccountId::derive_default_login(Vendor::Agy);
                    let mut m = HashMap::new();
                    for r in rows {
                        let mut row = IndexRow::new(
                            PathBuf::from(format!("{}#{}", db.display(), r.id)),
                            RowKind::AgySummary,
                            Vendor::Agy,
                            Some(acct.clone()),
                            Some(r.id.clone()),
                            FileId { mtime_ms: r.last_modified.unwrap_or(st.id.mtime_ms), ..st.id },
                        );
                        row.cwd = r.workspaces.first().cloned();
                        row.last_row_at = r.last_user_input.or(r.last_modified);
                        row.loaded = true;
                        m.insert(r.id.clone(), r);
                        found.push(row);
                    }
                    if let Ok(mut g) = self.agy_rows.lock() {
                        *g = m;
                    }
                }
            }
        }
        if found.len() > INDEX_CAP {
            found.sort_by_key(|r| std::cmp::Reverse(r.file.mtime_ms));
            found.truncate(INDEX_CAP);
            truncated = true;
        }

        let mut stats = RefreshStats { total: found.len() as u32, truncated, ..RefreshStats::default() };
        let mut st = self.state.lock().expect("history state");
        let seen: HashSet<PathBuf> = found.iter().map(|r| r.path.clone()).collect();
        let gone: Vec<PathBuf> = st.rows.keys().filter(|p| !seen.contains(*p)).cloned().collect();
        for p in &gone {
            if let Some(r) = st.rows.remove(p) {
                if let Some(sid) = r.vendor_session_id {
                    st.by_sid.remove(&(r.vendor, sid));
                }
            }
            st.titles.remove(p);
        }
        stats.removed = gone.len() as u32;
        for r in found {
            let changed = st.rows.get(&r.path).is_none_or(|old| old.file != r.file || (r.kind == RowKind::AgySummary && old.last_row_at != r.last_row_at));
            if changed {
                if let Some(sid) = &r.vendor_session_id {
                    st.by_sid.insert((r.vendor, sid.clone()), r.path.clone());
                }
                if r.kind == RowKind::AgySummary {
                    st.titles.remove(&r.path);
                }
                st.rows.insert(r.path.clone(), r);
            } else if let Some(old) = st.rows.get(&r.path) {
                if old.loaded {
                    stats.loaded += 1;
                }
            }
        }
        st.truncated = truncated;
        drop(st);
        // Queued for the index writer, in order with the upserts (A51.4).
        if let Some(w) = &self.writer {
            w.queue(gone.iter().map(PathBuf::as_path), true);
        }
        stats
    }

    /// Mark one file changed (FSEvents): its row reloads on next use.
    pub fn invalidate(&self, path: &Path) {
        if let Ok(mut st) = self.state.lock() {
            if let Some(r) = st.rows.get_mut(path) {
                r.loaded = false;
            }
            if let Some(t) = st.titles.get_mut(path) {
                t.loaded = false;
            }
        }
    }

    // -----------------------------------------------------------------------
    // loading one row (head/tail) — metadata + title
    // -----------------------------------------------------------------------

    /// Read the head/tail of one row's file on demand (a History page, one
    /// session) into memory, and queue the row for the index writer. Never
    /// waits for the store (A51.4): the query path's answer comes from
    /// memory only.
    pub fn load(&self, path: &Path) -> bool {
        if !self.load_row(path) {
            return false;
        }
        self.queue_upsert(path);
        true
    }

    /// Read the head/tail of one row's file into the in-memory index and the
    /// title cache. Touches no store: the callers queue the row.
    fn load_row(&self, path: &Path) -> bool {
        let Some(row) = self.state.lock().ok().and_then(|s| s.rows.get(path).cloned()) else { return false };
        let src = self.sources();
        let (meta, title) = match row.kind {
            RowKind::Transcript => self.load_transcript(&row),
            RowKind::Rollout => self.load_rollout(&row, &src),
            RowKind::CursorChat => self.load_cursor(&row, &src),
            RowKind::AgySummary => {
                let t = self.agy_rows.lock().ok().and_then(|m| row.vendor_session_id.as_ref().and_then(|id| m.get(id).and_then(|r| r.title.clone())));
                (Some(row.clone()), t)
            }
        };
        let Some(mut meta) = meta else { return false };
        meta.loaded = true;
        meta.scanned_at = now_wall_ms();
        let display = title.and_then(|t| display_text(&t, TextCap::Chars(TITLE_CHARS)));
        let folded = display.as_deref().map(fold).unwrap_or_default();
        let mut st = self.state.lock().expect("history state");
        if let Some(sid) = &meta.vendor_session_id {
            st.by_sid.insert((meta.vendor, sid.clone()), meta.path.clone());
        }
        st.titles.insert(meta.path.clone(), Titles { title: display, folded, loaded: true });
        st.rows.insert(meta.path.clone(), meta);
        true
    }

    fn load_transcript(&self, row: &IndexRow) -> (Option<IndexRow>, Option<String>) {
        let Some(root) = row.path.parent().and_then(|p| p.parent()).and_then(|p| Root::new(p).ok()) else { return (None, None) };
        let Ok(f) = safe_read::open_in(&root, &row.path) else { return (None, None) };
        let (Ok(head), Ok(tail)) = (transcripts::scan_head(&f), transcripts::scan_tail(&f, TailMode::History)) else { return (None, None) };
        let mut m = row.clone();
        m.file = f.id;
        m.cwd = head.cwd.clone();
        m.git_branch = head.git_branch.clone();
        m.entrypoint = head.entrypoint.clone();
        m.is_teammate = head.agent_name.is_some() && head.team_name.is_some();
        m.team_name = head.team_name.clone();
        m.teammate_name = head.agent_name.clone();
        m.continued_in = tail.continued_in.clone();
        m.turns = tail.turns;
        m.first_row_at = head.first_row_at;
        m.last_row_at = tail.last_row_at;
        let prompt = match &head.first_prompt {
            FirstPrompt::Found(p) => Some(p.chars().take(PROMPT_TITLE_CHARS).collect::<String>()),
            _ => None,
        };
        let title = tail.user_name.clone().or(tail.custom_title.clone()).or(tail.ai_title.clone()).or(prompt);
        (Some(m), title)
    }

    fn load_rollout(&self, row: &IndexRow, _src: &HistorySources) -> (Option<IndexRow>, Option<String>) {
        let Some(root) = row.path.ancestors().nth(4).and_then(|p| Root::new(p).ok()) else { return (None, None) };
        let Ok((head, last, id)) = codex::read_rollout(&root, &row.path) else { return (None, None) };
        let mut m = row.clone();
        m.file = id;
        if let Some(h) = head {
            m.cwd = h.cwd;
            m.entrypoint = h.source;
            m.first_row_at = h.started_at;
        }
        m.last_row_at = last;
        let thread = self.codex_threads.lock().ok().and_then(|t| row.vendor_session_id.as_ref().and_then(|id| t.get(id).cloned()));
        let title = thread.as_ref().and_then(|t| {
            t.title.clone().or_else(|| t.first_user_message.as_ref().map(|p| p.chars().take(PROMPT_TITLE_CHARS).collect()))
        });
        (Some(m), title)
    }

    fn load_cursor(&self, row: &IndexRow, src: &HistorySources) -> (Option<IndexRow>, Option<String>) {
        let Some(chats) = src.cursor_chats.as_ref().and_then(|c| Root::new(c).ok()) else { return (None, None) };
        let Some(chat_id) = row.vendor_session_id.clone() else { return (None, None) };
        let f = cursor::ChatFile { meta_path: row.path.clone(), chat_id, file: row.file };
        let Ok(Some(meta)) = cursor::read_meta(&chats, &f) else { return (None, None) };
        let mut m = row.clone();
        m.cwd = meta.cwd;
        m.first_row_at = meta.created_at;
        m.last_row_at = meta.updated_at;
        (Some(m), None) // cursor rows have no title [C6]
    }

    /// Test-only (A51.1): set the closure [`HistoryService::fill`] calls once
    /// per row, inside the row's measured work (`None` clears it).
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn set_fill_row_hook(&self, hook: Option<FillRowHook>) {
        if let Ok(mut h) = self.fill_row_hook.lock() {
            *h = hook;
        }
    }

    #[cfg(any(test, feature = "test-fixtures"))]
    fn run_fill_row_hook(&self, path: &Path) {
        let hook = self.fill_row_hook.lock().ok().and_then(|h| h.clone());
        if let Some(hook) = hook {
            hook(path);
        }
    }

    #[cfg(not(any(test, feature = "test-fixtures")))]
    #[inline(always)]
    fn run_fill_row_hook(&self, _path: &Path) {}

    /// Test-only (A51.4): set the index writer's batch interval (the app's
    /// is [`INDEX_BATCH_INTERVAL`]); nothing when memory-only.
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn set_index_batch_interval(&self, interval: Duration) {
        if let Some(w) = &self.writer {
            w.set_interval(interval);
        }
    }

    /// The background pass: load every row not loaded yet, newest first,
    /// throttled to ≤ 25 % of one core by resting 3× each row's thread CPU
    /// time (A51.1, [`thread_cpu_time`]). Each loaded row is queued for the
    /// index writer, which writes it in a batch (A51.4): the fill never waits
    /// for SQLite. A stopping writer drains only after this pass has queued
    /// the row in hand. Returns rows loaded.
    pub fn fill(&self) -> u32 {
        let mut todo: Vec<(i64, PathBuf)> = {
            let st = self.state.lock().expect("history state");
            st.rows
                .values()
                .filter(|r| !r.loaded || !st.titles.get(&r.path).is_some_and(|t| t.loaded))
                .map(|r| (r.file.mtime_ms, r.path.clone()))
                .collect()
        };
        todo.sort_by_key(|t| std::cmp::Reverse(t.0));
        let _producing = self.writer.as_ref().map(IndexWriter::producing);
        let mut n = 0;
        for (_, p) in todo {
            if self.stop.load(Ordering::SeqCst) {
                break;
            }
            let (wall0, cpu0) = (Instant::now(), thread_cpu_time());
            self.run_fill_row_hook(&p);
            if self.load_row(&p) {
                n += 1;
                self.queue_upsert(&p);
            }
            // Work 1, rest 3 — by CPU time, so waiting is not multiplied.
            std::thread::sleep(fill_rest(cpu0, thread_cpu_time(), wall0.elapsed()));
        }
        n
    }

    /// Start the background pass on its own thread at QoS **utility** (A51.5:
    /// the History screen shows its progress, "Searching titles… n of N", and
    /// utility is the class for visible progress; at QoS background the
    /// efficiency cores filled 10,000 rows in 511 s on a quiet Mac, at
    /// utility in 45 s). The ≤ 25 % CPU cap is the CPU-time rest (A51.1). A
    /// stat pass and the fill, then again on every [`HistoryService::kick`]
    /// (or at the latest after the reconcile interval). It sleeps on a
    /// condition variable — no polling. When it ends (after
    /// [`Self::stop`]) it drains and joins the index writer (A51.4), so
    /// whoever joins this thread waits for the queued rows to be written.
    pub fn spawn_fill(self: &Arc<Self>) -> std::thread::JoinHandle<()> {
        let me = self.clone();
        std::thread::Builder::new()
            .name("gilde-history-index".into())
            .spawn(move || {
                crate::discovery::set_thread_qos(false); // utility (A51.5)
                // Held across the stat passes too: a stop drains their
                // removals and the last fill's rows.
                let producing = me.writer.as_ref().map(IndexWriter::producing);
                loop {
                    me.refresh_index();
                    me.fill();
                    if me.stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let (m, c) = &me.kick;
                    let Ok(g) = m.lock() else { break };
                    let Ok((mut g, _)) = c.wait_timeout_while(g, std::time::Duration::from_secs(120), |k| {
                        !*k && !me.stop.load(Ordering::SeqCst)
                    }) else {
                        break;
                    };
                    *g = false;
                    if me.stop.load(Ordering::SeqCst) {
                        break;
                    }
                }
                drop(producing);
                me.stop_and_drain();
            })
            .expect("spawn the history thread")
    }

    // -----------------------------------------------------------------------
    // history_query / history_preview
    // -----------------------------------------------------------------------

    fn row_matches(&self, r: &IndexRow, req: &HistoryQueryReq) -> bool {
        let f = &req.filters;
        if f.vendor.is_some_and(|v| v != r.vendor) {
            return false;
        }
        if f.account_id.as_ref().is_some_and(|a| r.account_id.as_ref() != Some(a)) {
            return false;
        }
        if f.origin.is_some_and(|o| o != origin_of(r)) {
            return false;
        }
        if let Some(p) = &f.project_id {
            let Some(cwd) = &r.cwd else { return false };
            if self.project_of(cwd).as_ref() != Some(p) {
                return false;
            }
        }
        let start = r.first_row_at.or(r.last_row_at);
        let end = r.last_row_at.or(r.first_row_at);
        if let Some(from) = f.from {
            if end.is_none_or(|e| e < from) {
                return false;
            }
        }
        if let Some(to) = f.to {
            if start.is_none_or(|s| s > to) {
                return false;
            }
        }
        true
    }

    fn needs_metadata(req: &HistoryQueryReq) -> bool {
        let f = &req.filters;
        f.project_id.is_some() || f.origin.is_some() || f.from.is_some() || f.to.is_some()
    }

    /// `history_query` (§4.2.8). Rate class `query` (20/s → `BUSY`).
    pub fn query(&self, req: &HistoryQueryReq) -> Result<HistoryQueryRes, IpcError> {
        if let Err(ms) = self.bucket.lock().map_err(|_| 0u64).and_then(|mut b| b.take()) {
            return Err(IpcError::busy(ms.max(1)));
        }
        let qid = req.query_id.as_str().to_string();
        if let Ok(mut l) = self.latest_query.lock() {
            *l = Some(qid.clone());
        }
        let qh = query_hash(req);
        let after = match &req.cursor {
            None => None,
            Some(c) => Some(decode_cursor(c, &qh).ok_or_else(|| IpcError::invalid_argument("cursor", "pattern"))?),
        };
        let needle = fold(req.text.trim());
        let limit = usize::from(req.limit.clamp(1, 100));

        // Candidates in order (score desc, mtime desc, path hash).
        let mut cands: Vec<(CursorPos, PathBuf)> = {
            let st = self.state.lock().expect("history state");
            st.rows
                .values()
                .filter_map(|r| {
                    let score = if needle.is_empty() {
                        0
                    } else {
                        let t = st.titles.get(&r.path).filter(|t| t.loaded)?;
                        fuzzy_score(&needle, &t.folded)?
                    };
                    Some((CursorPos { score, mtime: r.file.mtime_ms, phash: path_hash(&r.path) }, r.path.clone()))
                })
                .collect()
        };
        cands.sort_by(|a, b| {
            b.0.score.cmp(&a.0.score).then(b.0.mtime.cmp(&a.0.mtime)).then(a.0.phash.cmp(&b.0.phash))
        });
        if let Some(pos) = &after {
            cands.retain(|(p, _)| before(pos, p));
        }

        let mut picked: Vec<(CursorPos, PathBuf)> = Vec::new();
        let mut on_demand = 0usize;
        let mut exhausted = true;
        for (pos, path) in cands {
            if picked.len() >= limit {
                exhausted = false;
                break;
            }
            let loaded = self.state.lock().ok().and_then(|s| s.rows.get(&path).map(|r| r.loaded)).unwrap_or(false);
            if !loaded && (Self::needs_metadata(req) || picked.len() < limit) {
                if on_demand >= ON_DEMAND_MAX {
                    exhausted = false;
                    break;
                }
                self.load(&path);
                on_demand += 1;
                if self.superseded(&qid) {
                    return Ok(self.superseded_res(req));
                }
            }
            let row = match self.state.lock().ok().and_then(|s| s.rows.get(&path).cloned()) {
                Some(r) => r,
                None => continue,
            };
            if self.row_matches(&row, req) {
                picked.push((pos, path));
            }
        }
        if self.superseded(&qid) {
            return Ok(self.superseded_res(req));
        }
        let next_cursor = if exhausted { None } else { picked.last().and_then(|(p, _)| encode_cursor(&qh, p)) };
        let rows = picked.iter().filter_map(|(_, p)| self.history_row(p)).collect();
        let (total, loaded) = self.coverage();
        Ok(HistoryQueryRes {
            query_id: req.query_id.clone(),
            rows,
            next_cursor,
            coverage: HistoryCoverage { titles_loaded: loaded, total },
            superseded: false,
        })
    }

    fn superseded(&self, qid: &str) -> bool {
        self.latest_query.lock().map(|l| l.as_deref() != Some(qid)).unwrap_or(false)
    }

    fn superseded_res(&self, req: &HistoryQueryReq) -> HistoryQueryRes {
        let (total, loaded) = self.coverage();
        HistoryQueryRes {
            query_id: req.query_id.clone(),
            rows: Vec::new(),
            next_cursor: None,
            coverage: HistoryCoverage { titles_loaded: loaded, total },
            superseded: true,
        }
    }

    /// The index path of the session `key` names: the `(vendor, session id)`
    /// map first, then every row (a copy of the same session id in another
    /// account) — always a row whose own identity is exactly `key`.
    fn path_of_key(&self, key: &IdentityKey) -> Option<PathBuf> {
        let st = self.state.lock().ok()?;
        if let Some(p) = st.by_sid.get(&(key.vendor, key.vendor_session_id.clone())) {
            if st.rows.get(p).and_then(IndexRow::key).as_ref() == Some(key) {
                return Some(p.clone());
            }
        }
        st.rows.values().find(|r| r.key().as_ref() == Some(key)).map(|r| r.path.clone())
    }

    /// The index row of `uid`, as the index holds it now.
    fn row_of_uid(&self, uid: &SessionUid) -> Option<IndexRow> {
        let key = self.resolver.key_of(uid)?;
        let path = self.path_of_key(&key)?;
        self.state.lock().ok()?.rows.get(&path).cloned()
    }

    /// [`Self::row_of_uid`] with its head/tail metadata, read on demand (one
    /// file, bounded, like a History page's rows) when not loaded yet.
    fn loaded_row_of(&self, uid: &SessionUid) -> Option<IndexRow> {
        let row = self.row_of_uid(uid)?;
        if row.loaded {
            return Some(row);
        }
        self.load(&row.path);
        self.state.lock().ok()?.rows.get(&row.path).cloned()
    }

    /// `history_row` (A29.2): the one row `history_query` returns for `uid`
    /// (the same builder), for the Session screen's read-only view of a
    /// session World does not hold. Rate class `query`, the bucket the other
    /// history commands share (→ `BUSY`); `NOT_FOUND{session}` when the index
    /// does not know the uid. The row carries the session's live uid (the
    /// resolver follows merges, §3.3.3), exactly as `history_query` shows it.
    pub fn row(&self, uid: &SessionUid) -> Result<HistoryRow, IpcError> {
        if let Err(ms) = self.bucket.lock().map_err(|_| 0u64).and_then(|mut b| b.take()) {
            return Err(IpcError::busy(ms.max(1)));
        }
        let row = self.loaded_row_of(uid).ok_or_else(|| IpcError::not_found("session"))?;
        self.history_row(&row.path).ok_or_else(|| IpcError::not_found("session"))
    }

    /// The one row builder of `history_query` and `history_row`.
    fn history_row(&self, path: &Path) -> Option<HistoryRow> {
        let (row, title) = {
            let st = self.state.lock().ok()?;
            let r = st.rows.get(path)?.clone();
            let t = st.titles.get(path).and_then(|t| t.title.clone());
            (r, t)
        };
        let key = row.key()?;
        let uid = self.resolver.uid_for(&key);
        let sid = row.vendor_session_id.clone().unwrap_or_default();
        let running = self.live.lock().map(|l| l.contains(&(row.vendor, sid.clone()))).unwrap_or(false);
        let src = self.sources();
        let continue_options = match (row.vendor, running) {
            (Vendor::Claude, true) => vec![ContinueOption::Fork],
            (Vendor::Claude, false) => vec![ContinueOption::Resume, ContinueOption::Fork],
            (_, true) => Vec::new(),
            (Vendor::Codex, false) => vec![ContinueOption::Resume],
            (Vendor::Cursor, false) => {
                let ok = src.cursor_chats.as_ref().zip(row.cwd.as_ref()).is_some_and(|(c, cwd)| cursor::chat_exists(c, cwd, &sid));
                if ok { vec![ContinueOption::Resume] } else { Vec::new() }
            }
            (Vendor::Agy, false) => {
                let ok = src.agy_dir.as_ref().is_some_and(|d| agy::conversation_exists(d, &sid));
                if ok { vec![ContinueOption::Resume] } else { Vec::new() }
            }
        };
        let continued_in = row.continued_in.as_ref().filter(|s| is_vendor_uuid(s)).map(|s| {
            self.resolver.uid_for(&IdentityKey { vendor: row.vendor, account_id: row.account_id.clone(), vendor_session_id: s.clone() })
        });
        let title_only = matches!(row.vendor, Vendor::Cursor | Vendor::Agy);
        Some(HistoryRow {
            uid,
            vendor: row.vendor,
            project_id: row.cwd.as_deref().and_then(|c| self.project_of(c)),
            account_id: row.account_id.clone(),
            origin: origin_of(&row),
            started_at: row.first_row_at,
            ended_at: if running { None } else { row.last_row_at },
            running,
            title_only,
            label: if row.vendor == Vendor::Cursor { HistoryLabel::VendorDate } else { HistoryLabel::Title },
            title: if row.vendor == Vendor::Cursor { None } else { title },
            continue_options,
            continued_in,
        })
    }

    /// `history_preview` (§4.2.8): **[content]** read on demand, memory only.
    pub fn preview(&self, uid: &SessionUid) -> Result<HistoryPreviewRes, IpcError> {
        if let Err(ms) = self.bucket.lock().map_err(|_| 0u64).and_then(|mut b| b.take()) {
            return Err(IpcError::busy(ms.max(1)));
        }
        let key = self.resolver.key_of(uid).ok_or_else(|| IpcError::not_found("session"))?;
        if matches!(key.vendor, Vendor::Cursor | Vendor::Agy) {
            return Err(IpcError::unsupported("titleOnly"));
        }
        let row = self.row_of(key.vendor, &key.vendor_session_id).ok_or_else(|| IpcError::not_found("session"))?;
        let continued_in = row.continued_in.as_ref().map(|s| {
            self.resolver.uid_for(&IdentityKey { vendor: row.vendor, account_id: row.account_id.clone(), vendor_session_id: s.clone() })
        });
        let cap = TextCap::Chars(PREVIEW_CHARS);
        match row.kind {
            RowKind::Transcript => {
                let root = row.path.parent().and_then(|p| p.parent()).and_then(|p| Root::new(p).ok()).ok_or_else(|| IpcError::not_found("session"))?;
                let f = safe_read::open_in(&root, &row.path).map_err(|_| IpcError::not_found("session"))?;
                let head = transcripts::scan_head(&f).map_err(|_| IpcError::not_found("session"))?;
                let tail = transcripts::scan_tail(&f, TailMode::Full).map_err(|_| IpcError::not_found("session"))?;
                let first = match &head.first_prompt {
                    FirstPrompt::Found(p) => display_text(p, cap),
                    _ => None,
                };
                Ok(HistoryPreviewRes {
                    first_prompt: first,
                    last_reply: tail.last_reply.as_deref().and_then(|r| display_text(r, cap)),
                    turns: tail.turns,
                    started_at: head.first_row_at,
                    ended_at: tail.last_row_at,
                    continued_in,
                })
            }
            RowKind::Rollout => {
                let thread = self.codex_threads.lock().ok().and_then(|t| t.get(&key.vendor_session_id).cloned());
                Ok(HistoryPreviewRes {
                    first_prompt: thread.as_ref().and_then(|t| t.first_user_message.as_deref()).and_then(|p| display_text(p, cap)),
                    last_reply: None,
                    turns: None,
                    started_at: row.first_row_at,
                    ended_at: row.last_row_at,
                    continued_in: None,
                })
            }
            RowKind::CursorChat | RowKind::AgySummary => Err(IpcError::unsupported("titleOnly")),
        }
    }
}

/// A29.2: the history index as the follow hub's fallback source for a
/// session World does not hold. The path is the index row's own absolute
/// path (found under a `safe_read` root by the stat pass), never a display
/// string; the hub reads it through its bounded follower, memory only.
impl crate::follow::hub::HistoryLocator for HistoryService {
    fn locate(&self, uid: &SessionUid) -> Option<crate::follow::hub::HistoryLocation> {
        use crate::follow::hub::HistoryLocation;
        let row = self.row_of_uid(uid)?;
        if matches!(row.kind, RowKind::CursorChat | RowKind::AgySummary) {
            // Title only: nothing of it is read for a conversation view.
            return Some(HistoryLocation::TitleOnly { vendor: row.vendor });
        }
        // The cwd (the content's location base) comes from the row's head.
        let row = self.loaded_row_of(uid).unwrap_or(row);
        let cwd = row.cwd.as_deref().and_then(|c| crate::contract::common::AbsPath::parse(c).ok());
        Some(match row.kind {
            RowKind::Transcript => HistoryLocation::ClaudeTranscript { path: row.path, cwd },
            RowKind::Rollout => HistoryLocation::CodexRollout { path: row.path, cwd },
            RowKind::CursorChat | RowKind::AgySummary => HistoryLocation::TitleOnly { vendor: row.vendor },
        })
    }
}

/// `history_index` persistence on a `rusqlite` connection (`state.db`, the
/// table of migration v2, §3.9). Metadata columns only. When the table does
/// not exist yet the store is inert.
///
/// Every [`IndexStore::upsert`] / [`IndexStore::remove`] /
/// [`IndexStore::write`] call is **one** `BEGIN IMMEDIATE` transaction
/// (A51.3, A51.4): one commit per call, not per row. A call that fails rolls
/// back as a whole and logs one warning (the error kind and the row count;
/// never a path or content) — index rows are an optimisation, but a failed
/// write is never dropped silently.
pub struct SqliteIndexStore {
    conn: Mutex<rusqlite::Connection>,
    /// Test-only (A51.3): write transactions committed.
    #[cfg(any(test, feature = "test-fixtures"))]
    commits: std::sync::atomic::AtomicU64,
    /// Test-only (A51.4): called inside every write transaction.
    #[cfg(any(test, feature = "test-fixtures"))]
    write_hook: Mutex<Option<StoreWriteHook>>,
}

/// A closure [`SqliteIndexStore`] calls inside every write transaction,
/// right after `BEGIN IMMEDIATE` — holding the connection and the SQLite
/// write lock, on the thread that writes (the index writer) — a **test hook
/// only** (A51.4): absent from builds without `test-fixtures`.
#[cfg(any(test, feature = "test-fixtures"))]
pub type StoreWriteHook = Arc<dyn Fn() + Send + Sync>;

/// The kind of a store error, for the log: the SQLite result code, or the
/// error's variant name — never its message, which may quote a path or SQL.
fn store_error_kind(err: &rusqlite::Error) -> String {
    match err {
        rusqlite::Error::SqliteFailure(e, _) => format!("{:?}/{}", e.code, e.extended_code),
        other => format!("{other:?}").chars().take_while(|c| c.is_ascii_alphanumeric()).collect(),
    }
}

impl SqliteIndexStore {
    /// Wrap a connection as it is (its pragmas are the caller's).
    pub fn new(conn: rusqlite::Connection) -> SqliteIndexStore {
        SqliteIndexStore {
            conn: Mutex::new(conn),
            #[cfg(any(test, feature = "test-fixtures"))]
            commits: std::sync::atomic::AtomicU64::new(0),
            #[cfg(any(test, feature = "test-fixtures"))]
            write_hook: Mutex::new(None),
        }
    }

    /// Wrap the index connection on `state.db` and give it the store's
    /// pragmas (A51.3, `store/mod.rs`): `busy_timeout=5000` (a write that
    /// meets the main connection's lock waits instead of failing; rusqlite's
    /// own open default happens to be the same, and it is pinned here, not
    /// inherited), `foreign_keys=ON`, `synchronous=NORMAL` (no fsync per
    /// commit in WAL). WAL is already the file's mode — [`crate::store::Db::open`]
    /// sets it, and it persists. The composition root opens the connection:
    /// discovery never opens a read-write SQLite connection itself (§9.10,
    /// `scripts/check-vendor-writes.mjs`).
    pub fn with_store_pragmas(conn: rusqlite::Connection) -> rusqlite::Result<SqliteIndexStore> {
        conn.busy_timeout(INDEX_BUSY_TIMEOUT)?;
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Ok(SqliteIndexStore::new(conn))
    }

    /// Test-only (A51.3): the write transactions this store has committed.
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn committed_writes(&self) -> u64 {
        self.commits.load(Ordering::SeqCst)
    }

    /// Test-only (A51.4): set the closure called inside every write
    /// transaction (`None` clears it).
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn set_write_hook(&self, hook: Option<StoreWriteHook>) {
        *self.write_hook.lock().unwrap_or_else(PoisonError::into_inner) = hook;
    }

    #[cfg(any(test, feature = "test-fixtures"))]
    fn run_write_hook(&self) {
        let hook = self.write_hook.lock().unwrap_or_else(PoisonError::into_inner).clone();
        if let Some(hook) = hook {
            hook();
        }
    }

    #[cfg(not(any(test, feature = "test-fixtures")))]
    #[inline(always)]
    fn run_write_hook(&self) {}

    fn vendor_str(v: Vendor) -> &'static str {
        v.as_str()
    }

    fn insert_rows(tx: &rusqlite::Transaction<'_>, rows: &[IndexRow]) -> rusqlite::Result<()> {
        let mut st = tx.prepare_cached(
            "INSERT OR REPLACE INTO history_index (path, vendor, account_id, vendor_session_id, size, mtime_ms, inode, cwd, git_branch, entrypoint, is_teammate, team_name, teammate_name, continued_in, turns, first_row_at, last_row_at, scanned_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
        )?;
        for r in rows {
            // A path that is not UTF-8 has no index row (as before).
            let Some(path) = r.path.to_str() else { continue };
            st.execute(rusqlite::params![
                path,
                Self::vendor_str(r.vendor),
                r.account_id.as_ref().map(|a| a.as_str().to_string()),
                r.vendor_session_id,
                r.file.size as i64,
                r.file.mtime_ms,
                r.file.inode as i64,
                r.cwd,
                r.git_branch,
                r.entrypoint,
                i64::from(r.is_teammate),
                r.team_name,
                r.teammate_name,
                r.continued_in,
                r.turns.map(i64::from),
                r.first_row_at,
                r.last_row_at,
                r.scanned_at
            ])?;
        }
        Ok(())
    }

    fn delete_rows(tx: &rusqlite::Transaction<'_>, paths: &[PathBuf]) -> rusqlite::Result<()> {
        let mut st = tx.prepare_cached("DELETE FROM history_index WHERE path = ?1")?;
        for p in paths {
            if let Some(s) = p.to_str() {
                st.execute([s])?;
            }
        }
        Ok(())
    }

    /// Run `write` over `rows` items in one `BEGIN IMMEDIATE` transaction:
    /// commit, or roll back the whole call and log it once. Nothing to write
    /// is no transaction.
    fn write_all(&self, op: &'static str, rows: usize, write: impl FnOnce(&rusqlite::Transaction<'_>) -> rusqlite::Result<()>) {
        if rows == 0 {
            return;
        }
        // A panic inside a transaction rolled it back while unwinding (the
        // dropped `Transaction`), so a poisoned connection is still sound.
        let mut c = self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let res = (|| {
            // IMMEDIATE takes the write lock up front, through `busy_timeout`,
            // so the transaction never fails half way on a lock it upgrades to.
            let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            self.run_write_hook();
            write(&tx)?;
            tx.commit()
            // On an error, the dropped `tx` rolls back.
        })();
        match res {
            Ok(()) => {
                #[cfg(any(test, feature = "test-fixtures"))]
                self.commits.fetch_add(1, Ordering::SeqCst);
            }
            Err(err) => {
                tracing::warn!(target: "discovery", op, kind = %store_error_kind(&err), rows, "history index write failed; rolled back");
            }
        }
    }
}

impl IndexStore for SqliteIndexStore {
    fn load(&self) -> Vec<IndexRow> {
        let Ok(c) = self.conn.lock() else { return Vec::new() };
        let Ok(mut st) = c.prepare(
            "SELECT path, vendor, account_id, vendor_session_id, size, mtime_ms, inode, cwd, git_branch, entrypoint, is_teammate, team_name, teammate_name, continued_in, turns, first_row_at, last_row_at, scanned_at FROM history_index",
        ) else {
            return Vec::new();
        };
        let rows = st.query_map([], |r| {
            let path: String = r.get(0)?;
            let vendor: String = r.get(1)?;
            Ok((path, vendor, r.get::<_, Option<String>>(2)?, r.get::<_, Option<String>>(3)?, r.get::<_, i64>(4)?, r.get::<_, i64>(5)?, r.get::<_, i64>(6)?, r.get::<_, Option<String>>(7)?, r.get::<_, Option<String>>(8)?, r.get::<_, Option<String>>(9)?, r.get::<_, i64>(10)?, r.get::<_, Option<String>>(11)?, r.get::<_, Option<String>>(12)?, r.get::<_, Option<String>>(13)?, r.get::<_, Option<i64>>(14)?, r.get::<_, Option<i64>>(15)?, r.get::<_, Option<i64>>(16)?, r.get::<_, i64>(17)?))
        });
        let Ok(rows) = rows else { return Vec::new() };
        rows.flatten()
            .filter_map(|t| {
                let vendor = Vendor::parse(&t.1)?;
                let kind = match vendor {
                    Vendor::Claude => RowKind::Transcript,
                    Vendor::Codex => RowKind::Rollout,
                    Vendor::Cursor => RowKind::CursorChat,
                    Vendor::Agy => RowKind::AgySummary,
                };
                Some(IndexRow {
                    path: PathBuf::from(t.0),
                    kind,
                    vendor,
                    account_id: t.2.and_then(|a| AccountId::parse(a).ok()),
                    vendor_session_id: t.3,
                    file: FileId { size: t.4.max(0) as u64, mtime_ms: t.5, inode: t.6.max(0) as u64, dev: 0 },
                    cwd: t.7,
                    git_branch: t.8,
                    entrypoint: t.9,
                    is_teammate: t.10 != 0,
                    team_name: t.11,
                    teammate_name: t.12,
                    continued_in: t.13,
                    turns: t.14.map(|v| v.max(0) as u32),
                    first_row_at: t.15,
                    last_row_at: t.16,
                    scanned_at: t.17,
                    loaded: true,
                })
            })
            .collect()
    }

    fn upsert(&self, rows: &[IndexRow]) {
        self.write_all("upsert", rows.len(), |tx| Self::insert_rows(tx, rows));
    }

    fn remove(&self, paths: &[PathBuf]) {
        self.write_all("remove", paths.len(), |tx| Self::delete_rows(tx, paths));
    }

    /// The deletes, then the upserts, in one transaction (A51.4).
    fn write(&self, remove: &[PathBuf], upsert: &[IndexRow]) {
        let op = match (remove.is_empty(), upsert.is_empty()) {
            (false, true) => "remove",
            (true, false) => "upsert",
            _ => "write",
        };
        self.write_all(op, remove.len() + upsert.len(), |tx| {
            Self::delete_rows(tx, remove)?;
            Self::insert_rows(tx, upsert)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn korean_safe_matcher() {
        assert!(fuzzy_score(&fold("그래프"), &fold("그래프 노드 필터")).is_some());
        assert!(fuzzy_score(&fold("ㄱ"), &fold("그래프")).is_none(), "per code point, never by jamo guess");
        // NFD (as APFS may hand it over) matches NFC.
        let nfd: String = "한글 제목".nfd().collect();
        assert!(fuzzy_score(&fold("한글"), &fold(&nfd)).is_some());
        assert!(fuzzy_score(&fold("graph"), &fold("Graph panel")).unwrap() > fuzzy_score(&fold("graph"), &fold("go run a parse helper")).unwrap_or(i32::MIN));
        assert_eq!(fuzzy_score("", "x"), Some(0));
    }

    #[test]
    fn cursor_round_trip_and_mismatch() {
        let pos = CursorPos { score: 3, mtime: 1_792_558_922_000, phash: "0123456789abcdef".into() };
        let c = encode_cursor("deadbeef", &pos).unwrap();
        assert!(c.as_str().len() <= 256);
        assert_eq!(decode_cursor(&c, "deadbeef"), Some(pos));
        assert_eq!(decode_cursor(&c, "00000000"), None);
    }

    /// A51.1: the rest is 3× the row's CPU time; a missing or backwards CPU
    /// reading falls back to 3× the wall time (the fill never spins).
    #[test]
    fn fill_rest_follows_cpu_time_and_falls_back_to_wall() {
        let ms = Duration::from_millis;
        assert_eq!(fill_rest(Some(ms(10)), Some(ms(12)), ms(80)), ms(6), "3× CPU, not 3× wall");
        assert_eq!(fill_rest(None, Some(ms(12)), ms(80)), ms(240));
        assert_eq!(fill_rest(Some(ms(10)), None, ms(80)), ms(240));
        assert_eq!(fill_rest(Some(ms(12)), Some(ms(10)), ms(80)), ms(240), "a clock going backwards rests by wall time");
    }

    /// A51.3 (a): the index connection carries the store's pragmas.
    #[test]
    fn index_store_open_applies_the_store_pragmas() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SqliteIndexStore::with_store_pragmas(rusqlite::Connection::open(dir.path().join("state.db")).expect("open")).expect("pragmas");
        let c = store.conn.lock().expect("conn");
        let pragma = |p: &str| -> i64 { c.query_row(&format!("PRAGMA {p}"), [], |r| r.get(0)).expect("pragma") };
        assert_eq!(pragma("synchronous"), 1, "synchronous=NORMAL");
        assert_eq!(pragma("busy_timeout"), 5_000);
        assert_eq!(pragma("foreign_keys"), 1);
    }

    #[derive(Clone, Default)]
    struct LogCapture(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for LogCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("log").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn index_row(path: &str) -> IndexRow {
        let file = FileId { size: 10, mtime_ms: 1_792_558_922_000, inode: 7, dev: 1 };
        let mut r = IndexRow::new(PathBuf::from(path), RowKind::Transcript, Vendor::Claude, None, Some(format!("sid-{}", path.len())), file);
        r.cwd = Some("/Users/someone/secret-project".into());
        r.loaded = true;
        r
    }

    /// A51.3 (b): `upsert` and `remove` are one transaction per call — a
    /// failing row rolls the whole call back and logs one warning with the
    /// error kind and the row count, never a path; an empty call is no
    /// transaction.
    #[test]
    fn a_failed_write_rolls_back_the_whole_call_and_logs_once_without_paths() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("state.db");
        let _db = crate::store::Db::open(&path).expect("state.db (migrated, WAL)");
        let store = SqliteIndexStore::with_store_pragmas(rusqlite::Connection::open(&path).expect("index connection")).expect("pragmas");
        let other = rusqlite::Connection::open(&path).expect("second connection");
        other
            .execute_batch(
                "CREATE TRIGGER poison_in BEFORE INSERT ON history_index WHEN NEW.path LIKE '%poison%' BEGIN SELECT RAISE(ABORT, 'poisoned'); END;
                 CREATE TRIGGER poison_out BEFORE DELETE ON history_index WHEN OLD.path LIKE '%keep%' BEGIN SELECT RAISE(ABORT, 'kept'); END;",
            )
            .expect("triggers");
        let count = || -> i64 { other.query_row("SELECT count(*) FROM history_index", [], |r| r.get(0)).expect("count") };

        let cap = LogCapture::default();
        let writer = cap.clone();
        let subscriber = tracing_subscriber::fmt().with_max_level(tracing::Level::TRACE).with_writer(move || writer.clone()).with_ansi(false).finish();
        tracing::subscriber::with_default(subscriber, || {
            store.upsert(&[index_row("/x/secret-a.jsonl"), index_row("/x/poison-b.jsonl"), index_row("/x/secret-c.jsonl")]);
        });
        assert_eq!(count(), 0, "the rows before the failing one rolled back too");
        assert_eq!(store.committed_writes(), 0);
        let log = String::from_utf8(cap.0.lock().expect("log").clone()).expect("utf-8 log");
        assert_eq!(log.matches("history index write failed").count(), 1, "one warning per call: {log}");
        for want in ["WARN", "discovery", "upsert", "rows=3", "ConstraintViolation"] {
            assert!(log.contains(want), "the warning carries {want}: {log}");
        }
        for leak in ["/x/", "secret", "poison", "someone"] {
            assert!(!log.contains(leak), "the warning names no path or content ({leak}): {log}");
        }

        let (a, keep) = (index_row("/x/secret-a.jsonl"), index_row("/x/keep-d.jsonl"));
        store.upsert(&[a.clone(), keep.clone()]);
        assert_eq!((count(), store.committed_writes()), (2, 1), "one commit for the call");
        store.remove(&[a.path.clone(), keep.path.clone()]);
        assert_eq!((count(), store.committed_writes()), (2, 1), "a failed remove keeps every row of the call");
        store.remove(&[a.path.clone(), PathBuf::from("/x/never-indexed.jsonl")]);
        assert_eq!((count(), store.committed_writes()), (1, 2));
        store.upsert(&[]);
        store.remove(&[]);
        assert_eq!(store.committed_writes(), 2, "nothing to write is no transaction");
        let left = store.load();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].path, keep.path);
        assert_eq!(left[0].cwd, keep.cwd);
    }

    /// The error kind is the SQLite code or the variant name, never a message.
    #[test]
    fn store_error_kind_carries_no_message() {
        let busy = rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(5), Some("database /x/secret is locked".into()));
        assert_eq!(store_error_kind(&busy), "DatabaseBusy/5");
        assert_eq!(store_error_kind(&rusqlite::Error::InvalidPath(PathBuf::from("/x/secret"))), "InvalidPath");
        assert_eq!(store_error_kind(&rusqlite::Error::QueryReturnedNoRows), "QueryReturnedNoRows");
    }

    /// The thread CPU clock reads, and blocking does not advance it.
    #[test]
    fn thread_cpu_clock_does_not_count_blocking() {
        let a = thread_cpu_time().expect("CLOCK_THREAD_CPUTIME_ID reads");
        std::thread::sleep(Duration::from_millis(50));
        let b = thread_cpu_time().expect("CLOCK_THREAD_CPUTIME_ID reads");
        assert!(b >= a);
        assert!(b - a < Duration::from_millis(25), "a 50 ms sleep cost {:?} of CPU", b - a);
    }

    /// A store that records each write call: `(removed paths, upserted paths)`.
    #[derive(Default)]
    struct RecordingStore {
        writes: Mutex<Vec<(Vec<PathBuf>, Vec<PathBuf>)>>,
    }

    impl IndexStore for RecordingStore {
        fn load(&self) -> Vec<IndexRow> {
            Vec::new()
        }
        fn upsert(&self, rows: &[IndexRow]) {
            self.write(&[], rows);
        }
        fn remove(&self, paths: &[PathBuf]) {
            self.write(paths, &[]);
        }
        fn write(&self, remove: &[PathBuf], upsert: &[IndexRow]) {
            self.writes.lock().expect("writes").push((remove.to_vec(), upsert.iter().map(|r| r.path.clone()).collect()));
        }
    }

    fn service_over(store: &Arc<RecordingStore>) -> Arc<HistoryService> {
        let svc = HistoryService::new(HistorySources::default(), Arc::new(MemoryUids::default()), Some(store.clone() as Arc<dyn IndexStore>));
        svc.set_index_batch_interval(Duration::from_secs(3_600));
        svc
    }

    /// A51.4: without a store there is no writer thread, and nothing waits.
    #[test]
    fn memory_only_has_no_writer() {
        let svc = HistoryService::new(HistorySources::default(), Arc::new(MemoryUids::default()), None);
        assert!(svc.writer.is_none());
        svc.stop_and_drain();
    }

    /// A51.4: the writer applies each path's calls in their order — a batch
    /// deletes first, then upserts the rows still loaded at flush time.
    #[test]
    fn the_writer_keeps_each_paths_order_of_removes_and_upserts() {
        let store = Arc::new(RecordingStore::default());
        let svc = service_over(&store);
        let w = svc.writer.as_ref().expect("a store has a writer");
        let p = PathBuf::from;
        {
            let mut st = svc.state.lock().expect("state");
            for s in ["/x/a", "/x/b", "/x/c"] {
                st.rows.insert(p(s), index_row(s));
            }
            st.rows.get_mut(Path::new("/x/c")).expect("c").loaded = false;
        }
        // a: loaded, then removed → removed only.
        w.queue([Path::new("/x/a")], false);
        w.queue([Path::new("/x/a")], true);
        // b: removed, then loaded again → removed, then its loaded row written.
        w.queue([Path::new("/x/b")], true);
        w.queue([Path::new("/x/b")], false);
        // c: queued, but no longer loaded at flush time → nothing written.
        w.queue([Path::new("/x/c")], false);
        // d: not in memory → removed (the store may hold an old row of it).
        w.queue([Path::new("/x/d")], true);
        assert!(store.writes.lock().expect("writes").is_empty(), "queued only: < 64 paths, the interval out of reach");
        svc.stop_and_drain();
        let writes = store.writes.lock().expect("writes").clone();
        assert_eq!(writes, vec![(vec![p("/x/a"), p("/x/b"), p("/x/d")], vec![p("/x/b")])], "one batch, removes before upserts");
        // After the drain the writer is gone: nothing is queued any more.
        w.queue([Path::new("/x/b")], false);
        assert_eq!(store.writes.lock().expect("writes").len(), 1);
    }

    /// A51.4: batches hold ≤ 64 paths; a stop drains the rest.
    #[test]
    fn the_writer_writes_batches_of_at_most_64_paths() {
        let store = Arc::new(RecordingStore::default());
        let svc = service_over(&store);
        let paths: Vec<PathBuf> = (0..130).map(|i| PathBuf::from(format!("/x/{i:03}"))).collect();
        {
            let mut st = svc.state.lock().expect("state");
            for path in &paths {
                st.rows.insert(path.clone(), index_row(path.to_str().expect("utf-8")));
            }
        }
        for path in &paths {
            svc.queue_upsert(path);
        }
        svc.stop_and_drain();
        let writes = store.writes.lock().expect("writes").clone();
        let sizes: Vec<usize> = writes.iter().map(|(r, u)| r.len() + u.len()).collect();
        assert_eq!(sizes, vec![64, 64, 2]);
        let written: Vec<PathBuf> = writes.into_iter().flat_map(|(_, u)| u).collect();
        assert_eq!(written, paths, "in queue order");
    }

    /// A51.4: `SqliteIndexStore::write` deletes, then upserts, in one
    /// transaction; nothing to write is no transaction.
    #[test]
    fn store_write_deletes_then_upserts_in_one_transaction() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("state.db");
        let _db = crate::store::Db::open(&path).expect("state.db");
        let store = SqliteIndexStore::with_store_pragmas(rusqlite::Connection::open(&path).expect("index connection")).expect("pragmas");
        let (a, b) = (index_row("/x/a.jsonl"), index_row("/x/b.jsonl"));
        store.upsert(&[a.clone(), b.clone()]);
        let mut newer = b.clone();
        newer.turns = Some(9);
        store.write(&[a.path.clone(), b.path.clone()], &[newer]);
        assert_eq!(store.committed_writes(), 2, "one commit per call");
        let left = store.load();
        assert_eq!(left.len(), 1);
        assert_eq!((&left[0].path, left[0].turns), (&b.path, Some(9)), "b removed, then written as the newer row");
        store.write(&[], &[]);
        assert_eq!(store.committed_writes(), 2);
    }
}
