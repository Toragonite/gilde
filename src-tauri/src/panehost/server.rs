//! The per-lead server model (§7.4.3) and the PaneHost service that answers
//! shim requests (§4.6) for every hosted lead.
//!
//! Owner: T07.
//!
//! ```text
//! Server { session, windows: [Window{ id:"@<n>", name:"swarm-view", panes }],
//!          panes: Map<%n, Pane>, next_pane, next_window }
//! Pane   { id, title, colour, remain_on_exit, state, kind, command, created_at }
//! state  = Placeholder{since} | Spawning | Running{pty, pid} | Exited{code, signal, at, kept} | Killed
//! ```
//!
//! * Ids are allocated from 0 upward and **never reused per server**; a new
//!   lead (also a resumed one) gets a new server.
//! * `list-panes` returns the **listed** panes of a window in creation order:
//!   placeholders, running panes and dead-but-kept panes (as tmux does). A
//!   pane that exits without being kept, or is killed, leaves its window; an
//!   empty window is destroyed, and the session with its last window — then
//!   `has-session` answers 1 again and the next `new-session` gets fresh ids.
//! * At most [`MAX_PANES`] listed panes per lead: the 16th is refused with
//!   `no space for new pane` (Claude maps it to its "no room" message).
//! * The model is pure; the service around it ([`PaneHost`]) runs every
//!   request of a lead under that lead's operation lock (a lead serializes
//!   pane creation itself [R3 §2.1]), changes the model under one short
//!   synchronous lock, and performs every side effect — PTY calls, compat
//!   records, evidence, table updates — after releasing it.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use parking_lot::{Mutex, RwLock};
use zeroize::Zeroizing;

use crate::contract::common::{EpochMs, IdentityColour};
use crate::contract::compat::CompatCapability;
use crate::contract::error::IpcError;
use crate::contract::ids::{AccountId, MemberKey, PaneId, PtyId, SessionUid, TeamKey};
use crate::contract::services::{CompatSink, LeadEnv, PaneHostApi, PtyApi, WorldApi};
use crate::contract::session::{Control, Origin};
use crate::panehost::auth::{self, Counters, PeerCred, RateWindow, Refusal};
use crate::panehost::proto::{self, extra, ShimRequest, ShimResponse};
use crate::panehost::subcommands::{self, texts, Cmd, ParseError, PaneOption, WindowTarget};

/// Listed panes per lead (§7.4.4 row 5/6: "> 15 panes for this lead → 1").
pub const MAX_PANES: usize = 15;
/// Unlisted panes kept for the pane table and idempotent `kill-pane`.
pub const MAX_HISTORY: usize = 64;
/// How long after a lead's exit a `kind:"bg"` registry entry may appear for
/// it to count as "moved to the background" (§7.4.7, P39 / LP-38).
pub const BACKGROUND_WINDOW: Duration = Duration::from_secs(3);

/// Wall clock in epoch milliseconds (display and evidence only; every
/// duration the service measures uses the monotonic clock).
pub fn now_ms() -> EpochMs {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as EpochMs)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// the model
// ---------------------------------------------------------------------------

/// `remain-on-exit` as stored from `set-option -p` (§7.4.4 row 14).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RemainOnExit {
    On,
    Off,
    Failed,
}

impl RemainOnExit {
    fn parse(v: &str) -> Option<RemainOnExit> {
        match v {
            "on" => Some(RemainOnExit::On),
            "off" => Some(RemainOnExit::Off),
            "failed" => Some(RemainOnExit::Failed),
            _ => None,
        }
    }
}

/// The life of one pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneState {
    /// Created by `new-session`/`new-window`/`split-window`; no process. The
    /// tile shows "starting…" and, 30 s after `since` without a
    /// `respawn-pane`, `team.didNotStart` — at once when `spawn_failed`.
    Placeholder { since: EpochMs, spawn_failed: bool },
    /// A `respawn-pane` is starting its process.
    Spawning { since: EpochMs },
    /// `/bin/sh -c <command>` runs in a PTY.
    Running { pty: PtyId, pid: i32, spawn_id: String, since: EpochMs },
    /// The process exited; `kept` = the tile stays as "Crashed · exit {code}".
    Exited { code: Option<i32>, signal: Option<i32>, at: EpochMs, kept: bool },
    /// Closed by `kill-pane`, by the user or by the lead's exit.
    Killed { at: EpochMs },
}

impl PaneState {
    /// What `list-panes` shows.
    pub fn listed(&self) -> bool {
        match self {
            PaneState::Placeholder { .. } | PaneState::Spawning { .. } | PaneState::Running { .. } => true,
            PaneState::Exited { kept, .. } => *kept,
            PaneState::Killed { .. } => false,
        }
    }
}

/// Why a pane is not a teammate (§7.4.5) [F-07, N-07].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnknownReason {
    /// The command does not have the teammate launch shape.
    NotTeammateCommand,
    /// The shape matches, but the claude binary is another install than the
    /// lead's (Details: `details.teammateBinaryDiffers`, A21.3).
    BinaryDiffers,
    /// The shape matches, but `--parent-session-id` names another session.
    OtherParent,
}

/// The §9.12 key of the Details note for a binary that differs from the
/// lead's install (A21.3).
pub const DETAILS_TEAMMATE_BINARY_DIFFERS: &str = "details.teammateBinaryDiffers";

impl UnknownReason {
    /// The Details note of this unknown pane, if it has one (A21.3): only a
    /// teammate command naming another claude install gets one.
    pub fn details_key(self) -> Option<&'static str> {
        match self {
            UnknownReason::BinaryDiffers => Some(DETAILS_TEAMMATE_BINARY_DIFFERS),
            UnknownReason::NotTeammateCommand | UnknownReason::OtherParent => None,
        }
    }
}

/// What runs in a pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneKind {
    /// No `respawn-pane` yet.
    Placeholder,
    /// The teammate launch pattern for this lead: may become a Member.
    Teammate { agent_id: String, team_name: String },
    /// Anything else: still runs (the lead asked for it), never a Member,
    /// never in the queue, never auto-focused (tile `team.unknownPane`).
    Unknown { reason: UnknownReason },
}

