//! The ContentHub (contract §4.2.4, §6.1.8, §6.4): conversation content per
//! session, memory only, delivered to at most 8 view subscriptions.
//!
//! Owner: T04. Implements [`ContentHubApi`] for the producers (T05 stream,
//! T08 turns, T07) and backs the `convo_*` commands.
//!
//! **Sources.** A session's content comes from exactly one source at a time
//! (§3.2 F-06: a view never mixes block-id schemes):
//! - a **producer** (`Stream` / `TurnMemory`) publishing [`ContentUpdate`]s;
//! - a **transcript** (Claude transcript or codex rollout) followed by
//!   offset through the shared [`Follower`];
//! - **none** (nothing readable yet).
//!
//! A source change (a producer starts for a transcript-backed session, a
//! producer announces another `Source`, a followed file is replaced or
//! truncated) sends `Reset{Replaced|Truncated}` to every view of that
//! session. A producer's content survives its `Ended` frozen, so an open view
//! keeps what it showed; a new subscription after that resolves the source
//! again only when nothing is kept (≤ 8 frozen sessions without views).
//!
//! **Producer protocol** ([`ContentHubApi::publish`]): send
//! `Source{Stream|TurnMemory}` first (an `Appended`/`Reset` without it is
//! taken as `Stream`), then `Reset`/`Appended`/`Updated`/`Ended`. An
//! `Appended` whose id exists replaces that block (as `Finalized`). Block
//! `seq` values are assigned here (per session, increasing in conversation
//! order; blocks loaded by `convo_load_older` get smaller ones).
//!
//! **Views.** ≤ 8 live subscriptions; the 9th evicts the least recently
//! subscribed with `Evicted`. The first event is always `Reset`. `TextDelta`s
//! are coalesced per view (50 ms, ≤ 16 KiB per event); with no view they are
//! only folded into block state. A view whose channel holds 256 undelivered
//! events gets `Reset{Lagging}` with the current tail instead of the backlog.
//! A closed channel ends its subscription.
//!
//! **Bounds.** ≤ 2,000 blocks / 16 MiB per session (oldest dropped; a
//! transcript-backed session reloads them through `convo_load_older`, which
//! scans ≤ 8 MiB per call).
//!
//! **Team message bodies** (A27.2/A27.3, [`MessageBodyReader`],
//! [`MessageBodySource`]): read on demand from the recipient's delivering
//! turn block, else the sender's `SendMessage` tool_use, ≤ 8 MiB scanned
//! backwards per file, cut to 16 KiB; behind T07's LRU.
//!
//! Nothing here is persisted or logged (content, §3.1 rule 5).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock, Weak};
use std::time::{Duration, Instant};

use serde_json::Value;
use zeroize::Zeroizing;

use crate::commands::team::MessageBodies;
use crate::contract::common::{AbsPath, EpochMs, Notice, Vendor};
use crate::contract::content::{
    Block, BlockKind, BlockPatch, ConvoEnvelope, ConvoEvent, ConvoSourceKind, ResetReason, ToolStatus, UserOrigin,
};
use crate::contract::display::TextCap;
use crate::contract::error::IpcError;
use crate::contract::ids::{BlockId, MessageKey, SessionUid, SubId, ViewId};
use crate::contract::ipc::{ConvoSubscribeRes, MessageBodySource as BodyFrom, TeamMessageBodyRes, DEFAULT_TAIL_BLOCKS};
use crate::contract::services::{ContentHubApi, ContentUpdate, WorldApi};
use crate::contract::session::{Control, ControlMode};
use crate::contract::team::{DeliveryStatus, MessageKind};
use crate::discovery::safe_read;
use crate::follow::tail::{
    expected_real_path, followed_root, open_followed, read_back, refuse_path, Follower, Line, LineListener, OlderCursor,
    TranscriptFormat, BACK_CHUNK, INITIAL_BUDGET, LINE_CAP, OLDER_BUDGET,
};
use crate::render::blocks::{
    apply_tool_result, content_bytes, is_interrupt_marker, parse_wrapped, protocol_frame_kind, ToolResultUpdate,
    ASSISTANT_TEXT_CAP,
};
use crate::render::rollout::rollout_content;
use crate::render::sanitize::sanitize_display_text;
use crate::render::tools::refine_verb_kind;
use crate::render::transcript::{
    line_text, origin_from_kind, parse_rfc3339_ms, row_content, row_time, row_too_large_block, ContentBase, ContentItem,
    ResultMeta,
};
use crate::world::{MessageLocator, World};

/// At most this many live subscriptions (§4.2.4).
pub const MAX_SUBSCRIPTIONS: usize = 8;
/// Blocks kept per session (§6.1.8).
pub const MAX_BLOCKS: usize = 2_000;
/// Content bytes kept per session (§6.1.8).
pub const MAX_CONTENT_BYTES: usize = 16 * 1024 * 1024;
/// A view this many events behind gets `Reset{Lagging}` (§4.2.4).
pub const LAG_EVENTS: usize = 256;
/// `TextDelta` coalescing window (§4.2.4).
pub const DELTA_WINDOW: Duration = Duration::from_millis(50);
/// `TextDelta` size per event (§4.2.4).
pub const DELTA_MAX_BYTES: usize = 16 * 1024;
/// Ended producer sessions kept without a view.
pub const MAX_FROZEN: usize = 8;
/// The seq of a session's first block; older blocks count down from it.
pub const SEQ_BASE: u64 = 1 << 40;
/// Results kept for tool calls that are not loaded yet (older rows).
const MAX_ORPHAN_RESULTS: usize = 2_000;
/// The follow note is refreshed at most this often.
const NOTE_INTERVAL: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// channels and sources
// ---------------------------------------------------------------------------

/// What happened to one envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    Sent,
    /// The view cannot take more now (backpressure); try again later.
    Full,
    /// The view is gone.
    Closed,
}

/// A view's channel (`Channel<ConvoEnvelope>` in the app; fakes in tests).
pub trait ConvoChannel: Send + Sync {
    fn send(&self, envelope: ConvoEnvelope) -> SendOutcome;
}

impl ConvoChannel for tauri::ipc::Channel<ConvoEnvelope> {
    fn send(&self, envelope: ConvoEnvelope) -> SendOutcome {
        match tauri::ipc::Channel::send(self, envelope) {
            Ok(()) => SendOutcome::Sent,
            Err(_) => SendOutcome::Closed,
        }
    }
}

/// Where a session's conversation can be read.
#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedSource {
    /// A Gilde-owned session whose producer publishes (T05 stream, T08 turns).
    Producer { kind: ConvoSourceKind, note: Option<Notice> },
    /// Follow a transcript or rollout.
    File { path: PathBuf, format: TranscriptFormat, base: ContentBase, live: bool },
    /// Nothing to show (yet).
    Nothing { note: Option<Notice> },
}

/// Resolves a session to its content source (World's knowledge).
pub trait SourceResolver: Send + Sync {
    /// `NOT_FOUND{session}`, or `UNSUPPORTED{noReadableConversation}` for
    /// cursor/agy sessions started elsewhere and codex without a mapping.
    fn resolve(&self, uid: &SessionUid) -> Result<ResolvedSource, IpcError>;
}

/// What the history index knows about a session World does not hold
/// (A29.2): the index's own file for it, or that its vendor keeps nothing
/// Gilde can read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryLocation {
    /// A Claude transcript, at the index row's own **absolute** path.
    ClaudeTranscript { path: PathBuf, cwd: Option<AbsPath> },
    /// A codex rollout, at the index row's own **absolute** path.
    CodexRollout { path: PathBuf, cwd: Option<AbsPath> },
    /// A cursor chat or an agy summary: known, title only, no conversation.
    TitleOnly { vendor: Vendor },
}

/// The history index as a conversation source (A29.2, implemented by T03's
/// `HistoryService`). The path is always the index row's own absolute path,
/// never a display string. `None` = the index does not know the uid.
pub trait HistoryLocator: Send + Sync {
    fn locate(&self, uid: &SessionUid) -> Option<HistoryLocation>;
}

/// The resolver over World (T02): control mode and vendor from the session,
/// the file from [`WorldApi::transcript`] only (A10.3). The `~/…` display
/// string of `SessionDetails.transcript_path` is never turned into a path:
/// display strings are sanitized and may be cut, so they can name another
/// file. No transcript known → nothing is shown; a path is never guessed.
///
/// A29.2: World is asked first; only when World has **no session** for the
/// uid is the [`HistoryLocator`] asked (History's Read of an old session):
/// a claude transcript / codex rollout is followed read-only
/// (`live: false`), a cursor/agy row is `UNSUPPORTED{noReadableConversation}`,
/// and `NOT_FOUND{session}` only when neither knows the uid.
pub struct WorldResolver {
    world: Arc<dyn WorldApi>,
    home: Option<AbsPath>,
    history: Option<Arc<dyn HistoryLocator>>,
}

impl WorldResolver {
    pub fn new(world: Arc<dyn WorldApi>, home: Option<AbsPath>) -> WorldResolver {
        WorldResolver { world, home, history: None }
    }

