//! The SessionGraph: sessions, projects, locations, accounts, marks.
//!
//! Owner: T02. One authoritative in-memory graph (D2). Every mutation enters
//! through the World lock (§2.5): evidence producers send updates, World
//! resolves their identity, stamps every fact that feeds a duration with the
//! uptime clock, re-derives the affected sessions, recomputes the queue and
//! the team values, and writes the store ops into the outbox.
//!
//! Privacy (§3.1 rule 5, F-03): titles, prompts, message and task text are
//! **[content]** — they live in this memory only and are never persisted or
//! logged; [`MetaRow`] (the `session_meta` port) carries metadata only.
//!
//! Store membership (§5.9): a session is in the store while it is live, ended
//! within the current local calendar day, referenced by an open view, the
//! Split set or a quit record, or a tombstone (10 min). A row that leaves the
//! store (local midnight, a time-zone change, a released reference) is
//! removed; a referenced row is never removed (§3.3.3).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use crate::contract::account::Account;
use crate::contract::activity::{
    Activity, ActivityState, EvidenceSet, GildeFacts, OpenToolUse, ProcessFacts, PtyFacts, ScreenPromptFacts, UserMarks,
};
use crate::contract::attention::AttentionQueue;
use crate::contract::common::{AbsPath, Confidence, EpochMs, Notice, Vendor};
use crate::contract::compat::ClaudeCompat;
use crate::contract::display::TextCap;
use crate::contract::ids::{hash16, AccountId, LocationId, MemberKey, ProcessKey, ProjectId, PtyId, RequestId, SessionUid, TeamKey};
use crate::contract::interaction::{InteractionKind, InteractionOption, OptionRole, PendingInteraction};
use crate::contract::process::ProcessRecord;
use crate::contract::project::{Location, LocationKind, Project, ProjectKind};
use crate::contract::services::{
    EvidenceUpdate, IdentityKey, LifecycleOp, LocationFacts, SessionMetaFacts, SourceFacts, UsageSource,
};
use crate::contract::session::{
    Control, ControlMode, EndedHow, MemberRole, Origin, Session, SessionDetails, TeamRole, Transport, UsageToday,
};
use crate::contract::store::{
    BootStatus, Entity, EntityKind, IntegrationsState, OrchestratorPresence, SourceKind, SourceState, SourceStatus,
    StoreOp, StorePatch, StoreSnapshot,
};
use crate::contract::team::{TeamCapabilities, TeamEvent, TeamSource};
use crate::render::sanitize::{sanitize_display_text, sanitize_label_text};
use crate::render::tools::{claude_tool_summary, display_path, PathBase};
use crate::world::attention::{self, LaterMap, LeftoverSummary, QueueSession};
use crate::world::derive::{
    self, band, caps, derive, min_opt, primary_action, ready_signal, CapsInput, ControlKind, DelegatedWait, DeriveCtx,
    Derived, Src, Stamps, TeamPosition,
};
use crate::world::episodes::{up_of, EpisodeTracker};
use crate::world::identity::{IdentityIndex, Resolved, TEAMMATE_HOLD_MS};
use crate::world::patch::{Outbox, Publisher};
use crate::world::teams::assembly::{RoleInfo, SessionView, TeamDetails, TeamsState};

// ---------------------------------------------------------------------------
// ports: the clock and the session_meta store
// ---------------------------------------------------------------------------

/// The clocks World reads (§3.5.2): wall for display and source timestamps,
/// uptime (`CLOCK_UPTIME_RAW`, stops during sleep) for every threshold.
pub trait WorldClock: Send + Sync {
    fn wall_ms(&self) -> EpochMs;
    fn up_ms(&self) -> u64;
    /// The local calendar day `YYYY-MM-DD` of an instant.
    fn local_day(&self, wall: EpochMs) -> String;
    /// The next local midnight after `wall`.
    fn next_local_midnight(&self, wall: EpochMs) -> EpochMs;
    /// The time zone changed (`NSSystemTimeZoneDidChange`): re-read it.
    fn zone_changed(&self) {}
}

extern "C" {
    fn tzset();
}

/// The system clocks (`CLOCK_UPTIME_RAW`, `localtime_r`/`mktime`).
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl WorldClock for SystemClock {
    fn wall_ms(&self) -> EpochMs {
        crate::discovery::now_wall_ms()
    }
    fn up_ms(&self) -> u64 {
        crate::discovery::now_up_ms()
    }
    fn local_day(&self, wall: EpochMs) -> String {
        crate::follow::tail::local_day(wall)
    }
    fn next_local_midnight(&self, wall: EpochMs) -> EpochMs {
        let secs = wall.div_euclid(1000) as libc::time_t;
        // SAFETY: `localtime_r`/`mktime` only read and write the `tm` we pass.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        if unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
            return (wall.div_euclid(86_400_000) + 1) * 86_400_000;
        }
        tm.tm_hour = 0;
        tm.tm_min = 0;
        tm.tm_sec = 0;
        tm.tm_mday += 1;
        tm.tm_isdst = -1;
        let t = unsafe { libc::mktime(&mut tm) };
        if t <= 0 {
            return (wall.div_euclid(86_400_000) + 1) * 86_400_000;
        }
        (t as EpochMs) * 1000
    }
    fn zone_changed(&self) {
        // SAFETY: `tzset` re-reads the time zone; no arguments.
        unsafe { tzset() }
    }
}

/// A settable clock for tests and the showcase (local time = UTC + offset).
#[cfg(any(test, feature = "test-fixtures", feature = "showcase"))]
#[derive(Debug)]
pub struct ManualClock {
    wall: std::sync::atomic::AtomicI64,
    up: std::sync::atomic::AtomicU64,
    offset_ms: std::sync::atomic::AtomicI64,
}

#[cfg(any(test, feature = "test-fixtures", feature = "showcase"))]
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

#[cfg(any(test, feature = "test-fixtures", feature = "showcase"))]
impl ManualClock {
    pub fn new(wall: EpochMs, up: u64) -> ManualClock {
        ManualClock { wall: AtomicI64::new(wall), up: AtomicU64::new(up), offset_ms: AtomicI64::new(0) }
    }
    /// Advance both clocks (awake time).
    pub fn advance(&self, ms: u64) {
        self.wall.fetch_add(ms as i64, Ordering::SeqCst);
        self.up.fetch_add(ms, Ordering::SeqCst);
    }
    /// A sleep: the wall clock moves, the uptime clock does not.
    pub fn sleep(&self, ms: u64) {
        self.wall.fetch_add(ms as i64, Ordering::SeqCst);
    }
    pub fn set_wall(&self, wall: EpochMs) {
        self.wall.store(wall, Ordering::SeqCst);
    }
    pub fn set_offset_ms(&self, offset: i64) {
        self.offset_ms.store(offset, Ordering::SeqCst);
    }
}

#[cfg(any(test, feature = "test-fixtures", feature = "showcase"))]
impl WorldClock for ManualClock {
    fn wall_ms(&self) -> EpochMs {
        self.wall.load(Ordering::SeqCst)
    }
    fn up_ms(&self) -> u64 {
        self.up.load(Ordering::SeqCst)
    }
    fn local_day(&self, wall: EpochMs) -> String {
        crate::follow::tail::utc_day(wall + self.offset_ms.load(Ordering::SeqCst))
    }
    fn next_local_midnight(&self, wall: EpochMs) -> EpochMs {
        let off = self.offset_ms.load(Ordering::SeqCst);
        ((wall + off).div_euclid(86_400_000) + 1) * 86_400_000 - off
    }
}

/// One `session_meta` row (§3.9) — **metadata only**: no title, prompt or
/// any other content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaRow {
    pub uid: SessionUid,
    pub vendor: Option<Vendor>,
    pub account_id: Option<AccountId>,
    pub vendor_session_id: Option<String>,
    pub project_id: Option<ProjectId>,
    pub location_path: Option<String>,
    pub cwd: Option<String>,
    pub transcript_path: Option<String>,
    pub origin_last: Option<Origin>,
    pub control_mode_last: Option<ControlMode>,
    pub permission_mode_last: Option<crate::contract::common::PermissionMode>,
    pub model_last: Option<String>,
    pub cli_path_last: Option<String>,
    pub cli_version_last: Option<String>,
    pub team_key: Option<TeamKey>,
    pub agent_id: Option<String>,
    pub teammate_name: Option<String>,
    pub parent_session_id: Option<String>,
    /// A24.2: the session this one was copied from (Fork, Continue on
    /// another account; §3.3.3) — `session_meta.parent_uid`.
    pub parent_uid: Option<SessionUid>,
    pub first_seen_at: EpochMs,
    pub started_at: Option<EpochMs>,
    pub ended_at: Option<EpochMs>,
    pub last_activity_at: Option<EpochMs>,
    pub last_seen_at: Option<EpochMs>,
    pub usage: Option<UsageToday>,
}

impl MetaRow {
    /// A row with only its identity filled in.
    pub fn new(uid: SessionUid, first_seen_at: EpochMs) -> MetaRow {
        MetaRow {
            uid,
            vendor: None,
            account_id: None,
            vendor_session_id: None,
            project_id: None,
            location_path: None,
            cwd: None,
            transcript_path: None,
            origin_last: None,
            control_mode_last: None,
            permission_mode_last: None,
            model_last: None,
            cli_path_last: None,
            cli_version_last: None,
            team_key: None,
            agent_id: None,
            teammate_name: None,
            parent_session_id: None,
            parent_uid: None,
            first_seen_at,
            started_at: None,
            ended_at: None,
            last_activity_at: None,
            last_seen_at: None,
            usage: None,
        }
    }
}

/// The `session_meta` table as World uses it (implemented by the store over
/// `state.db` in T09b; [`MemoryMetaStore`] in memory-only mode). Writes are
/// debounced by the implementer and must not block.
pub trait MetaStore: Send + Sync {
    fn load(&self) -> Vec<MetaRow>;
    fn upsert(&self, row: &MetaRow);
    fn remove(&self, uid: &SessionUid);
}

/// Memory-only `session_meta` (memory-only mode, §2.4, and tests).
#[derive(Debug, Default)]
pub struct MemoryMetaStore {
    rows: parking_lot::Mutex<BTreeMap<String, MetaRow>>,
}

impl MemoryMetaStore {
    pub fn rows(&self) -> Vec<MetaRow> {
        self.rows.lock().values().cloned().collect()
    }
}

impl MetaStore for MemoryMetaStore {
    fn load(&self) -> Vec<MetaRow> {
        self.rows()
    }
    fn upsert(&self, row: &MetaRow) {
        self.rows.lock().insert(row.uid.as_str().to_string(), row.clone());
    }
    fn remove(&self, uid: &SessionUid) {
        self.rows.lock().remove(uid.as_str());
    }
}

// ---------------------------------------------------------------------------
// configuration and side inputs
// ---------------------------------------------------------------------------

/// The settings World reads (§3.5.2, §9.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorldSettings {
    /// Default 300, range 60..=3600.
    pub no_progress_after_secs: u32,
    /// "Finished turns from sessions started elsewhere" (band 5 for Watching).
    pub watched_band5_allowed: bool,
}

impl Default for WorldSettings {
    fn default() -> WorldSettings {
        WorldSettings { no_progress_after_secs: derive::NO_PROGRESS_DEFAULT_SECS, watched_band5_allowed: false }
    }
}

/// Facts about a Gilde-owned session that only its session actor knows and
/// that `GildeFacts` does not carry (contract request: fold into
/// `GildeFacts`). Absent: the transport follows the mode, no PTY id.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ControlDetail {
    pub pty: Option<PtyId>,
    pub transport: Option<Transport>,
    pub hosted_team: bool,
}

/// Driver-only Details facts (A19.1): each `set_details_extra` call fully
/// replaces the previous one. Never persisted.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DetailsExtra {
    /// §6.1.11: the tool that ran without a seen permission request.
    pub approval_channel_unconfirmed: Option<String>,
    /// Spawn-time vs `system/init` version note [A1].
    pub version_note: Option<String>,
    /// Session lines (`convo.notReading`, `convo.unanswerable`).
    pub lines: Vec<crate::contract::activity::EvidenceLine>,
}

/// A20.1: how long a bound spawn waits for its Gilde facts before the
/// session's row is published as it is.
pub const AWAIT_SPAWN_MS: u64 = 10_000;

/// At most this many driver lines are kept per session.
pub const DETAILS_EXTRA_LINES: usize = 8;

/// A24.2: how far a parent chain is followed when checking for a loop.
pub const PARENT_CHAIN_MAX: usize = 64;

/// Who holds a reference that keeps a session in the store (§5.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RefSource {
    /// `app_view_state.visibleUids` ∪ focused ∪ zoomed.
    View,
    /// The persisted Split set (`ui.splitSet`).
    Split,
    /// Unresolved quit / crash records.
    QuitRecord,
}

/// P19 (§1.3): pane teammates' prompts and questions render in the lead.
pub const FLAG_LEAD_PROMPTS_FOR_TEAMMATES: &str = "leadPromptsForTeammates";
/// P22 (§1.3): a pane teammate's AskUserQuestion renders in the lead.
pub const FLAG_TEAMMATE_QUESTION_LOCATION: &str = "teammateQuestionLocation";

/// The prompt attribution patterns (§7.2.6, `agent-signatures.json`
/// `teammatePromptHeader`); data-switched, the frozen 2.1.307 shape is the
/// default.
#[derive(Debug, Clone)]
pub struct TeamPatterns {
    pub prompt_header: regex::Regex,
}

impl Default for TeamPatterns {
    fn default() -> TeamPatterns {
        TeamPatterns { prompt_header: regex::Regex::new(r"from the (\S+) agent").expect("static regex") }
    }
}

// ---------------------------------------------------------------------------
// one session
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Now {
    pub wall: EpochMs,
    pub up: u64,
}