/// The pane command as Gilde keeps it: redacted (§9.7) plus a sha-256 of the
/// original; the original is never stored, logged or displayed [F-03].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneCommand {
    pub redacted: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Pane {
    pub id: u32,
    pub window: u32,
    /// Sanitized, ≤ 64 characters.
    pub title: Option<String>,
    pub colour: Option<IdentityColour>,
    /// A style named a colour outside the map (stored as unknown + a note).
    pub colour_unknown: bool,
    pub remain_on_exit: Option<RemainOnExit>,
    pub state: PaneState,
    pub kind: PaneKind,
    pub command: Option<PaneCommand>,
    pub created_at: EpochMs,
    /// Bumped by every respawn and close: an exit or a late spawn result of
    /// an older epoch is not this pane's current process.
    pub epoch: u64,
    /// Gilde signalled the current process (kill-pane, close, lead exit,
    /// respawn -k): its exit is a stop, never a crash.
    pub signalled: bool,
    /// An exit that arrived while the spawn had not returned yet.
    pub early_exit: Option<EarlyExit>,
    /// The last process this pane ran (kept after it exits or is killed, for
    /// the exit evidence of its session row).
    pub last_proc: Option<ProcRef>,
    /// The member's session row (teammate panes, A18.1).
    pub session: Option<SessionUid>,
}

/// `(code, signal, at)` of an exit that beat its spawn's return.
pub type EarlyExit = (Option<i32>, Option<i32>, EpochMs);

/// A pane process as the registry knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcRef {
    pub pty: PtyId,
    pub pid: i32,
    pub spawn_id: String,
    pub since: EpochMs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Window {
    id: u32,
    /// Listed panes in creation order.
    panes: Vec<u32>,
}

/// One hosted lead's tmux server (§7.4.3).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Server {
    session: bool,
    windows: Vec<Window>,
    panes: BTreeMap<u32, Pane>,
    next_pane: u32,
    next_window: u32,
    current: Option<u32>,
}

/// A model answer: `Ok(stdout)` or `Err(stderr)` (exit 1).
pub type Answer = Result<String, String>;

/// The colour of a style value such as `bg=default,fg=blue`: `None` without
/// an `fg`, `Some(Err(()))` for a colour outside the map [R3 §2.2].
pub fn colour_of(style: &str) -> Option<Result<IdentityColour, ()>> {
    let fg = style.split(',').find_map(|part| part.trim().strip_prefix("fg="))?;
    Some(match fg {
        "red" => Ok(IdentityColour::Red),
        "blue" => Ok(IdentityColour::Blue),
        "green" => Ok(IdentityColour::Green),
        "yellow" => Ok(IdentityColour::Yellow),
        "magenta" => Ok(IdentityColour::Purple),
        "colour208" => Ok(IdentityColour::Orange),
        "colour205" => Ok(IdentityColour::Pink),
        "cyan" => Ok(IdentityColour::Cyan),
        _ => Err(()),
    })
}

impl Server {
    pub fn new() -> Server {
        Server::default()
    }

    pub fn has_session(&self) -> bool {
        self.session
    }

    pub fn pane(&self, n: u32) -> Option<&Pane> {
        self.panes.get(&n)
    }

    pub fn pane_mut(&mut self, n: u32) -> Option<&mut Pane> {
        self.panes.get_mut(&n)
    }

    /// Every pane in creation order (listed or not).
    pub fn panes(&self) -> impl Iterator<Item = &Pane> {
        self.panes.values()
    }

    /// Listed panes across all windows.
    pub fn listed_count(&self) -> usize {
        self.windows.iter().map(|w| w.panes.len()).sum()
    }

    /// A pane that `list-panes` would show, else `can't find pane`.
    pub fn listed_pane(&self, n: u32) -> Result<&Pane, String> {
        self.panes
            .get(&n)
            .filter(|p| p.state.listed())
            .ok_or_else(|| texts::cant_find_pane(&n.to_string()))
    }

    fn window_of(&self, t: WindowTarget) -> Result<&Window, String> {
        if !self.session {
            return Err(match t {
                WindowTarget::Id(_) => texts::cant_find_window(&t.spelling()),
                _ => texts::CANT_FIND_SESSION.to_string(),
            });
        }
        let found = match t {
            WindowTarget::Current => self.current.and_then(|c| self.windows.iter().find(|w| w.id == c)),
            // Every window is `swarm-view`; tmux picks the current one first.
            WindowTarget::Named => self
                .current
                .and_then(|c| self.windows.iter().find(|w| w.id == c))
                .or_else(|| self.windows.first()),
            WindowTarget::Id(n) => self.windows.iter().find(|w| w.id == n),
        };
        found.ok_or_else(|| texts::cant_find_window(&t.spelling()))
    }

    /// The window exists (for the ignored layout/option calls).
    pub fn check_window(&self, t: WindowTarget) -> Result<(), String> {
        self.window_of(t).map(|_| ())
    }

    fn alloc_pane(&mut self, window: u32, now: EpochMs) -> u32 {
        let id = self.next_pane;
        self.next_pane += 1;
        self.panes.insert(
            id,
            Pane {
                id,
                window,
                title: None,
                colour: None,
                colour_unknown: false,
                remain_on_exit: None,
                state: PaneState::Placeholder { since: now, spawn_failed: false },
                kind: PaneKind::Placeholder,
                command: None,
                created_at: now,
                epoch: 0,
                signalled: false,
                early_exit: None,
                last_proc: None,
                session: None,
            },
        );
        if let Some(w) = self.windows.iter_mut().find(|w| w.id == window) {
            w.panes.push(id);
        }
        id
    }

    fn alloc_window(&mut self) -> u32 {
        let id = self.next_window;
        self.next_window += 1;
        self.windows.push(Window { id, panes: Vec::new() });
        self.current = Some(id);
        id
    }

    /// 11a.
    pub fn has_session_answer(&self) -> Answer {
        if self.session {
            Ok(String::new())
        } else {
            Err(texts::CANT_FIND_SESSION.to_string())
        }
    }

    /// 11b: session, window `@<n>`, placeholder pane.
    pub fn new_session(&mut self, now: EpochMs) -> Answer {
        if self.session {
            return Err(texts::DUPLICATE_SESSION.to_string());
        }
        if self.listed_count() >= MAX_PANES {
            return Err(texts::NO_SPACE.to_string());
        }
        self.session = true;
        let w = self.alloc_window();
        let p = self.alloc_pane(w, now);
        Ok(format!("%{p}\n"))
    }