    /// Fall back to `history` for a uid World does not hold (A29.2).
    pub fn with_history(mut self, history: Arc<dyn HistoryLocator>) -> WorldResolver {
        self.history = Some(history);
        self
    }

    /// The history fallback (World has no session for `uid`).
    fn resolve_from_history(&self, uid: &SessionUid) -> Result<ResolvedSource, IpcError> {
        let located = self.history.as_ref().and_then(|h| h.locate(uid));
        let (path, cwd, format) = match located {
            None => return Err(IpcError::not_found("session")),
            Some(HistoryLocation::TitleOnly { .. }) => return Err(IpcError::unsupported("noReadableConversation")),
            Some(HistoryLocation::ClaudeTranscript { path, cwd }) => (path, cwd, TranscriptFormat::Claude),
            Some(HistoryLocation::CodexRollout { path, cwd }) => (path, cwd, TranscriptFormat::CodexRollout),
        };
        // The locator's contract is an absolute index path; anything else is
        // never opened (and never guessed into one).
        if !path.is_absolute() {
            return Err(IpcError::unsupported("noReadableConversation"));
        }
        let base = ContentBase { location: cwd, home: self.home.clone() };
        Ok(ResolvedSource::File { path, format, base, live: false })
    }
}

impl SourceResolver for WorldResolver {
    fn resolve(&self, uid: &SessionUid) -> Result<ResolvedSource, IpcError> {
        let Some(mut session) = self.world.session(uid) else {
            return self.resolve_from_history(uid);
        };
        if let Some(target) = session.merged_into.clone() {
            if let Some(s) = self.world.session(&target) {
                session = s;
            }
        }
        // The file and its format come from World's absolute path and the
        // vendor it was found for; a vendor mismatch is treated as unknown.
        let transcript = self.world.transcript(&session.uid).filter(|(_, v)| *v == session.vendor);
        let path = transcript.map(|(p, _)| PathBuf::from(p.as_str()));
        let base = ContentBase { location: session.cwd.clone(), home: self.home.clone() };
        let live = !matches!(session.control, Control::NotRunning { .. });
        let earlier_not_shown = || Some(Notice::new("turn.earlierNotShown"));
        match (&session.control, session.vendor) {
            (Control::InGilde { mode: ControlMode::Conversation, .. }, _) => {
                Ok(ResolvedSource::Producer { kind: ConvoSourceKind::Stream, note: None })
            }
            (Control::InGilde { mode: ControlMode::TurnBased, .. }, v) => Ok(ResolvedSource::Producer {
                kind: ConvoSourceKind::TurnMemory,
                note: matches!(v, Vendor::Cursor | Vendor::Agy).then(earlier_not_shown).flatten(),
            }),
            (_, Vendor::Claude) => Ok(match path {
                Some(path) => ResolvedSource::File { path, format: TranscriptFormat::Claude, base, live },
                None => ResolvedSource::Nothing { note: None },
            }),
            (_, Vendor::Codex) => match path {
                Some(path) => Ok(ResolvedSource::File { path, format: TranscriptFormat::CodexRollout, base, live }),
                None => Err(IpcError::unsupported("noReadableConversation")),
            },
            (Control::NotRunning { .. }, Vendor::Cursor | Vendor::Agy)
                if session.details.control_mode == Some(ControlMode::TurnBased) =>
            {
                Ok(ResolvedSource::Nothing { note: earlier_not_shown() })
            }
            (_, Vendor::Cursor | Vendor::Agy) => Err(IpcError::unsupported("noReadableConversation")),
        }
    }
}

// ---------------------------------------------------------------------------
// state
// ---------------------------------------------------------------------------

struct FileContent {
    path: PathBuf,
    format: TranscriptFormat,
    base: ContentBase,
    attach_id: u64,
    live: bool,
    /// Lines before this are not loaded.
    older: OlderCursor,
    has_older: bool,
    /// Row start of each loaded block (load older by id).
    block_offsets: HashMap<BlockId, u64>,
    /// Results whose tool call is older than what is loaded.
    orphan_results: HashMap<String, (ToolResultUpdate, ResultMeta)>,
    last_row_at: Option<EpochMs>,
    last_note: Option<Instant>,
}

struct Content {
    source: ConvoSourceKind,
    note: Option<Notice>,
    ended: bool,
    blocks: VecDeque<Block>,
    bytes: usize,
    next_seq: u64,
    first_seq: u64,
    /// Producer content dropped from memory (cannot be reloaded).
    dropped: bool,
    file: Option<FileContent>,
    touched: Instant,
}

impl Content {
    fn new(source: ConvoSourceKind, note: Option<Notice>) -> Content {
        Content {
            source,
            note,
            ended: false,
            blocks: VecDeque::new(),
            bytes: 0,
            next_seq: SEQ_BASE,
            first_seq: SEQ_BASE,
            dropped: false,
            file: None,
            touched: Instant::now(),
        }
    }

    fn is_producer(&self) -> bool {
        matches!(self.source, ConvoSourceKind::Stream | ConvoSourceKind::TurnMemory)
    }

    fn has_older(&self) -> bool {
        self.file.as_ref().is_some_and(|f| f.has_older)
    }

    fn find(&self, id: &BlockId) -> Option<usize> {
        self.blocks.iter().rposition(|b| &b.id == id)
    }

    fn clear(&mut self) {
        self.blocks.clear();
        self.bytes = 0;
        self.next_seq = SEQ_BASE;
        self.first_seq = SEQ_BASE;
        self.dropped = false;
    }

    fn push(&mut self, mut block: Block) -> Block {
        block.seq = self.next_seq;
        self.next_seq += 1;
        if self.blocks.is_empty() {
            self.first_seq = block.seq;
        }
        self.bytes += content_bytes(&block);
        self.blocks.push_back(block.clone());
        self.enforce_bounds();
        block
    }

    fn replace(&mut self, idx: usize, mut block: Block) -> Block {
        block.seq = self.blocks[idx].seq;
        self.bytes = self.bytes.saturating_sub(content_bytes(&self.blocks[idx])) + content_bytes(&block);
        self.blocks[idx] = block.clone();
        block
    }

    fn enforce_bounds(&mut self) {
        while self.blocks.len() > MAX_BLOCKS || (self.bytes > MAX_CONTENT_BYTES && self.blocks.len() > 1) {
            let Some(old) = self.blocks.pop_front() else { break };
            self.bytes = self.bytes.saturating_sub(content_bytes(&old));
            match &mut self.file {
                Some(f) => {
                    let next = self.blocks.front().and_then(|b| f.block_offsets.get(&b.id).copied());
                    f.block_offsets.remove(&old.id);
                    if let Some(offset) = next {
                        f.older = OlderCursor::at(offset);
                        f.has_older = true;
                    }
                }
                None => self.dropped = true,
            }
            if let Some(first) = self.blocks.front() {
                self.first_seq = first.seq;
            }
        }
    }

    fn tail(&self, n: usize) -> Vec<Block> {
        let skip = self.blocks.len().saturating_sub(n);
        self.blocks.iter().skip(skip).cloned().collect()
    }

    fn tail_has_older(&self, n: usize) -> bool {
        self.blocks.len() > n || self.has_older()
    }
}

struct PendingDelta {
    block_id: BlockId,
    text: String,
    since: Instant,
}

struct Sub {
    id: SubId,
    uid: SessionUid,
    view: ViewId,
    channel: Arc<dyn ConvoChannel>,
    order: u64,
    next_seq: u64,
    queue: VecDeque<ConvoEvent>,
    delta: Option<PendingDelta>,
    tail_blocks: usize,
    closed: bool,
}

#[derive(Default)]
struct Inner {
    contents: HashMap<SessionUid, Content>,
    subs: Vec<Sub>,
    order: u64,
}

/// The ContentHub.
pub struct ContentHub {
    inner: Mutex<Inner>,
    follower: Follower,
    resolver: RwLock<Option<Arc<dyn SourceResolver>>>,
    me: Weak<ContentHub>,
    tick: Condvar,
    tick_lock: Mutex<bool>,
}

fn lock(hub: &ContentHub) -> MutexGuard<'_, Inner> {
    hub.inner.lock().unwrap_or_else(|p| p.into_inner())
}