/// One session's facts and derived state.
#[derive(Debug, Clone)]
pub(crate) struct SessionRec {
    pub uid: SessionUid,
    pub vendor: Vendor,
    pub account_id: Option<AccountId>,
    pub vendor_session_id: Option<String>,
    pub first_seen_wall: EpochMs,
    pub gilde: Option<GildeFacts>,
    pub process: Option<ProcessFacts>,
    pub stream: Option<crate::contract::activity::StreamFacts>,
    pub registry: Option<crate::contract::activity::RegistryFacts>,
    pub transcript: Option<crate::contract::activity::TranscriptFacts>,
    pub team_facts: Option<crate::contract::activity::TeamFileFacts>,
    pub pty: Option<PtyFacts>,
    /// A prompt that just left the screen, kept 300 ms (§3.5.4 debounce).
    pub prompt_hold: Option<(ScreenPromptFacts, u64)>,
    pub meta: SessionMetaFacts,
    pub usage: BTreeMap<UsageSourceKey, UsageToday>,
    pub source_denied: bool,
    pub marks: UserMarks,
    pub stamps: Stamps,
    pub control_detail: Option<ControlDetail>,
    /// **[content]** messages a driver holds (A18.3): memory only.
    pub queued: Vec<crate::contract::session::QueuedMessage>,
    /// Driver-only Details facts (A19.1): memory only.
    pub details_extra: DetailsExtra,
    /// A35.1: the Conversation driver's own report of the permission mode
    /// (`Some(None)` = a mode Gilde does not know, A33.3); it overrides the
    /// metadata while Gilde runs the session in Conversation mode.
    pub mode_reported: Option<Option<crate::contract::common::PermissionMode>>,
    /// A21.1: lines a hosted lead's pane table gives it (an unknown pane
    /// whose claude install differs from the lead's).
    pub pane_lines: Vec<crate::contract::activity::EvidenceLine>,
    /// A20.1: created through a spawn id not bound yet (hidden until bound).
    pub spawn_unbound: bool,
    /// A20.1: bound to a new spawn whose Gilde facts have not arrived yet:
    /// the store row keeps its last published value until then (uptime).
    pub awaiting_spawn_up: Option<u64>,
    /// Resolved through an unbound Gilde spawn id: never shown until Gilde
    /// facts or a vendor key arrive (PM-22, T-world-ownDedupe).
    pub own_unbound: bool,
    /// A24.2: the session this one was copied from (persisted with the
    /// session's `session_meta` row once it has a vendor id).
    pub parent_uid: Option<SessionUid>,
    // derived
    pub derived: Option<Derived>,
    pub episode: EpisodeTracker,
    pub activity: Option<Activity>,
    pub since_up: u64,
    pub control: Option<Control>,
    pub control_kind: Option<ControlKind>,
    pub pending: Vec<PendingInteraction>,
    pub recheck_up: Option<u64>,
    /// An S5 state change waiting out its 300 ms (target class, deadline).
    pub debounce: Option<(String, u64)>,
    pub merged_into: Option<SessionUid>,
    pub published: Option<Session>,
    pub persisted: Option<MetaRow>,
}

/// `UsageSource` ordered by priority (Stream > Turns > Transcript).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct UsageSourceKey(u8);

impl UsageSourceKey {
    fn of(s: UsageSource) -> UsageSourceKey {
        UsageSourceKey(match s {
            UsageSource::Stream => 0,
            UsageSource::Turns => 1,
            UsageSource::Transcript => 2,
        })
    }
}

impl SessionRec {
    fn new(uid: SessionUid, vendor: Vendor, now: &Now) -> SessionRec {
        SessionRec {
            uid,
            vendor,
            account_id: None,
            vendor_session_id: None,
            first_seen_wall: now.wall,
            gilde: None,
            process: None,
            stream: None,
            registry: None,
            transcript: None,
            team_facts: None,
            pty: None,
            prompt_hold: None,
            meta: SessionMetaFacts::default(),
            usage: BTreeMap::new(),
            source_denied: false,
            marks: UserMarks::default(),
            stamps: Stamps::default(),
            control_detail: None,
            queued: Vec::new(),
            details_extra: DetailsExtra::default(),
            mode_reported: None,
            awaiting_spawn_up: None,
            pane_lines: Vec::new(),
            spawn_unbound: false,
            own_unbound: false,
            parent_uid: None,
            derived: None,
            episode: EpisodeTracker::default(),
            activity: None,
            since_up: now.up,
            control: None,
            control_kind: None,
            pending: Vec::new(),
            recheck_up: None,
            debounce: None,
            merged_into: None,
            published: None,
            persisted: None,
        }
    }

    fn alive(&self) -> bool {
        matches!(self.process, Some(ProcessFacts::Alive { .. }))
    }

    /// A Gilde spawn whose process has not reported yet counts as alive —
    /// unless Gilde already stopped it, or it is turn-based (its turns'
    /// processes are not the session's, §6.3).
    fn pending_spawn(&self) -> bool {
        self.process.is_none()
            && self.gilde.as_ref().is_some_and(|g| g.stop_cause.is_none() && g.mode != ControlMode::TurnBased)
    }

    fn ended_at(&self) -> Option<EpochMs> {
        match self.process {
            Some(ProcessFacts::Exited { at, .. }) | Some(ProcessFacts::Gone { at }) => Some(at),
            Some(ProcessFacts::HistoryOnly) => self.last_activity_at(),
            _ => None,
        }
    }

    fn last_activity_at(&self) -> Option<EpochMs> {
        [
            self.stream.as_ref().map(|s| s.last_frame_at),
            self.transcript.as_ref().and_then(|t| t.last_row_at),
            self.pty.as_ref().and_then(|p| p.last_output_at),
        ]
        .into_iter()
        .flatten()
        .max()
    }

    fn started_at(&self) -> Option<EpochMs> {
        self.meta.started_at.or(self.gilde.as_ref().map(|g| g.spawned_at)).or(match self.process {
            Some(ProcessFacts::Alive { start_sec, .. }) if start_sec > 0 => Some(start_sec * 1000),
            _ => None,
        })
    }

    /// The claude version that selects the session's compat record (§3.10).
    fn version(&self) -> Option<String> {
        self.meta.claude_version.clone().or_else(|| self.meta.binary.as_ref().and_then(|b| b.version.clone()))
    }

    fn usage_today(&self, today: &str) -> Option<UsageToday> {
        self.usage.values().find(|u| u.day == today).cloned()
    }
}

/// A teammate update held for its lead (§3.3.2 rule 3).
#[derive(Debug, Clone)]
struct Held {
    update: EvidenceUpdate,
    deadline_up: u64,
}

#[derive(Debug, Clone, PartialEq)]
struct ProjectRec {
    id: ProjectId,
    root: AbsPath,
    git: bool,
    seq: u64,
    /// A24.4: when the user last picked it (`register_project`); counts as
    /// activity for `earlier`.
    registered_at: Option<EpochMs>,
    published: Option<Project>,
}

/// What a derivation pass produced for one session (computed read-only,
/// stored afterwards).
struct Computed {
    derived: Derived,
    control: Control,
    control_kind: ControlKind,
    pending: Vec<PendingInteraction>,
}

// ---------------------------------------------------------------------------
// the graph
// ---------------------------------------------------------------------------

/// World's state behind the World lock.
pub(crate) struct Graph {
    pub clock: Arc<dyn WorldClock>,
    pub meta_store: Arc<dyn MetaStore>,
    pub settings: WorldSettings,
    pub patterns: TeamPatterns,
    pub home: Option<String>,
    pub ids: IdentityIndex,
    pub sessions: HashMap<SessionUid, SessionRec>,
    projects: HashMap<ProjectId, ProjectRec>,
    project_seq: u64,
    project_order: Vec<ProjectId>,
    project_folds: HashSet<ProjectId>,
    locations: HashMap<LocationId, (Location, bool)>,
    accounts: HashMap<AccountId, (Account, Option<Account>)>,
    leftovers: BTreeMap<String, ProcessRecord>,
    leftover_seen: HashMap<String, u64>,
    pub compat: Vec<ClaudeCompat>,
    /// The published list: T09's lifecycle notices, then the ones World
    /// derives from the `Sources` state (A33.4).
    notices: Vec<Notice>,
    /// The lifecycle feed's current list (`LifecycleOp::Notices`).
    lifecycle_notices: Vec<Notice>,
    boot: BootStatus,
    sources: BTreeMap<(u8, u8), SourceStatus>,
    integrations: IntegrationsState,
    pub teams: TeamsState,
    roles: HashMap<SessionUid, RoleInfo>,
    roles_dirty: bool,
    /// A24.8: an account changed; published on the next refresh, sessions or
    /// not.
    accounts_dirty: bool,
    pub queue: AttentionQueue,
    queue_rev: u64,
    pub later: LaterMap,
    paused_until: Option<EpochMs>,
    pub outbox: Outbox,
    pub publisher: Publisher,
    held: Vec<Held>,
    refs: HashMap<RefSource, HashSet<SessionUid>>,
    dirty: HashSet<SessionUid>,
    queue_dirty: bool,
    today: String,
    midnight_up: u64,
    team_recheck_wall: Option<EpochMs>,
    /// Persisted rows not (yet) in the graph: uid → row.
    pub dormant: HashMap<SessionUid, MetaRow>,
}

impl Graph {
    pub fn new(clock: Arc<dyn WorldClock>, meta_store: Arc<dyn MetaStore>, settings: WorldSettings, home: Option<String>) -> Graph {
        let now_wall = clock.wall_ms();
        let now_up = clock.up_ms();
        let today = clock.local_day(now_wall);
        let midnight = clock.next_local_midnight(now_wall);
        let mut g = Graph {
            clock,
            meta_store,
            settings,
            patterns: TeamPatterns::default(),
            home,
            ids: IdentityIndex::default(),
            sessions: HashMap::new(),
            projects: HashMap::new(),
            project_seq: 0,
            project_order: Vec::new(),
            project_folds: HashSet::new(),
            locations: HashMap::new(),
            accounts: HashMap::new(),
            leftovers: BTreeMap::new(),
            leftover_seen: HashMap::new(),
            compat: Vec::new(),
            notices: Vec::new(),
            lifecycle_notices: Vec::new(),
            boot: BootStatus { spawn_gate_open: false, first_discovery_done: false, agents_detected: false, memory_only: None, agents: Vec::new(), agents_scanned_at: None, recovery: None },
            sources: BTreeMap::new(),
            integrations: IntegrationsState { orchestrator: OrchestratorPresence::Absent },
            teams: TeamsState::default(),
            roles: HashMap::new(),
            roles_dirty: false,
            accounts_dirty: false,
            queue: AttentionQueue { rev: 0, items: Vec::new(), need_you: 0, also: 0, paused_until: None },
            queue_rev: 0,
            later: LaterMap::default(),
            paused_until: None,
            outbox: Outbox::default(),
            publisher: Publisher::default(),
            held: Vec::new(),
            refs: HashMap::new(),
            dirty: HashSet::new(),
            queue_dirty: false,
            today,
            midnight_up: now_up + u64::try_from((midnight - now_wall).max(0)).unwrap_or(0),
            team_recheck_wall: None,
            dormant: HashMap::new(),
        };
        for row in g.meta_store.load() {
            let key = match (&row.vendor, &row.vendor_session_id) {
                (Some(v), Some(id)) => Some(IdentityKey { vendor: *v, account_id: row.account_id.clone(), vendor_session_id: id.clone() }),
                _ => None,
            };
            g.ids.load(row.uid.clone(), key, row.first_seen_at);
            g.dormant.insert(row.uid.clone(), row);
        }
        g
    }

    pub fn now(&self) -> Now {
        Now { wall: self.clock.wall_ms(), up: self.clock.up_ms() }
    }

    // -- identity -------------------------------------------------------------

    fn lead_account_for(&self, u: &EvidenceUpdate) -> Option<AccountId> {
        let vendor = u.hint.vendor.unwrap_or(Vendor::Claude);
        let parent = match &u.facts {
            SourceFacts::Meta(m) => m.teammate.as_ref().and_then(|t| t.parent_session_id.clone()),
            _ => None,
        };
        parent.and_then(|p| self.ids.account_of_vsid(vendor, &p))
    }

    /// `UidResolver::uid_for`: the uid of a key, minted and persisted on first
    /// sight (history rows, `--session-id` starts).
    pub fn uid_for_key(&mut self, key: &IdentityKey) -> SessionUid {
        let (uid, minted) = self.ids.uid_for_key(key);
        if minted || !self.ids.is_persisted(&uid) {
            let now = self.now();
            let mut row = MetaRow::new(uid.clone(), now.wall);
            row.vendor = Some(key.vendor);
            row.account_id = key.account_id.clone();
            row.vendor_session_id = Some(key.vendor_session_id.clone());
            self.ids.mark_persisted(&uid, now.wall);
            self.meta_store.upsert(&row);
            self.dormant.insert(uid.clone(), row);
        }
        uid
    }

    pub fn key_of(&self, uid: &SessionUid) -> Option<IdentityKey> {
        self.ids.key_of(&self.ids.live(uid)).cloned()
    }

    /// Bind a Gilde spawn id before the spawn (the identity rule 1 input).
    /// A20.1: bind a Gilde spawn id to a session right after the spawn.
    /// `Some(uid)` always returns `uid` (Resume keeps its uid): an own-unbound
    /// row the spawn's evidence already created is merged into it — evidence
    /// kept, the hidden row removed, no store op for it. `None` adopts that
    /// row's uid if there is one, else mints. Until the spawn's Gilde facts
    /// arrive (≤ 10 s) the session's store row keeps its last published value,
    /// so nothing flashes.
    pub fn bind_spawn(&mut self, spawn_id: &str, uid: Option<&SessionUid>, vendor: Vendor, account: Option<AccountId>) -> SessionUid {
        let now = self.now();
        let uid = match uid {
            Some(u) => {
                let u = self.ids.live(u);
                if let Some(absorbed) = self.ids.bind_spawn_to(spawn_id, &u) {
                    self.absorb_rec(&u, &absorbed, &now);
                }
                u
            }
            None => self.ids.bind_spawn(spawn_id, None),
        };
        self.ensure_rec(&uid, vendor, &now, false);
        let rec = self.sessions.get_mut(&uid).expect("ensured");
        if rec.account_id.is_none() {
            rec.account_id = account;
        }
        rec.spawn_unbound = false;
        // Hold the row until this spawn's Gilde facts arrive, unless they
        // already did (the session runs on facts of this spawn).
        let finished = rec.gilde.as_ref().is_none_or(|g| g.stop_cause.is_some())
            || matches!(rec.process, Some(ProcessFacts::Exited { .. }) | Some(ProcessFacts::Gone { .. }) | Some(ProcessFacts::HistoryOnly));
        if finished {
            rec.awaiting_spawn_up = Some(now.up + AWAIT_SPAWN_MS);
            rec.recheck_up = min_opt(rec.recheck_up, Some(now.up + AWAIT_SPAWN_MS + 1));
        }
        self.dirty.insert(uid.clone());
        uid
    }

    /// Merge an own-unbound row into the session its spawn was bound to
    /// (A20.1). The absorbed row's facts are the new spawn's, so its process,
    /// registry, PTY and stream facts win over the survivor's older ones.
    fn absorb_rec(&mut self, survivor: &SessionUid, gone: &SessionUid, now: &Now) {
        let Some(g) = self.sessions.remove(gone) else {
            self.dormant.remove(gone);
            return;
        };
        self.ensure_rec(survivor, g.vendor, now, false);
        let s = self.sessions.get_mut(survivor).expect("ensured");
        if g.process.is_some() {
            s.process = g.process;
        }
        if g.registry.is_some() {
            s.registry = g.registry.clone();
            s.stamps.status_up = g.stamps.status_up.or(s.stamps.status_up);
        }
        if g.pty.is_some() {
            s.pty = g.pty.clone();
            s.stamps.output_up = g.stamps.output_up.or(s.stamps.output_up);
        }
        if g.stream.is_some() {
            s.stream = g.stream.clone();
            s.stamps.frame_up = g.stamps.frame_up.or(s.stamps.frame_up);
        }
        if g.transcript.is_some() {
            s.transcript = g.transcript.clone();
            s.stamps.row_up = g.stamps.row_up.or(s.stamps.row_up);
        }
        if g.gilde.is_some() {
            s.gilde = g.gilde.clone();
            s.stamps.spawn_up = g.stamps.spawn_up;
        }
        s.stamps.cpu_up = s.stamps.cpu_up.max(g.stamps.cpu_up);
        s.team_facts = g.team_facts.clone().or(s.team_facts.take());
        s.control_detail = s.control_detail.take().or(g.control_detail.clone());
        s.account_id = s.account_id.take().or(g.account_id.clone());
        s.parent_uid = s.parent_uid.take().or(g.parent_uid.clone()).filter(|p| p != survivor);
        merge_meta(&mut s.meta, g.meta.clone());
        for (k, v) in &g.usage {
            s.usage.entry(*k).or_insert_with(|| v.clone());
        }
        s.own_unbound = s.own_unbound || g.own_unbound;
        s.spawn_unbound = false;
        if g.published.is_some() {
            // It was shown after all (not the A20.1 order): a tombstone the
            // WebView remaps.
            let mut tomb = g;
            tomb.merged_into = Some(survivor.clone());
            tomb.pending.clear();
            self.sessions.insert(gone.clone(), tomb);
            self.ids.add_tombstone(gone, survivor, now.up);
            self.dirty.insert(gone.clone());
        } else if g.persisted.is_some() {
            self.meta_store.remove(gone);
        }
        self.teams.remap_session(gone, survivor);
        self.roles_dirty = true;
        self.dirty.insert(survivor.clone());
    }