    /// 12a.
    pub fn list_windows(&self) -> Answer {
        if !self.session {
            return Err(texts::CANT_FIND_SESSION.to_string());
        }
        Ok(self.windows.iter().map(|_| format!("{}\n", subcommands::WINDOW)).collect())
    }

    /// 12b.
    pub fn new_window(&mut self, now: EpochMs) -> Answer {
        if !self.session {
            return Err(texts::CANT_FIND_SESSION.to_string());
        }
        if self.listed_count() >= MAX_PANES {
            return Err(texts::NO_SPACE.to_string());
        }
        let w = self.alloc_window();
        let p = self.alloc_pane(w, now);
        Ok(format!("%{p}\n"))
    }

    /// 4.
    pub fn list_panes(&self, t: WindowTarget) -> Answer {
        let w = self.window_of(t)?;
        Ok(w.panes.iter().map(|p| format!("%{p}\n")).collect())
    }

    /// 5/6: a placeholder in the target's window; focus never moves.
    pub fn split_window(&mut self, target: u32, now: EpochMs) -> Answer {
        let window = self.listed_pane(target)?.window;
        if self.listed_count() >= MAX_PANES {
            return Err(texts::NO_SPACE.to_string());
        }
        let p = self.alloc_pane(window, now);
        Ok(format!("%{p}\n"))
    }

    /// 7/14. Returns a deviation note for a colour outside the map or an
    /// unknown `remain-on-exit` value (the call still answers 0).
    pub fn set_pane_option(&mut self, target: u32, option: PaneOption, value: &str) -> Result<Option<String>, String> {
        self.listed_pane(target)?;
        let pane = self.panes.get_mut(&target).expect("listed pane exists");
        let mut note = None;
        match option {
            PaneOption::WindowStyle | PaneOption::PaneBorderStyle | PaneOption::PaneActiveBorderStyle => {
                match colour_of(value) {
                    Some(Ok(c)) => {
                        pane.colour = Some(c);
                        pane.colour_unknown = false;
                    }
                    Some(Err(())) => {
                        pane.colour_unknown = true;
                        note = Some("unknown shim pane colour".to_string());
                    }
                    None => {}
                }
            }
            // Stored only as "seen"; the tile draws its own label.
            PaneOption::PaneBorderFormat => {}
            PaneOption::RemainOnExit => match RemainOnExit::parse(value) {
                Some(r) => pane.remain_on_exit = Some(r),
                None => note = Some("unknown shim remain-on-exit value".to_string()),
            },
        }
        Ok(note)
    }

    /// 8: the title is sanitized (bidi, ANSI) and capped at 64 characters.
    pub fn set_title(&mut self, target: u32, title: &str) -> Answer {
        self.listed_pane(target)?;
        let clean = crate::render::sanitize::sanitize_display_text(
            title,
            crate::contract::display::TextCap::Chars(subcommands::MAX_TITLE_CHARS),
        );
        let pane = self.panes.get_mut(&target).expect("listed pane exists");
        pane.title = (!clean.text.is_empty()).then_some(clean.text);
        Ok(String::new())
    }

    /// Take a pane out of `list-panes`; an empty window is destroyed, and the
    /// session with its last window.
    pub fn unlist(&mut self, n: u32) {
        for w in &mut self.windows {
            w.panes.retain(|p| *p != n);
        }
        self.windows.retain(|w| !w.panes.is_empty());
        if self.current.is_some_and(|c| !self.windows.iter().any(|w| w.id == c)) {
            self.current = self.windows.last().map(|w| w.id);
        }
        if self.windows.is_empty() {
            self.session = false;
            self.current = None;
        }
        self.trim_history();
    }

    /// Keep at most [`MAX_HISTORY`] unlisted panes (oldest dropped first).
    fn trim_history(&mut self) {
        let unlisted: Vec<u32> = self
            .panes
            .values()
            .filter(|p| !p.state.listed())
            .map(|p| p.id)
            .collect();
        if unlisted.len() > MAX_HISTORY {
            for id in &unlisted[..unlisted.len() - MAX_HISTORY] {
                self.panes.remove(id);
            }
        }
    }

    /// Whether `n` was ever allocated by this server (and is still in its
    /// history): `kill-pane` on such a pane answers 0 even when it is gone.
    pub fn knows(&self, n: u32) -> bool {
        self.panes.contains_key(&n)
    }

    /// Panes whose process runs (for closing).
    pub fn running(&self) -> Vec<(u32, PtyId)> {
        self.panes
            .values()
            .filter_map(|p| match &p.state {
                PaneState::Running { pty, .. } => Some((p.id, pty.clone())),
                _ => None,
            })
            .collect()
    }

    /// Any pane still listed or starting.
    pub fn has_live_panes(&self) -> bool {
        self.panes.values().any(|p| p.state.listed())
    }

