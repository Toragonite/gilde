//! Observed follow (contract §6.4): one `TranscriptTail` per file, read by
//! offset, shared by the structure sink (S3 facts for World, always on for the
//! sessions that need them) and by content subscribers (only while a
//! `convo_subscribe` exists).
//!
//! Owner: T04.
//!
//! * **Forward** ([`TranscriptTail::advance`]): `size > offset` → read
//!   `[offset, size)` in ≤ 1 MiB chunks, split on `\n`, buffer a partial
//!   trailing line (never parsed half), emit complete lines. A line longer
//!   than 4 MiB is never buffered whole: it becomes [`Line::TooLarge`] and the
//!   reader skips to its newline.
//! * **Backward** ([`read_back`]): from an end offset in 256 KiB chunks until
//!   the caller has enough or the byte budget is spent (initial load 4 MiB,
//!   load older 8 MiB per call); an oversize line is skipped without being
//!   buffered and reported as `TooLarge`.
//! * **Reset**: `size < offset` → `Reset{Truncated}`; a different inode at the
//!   path → reopen + `Reset{Replaced}` [R2 Q6].
//! * **Structure** ([`StructureState`]): S3 `TranscriptFacts`, usage summed
//!   once per `message.id` (LP-26), the tool-set attachment names, the
//!   interrupt marker as a turn end, and diagnostics counters. No content.
//! * **Spend** ([`DayUsage`], A10.4): the running total of the local calendar
//!   day, each `message.id` once, sent to World as
//!   `SourceFacts::Usage{source: Transcript}` when it changes, ≤ 1 per second
//!   per file, only when the P32 gate ([`UsageGate`]) is on and the day's rows
//!   were all read; otherwise nothing (absent = "unknown", never 0).
//! * **[`Follower`]**: the shared registry of tails with its own worker thread
//!   (QoS utility), an FSEvents watch on each followed file's directory, the
//!   1 s `fstat` poll while a content listener exists, ≤ 1 read per 100 ms
//!   per file. Structure facts go to World as `SourceFacts::Transcript`.
//!
//! Files are opened `O_RDONLY | O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK`, must be
//! regular `.jsonl` files with one link, pass the never-open filter (§9.7) by
//! the given name and by the opened file's own name, and stay inside their
//! vendor root with no symlinked directory below it ([`open_followed`],
//! A50.1). Nothing here ever writes (§9.10).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock};
use std::time::{Duration, Instant};

use crate::contract::activity::{OpenToolUse, TranscriptFacts};
use crate::contract::common::{clamp_safe_u64, EpochMs};
use crate::contract::content::ResetReason;
use crate::contract::services::{EvidenceSink, EvidenceUpdate, IdentityHint, SourceFacts, UsageFacts, UsageSource};
use crate::contract::session::UsageToday;
use crate::render::rollout::rollout_facts;
use crate::render::transcript::{row_facts, LineOutcome, RowFacts, TokenUsage, TurnEndKind};

/// Backward reads go in chunks of this size (§6.4).
pub const BACK_CHUNK: u64 = 256 * 1024;
/// Forward reads go in chunks of at most this size (§6.4).
pub const FORWARD_CHUNK: usize = 1024 * 1024;
/// A transcript line longer than this is `RowTooLarge` (§5.8).
pub const LINE_CAP: usize = 4 * 1024 * 1024;
/// The initial load reads at most this much (§6.4).
pub const INITIAL_BUDGET: u64 = 4 * 1024 * 1024;
/// One `convo_load_older` scans at most this much (§4.2.4, A §4.4).
pub const OLDER_BUDGET: u64 = 8 * 1024 * 1024;

/// The vendor format of a followed file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TranscriptFormat {
    /// `<cfg>/projects/*/<sessionId>.jsonl`.
    Claude,
    /// `~/.codex/sessions/…/rollout-*.jsonl`.
    CodexRollout,
}

impl TranscriptFormat {
    /// The structure facts of one line in this format.
    pub fn facts(self, bytes: &[u8]) -> (Option<RowFacts>, LineOutcome) {
        match self {
            TranscriptFormat::Claude => row_facts(bytes),
            TranscriptFormat::CodexRollout => rollout_facts(bytes),
        }
    }
}

// ---------------------------------------------------------------------------
// opening
// ---------------------------------------------------------------------------

/// The never-open filter (§9.7) as it applies to followed files: only
/// `.jsonl` files whose name carries no credential or content-history marker.
pub fn refuse_path(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return true;
    };
    let lower = name.to_ascii_lowercase();
    !lower.ends_with(".jsonl")
        || lower == "history.jsonl"
        || ["token", "credential", "oauth", ".key", ".pem", "secret"].iter().any(|m| lower.contains(m))
}

/// The vendor root a followed file must stay inside (A50.1), from the layout
/// discovery and history build the path in: a Claude transcript
/// `<cfg>/projects/<slug>/<sid>.jsonl` → `<cfg>/projects`; a codex rollout
/// `<home>/sessions/YYYY/MM/DD/<file>` → `<home>/sessions`. A path in
/// neither layout (a fixture) is held to its own directory. The root itself
/// may be reached through a symlink (a config dir kept elsewhere); nothing
/// below it may (see [`open_followed`]).
pub fn followed_root(path: &Path) -> Option<&Path> {
    let mut up = path.ancestors().skip(1);
    let (a1, a2, a3, a4) = (up.next(), up.next(), up.next(), up.next());
    let named = |p: Option<&Path>, name: &str| p.is_some_and(|p| p.file_name().is_some_and(|f| f == name));
    let digits = |p: Option<&Path>, len: usize| {
        p.and_then(Path::file_name)
            .and_then(|f| f.to_str())
            .is_some_and(|s| s.len() == len && s.bytes().all(|b| b.is_ascii_digit()))
    };
    if named(a2, "projects") {
        return a2;
    }
    if digits(a1, 2) && digits(a2, 2) && digits(a3, 4) && named(a4, "sessions") {
        return a4;
    }
    a1.filter(|p| !p.as_os_str().is_empty())
}

/// The path the kernel opened for `file` (`F_GETPATH`).
fn opened_path(file: &File) -> io::Result<PathBuf> {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::ffi::OsStrExt as _;
    let mut buf = vec![0u8; libc::PATH_MAX as usize + 1];
    // SAFETY: F_GETPATH writes at most MAXPATHLEN bytes incl. NUL into `buf`,
    // and the fd stays open for the call.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, buf.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(&buf[..len])))
}

/// What `path` must resolve to when nothing between its vendor root
/// ([`followed_root`]) and the file is a symlink: the root's realpath joined
/// with the rest of the path as given.
pub fn expected_real_path(path: &Path) -> io::Result<PathBuf> {
    let outside = || io::Error::new(io::ErrorKind::PermissionDenied, "outside its vendor root");
    let root = followed_root(path).ok_or_else(outside)?;
    let rest = path.strip_prefix(root).map_err(|_| outside())?;
    if rest.components().any(|c| !matches!(c, std::path::Component::Normal(_))) {
        return Err(outside());
    }
    Ok(std::fs::canonicalize(root)?.join(rest))
}