    // -- evidence -------------------------------------------------------------

    pub fn apply_evidence(&mut self, u: EvidenceUpdate) {
        let lead_account = self.lead_account_for(&u);
        match self.ids.resolve(&u.hint, lead_account, false) {
            Resolved::Hold => {
                let now = self.now();
                // Several updates of one teammate keep their order.
                self.held.push(Held { update: u, deadline_up: now.up + TEAMMATE_HOLD_MS });
            }
            Resolved::Uid { uid, rule, account, .. } => self.ingest(uid, rule, account, u),
        }
    }

    /// Retry held teammate updates; file expired ones under account `None`.
    fn retry_held(&mut self, now: &Now) {
        if self.held.is_empty() {
            return;
        }
        let held = std::mem::take(&mut self.held);
        for h in held {
            let lead_account = self.lead_account_for(&h.update);
            let expired = now.up >= h.deadline_up;
            match self.ids.resolve(&h.update.hint, lead_account, expired) {
                Resolved::Hold => self.held.push(h),
                Resolved::Uid { uid, rule, account, .. } => self.ingest(uid, rule, account, h.update),
            }
        }
    }

    fn ingest(&mut self, uid: SessionUid, rule: u8, account: Option<AccountId>, u: EvidenceUpdate) {
        let now = self.now();
        let vendor = u.hint.vendor.unwrap_or(Vendor::Claude);
        let out = self.ids.bind(&uid, &u.hint, account.clone(), now.up);
        let uid = out.uid.clone().unwrap_or(uid);
        if let Some((survivor, gone)) = &out.merged {
            self.merge_recs(survivor, gone, &now);
        }
        if let Some((old, new)) = &out.rekeyed {
            self.rekey_recs(old, new, &now);
        }
        let discovered = !matches!(u.facts, SourceFacts::Gilde(_)) && u.hint.gilde_spawn_id.is_none();
        self.ensure_rec(&uid, vendor, &now, discovered);
        let rec = self.sessions.get_mut(&uid).expect("ensured");
        if rec.account_id.is_none() {
            rec.account_id = account.clone();
        }
        if let Some(vsid) = &u.hint.vendor_session_id {
            if self.ids.key_of(&uid).is_some_and(|k| &k.vendor_session_id == vsid) {
                rec.vendor_session_id = Some(vsid.clone());
            }
        }
        // Gilde's own process before its Gilde facts: hidden (PM-22), bound
        // or not.
        if u.hint.gilde_spawn_id.is_some() && rec.gilde.is_none() {
            rec.own_unbound = true;
        }
        // A row that exists only through a spawn id T10 has not bound yet is
        // hidden until `bind_spawn` adopts or absorbs it (A20.1).
        if rule == 5 && u.hint.gilde_spawn_id.is_some() {
            rec.spawn_unbound = true;
        }
        if matches!(u.facts, SourceFacts::Gilde(_)) {
            rec.own_unbound = false;
        }
        apply_facts(rec, u.facts, u.hint.process.as_ref(), &now);
        if out.newly_keyed {
            self.ids.mark_persisted(&uid, now.wall);
        }
        self.dirty.insert(uid);
    }

    /// The record of a uid, created on first evidence (from its persisted
    /// `session_meta` row when there is one).
    fn ensure_rec(&mut self, uid: &SessionUid, vendor: Vendor, now: &Now, discovered: bool) {
        if self.sessions.contains_key(uid) {
            return;
        }
        let mut r = SessionRec::new(uid.clone(), vendor, now);
        match self.dormant.remove(uid) {
            Some(row) => {
                // Never seen in an earlier run: what happened before this
                // launch is not a new reply (§8.5 rule 5).
                r.marks.last_seen_at = row.last_seen_at.or(Some(now.wall));
                r.first_seen_wall = row.first_seen_at;
                r.vendor_session_id = row.vendor_session_id.clone();
                r.account_id = row.account_id.clone();
                r.parent_uid = row.parent_uid.clone();
                if let Some(v) = row.vendor {
                    r.vendor = v;
                }
                r.persisted = Some(row);
            }
            // A session discovered elsewhere: a turn that ended before Gilde
            // first saw it is not a new reply for this user.
            None if discovered => r.marks.last_seen_at = Some(now.wall),
            None => {}
        }
        self.sessions.insert(uid.clone(), r);
    }

    fn merge_recs(&mut self, survivor: &SessionUid, gone: &SessionUid, now: &Now) {
        let Some(g) = self.sessions.remove(gone) else {
            self.dormant.remove(gone);
            return;
        };
        self.ensure_rec(survivor, g.vendor, now, false);
        let s = self.sessions.get_mut(survivor).expect("ensured");
        if s.gilde.is_none() {
            s.gilde = g.gilde.clone();
            s.stamps.spawn_up = s.stamps.spawn_up.or(g.stamps.spawn_up);
        }
        if s.process.is_none() || (!s.alive() && g.alive()) {
            s.process = g.process;
        }
        s.stream = s.stream.take().or(g.stream.clone());
        s.registry = s.registry.take().or(g.registry.clone());
        s.transcript = s.transcript.take().or(g.transcript.clone());
        s.team_facts = s.team_facts.take().or(g.team_facts.clone());
        s.pty = s.pty.take().or(g.pty.clone());
        s.control_detail = s.control_detail.take().or(g.control_detail.clone());
        s.account_id = s.account_id.take().or(g.account_id.clone());
        s.parent_uid = s.parent_uid.take().or(g.parent_uid.clone()).filter(|p| p != survivor);
        merge_meta(&mut s.meta, g.meta.clone());
        for (k, v) in &g.usage {
            s.usage.entry(*k).or_insert_with(|| v.clone());
        }
        s.marks.last_seen_at = s.marks.last_seen_at.max(g.marks.last_seen_at);
        s.own_unbound = s.own_unbound && g.own_unbound;
        // The tombstone stays visible (mergedInto) for 10 min.
        let mut tomb = g;
        tomb.merged_into = Some(survivor.clone());
        tomb.pending.clear();
        self.sessions.insert(gone.clone(), tomb);
        self.teams.remap_session(gone, survivor);
        self.roles_dirty = true;
        self.dirty.insert(survivor.clone());
        self.dirty.insert(gone.clone());
    }

    fn rekey_recs(&mut self, old: &SessionUid, new: &SessionUid, now: &Now) {
        let Some(o) = self.sessions.get_mut(old) else { return };
        let process = o.process.take();
        let registry = o.registry.take();
        let pty = o.pty.take();
        let stream = o.stream.take();
        let vendor = o.vendor;
        let account = o.account_id.clone();
        // The old session's process now serves another session id.
        o.process = Some(ProcessFacts::Gone { at: now.wall });
        self.ensure_rec(new, vendor, now, true);
        let n = self.sessions.get_mut(new).expect("ensured");
        n.process = process;
        n.registry = registry;
        n.pty = pty;
        n.stream = stream;
        n.account_id = n.account_id.take().or(account);
        self.dirty.insert(old.clone());
        self.dirty.insert(new.clone());
    }

    /// A18.3: the full current list of a session's queued messages, oldest
    /// first (an empty list clears it). **[content]**: kept in this memory
    /// only — never persisted, never logged. Distinct `client_msg_id`s; each
    /// text capped at 256 KiB (a char boundary).
    pub fn set_queued(&mut self, uid: &SessionUid, list: Vec<crate::contract::session::QueuedMessage>) {
        const CAP: usize = 256 * 1024;
        let uid = self.ids.live(uid);
        let Some(r) = self.sessions.get_mut(&uid) else { return };
        let mut seen = HashSet::new();
        let mut v: Vec<crate::contract::session::QueuedMessage> = list
            .into_iter()
            .filter(|m| seen.insert(m.client_msg_id.clone()))
            .map(|mut m| {
                if m.text.len() > CAP {
                    let mut cut = CAP;
                    while !m.text.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    m.text.truncate(cut);
                }
                m
            })
            .collect();
        v.sort_by_key(|m| m.queued_at);
        if r.queued != v {
            r.queued = v;
            self.dirty.insert(uid);
        }
    }

    /// A35.1: the permission mode the Conversation driver reports (`None` = a
    /// mode Gilde does not know, A33.3; never the previous value). It
    /// overrides the metadata while Gilde runs the session in Conversation
    /// mode, until the next report.
    pub fn set_permission_mode(&mut self, uid: &SessionUid, mode: Option<crate::contract::common::PermissionMode>) {
        let uid = self.ids.live(uid);
        let Some(r) = self.sessions.get_mut(&uid) else { return };
        if r.mode_reported != Some(mode) {
            r.mode_reported = Some(mode);
            self.dirty.insert(uid);
        }
    }

    /// A19.1: replace a session's driver-only Details facts (`None` / an
    /// empty list clear them). Text is sanitized; nothing is persisted.
    pub fn set_details_extra(&mut self, uid: &SessionUid, extra: DetailsExtra) {
        let uid = self.ids.live(uid);
        let Some(r) = self.sessions.get_mut(&uid) else { return };
        let mut lines = extra.lines;
        lines.truncate(DETAILS_EXTRA_LINES);
        let clean_extra = DetailsExtra {
            approval_channel_unconfirmed: extra.approval_channel_unconfirmed.as_deref().and_then(|s| clean(s, 120)),
            version_note: extra.version_note.as_deref().and_then(|s| clean(s, 200)),
            lines,
        };
        if r.details_extra != clean_extra {
            r.details_extra = clean_extra;
            self.dirty.insert(uid);
        }
    }

    /// Facts whose source is a team adapter (A21.1: a hosted pane member's
    /// Gilde facts and exit from PaneHost's pane table). Unknown uids are
    /// ignored (the pane's session is created by `bind_spawn`).
    pub fn apply_session_facts(&mut self, uid: &SessionUid, facts: SourceFacts) {
        let uid = self.ids.live(uid);
        let now = self.now();
        let Some(r) = self.sessions.get_mut(&uid) else { return };
        if matches!(facts, SourceFacts::Gilde(_)) {
            r.own_unbound = false;
            r.spawn_unbound = false;
        }
        apply_facts(r, facts, None, &now);
        self.dirty.insert(uid);
    }

    /// A21.1: replace a hosted lead's pane lines.
    pub fn set_pane_lines(&mut self, uid: &SessionUid, lines: Vec<crate::contract::activity::EvidenceLine>) {
        let uid = self.ids.live(uid);
        let Some(r) = self.sessions.get_mut(&uid) else { return };
        let mut lines = lines;
        lines.truncate(DETAILS_EXTRA_LINES);
        if r.pane_lines != lines {
            r.pane_lines = lines;
            self.dirty.insert(uid);
        }
    }

    pub fn set_control_detail(&mut self, uid: &SessionUid, detail: ControlDetail) {
        let uid = self.ids.live(uid);
        if let Some(r) = self.sessions.get_mut(&uid) {
            r.control_detail = Some(detail);
            self.dirty.insert(uid);
        }
    }

    // -- A24: parents, open tool calls, picked projects -----------------------

    /// A24.2: `uid` was copied from `parent` (Fork, Continue on another
    /// account; §3.3.3). Kept on the session and persisted as
    /// `session_meta.parent_uid` with its row: now when the session has a
    /// vendor id, else as soon as it gets one (§3.3.2 rule 5). Both uids
    /// follow merges; a later call replaces the parent. Refused (`false`,
    /// nothing changes): a uid World does not know, a session as its own
    /// parent, and a parent whose own chain leads back to `uid`.
    pub fn set_parent(&mut self, uid: &SessionUid, parent: &SessionUid) -> bool {
        let uid = self.ids.live(uid);
        let parent = self.ids.live(parent);
        if uid == parent || self.parent_chain_reaches(&parent, &uid) {
            return false;
        }
        if let Some(r) = self.sessions.get_mut(&uid) {
            if r.parent_uid.as_ref() != Some(&parent) {
                r.parent_uid = Some(parent);
                self.persist(&uid);
            }
            return true;
        }
        if let Some(row) = self.dormant.get_mut(&uid) {
            if row.parent_uid.as_ref() != Some(&parent) {
                row.parent_uid = Some(parent);
                self.meta_store.upsert(row);
            }
            return true;
        }
        false
    }

    /// A24.2: the parent recorded for `uid` (this run or a persisted row),
    /// as its live uid; never the session itself.
    pub fn parent_of(&self, uid: &SessionUid) -> Option<SessionUid> {
        let uid = self.ids.live(uid);
        let parent = match self.sessions.get(&uid) {
            Some(r) => r.parent_uid.clone(),
            None => self.dormant.get(&uid).and_then(|row| row.parent_uid.clone()),
        }?;
        Some(self.ids.live(&parent)).filter(|p| *p != uid)
    }

    /// Whether `target` is `from` or one of its ancestors (bounded walk).
    fn parent_chain_reaches(&self, from: &SessionUid, target: &SessionUid) -> bool {
        let mut cur = Some(from.clone());
        for _ in 0..PARENT_CHAIN_MAX {
            match cur {
                Some(c) if &c == target => return true,
                Some(c) => cur = self.parent_of(&c),
                None => return false,
            }
        }
        false
    }

    /// A24.3 (F-05): the tool call the session's transcript ends in — a
    /// `tool_use` without a result, with no prompt or turn end after it.
    /// `None` without transcript facts, after a later prompt or turn end, or
    /// when the id or name is empty. Meant for an ended session (Continue);
    /// for a running one it is the call in flight.
    pub fn open_tool_use(&self, uid: &SessionUid) -> Option<OpenToolUse> {
        let uid = self.ids.live(uid);
        let t = self.sessions.get(&uid)?.transcript.as_ref()?;
        let o = t.open_tool_use.as_ref()?;
        let after = |at: Option<EpochMs>| at.is_some_and(|x| x > o.at);
        if o.tool_use_id.trim().is_empty() || o.tool_name.trim().is_empty() || after(t.last_turn_end_at) || after(t.last_human_prompt_at) {
            return None;
        }
        Some(o.clone())
    }