    pub fn views(&self) -> Vec<PaneView> {
        self.panes
            .values()
            .map(|p| PaneView {
                id: PaneId::new(p.id),
                window: format!("@{}", p.window),
                listed: p.state.listed(),
                title: p.title.clone(),
                colour: p.colour,
                colour_unknown: p.colour_unknown,
                remain_on_exit: p.remain_on_exit,
                state: p.state.clone(),
                kind: p.kind.clone(),
                command: p.command.clone(),
                created_at: p.created_at,
                session: p.session.clone(),
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// the pane table (the hosted-team input of World's Claude adapter, §7.2.1)
// ---------------------------------------------------------------------------

/// One pane as the rest of the core sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct PaneView {
    pub id: PaneId,
    /// `@<n>`.
    pub window: String,
    /// What `list-panes` shows.
    pub listed: bool,
    pub title: Option<String>,
    pub colour: Option<IdentityColour>,
    pub colour_unknown: bool,
    pub remain_on_exit: Option<RemainOnExit>,
    pub state: PaneState,
    pub kind: PaneKind,
    pub command: Option<PaneCommand>,
    pub created_at: EpochMs,
    /// The member's session row (teammate panes).
    pub session: Option<SessionUid>,
}

/// Where a hosted lead is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LeadPhase {
    /// `lead_env` minted its token; the process is not registered yet.
    Pending,
    Running,
    /// The lead exited into a `kind:"bg"` job: its panes stay, its later
    /// tmux calls are refused (§7.4.7) [F-19].
    Backgrounded,
    /// The lead is gone and every pane was closed; the server is dropped.
    Ended,
}

/// The full pane table of one hosted lead, sent after every change.
#[derive(Debug, Clone, PartialEq)]
pub struct LeadPaneTable {
    pub lead: SessionUid,
    pub lead_pid: Option<i32>,
    pub phase: LeadPhase,
    /// A real `tmux -L claude-swarm-<pid>` server exists for this lead (the
    /// shim fell back): `paneHosting = OutsideGilde`, ux-ia #104 (§7.4.9).
    pub outside_gilde: bool,
    /// The `claude-swarm` session exists.
    pub session: bool,
    /// Creation order, unlisted history included.
    pub panes: Vec<PaneView>,
    pub at: EpochMs,
}

/// Receives pane tables (World's Claude team adapter, wired by T09b). Bounded
/// and non-blocking like every sink: it must not call back into PaneHost.
pub trait PaneTableSink: Send + Sync {
    fn table(&self, table: LeadPaneTable);
}

// ---------------------------------------------------------------------------
// the service
// ---------------------------------------------------------------------------

/// The composed base env of a lead's account (§6.0): T09b wires it to the
/// supervisor's `compose_env` for the lead's account. PaneHost adds its own
/// keys on top (§7.4.1) and, for panes, removes the token (§7.4.5).
pub trait LeadEnvSource: Send + Sync {
    fn base_env(&self, lead: &SessionUid) -> Result<BTreeMap<String, String>, IpcError>;
}

/// The pane member's session row in World (A18.1): bind the pane process's
/// spawn id to a session uid and record its PTY id, so the row is In Gilde ·
/// Terminal with that PTY. T09b implements it over `Arc<World>`
/// (`bind_spawn(spawn_id, None, Claude, account)` then
/// `set_control_detail(uid, {pty, transport: Pty, hosted_team: false})`).
/// Called for teammate panes only (unknown panes are never sessions).
pub trait PaneSessions: Send + Sync {
    fn pane_spawned(&self, spawn_id: &str, account: Option<AccountId>, pty: &PtyId) -> Option<SessionUid>;
}

/// No World binding (tests that do not look at session rows).
pub struct NoPaneSessions;

impl PaneSessions for NoPaneSessions {
    fn pane_spawned(&self, _spawn_id: &str, _account: Option<AccountId>, _pty: &PtyId) -> Option<SessionUid> {
        None
    }
}

/// Whether an exited lead moved to the background (§7.4.7, P39): a
/// `kind:"bg"` registry entry with its session id appeared within 3 s.
#[async_trait]
pub trait LeadFate: Send + Sync {
    async fn moved_to_background(&self, lead: &SessionUid) -> bool;
}

/// The default [`LeadFate`]: World shows the lead row as Watching ·
/// Background (discovery's `kind:"bg"` registry entry) within the window.
pub struct WorldLeadFate {
    world: Arc<dyn WorldApi>,
    window: Duration,
    step: Duration,
}

impl WorldLeadFate {
    pub fn new(world: Arc<dyn WorldApi>) -> WorldLeadFate {
        WorldLeadFate { world, window: BACKGROUND_WINDOW, step: Duration::from_millis(100) }
    }
}

#[async_trait]
impl LeadFate for WorldLeadFate {
    async fn moved_to_background(&self, lead: &SessionUid) -> bool {
        let start = Instant::now();
        loop {
            let backgrounded = self.world.session(lead).is_some_and(|s| {
                matches!(s.control, Control::Watching { origin: Origin::Background, .. })
            });
            if backgrounded {
                return true;
            }
            if start.elapsed() >= self.window {
                return false;
            }
            tokio::time::sleep(self.step).await;
        }
    }
}

/// Where the service lives on disk and who it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneHostConfig {
    pub socket_path: PathBuf,
    /// `<app_data>/shim/<buildId>` (contains `tmux`).
    pub shim_dir: PathBuf,
    /// `<app_data>/shim` (every PATH entry below it is a shim dir).
    pub shim_root: PathBuf,
    pub build_id: String,
    pub app_uid: u32,
    /// How long a `respawn-pane` waits for its process before answering
    /// `failed to spawn` (below the shim's 10 s).
    pub spawn_deadline: Duration,
}

impl PaneHostConfig {
    /// The configuration for an app data dir (§4.6 socket rule, §4.7 shim dir).
    pub fn for_paths(paths: &crate::paths::AppPaths, build_id: &str) -> PaneHostConfig {
        let app_uid = auth::app_uid();
        PaneHostConfig {
            socket_path: crate::panehost::listener::socket_path(paths.app_data_dir(), app_uid),
            shim_dir: paths.shim_dir(build_id),
            shim_root: paths.shim_root().to_path_buf(),
            build_id: build_id.to_string(),
            app_uid,
            spawn_deadline: crate::panehost::panes::SPAWN_DEADLINE,
        }
    }
}

/// What PaneHost consumes (contract §15.2 T07 row).
#[derive(Clone)]
pub struct PaneHostDeps {
    pub pty: Arc<dyn PtyApi>,
    pub world: Arc<dyn WorldApi>,
    pub compat: Arc<dyn CompatSink>,
    pub env: Arc<dyn LeadEnvSource>,
    pub fate: Arc<dyn LeadFate>,
    /// A18.1: the pane member's session row (T09b: World).
    pub sessions: Arc<dyn PaneSessions>,
}

/// One hosted lead.
pub(crate) struct Lead {
    pub(crate) token: Zeroizing<String>,
    pub(crate) pid: Option<i32>,
    pub(crate) start_sec: Option<i64>,
    /// `proc_pidpath` of the lead at `lead_started` [N-07].
    pub(crate) pidpath: Option<String>,
    pub(crate) phase: LeadPhase,
    pub(crate) server: Option<Server>,
    pub(crate) rate: RateWindow,
    pub(crate) deviated: bool,
    pub(crate) outside_gilde: bool,
    /// Serializes this lead's requests (and its exit) across awaits.
    pub(crate) op: Arc<tokio::sync::Mutex<()>>,
    /// The `LeadEnv.set` pairs except the token: what a pane adds to the base.
    pub(crate) lead_set: Vec<(String, String)>,
}

/// The PaneHost service: one per app; one [`Server`] per hosted lead.
pub struct PaneHost {
    pub(crate) cfg: PaneHostConfig,
    pub(crate) deps: PaneHostDeps,
    pub(crate) leads: Mutex<HashMap<SessionUid, Lead>>,
    pub(crate) counters: Counters,
    pub(crate) table_sink: RwLock<Option<Arc<dyn PaneTableSink>>>,
    pub(crate) listener: Mutex<Option<crate::panehost::listener::Running>>,
    pub(crate) sweep: Mutex<crate::panehost::sweep::SweepState>,
    /// Whether this host can serve leads (the socket dir passed its checks
    /// and the listener is bound). `lead_env` refuses otherwise, so the lead
    /// runs its teammates in-process.
    pub(crate) hosting: std::sync::atomic::AtomicBool,
    pub(crate) me: Weak<PaneHost>,
}

/// What a request turned into, plus the side effects to run after the lock.
#[derive(Default)]
pub(crate) struct Effects {
    pub(crate) table_changed: bool,
    pub(crate) deviation: Option<String>,
    pub(crate) note: Option<String>,
}

impl PaneHost {
    pub fn new(cfg: PaneHostConfig, deps: PaneHostDeps) -> Arc<PaneHost> {
        Arc::new_cyclic(|me| PaneHost {
            cfg,
            deps,
            leads: Mutex::new(HashMap::new()),
            counters: Counters::default(),
            table_sink: RwLock::new(None),
            listener: Mutex::new(None),
            sweep: Mutex::new(Default::default()),
            hosting: std::sync::atomic::AtomicBool::new(false),
            me: me.clone(),
        })
    }