/// Open a followed file read-only with the checks of `safe_read::open_in`
/// (A50.1, A34.3): the never-open filter by name ([`refuse_path`] and
/// §9.7's list) before the open; `O_NOFOLLOW` (and `O_NONBLOCK`, so a FIFO
/// planted under a root can never block the follower); a regular file with
/// one link; the path the kernel opened must be exactly the vendor root's
/// realpath plus the rest of the path (no symlinked directory below the
/// root, nothing outside it); and the never-open filter again on the opened
/// file's own name. A refused open reads nothing.
pub fn open_followed(path: &Path) -> io::Result<File> {
    let refused = || io::Error::new(io::ErrorKind::PermissionDenied, "refused by the never-open filter");
    if refuse_path(path) || crate::discovery::safe_read::never_open(path).is_some() {
        return Err(refused());
    }
    let expected = expected_real_path(path)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.file_type().is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"));
    }
    // A hard link has a second name the filter never saw (A34.3).
    if meta.nlink() > 1 {
        return Err(refused());
    }
    let real = opened_path(&file)?;
    if real != expected {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "outside its vendor root"));
    }
    if refuse_path(&real) || crate::discovery::safe_read::never_open(&real).is_some() {
        return Err(refused());
    }
    Ok(file)
}

fn read_exact_at_retry(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    let mut done = 0usize;
    while done < buf.len() {
        match file.read_at(&mut buf[done..], offset + done as u64) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short read")),
            Ok(n) => done += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// lines
// ---------------------------------------------------------------------------

/// One line of a followed file: its start offset and bytes (without the
/// newline), or the marker of a line over [`LINE_CAP`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    Complete { start: u64, bytes: Vec<u8> },
    TooLarge { start: u64 },
}

impl Line {
    pub fn start(&self) -> u64 {
        match self {
            Line::Complete { start, .. } | Line::TooLarge { start } => *start,
        }
    }
}

/// Where the next backward read ends (`convo_load_older`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OlderCursor {
    /// Lines strictly before this offset are older.
    pub end: u64,
    /// Set while a backward scan is inside an oversize line: the bytes
    /// `[scanned_from, end)` hold no line start.
    pub scanned_from: Option<u64>,
}

impl OlderCursor {
    pub const fn at(end: u64) -> OlderCursor {
        OlderCursor { end, scanned_from: None }
    }
}

/// The result of a backward read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackRead {
    /// File order (oldest first).
    pub lines: Vec<Line>,
    pub cursor: OlderCursor,
    /// Lines remain before `cursor`.
    pub has_older: bool,
    pub scanned: u64,
}

/// Read lines backwards from `cursor` until `enough` returns true for a line
/// (called newest first) or `budget` bytes were read. Lines over `line_cap`
/// are reported as `TooLarge` without being buffered whole.
pub fn read_back(file: &File, cursor: OlderCursor, budget: u64, line_cap: usize, enough: &mut dyn FnMut(&Line) -> bool) -> io::Result<BackRead> {
    let mut end = cursor.end;
    let mut oversize_from = cursor.scanned_from;
    let mut pos = oversize_from.unwrap_or(end);
    let mut buf: Vec<u8> = Vec::new(); // bytes [pos, end) while not in oversize mode
    let mut scanned = 0u64;
    let mut newest_first: Vec<Line> = Vec::new();
    loop {
        if end == 0 {
            break;
        }
        if oversize_from.is_none() {
            let search_end = buf.len().saturating_sub(1);
            if let Some(i) = memchr::memrchr(b'\n', &buf[..search_end]) {
                let start = pos + i as u64 + 1;
                let mut bytes = buf.split_off(i + 1);
                if bytes.last() == Some(&b'\n') {
                    bytes.pop();
                }
                end = start;
                let line = Line::Complete { start, bytes };
                let stop = enough(&line);
                newest_first.push(line);
                if stop {
                    break;
                }
                continue;
            }
            if pos == 0 {
                let mut bytes = std::mem::take(&mut buf);
                if bytes.last() == Some(&b'\n') {
                    bytes.pop();
                }
                end = 0;
                let line = Line::Complete { start: 0, bytes };
                let _ = enough(&line);
                newest_first.push(line);
                break;
            }
            if (end - pos) as usize > line_cap.saturating_add(1) {
                oversize_from = Some(pos);
                buf = Vec::new();
                continue;
            }
        } else if pos == 0 {
            end = 0;
            oversize_from = None;
            let line = Line::TooLarge { start: 0 };
            let _ = enough(&line);
            newest_first.push(line);
            break;
        }
        if scanned >= budget {
            break;
        }
        let n = BACK_CHUNK.min(pos).min(budget - scanned);
        let mut chunk = vec![0u8; n as usize];
        read_exact_at_retry(file, &mut chunk, pos - n)?;
        scanned += n;
        pos -= n;
        if oversize_from.is_some() {
            if let Some(i) = memchr::memrchr(b'\n', &chunk) {
                let start = pos + i as u64 + 1;
                end = start;
                oversize_from = None;
                chunk.truncate(i + 1);
                buf = chunk;
                let line = Line::TooLarge { start };
                let stop = enough(&line);
                newest_first.push(line);
                if stop {
                    break;
                }
            } else {
                oversize_from = Some(pos);
            }
        } else {
            chunk.extend_from_slice(&buf);
            buf = chunk;
        }
    }
    newest_first.reverse();
    Ok(BackRead {
        lines: newest_first,
        cursor: OlderCursor { end, scanned_from: oversize_from.map(|_| pos) },
        has_older: end > 0,
        scanned,
    })
}

/// The offset after the last complete line of a file of `size` bytes, and
/// whether the bytes after it are an oversize partial line already.
fn last_line_end(file: &File, size: u64) -> io::Result<(u64, bool)> {
    let mut pos = size;
    let limit = (LINE_CAP as u64).saturating_add(BACK_CHUNK);
    while pos > 0 && size - pos < limit {
        let n = BACK_CHUNK.min(pos);
        let mut chunk = vec![0u8; n as usize];
        read_exact_at_retry(file, &mut chunk, pos - n)?;
        if let Some(i) = memchr::memrchr(b'\n', &chunk) {
            let end = pos - n + i as u64 + 1;
            return Ok((end, size - end > LINE_CAP as u64));
        }
        pos -= n;
    }
    if pos == 0 {
        Ok((0, size > LINE_CAP as u64))
    } else {
        Ok((size, true))
    }
}

/// The outcome of one forward step.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Advance {
    pub lines: Vec<Line>,
    /// The file was truncated or replaced: the caller restarts its view.
    pub reset: Option<ResetReason>,
}