    /// A24.4: a folder the user picked is a project at once: its project and
    /// location are published now, with or without a session, and stay
    /// (World never drops a project; v1 has no remove). The pick counts as
    /// activity in the project, so it is not `earlier` for 24 h (M3).
    /// Picking a known project again only refreshes that. Returns the id.
    pub fn register_project(&mut self, facts: LocationFacts) -> ProjectId {
        let id = facts.project_id.clone();
        let now = self.now();
        self.apply_location(facts);
        if let Some(p) = self.projects.get_mut(&id) {
            p.registered_at = Some(now.wall);
        }
        self.publish_projects(&now);
        id
    }

    // -- other sink inputs ----------------------------------------------------

    pub fn apply_location(&mut self, f: LocationFacts) {
        if !self.projects.contains_key(&f.project_id) {
            self.project_seq += 1;
            self.project_order.push(f.project_id.clone());
            self.projects.insert(
                f.project_id.clone(),
                ProjectRec {
                    id: f.project_id.clone(),
                    root: f.project_root.clone(),
                    git: f.git,
                    seq: self.project_seq,
                    registered_at: None,
                    published: None,
                },
            );
        } else if let Some(p) = self.projects.get_mut(&f.project_id) {
            p.root = f.project_root.clone();
            p.git = f.git;
        }
        let kind = match (f.git, f.worktree) {
            (_, true) => LocationKind::Worktree,
            (true, false) => LocationKind::Main,
            (false, false) => LocationKind::Folder,
        };
        // A48.3: labels lose bidi controls and zero-width characters.
        let worktree_name = f
            .worktree
            .then(|| f.location_path.as_str().rsplit('/').next().unwrap_or(""))
            .and_then(|n| label(n, 255));
        let loc = Location {
            id: f.location_id.clone(),
            project_id: f.project_id.clone(),
            path: f.location_path.clone(),
            kind,
            branch: f.branch.as_deref().and_then(|b| label(b, 255)),
            worktree_name,
            // A17.7: the one rule, T11's helper (both worktree roots count).
            gilde_created: crate::worktree::naming::is_gilde_created(
                std::path::Path::new(f.project_root.as_str()),
                std::path::Path::new(f.location_path.as_str()),
                f.branch.as_deref(),
            ),
            exists: true,
        };
        let changed = self.locations.get(&f.location_id).map(|(l, _)| l) != Some(&loc);
        if changed {
            self.outbox.upsert(Entity::Location { value: loc.clone() });
            self.locations.insert(f.location_id, (loc, true));
            // Sessions may map to the new location (longest prefix of cwd).
            let uids: Vec<SessionUid> = self.sessions.keys().cloned().collect();
            self.dirty.extend(uids);
        }
    }

    pub fn apply_source_status(&mut self, s: SourceStatus) {
        let key = (s.vendor as u8, source_kind_ord(s.source));
        if self.sources.get(&key) != Some(&s) {
            self.sources.insert(key, s);
            self.outbox.push(StoreOp::Sources { statuses: self.sources.values().cloned().collect() });
            self.republish_notices();
        }
    }

    /// Publish `Notices` when the lifecycle list or the derived denial
    /// notices changed (A33.4, §5.8: one `notice.cantReadFolders` while any
    /// source is `PermissionDenied`, plus one `health.folderDenied{category}`
    /// per denied folder category; both clear when access comes back).
    fn republish_notices(&mut self) {
        let denied: Vec<SourceKind> = self
            .sources
            .values()
            .filter(|s| s.state == SourceState::PermissionDenied)
            .map(|s| s.source)
            .collect();
        let mut next = self.lifecycle_notices.clone();
        if !denied.is_empty() {
            next.retain(|n| n.key != CANT_READ_NOTICE && n.key != FOLDER_DENIED_LINE);
            next.push(Notice::new(CANT_READ_NOTICE));
            for c in denied_categories(&denied) {
                next.push(Notice::new(FOLDER_DENIED_LINE).param("category", c));
            }
        }
        if self.notices != next {
            self.notices = next;
            self.outbox.push(StoreOp::Notices { notices: self.notices.clone() });
        }
    }

    /// A24.8: an account reaches the store on the next refresh even while no
    /// session exists; an unchanged account changes nothing.
    pub fn apply_account(&mut self, a: Account) {
        let id = a.id.clone();
        if self.accounts.get(&id).is_some_and(|(known, _)| *known == a) {
            return;
        }
        let published = self.accounts.get(&id).and_then(|(_, p)| p.clone());
        self.accounts.insert(id, (a, published));
        self.accounts_dirty = true;
        // Sessions show the account's label and config dir in Details.
        let uids: Vec<SessionUid> = self.sessions.keys().cloned().collect();
        self.dirty.extend(uids);
    }

    /// A10.1: replace the compat list; `Compat{list}` only when it differs.
    pub fn apply_compat(&mut self, list: Vec<ClaudeCompat>) {
        if self.compat != list {
            self.compat = list;
            self.outbox.push(StoreOp::Compat { list: self.compat.clone() });
            // Caps and Details depend on compat.
            let uids: Vec<SessionUid> = self.sessions.keys().cloned().collect();
            self.dirty.extend(uids);
        }
    }

    /// A11.6: `Integrations{state}` only on change.
    pub fn apply_integrations(&mut self, state: IntegrationsState) {
        if self.integrations != state {
            self.integrations = state;
            self.outbox.push(StoreOp::Integrations { state: self.integrations.clone() });
        }
    }

    /// A12.1: lifecycle facts become store ops with normal rev ordering.
    pub fn apply_lifecycle(&mut self, op: LifecycleOp) {
        match op {
            LifecycleOp::Boot(b) => {
                if self.boot != b {
                    self.boot = b;
                    self.outbox.push(StoreOp::Boot { status: self.boot.clone() });
                }
            }
            LifecycleOp::Notices(list) => {
                self.lifecycle_notices = list;
                self.republish_notices();
            }
            LifecycleOp::Quit { stopped, total, phase } => {
                self.outbox.push(StoreOp::Quit { stopped, total, phase });
            }
            LifecycleOp::LeftoverUpsert(r) => {
                let r = *r;
                let key = r.key.as_str().to_string();
                if self.leftovers.get(&key) != Some(&r) {
                    if !self.leftover_seen.contains_key(&key) {
                        let up = self.clock.up_ms();
                        self.leftover_seen.insert(key.clone(), up);
                    }
                    self.outbox.upsert(Entity::Leftover { value: r.clone() });
                    self.leftovers.insert(key, r);
                    self.queue_dirty = true;
                }
            }
            LifecycleOp::LeftoverRemove(k) => {
                if self.leftovers.remove(k.as_str()).is_some() {
                    self.leftover_seen.remove(k.as_str());
                    self.outbox.remove(EntityKind::Leftover, k.as_str());
                    self.queue_dirty = true;
                }
            }
        }
    }

    pub fn apply_team_event(&mut self, source: TeamSource, ev: TeamEvent, caps: Option<TeamCapabilities>) {
        let fx = self.teams.apply(source, ev, caps);
        let now = self.now();
        for (uid, facts) in fx.session_team_facts {
            let uid = self.ids.live(&uid);
            if let Some(r) = self.sessions.get_mut(&uid) {
                apply_facts(r, SourceFacts::Team(facts), None, &now);
                self.dirty.insert(uid);
            }
        }
        for uid in fx.dirty_sessions {
            self.dirty.insert(self.ids.live(&uid));
        }
        self.roles_dirty = true;
    }

    pub fn set_team_details(&mut self, key: &TeamKey, source: TeamSource, details: TeamDetails) {
        self.teams.set_details(key, source, details);
        self.roles_dirty = true;
    }

    pub fn set_settings(&mut self, s: WorldSettings) {
        if self.settings != s {
            self.settings = s;
            let uids: Vec<SessionUid> = self.sessions.keys().cloned().collect();
            self.dirty.extend(uids);
        }
    }

    pub fn set_refs(&mut self, source: RefSource, uids: Vec<SessionUid>) {
        let set: HashSet<SessionUid> = uids.iter().map(|u| self.ids.live(u)).collect();
        let old = self.refs.insert(source, set.clone()).unwrap_or_default();
        self.dirty.extend(old.symmetric_difference(&set).cloned());
    }

    pub fn set_paused_until(&mut self, until: Option<EpochMs>) {
        if self.paused_until != until {
            self.paused_until = until;
            self.queue_dirty = true;
        }
    }

    pub fn set_project_order(&mut self, order: Vec<ProjectId>) {
        let mut seen = HashSet::new();
        let mut v: Vec<ProjectId> = order.into_iter().filter(|p| self.projects.contains_key(p) && seen.insert(p.clone())).collect();
        for p in &self.project_order {
            if seen.insert(p.clone()) {
                v.push(p.clone());
            }
        }
        self.project_order = v;
        let now = self.now();
        self.publish_projects(&now);
    }

    /// `ui.sidebarFolds` (user fold state, M3).
    pub fn set_project_folds(&mut self, folded: Vec<ProjectId>) {
        self.project_folds = folded.into_iter().collect();
        let now = self.now();
        self.publish_projects(&now);
    }

    pub fn time_zone_changed(&mut self) {
        self.clock.zone_changed();
        let now = self.now();
        self.check_day(&now, true);
    }

    // -- marks ----------------------------------------------------------------

    pub fn mark_seen(&mut self, uid: &SessionUid) -> Result<(), crate::contract::IpcError> {
        let uid = self.ids.live(uid);
        let now = self.now();
        let r = self.sessions.get_mut(&uid).ok_or_else(|| crate::contract::IpcError::not_found("session"))?;
        r.marks.last_seen_at = Some(now.wall);
        self.dirty.insert(uid);
        Ok(())
    }

    pub fn dismiss(&mut self, uid: &SessionUid, what: crate::contract::ipc::DismissWhat) -> Result<(), crate::contract::IpcError> {
        use crate::contract::ipc::DismissWhat;
        let uid = self.ids.live(uid);
        let now = self.now();
        let r = self.sessions.get_mut(&uid).ok_or_else(|| crate::contract::IpcError::not_found("session"))?;
        let state = r.activity.as_ref().map(|a| a.state.clone());
        let ok = matches!(
            (what, &state),
            (DismissWhat::NewReply, Some(ActivityState::NewReply))
                | (DismissWhat::Crash, Some(ActivityState::Crashed { .. }))
                | (DismissWhat::Error, Some(ActivityState::Error { .. }))
        );
        if !ok {
            return Err(crate::contract::IpcError::refused("notInState"));
        }
        match what {
            DismissWhat::NewReply => r.marks.new_reply_dismissed_at = Some(now.wall),
            DismissWhat::Crash => r.marks.crash_dismissed_at = Some(now.wall),
            DismissWhat::Error => r.marks.error_dismissed_at = Some(now.wall),
        }
        self.dirty.insert(uid);
        Ok(())
    }

    pub fn queue_later(&mut self, uid: &SessionUid) -> Result<EpochMs, crate::contract::IpcError> {
        let uid = self.ids.live(uid);
        let key = attention::item_key(&uid);
        let item_band = self.queue.items.iter().find(|i| i.key == key).map(|i| i.band);
        let Some(band) = item_band else {
            return Err(crate::contract::IpcError::not_found("queueItem"));
        };
        let now = self.now();
        let until = self.later.set(&key, band, now.up, now.wall);
        self.queue_dirty = true;
        Ok(until)
    }

    // -- derivation -------------------------------------------------------------

    fn position(&self, uid: &SessionUid) -> TeamPosition {
        match self.roles.get(uid) {
            None => TeamPosition::Solo,
            Some(r) if r.role == MemberRole::Lead => {
                if r.live_panes {
                    TeamPosition::LeadWithLivePanes
                } else {
                    TeamPosition::Lead
                }
            }
            Some(r) if r.team_hosted && r.backend == crate::contract::team::MemberBackend::Pane => TeamPosition::HostedTeammate,
            Some(_) => TeamPosition::Teammate,
        }
    }

    fn control_kind(&self, r: &SessionRec, position: TeamPosition) -> (ControlKind, bool) {
        let alive = r.alive() || r.pending_spawn();
        if let Some(g) = &r.gilde {
            let held = g.mode == ControlMode::TurnBased && g.stop_cause.is_none();
            if alive || held {
                return (ControlKind::InGilde(g.mode), held);
            }
            return (ControlKind::NotRunning, false);
        }
        if alive && position == TeamPosition::HostedTeammate {
            return (ControlKind::InGilde(ControlMode::Terminal), false);
        }
        if alive {
            (ControlKind::Watching, false)
        } else {
            (ControlKind::NotRunning, false)
        }
    }

    /// `lead_has_working_members` (§3.5.2): from the members' own derived
    /// activity, never from the lead's registry [R6 §5 LP-20].
    fn lead_has_working_members(&self, lead: &SessionUid) -> bool {
        let Some(role) = self.roles.get(lead) else { return false };
        if role.role != MemberRole::Lead {
            return false;
        }
        let Some(team) = self.teams.teams.get(&role.team_key) else { return false };
        team.members.values().filter(|m| m.role == MemberRole::Member).any(|m| {
            if let Some(u) = &m.session_uid {
                if let Some(a) = self.sessions.get(&self.ids.live(u)).and_then(|r| r.activity.as_ref()) {
                    return a.state == ActivityState::Working;
                }
            }
            team.member_facts
                .get(&m.key)
                .and_then(|f| crate::world::teams::assembly::member_state_from_facts(f, false))
                .is_some_and(|s| s == ActivityState::Working)
        })
    }

    /// The wait the lead shows on this member's behalf, if any.
    fn delegated(&self, uid: &SessionUid) -> Option<(DelegatedWait, PendingInteraction)> {
        let role = self.roles.get(uid)?;
        if role.role != MemberRole::Member {
            return None;
        }
        let lead = role.lead_uid.as_ref()?;
        let lead_rec = self.sessions.get(&self.ids.live(lead))?;
        let card = lead_rec.pending.iter().find(|p| p.on_behalf_of.as_ref() == Some(&role.member_key))?;
        let question = waits_for_answer(card);
        let mut mirror = card.clone();
        mirror.answer_in = Some(lead_rec.uid.clone());
        Some((DelegatedWait { question, since: card.received_at }, mirror))
    }

    fn evidence_set(&self, r: &SessionRec, position: TeamPosition, now: &Now) -> EvidenceSet {
        let process = match (r.process, &r.gilde) {
            (Some(p), _) => p,
            (None, Some(_)) if r.pending_spawn() => ProcessFacts::Alive { pid: 0, start_sec: 0, cpu_active_at: None },
            // A stopped spawn that never reported a process, or a turn-based
            // session between turns (R0 decides while it is held).
            (None, Some(g)) => ProcessFacts::Gone { at: g.spawned_at },
            (None, None) => ProcessFacts::HistoryOnly,
        };
        let mut pty = r.pty.clone();
        if let (Some(p), Some((held, until))) = (pty.as_mut(), &r.prompt_hold) {
            if p.prompt.is_none() && *until > now.up {
                p.prompt = Some(held.clone());
            }
        }
        EvidenceSet {
            gilde: r.gilde.clone(),
            process,
            stream: r.stream.clone(),
            registry: r.registry.clone(),
            transcript: r.transcript.clone(),
            team: r.team_facts.clone(),
            pty,
            marks: r.marks,
            lead_has_working_members: matches!(position, TeamPosition::Lead | TeamPosition::LeadWithLivePanes)
                && self.lead_has_working_members(&r.uid),
            watched_band5_allowed: self.settings.watched_band5_allowed,
            no_progress_after_secs: self.settings.no_progress_after_secs,
            source_denied: r.source_denied,
        }
    }