    pub fn config(&self) -> &PaneHostConfig {
        &self.cfg
    }

    pub fn counters(&self) -> &Counters {
        &self.counters
    }

    pub fn world(&self) -> &Arc<dyn WorldApi> {
        &self.deps.world
    }

    /// Register the pane-table receiver (T09b: World's Claude adapter).
    pub fn set_table_sink(&self, sink: Arc<dyn PaneTableSink>) {
        *self.table_sink.write() = Some(sink);
    }

    /// The current table of every hosted lead.
    pub fn tables(&self) -> Vec<LeadPaneTable> {
        let leads = self.leads.lock();
        leads.iter().map(|(uid, l)| table_of(uid, l)).collect()
    }

    /// The current table of one lead.
    pub fn table(&self, lead: &SessionUid) -> Option<LeadPaneTable> {
        self.leads.lock().get(lead).map(|l| table_of(lead, l))
    }

    pub(crate) fn emit_table(&self, lead: &SessionUid) {
        let Some(table) = self.table(lead) else { return };
        let sink = self.table_sink.read().clone();
        if let Some(sink) = sink {
            sink.table(table);
        }
    }

    pub(crate) fn emit_ended(&self, lead: &SessionUid, lead_pid: Option<i32>) {
        let sink = self.table_sink.read().clone();
        if let Some(sink) = sink {
            sink.table(LeadPaneTable {
                lead: lead.clone(),
                lead_pid,
                phase: LeadPhase::Ended,
                outside_gilde: false,
                session: false,
                panes: Vec::new(),
                at: now_ms(),
            });
        }
    }

    /// The Claude version of a lead's session, for compat records.
    pub(crate) fn lead_version(&self, lead: &SessionUid) -> Option<String> {
        let s = self.deps.world.session(lead)?;
        s.claude_version.or_else(|| s.binary.and_then(|b| b.version))
    }

    /// Record the first deviation of a lead (§7.4.9): `paneHosting` →
    /// `Observed/Failed` for its version. Only ever called for an authorized
    /// call from the lead process itself [N-02].
    pub(crate) fn record_deviation(&self, lead: &SessionUid, note: String) {
        let first = {
            let mut leads = self.leads.lock();
            match leads.get_mut(lead) {
                Some(l) if !l.deviated => {
                    l.deviated = true;
                    true
                }
                _ => false,
            }
        };
        tracing::warn!(target: "panehost", note = %note, first, "pane hosting deviation");
        if !first {
            return;
        }
        match self.lead_version(lead) {
            Some(version) => self.deps.compat.deviation(&version, CompatCapability::PaneHosting, note),
            None => tracing::warn!(target: "panehost", "deviation not recorded: the lead's claude version is unknown"),
        }
    }

    /// A deviation note that does not fail the capability (an unknown option
    /// name or colour: the call answered 0).
    pub(crate) fn record_note(&self, lead: &SessionUid, note: String) {
        tracing::info!(target: "panehost", note = %note, "pane hosting note");
        if let Some(version) = self.lead_version(lead) {
            self.deps.compat.note(&version, note);
        }
    }

    /// Every refused request is counted (one counter per reason, summed by
    /// `Counters::total_refusals`) and logged without content (A33.7).
    pub(crate) fn refuse(&self, why: Refusal, peer: &PeerCred, sub: &str) -> ShimResponse {
        self.counters.refused(why);
        tracing::warn!(
            target: "panehost::security",
            reason = why.as_str(),
            peer_pid = peer.pid,
            subcommand = %subcommands::display_name(sub),
            "shim call refused"
        );
        match why {
            Refusal::VersionMismatch => ShimResponse::version_mismatch(),
            Refusal::RateLimited => ShimResponse::fail(proto::messages::BUSY),
            Refusal::BadFrame => ShimResponse::fail(extra::BAD_REQUEST),
            _ => ShimResponse::not_authorized(),
        }
    }

    /// A call whose lead ended between the checks and the call itself (a
    /// race): refused as `LeadGone`, counted and logged like every refusal
    /// (A33.7).
    pub(crate) fn refuse_vanished(&self, sub: &str) -> ShimResponse {
        self.counters.refused(Refusal::LeadGone);
        tracing::warn!(
            target: "panehost::security",
            reason = Refusal::LeadGone.as_str(),
            subcommand = %subcommands::display_name(sub),
            "shim call refused"
        );
        ShimResponse::not_authorized()
    }

    /// The lead holding `token` (constant-time compare against every lead).
    fn lead_for_token(&self, token: &str) -> Option<SessionUid> {
        let leads = self.leads.lock();
        let mut found = None;
        for (uid, l) in leads.iter() {
            if auth::token_eq(&l.token, token) && l.phase != LeadPhase::Ended {
                found = Some(uid.clone());
            }
        }
        found
    }

    /// Answer one request line from a peer (§4.6). Never panics; every path
    /// answers exactly once.
    pub async fn handle_line(&self, line: &[u8], peer: PeerCred) -> ShimResponse {
        // (1) peer uid.
        if peer.uid != self.cfg.app_uid {
            return self.refuse(Refusal::ForeignUid, &peer, "");
        }
        // (2) the frame.
        let req = match ShimRequest::parse_line(line) {
            Ok(req) => req,
            Err(_) => return self.refuse(Refusal::BadFrame, &peer, ""),
        };
        self.handle(req, peer).await
    }