fn now_ms() -> EpochMs {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// The follow note of a live transcript-backed view (§6.4, §9.12).
pub fn follow_note(last_row_at: Option<EpochMs>) -> Notice {
    let mut n = Notice::new("details.followNote");
    if let Some(t) = last_row_at {
        n.params.insert("time".into(), serde_json::Value::from(t));
    }
    n
}

struct HubListener {
    hub: Weak<ContentHub>,
    uid: SessionUid,
}

impl LineListener for HubListener {
    fn lines(&self, path: &Path, lines: &[Line]) {
        if let Some(hub) = self.hub.upgrade() {
            hub.file_lines(&self.uid, path, lines);
        }
    }
    fn reset(&self, path: &Path, reason: ResetReason, offset: u64) {
        if let Some(hub) = self.hub.upgrade() {
            hub.file_reset(&self.uid, path, reason, offset);
        }
    }
}

/// One parsed line: its start, the row time, and its content items.
struct ParsedLine {
    start: u64,
    at: Option<EpochMs>,
    items: Vec<ContentItem>,
}

fn parse_line(format: TranscriptFormat, line: &Line, base: &ContentBase, fallback_at: EpochMs) -> ParsedLine {
    match line {
        Line::TooLarge { start } => ParsedLine { start: *start, at: None, items: vec![ContentItem::Block(row_too_large_block(*start, fallback_at))] },
        Line::Complete { start, bytes } => {
            let items = match format {
                TranscriptFormat::Claude => row_content(bytes, *start, fallback_at, base),
                TranscriptFormat::CodexRollout => rollout_content(bytes, *start, fallback_at, base),
            };
            ParsedLine { start: *start, at: row_time(bytes), items }
        }
    }
}

fn block_count(items: &[ContentItem]) -> usize {
    items.iter().filter(|i| matches!(i, ContentItem::Block(_))).count()
}

/// Refine a tool call block with a result row's meta.
fn apply_result(block: &mut Block, update: &ToolResultUpdate, meta: &ResultMeta) {
    let (fc, verb) = match &block.kind {
        BlockKind::ToolCall { summary, file_change, .. } => {
            let mut fc = file_change.clone();
            if let Some(f) = fc.as_mut() {
                if let Some((a, r)) = meta.patch {
                    if a > 0 || r > 0 {
                        f.added = a;
                        f.removed = r;
                    }
                }
                if let Some(k) = meta.write_kind {
                    f.change = k;
                }
            }
            (fc, refine_verb_kind(&summary.tool, meta.write_kind))
        }
        _ => (None, None),
    };
    apply_tool_result(block, update, fc, verb);
}

impl ContentHub {
    /// A hub over `follower`, with its delta-flush thread.
    pub fn new(follower: Follower, resolver: Option<Arc<dyn SourceResolver>>) -> Arc<ContentHub> {
        let hub = ContentHub::new_manual(follower, resolver);
        let weak = Arc::downgrade(&hub);
        let _ = std::thread::Builder::new().name("gilde-content".into()).spawn(move || loop {
            let Some(hub) = weak.upgrade() else { return };
            let has_pending = {
                let inner = lock(&hub);
                inner.subs.iter().any(|s| s.delta.is_some() || !s.queue.is_empty())
            };
            if has_pending {
                hub.flush_deltas(Instant::now());
            }
            let guard = hub.tick_lock.lock().unwrap_or_else(|p| p.into_inner());
            let wait = if has_pending { DELTA_WINDOW / 2 } else { Duration::from_millis(500) };
            drop(hub.tick.wait_timeout(guard, wait));
        });
        hub
    }

    /// A hub without its flush thread (tests call [`ContentHub::flush_deltas`]).
    pub fn new_manual(follower: Follower, resolver: Option<Arc<dyn SourceResolver>>) -> Arc<ContentHub> {
        Arc::new_cyclic(|me| ContentHub {
            inner: Mutex::new(Inner::default()),
            follower,
            resolver: RwLock::new(resolver),
            me: me.clone(),
            tick: Condvar::new(),
            tick_lock: Mutex::new(false),
        })
    }

    pub fn set_resolver(&self, resolver: Arc<dyn SourceResolver>) {
        *self.resolver.write().unwrap_or_else(|p| p.into_inner()) = Some(resolver);
    }

    pub fn follower(&self) -> &Follower {
        &self.follower
    }

    /// Live subscriptions (diagnostics and tests).
    pub fn subscription_count(&self) -> usize {
        lock(self).subs.len()
    }

    // -- subscribe -------------------------------------------------------

    /// `convo_subscribe`.
    pub fn subscribe(
        &self,
        uid: &SessionUid,
        view: &ViewId,
        tail_blocks: Option<u16>,
        channel: Arc<dyn ConvoChannel>,
    ) -> Result<ConvoSubscribeRes, IpcError> {
        let tail = usize::from(tail_blocks.unwrap_or(DEFAULT_TAIL_BLOCKS).clamp(1, 500));
        let known = lock(self).contents.contains_key(uid);
        let mut resolved = if known { None } else { Some(self.resolve(uid)?) };
        let mut inner = lock(self);
        if !inner.contents.contains_key(uid) {
            let source = match resolved.take() {
                Some(r) => r,
                None => {
                    // The content went away between the two looks: resolve again.
                    drop(inner);
                    let r = self.resolve(uid)?;
                    inner = lock(self);
                    r
                }
            };
            if !inner.contents.contains_key(uid) {
                let content = match source {
                    ResolvedSource::Producer { kind, note } => Content::new(kind, note),
                    ResolvedSource::File { path, format, base, live } => self.open_file_content(uid, path, format, base, live, tail)?,
                    ResolvedSource::Nothing { note } => Content::new(ConvoSourceKind::None, note),
                };
                inner.contents.insert(uid.clone(), content);
            }
        }
        // Evict the least recently subscribed views; their content is released
        // only after the new view holds its own.
        let mut evicted_uids = Vec::new();
        while inner.subs.len() >= MAX_SUBSCRIPTIONS {
            let Some(idx) = inner.subs.iter().enumerate().min_by_key(|(_, s)| s.order).map(|(i, _)| i) else { break };
            let mut evicted = inner.subs.remove(idx);
            evicted.queue.clear();
            evicted.delta = None;
            Self::send_now(&mut evicted, ConvoEvent::Evicted);
            evicted_uids.push(evicted.uid);
        }
        inner.order += 1;
        let order = inner.order;
        let content = inner.contents.get_mut(uid).expect("present");
        content.touched = Instant::now();
        let source = content.source;
        let note = match (&content.file, content.source) {
            (Some(f), ConvoSourceKind::Transcript) if f.live => Some(follow_note(f.last_row_at)),
            _ => content.note.clone(),
        };
        let reset = ConvoEvent::Reset { blocks: content.tail(tail), has_older: content.tail_has_older(tail), reason: ResetReason::Initial };
        let id = SubId::mint();
        let mut sub = Sub {
            id: id.clone(),
            uid: uid.clone(),
            view: view.clone(),
            channel,
            order,
            next_seq: 0,
            queue: VecDeque::new(),
            delta: None,
            tail_blocks: tail,
            closed: false,
        };
        sub.queue.push_back(reset);
        Self::deliver(&mut sub, content);
        inner.subs.push(sub);
        for u in evicted_uids {
            self.release(&mut inner, &u);
        }
        self.reap(&mut inner);
        Ok(ConvoSubscribeRes { sub_id: id, source, note })
    }

    fn resolve(&self, uid: &SessionUid) -> Result<ResolvedSource, IpcError> {
        let resolver = self.resolver.read().unwrap_or_else(|p| p.into_inner()).clone();
        match resolver {
            Some(r) => r.resolve(uid),
            None => Ok(ResolvedSource::Nothing { note: None }),
        }
    }

    /// The live subscriptions: `(subId, viewId, uid)` (diagnostics, tests).
    pub fn views(&self) -> Vec<(SubId, ViewId, SessionUid)> {
        lock(self).subs.iter().map(|s| (s.id.clone(), s.view.clone(), s.uid.clone())).collect()
    }

    fn open_file_content(
        &self,
        uid: &SessionUid,
        path: PathBuf,
        format: TranscriptFormat,
        base: ContentBase,
        live: bool,
        tail: usize,
    ) -> Result<Content, IpcError> {
        let listener: Arc<dyn LineListener> = Arc::new(HubListener { hub: self.me.clone(), uid: uid.clone() });
        let attach = self.follower.attach_content(&path, format, listener).map_err(|_| IpcError::unsupported("noReadableConversation"))?;
        let mut content = Content::new(ConvoSourceKind::Transcript, None);
        content.file = Some(FileContent {
            path: path.clone(),
            format,
            base,
            attach_id: attach.id,
            live,
            older: OlderCursor::at(attach.offset),
            has_older: attach.offset > 0,
            block_offsets: HashMap::new(),
            orphan_results: HashMap::new(),
            last_row_at: None,
            last_note: None,
        });
        Self::initial_load(&mut content, tail, attach.offset);
        Ok(content)
    }

    /// Read backwards from `end` until `tail` blocks exist or 4 MiB were read.
    fn initial_load(content: &mut Content, tail: usize, end: u64) {
        let Some(f) = content.file.as_mut() else { return };
        f.older = OlderCursor::at(end);
        f.has_older = end > 0;
        let Ok(file) = open_followed(&f.path) else { return };
        let format = f.format;
        let base = f.base.clone();
        let mut parsed: Vec<ParsedLine> = Vec::new();
        let mut count = 0usize;
        let back = read_back(&file, OlderCursor::at(end), INITIAL_BUDGET, LINE_CAP, &mut |line: &Line| {
            let p = parse_line(format, line, &base, 0);
            count += block_count(&p.items);
            parsed.push(p);
            count >= tail
        });
        let Ok(back) = back else { return };
        f.older = back.cursor;
        f.has_older = back.has_older;
        parsed.reverse();
        Self::apply_lines(content, parsed, false);
    }

    /// Apply parsed lines (file order) to the content's tail; returns the
    /// content events for views.
    fn apply_lines(content: &mut Content, lines: Vec<ParsedLine>, emit: bool) -> Vec<ConvoEvent> {
        let mut events = Vec::new();
        for line in lines {
            let fallback = content.file.as_ref().and_then(|f| f.last_row_at).unwrap_or(0);
            if let (Some(f), Some(at)) = (content.file.as_mut(), line.at) {
                f.last_row_at = Some(f.last_row_at.map_or(at, |p| p.max(at)));
            }
            for item in line.items {
                match item {
                    ContentItem::Block(mut b) => {
                        if b.at == 0 {
                            b.at = fallback;
                        }
                        let id = b.id.clone();
                        if let Some(f) = content.file.as_mut() {
                            f.block_offsets.insert(id.clone(), line.start);
                        }
                        match content.find(&id) {
                            Some(idx) => {
                                let b = content.replace(idx, b);
                                if emit {
                                    events.push(ConvoEvent::Updated { block_id: id, patch: BlockPatch::Finalized { block: Box::new(b) } });
                                }
                            }
                            None => {
                                let b = content.push(b);
                                if emit {
                                    events.push(ConvoEvent::Appended { block: b });
                                }
                            }
                        }
                    }
                    ContentItem::ToolResult { update, meta } => {
                        let found = content.blocks.iter().rposition(|b| matches!(&b.kind, BlockKind::ToolCall { tool_use_id, .. } if *tool_use_id == update.tool_use_id));
                        match found {
                            Some(idx) => {
                                let mut block = content.blocks[idx].clone();
                                apply_result(&mut block, &update, &meta);
                                let block = content.replace(idx, block);
                                if emit {
                                    events.push(ConvoEvent::Updated {
                                        block_id: block.id.clone(),
                                        patch: BlockPatch::Finalized { block: Box::new(block) },
                                    });
                                }
                            }
                            None => {
                                if let Some(f) = content.file.as_mut() {
                                    if f.orphan_results.len() < MAX_ORPHAN_RESULTS {
                                        f.orphan_results.insert(update.tool_use_id.clone(), (update, meta));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        events
    }

    // -- unsubscribe, eviction, release --------------------------------------

    /// `convo_unsubscribe` (idempotent).
    pub fn unsubscribe(&self, sub_id: &SubId) {
        let mut inner = lock(self);
        if let Some(idx) = inner.subs.iter().position(|s| &s.id == sub_id) {
            let sub = inner.subs.remove(idx);
            self.release(&mut inner, &sub.uid);
        }
    }

    /// Drop content nobody needs: a transcript view's content when its last
    /// view leaves; an ended producer's content beyond the frozen budget.
    fn release(&self, inner: &mut Inner, uid: &SessionUid) {
        if inner.subs.iter().any(|s| &s.uid == uid) {
            return;
        }
        let drop_it = match inner.contents.get(uid) {
            Some(c) if c.file.is_some() || c.source == ConvoSourceKind::None => true,
            Some(_) => false,
            None => return,
        };
        if drop_it {
            if let Some(c) = inner.contents.remove(uid) {
                if let Some(f) = c.file {
                    self.follower.detach_content(&f.path, f.attach_id);
                }
            }
        }
        // Frozen (ended, unviewed) producer content: keep the newest few.
        let mut frozen: Vec<(Instant, SessionUid)> = inner
            .contents
            .iter()
            .filter(|(u, c)| c.ended && c.is_producer() && !inner.subs.iter().any(|s| &s.uid == *u))
            .map(|(u, c)| (c.touched, u.clone()))
            .collect();
        if frozen.len() > MAX_FROZEN {
            frozen.sort();
            for (_, u) in frozen.iter().take(frozen.len() - MAX_FROZEN) {
                inner.contents.remove(u);
            }
        }
    }

    /// Remove subscriptions whose channel closed.
    fn reap(&self, inner: &mut Inner) {
        let closed: Vec<SessionUid> = inner.subs.iter().filter(|s| s.closed).map(|s| s.uid.clone()).collect();
        if closed.is_empty() {
            return;
        }
        inner.subs.retain(|s| !s.closed);
        for uid in closed {
            self.release(inner, &uid);
        }
    }

    // -- delivery ---------------------------------------------------------

    fn send_now(sub: &mut Sub, event: ConvoEvent) -> SendOutcome {
        let env = ConvoEnvelope { sub_id: sub.id.clone(), seq: sub.next_seq, event };
        let outcome = sub.channel.send(env);
        match outcome {
            SendOutcome::Sent => sub.next_seq += 1,
            SendOutcome::Closed => sub.closed = true,
            SendOutcome::Full => {}
        }
        outcome
    }

    /// Flush the queue; a backlog of `LAG_EVENTS` becomes `Reset{Lagging}`.
    fn deliver(sub: &mut Sub, content: &Content) {
        if sub.queue.len() >= LAG_EVENTS {
            sub.queue.clear();
            sub.delta = None;
            sub.queue.push_back(ConvoEvent::Reset {
                blocks: content.tail(sub.tail_blocks),
                has_older: content.tail_has_older(sub.tail_blocks),
                reason: ResetReason::Lagging,
            });
        }
        while let Some(event) = sub.queue.pop_front() {
            let env = ConvoEnvelope { sub_id: sub.id.clone(), seq: sub.next_seq, event };
            match sub.channel.send(env.clone()) {
                SendOutcome::Sent => sub.next_seq += 1,
                SendOutcome::Full => {
                    sub.queue.push_front(env.event);
                    break;
                }
                SendOutcome::Closed => {
                    sub.closed = true;
                    sub.queue.clear();
                    break;
                }
            }
        }
    }

    fn flush_delta(sub: &mut Sub) {
        if let Some(d) = sub.delta.take() {
            sub.queue.push_back(ConvoEvent::Updated { block_id: d.block_id, patch: BlockPatch::TextDelta { append: d.text } });
        }
    }

    /// Send `events` to every view of `uid` (pending deltas first).
    fn broadcast(inner: &mut Inner, uid: &SessionUid, events: &[ConvoEvent]) {
        let Inner { contents, subs, .. } = inner;
        let Some(content) = contents.get(uid) else { return };
        for sub in subs.iter_mut().filter(|s| &s.uid == uid) {
            for e in events {
                Self::flush_delta(sub);
                sub.queue.push_back(e.clone());
            }
            Self::deliver(sub, content);
        }
    }

    /// Flush coalesced deltas older than the window and retry backlogs.
    pub fn flush_deltas(&self, now: Instant) {
        let mut inner = lock(self);
        let Inner { contents, subs, .. } = &mut *inner;
        for sub in subs.iter_mut() {
            if sub.delta.as_ref().is_some_and(|d| now.duration_since(d.since) >= DELTA_WINDOW) {
                Self::flush_delta(sub);
            }
            if !sub.queue.is_empty() {
                if let Some(content) = contents.get(&sub.uid) {
                    Self::deliver(sub, content);
                }
            }
        }
        self.reap(&mut inner);
    }

    /// Send every pending delta now (tests; quit).
    pub fn flush_all(&self) {
        let mut inner = lock(self);
        let Inner { contents, subs, .. } = &mut *inner;
        for sub in subs.iter_mut() {
            Self::flush_delta(sub);
            if let Some(content) = contents.get(&sub.uid) {
                Self::deliver(sub, content);
            }
        }
        self.reap(&mut inner);
    }

    fn wake_ticker(&self) {
        self.tick.notify_all();
    }

    // -- load older --------------------------------------------------------

    /// `convo_load_older`: blocks before `before` arrive as an `Older` event
    /// on that subscription's channel.
    pub fn load_older(&self, sub_id: &SubId, before: &BlockId, count: u16) -> Result<(), IpcError> {
        let count = usize::from(count.clamp(1, 200));
        let mut inner = lock(self);
        let Inner { contents, subs, .. } = &mut *inner;
        let sub = subs.iter_mut().find(|s| &s.id == sub_id).ok_or_else(|| IpcError::not_found("subscription"))?;
        let content = contents.get_mut(&sub.uid).ok_or_else(|| IpcError::not_found("subscription"))?;
        let (blocks, has_older) = match content.find(before) {
            Some(idx) => {
                let from = idx.saturating_sub(count);
                let mut blocks: Vec<Block> = content.blocks.range(from..idx).cloned().collect();
                let mut has_older = from > 0 || content.has_older();
                if blocks.len() < count && from == 0 && content.has_older() {
                    let (older, more) = Self::load_file_older(content, None, count - blocks.len(), true)?;
                    let mut all = older;
                    all.extend(blocks);
                    blocks = all;
                    has_older = more;
                }
                (blocks, has_older)
            }
            None => {
                let offset = content.file.as_ref().and_then(|f| f.block_offsets.get(before).copied());
                match offset {
                    Some(o) => Self::load_file_older(content, Some(OlderCursor::at(o)), count, false)?,
                    None => return Err(IpcError::not_found("block")),
                }
            }
        };
        Self::flush_delta(sub);
        sub.queue.push_back(ConvoEvent::Older { blocks, has_older });
        Self::deliver(sub, content);
        self.reap(&mut inner);
        Ok(())
    }

    /// Read older rows of a transcript-backed content. With `cursor = None`
    /// the read continues before the loaded window and the blocks join the
    /// session's memory (with smaller seqs); otherwise they are only returned.
    fn load_file_older(content: &mut Content, cursor: Option<OlderCursor>, count: usize, extend: bool) -> Result<(Vec<Block>, bool), IpcError> {
        let Some(f) = content.file.as_mut() else { return Ok((Vec::new(), false)) };
        let start = cursor.unwrap_or(f.older);
        if start.end == 0 {
            return Ok((Vec::new(), false));
        }
        let file = open_followed(&f.path).map_err(|_| IpcError::not_found("block"))?;
        let format = f.format;
        let base = f.base.clone();
        let mut parsed: Vec<ParsedLine> = Vec::new();
        let mut n = 0usize;
        let back = read_back(&file, start, OLDER_BUDGET, LINE_CAP, &mut |line: &Line| {
            let p = parse_line(format, line, &base, 0);
            n += block_count(&p.items);
            parsed.push(p);
            n >= count
        })
        .map_err(|_| IpcError::internal("older-read"))?;
        parsed.reverse();
        // Results inside this batch apply to its calls; orphans from newer rows too.
        let mut blocks: Vec<(u64, Block)> = Vec::new();
        let mut fallback = 0;
        for line in parsed {
            if let Some(at) = line.at {
                fallback = at;
            }
            for item in line.items {
                match item {
                    ContentItem::Block(mut b) => {
                        if b.at == 0 {
                            b.at = fallback;
                        }
                        blocks.push((line.start, b));
                    }
                    ContentItem::ToolResult { update, meta } => {
                        if let Some((_, b)) = blocks.iter_mut().rev().find(|(_, b)| matches!(&b.kind, BlockKind::ToolCall { tool_use_id, .. } if *tool_use_id == update.tool_use_id)) {
                            apply_result(b, &update, &meta);
                        }
                    }
                }
            }
        }
        for (_, b) in blocks.iter_mut() {
            if let BlockKind::ToolCall { tool_use_id, status: ToolStatus::Running, .. } = &b.kind {
                if let Some((update, meta)) = f.orphan_results.remove(tool_use_id.as_str()) {
                    apply_result(b, &update, &meta);
                }
            }
        }
        let has_older = back.has_older;
        let mut seq = content.first_seq;
        let mut out = Vec::with_capacity(blocks.len());
        for (_, b) in blocks.iter_mut().rev() {
            seq = seq.saturating_sub(1);
            b.seq = seq;
        }
        let room = MAX_BLOCKS.saturating_sub(content.blocks.len());
        let keep = extend && blocks.len() <= room;
        if let Some(f) = content.file.as_mut() {
            if keep || cursor.is_none() {
                f.older = back.cursor;
                f.has_older = back.has_older;
            }
            for (offset, b) in &blocks {
                f.block_offsets.insert(b.id.clone(), *offset);
            }
        }
        if keep {
            for (_, b) in blocks.iter().rev() {
                content.bytes += content_bytes(b);
                content.blocks.push_front(b.clone());
            }
            if let Some(first) = content.blocks.front() {
                content.first_seq = first.seq;
            }
        } else if !blocks.is_empty() {
            content.first_seq = blocks[0].1.seq;
        }
        out.extend(blocks.into_iter().map(|(_, b)| b));
        Ok((out, has_older))
    }

    // -- transcript lines ----------------------------------------------------

    fn file_lines(&self, uid: &SessionUid, path: &Path, lines: &[Line]) {
        let mut inner = lock(self);
        let events = {
            let Some(content) = inner.contents.get_mut(uid) else { return };
            let Some(f) = content.file.as_ref() else { return };
            if f.path != path {
                return;
            }
            let format = f.format;
            let base = f.base.clone();
            let fallback = f.last_row_at.unwrap_or_else(now_ms);
            let parsed: Vec<ParsedLine> = lines.iter().map(|l| parse_line(format, l, &base, fallback)).collect();
            let before = content.file.as_ref().and_then(|f| f.last_row_at);
            let mut events = Self::apply_lines(content, parsed, true);
            if let Some(f) = content.file.as_mut() {
                let due = f.last_note.is_none_or(|t| t.elapsed() >= NOTE_INTERVAL);
                if f.live && f.last_row_at != before && due {
                    f.last_note = Some(Instant::now());
                    events.push(ConvoEvent::Source { source: ConvoSourceKind::Transcript, note: Some(follow_note(f.last_row_at)) });
                }
            }
            events
        };
        if !events.is_empty() {
            Self::broadcast(&mut inner, uid, &events);
            self.reap(&mut inner);
        }
    }

    fn file_reset(&self, uid: &SessionUid, path: &Path, reason: ResetReason, offset: u64) {
        let mut inner = lock(self);
        let Inner { contents, subs, .. } = &mut *inner;
        let Some(content) = contents.get_mut(uid) else { return };
        if content.file.as_ref().is_none_or(|f| f.path != path) {
            return;
        }
        content.clear();
        if let Some(f) = content.file.as_mut() {
            f.block_offsets.clear();
            f.orphan_results.clear();
            f.last_row_at = None;
        }
        let max_tail = subs.iter().filter(|s| &s.uid == uid).map(|s| s.tail_blocks).max().unwrap_or(usize::from(DEFAULT_TAIL_BLOCKS));
        Self::initial_load(content, max_tail, offset);
        for sub in subs.iter_mut().filter(|s| &s.uid == uid) {
            sub.queue.clear();
            sub.delta = None;
            sub.queue.push_back(ConvoEvent::Reset { blocks: content.tail(sub.tail_blocks), has_older: content.tail_has_older(sub.tail_blocks), reason });
            Self::deliver(sub, content);
        }
        self.reap(&mut inner);
    }

    // -- producers ------------------------------------------------------------

    /// Switch a session to another source: views get `Reset{Replaced}`.
    fn switch_source(&self, inner: &mut Inner, uid: &SessionUid, kind: ConvoSourceKind, note: Option<Notice>) {
        let Inner { contents, subs, .. } = inner;
        let content = contents.entry(uid.clone()).or_insert_with(|| Content::new(kind, None));
        if let Some(f) = content.file.take() {
            self.follower.detach_content(&f.path, f.attach_id);
        }
        content.clear();
        content.source = kind;
        content.note = note.clone();
        content.ended = false;
        for sub in subs.iter_mut().filter(|s| &s.uid == uid) {
            sub.queue.clear();
            sub.delta = None;
            sub.queue.push_back(ConvoEvent::Reset { blocks: Vec::new(), has_older: false, reason: ResetReason::Replaced });
            sub.queue.push_back(ConvoEvent::Source { source: kind, note: note.clone() });
            Self::deliver(sub, content);
        }
    }

    fn publish_locked(&self, inner: &mut Inner, uid: &SessionUid, update: ContentUpdate) {
        // A producer for a session that shows a transcript (or nothing) takes over.
        let current = inner.contents.get(uid).map(|c| c.source);
        let producer_kind = match &update {
            ContentUpdate::Source { source, .. } => *source,
            _ => match current {
                Some(k @ (ConvoSourceKind::Stream | ConvoSourceKind::TurnMemory)) => k,
                _ => ConvoSourceKind::Stream,
            },
        };
        if current != Some(producer_kind) {
            let note = match &update {
                ContentUpdate::Source { note, .. } => note.clone(),
                _ => None,
            };
            if current.is_none() {
                inner.contents.insert(uid.clone(), Content::new(producer_kind, note));
            } else {
                self.switch_source(inner, uid, producer_kind, note);
            }
            if matches!(update, ContentUpdate::Source { .. }) {
                return;
            }
        }
        let viewed = inner.subs.iter().any(|s| &s.uid == uid);
        let Some(content) = inner.contents.get_mut(uid) else { return };
        content.touched = Instant::now();
        let mut events: Vec<ConvoEvent> = Vec::new();
        match update {
            ContentUpdate::Source { source, note } => {
                content.note = note.clone();
                events.push(ConvoEvent::Source { source, note });
            }
            ContentUpdate::Reset { blocks, has_older, reason } => {
                content.clear();
                content.ended = false;
                for b in blocks {
                    content.push(b);
                }
                let Inner { contents, subs, .. } = &mut *inner;
                let content = contents.get(uid).expect("present");
                for sub in subs.iter_mut().filter(|s| &s.uid == uid) {
                    sub.queue.clear();
                    sub.delta = None;
                    sub.queue.push_back(ConvoEvent::Reset { blocks: content.tail(sub.tail_blocks), has_older: has_older || content.blocks.len() > sub.tail_blocks, reason });
                    Self::deliver(sub, content);
                }
                self.reap(inner);
                return;
            }
            ContentUpdate::Appended(block) => {
                let id = block.id.clone();
                match content.find(&id) {
                    Some(idx) => {
                        let b = content.replace(idx, block);
                        events.push(ConvoEvent::Updated { block_id: id, patch: BlockPatch::Finalized { block: Box::new(b) } });
                    }
                    None => events.push(ConvoEvent::Appended { block: content.push(block) }),
                }
            }
            ContentUpdate::Updated { block_id, patch } => {
                let Some(idx) = content.find(&block_id) else { return };
                match patch {
                    BlockPatch::TextDelta { append } => {
                        let mut appended = String::new();
                        let mut cut = false;
                        if let BlockKind::AssistantText { markdown, .. } = &mut content.blocks[idx].kind {
                            let crate::contract::display::TextCap::Bytes(cap) = ASSISTANT_TEXT_CAP else { unreachable!("bytes") };
                            let room = cap.saturating_sub(markdown.len());
                            let s = sanitize_display_text(&append, crate::contract::display::TextCap::Bytes(room));
                            cut = s.truncated;
                            markdown.push_str(&s.text);
                            appended = s.text;
                        } else if let BlockKind::Thinking { text, .. } = &mut content.blocks[idx].kind {
                            let t = text.get_or_insert_with(String::new);
                            let s = sanitize_display_text(&append, crate::contract::display::TextCap::Bytes((64 * 1024usize).saturating_sub(t.len())));
                            cut = s.truncated;
                            t.push_str(&s.text);
                            appended = s.text;
                        }
                        content.bytes += appended.len();
                        content.blocks[idx].truncated |= cut;
                        if !viewed || appended.is_empty() {
                            return;
                        }
                        let now = Instant::now();
                        let Inner { contents, subs, .. } = &mut *inner;
                        let content = contents.get(uid).expect("present");
                        for sub in subs.iter_mut().filter(|s| &s.uid == uid) {
                            match &mut sub.delta {
                                Some(d) if d.block_id == block_id => d.text.push_str(&appended),
                                _ => {
                                    Self::flush_delta(sub);
                                    sub.delta = Some(PendingDelta { block_id: block_id.clone(), text: appended.clone(), since: now });
                                }
                            }
                            if sub.delta.as_ref().is_some_and(|d| d.text.len() >= DELTA_MAX_BYTES) {
                                Self::flush_delta(sub);
                                Self::deliver(sub, content);
                            }
                        }
                        self.wake_ticker();
                        return;
                    }
                    BlockPatch::ToolResult { status, preview } => {
                        if let BlockKind::ToolCall { status: s, result_preview, .. } = &mut content.blocks[idx].kind {
                            *s = status;
                            *result_preview = preview.clone();
                        }
                        events.push(ConvoEvent::Updated { block_id, patch: BlockPatch::ToolResult { status, preview } });
                    }
                    BlockPatch::Interaction { outcome } => {
                        if let BlockKind::Interaction { outcome: o, .. } = &mut content.blocks[idx].kind {
                            *o = outcome;
                        }
                        events.push(ConvoEvent::Updated { block_id, patch: BlockPatch::Interaction { outcome } });
                    }
                    BlockPatch::Finalized { block } => {
                        let b = content.replace(idx, *block);
                        events.push(ConvoEvent::Updated { block_id, patch: BlockPatch::Finalized { block: Box::new(b) } });
                    }
                }
            }
            ContentUpdate::Ended => {
                content.ended = true;
                events.push(ConvoEvent::Ended);
            }
        }
        if viewed {
            Self::broadcast(inner, uid, &events);
            self.reap(inner);
        } else if matches!(events.last(), Some(ConvoEvent::Ended)) {
            self.release(inner, uid);
        }
    }
}

impl ContentHubApi for ContentHub {
    fn publish(&self, uid: &SessionUid, update: ContentUpdate) {
        let mut inner = lock(self);
        self.publish_locked(&mut inner, uid, update);
    }

    fn has_subscribers(&self, uid: &SessionUid) -> bool {
        lock(self).subs.iter().any(|s| &s.uid == uid)
    }
}

// ---------------------------------------------------------------------------
// team message bodies on demand (A27.2, A27.3)
// ---------------------------------------------------------------------------

/// A body is cut to this, grapheme-safe, `…` included (§4.2.6).
pub const BODY_TEXT_CAP: TextCap = TextCap::Bytes(16 * 1024);
/// The sender-written summary (≤ 200 chars, R5 §2.3).
pub const BODY_SUMMARY_CAP: TextCap = TextCap::Chars(200);
/// One lookup reads at most this much of each transcript, from its end, so a
/// huge transcript cannot stall the call (A27.2).
pub const BODY_SCAN_BUDGET: u64 = 8 * 1024 * 1024;
/// A message without an exact id is matched to a row within this window of
/// the time World observed it (the §7.2.4 join window).
pub const BODY_WINDOW_MS: EpochMs = 120_000;

/// What one lookup read, per file (the scan bound is a test).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BodyScan {
    pub recipient_bytes: u64,
    pub sender_bytes: u64,
}

/// A body as found (raw; sanitized and cut when it is returned).
struct Found {
    text: String,
    summary: Option<String>,
}

/// The nearest window candidate; a tie at the nearest distance is ambiguous
/// and matches nothing (a wrong body is worse than none).
#[derive(Default)]
struct Nearest {
    best: Option<(EpochMs, Found)>,
    tie: bool,
}

impl Nearest {
    fn offer(&mut self, distance: EpochMs, make: impl FnOnce() -> Found) {
        match &self.best {
            Some((d, _)) if *d < distance => {}
            Some((d, _)) if *d == distance => self.tie = true,
            _ => {
                self.best = Some((distance, make()));
                self.tie = false;
            }
        }
    }

    fn take(self) -> Option<Found> {
        if self.tie {
            None
        } else {
            self.best.map(|(_, f)| f)
        }
    }
}

/// A27.2: reads one team message's body on demand — first the recipient's
/// turn block that delivered it, else the sender's `SendMessage` tool_use.
///
/// **[content]**: the body is returned and never stored, logged or put into
/// an error. Transcripts are opened only at the paths World knows
/// ([`WorldApi::transcript`], A10.3; a display path is never turned into
/// one), only Claude `.jsonl` files, through `safe_read` (the never-open
/// filter, `O_NOFOLLOW`, a regular file under its own directory), and each
/// is scanned backwards for at most [`BODY_SCAN_BUDGET`] bytes.
///
/// Matching, most exact first:
/// * recipient: the row `uuid` + block index World saw deliver it, else a
///   block whose `msg_id` derives this message's key, else (only for a
///   message World knows as delivered, or of unknown status, and when no
///   delivering row was recorded) the block of the same sender and kind in
///   the row nearest to the observed time within [`BODY_WINDOW_MS`];
/// * sender: the `SendMessage` tool_use with the recorded id, else the one
///   whose result row's `msg_id` derives this key, else (no id known) the
///   call to the same recipient nearest in time within the window; calls
///   whose result names another message are never taken.
pub struct MessageBodyReader {
    world: Arc<dyn WorldApi>,
}

impl MessageBodyReader {
    pub fn new(world: Arc<dyn WorldApi>) -> MessageBodyReader {
        MessageBodyReader { world }
    }

    /// The body of the located message (`unavailable` when neither
    /// transcript has it within the scan bound).
    pub fn read(&self, loc: &MessageLocator) -> TeamMessageBodyRes {
        self.read_scanned(loc).0
    }

    /// [`MessageBodyReader::read`] plus what it read.
    pub fn read_scanned(&self, loc: &MessageLocator) -> (TeamMessageBodyRes, BodyScan) {
        let mut scan = BodyScan::default();
        let mut result = None;
        if let Some(file) = loc.recipient.as_ref().and_then(|uid| self.open(uid)) {
            let (found, bytes) = from_recipient(&file, loc);
            scan.recipient_bytes = bytes;
            result = found.map(|f| (f, BodyFrom::RecipientTranscript));
        }
        if result.is_none() {
            if let Some(file) = loc.sender.as_ref().and_then(|uid| self.open(uid)) {
                let (found, bytes) = from_sender(&file, loc);
                scan.sender_bytes = bytes;
                result = found.map(|f| (f, BodyFrom::SenderToolUse));
            }
        }
        let res = match result {
            Some((found, source)) => TeamMessageBodyRes {
                text: Some(sanitize_display_text(&found.text, BODY_TEXT_CAP).text),
                summary: found.summary.map(|s| sanitize_display_text(&s, BODY_SUMMARY_CAP).text),
                source,
            },
            None => TeamMessageBodyRes { text: None, summary: None, source: BodyFrom::Unavailable },
        };
        // Ids and counts only, never the body.
        tracing::trace!(
            target: "follow",
            source = ?res.source,
            recipient_bytes = scan.recipient_bytes,
            sender_bytes = scan.sender_bytes,
            "team message body lookup"
        );
        (res, scan)
    }

    /// A session's Claude transcript, opened read-only through `safe_read`
    /// under its **vendor root** (A50.1: `<cfg>/projects`, not the file's own
    /// directory, which a symlinked project dir would move out of the root),
    /// with no symlinked directory below that root.
    fn open(&self, uid: &SessionUid) -> Option<safe_read::Opened> {
        let (path, vendor) = self.world.transcript(uid)?;
        if vendor != Vendor::Claude {
            return None;
        }
        let path = Path::new(path.as_str());
        if refuse_path(path) {
            return None;
        }
        let expected = expected_real_path(path).ok()?;
        if std::fs::canonicalize(path).ok()? != expected {
            return None;
        }
        let root = safe_read::Root::new(followed_root(path)?).ok()?;
        safe_read::open_in(&root, path).ok()
    }
}

/// A27.3: the `team_message_body` source — World's locator (A27.1) and the
/// reader (A27.2) behind T07's [`MessageBodies`] (its LRU of 200 bodies,
/// memory only). A message World no longer holds → `None` (the command
/// answers `unavailable`).
pub struct MessageBodySource {
    world: Arc<World>,
    reader: MessageBodyReader,
}

impl MessageBodySource {
    pub fn new(world: Arc<World>) -> MessageBodySource {
        let api: Arc<dyn WorldApi> = world.clone();
        MessageBodySource { world, reader: MessageBodyReader::new(api) }
    }
}

impl MessageBodies for MessageBodySource {
    fn body(&self, key: &MessageKey) -> Option<TeamMessageBodyRes> {
        let loc = self.world.message_locator(key)?;
        Some(self.reader.read(&loc))
    }
}

/// Walk `file`'s lines from its end, newest first, reading at most `budget`
/// bytes; `visit` returns `true` to stop. A line that does not fit in the
/// budget is never visited. Buffers are wiped when dropped. Returns the
/// bytes read.
fn scan_back(file: &safe_read::Opened, budget: u64, visit: &mut dyn FnMut(&[u8]) -> bool) -> u64 {
    let mut pos = file.id.size;
    let mut scanned = 0u64;
    let mut carry: Zeroizing<Vec<u8>> = Zeroizing::new(Vec::new());
    while pos > 0 && scanned < budget {
        // A long partial line doubles the next read, so the copying stays
        // linear in the bytes read.
        let want = BACK_CHUNK.max(carry.len() as u64).min(pos).min(budget - scanned);
        let Ok(chunk) = file.read_range(pos - want, want as usize) else { break };
        if (chunk.len() as u64) < want {
            break; // truncated while being read
        }
        scanned += want;
        pos -= want;
        let mut buf = Zeroizing::new(Vec::with_capacity(chunk.len() + carry.len()));
        buf.extend_from_slice(&chunk);
        buf.extend_from_slice(&carry);
        let mut end = buf.len();
        while let Some(i) = memchr::memrchr(b'\n', &buf[..end]) {
            if i + 1 < end && visit(&buf[i + 1..end]) {
                return scanned;
            }
            end = i;
        }
        if pos == 0 {
            if end > 0 {
                visit(&buf[..end]);
            }
            return scanned;
        }
        buf.truncate(end);
        carry = buf;
    }
    scanned
}

fn contains(line: &[u8], needle: &[u8]) -> bool {
    memchr::memmem::find(line, needle).is_some()
}

fn parse_row(line: &[u8]) -> Option<Value> {
    let text = line_text(line).ok()?;
    serde_json::from_str(&text).ok()
}

fn row_at(v: &Value) -> Option<EpochMs> {
    v.get("timestamp").and_then(Value::as_str).and_then(parse_rfc3339_ms)
}

fn flag(v: &Value, key: &str) -> bool {
    v.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// The name before `@` (`alpha@session-…` → `alpha`).
fn native(name: &str) -> &str {
    name.split('@').next().unwrap_or(name)
}

fn kind_of(body: &str) -> MessageKind {
    protocol_frame_kind(body).unwrap_or(MessageKind::Plain)
}

/// One wrapped block of a recipient's row.
struct RowBlock {
    sender: Option<String>,
    msg_id: Option<String>,
    summary: Option<String>,
    kind: MessageKind,
    body: String,
}

impl RowBlock {
    fn found(&self) -> Found {
        Found { text: self.body.clone(), summary: self.summary.clone() }
    }
}

/// The wrapped blocks of one text, exactly as `render::transcript::row_facts`
/// builds `RowFacts::wrapped` (so a recorded block index finds this block):
/// the wrappers, else — for a non-human origin — the whole text as one block.
fn push_blocks(out: &mut Vec<RowBlock>, text: &str, origin: UserOrigin) {
    match parse_wrapped(text) {
        Some(ws) => out.extend(ws.into_iter().map(|w| RowBlock {
            sender: w.attr("teammate_id").or_else(|| w.attr("from")).map(|n| native(n).to_string()),
            msg_id: w.attr("msg_id").map(str::to_string),
            summary: w.attr("summary").map(str::to_string),
            kind: kind_of(&w.body),
            body: w.body,
        })),
        None if origin != UserOrigin::Human => out.push(RowBlock {
            sender: None,
            msg_id: None,
            summary: None,
            kind: MessageKind::Plain,
            body: text.to_string(),
        }),
        None => {}
    }
}

fn origin_of(v: Option<&Value>) -> UserOrigin {
    v.and_then(|o| o.get("kind")).and_then(Value::as_str).and_then(origin_from_kind).unwrap_or(UserOrigin::Human)
}

/// A `queued_command` prompt: a string, or text items joined by newlines.
fn prompt_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Array(items) => Some(
            items
                .iter()
                .filter(|i| i.get("type").and_then(Value::as_str).is_none_or(|t| t == "text"))
                .filter_map(|i| i.get("text").and_then(Value::as_str).or_else(|| i.as_str()))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => None,
    }
}

/// The wrapped blocks of a user row or a `queued_command` attachment.
fn row_blocks(v: &Value) -> Vec<RowBlock> {
    let mut out = Vec::new();
    match v.get("type").and_then(Value::as_str) {
        Some("user") => {
            let origin = origin_of(v.get("origin"));
            let meta = flag(v, "isMeta") || flag(v, "isCompactSummary");
            let mut user_text = |text: &str| {
                if !is_interrupt_marker(text) && !meta {
                    push_blocks(&mut out, text, origin);
                }
            };
            match v.get("message").and_then(|m| m.get("content")) {
                Some(Value::String(text)) => user_text(text),
                Some(Value::Array(items)) => {
                    let ty = |i: &Value| i.get("type").and_then(Value::as_str).unwrap_or("").to_string();
                    let has_result = items.iter().any(|i| {
                        let t = ty(i);
                        t == "tool_result" || t.ends_with("_tool_result")
                    });
                    if !has_result {
                        for i in items.iter().filter(|i| ty(i) == "text") {
                            if let Some(text) = i.get("text").and_then(Value::as_str) {
                                user_text(text);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        Some("attachment") => {
            let a = v.get("attachment");
            if a.and_then(|a| a.get("type")).and_then(Value::as_str) == Some("queued_command") {
                let origin = origin_of(a.and_then(|a| a.get("origin")));
                if origin != UserOrigin::Human {
                    if let Some(text) = a.and_then(|a| a.get("prompt")).and_then(prompt_text) {
                        push_blocks(&mut out, &text, origin);
                    }
                }
            }
        }
        _ => {}
    }
    out
}

/// The recipient's turn block that delivered the message.
fn from_recipient(file: &safe_read::Opened, loc: &MessageLocator) -> (Option<Found>, u64) {
    let uuid_needle = loc.delivered.as_ref().map(|d| format!("\"{}\"", d.row_uuid));
    // Only a message World knows as delivered (or of unknown status) is
    // matched by time, and only when no delivering row was recorded: a
    // message still on its way must not take an earlier block's body.
    let by_time = loc.delivered.is_none()
        && loc.sender_name.is_some()
        && matches!(loc.status, DeliveryStatus::Delivered | DeliveryStatus::Unknown);
    let mut exact: Option<Found> = None;
    let mut nearest = Nearest::default();
    let scanned = scan_back(file, BODY_SCAN_BUDGET, &mut |line| {
        let candidate = uuid_needle.as_deref().is_some_and(|n| contains(line, n.as_bytes()))
            || contains(line, b"-message")
            || contains(line, b"task-notification")
            || contains(line, b"queued_command")
            || contains(line, b"\"origin\"");
        if !candidate {
            return false;
        }
        let Some(v) = parse_row(line) else { return false };
        let blocks = row_blocks(&v);
        if blocks.is_empty() {
            return false;
        }
        // 1. The delivering row World saw (the block must still agree).
        if let Some(d) = &loc.delivered {
            if v.get("uuid").and_then(Value::as_str) == Some(d.row_uuid.as_str()) {
                if let Some(b) = blocks.get(d.index) {
                    let same_sender = match (&b.sender, &loc.sender_name) {
                        (Some(a), Some(b)) => a == b,
                        _ => true,
                    };
                    if b.kind == loc.kind && same_sender {
                        exact = Some(b.found());
                        return true;
                    }
                }
            }
        }
        if flag(&v, "isSidechain") {
            return false;
        }
        let at = row_at(&v);
        for b in &blocks {
            // 2. A block carrying this message's msg_id; another msg_id is
            // another message.
            if let Some(id) = &b.msg_id {
                if MessageKey::derive(&loc.team, id) == loc.key {
                    exact = Some(b.found());
                    return true;
                }
                continue;
            }
            // 3. Same sender and kind, nearest in time within the window.
            if by_time && b.kind == loc.kind && b.sender.is_some() && b.sender == loc.sender_name {
                if let Some(at) = at {
                    let d = (at - loc.at).abs();
                    if d <= BODY_WINDOW_MS {
                        nearest.offer(d, || b.found());
                    }
                }
            }
        }
        false
    });
    (exact.or_else(|| nearest.take()), scanned)
}

/// One `SendMessage` call of an assistant row.
struct SendCall {
    id: String,
    to: Option<String>,
    kind: MessageKind,
    text: Option<String>,
    summary: Option<String>,
}

impl SendCall {
    fn found(&self) -> Option<Found> {
        Some(Found { text: self.text.clone()?, summary: self.summary.clone() })
    }
}

/// The `SendMessage` calls of an assistant row: `to` (or `recipient`), the
/// `message` (a string, or a protocol object as compact JSON; else `content`
/// or `text`) and the `summary`.
fn send_calls(v: &Value) -> Vec<SendCall> {
    if v.get("type").and_then(Value::as_str) != Some("assistant") {
        return Vec::new();
    }
    let Some(Value::Array(items)) = v.get("message").and_then(|m| m.get("content")) else { return Vec::new() };
    items
        .iter()
        .filter(|i| matches!(i.get("type").and_then(Value::as_str), Some("tool_use" | "server_tool_use" | "mcp_tool_use")))
        .filter(|i| i.get("name").and_then(Value::as_str) == Some("SendMessage"))
        .filter_map(|i| {
            let id = i.get("id").and_then(Value::as_str)?.to_string();
            let input = i.get("input");
            let field = |k: &str| input.and_then(|x| x.get(k));
            let to = field("to").or_else(|| field("recipient")).and_then(Value::as_str).map(|t| native(t).to_string());
            let text = ["message", "content", "text"].iter().find_map(|k| match field(k) {
                Some(Value::String(s)) => Some(s.clone()),
                Some(Value::Null) | None => None,
                Some(other) => Some(other.to_string()),
            });
            let summary = field("summary").and_then(Value::as_str).map(str::to_string);
            let kind = text.as_deref().map_or(MessageKind::Plain, kind_of);
            Some(SendCall { id, to, kind, text, summary })
        })
        .collect()
}

/// The `(tool_use_id, msg_id)` pairs of a user result row
/// (`toolUseResult.msg_id`, R6 §5 LP-16).
fn send_results(v: &Value) -> Vec<(String, String)> {
    if v.get("type").and_then(Value::as_str) != Some("user") {
        return Vec::new();
    }
    let Some(msg_id) = v.get("toolUseResult").and_then(|r| r.get("msg_id")).and_then(Value::as_str) else {
        return Vec::new();
    };
    let Some(Value::Array(items)) = v.get("message").and_then(|m| m.get("content")) else { return Vec::new() };
    items
        .iter()
        .filter(|i| i.get("type").and_then(Value::as_str).is_some_and(|t| t == "tool_result" || t.ends_with("_tool_result")))
        .filter_map(|i| i.get("tool_use_id").and_then(Value::as_str))
        .map(|id| (id.to_string(), msg_id.to_string()))
        .collect()
}

/// The sender's `SendMessage` tool_use for the message.
fn from_sender(file: &safe_read::Opened, loc: &MessageLocator) -> (Option<Found>, u64) {
    let id_needle = loc.tool_use_id.as_ref().map(|id| format!("\"{id}\""));
    // Result rows come after their call, so a backward scan learns a call's
    // msg_id before it reaches the call.
    let mut target = loc.tool_use_id.clone();
    let mut foreign: HashSet<String> = HashSet::new();
    let mut exact: Option<Found> = None;
    let mut nearest = Nearest::default();
    let scanned = scan_back(file, BODY_SCAN_BUDGET, &mut |line| {
        let candidate = id_needle.as_deref().is_some_and(|n| contains(line, n.as_bytes()))
            || contains(line, b"SendMessage")
            || contains(line, b"msg_id");
        if !candidate {
            return false;
        }
        let Some(v) = parse_row(line) else { return false };
        for (id, msg_id) in send_results(&v) {
            if MessageKey::derive(&loc.team, &msg_id) == loc.key {
                target.get_or_insert(id);
            } else {
                foreign.insert(id);
            }
        }
        if flag(&v, "isSidechain") {
            return false;
        }
        let at = row_at(&v);
        for c in send_calls(&v) {
            if target.as_deref() == Some(c.id.as_str()) {
                exact = c.found();
                return true;
            }
            if target.is_none()
                && !foreign.contains(&c.id)
                && c.kind == loc.kind
                && c.to.is_some()
                && c.to == loc.recipient_name
                && c.text.is_some()
            {
                if let Some(at) = at {
                    let d = (at - loc.at).abs();
                    if d <= BODY_WINDOW_MS {
                        nearest.offer(d, || c.found().expect("text checked"));
                    }
                }
            }
        }
        false
    });
    // A known call that was not found within the bound is never replaced by
    // a time match.
    let found = exact.or_else(|| if target.is_none() { nearest.take() } else { None });
    (found, scanned)
}

#[cfg(test)]
mod body_tests {
    use super::*;

    fn write(dir: &Path, name: &str, lines: &[String]) -> safe_read::Opened {
        let path = dir.join(name);
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        let root = safe_read::Root::new(dir).unwrap();
        safe_read::open_in(&root, &root.join(name)).unwrap()
    }

    fn lines_of(file: &safe_read::Opened, budget: u64) -> (Vec<String>, u64) {
        let mut out = Vec::new();
        let n = scan_back(file, budget, &mut |l| {
            out.push(String::from_utf8(l.to_vec()).unwrap());
            false
        });
        (out, n)
    }

    #[test]
    fn scan_back_visits_every_line_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let long = "x".repeat(700 * 1024);
        let lines: Vec<String> = vec!["a".into(), long.clone(), "b".into(), String::new(), "c".into()];
        let f = write(dir.path(), "t.jsonl", &lines);
        let (seen, n) = lines_of(&f, BODY_SCAN_BUDGET);
        assert_eq!(seen, vec!["c".to_string(), "b".into(), long, "a".into()], "blank lines skipped");
        assert_eq!(n, f.id.size);
    }

    #[test]
    fn scan_back_never_reads_past_its_budget_nor_visits_a_cut_line() {
        let dir = tempfile::tempdir().unwrap();
        let lines: Vec<String> = (0..64).map(|i| format!("{i:04}{}", "y".repeat(16 * 1024))).collect();
        let f = write(dir.path(), "t.jsonl", &lines);
        let budget = 100 * 1024;
        let (seen, n) = lines_of(&f, budget);
        assert!(n <= budget);
        assert!(!seen.is_empty() && seen.len() < 64);
        assert!(seen.iter().all(|l| l.len() == 4 + 16 * 1024), "no partial line is visited");
    }

    #[test]
    fn row_blocks_match_the_structure_parser() {
        let rows = [
            r#"{"type":"user","uuid":"u1","message":{"role":"user","content":"<teammate-message teammate_id=\"team-lead\" summary=\"s1\">one</teammate-message>\n<teammate-message teammate_id=\"beta@t\">{\"type\":\"idle_notification\",\"from\":\"beta\"}</teammate-message>"}}"#,
            r#"{"type":"user","uuid":"u2","origin":{"kind":"peer"},"message":{"role":"user","content":[{"type":"text","text":"plain peer text"},{"type":"text","text":"<teammate-message teammate_id=\"alpha\">two</teammate-message>"}]}}"#,
            r#"{"type":"attachment","uuid":"u3","attachment":{"type":"queued_command","origin":{"kind":"peer"},"prompt":"<agent-message from=\"gamma\">three</agent-message>"}}"#,
            r#"{"type":"user","uuid":"u4","origin":{"kind":"peer"},"message":{"role":"user","content":"[Request interrupted by user]"}}"#,
            r#"{"type":"user","uuid":"u5","isMeta":true,"message":{"role":"user","content":"<teammate-message teammate_id=\"team-lead\">meta</teammate-message>"}}"#,
            r#"{"type":"user","uuid":"u6","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"x"},{"type":"text","text":"<teammate-message teammate_id=\"team-lead\">ignored</teammate-message>"}]}}"#,
        ];
        for row in rows {
            let (facts, _) = crate::render::transcript::row_facts(row.as_bytes());
            let facts = facts.unwrap();
            let v: Value = serde_json::from_str(row).unwrap();
            let blocks = row_blocks(&v);
            let mine: Vec<(Option<String>, MessageKind)> = blocks.iter().map(|b| (b.sender.clone(), b.kind)).collect();
            let theirs: Vec<(Option<String>, MessageKind)> =
                facts.wrapped.iter().map(|w| (w.teammate_id.as_deref().map(|t| native(t).to_string()), w.kind)).collect();
            assert_eq!(mine, theirs, "row {row}");
        }
    }

    #[test]
    fn nearest_is_unique_or_nothing() {
        let f = |t: &str| Found { text: t.into(), summary: None };
        let mut n = Nearest::default();
        n.offer(50, || f("far"));
        n.offer(10, || f("near"));
        n.offer(30, || f("mid"));
        assert_eq!(n.take().map(|f| f.text), Some("near".into()));
        let mut n = Nearest::default();
        n.offer(10, || f("a"));
        n.offer(10, || f("b"));
        assert!(n.take().is_none(), "a tie is ambiguous");
        let mut n = Nearest::default();
        n.offer(10, || f("a"));
        n.offer(10, || f("b"));
        n.offer(5, || f("c"));
        assert_eq!(n.take().map(|f| f.text), Some("c".into()), "a strictly nearer one breaks the tie");
    }
}