    fn compute(&self, uid: &SessionUid, now: &Now) -> Option<Computed> {
        let r = self.sessions.get(uid)?;
        if r.merged_into.is_some() {
            return None;
        }
        let position = self.position(uid);
        let (kind, held) = self.control_kind(r, position);
        let e = self.evidence_set(r, position, now);
        let delegated = self.delegated(uid);
        let mut stamps = r.stamps.clone();
        if stamps.config_mtime.is_none() {
            stamps.config_mtime = self.roles.get(uid).and_then(|ro| ro.config_mtime);
        }
        let cx = DeriveCtx {
            vendor: r.vendor,
            control: kind,
            position,
            held_turn_based: held,
            delegated: delegated.as_ref().map(|(d, _)| d.clone()),
            stamps,
            now_up: now.up,
            now_wall: now.wall,
        };
        let d = derive(&e, &cx);
        // The NotRunning invariant [F-14, F-21]: a dead session only shows
        // Ended / Stopped / Crashed / an actionable error.
        let kind = match (&d.state, kind) {
            (ActivityState::Ended | ActivityState::Stopped | ActivityState::Crashed { .. }, ControlKind::InGilde(_))
                if !held =>
            {
                ControlKind::NotRunning
            }
            (_, k) => k,
        };
        let control = self.control_of(r, kind, &d.state);
        let pending = if matches!(kind, ControlKind::NotRunning) {
            Vec::new()
        } else {
            self.pending_of(r, &d, kind, &e, delegated.map(|(_, m)| m))
        };
        Some(Computed { derived: d, control, control_kind: kind, pending })
    }

    fn control_of(&self, r: &SessionRec, kind: ControlKind, state: &ActivityState) -> Control {
        match kind {
            ControlKind::InGilde(mode) => {
                let detail = r.control_detail.clone().unwrap_or_default();
                let transport = detail.transport.unwrap_or(match mode {
                    ControlMode::Conversation => Transport::L1,
                    ControlMode::Terminal => Transport::Pty,
                    ControlMode::TurnBased => Transport::Turns,
                });
                let hosted_team = detail.hosted_team
                    || (mode == ControlMode::Terminal
                        && self.roles.get(&r.uid).is_some_and(|ro| ro.role == MemberRole::Lead && ro.team_hosted));
                Control::InGilde {
                    mode,
                    pty: if mode == ControlMode::Terminal { detail.pty } else { None },
                    hosted_team,
                    transport,
                }
            }
            ControlKind::Watching => Control::Watching {
                origin: r.meta.origin.unwrap_or(Origin::Unknown),
                owner: r.meta.owner.clone(),
            },
            ControlKind::NotRunning => Control::NotRunning {
                last_origin: r.meta.origin.or(r.persisted.as_ref().and_then(|p| p.origin_last)),
                ended_how: match state {
                    ActivityState::Stopped => EndedHow::Stopped,
                    ActivityState::Crashed { .. } => EndedHow::Crashed,
                    _ if matches!(r.process, Some(ProcessFacts::HistoryOnly) | None) => EndedHow::HistoryOnly,
                    _ => EndedHow::Ended,
                },
            },
        }
    }

    /// `Session.pending` (§3.4.5): the structured cards (Conversation), the
    /// screen prompt on the session's own screen (Terminal), a mirror of the
    /// lead's card (a member answered at the lead), or the synthesized
    /// watched card (Watching).
    fn pending_of(
        &self,
        r: &SessionRec,
        d: &Derived,
        kind: ControlKind,
        e: &EvidenceSet,
        mirror: Option<PendingInteraction>,
    ) -> Vec<PendingInteraction> {
        let waiting = matches!(d.state, ActivityState::NeedsApproval | ActivityState::NeedsAnswer);
        if !waiting {
            return Vec::new();
        }
        if d.decided_by == Src::Delegated {
            return mirror.into_iter().collect();
        }
        match kind {
            ControlKind::InGilde(ControlMode::Conversation) | ControlKind::InGilde(ControlMode::TurnBased) => {
                let mut v = e.stream.as_ref().map(|s| s.pending.clone()).unwrap_or_default();
                v.sort_by_key(|p| p.received_at);
                let mut seen = HashSet::new();
                v.retain(|p| seen.insert(p.request_id.clone()));
                v
            }
            ControlKind::InGilde(ControlMode::Terminal) => e
                .pty
                .as_ref()
                .and_then(|p| p.prompt.as_ref().map(|pr| (pr, p.confidence)))
                .map(|(pr, conf)| vec![self.screen_card(r, pr, conf)])
                .unwrap_or_default(),
            ControlKind::Watching => vec![watched_card(r, d, e)],
            ControlKind::NotRunning => Vec::new(),
        }
    }

    /// A screen-prompt card (§6.2.4) with the teammate attribution of
    /// §7.2.6: (1) the lead's prompt header, (3) a lead-inbox frame, (4) none.
    fn screen_card(&self, r: &SessionRec, pr: &ScreenPromptFacts, confidence: f32) -> PendingInteraction {
        let text = |s: &str, n: usize| sanitize_display_text(s, TextCap::Chars(n)).text;
        // The one role mapping (A25.1), incl. the trust dialog's TrustFolder
        // option; the §3.4.5 key filter stays here.
        let mut keys = HashSet::new();
        let options: Vec<InteractionOption> = crate::status::evidence::card_options(pr)
            .into_iter()
            .filter(|o| crate::contract::interaction::is_option_key(&o.key) && keys.insert(o.key.clone()))
            .collect();
        let mut on_behalf_of: Option<MemberKey> = None;
        // P19 (data-switched): pane teammates' prompts render in the lead;
        // fallback (flag off or absent): the card stays the lead's own.
        let flag = |f: &str| self.compat_of(r.version().as_deref()).and_then(|c| c.features.get(f).copied()) == Some(true);
        let lead_prompts = flag(FLAG_LEAD_PROMPTS_FOR_TEAMMATES);
        let question_location = flag(FLAG_TEAMMATE_QUESTION_LOCATION);
        if lead_prompts && self.roles.get(&r.uid).is_some_and(|ro| ro.role == MemberRole::Lead) {
            let haystack: Vec<&str> = pr
                .header
                .iter()
                .chain(pr.question.iter())
                .map(String::as_str)
                .chain(pr.options.iter().map(|o| o.label.as_str()))
                .collect();
            for h in haystack {
                if let Some(c) = self.patterns.prompt_header.captures(h) {
                    if let Some(m) = c.get(1).and_then(|n| self.teams.member_of_lead_by_name(&r.uid, n.as_str())) {
                        on_behalf_of = Some(m);
                        break;
                    }
                }
            }
            if on_behalf_of.is_none() {
                // P22: a pane teammate's question renders in the lead only
                // where that is verified; otherwise "location unverified".
                on_behalf_of = self
                    .teams
                    .attributions_for(&r.uid, pr.first_seen_at - 5_000)
                    .into_iter()
                    .find(|a| !a.question || question_location)
                    .map(|a| a.member);
            }
        }
        // A35.2: the prompt box's header is what the card approves (shared
        // with T06's builder); without it, a permission card keeps ⌘⏎ inert.
        let detail_lines = crate::status::evidence::card_detail_lines(pr);
        let first_detail = detail_lines.first().and_then(|l| l.params.get("text")).and_then(|t| t.as_str());
        let title = pr.question.as_deref().or(first_detail).map(|s| text(s, 200)).unwrap_or_default();
        PendingInteraction {
            request_id: RequestId::parse(format!("sp_{}", pr.prompt_rev))
                .unwrap_or_else(|_| RequestId::parse(format!("sp_{}", hash16(pr.prompt_rev.as_bytes()))).expect("hex id")),
            kind: if pr.setup { InteractionKind::SetupPrompt } else { InteractionKind::ScreenPrompt },
            received_at: pr.first_seen_at,
            on_behalf_of,
            answer_in: None,
            title,
            tool: None,
            default_to_no: crate::status::evidence::approves_unseen(pr),
            detail_lines,
            options,
            always_label: None,
            degraded: false,
            prompt_rev: Some(pr.prompt_rev.clone()),
            confidence: if confidence < derive::PTY_LIKELY_BELOW { Confidence::Likely } else { Confidence::High },
            multi_select: false,
            questions: Vec::new(),
        }
    }

    /// Store one computed result; returns whether the pending list changed.
    fn store(&mut self, uid: &SessionUid, c: Computed, now: &Now) -> bool {
        let Some(r) = self.sessions.get_mut(uid) else { return false };
        // S5 debounce: a state from the PTY engine is published only once it
        // held for 300 ms; S1 transitions publish at once.
        if c.derived.decided_by == Src::S5 && !c.derived.corroborated {
            if let Some(cur) = &r.activity {
                let target = crate::world::episodes::class_of(&c.derived.state);
                if crate::world::episodes::class_of(&cur.state) != target {
                    match &r.debounce {
                        Some((t, deadline)) if *t == target && *deadline <= now.up => r.debounce = None,
                        Some((t, _)) if *t == target => return false,
                        _ => {
                            r.debounce = Some((target, now.up + 300));
                            return false;
                        }
                    }
                } else {
                    r.debounce = None;
                }
            }
        } else {
            r.debounce = None;
        }
        let ep = r.episode.observe(uid, &c.derived.state, c.derived.since_hint, now.wall, now.up);
        let mut state = c.derived.state.clone();
        // NoProgress: `for_secs` drifts every second; keep the published value
        // within a minute so a quiet session does not patch every tick.
        if let (ActivityState::NoProgress { for_secs }, Some(Activity { state: ActivityState::NoProgress { for_secs: old }, .. })) =
            (&state, &r.activity)
        {
            if for_secs.abs_diff(*old) < 60 {
                state = ActivityState::NoProgress { for_secs: *old };
            }
        }
        let activity = Activity {
            state,
            since: ep.since,
            confidence: c.derived.confidence,
            evidence: c.derived.evidence.clone(),
            episode: ep.id,
        };
        let pending_changed = r.pending != c.pending;
        r.since_up = ep.since_up;
        r.activity = Some(activity);
        r.recheck_up = c.derived.recheck_up;
        r.derived = Some(c.derived);
        r.control = Some(c.control);
        r.control_kind = Some(c.control_kind);
        r.pending = c.pending;
        pending_changed
    }

    /// Re-derive the dirty sessions (members, then leads, then the members
    /// of leads whose cards changed), publish what changed, recompute teams,
    /// projects, accounts and the queue. Returns the new queue when it
    /// changed (World passes it to `NotifyApi::queue_changed`).
    pub fn refresh(&mut self) -> Option<AttentionQueue> {
        let now = self.now();
        self.retry_held(&now);
        self.check_day(&now, false);
        if self.roles_dirty {
            let old = std::mem::take(&mut self.roles);
            self.roles = self.teams.roles();
            for (u, ro) in &self.roles {
                if old.get(u) != Some(ro) {
                    self.dirty.insert(u.clone());
                }
            }
            for u in old.keys() {
                if !self.roles.contains_key(u) {
                    self.dirty.insert(u.clone());
                }
            }
            self.roles_dirty = false;
        }
        if self.dirty.is_empty() && !self.queue_dirty && self.teams.dirty.is_empty() {
            if std::mem::take(&mut self.accounts_dirty) {
                self.publish_accounts();
            }
            return self.finish_queue(&now);
        }
        let mut todo: HashSet<SessionUid> = std::mem::take(&mut self.dirty);
        // Members influence their lead (working members); leads their members.
        let extra: Vec<SessionUid> = todo
            .iter()
            .filter_map(|u| self.roles.get(u))
            .filter_map(|ro| ro.lead_uid.clone())
            .collect();
        todo.extend(extra.into_iter().map(|u| self.ids.live(&u)));
        let (leads, others): (Vec<SessionUid>, Vec<SessionUid>) = todo
            .iter()
            .cloned()
            .partition(|u| self.roles.get(u).is_some_and(|r| r.role == MemberRole::Lead));
        for uid in &others {
            if let Some(c) = self.compute(uid, &now) {
                self.store(uid, c, &now);
            }
        }
        let mut again: Vec<SessionUid> = Vec::new();
        for uid in &leads {
            if let Some(c) = self.compute(uid, &now) {
                if self.store(uid, c, &now) {
                    if let Some(ro) = self.roles.get(uid) {
                        if let Some(t) = self.teams.teams.get(&ro.team_key) {
                            again.extend(t.members.values().filter(|m| m.role == MemberRole::Member).filter_map(|m| m.session_uid.clone()));
                        }
                    }
                }
            }
        }
        for uid in &again {
            let uid = self.ids.live(uid);
            if let Some(c) = self.compute(&uid, &now) {
                self.store(&uid, c, &now);
            }
            todo.insert(uid);
        }
        let changed = self.publish_sessions(&todo, &now);
        if !changed.is_empty() {
            self.teams.touch_sessions(&changed);
        }
        if !self.teams.dirty.is_empty() {
            let snap = view_snapshot(&View { g: self });
            let mut outbox = std::mem::take(&mut self.outbox);
            let recheck = self.teams.publish(&snap, &mut outbox, now.wall, false);
            self.outbox = outbox;
            self.team_recheck_wall = recheck;
        }
        if !changed.is_empty() {
            self.publish_projects(&now);
            self.accounts_dirty = true;
            self.queue_dirty = true;
        }
        // A24.8: after the sessions, so live counts are current.
        if std::mem::take(&mut self.accounts_dirty) {
            self.publish_accounts();
        }
        self.finish_queue(&now)
    }

    /// Gilde's own process seen before its Gilde facts: never shown, so it
    /// is never Watching, not even for one scan (PM-22). Hosted pane members
    /// (Gilde-owned, no Gilde facts of their own) are shown through their
    /// team role.
    fn hidden(&self, r: &SessionRec) -> bool {
        (r.spawn_unbound || (r.own_unbound && r.gilde.is_none()))
            && r.meta.teammate.is_none()
            && !self.roles.contains_key(&r.uid)
    }

    fn in_store(&self, r: &SessionRec) -> bool {
        if self.hidden(r) {
            return false;
        }
        if r.merged_into.is_some() {
            return true;
        }
        let referenced = self.refs.values().any(|s| s.contains(&r.uid));
        let live = !matches!(r.control_kind, Some(ControlKind::NotRunning) | None);
        let ended_today = r
            .ended_at()
            .or(r.last_activity_at())
            .is_some_and(|t| self.clock.local_day(t) == self.today);
        live || ended_today || referenced
    }

    /// Publish what changed; returns the sessions whose store row changed.
    fn publish_sessions(&mut self, uids: &HashSet<SessionUid>, now: &Now) -> HashSet<SessionUid> {
        let mut changed = HashSet::new();
        for uid in uids {
            let Some(r) = self.sessions.get(uid) else { continue };
            if r.activity.is_none() && r.merged_into.is_none() {
                continue;
            }
            // A20.1: bound to a spawn whose Gilde facts are still on their way.
            if r.awaiting_spawn_up.is_some_and(|t| t > now.up) {
                continue;
            }
            let member = self.in_store(r);
            if member {
                let s = self.project(r, now);
                if r.published.as_ref() != Some(&s) {
                    self.outbox.upsert(Entity::Session { value: Box::new(s.clone()) });
                    changed.insert(uid.clone());
                    if let Some(rm) = self.sessions.get_mut(uid) {
                        rm.published = Some(s);
                    }
                }
            } else if r.published.is_some() {
                self.outbox.remove(EntityKind::Session, uid.as_str());
                changed.insert(uid.clone());
                if let Some(rm) = self.sessions.get_mut(uid) {
                    rm.published = None;
                }
            }
            self.persist(uid);
        }
        changed
    }