/// One followed file, read by offset.
#[derive(Debug)]
pub struct TranscriptTail {
    path: PathBuf,
    file: File,
    ino: u64,
    dev: u64,
    /// Lines before this offset were delivered (the start of the partial).
    line_start: u64,
    /// The next byte to read.
    read_pos: u64,
    /// Bytes of the trailing partial line `[line_start, read_pos)`.
    partial: Vec<u8>,
    /// Inside an oversize line: skip to its newline.
    skipping: bool,
}

impl TranscriptTail {
    /// Open `path` positioned after its last complete line (the initial load
    /// reads backwards from [`TranscriptTail::offset`]).
    pub fn open(path: &Path) -> io::Result<TranscriptTail> {
        let file = open_followed(path)?;
        let meta = file.metadata()?;
        let mut tail = TranscriptTail {
            path: path.to_path_buf(),
            file,
            ino: meta.ino(),
            dev: meta.dev(),
            line_start: 0,
            read_pos: 0,
            partial: Vec::new(),
            skipping: false,
        };
        tail.restart()?;
        Ok(tail)
    }

    /// Re-position after the last complete line of the current file.
    pub fn restart(&mut self) -> io::Result<()> {
        let size = self.file.metadata()?.len();
        let (end, oversize) = last_line_end(&self.file, size)?;
        self.line_start = end;
        self.read_pos = end;
        self.partial.clear();
        self.skipping = false;
        if oversize {
            // The trailing partial line is already over the cap: skip it.
            self.read_pos = size;
            self.skipping = true;
        }
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Lines before this offset have been delivered (or belong to the initial
    /// load): the end cursor of backward reads.
    pub fn offset(&self) -> u64 {
        self.line_start
    }

    /// The open file (for backward reads of the same inode).
    pub fn file(&self) -> &File {
        &self.file
    }

    /// Read what was appended since the last call.
    pub fn advance(&mut self) -> io::Result<Advance> {
        match std::fs::symlink_metadata(&self.path) {
            Ok(m) if m.ino() == self.ino && m.dev() == self.dev => {}
            Ok(m) if m.file_type().is_file() => {
                let file = open_followed(&self.path)?;
                let meta = file.metadata()?;
                self.file = file;
                self.ino = meta.ino();
                self.dev = meta.dev();
                self.restart()?;
                return Ok(Advance { lines: Vec::new(), reset: Some(ResetReason::Replaced) });
            }
            // Gone or not a regular file any more: nothing to read for now.
            _ => return Ok(Advance::default()),
        }
        let size = self.file.metadata()?.len();
        if size < self.read_pos {
            self.restart()?;
            return Ok(Advance { lines: Vec::new(), reset: Some(ResetReason::Truncated) });
        }
        let mut lines = Vec::new();
        while self.read_pos < size {
            let n = ((size - self.read_pos) as usize).min(FORWARD_CHUNK);
            let mut chunk = vec![0u8; n];
            read_exact_at_retry(&self.file, &mut chunk, self.read_pos)?;
            let chunk_start = self.read_pos;
            self.read_pos += n as u64;
            self.split(&chunk, chunk_start, &mut lines);
        }
        Ok(Advance { lines, reset: None })
    }

    fn split(&mut self, chunk: &[u8], chunk_start: u64, out: &mut Vec<Line>) {
        let mut at = 0usize;
        while at < chunk.len() {
            match memchr::memchr(b'\n', &chunk[at..]) {
                Some(i) => {
                    let line_end = chunk_start + (at + i) as u64 + 1;
                    if self.skipping {
                        out.push(Line::TooLarge { start: self.line_start });
                        self.skipping = false;
                    } else if self.partial.len() + i > LINE_CAP {
                        out.push(Line::TooLarge { start: self.line_start });
                        self.partial.clear();
                    } else {
                        let mut bytes = std::mem::take(&mut self.partial);
                        bytes.extend_from_slice(&chunk[at..at + i]);
                        out.push(Line::Complete { start: self.line_start, bytes });
                    }
                    self.line_start = line_end;
                    at += i + 1;
                }
                None => {
                    let rest = &chunk[at..];
                    if !self.skipping {
                        if self.partial.len() + rest.len() > LINE_CAP {
                            self.partial = Vec::new();
                            self.skipping = true;
                        } else {
                            self.partial.extend_from_slice(rest);
                        }
                    }
                    at = chunk.len();
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// the structure sink
// ---------------------------------------------------------------------------

/// Diagnostics counters of one followed file (counts only, never content).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineCounters {
    pub rows: u64,
    pub meta: u64,
    pub invalid_utf8: u64,
    pub unparseable: u64,
    pub too_large: u64,
    pub unknown_types: BTreeMap<String, u64>,
}

impl LineCounters {
    pub fn count(&mut self, outcome: &LineOutcome) {
        match outcome {
            LineOutcome::Content => self.rows += 1,
            LineOutcome::Meta => self.meta += 1,
            LineOutcome::UnknownType(t) => *self.unknown_types.entry(t.clone()).or_default() += 1,
            LineOutcome::InvalidUtf8 => self.invalid_utf8 += 1,
            LineOutcome::Unparseable => self.unparseable += 1,
            LineOutcome::TooLarge => self.too_large += 1,
            LineOutcome::Blank => {}
        }
    }
}

/// At most this many message ids are remembered for the usage dedupe.
const MAX_SEEN_MESSAGE_IDS: usize = 20_000;
/// At most this many open tool uses are tracked.
const MAX_OPEN_TOOL_USES: usize = 256;

/// The accumulated structure facts of one followed file.
#[derive(Debug, Clone)]
pub struct StructureState {
    /// S3 for World (§3.5.2).
    pub facts: TranscriptFacts,
    /// Summed once per `message.id` (LP-26).
    pub usage: TokenUsage,
    /// At least one row carried `message.usage` (0 tokens ≠ unknown).
    pub usage_seen: bool,
    /// Tool names from tool-set attachments (§7.2.5).
    pub tool_set: BTreeSet<String>,
    /// A `TaskCreate`/`TaskUpdate` call or name was seen (§7.2.5 board choice).
    pub task_tools_seen: bool,
    /// The last `"[Request interrupted by user]"` row (LP-14b).
    pub last_interrupt_at: Option<EpochMs>,
    pub counters: LineCounters,
    open_tool_uses: VecDeque<(String, String, EpochMs)>,
    seen_message_ids: HashSet<String>,
    seen_order: VecDeque<String>,
    last_at: Option<EpochMs>,
}

/// Transcript facts with nothing known yet.
pub fn empty_facts() -> TranscriptFacts {
    TranscriptFacts { last_row_at: None, open_tool_use: None, last_turn_end_at: None, last_human_prompt_at: None, last_api_error: None }
}

impl Default for StructureState {
    fn default() -> StructureState {
        StructureState {
            facts: empty_facts(),
            usage: TokenUsage::default(),
            usage_seen: false,
            tool_set: BTreeSet::new(),
            task_tools_seen: false,
            last_interrupt_at: None,
            counters: LineCounters::default(),
            open_tool_uses: VecDeque::new(),
            seen_message_ids: HashSet::new(),
            seen_order: VecDeque::new(),
            last_at: None,
        }
    }
}

impl StructureState {
    /// Count a line outcome and apply its facts.
    pub fn line(&mut self, facts: Option<&RowFacts>, outcome: &LineOutcome) {
        self.counters.count(outcome);
        if let Some(f) = facts {
            if matches!(outcome, LineOutcome::Content) {
                self.apply(f);
            } else if let Some(at) = f.at {
                self.touch(at);
            }
        }
    }

    fn touch(&mut self, at: EpochMs) {
        self.facts.last_row_at = Some(self.facts.last_row_at.map_or(at, |p| p.max(at)));
        self.last_at = Some(at);
    }

    /// Apply one content row's facts (rows in file order).
    pub fn apply(&mut self, f: &RowFacts) {
        if let Some(at) = f.at {
            self.touch(at);
        }
        let at = f.at.or(self.last_at).unwrap_or(0);
        if f.sidechain {
            return;
        }
        if let Some(u) = &f.usage {
            self.usage_seen = true;
            match &f.message_id {
                Some(id) => {
                    if self.seen_message_ids.insert(id.clone()) {
                        self.usage.add(u);
                        self.seen_order.push_back(id.clone());
                        if self.seen_order.len() > MAX_SEEN_MESSAGE_IDS {
                            if let Some(old) = self.seen_order.pop_front() {
                                self.seen_message_ids.remove(&old);
                            }
                        }
                    }
                }
                None => self.usage.add(u),
            }
        }
        for t in &f.tool_uses {
            if matches!(t.name.as_str(), "TaskCreate" | "TaskUpdate" | "TaskList") {
                self.task_tools_seen = true;
            }
            self.open_tool_uses.push_back((t.id.clone(), t.name.clone(), at));
            if self.open_tool_uses.len() > MAX_OPEN_TOOL_USES {
                self.open_tool_uses.pop_front();
            }
        }
        if !f.tool_results.is_empty() {
            self.open_tool_uses.retain(|(id, _, _)| !f.tool_results.contains(id));
        }
        for name in &f.tool_names {
            if matches!(name.as_str(), "TaskCreate" | "TaskUpdate" | "TaskList") {
                self.task_tools_seen = true;
            }
            self.tool_set.insert(name.clone());
        }
        if f.turn_open.is_some() {
            self.facts.last_human_prompt_at = Some(at);
        }
        if let Some(end) = f.turn_end {
            self.facts.last_turn_end_at = Some(at);
            if end == TurnEndKind::Interrupted {
                self.last_interrupt_at = Some(at);
            }
            // A turn that ended has no pending tool call any more.
            self.open_tool_uses.clear();
        }
        if let Some(sub) = &f.api_error {
            self.facts.last_api_error = Some((at, sub.clone()));
        }
        self.facts.open_tool_use = self.open_tool_uses.back().map(|(id, name, at)| OpenToolUse {
            tool_use_id: id.clone(),
            tool_name: name.clone(),
            at: *at,
            is_question: matches!(name.as_str(), "AskUserQuestion" | "ExitPlanMode"),
        });
    }

    /// A copy without the internal bookkeeping.
    pub fn snapshot(&self) -> StructureSnapshot {
        StructureSnapshot {
            facts: self.facts.clone(),
            usage: self.usage,
            usage_seen: self.usage_seen,
            tool_set: self.tool_set.clone(),
            task_tools_seen: self.task_tools_seen,
            last_interrupt_at: self.last_interrupt_at,
            counters: self.counters.clone(),
        }
    }
}

/// What the structure sink knows about one file.
#[derive(Debug, Clone, PartialEq)]
pub struct StructureSnapshot {
    pub facts: TranscriptFacts,
    pub usage: TokenUsage,
    pub usage_seen: bool,
    pub tool_set: BTreeSet<String>,
    pub task_tools_seen: bool,
    pub last_interrupt_at: Option<EpochMs>,
    pub counters: LineCounters,
}

impl StructureSnapshot {
    /// P32 / Claude flag `transcriptUsage` (§1.3, frozen 2.1.306): the
    /// transcript's tokens, summed once per `message.id`, when the flag is on
    /// for the session's version; otherwise — and when no row carried usage —
    /// unknown (`None`, never 0). Transcripts carry no cost (§6.4).
    pub fn transcript_usage(&self, flag_on: bool) -> Option<TokenUsage> {
        (flag_on && self.usage_seen).then_some(self.usage)
    }
}

/// The Claude compat flag of P32 (§1.3).
pub const TRANSCRIPT_USAGE_FLAG: &str = "transcriptUsage";

/// Per-row structure facts in file order, for the team adapter (message
/// log join, inferred items, task board, interrupt markers).
pub trait RowSink: Send + Sync {
    fn rows(&self, path: &Path, hint: &IdentityHint, rows: &[RowFacts]);
}

/// Receives the lines of a followed file for content (the ContentHub).
/// Called from the follower's thread, never under its lock.
pub trait LineListener: Send + Sync {
    fn lines(&self, path: &Path, lines: &[Line]);
    /// The file was truncated or replaced: re-run the initial load from
    /// `offset` (lines from it on arrive through [`LineListener::lines`]).
    fn reset(&self, path: &Path, reason: ResetReason, offset: u64);
}

// ---------------------------------------------------------------------------
// the follower
// ---------------------------------------------------------------------------

/// Tunables (tests shorten them).
#[derive(Debug, Clone)]
pub struct FollowConfig {
    /// The `fstat` poll while a content listener exists (§6.4: 1 s).
    pub poll_interval: Duration,
    /// ≤ 1 read per this interval per file (§6.4: 100 ms).
    pub min_read_interval: Duration,
    /// Retry interval for files that do not exist yet.
    pub missing_retry: Duration,
    /// Watch the followed files' directories (FSEvents).
    pub watch: bool,
    /// The wall clock (spend days and the report throttle; tests inject one).
    pub clock: fn() -> EpochMs,
    /// The local calendar day of an instant, `YYYY-MM-DD` (tests inject UTC).
    pub local_day: fn(EpochMs) -> String,
}

impl Default for FollowConfig {
    fn default() -> FollowConfig {
        FollowConfig {
            poll_interval: Duration::from_secs(1),
            min_read_interval: Duration::from_millis(100),
            missing_retry: Duration::from_secs(2),
            watch: true,
            clock: now_ms,
            local_day,
        }
    }
}

// ---------------------------------------------------------------------------
// spend (A10.4)
// ---------------------------------------------------------------------------

/// Today's rows are scanned at most this far back from the end of a file;
/// a day longer than that is "unknown" (nothing is sent), never undercounted.
pub const USAGE_SCAN_BUDGET: u64 = 256 * 1024 * 1024;
/// At most one spend report per followed file per this interval (A10.4).
pub const USAGE_INTERVAL_MS: EpochMs = 1_000;

/// The P32 gate of one followed file: the Claude flag `transcriptUsage`
/// ([`TRANSCRIPT_USAGE_FLAG`]) for the session's version. The composition root
/// wires it (T09b, A10.6) to `CompatApi::feature`; it is called outside the
/// follower's lock. No gate = flag off = nothing is sent.
pub type UsageGate = Arc<dyn Fn(&Path, &IdentityHint) -> bool + Send + Sync>;

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The UTC calendar day of an instant, `YYYY-MM-DD`.
pub fn utc_day(ms: EpochMs) -> String {
    let (y, m, d) = civil_from_days(ms.div_euclid(86_400_000));
    format!("{y:04}-{m:02}-{d:02}")
}

/// The local calendar day of an instant, `YYYY-MM-DD` (`localtime_r`, so
/// the time zone and daylight saving time of this Mac apply).
pub fn local_day(ms: EpochMs) -> String {
    let secs = ms.div_euclid(1000) as libc::time_t;
    // SAFETY: `localtime_r` writes only into the `tm` we pass.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { !libc::localtime_r(&secs, &mut tm).is_null() };
    if !ok {
        return utc_day(ms);
    }
    format!("{:04}-{:02}-{:02}", i64::from(tm.tm_year) + 1900, tm.tm_mon + 1, tm.tm_mday)
}

/// Today's transcript spend of one followed file (A10.4): the running total
/// for the local calendar day `day`, each `message.id` counted once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DayUsage {
    pub day: String,
    pub total: TokenUsage,
    /// At least one usage row of `day` was counted.
    pub rows: bool,
    /// Every row of `day` was read; otherwise the total is unknown and is
    /// never reported.
    pub complete: bool,
    seen: HashSet<String>,
    dirty: bool,
    last_attempt: Option<EpochMs>,
}

impl DayUsage {
    pub fn new(day: String, complete: bool) -> DayUsage {
        DayUsage { day, total: TokenUsage::default(), rows: false, complete, seen: HashSet::new(), dirty: false, last_attempt: None }
    }

    /// Count a content row's usage when its timestamp falls on `day`.
    pub fn apply(&mut self, f: &RowFacts, local_day: fn(EpochMs) -> String) {
        let (Some(u), Some(at)) = (&f.usage, f.at) else { return };
        if local_day(at) != self.day {
            return;
        }
        if let Some(id) = &f.message_id {
            if !self.seen.insert(id.clone()) {
                return;
            }
        }
        self.total.add(u);
        self.rows = true;
        self.dirty = true;
    }

    /// The report: input = uncached + cache-creation input tokens, cached =
    /// cache-read input tokens; transcripts carry no cost (`cost_micros`
    /// absent, §6.4).
    pub fn today(&self) -> UsageToday {
        let t = &self.total;
        UsageToday {
            day: self.day.clone(),
            input_tokens: Some(clamp_safe_u64(t.input.saturating_add(t.cache_creation))),
            output_tokens: Some(clamp_safe_u64(t.output)),
            cached_tokens: Some(clamp_safe_u64(t.cache_read)),
            cost_micros: None,
        }
    }

    /// A report is due: the total changed, the day is complete and has usage
    /// rows, and the last report is at least a second old.
    fn due(&self, now: EpochMs) -> bool {
        self.dirty && self.complete && self.rows && self.last_attempt.is_none_or(|t| now - t >= USAGE_INTERVAL_MS)
    }

    fn pending(&self) -> bool {
        self.dirty && self.complete && self.rows
    }
}

/// Today's usage rows before `end`, read backwards until a row of an earlier
/// day, the file start or [`USAGE_SCAN_BUDGET`]; the flag says whether the
/// whole day was read.
fn scan_day_rows(file: &File, end: u64, day: &str, local_day: fn(EpochMs) -> String) -> (Vec<RowFacts>, bool) {
    let mut rows: Vec<RowFacts> = Vec::new();
    let mut reached = false;
    let back = read_back(file, OlderCursor::at(end), USAGE_SCAN_BUDGET, LINE_CAP, &mut |line: &Line| {
        let Line::Complete { bytes, .. } = line else { return false };
        let (Some(f), LineOutcome::Content) = row_facts(bytes) else { return false };
        if let Some(at) = f.at {
            if local_day(at).as_str() < day {
                reached = true;
                return true;
            }
        }
        if f.usage.is_some() {
            rows.push(f);
        }
        false
    });
    let complete = match back {
        Ok(b) => reached || !b.has_older,
        Err(_) => false,
    };
    rows.reverse();
    (rows, complete)
}

/// A pending backward scan of today's rows (run outside the lock).
struct ScanJob {
    path: PathBuf,
    end: u64,
    day: String,
    generation: u64,
}

struct StructureFollow {
    hint: IdentityHint,
    state: StructureState,
    refs: usize,
    last_sent: Option<TranscriptFacts>,
    usage: DayUsage,
    /// Bumped on every restart of `usage`, so a stale scan is discarded.
    usage_generation: u64,
}

impl StructureFollow {
    /// A fresh follow (or a restart after a reset): structure facts from the
    /// last 4 MiB; today's spend waits for a backward scan (`ScanJob`).
    fn start(hint: IdentityHint, tail: Option<&TranscriptTail>, format: TranscriptFormat, cfg: &FollowConfig, generation: u64, path: &Path) -> (StructureFollow, Option<ScanJob>) {
        let mut state = StructureState::default();
        let today = (cfg.local_day)((cfg.clock)());
        let mut job = None;
        if let Some(tail) = tail {
            if let Ok(back) = read_back(tail.file(), OlderCursor::at(tail.offset()), INITIAL_BUDGET, LINE_CAP, &mut |_: &Line| false) {
                for line in &back.lines {
                    apply_line(&mut state, format, line);
                }
            }
            if format == TranscriptFormat::Claude {
                job = Some(ScanJob { path: path.to_path_buf(), end: tail.offset(), day: today.clone(), generation });
            }
        }
        let follow = StructureFollow {
            hint,
            state,
            refs: 1,
            last_sent: None,
            usage: DayUsage::new(today, false),
            usage_generation: generation,
        };
        (follow, job)
    }
}

struct FileEntry {
    /// The path as FSEvents reports it (realpath of the directory + name).
    canonical: PathBuf,
    format: TranscriptFormat,
    tail: Option<TranscriptTail>,
    structure: Option<StructureFollow>,
    listeners: Vec<(u64, Arc<dyn LineListener>)>,
    last_read: Option<Instant>,
    last_open_try: Option<Instant>,
}

#[derive(Default)]
struct FollowState {
    files: HashMap<PathBuf, FileEntry>,
    poked: HashSet<PathBuf>,
    next_listener: u64,
    watched_dirs: HashMap<PathBuf, usize>,
    /// Directories whose watch failed (not created yet): retried when a file
    /// in them opens.
    unwatched: HashSet<PathBuf>,
    /// Backward spend scans waiting for the worker (or the next `run_once`).
    scans: Vec<ScanJob>,
    /// Watch changes to apply outside the lock (FSEvents' `watch` joins its
    /// run-loop thread, whose callback takes this lock).
    to_watch: Vec<PathBuf>,
    to_unwatch: Vec<PathBuf>,
}

struct Shared {
    state: Mutex<FollowState>,
    wake: Condvar,
    stop: AtomicBool,
    config: FollowConfig,
    evidence: RwLock<Option<Arc<dyn EvidenceSink>>>,
    rows: RwLock<Option<Arc<dyn RowSink>>>,
    usage_gate: RwLock<Option<UsageGate>>,
    watcher: Mutex<Option<notify::RecommendedWatcher>>,
}

/// The registry of followed files (one tail per file), cheap to clone.
#[derive(Clone)]
pub struct Follower {
    shared: Arc<Shared>,
}

/// A content attachment: the lines before `offset` are read backwards by the
/// caller; every line from `offset` on reaches the listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentAttach {
    pub id: u64,
    pub offset: u64,
}

/// What one advance of one file delivers to its content listeners.
struct Delivery {
    path: PathBuf,
    listeners: Vec<Arc<dyn LineListener>>,
    advance: Advance,
    /// The tail offset after the advance (the restart point of a reset).
    offset: u64,
}

fn now_ms() -> EpochMs {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn lock(shared: &Shared) -> MutexGuard<'_, FollowState> {
    shared.state.lock().unwrap_or_else(|p| p.into_inner())
}

impl Follower {
    /// A follower without its own thread: [`Follower::run_once`] does the
    /// work (tests, and callers that drive it from their own loop).
    pub fn manual(config: FollowConfig) -> Follower {
        Follower {
            shared: Arc::new(Shared {
                state: Mutex::new(FollowState::default()),
                wake: Condvar::new(),
                stop: AtomicBool::new(false),
                config,
                evidence: RwLock::new(None),
                rows: RwLock::new(None),
                usage_gate: RwLock::new(None),
                watcher: Mutex::new(None),
            }),
        }
    }

    /// A follower with its worker thread (QoS utility) and, if configured,
    /// an FSEvents watch on the followed files' directories.
    pub fn start(config: FollowConfig) -> Follower {
        let follower = Follower::manual(config);
        if follower.shared.config.watch {
            let weak = Arc::downgrade(&follower.shared);
            let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                let (Ok(event), Some(shared)) = (res, weak.upgrade()) else { return };
                let mut st = lock(&shared);
                for p in event.paths {
                    let key = st.files.iter().find(|(k, e)| **k == p || e.canonical == p).map(|(k, _)| k.clone());
                    if let Some(k) = key {
                        st.poked.insert(k);
                    }
                }
                drop(st);
                shared.wake.notify_all();
            });
            if let Ok(w) = watcher {
                *follower.shared.watcher.lock().unwrap_or_else(|p| p.into_inner()) = Some(w);
            }
        }
        let worker = follower.clone();
        let _ = std::thread::Builder::new().name("gilde-follow".into()).spawn(move || {
            // SAFETY: plain libc call on the current thread; lowers its QoS.
            unsafe {
                libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_UTILITY, 0);
            }
            worker.worker_loop();
        });
        follower
    }

    /// Stop the worker thread (the follower keeps working in manual mode).
    pub fn shutdown(&self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.shared.wake.notify_all();
    }

    /// Where structure facts go (World's evidence input).
    pub fn set_evidence_sink(&self, sink: Arc<dyn EvidenceSink>) {
        *self.shared.evidence.write().unwrap_or_else(|p| p.into_inner()) = Some(sink);
    }

    /// Where per-row structure facts go (the team adapter).
    pub fn set_row_sink(&self, sink: Arc<dyn RowSink>) {
        *self.shared.rows.write().unwrap_or_else(|p| p.into_inner()) = Some(sink);
    }

    /// The file changed (an FSEvent from discovery or the own watch).
    pub fn poke(&self, path: &Path) {
        let mut st = lock(&self.shared);
        if st.files.contains_key(path) {
            st.poked.insert(path.to_path_buf());
        }
        drop(st);
        self.shared.wake.notify_all();
    }

    fn watch_dir(&self, st: &mut FollowState, path: &Path) {
        if !self.shared.config.watch {
            return;
        }
        let Some(dir) = path.parent().map(Path::to_path_buf) else { return };
        let count = st.watched_dirs.entry(dir.clone()).or_insert(0);
        *count += 1;
        if *count == 1 {
            st.to_watch.push(dir);
        }
    }

    /// Apply queued watch changes. Never called with the state lock held.
    fn sync_watches(&self) {
        if !self.shared.config.watch {
            return;
        }
        let (watch, unwatch) = {
            let mut st = lock(&self.shared);
            (std::mem::take(&mut st.to_watch), std::mem::take(&mut st.to_unwatch))
        };
        if watch.is_empty() && unwatch.is_empty() {
            return;
        }
        let mut failed = Vec::new();
        if let Some(w) = self.shared.watcher.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
            use notify::Watcher as _;
            for dir in unwatch {
                let _ = w.unwatch(&dir);
            }
            for dir in watch {
                if w.watch(&dir, notify::RecursiveMode::NonRecursive).is_err() {
                    failed.push(dir);
                }
            }
        }
        if !failed.is_empty() {
            let mut st = lock(&self.shared);
            for dir in failed {
                if st.watched_dirs.contains_key(&dir) {
                    st.unwatched.insert(dir);
                }
            }
        }
    }