    /// Answer one parsed request (checks 3–7, then the call).
    pub async fn handle(&self, req: ShimRequest, peer: PeerCred) -> ShimResponse {
        if peer.uid != self.cfg.app_uid {
            return self.refuse(Refusal::ForeignUid, &peer, "");
        }
        if req.check_form().is_err() {
            return self.refuse(Refusal::BadFrame, &peer, "");
        }
        let sub = sub_of(&req.argv).to_string();
        // (3) version: the update message, never a failed version.
        if req.v != proto::SHIM_PROTOCOL_V || req.build != self.cfg.build_id {
            return self.refuse(Refusal::VersionMismatch, &peer, &sub);
        }
        // (4) token.
        let Some(lead) = self.lead_for_token(&req.token) else {
            return self.refuse(Refusal::Token, &peer, &sub);
        };
        let version_query = proto::is_version_query(&req.argv);
        let swarm = proto::swarm_target(&req.argv).map(str::to_string);
        if !version_query && swarm.is_none() {
            return self.refuse(Refusal::BadFrame, &peer, &sub);
        }
        // (5) lead pid and identity, (6) rate.
        let lead_pid = {
            let mut leads = self.leads.lock();
            let Some(l) = leads.get_mut(&lead) else {
                drop(leads);
                return self.refuse(Refusal::Token, &peer, &sub);
            };
            if !version_query {
                let (Some(pid), LeadPhase::Running) = (l.pid, l.phase) else {
                    let why = if l.phase == LeadPhase::Backgrounded { Refusal::LeadPid } else { Refusal::LeadGone };
                    drop(leads);
                    return self.refuse(why, &peer, &sub);
                };
                if swarm.as_deref() != Some(pid.to_string().as_str()) {
                    drop(leads);
                    return self.refuse(Refusal::LeadPid, &peer, &sub);
                }
            }
            if !l.rate.admit(Instant::now()) {
                drop(leads);
                return self.refuse(Refusal::RateLimited, &peer, &sub);
            }
            l.pid
        };
        if version_query {
            return ShimResponse::ok(proto::TMUX_VERSION_LINE);
        }
        let lead_pid = lead_pid.expect("checked above");
        let recorded_start = self.leads.lock().get(&lead).and_then(|l| l.start_sec);
        if let Some(start) = recorded_start {
            if auth::start_of(lead_pid) != Some(start) {
                return self.refuse(Refusal::LeadGone, &peer, &sub);
            }
        }
        // (7) kernel peer pid parent check for mutating calls [F-07].
        let from_lead = auth::parent_of(peer.pid) == Some(lead_pid);
        if auth::is_mutating(&sub) && !from_lead {
            return self.refuse(Refusal::NotParent, &peer, &sub);
        }
        match subcommands::parse(&req.argv[2..]) {
            Err(ParseError::Unsupported { name, note }) => {
                // Only an authorized call from the lead itself is a deviation:
                // a token-bearing child of the lead cannot disable pane
                // hosting for the version [N-02].
                if from_lead {
                    self.record_deviation(&lead, note);
                } else {
                    tracing::warn!(target: "panehost", subcommand = %name, "unsupported call from a lead's child (not a deviation)");
                }
                ShimResponse::fail(format!("{}{name}\n", extra::UNSUPPORTED_PREFIX))
            }
            Err(ParseError::Refused { stderr }) => ShimResponse::fail(stderr),
            Ok(cmd) => self.execute(&lead, cmd, &req.cwd).await,
        }
    }

    /// Run one call under the lead's operation lock.
    async fn execute(&self, lead: &SessionUid, cmd: Cmd, cwd: &str) -> ShimResponse {
        let op = self.leads.lock().get(lead).map(|l| l.op.clone());
        let Some(op) = op else {
            return self.refuse_vanished(cmd.name());
        };
        let _turn = op.lock().await;
        match cmd {
            Cmd::RespawnPane { target, command } => self.respawn(lead, target, command, cwd).await,
            Cmd::KillPane { target } => self.kill_pane(lead, target),
            other => self.execute_model(lead, other),
        }
    }

    /// The calls that only change the model.
    fn execute_model(&self, lead: &SessionUid, cmd: Cmd) -> ShimResponse {
        let now = now_ms();
        let mut fx = Effects::default();
        let answer: Answer = {
            let mut leads = self.leads.lock();
            let Some(l) = leads.get_mut(lead) else {
                drop(leads);
                return self.refuse_vanished(cmd.name());
            };
            // A server is created lazily on the lead's first call.
            let server = l.server.get_or_insert_with(Server::new);
            match cmd {
                Cmd::HasSession => server.has_session_answer(),
                Cmd::NewSession => {
                    fx.table_changed = true;
                    server.new_session(now)
                }
                Cmd::ListWindows => server.list_windows(),
                Cmd::NewWindow => {
                    fx.table_changed = true;
                    server.new_window(now)
                }
                Cmd::ListPanes { target } => server.list_panes(target),
                Cmd::SplitWindow { target, .. } => {
                    fx.table_changed = true;
                    server.split_window(target, now)
                }
                Cmd::SetPaneOption { target, option, value } => match server.set_pane_option(target, option, &value) {
                    Ok(note) => {
                        fx.table_changed = true;
                        fx.note = note;
                        Ok(String::new())
                    }
                    Err(e) => Err(e),
                },
                Cmd::SetPaneUnknownOption { target, name } => server.listed_pane(target).map(|_| {
                    fx.note = Some(format!("unknown shim pane option: {name}"));
                    String::new()
                }),
                Cmd::SetWindowOption { target, known, name } => server.check_window(target).map(|_| {
                    if !known {
                        fx.note = Some(format!("unknown shim window option: {name}"));
                    }
                    String::new()
                }),
                Cmd::SelectPaneTitle { target, title } => {
                    fx.table_changed = true;
                    server.set_title(target, &title)
                }
                Cmd::SelectLayout { target } => server.check_window(target).map(|_| String::new()),
                Cmd::ResizePane { target } => server.listed_pane(target).map(|_| String::new()),
                Cmd::RespawnPane { .. } | Cmd::KillPane { .. } => Err(texts::RESPAWN_FAILED.to_string()),
            }
        };
        self.apply_effects(lead, fx);
        match answer {
            Ok(stdout) => ShimResponse::ok(stdout),
            Err(stderr) => ShimResponse::fail(stderr),
        }
    }