    /// The `session_meta` row of a keyed session, upserted when it changed.
    fn persist(&mut self, uid: &SessionUid) {
        let Some(key) = self.ids.key_of(uid).cloned() else { return };
        let Some(r) = self.sessions.get(uid) else { return };
        if r.merged_into.is_some() {
            return;
        }
        let first_seen = r.persisted.as_ref().map(|p| p.first_seen_at).unwrap_or(r.first_seen_wall);
        let today = self.today.clone();
        let location_path = self.location_of(r).map(|l| l.path.as_str().to_string());
        let role = self.roles.get(uid);
        let labels = r.meta.teammate.clone().unwrap_or_default();
        let row = MetaRow {
            uid: uid.clone(),
            vendor: Some(key.vendor),
            account_id: key.account_id.clone(),
            vendor_session_id: Some(key.vendor_session_id.clone()),
            project_id: self.location_of(r).map(|l| l.project_id.clone()),
            location_path,
            cwd: r.meta.cwd.as_ref().map(|c| c.as_str().to_string()),
            transcript_path: r.meta.transcript_path.as_ref().map(|c| c.as_str().to_string()),
            origin_last: r.meta.origin.or(r.persisted.as_ref().and_then(|p| p.origin_last)),
            control_mode_last: r.gilde.as_ref().map(|g| g.mode).or(r.persisted.as_ref().and_then(|p| p.control_mode_last)),
            permission_mode_last: r.meta.permission_mode,
            model_last: r.meta.model.clone(),
            cli_path_last: r.meta.binary.as_ref().map(|b| b.realpath.as_str().to_string()),
            cli_version_last: r.meta.binary.as_ref().and_then(|b| b.version.clone()).or(r.meta.claude_version.clone()),
            team_key: role.map(|ro| ro.team_key.clone()),
            agent_id: labels.agent_id.clone(),
            teammate_name: labels.agent_name.clone(),
            parent_session_id: labels.parent_session_id.clone(),
            parent_uid: r.parent_uid.clone(),
            first_seen_at: first_seen,
            started_at: r.started_at(),
            ended_at: r.ended_at(),
            last_activity_at: r.last_activity_at(),
            last_seen_at: r.marks.last_seen_at,
            usage: r.usage_today(&today),
        };
        if r.persisted.as_ref() != Some(&row) {
            self.meta_store.upsert(&row);
            self.ids.mark_persisted(uid, first_seen);
            if let Some(rm) = self.sessions.get_mut(uid) {
                rm.persisted = Some(row);
            }
        }
    }

    fn location_of(&self, r: &SessionRec) -> Option<&Location> {
        let cwd = r.meta.cwd.as_ref()?.as_str();
        self.locations
            .values()
            .map(|(l, _)| l)
            .filter(|l| {
                let p = l.path.as_str();
                cwd == p || cwd.strip_prefix(p).is_some_and(|rest| rest.starts_with('/'))
            })
            .max_by_key(|l| l.path.as_str().len())
    }

    fn display(&self, path: &str) -> String {
        display_path(path, PathBase { location: None, home: self.home.as_deref() })
    }

    fn compat_of(&self, version: Option<&str>) -> Option<&ClaudeCompat> {
        let v = version?;
        self.compat.iter().find(|c| c.version == v)
    }

    /// The contract `Session` of a record.
    fn project(&self, r: &SessionRec, now: &Now) -> Session {
        let mut activity = r.activity.clone().unwrap_or_else(|| Activity {
            state: ActivityState::StatusUnknown { why: crate::contract::activity::UnknownWhy::ProcessOnly },
            since: now.wall,
            confidence: Confidence::Likely,
            evidence: None,
            episode: crate::contract::ids::EpisodeId::derive(&r.uid, "statusUnknown", now.wall),
        });
        let kind = r.control_kind.unwrap_or(ControlKind::NotRunning);
        // A driver's session line (A19.1) is the evidence line while the
        // session runs; World's own line ("Can't read updates") comes first.
        if activity.evidence.is_none() && !matches!(kind, ControlKind::NotRunning) {
            activity.evidence = r.details_extra.lines.first().or(r.pane_lines.first()).cloned();
        }
        let control = r.control.clone().unwrap_or(Control::NotRunning { last_origin: None, ended_how: EndedHow::HistoryOnly });
        // A35.1: the Conversation driver's report wins while it runs the session.
        let permission_mode = match (&control, r.mode_reported) {
            (Control::InGilde { mode: ControlMode::Conversation, .. }, Some(m)) => m,
            _ => r.meta.permission_mode,
        };
        let loc = self.location_of(r);
        let role = self.roles.get(&r.uid);
        let team = role.filter(|ro| ro.published).map(|ro| TeamRole {
            team_key: ro.team_key.clone(),
            member_key: ro.member_key.clone(),
            role: ro.role,
            colour: ro.colour,
        });
        let version = r.version();
        let compat = self.compat_of(version.as_deref());
        let git_location = loc
            .and_then(|l| self.projects.get(&l.project_id))
            .is_some_and(|p| p.git);
        let transcript_known = r.meta.transcript_path.is_some() || r.transcript.is_some();
        let caps_in = CapsInput {
            vendor_session_id_known: r.vendor_session_id.is_some(),
            transcript_known,
            origin: r.meta.origin,
            has_owner: r.meta.owner.is_some(),
            launch_permission_mode: r.meta.permission_mode,
            pty_known: matches!(&control, Control::InGilde { pty: Some(_), .. }),
            git_location,
            gilde_spawned: r.gilde.is_some(),
            conversation_compat: compat.map(|c| c.conversation),
        };
        let caps = caps(kind, r.vendor, &activity.state, &caps_in);
        let account = r.account_id.as_ref().and_then(|a| self.accounts.get(a)).map(|(a, _)| a);
        let lead_version = role
            .and_then(|ro| ro.lead_uid.as_ref())
            .and_then(|l| self.sessions.get(&self.ids.live(l)))
            .and_then(|l| l.meta.claude_version.clone());
        let team_rec = role.and_then(|ro| self.teams.teams.get(&ro.team_key));
        let details = SessionDetails {
            vendor_session_id: r.vendor_session_id.clone(),
            account_label: account.map(|a| a.label.clone()),
            config_dir: account.and_then(|a| a.config_dir.as_ref()).map(|c| self.display(c.as_str())),
            cwd: r.meta.cwd.as_ref().map(|c| self.display(c.as_str())),
            location: loc.map(|l| self.display(l.path.as_str())),
            model: r.meta.model.clone(),
            control_mode: match kind {
                ControlKind::InGilde(m) => Some(m),
                _ => r.gilde.as_ref().map(|g| g.mode),
            },
            connection: match &control {
                Control::InGilde { transport, .. } => Some(*transport),
                _ => None,
            },
            claude_version: version.clone(),
            compat_state: compat.map(|c| match kind {
                ControlKind::InGilde(ControlMode::Conversation) => c.conversation,
                _ if role.is_some_and(|ro| ro.team_hosted) => c.pane_hosting,
                _ => c.conversation,
            }),
            binary_realpath: r.meta.binary.as_ref().map(|b| self.display(b.realpath.as_str())),
            version_note: r.details_extra.version_note.clone(),
            started_at: r.started_at(),
            memory_bytes: r.meta.memory_bytes,
            transcript_path: r.meta.transcript_path.as_ref().map(|t| self.display(t.as_str())),
            team_raw_id: team_rec.map(|t| t.raw_id.clone()).filter(|s| !s.is_empty()),
            orchestrator_task_title: None,
            usage_today: r.usage_today(&self.today),
            approval_channel_unconfirmed: r.details_extra.approval_channel_unconfirmed.clone(),
            follow_last_row_at: if matches!(kind, ControlKind::Watching) {
                r.transcript.as_ref().and_then(|t| t.last_row_at)
            } else {
                None
            },
            teammate_version_differs: role.is_some_and(|ro| ro.role == MemberRole::Member)
                && version.is_some()
                && lead_version.is_some()
                && version != lead_version,
            team_format_unverified: role.is_some()
                && compat.is_some_and(|c| c.features.get("teamStoreVerified") == Some(&false)),
        };
        Session {
            uid: r.uid.clone(),
            vendor: r.vendor,
            account_id: r.account_id.clone(),
            vendor_session_id: r.vendor_session_id.clone(),
            project_id: loc.map(|l| l.project_id.clone()),
            location_id: loc.map(|l| l.id.clone()),
            cwd: r.meta.cwd.clone(),
            control,
            activity,
            title: r.meta.title.clone(),
            team,
            started_at: r.started_at(),
            ended_at: if matches!(kind, ControlKind::NotRunning) { r.ended_at() } else { None },
            last_activity_at: r.last_activity_at(),
            pending: r.pending.clone(),
            // A queued message does not outlive its process.
            queued: if matches!(kind, ControlKind::NotRunning) { Vec::new() } else { r.queued.clone() },
            model: r.meta.model.clone(),
            permission_mode,
            binary: r.meta.binary.clone(),
            claude_version: version,
            memory_bytes: r.meta.memory_bytes.map(crate::discovery::reconcile::quantize_memory),
            usage_today: r.usage_today(&self.today),
            caps,
            details,
            merged_into: r.merged_into.clone(),
        }
    }

    fn publish_projects(&mut self, now: &Now) {
        let mut live: HashMap<ProjectId, (u32, HashSet<TeamKey>, Option<EpochMs>)> = HashMap::new();
        for r in self.sessions.values() {
            if r.merged_into.is_some() || self.hidden(r) {
                continue;
            }
            let Some(loc) = self.location_of(r) else { continue };
            let e = live.entry(loc.project_id.clone()).or_default();
            if let Some(at) = r.last_activity_at() {
                e.2 = Some(e.2.map_or(at, |x: EpochMs| x.max(at)));
            }
            if !matches!(r.control_kind, Some(ControlKind::NotRunning) | None) {
                // A team counts once (ux-ia §2.5).
                match self.roles.get(&r.uid).filter(|ro| ro.published) {
                    Some(ro) => {
                        e.1.insert(ro.team_key.clone());
                    }
                    None => e.0 += 1,
                }
            }
        }
        let day_ms: EpochMs = 24 * 60 * 60 * 1000;
        let mut rows: Vec<(bool, u64, ProjectId)> = Vec::new();
        for id in &self.project_order {
            let Some(p) = self.projects.get(id) else { continue };
            let (solo, teams, last) = live.get(id).cloned().unwrap_or_default();
            let n = solo + teams.len() as u32;
            // A24.4: a pick is the user's activity in the project.
            let touched = last.max(p.registered_at);
            let earlier = n == 0 && touched.is_none_or(|t| now.wall - t > day_ms);
            rows.push((earlier, p.seq, id.clone()));
        }
        // Dense order over non-earlier projects first, in user order.
        let mut order = 0u32;
        let mut ordered: Vec<(ProjectId, u32, bool)> = Vec::new();
        for (earlier, _, id) in rows.iter().filter(|r| !r.0) {
            ordered.push((id.clone(), order, *earlier));
            order += 1;
        }
        for (earlier, _, id) in rows.iter().filter(|r| r.0) {
            ordered.push((id.clone(), order, *earlier));
            order += 1;
        }
        for (id, ord, earlier) in ordered {
            let (solo, teams, last) = live.get(&id).cloned().unwrap_or_default();
            let Some(p) = self.projects.get(&id) else { continue };
            let root = p.root.as_str();
            let name = root.rsplit('/').next().unwrap_or(root);
            let project = Project {
                id: id.clone(),
                name: label(name, 255).unwrap_or_default(),
                root: p.root.clone(),
                kind: if p.git { ProjectKind::Git } else { ProjectKind::Folder },
                order: ord,
                folded: self.project_folds.contains(&id),
                earlier,
                last_activity_at: last,
                live_sessions: solo + teams.len() as u32,
                is_temp: root.starts_with("/tmp/") || root.starts_with("/private/tmp/") || root == "/tmp" || root == "/private/tmp",
            };
            if p.published.as_ref() != Some(&project) {
                self.outbox.upsert(Entity::Project { value: project.clone() });
                if let Some(pm) = self.projects.get_mut(&id) {
                    pm.published = Some(project);
                }
            }
        }
    }

    fn publish_accounts(&mut self) {
        let mut live: HashMap<AccountId, u32> = HashMap::new();
        for r in self.sessions.values() {
            if r.merged_into.is_some() || matches!(r.control_kind, Some(ControlKind::NotRunning) | None) {
                continue;
            }
            if let Some(a) = &r.account_id {
                *live.entry(a.clone()).or_default() += 1;
            }
        }
        for (id, (a, published)) in self.accounts.iter_mut() {
            let mut a2 = a.clone();
            a2.live_sessions = live.get(id).copied().unwrap_or(0);
            if published.as_ref() != Some(&a2) {
                self.outbox.upsert(Entity::Account { value: a2.clone() });
                *published = Some(a2);
            }
        }
    }

    fn queue_sessions(&self) -> Vec<QueueSession> {
        let mut out = Vec::new();
        for r in self.sessions.values() {
            if r.merged_into.is_some() || self.hidden(r) || r.published.is_none() {
                continue;
            }
            // A row held for its spawn (A20.1) counts as it is shown.
            let held = r.awaiting_spawn_up.is_some();
            let shown = r.published.as_ref().expect("published");
            let a = if held { &shown.activity } else {
                match &r.activity {
                    Some(a) => a,
                    None => continue,
                }
            };
            let pending_now: &Vec<PendingInteraction> = if held { &shown.pending } else { &r.pending };
            let kind = if held {
                match &shown.control {
                    Control::InGilde { mode, .. } => ControlKind::InGilde(*mode),
                    Control::Watching { .. } => ControlKind::Watching,
                    Control::NotRunning { .. } => ControlKind::NotRunning,
                }
            } else {
                r.control_kind.unwrap_or(ControlKind::NotRunning)
            };
            let b = band(&a.state);
            if b.is_none() {
                continue;
            }
            // Q3: a lead whose cards all carry on_behalf_of, each member with
            // an item of its own.
            // (A member's mirrored card has `answer_in` set: it is the item.)
            let suppressed = !pending_now.is_empty()
                && pending_now.iter().all(|p| {
                    p.answer_in.is_none()
                        && p.on_behalf_of.as_ref().is_some_and(|m| {
                            self.teams
                                .member_session(m)
                                .map(|u| self.ids.live(&u))
                                .filter(|u| u != &r.uid)
                                .and_then(|u| self.sessions.get(&u))
                                .is_some_and(|mr| mr.published.is_some())
                        })
                });
            let (on_behalf_of, answer_in) = match pending_now.first() {
                Some(p) => (p.on_behalf_of.clone(), p.answer_in.clone()),
                None => (None, None),
            };
            out.push(QueueSession {
                uid: r.uid.clone(),
                state: a.state.clone(),
                band: b,
                episode: a.episode.clone(),
                since: a.since,
                since_up: r.since_up,
                primary: primary_action(&a.state, kind, false),
                watched: matches!(kind, ControlKind::Watching),
                on_behalf_of,
                answer_in,
                suppressed,
            });
        }
        out
    }