    fn unwatch_dir(&self, st: &mut FollowState, path: &Path) {
        let Some(dir) = path.parent().map(Path::to_path_buf) else { return };
        let Some(count) = st.watched_dirs.get_mut(&dir) else { return };
        *count -= 1;
        if *count == 0 {
            st.watched_dirs.remove(&dir);
            if !st.unwatched.remove(&dir) && self.shared.config.watch {
                st.to_unwatch.push(dir);
            }
        }
    }

    fn entry<'s>(&self, st: &'s mut FollowState, path: &Path, format: TranscriptFormat) -> &'s mut FileEntry {
        if !st.files.contains_key(path) {
            self.watch_dir(st, path);
            let tail = TranscriptTail::open(path).ok();
            let canonical = match (path.parent().and_then(|d| std::fs::canonicalize(d).ok()), path.file_name()) {
                (Some(dir), Some(name)) => dir.join(name),
                _ => path.to_path_buf(),
            };
            st.files.insert(
                path.to_path_buf(),
                FileEntry {
                    canonical,
                    format,
                    tail,
                    structure: None,
                    listeners: Vec::new(),
                    last_read: None,
                    last_open_try: Some(Instant::now()),
                },
            );
        }
        st.files.get_mut(path).expect("inserted")
    }

    fn drop_if_unused(&self, st: &mut FollowState, path: &Path) {
        let unused = st.files.get(path).is_some_and(|e| e.structure.is_none() && e.listeners.is_empty());
        if unused {
            st.files.remove(path);
            st.poked.remove(path);
            self.unwatch_dir(st, path);
        }
    }

    /// Follow a file's structure for World (S3). Reads backwards ≤ 4 MiB for
    /// the current facts, then follows appends. Reference-counted per file.
    pub fn follow_structure(&self, path: &Path, format: TranscriptFormat, hint: IdentityHint) {
        let mut st = lock(&self.shared);
        let entry = self.entry(&mut st, path, format);
        if let Some(s) = entry.structure.as_mut() {
            s.refs += 1;
            s.hint = hint;
            drop(st);
            self.sync_watches();
            return;
        }
        let (follow, job) = StructureFollow::start(hint, entry.tail.as_ref(), format, &self.shared.config, 1, path);
        entry.structure = Some(follow);
        let update = take_evidence(entry);
        // Today's spend scan can read far back: the worker runs it.
        st.scans.extend(job);
        drop(st);
        self.sync_watches();
        self.emit_evidence(update);
        self.shared.wake.notify_all();
    }

    /// Release one structure reference.
    pub fn unfollow_structure(&self, path: &Path) {
        let mut st = lock(&self.shared);
        if let Some(e) = st.files.get_mut(path) {
            if let Some(s) = e.structure.as_mut() {
                s.refs = s.refs.saturating_sub(1);
                if s.refs == 0 {
                    e.structure = None;
                }
            }
        }
        self.drop_if_unused(&mut st, path);
        drop(st);
        self.sync_watches();
    }

    /// The structure facts of a followed file.
    pub fn structure(&self, path: &Path) -> Option<StructureSnapshot> {
        let st = lock(&self.shared);
        st.files.get(path)?.structure.as_ref().map(|s| s.state.snapshot())
    }

    /// Attach a content listener. Lines before the returned offset belong to
    /// the caller's initial load (backwards); every complete line from that
    /// offset on reaches `listener`. Fails when the file cannot be opened.
    pub fn attach_content(&self, path: &Path, format: TranscriptFormat, listener: Arc<dyn LineListener>) -> io::Result<ContentAttach> {
        let mut st = lock(&self.shared);
        st.next_listener += 1;
        let id = st.next_listener;
        let entry = self.entry(&mut st, path, format);
        if entry.tail.is_none() {
            match TranscriptTail::open(path) {
                Ok(t) => entry.tail = Some(t),
                Err(e) => {
                    self.drop_if_unused(&mut st, path);
                    drop(st);
                    self.sync_watches();
                    return Err(e);
                }
            }
        }
        let entry = st.files.get_mut(path).expect("present");
        let offset = entry.tail.as_ref().map_or(0, TranscriptTail::offset);
        entry.listeners.push((id, listener));
        drop(st);
        self.sync_watches();
        Ok(ContentAttach { id, offset })
    }

    /// Detach a content listener.
    pub fn detach_content(&self, path: &Path, id: u64) {
        let mut st = lock(&self.shared);
        if let Some(e) = st.files.get_mut(path) {
            e.listeners.retain(|(l, _)| *l != id);
        }
        self.drop_if_unused(&mut st, path);
        drop(st);
        self.sync_watches();
    }

    /// Paths currently followed (diagnostics).
    pub fn followed(&self) -> Vec<PathBuf> {
        lock(&self.shared).files.keys().cloned().collect()
    }

    /// Process every due file once: poked files, the content poll, missing
    /// files to retry. Returns how many files were read.
    pub fn run_once(&self) -> usize {
        self.run_due(Instant::now(), true)
    }

    fn run_due(&self, now: Instant, force: bool) -> usize {
        let cfg = self.shared.config.clone();
        let mut deliveries: Vec<Delivery> = Vec::new();
        let mut evidence = Vec::new();
        let mut rows_out: Vec<(PathBuf, IdentityHint, Vec<RowFacts>)> = Vec::new();
        let mut read = 0usize;
        let mut opened_dirs: Vec<PathBuf> = Vec::new();
        let mut scans: Vec<ScanJob> = Vec::new();
        let spend: Vec<(PathBuf, IdentityHint, UsageToday)>;
        let wall = (cfg.clock)();
        {
            let mut st = lock(&self.shared);
            let poked = std::mem::take(&mut st.poked);
            scans.append(&mut st.scans);
            let mut repoke: Vec<PathBuf> = Vec::new();
            for (path, entry) in st.files.iter_mut() {
                if entry.tail.is_none() {
                    let retry = entry.last_open_try.is_none_or(|t| now.duration_since(t) >= cfg.missing_retry);
                    if poked.contains(path) || retry || force {
                        entry.last_open_try = Some(now);
                        entry.tail = TranscriptTail::open(path).ok();
                        if entry.tail.is_none() {
                            continue;
                        }
                        if let Some(dir) = path.parent() {
                            opened_dirs.push(dir.to_path_buf());
                        }
                        if let Some(s) = entry.structure.as_mut() {
                            let (mut follow, job) = StructureFollow::start(s.hint.clone(), entry.tail.as_ref(), entry.format, &cfg, s.usage_generation + 1, path);
                            follow.refs = s.refs;
                            follow.last_sent = s.last_sent.take();
                            *s = follow;
                            scans.extend(job);
                        }
                    } else {
                        continue;
                    }
                }
                let due_poll = !entry.listeners.is_empty() && entry.last_read.is_none_or(|t| now.duration_since(t) >= cfg.poll_interval);
                let wanted = force || poked.contains(path) || due_poll;
                let rate_ok = force || entry.last_read.is_none_or(|t| now.duration_since(t) >= cfg.min_read_interval);
                if !wanted {
                    continue;
                }
                if !rate_ok {
                    // Too soon: the poke stays pending for the next round.
                    if poked.contains(path) {
                        repoke.push(path.clone());
                    }
                    continue;
                }
                entry.last_read = Some(now);
                let Some(tail) = entry.tail.as_mut() else { continue };
                let Ok(adv) = tail.advance() else { continue };
                read += 1;
                if let Some(s) = entry.structure.as_mut() {
                    if adv.reset.is_some() {
                        let (mut follow, job) = StructureFollow::start(s.hint.clone(), Some(&*tail), entry.format, &cfg, s.usage_generation + 1, path);
                        follow.refs = s.refs;
                        follow.last_sent = s.last_sent.take();
                        *s = follow;
                        scans.extend(job);
                    }
                    // A new local day starts a new total (A10.4); its rows all
                    // arrive from now on, so it is complete.
                    let today = (cfg.local_day)(wall);
                    if s.usage.day != today {
                        s.usage_generation += 1;
                        s.usage = DayUsage::new(today, true);
                    }
                    let mut facts_rows = Vec::new();
                    for line in &adv.lines {
                        if let Some(f) = apply_line(&mut s.state, entry.format, line) {
                            s.usage.apply(&f, cfg.local_day);
                            facts_rows.push(f);
                        }
                    }
                    if !facts_rows.is_empty() {
                        rows_out.push((path.clone(), s.hint.clone(), facts_rows));
                    }
                }
                if let Some(u) = take_evidence(entry) {
                    evidence.push(u);
                }
                if (!adv.lines.is_empty() || adv.reset.is_some()) && !entry.listeners.is_empty() {
                    let offset = entry.tail.as_ref().map_or(0, TranscriptTail::offset);
                    deliveries.push(Delivery {
                        path: path.clone(),
                        listeners: entry.listeners.iter().map(|(_, l)| l.clone()).collect(),
                        advance: adv,
                        offset,
                    });
                }
            }
            st.poked.extend(repoke);
            for dir in opened_dirs {
                if st.unwatched.remove(&dir) {
                    st.to_watch.push(dir);
                }
            }
            spend = take_spend(&mut st, wall);
        }
        self.sync_watches();
        for d in deliveries {
            for l in d.listeners {
                match d.advance.reset {
                    Some(reason) => l.reset(&d.path, reason, d.offset),
                    None => l.lines(&d.path, &d.advance.lines),
                }
            }
        }
        if let Some(sink) = self.shared.rows.read().unwrap_or_else(|p| p.into_inner()).clone() {
            for (path, hint, rows) in &rows_out {
                sink.rows(path, hint, rows);
            }
        }
        for u in evidence {
            self.emit_evidence(Some(u));
        }
        self.emit_spend(spend);
        self.run_scans(scans);
        read
    }

    /// Where the P32 gate comes from (see [`UsageGate`]).
    pub fn set_usage_gate(&self, gate: UsageGate) {
        *self.shared.usage_gate.write().unwrap_or_else(|p| p.into_inner()) = Some(gate);
    }

    /// Today's spend of a followed file, as counted so far.
    pub fn day_usage(&self, path: &Path) -> Option<DayUsage> {
        let st = lock(&self.shared);
        st.files.get(path)?.structure.as_ref().map(|s| s.usage.clone())
    }

    /// Run backward scans of today's rows outside the lock, then merge them
    /// into the live totals (dedupe by `message.id` across both).
    fn run_scans(&self, jobs: Vec<ScanJob>) {
        if jobs.is_empty() {
            return;
        }
        let cfg = &self.shared.config;
        for job in jobs {
            let (rows, complete) = match open_followed(&job.path) {
                Ok(file) => scan_day_rows(&file, job.end, &job.day, cfg.local_day),
                Err(_) => (Vec::new(), false),
            };
            let mut st = lock(&self.shared);
            let Some(s) = st.files.get_mut(&job.path).and_then(|e| e.structure.as_mut()) else { continue };
            if s.usage_generation != job.generation || s.usage.day != job.day {
                continue;
            }
            for f in &rows {
                s.usage.apply(f, cfg.local_day);
            }
            s.usage.complete = complete;
        }
        let spend = take_spend(&mut lock(&self.shared), (cfg.clock)());
        self.emit_spend(spend);
    }

    /// Send due spend reports whose P32 gate is on (outside the lock).
    fn emit_spend(&self, spend: Vec<(PathBuf, IdentityHint, UsageToday)>) {
        if spend.is_empty() {
            return;
        }
        let gate = self.shared.usage_gate.read().unwrap_or_else(|p| p.into_inner()).clone();
        let Some(gate) = gate else { return };
        let at = (self.shared.config.clock)();
        for (path, hint, today) in spend {
            if gate(&path, &hint) {
                self.emit_evidence(Some(EvidenceUpdate {
                    hint,
                    facts: SourceFacts::Usage(UsageFacts { source: UsageSource::Transcript, today }),
                    at,
                }));
            }
        }
    }

    fn emit_evidence(&self, update: Option<EvidenceUpdate>) {
        let Some(update) = update else { return };
        if let Some(sink) = self.shared.evidence.read().unwrap_or_else(|p| p.into_inner()).clone() {
            sink.evidence(update);
        }
    }

    fn worker_loop(&self) {
        while !self.shared.stop.load(Ordering::SeqCst) {
            self.run_due(Instant::now(), false);
            let st = lock(&self.shared);
            if !st.poked.is_empty() {
                drop(st);
                std::thread::sleep(self.shared.config.min_read_interval);
                continue;
            }
            if !st.scans.is_empty() {
                drop(st);
                continue;
            }
            let has_listeners = st.files.values().any(|e| !e.listeners.is_empty() || e.tail.is_none());
            let spend_pending = st.files.values().any(|e| e.structure.as_ref().is_some_and(|s| s.usage.pending()));
            let mut wait = if has_listeners { self.shared.config.poll_interval.min(self.shared.config.missing_retry) } else { Duration::from_secs(5) };
            if spend_pending {
                wait = wait.min(Duration::from_millis(USAGE_INTERVAL_MS as u64));
            }
            let _ = self.shared.wake.wait_timeout(st, wait);
        }
    }
}