    pub(crate) fn apply_effects(&self, lead: &SessionUid, fx: Effects) {
        if fx.table_changed {
            self.emit_table(lead);
        }
        if let Some(note) = fx.note {
            self.record_note(lead, note);
        }
        if let Some(dev) = fx.deviation {
            self.record_deviation(lead, dev);
        }
    }

    /// Whether a socket is bound and leads can be hosted.
    pub fn is_hosting(&self) -> bool {
        self.hosting.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Mint a lead's token and env (§7.4.1). Refused when hosting is
    /// unavailable (the socket dir failed its checks, or no shim is
    /// installed): the lead then runs teammates in-process.
    pub fn lead_env_for(&self, lead: &SessionUid) -> Result<LeadEnv, IpcError> {
        if !self.is_hosting() {
            return Err(IpcError::unsupported("paneHosting"));
        }
        let shim = self.cfg.shim_dir.join("tmux");
        if !crate::panehost::lead_env::is_executable_file(&shim) {
            return Err(IpcError::unsupported("paneHosting"));
        }
        let base = self.deps.env.base_env(lead)?;
        let base_path = base.get("PATH").cloned().unwrap_or_default();
        let real_tmux = crate::panehost::lead_env::find_real_tmux(&base_path, &self.cfg.shim_root);
        let token = auth::mint_token();
        let env = crate::panehost::lead_env::build(
            &self.cfg.socket_path,
            &self.cfg.shim_dir,
            &self.cfg.shim_root,
            &token,
            &base_path,
            real_tmux.as_deref(),
        );
        let lead_set: Vec<(String, String)> = env
            .set
            .iter()
            .filter(|(k, _)| k != proto::ENV_PANE_HOST_TOKEN)
            .cloned()
            .collect();
        let replaced = {
            let mut leads = self.leads.lock();
            leads.insert(
                lead.clone(),
                Lead {
                    token,
                    pid: None,
                    start_sec: None,
                    pidpath: None,
                    phase: LeadPhase::Pending,
                    server: None,
                    rate: RateWindow::default(),
                    deviated: false,
                    outside_gilde: false,
                    op: Arc::new(tokio::sync::Mutex::new(())),
                    lead_set,
                },
            )
        };
        // A new lead for the same session (a restart): the old token is
        // revoked and the old server's panes are closed.
        if let Some(old) = replaced {
            if let Some(server) = old.server {
                let running = server.running();
                if !running.is_empty() {
                    tracing::warn!(target: "panehost", panes = running.len(), "a lead was replaced: closing its panes");
                    self.close_detached(running, crate::panehost::panes::KILL_GRACE_MS);
                }
            }
        }
        tracing::info!(target: "panehost", "hosted lead env minted");
        Ok(env)
    }

    /// The lead process exists (`pid` is what `claude-swarm-<pid>` names).
    pub fn lead_started_at(&self, lead: &SessionUid, pid: i32) {
        let start = auth::start_of(pid);
        let pidpath = crate::supervise::procscan::exe_path_of(pid);
        let known = {
            let mut leads = self.leads.lock();
            match leads.get_mut(lead) {
                Some(l) => {
                    l.pid = Some(pid);
                    l.start_sec = start;
                    l.pidpath = pidpath;
                    l.phase = LeadPhase::Running;
                    true
                }
                None => false,
            }
        };
        if known {
            self.emit_table(lead);
        } else {
            tracing::warn!(target: "panehost", "lead_started for a lead without an env");
        }
    }

    /// Close `panes` in the background (no answer waits for it).
    pub(crate) fn close_detached(&self, panes: Vec<(u32, PtyId)>, grace_ms: u32) {
        let pty = self.deps.pty.clone();
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(target: "panehost", "no runtime to close panes on");
            return;
        };
        rt.spawn(async move {
            for (_, id) in panes {
                let _ = pty
                    .close(&id, crate::contract::services::SignalPlan::HupThenTerm { grace_ms })
                    .await;
            }
        });
    }
}

/// The subcommand word of a request's argv (for logs: never values).
fn sub_of(argv: &[String]) -> &str {
    if proto::swarm_target(argv).is_some() {
        argv.get(2).map(String::as_str).unwrap_or("")
    } else {
        argv.first().map(String::as_str).unwrap_or("")
    }
}

fn table_of(uid: &SessionUid, l: &Lead) -> LeadPaneTable {
    LeadPaneTable {
        lead: uid.clone(),
        lead_pid: l.pid,
        phase: l.phase,
        outside_gilde: l.outside_gilde,
        session: l.server.as_ref().is_some_and(Server::has_session),
        panes: l.server.as_ref().map(Server::views).unwrap_or_default(),
        at: now_ms(),
    }
}

#[async_trait]
impl PaneHostApi for PaneHost {
    fn lead_env(&self, lead: &SessionUid) -> Result<LeadEnv, IpcError> {
        self.lead_env_for(lead)
    }

    fn lead_started(&self, lead: &SessionUid, pid: i32) {
        self.lead_started_at(lead, pid);
    }

    async fn lead_exited(&self, lead: &SessionUid) {
        self.lead_gone(lead).await;
    }

    async fn send_to_member(&self, team: &TeamKey, member: &MemberKey, text: &str) -> Result<(), IpcError> {
        self.send_to_member_pane(team, member, text).await
    }