    fn finish_queue(&mut self, now: &Now) -> Option<AttentionQueue> {
        if !self.queue_dirty {
            return None;
        }
        self.queue_dirty = false;
        let sessions = self.queue_sessions();
        let live_bands: HashMap<String, u8> = sessions
            .iter()
            .filter(|s| !s.suppressed)
            .filter_map(|s| s.band.map(|b| (attention::item_key(&s.uid), b)))
            .chain(
                (!self.leftovers.values().all(|l| l.leftover.is_none()))
                    .then(|| (attention::LEFTOVERS_KEY.to_string(), 6u8)),
            )
            .collect();
        self.later.prune(&live_bands, now.up);
        let lefts: Vec<&ProcessRecord> = self.leftovers.values().filter(|l| l.leftover.is_some()).collect();
        let summary = if lefts.is_empty() {
            LeftoverSummary::default()
        } else {
            let since = lefts.iter().filter_map(|l| l.leftover.as_ref().map(|x| x.since)).min().unwrap_or(now.wall);
            LeftoverSummary {
                count: lefts.len() as u32,
                bytes: lefts.iter().map(|l| l.rss_bytes).sum(),
                since,
                since_up: up_of(since, now.wall, now.up),
            }
        };
        let q = attention::compute(&sessions, summary, &self.later, self.paused_until, self.queue_rev + 1, now.up);
        if attention::same_queue(&q, &self.queue) {
            return None;
        }
        self.queue_rev += 1;
        self.queue = q.clone();
        self.outbox.push(StoreOp::Queue { queue: q.clone() });
        Some(q)
    }

    // -- time -------------------------------------------------------------------

    fn check_day(&mut self, now: &Now, force: bool) {
        let today = self.clock.local_day(now.wall);
        if force || today != self.today || now.up >= self.midnight_up {
            self.today = today;
            let midnight = self.clock.next_local_midnight(now.wall);
            self.midnight_up = now.up + u64::try_from((midnight - now.wall).max(1)).unwrap_or(1);
            // Membership ("ended today") is recomputed for every session.
            let uids: Vec<SessionUid> = self.sessions.keys().cloned().collect();
            self.dirty.extend(uids);
        }
    }

    /// Time-based work: held updates, tombstones, rechecks, Later expiry,
    /// local midnight, debounces. Call when [`Graph::next_deadline`] passed.
    pub fn tick(&mut self) {
        let now = self.now();
        for gone in self.ids.expire_tombstones(now.up) {
            if let Some(r) = self.sessions.remove(&gone) {
                if r.published.is_some() {
                    self.outbox.remove(EntityKind::Session, gone.as_str());
                }
            }
            self.meta_store.remove(&gone);
        }
        let due: Vec<SessionUid> = self
            .sessions
            .values()
            .filter(|r| {
                r.recheck_up.is_some_and(|t| t <= now.up)
                    || r.debounce.as_ref().is_some_and(|(_, d)| *d <= now.up)
                    || r.prompt_hold.as_ref().is_some_and(|(_, d)| *d <= now.up)
            })
            .map(|r| r.uid.clone())
            .collect();
        for uid in due {
            if let Some(r) = self.sessions.get_mut(&uid) {
                if r.prompt_hold.as_ref().is_some_and(|(_, d)| *d <= now.up) {
                    r.prompt_hold = None;
                }
                r.recheck_up = None;
            }
            self.dirty.insert(uid);
        }
        if self.team_recheck_wall.is_some_and(|w| w <= now.wall) {
            let keys: Vec<TeamKey> = self.teams.teams.keys().cloned().collect();
            self.teams.dirty.extend(keys);
            self.team_recheck_wall = None;
        }
        if self.later.next_expiry().is_some_and(|t| t <= now.up) {
            self.queue_dirty = true;
        }
    }

    /// The earliest uptime at which [`Graph::tick`] has work.
    pub fn next_deadline(&self) -> Option<u64> {
        let now = self.now();
        let mut at = Some(self.midnight_up);
        for r in self.sessions.values() {
            at = min_opt(at, r.recheck_up);
            at = min_opt(at, r.debounce.as_ref().map(|(_, d)| *d));
            at = min_opt(at, r.prompt_hold.as_ref().map(|(_, d)| *d));
        }
        at = min_opt(at, self.ids.next_tombstone_expiry());
        at = min_opt(at, self.held.iter().map(|h| h.deadline_up).min());
        at = min_opt(at, self.later.next_expiry());
        if let Some(w) = self.team_recheck_wall {
            at = min_opt(at, Some(now.up + u64::try_from((w - now.wall).max(0)).unwrap_or(0)));
        }
        at
    }

    // -- reads --------------------------------------------------------------------

    pub fn snapshot(&self) -> StoreSnapshot {
        let mut sessions: Vec<Session> = self
            .sessions
            .values()
            .filter(|r| r.published.is_some() && self.in_store(r))
            .filter_map(|r| r.published.clone())
            .collect();
        sessions.sort_by(|a, b| a.uid.as_str().cmp(b.uid.as_str()));
        let mut teams = Vec::new();
        let mut members = Vec::new();
        let mut tasks = Vec::new();
        let mut messages = Vec::new();
        for t in self.teams.teams.values() {
            if let Some(team) = &t.pub_team {
                teams.push(team.clone());
                members.extend(t.ordered_members().into_iter().filter(|m| t.pub_members.contains_key(&m.key)).cloned());
                tasks.extend(t.pub_tasks.values().cloned());
                let mut ms: Vec<_> = t.pub_message_values.values().cloned().collect();
                ms.sort_by(|a, b| (a.at, a.key.as_str()).cmp(&(b.at, b.key.as_str())));
                messages.extend(ms);
            }
        }
        StoreSnapshot {
            rev: self.publisher.rev(),
            projects: self.project_order.iter().filter_map(|p| self.projects.get(p)).filter_map(|p| p.published.clone()).collect(),
            locations: self.locations.values().map(|(l, _)| l.clone()).collect(),
            sessions,
            teams,
            members,
            tasks,
            messages,
            queue: self.queue.clone(),
            accounts: self.accounts.values().filter_map(|(_, p)| p.clone()).collect(),
            leftovers: self.leftovers.values().cloned().collect(),
            compat: self.compat.clone(),
            notices: self.notices.clone(),
            boot: self.boot.clone(),
            sources: self.sources.values().cloned().collect(),
            integrations: self.integrations.clone(),
        }
    }

    pub fn flush(&mut self) -> Option<StorePatch> {
        let up = self.clock.up_ms();
        let mut outbox = std::mem::take(&mut self.outbox);
        let p = self.publisher.flush(&mut outbox, up);
        self.outbox = outbox;
        p
    }

    /// A session row as the store has it; a merged uid returns its tombstone
    /// (`merged_into` names the survivor) while it exists.
    pub fn session(&self, uid: &SessionUid) -> Option<Session> {
        let uid = if self.sessions.contains_key(uid) { uid.clone() } else { self.ids.live(uid) };
        let r = self.sessions.get(&uid)?;
        let now = self.now();
        r.activity.as_ref()?;
        Some(r.published.clone().unwrap_or_else(|| self.project(r, &now)))
    }

    /// A10.3: the absolute transcript path and vendor (never from the
    /// display string).
    pub fn transcript(&self, uid: &SessionUid) -> Option<(AbsPath, Vendor)> {
        let uid = self.ids.live(uid);
        if let Some(r) = self.sessions.get(&uid) {
            if let Some(p) = &r.meta.transcript_path {
                return Some((p.clone(), r.vendor));
            }
        }
        let row = self.dormant.get(&uid)?;
        let p = AbsPath::parse(row.transcript_path.clone()?).ok()?;
        Some((p, row.vendor?))
    }

    /// The uid of a live process (orchestrator member matching, §7.3.1).
    pub fn uid_by_pid(&self, pid: i32) -> Option<SessionUid> {
        self.sessions
            .values()
            .find(|r| r.merged_into.is_none() && matches!(r.process, Some(ProcessFacts::Alive { pid: p, .. }) if p == pid))
            .map(|r| r.uid.clone())
    }
}

// ---------------------------------------------------------------------------
// the SessionView for team assembly (a snapshot to avoid aliasing the graph)
// ---------------------------------------------------------------------------

struct View<'a> {
    g: &'a Graph,
}

/// The session facts team assembly reads, copied out of the graph.
pub(crate) struct ViewSnapshot {
    states: HashMap<SessionUid, (ActivityState, Option<u8>)>,
    in_gilde: HashSet<SessionUid>,
    projects: HashMap<SessionUid, ProjectId>,
    features: HashMap<SessionUid, BTreeMap<String, bool>>,
}

fn view_snapshot(v: &View<'_>) -> ViewSnapshot {
    let mut states = HashMap::new();
    let mut in_gilde = HashSet::new();
    let mut projects = HashMap::new();
    let mut features = HashMap::new();
    for r in v.g.sessions.values() {
        if let Some(c) = v.g.compat_of(r.version().as_deref()) {
            features.insert(r.uid.clone(), c.features.clone());
        }
        if let Some(a) = &r.activity {
            states.insert(r.uid.clone(), (a.state.clone(), band(&a.state)));
        }
        if matches!(r.control_kind, Some(ControlKind::InGilde(_))) {
            in_gilde.insert(r.uid.clone());
        }
        if let Some(l) = v.g.location_of(r) {
            projects.insert(r.uid.clone(), l.project_id.clone());
        }
    }
    ViewSnapshot { states, in_gilde, projects, features }
}

impl SessionView for ViewSnapshot {
    fn state(&self, uid: &SessionUid) -> Option<(ActivityState, Option<u8>)> {
        self.states.get(uid).cloned()
    }
    fn in_gilde(&self, uid: &SessionUid) -> bool {
        self.in_gilde.contains(uid)
    }
    fn project(&self, uid: &SessionUid) -> Option<ProjectId> {
        self.projects.get(uid).cloned()
    }
    fn feature(&self, uid: &SessionUid, flag: &str) -> Option<bool> {
        self.features.get(uid).and_then(|f| f.get(flag).copied())
    }
}

// ---------------------------------------------------------------------------
// fact ingestion
// ---------------------------------------------------------------------------

fn clean(s: &str, n: usize) -> Option<String> {
    let t = sanitize_display_text(s, TextCap::Chars(n)).text;
    (!t.is_empty()).then_some(t)
}

/// A name shown as a label (project, worktree, branch): [`clean`] that also
/// drops zero-width characters (A48.3).
fn label(s: &str, n: usize) -> Option<String> {
    let t = sanitize_label_text(s, TextCap::Chars(n)).text;
    (!t.is_empty()).then_some(t)
}

fn source_kind_ord(k: SourceKind) -> u8 {
    match k {
        SourceKind::Registry => 0,
        SourceKind::Processes => 1,
        SourceKind::Teams => 2,
        SourceKind::Tasks => 3,
        SourceKind::Transcripts => 4,
        SourceKind::Jobs => 5,
        SourceKind::Rollouts => 6,
        SourceKind::ThreadIndex => 7,
        SourceKind::Chats => 8,
        SourceKind::Summaries => 9,
    }
}

fn merge_meta(into: &mut SessionMetaFacts, m: SessionMetaFacts) {
    if m.cwd.is_some() {
        into.cwd = m.cwd;
    }
    if m.started_at.is_some() {
        into.started_at = m.started_at;
    }
    if m.model.is_some() {
        into.model = m.model.as_deref().and_then(|s| clean(s, 128));
    }
    if m.permission_mode.is_some() {
        into.permission_mode = m.permission_mode;
    }
    if m.origin.is_some() {
        into.origin = m.origin;
    }
    if m.owner.is_some() {
        into.owner = m.owner;
    }
    if m.binary.is_some() {
        into.binary = m.binary;
    }
    if m.claude_version.is_some() {
        into.claude_version = m.claude_version;
    }
    if m.transcript_path.is_some() {
        into.transcript_path = m.transcript_path;
    }
    if let Some(t) = m.title {
        // [content]: memory only, sanitized, ≤ 120 chars.
        into.title = clean(&t, 120);
    }
    if m.teammate.is_some() {
        into.teammate = m.teammate;
    }
    if m.memory_bytes.is_some() {
        into.memory_bytes = m.memory_bytes;
    }
}

/// The Home notice of a source in `permissionDenied` (§9.12).
pub const CANT_READ_NOTICE: &str = "notice.cantReadFolders";
/// The Accounts & health line per denied folder category (§9.12, A38.8).
pub const FOLDER_DENIED_LINE: &str = "health.folderDenied";

/// The folder categories of the denied sources (A38.8: `config`, `teams`,
/// `transcripts`, `projects`), in that order, each once. A denied config root
/// denies its registry, teams and transcripts together; that is one line,
/// `config`. Processes are not a folder.
pub fn denied_categories(kinds: &[SourceKind]) -> Vec<&'static str> {
    let has = |k: SourceKind| kinds.contains(&k);
    if has(SourceKind::Registry) && has(SourceKind::Teams) && has(SourceKind::Transcripts) {
        return vec!["config"];
    }
    let mut out = Vec::new();
    for k in kinds {
        let c = match k {
            SourceKind::Registry | SourceKind::Jobs => "config",
            SourceKind::Teams | SourceKind::Tasks => "teams",
            SourceKind::Transcripts => "transcripts",
            SourceKind::Rollouts | SourceKind::ThreadIndex | SourceKind::Chats | SourceKind::Summaries => "config",
            SourceKind::Processes => continue,
        };
        if !out.contains(&c) {
            out.push(c);
        }
    }
    let rank = |c: &&str| ["config", "teams", "transcripts", "projects"].iter().position(|x| x == c);
    out.sort_by_key(rank);
    out
}