/// Count and apply one line to a structure state; the row facts of a content
/// row are returned for the row sink.
pub fn apply_line(state: &mut StructureState, format: TranscriptFormat, line: &Line) -> Option<RowFacts> {
    match line {
        Line::TooLarge { .. } => {
            state.counters.count(&LineOutcome::TooLarge);
            None
        }
        Line::Complete { bytes, .. } => {
            let (facts, outcome) = format.facts(bytes);
            state.line(facts.as_ref(), &outcome);
            facts.filter(|_| matches!(outcome, LineOutcome::Content))
        }
    }
}

/// Collect the spend reports that are due and mark them attempted (a gate
/// that says no is not asked again until the total changes).
fn take_spend(st: &mut FollowState, now: EpochMs) -> Vec<(PathBuf, IdentityHint, UsageToday)> {
    let mut out = Vec::new();
    for (path, e) in st.files.iter_mut() {
        let Some(s) = e.structure.as_mut() else { continue };
        if s.usage.due(now) {
            s.usage.dirty = false;
            s.usage.last_attempt = Some(now);
            out.push((path.clone(), s.hint.clone(), s.usage.today()));
        }
    }
    out
}

fn take_evidence(entry: &mut FileEntry) -> Option<EvidenceUpdate> {
    let s = entry.structure.as_mut()?;
    if s.last_sent.as_ref() == Some(&s.state.facts) {
        return None;
    }
    s.last_sent = Some(s.state.facts.clone());
    Some(EvidenceUpdate { hint: s.hint.clone(), facts: SourceFacts::Transcript(s.state.facts.clone()), at: now_ms() })
}