    async fn close_member_pane(&self, team: &TeamKey, member: &MemberKey) -> Result<(), IpcError> {
        self.close_member(team, member, None).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: EpochMs = 1_761_000_000_000;

    #[test]
    fn the_first_has_session_finds_nothing_then_new_session_creates_pane_0() {
        let mut s = Server::new();
        assert_eq!(s.has_session_answer(), Err("can't find session: claude-swarm\n".into()));
        assert_eq!(s.new_session(NOW), Ok("%0\n".into()));
        assert_eq!(s.has_session_answer(), Ok(String::new()));
        assert_eq!(s.new_session(NOW), Err("duplicate session: claude-swarm\n".into()));
        assert_eq!(s.list_windows(), Ok("swarm-view\n".into()));
        assert_eq!(s.list_panes(WindowTarget::Named), Ok("%0\n".into()));
        assert_eq!(s.list_panes(WindowTarget::Current), Ok("%0\n".into()));
        assert_eq!(s.list_panes(WindowTarget::Id(0)), Ok("%0\n".into()));
        assert_eq!(s.list_panes(WindowTarget::Id(7)), Err("can't find window: @7\n".into()));
    }

    #[test]
    fn without_a_session_the_queries_fail_like_tmux() {
        let s = Server::new();
        assert_eq!(s.list_windows(), Err("can't find session: claude-swarm\n".into()));
        assert_eq!(s.list_panes(WindowTarget::Named), Err("can't find session: claude-swarm\n".into()));
        assert_eq!(s.list_panes(WindowTarget::Id(0)), Err("can't find window: @0\n".into()));
        let mut s = Server::new();
        assert_eq!(s.new_window(NOW), Err("can't find session: claude-swarm\n".into()));
    }

    #[test]
    fn splits_list_in_creation_order_and_the_16th_pane_is_refused() {
        let mut s = Server::new();
        s.new_session(NOW).unwrap();
        for n in 1..MAX_PANES as u32 {
            assert_eq!(s.split_window(0, NOW), Ok(format!("%{n}\n")));
        }
        assert_eq!(s.listed_count(), MAX_PANES);
        assert_eq!(s.split_window(0, NOW), Err("no space for new pane\n".into()));
        assert_eq!(s.new_window(NOW), Err("no space for new pane\n".into()));
        let listed = s.list_panes(WindowTarget::Named).unwrap();
        let expect: String = (0..MAX_PANES).map(|n| format!("%{n}\n")).collect();
        assert_eq!(listed, expect);
    }

    #[test]
    fn ids_are_never_reused_within_a_server() {
        let mut s = Server::new();
        s.new_session(NOW).unwrap();
        s.split_window(0, NOW).unwrap(); // %1
        s.pane_mut(1).unwrap().state = PaneState::Killed { at: NOW };
        s.unlist(1);
        assert_eq!(s.split_window(0, NOW), Ok("%2\n".into()));
        // The last pane goes: window and session are destroyed; ids go on.
        for n in [0, 2] {
            s.pane_mut(n).unwrap().state = PaneState::Killed { at: NOW };
            s.unlist(n);
        }
        assert!(!s.has_session());
        assert_eq!(s.has_session_answer(), Err("can't find session: claude-swarm\n".into()));
        assert_eq!(s.new_session(NOW), Ok("%3\n".into()));
        assert_eq!(s.list_panes(WindowTarget::Id(1)), Ok("%3\n".into()), "window ids go on too");
        assert!(s.knows(1) && s.knows(0));
    }

    #[test]
    fn dead_but_kept_panes_stay_listed() {
        let mut s = Server::new();
        s.new_session(NOW).unwrap();
        s.split_window(0, NOW).unwrap();
        s.pane_mut(1).unwrap().state = PaneState::Exited { code: Some(1), signal: None, at: NOW, kept: true };
        assert_eq!(s.list_panes(WindowTarget::Named), Ok("%0\n%1\n".into()));
        s.pane_mut(0).unwrap().state = PaneState::Exited { code: Some(0), signal: None, at: NOW, kept: false };
        s.unlist(0);
        assert_eq!(s.list_panes(WindowTarget::Named), Ok("%1\n".into()));
    }

    #[test]
    fn colours_map_like_the_lead_names_them() {
        assert_eq!(colour_of("bg=default,fg=blue"), Some(Ok(IdentityColour::Blue)));
        assert_eq!(colour_of("fg=magenta"), Some(Ok(IdentityColour::Purple)));
        assert_eq!(colour_of("fg=colour208"), Some(Ok(IdentityColour::Orange)));
        assert_eq!(colour_of("fg=colour205"), Some(Ok(IdentityColour::Pink)));
        for c in ["red", "green", "yellow", "cyan"] {
            assert!(matches!(colour_of(&format!("fg={c}")), Some(Ok(_))), "{c}");
        }
        assert_eq!(colour_of("fg=colour99"), Some(Err(())));
        assert_eq!(colour_of("bg=default"), None);
    }

    #[test]
    fn options_are_stored_and_odd_values_are_notes() {
        let mut s = Server::new();
        s.new_session(NOW).unwrap();
        assert_eq!(s.set_pane_option(0, PaneOption::WindowStyle, "bg=default,fg=green"), Ok(None));
        assert_eq!(s.pane(0).unwrap().colour, Some(IdentityColour::Green));
        assert!(s.set_pane_option(0, PaneOption::PaneBorderStyle, "fg=colour99").unwrap().is_some());
        assert!(s.pane(0).unwrap().colour_unknown);
        assert_eq!(s.set_pane_option(0, PaneOption::RemainOnExit, "failed"), Ok(None));
        assert_eq!(s.pane(0).unwrap().remain_on_exit, Some(RemainOnExit::Failed));
        assert!(s.set_pane_option(0, PaneOption::RemainOnExit, "sometimes").unwrap().is_some());
        assert_eq!(s.set_pane_option(9, PaneOption::RemainOnExit, "on"), Err("can't find pane: %9\n".into()));
    }

    #[test]
    fn titles_are_sanitized_and_capped() {
        let mut s = Server::new();
        s.new_session(NOW).unwrap();
        s.set_title(0, "alpha").unwrap();
        assert_eq!(s.pane(0).unwrap().title.as_deref(), Some("alpha"));
        s.set_title(0, &"x".repeat(200)).unwrap();
        assert!(s.pane(0).unwrap().title.as_ref().unwrap().chars().count() <= subcommands::MAX_TITLE_CHARS);
        s.set_title(0, "a\u{202e}b").unwrap();
        assert_eq!(s.pane(0).unwrap().title.as_deref(), Some("ab"), "bidi overrides are stripped");
    }

    #[test]
    fn history_is_bounded() {
        let mut s = Server::new();
        s.new_session(NOW).unwrap();
        for _ in 0..(MAX_HISTORY + 10) {
            let out = s.split_window(0, NOW).unwrap();
            let n: u32 = out.trim().trim_start_matches('%').parse().unwrap();
            s.pane_mut(n).unwrap().state = PaneState::Killed { at: NOW };
            s.unlist(n);
        }
        let unlisted = s.panes().filter(|p| !p.state.listed()).count();
        assert_eq!(unlisted, MAX_HISTORY);
    }
}