/// Merge one source's facts into a record, stamping what feeds a duration.
fn apply_facts(r: &mut SessionRec, facts: SourceFacts, hint_process: Option<&ProcessKey>, now: &Now) {
    let up = |wall: EpochMs| up_of(wall, now.wall, now.up);
    match facts {
        SourceFacts::Gilde(g) => {
            r.awaiting_spawn_up = None;
            let new_spawn = r.gilde.as_ref().map(|o| o.spawned_at) != Some(g.spawned_at);
            if new_spawn {
                r.stamps.spawn_up = Some(up(g.spawned_at));
                r.stamps.ready_latched = false;
                r.stamps.starting_paused_ms = 0;
                r.stamps.prompt_since_up = None;
                // The previous process's facts belong to the previous spawn.
                if matches!(r.process, Some(ProcessFacts::Exited { at, .. }) | Some(ProcessFacts::Gone { at }) if at <= g.spawned_at)
                    || matches!(r.process, Some(ProcessFacts::HistoryOnly))
                {
                    r.process = None;
                }
                if r.gilde.is_some() {
                    r.stream = None;
                    r.pty = None;
                    r.registry = None;
                    r.prompt_hold = None;
                }
            }
            r.gilde = Some(g);
        }
        SourceFacts::Process(p) => {
            match p {
                ProcessFacts::Alive { cpu_active_at, .. } => {
                    if let Some(c) = cpu_active_at {
                        let s = up(c);
                        r.stamps.cpu_up = Some(r.stamps.cpu_up.map_or(s, |o| o.max(s)));
                    }
                    r.process = Some(p);
                }
                ProcessFacts::HistoryOnly => {
                    // Never replaces a live (or a just-planned) process.
                    if !r.alive() && !r.pending_spawn() {
                        r.process = Some(p);
                    }
                }
                ProcessFacts::Exited { .. } | ProcessFacts::Gone { .. } => {
                    // A late exit of an earlier process (a resumed session
                    // runs a new one) is not this process's exit.
                    let other_process = match (r.process, hint_process) {
                        (Some(ProcessFacts::Alive { pid, start_sec, .. }), Some(k)) if pid > 0 => {
                            ProcessKey::new(pid as u32, start_sec.max(0) as u64) != *k
                        }
                        _ => false,
                    };
                    // A `Gone` after an `Exited` keeps the exit status.
                    let keep_status = matches!(r.process, Some(ProcessFacts::Exited { .. }))
                        && matches!(p, ProcessFacts::Gone { .. });
                    if !other_process && !keep_status {
                        r.process = Some(p);
                    }
                }
            }
        }
        SourceFacts::Stream(s) => {
            let advanced = r.stream.as_ref().is_none_or(|o| s.last_frame_at > o.last_frame_at);
            if advanced {
                r.stamps.frame_up = Some(up(s.last_frame_at));
            }
            r.stream = Some(s);
        }
        SourceFacts::Registry(reg) => {
            // A33.5(b): stamp `statusUpdatedAt` once per value (a re-sent
            // or unchanged value keeps its stamp, so a sleep never ages it).
            let changed = r.registry.as_ref().map(|o| o.status_updated_at) != Some(reg.status_updated_at);
            if changed {
                r.stamps.status_up = reg.status_updated_at.map(up);
            }
            r.registry = Some(reg);
        }
        SourceFacts::Transcript(t) => {
            let advanced = match (t.last_row_at, r.transcript.as_ref().and_then(|o| o.last_row_at)) {
                (Some(n), Some(o)) => n > o,
                (Some(_), None) => true,
                _ => false,
            };
            if advanced {
                r.stamps.row_up = t.last_row_at.map(up);
            }
            r.transcript = Some(t);
        }
        SourceFacts::Team(f) => {
            let new_request = match (&f.permission_request, r.team_facts.as_ref().and_then(|o| o.permission_request.as_ref())) {
                (Some((a, _)), Some((b, _))) => a != b,
                (Some(_), None) => true,
                _ => false,
            };
            if new_request {
                r.stamps.permission_request_up = f.permission_request.as_ref().map(|(at, _)| up(*at));
            }
            r.team_facts = Some(f);
        }
        SourceFacts::Pty(p) => {
            if let Some(o) = p.last_output_at {
                r.stamps.output_up = Some(up(o));
            }
            let ready = r.gilde.as_ref().is_some_and(|g| g.ready_at.is_some()) || r.stamps.ready_latched;
            let had = r.pty.as_ref().and_then(|o| o.prompt.clone());
            match (&had, &p.prompt) {
                (None, Some(_)) => {
                    if !ready {
                        r.stamps.prompt_since_up = Some(now.up);
                    }
                    r.prompt_hold = None;
                }
                (Some(old), None) => {
                    if let Some(since) = r.stamps.prompt_since_up.take() {
                        r.stamps.starting_paused_ms += now.up.saturating_sub(since);
                    }
                    // A card disappears only after 300 ms without the grid.
                    r.prompt_hold = Some((old.clone(), now.up + 300));
                }
                _ => {}
            }
            r.pty = Some(p);
        }
        SourceFacts::Meta(m) => merge_meta(&mut r.meta, *m),
        SourceFacts::SourceDenied(b) => r.source_denied = b,
        SourceFacts::Usage(u) => {
            // A10.4: latest report per (session, source); an older day is
            // ignored; absent = unknown, never 0.
            let key = UsageSourceKey::of(u.source);
            let older = r.usage.get(&key).is_some_and(|held| u.today.day < held.day);
            if !older {
                r.usage.insert(key, u.today);
            }
        }
    }
    // Latch a ready signal for a Gilde spawn (§3.5.6).
    if r.gilde.as_ref().is_some_and(|g| g.ready_at.is_none()) && !r.stamps.ready_latched {
        let e = EvidenceSet {
            gilde: r.gilde.clone(),
            process: r.process.unwrap_or(ProcessFacts::HistoryOnly),
            stream: r.stream.clone(),
            registry: r.registry.clone(),
            transcript: None,
            team: None,
            pty: r.pty.clone(),
            marks: r.marks,
            lead_has_working_members: false,
            watched_band5_allowed: false,
            no_progress_after_secs: 300,
            source_denied: false,
        };
        if ready_signal(&e) {
            r.stamps.ready_latched = true;
        }
    }
}

/// The synthesized card of a watched wait (§3.5.4 "Watched interactions").
fn watched_card(r: &SessionRec, d: &Derived, e: &EvidenceSet) -> PendingInteraction {
    let open = e.transcript.as_ref().and_then(|t| t.open_tool_use.as_ref());
    let digest_src = open
        .map(|o| o.tool_use_id.clone())
        .or_else(|| e.registry.as_ref().and_then(|g| g.status_updated_at).map(|t| t.to_string()))
        .unwrap_or_else(|| r.uid.as_str().to_string());
    let tool = open.map(|o| claude_tool_summary(&o.tool_name, &serde_json::Value::Null, PathBase::default()));
    let question = d.state == ActivityState::NeedsAnswer;
    PendingInteraction {
        request_id: RequestId::parse(format!("wa_{}", hash16(digest_src.as_bytes()))).expect("hex id"),
        kind: if question { InteractionKind::WatchedQuestion } else { InteractionKind::WatchedApproval },
        received_at: d.since_hint.unwrap_or(e.registry.as_ref().and_then(|g| g.status_updated_at).unwrap_or(0)),
        on_behalf_of: None,
        answer_in: None,
        title: open.map(|o| sanitize_display_text(&o.tool_name, TextCap::Chars(64)).text).unwrap_or_default(),
        tool,
        detail_lines: Vec::new(),
        options: Vec::new(),
        always_label: None,
        default_to_no: false,
        degraded: false,
        prompt_rev: None,
        confidence: d.confidence,
        multi_select: false,
        questions: Vec::new(),
    }
}

/// A delegated wait is a question unless the card is a tool approval or offers
/// an allow option — `AllowOnce`, or `AllowWithNote` (the same allow on a
/// "Tab to amend" prompt, A25.1).
fn waits_for_answer(card: &PendingInteraction) -> bool {
    !matches!(card.kind, InteractionKind::ToolApproval)
        && !card.options.iter().any(|o| matches!(o.role, OptionRole::AllowOnce | OptionRole::AllowWithNote))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt(labels: &[&str], selected: usize, setup: bool, footer: &str) -> ScreenPromptFacts {
        let options = labels
            .iter()
            .enumerate()
            .map(|(i, l)| crate::contract::activity::ScreenOptionFact {
                key: if setup { String::new() } else { (i + 1).to_string() },
                label: (*l).to_string(),
                description: None,
                selected: i == selected,
            })
            .collect();
        let first = labels.first().map(|l| l.to_lowercase()).unwrap_or_default();
        let last = labels.last().map(|l| l.to_lowercase()).unwrap_or_default();
        ScreenPromptFacts {
            prompt_rev: "0123456789abcdef".into(),
            question: Some("Do you want to proceed?".into()),
            header: None,
            options,
            footer: Some(footer.into()),
            permission_shaped: !setup && first.starts_with("yes") && last.starts_with("no"),
            setup,
            trust_dialog: setup,
            allows_note: footer.contains("Tab to amend"),
            near_bottom: true,
            first_seen_at: 1_000,
        }
    }

    fn card_for(pr: &ScreenPromptFacts) -> PendingInteraction {
        let g = Graph::new(Arc::new(ManualClock::new(1_000, 0)), Arc::new(MemoryMetaStore::default()), WorldSettings::default(), None);
        let rec = SessionRec::new(SessionUid::mint(), Vendor::Claude, &Now { wall: 1_000, up: 0 });
        g.screen_card(&rec, pr, 0.9)
    }

    /// A25.1: a "Tab to amend" permission prompt's option 1 is the
    /// note-capable allow — still an approval for a delegated wait, never a
    /// question.
    #[test]
    fn a_tab_to_amend_permission_prompt_stays_an_approval() {
        let pr = prompt(&["Yes", "Yes, and switch to auto mode", "No"], 0, false, "Esc to cancel · Tab to amend");
        let card = card_for(&pr);
        assert_eq!(card.kind, InteractionKind::ScreenPrompt);
        let roles: Vec<OptionRole> = card.options.iter().map(|o| o.role).collect();
        assert_eq!(roles, vec![OptionRole::AllowWithNote, OptionRole::Other, OptionRole::Deny]);
        assert!(!waits_for_answer(&card), "a permission card, not a question");
        let plain = card_for(&prompt(&["Yes", "No"], 0, false, "Esc to cancel"));
        assert_eq!(plain.options[0].role, OptionRole::AllowOnce);
        assert!(!waits_for_answer(&plain));
        let question = card_for(&prompt(&["Red", "Green"], 0, false, "Enter to select · Esc to cancel"));
        assert!(waits_for_answer(&question), "a choice prompt is a question");
    }

    /// A25.1 + A6: the trust dialog's setup card carries exactly one
    /// TrustFolder option (key = the trust item's menu position).
    #[test]
    fn the_trust_dialog_card_carries_one_trust_folder_option() {
        let card = card_for(&prompt(&["No, exit", "Yes, I trust this folder"], 0, true, "Enter to confirm · Esc to cancel"));
        assert_eq!(card.kind, InteractionKind::SetupPrompt);
        assert_eq!(card.options.len(), 1, "{:?}", card.options);
        assert_eq!(
            (card.options[0].key.as_str(), card.options[0].label.as_str(), card.options[0].role),
            ("2", "Yes, I trust this folder", OptionRole::TrustFolder)
        );
    }

    /// The A6 fallback: no single "Yes…" item (none, or several) — or a setup
    /// menu that is not the trust dialog — offers no option.
    #[test]
    fn no_single_yes_item_or_another_setup_menu_offers_nothing() {
        for labels in [&["No, exit", "Trust it"][..], &["Yes, exit", "Yes, I trust this folder"][..]] {
            let card = card_for(&prompt(labels, 0, true, "Enter to confirm · Esc to cancel"));
            assert!(card.options.is_empty(), "{labels:?} → {:?}", card.options);
        }
        let mut theme = prompt(&["Dark mode", "Light mode"], 0, true, "Enter to confirm");
        theme.trust_dialog = false;
        assert!(card_for(&theme).options.is_empty(), "other setup prompts are never answered");
    }

    #[test]
    fn manual_clock_midnight_and_offset() {
        let c = ManualClock::new(86_400_000 * 10 + 3_600_000, 0);
        assert_eq!(c.next_local_midnight(c.wall_ms()), 86_400_000 * 11);
        c.set_offset_ms(-2 * 3_600_000);
        assert_eq!(c.local_day(86_400_000 * 10 + 3_600_000), crate::follow::tail::utc_day(86_400_000 * 10 - 3_600_000));
    }

    #[test]
    fn usage_source_priority_order() {
        assert!(UsageSourceKey::of(UsageSource::Stream) < UsageSourceKey::of(UsageSource::Turns));
        assert!(UsageSourceKey::of(UsageSource::Turns) < UsageSourceKey::of(UsageSource::Transcript));
    }

    #[test]
    fn memory_meta_store_round_trip_has_no_content_fields() {
        let s = MemoryMetaStore::default();
        let row = MetaRow::new(SessionUid::mint(), 1);
        s.upsert(&row);
        assert_eq!(s.load(), vec![row]);
    }
    fn status(source: SourceKind, state: SourceState) -> SourceStatus {
        SourceStatus { vendor: Vendor::Claude, source, state, last_reconcile_at: Some(1), hidden_stale: 0, indexed: 0, truncated: false }
    }

    fn keys(g: &Graph) -> Vec<String> {
        g.snapshot().notices.iter().map(|n| {
            match n.params.get("category").and_then(|v| v.as_str()) {
                Some(c) => format!("{}:{c}", n.key),
                None => n.key.clone(),
            }
        }).collect()
    }

    /// A33.4 / §5.8: World derives exactly one `notice.cantReadFolders` (plus a
    /// `health.folderDenied{category}` line per category) from the `Sources`
    /// state, after the lifecycle notices; access coming back clears both.
    #[test]
    fn a33_4_denied_sources_raise_one_notice_and_clear() {
        let mut g = Graph::new(Arc::new(ManualClock::new(1_000, 0)), Arc::new(MemoryMetaStore::default()), WorldSettings::default(), None);
        g.apply_lifecycle(LifecycleOp::Notices(vec![Notice::new("notice.memoryOnly").param("reason", "diskFull")]));
        g.apply_source_status(status(SourceKind::Transcripts, SourceState::PermissionDenied));
        assert_eq!(keys(&g), vec!["notice.memoryOnly", "notice.cantReadFolders", "health.folderDenied:transcripts"]);
        // A second denied source: still one notice, one more category line.
        g.apply_source_status(status(SourceKind::Teams, SourceState::PermissionDenied));
        assert_eq!(keys(&g), vec!["notice.memoryOnly", "notice.cantReadFolders", "health.folderDenied:teams", "health.folderDenied:transcripts"]);
        // The config root: registry + teams + transcripts → one `config` line.
        g.apply_source_status(status(SourceKind::Registry, SourceState::PermissionDenied));
        assert_eq!(keys(&g), vec!["notice.memoryOnly", "notice.cantReadFolders", "health.folderDenied:config"]);
        // A lifecycle change keeps the derived notices.
        g.apply_lifecycle(LifecycleOp::Notices(vec![]));
        assert_eq!(keys(&g), vec!["notice.cantReadFolders", "health.folderDenied:config"]);
        // Access comes back.
        for k in [SourceKind::Registry, SourceKind::Teams, SourceKind::Transcripts] {
            g.apply_source_status(status(k, SourceState::Ok));
        }
        assert!(keys(&g).is_empty(), "{:?}", keys(&g));
        // Unavailable (not installed) is no denial.
        g.apply_source_status(status(SourceKind::Chats, SourceState::Unavailable { reason: "notInstalled".into() }));
        assert!(keys(&g).is_empty());
    }

    #[test]
    fn a33_4_denied_categories() {
        assert_eq!(denied_categories(&[SourceKind::Registry]), vec!["config"]);
        assert_eq!(denied_categories(&[SourceKind::Tasks, SourceKind::Teams]), vec!["teams"]);
        assert_eq!(denied_categories(&[SourceKind::Transcripts, SourceKind::Registry]), vec!["config", "transcripts"]);
        assert_eq!(denied_categories(&[SourceKind::Processes]), Vec::<&str>::new());
        assert_eq!(denied_categories(&[SourceKind::Transcripts, SourceKind::Teams, SourceKind::Registry, SourceKind::Tasks]), vec!["config"]);
    }
}
