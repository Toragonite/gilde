//! The Claude native adapter logic (§7.2).
//!
//! Owner: T02. Observe-only; acting goes through the lead (prefill) or a
//! hosted member pane (PaneHost). Inputs (all read-only, produced elsewhere):
//! * T03's [`TeamObserver`] observations of `teams/<dir>/` (roster, inbox
//!   frames without text, task files, the lead join, live pane processes,
//!   in-process metas) [A11.1];
//! * T04's [`RowSink`] structure rows of followed transcripts (lead and pane
//!   members): `SendMessage` calls and their `msg_id`, delivered
//!   `<teammate-message>` blocks, task tool calls, tool-set attachment names,
//!   turn ends incl. the interrupt marker [A10.5].
//!
//! It turns them into [`TeamEvent`]s for World. Rules it pins:
//! * member keys include `joined_at` (reused names never merge); a resumed
//!   lead's new team dir is a new team (never merged);
//! * presence: pane member = in the roster ∧ a live process with its
//!   `--agent-id`; in-process member = the lead is alive;
//! * the message log: Sent from the sender's `SendMessage` (joined by the
//!   result's `msg_id`, frozen LP-16), Delivered from the recipient's
//!   turn-opening `<teammate-message>` blocks, Queued from unread inbox
//!   entries; joined by order and a 120 s window with `derived: true` when no
//!   `msg_id` exists; messages to a gone (closed) pane stay Queued;
//!   counts are never "no messages" (`observe.messages = Partial` with the
//!   `team.messagesUnknown` note);
//! * the board: task tools available (a `TaskCreate`/`TaskUpdate`/`TaskList`
//!   name in the tool-set attachments, any task row, or task files) → the
//!   transcript/file board (`observe.tasks = Yes`); otherwise inferred items
//!   are the board (`Inferred`, the default on 2.1.307): a lead plain message
//!   opens an item, the member's turn-opening row starts it, its next
//!   `idle_notification`, the interrupt marker or its next turn-opening row
//!   closes every item of that turn (Reported);
//! * inbox frames are hints only (≤ 1 s): attribution of the lead's prompts,
//!   idle notifications, shutdown approvals, plan reviews, in-flight counts.
//!
//! Observation never writes (§7.1.4 rule 1, T-team-nowrite): this module has
//! no file, socket or process call at all.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};

use crate::contract::common::{EpochMs, IdentityColour, Vendor};
use crate::contract::content::UserOrigin;
use crate::contract::ids::{AccountId, MemberKey, MessageKey, PaneId, RequestId, SessionUid, TaskKey, TeamKey};
use crate::contract::interaction::Decision;
use crate::contract::services::{IdentityHint, IdentityKey, SourceFacts};
use crate::contract::session::MemberRole;
use crate::contract::team::{
    ActOutcome, ApprovalRoute, Correlation, DeliveryStatus, Durability, ExternalHost, GoneReason, IdleReason, LeadBinding,
    Member, MemberBackend, MemberSpec, MessageKind, PaneHostingState, PaneTile, PaneTileState, Party, PrefillAction,
    Presence, QuestionLocation, SendRoute, StopHow, StopRoute, Support, Task, TaskOp, TaskSource, TaskStatus, TaskWrite,
    TeamAdapter, TeamCapabilities, TeamEvent, TeamEventSink, TeamMessage, TeamSource,
};
use crate::contract::activity::{EvidenceLine, GildeFacts, ProcessFacts, StopCause};
use crate::contract::session::ControlMode;
use crate::discovery::claude::teams::{InboxEntry, TaskFile};
use crate::panehost::{LeadPaneTable, LeadPhase, PaneKind, PaneState, PaneTableSink, UnknownReason, DETAILS_TEAMMATE_BINARY_DIFFERS};
use crate::discovery::reconcile::{TeamObservation, TeamObserver};
use crate::follow::tail::RowSink;
use crate::render::transcript::{RowFacts, TurnEndKind};
use crate::world::teams::assembly::{TeamDetails, TeamHooks, PANE_TILES_CAP};

/// The lead's roster name.
pub const LEAD_NAME: &str = "team-lead";
/// The join window for messages without `msg_id` (§7.2.4).
pub const JOIN_WINDOW_MS: EpochMs = 120_000;
/// The fidelity note of a native team (§7.1.2, §9.12 `team.messagesUnknown`).
pub const MESSAGES_UNKNOWN_NOTE: &str = "team.messagesUnknown";
/// A33.5(d): inbox entries applied per observation of a team, newest first;
/// older unseen ones are skipped (World keeps 5,000 messages per team).
pub const INBOX_ENTRIES_PER_CYCLE: usize = 5_000;
/// The adapter's memory of applied inbox entries.
const HINTS_CAP: usize = 20_000;
/// The adapter's memory of messages per team (the store keeps the newest 500).
const MESSAGES_CAP: usize = 5_000;

/// What one team observation cost (A33.5(d) structural counters).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InboxCycle {
    /// Entries in the observation's inboxes.
    pub entries: u32,
    /// Skipped: applied before with the same `read` flag.
    pub seen: u32,
    /// Skipped: beyond the per-cycle cap or older than every message held.
    pub over_cap: u32,
    /// Applied this cycle (≤ [`INBOX_ENTRIES_PER_CYCLE`]).
    pub processed: u32,
    /// Team events the observation produced.
    pub events: u32,
    /// World mutations (one refresh each) that applied those events.
    pub world_batches: u32,
    /// The observation ran on the discovery thread (it must not, A33.5(d)).
    pub on_discovery_thread: bool,
}

/// Counters of the adapter's team observations.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InboxStats {
    pub observations: u64,
    pub processed_total: u64,
    pub events_total: u64,
    /// The largest single cycle seen: inbox entries, applied entries, World
    /// mutations for its events.
    pub max_entries: u32,
    pub max_processed: u32,
    pub max_world_batches: u32,
    /// Any observation ran on the discovery thread.
    pub on_discovery_thread: bool,
    pub last: InboxCycle,
}

fn is_task_tool(name: &str) -> bool {
    matches!(name, "TaskCreate" | "TaskUpdate" | "TaskList")
}

/// `--agent-color` / roster `color` → identity colour (M12).
pub fn colour_of(s: &str) -> Option<IdentityColour> {
    Some(match s.to_ascii_lowercase().as_str() {
        "red" => IdentityColour::Red,
        "blue" => IdentityColour::Blue,
        "green" => IdentityColour::Green,
        "yellow" => IdentityColour::Yellow,
        "purple" => IdentityColour::Purple,
        "orange" => IdentityColour::Orange,
        "pink" => IdentityColour::Pink,
        "cyan" => IdentityColour::Cyan,
        _ => return None,
    })
}

fn message_kind_of(frame: &str) -> MessageKind {
    match frame {
        "idle_notification" => MessageKind::IdleNotification,
        "shutdown_request" => MessageKind::ShutdownRequest,
        "shutdown_approved" => MessageKind::ShutdownApproved,
        "shutdown_rejected" => MessageKind::ShutdownRejected,
        "plan_approval_request" => MessageKind::PlanApprovalRequest,
        "plan_approval_response" => MessageKind::PlanApprovalResponse,
        "permission_request" => MessageKind::PermissionRequest,
        "permission_response" => MessageKind::PermissionResponse,
        "task_assignment" => MessageKind::TaskAssignment,
        "teammate_terminated" => MessageKind::TeammateTerminated,
        _ => MessageKind::Unknown,
    }
}

fn task_status_of(s: &str) -> Option<TaskStatus> {
    Some(match s {
        "pending" | "open" => TaskStatus::Open,
        "in_progress" => TaskStatus::InProgress,
        "completed" => TaskStatus::Done,
        "deleted" => TaskStatus::Deleted,
        _ => return None,
    })
}

/// Who wrote a followed transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Who {
    Lead,
    Member(MemberKey),
}

#[derive(Debug, Clone)]
struct MemberRec {
    member: Member,
    was_live: bool,
    emitted: Option<Member>,
    /// The roster's `tmuxPaneId` (the pane link needs it and the pane table
    /// to agree, §7.4.5).
    roster_pane: Option<String>,
    /// Presence and session from the files and the process scan, before
    /// PaneHost's pane table overlays them.
    scan_presence: Presence,
    scan_session: Option<SessionUid>,
}

/// One inferred item (§7.2.5).
#[derive(Debug, Clone)]
struct Item {
    task: Task,
    /// The `msg_id` / tool-use id / row uuid that opened it.
    opened_by: String,
    emitted: Option<Task>,
}

#[derive(Debug, Clone)]
struct PendingSend {
    from: Party,
    to_name: Option<String>,
    at: EpochMs,
}

#[derive(Debug, Clone)]
struct Msg {
    msg: TeamMessage,
    emitted: Option<TeamMessage>,
    evidence: MessageEvidence,
}

/// A27.1: where the transcripts showed a message — ids only, never content.
/// `team_message_body` reads the body on demand from these places.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MessageEvidence {
    /// The sender's `SendMessage` tool_use id (joined by its result row).
    pub tool_use_id: Option<String>,
    /// The recipient's turn-opening row that delivered it.
    pub delivered: Option<DeliveredBlock>,
}

/// One wrapped block of a recipient's row: the row `uuid` and the block's
/// index among the row's wrapped blocks (`RowFacts::wrapped` order).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveredBlock {
    pub row_uuid: String,
    pub index: usize,
}

#[derive(Debug)]
struct TeamState {
    key: TeamKey,
    dir_name: String,
    name: String,
    binding: LeadBinding,
    lead_sid: Option<String>,
    lead_uid: Option<SessionUid>,
    lead_key: Option<MemberKey>,
    lead_alive: bool,
    ended: bool,
    seen_emitted: Option<(String, Option<MemberKey>, LeadBinding)>,
    members: HashMap<MemberKey, MemberRec>,
    task_tools: bool,
    board: HashMap<String, (Task, Option<Task>)>,
    board_reset_sent: bool,
    files_present: bool,
    items: Vec<Item>,
    next_k: u32,
    sends: HashMap<String, PendingSend>,
    messages: HashMap<MessageKey, Msg>,
    ordinal: u64,
    /// Applied inbox entries: hint id → (`read` flag, timestamp).
    hints_seen: HashMap<String, (Option<bool>, EpochMs)>,
    /// Messages older than this were trimmed from memory and are never
    /// created again (no re-emit of the trimmed ones, A33.5(d)).
    trim_floor: EpochMs,
    in_flight: Option<Option<u32>>,
    shutdown_approved: HashSet<MemberKey>,
    /// Details from the team files (before the pane table's overlay).
    base_details: Option<TeamDetails>,
}

impl TeamState {
    fn hosted(&self) -> bool {
        self.binding == LeadBinding::Hosted
    }

    fn member_by_name(&self, name: &str) -> Option<MemberKey> {
        if name == LEAD_NAME {
            return self.lead_key.clone();
        }
        let mut c: Vec<&MemberRec> = self.members.values().filter(|m| m.member.name == name).collect();
        c.sort_by_key(|m| (m.member.presence != Presence::Present, std::cmp::Reverse(m.member.joined_at)));
        c.first().map(|m| m.member.key.clone())
    }

    fn party_of(&self, name: &str) -> Party {
        match self.member_by_name(name) {
            Some(k) => Party::Member { member: k },
            None => Party::System,
        }
    }

    fn party_of_who(&self, who: &Who) -> Party {
        match who {
            Who::Lead => self.lead_key.clone().map(|member| Party::Member { member }).unwrap_or(Party::System),
            Who::Member(m) => Party::Member { member: m.clone() },
        }
    }

    fn next_ordinal(&mut self) -> u64 {
        self.ordinal += 1;
        self.ordinal
    }

    fn caps(&self) -> TeamCapabilities {
        let hosted = self.hosted();
        TeamCapabilities {
            observe_members: Support::Yes,
            observe_member_state: Support::Partial,
            // SendMessage is a deferred tool; the log may be empty while work
            // happens (LP-15): never "no messages".
            observe_messages: Support::Partial,
            observe_in_flight: Support::Partial,
            observe_tasks: if self.task_tools { Support::Yes } else { Support::Inferred },
            observe_approvals: if hosted { Support::Yes } else { Support::Partial },
            observe_questions: if hosted { Support::Yes } else { Support::Partial },
            act_send: if hosted { vec![SendRoute::ViaMemberPane, SendRoute::ViaLead] } else { vec![SendRoute::ViaLead] },
            act_broadcast: vec![SendRoute::ViaLead],
            act_spawn: vec![SendRoute::ViaLead],
            act_stop: if hosted { vec![StopRoute::ViaLead, StopRoute::ClosePane] } else { vec![StopRoute::ViaLead] },
            act_answer_approval: ApprovalRoute::InLeadUi,
            act_task_write: TaskWrite::None,
            correlation_report: Correlation::PerTurn,
            durability_task_board: Durability::Derived,
            durability_message_log: Durability::Derived,
            fidelity_note: Some(MESSAGES_UNKNOWN_NOTE.to_string()),
        }
    }
}

#[derive(Debug, Default)]
struct State {
    teams: HashMap<TeamKey, TeamState>,
    /// vendor session id → (team, who)
    sessions: HashMap<String, (TeamKey, Who)>,
    /// PaneHost's latest pane table per hosted lead (A21.1).
    tables: HashMap<SessionUid, LeadPaneTable>,
    /// What was last sent per pane member session: (Gilde facts, exit).
    pane_sent: HashMap<SessionUid, (Option<GildeFacts>, Option<ProcessFacts>)>,
    /// When each pane member's process started (its spawn).
    pane_spawned: HashMap<SessionUid, EpochMs>,
    /// The pane lines last sent per lead.
    lines_sent: HashMap<SessionUid, Vec<EvidenceLine>>,
    inbox: InboxStats,
}

/// The Claude native adapter.
pub struct ClaudeTeamAdapter {
    hooks: Arc<dyn TeamHooks>,
    sink: RwLock<Option<Arc<dyn TeamEventSink>>>,
    state: Mutex<State>,
}

impl std::fmt::Debug for ClaudeTeamAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeTeamAdapter").field("teams", &self.state.lock().teams.len()).finish()
    }
}

impl ClaudeTeamAdapter {
    pub fn new(hooks: Arc<dyn TeamHooks>) -> ClaudeTeamAdapter {
        ClaudeTeamAdapter { hooks, sink: RwLock::new(None), state: Mutex::new(State::default()) }
    }

    /// Emit with the adapter lock released (lock order, see `assembly`), as
    /// one World mutation when World offers it (A33.5(d): no per-event
    /// refresh). Returns the World mutations used.
    fn emit_all(&self, events: Vec<TeamEvent>) -> u32 {
        if events.is_empty() {
            return 0;
        }
        let sink = self.sink.read().clone();
        let Some(s) = sink else { return 0 };
        let batch: Vec<(TeamEvent, Option<TeamCapabilities>)> = events
            .into_iter()
            .map(|e| {
                let caps = match &e {
                    TeamEvent::TeamSeen { key, .. } => Some(self.capabilities(key)),
                    _ => None,
                };
                (e, caps)
            })
            .collect();
        match self.hooks.team_events(TeamSource::ClaudeNative, batch) {
            None => 1,
            Some(back) => {
                let n = back.len() as u32;
                for (e, _) in back {
                    s.emit(e);
                }
                n
            }
        }
    }

    /// The counters of the team observations (A33.5(d)).
    pub fn inbox_stats(&self) -> InboxStats {
        self.state.lock().inbox.clone()
    }

    fn uid(&self, account: &AccountId, sid: &str) -> Option<SessionUid> {
        self.hooks.uid_for(&IdentityKey {
            vendor: Vendor::Claude,
            account_id: Some(account.clone()),
            vendor_session_id: sid.to_string(),
        })
    }

    // -- observations (T03) ------------------------------------------------------

    fn observe(&self, obs: TeamObservation) -> (Vec<TeamEvent>, Option<(TeamKey, TeamDetails)>) {
        let key = TeamKey::derive(TeamSource::ClaudeNative.as_str(), Some(&obs.account), &obs.dir.dir_name);
        // Uids first (they take the World lock; no adapter state is borrowed).
        let lead_uid = obs.join.lead_session_id.as_deref().and_then(|sid| self.uid(&obs.account, sid));
        let member_uids: HashMap<String, (SessionUid, String)> = obs
            .live_members
            .iter()
            .filter_map(|m| {
                let sid = m.session_id.clone()?;
                Some((m.agent_id.clone(), (self.uid(&obs.account, &sid)?, sid)))
            })
            .collect();
        let mut st = self.state.lock();
        let known = st.teams.contains_key(&key);
        if !known && !obs.visible {
            // A team dir with no live lead and no live member creates no team row.
            return (Vec::new(), None);
        }
        let mut ev = Vec::new();
        let State { teams, sessions, tables, inbox, .. } = &mut *st;
        let t = teams.entry(key.clone()).or_insert_with(|| TeamState {
            key: key.clone(),
            dir_name: obs.dir.dir_name.clone(),
            name: obs.dir.dir_name.clone(),
            binding: LeadBinding::None,
            lead_sid: None,
            lead_uid: None,
            lead_key: None,
            lead_alive: false,
            ended: false,
            seen_emitted: None,
            members: HashMap::new(),
            task_tools: false,
            board: HashMap::new(),
            board_reset_sent: false,
            files_present: false,
            items: Vec::new(),
            next_k: 0,
            sends: HashMap::new(),
            messages: HashMap::new(),
            ordinal: 0,
            hints_seen: HashMap::new(),
            trim_floor: EpochMs::MIN,
            in_flight: None,
            shutdown_approved: HashSet::new(),
            base_details: None,
        });
        if let Some(c) = &obs.dir.config {
            if let Some(n) = &c.name {
                t.name = crate::render::sanitize::sanitize_display_text(n, crate::contract::display::TextCap::Chars(128)).text;
            }
        }
        // A team keeps its first binding unless a better one arrives.
        if obs.join.binding != LeadBinding::None {
            t.binding = obs.join.binding;
        }
        if let Some(sid) = &obs.join.lead_session_id {
            t.lead_sid = Some(sid.clone());
            sessions.insert(sid.clone(), (key.clone(), Who::Lead));
        }
        if lead_uid.is_some() {
            t.lead_uid = lead_uid.clone();
        }
        let was_alive = t.lead_alive;
        t.lead_alive = obs.lead_alive;
        // Roster.
        let roster = obs.dir.config.as_ref().map(|c| c.members.clone()).unwrap_or_default();
        let live: HashSet<&str> = obs.live_members.iter().map(|m| m.agent_id.as_str()).collect();
        let mut in_roster: HashSet<MemberKey> = HashSet::new();
        let mut pane_backends: Vec<String> = Vec::new();
        for row in &roster {
            let mk = MemberKey::derive(&key, &row.agent_id, row.joined_at);
            in_roster.insert(mk.clone());
            let lead = row.is_lead();
            let backend_s = row.backend_type.clone().unwrap_or_default();
            // The lead is not a pane member; a teammate is a pane member when
            // its backend is a terminal multiplexer or its pane id is `%N`.
            let pane_member = !lead
                && backend_s != "in-process"
                && row.tmux_pane_id.as_deref() != Some("in-process")
                && (backend_s == "tmux"
                    || backend_s == "iterm2"
                    || row.tmux_pane_id.as_deref().is_some_and(|p| p.starts_with('%')));
            let backend = if pane_member { MemberBackend::Pane } else { MemberBackend::InProcess };
            if backend == MemberBackend::Pane {
                pane_backends.push(backend_s.clone());
            }
            if lead {
                t.lead_key = Some(mk.clone());
            }
            let session_uid = if lead {
                t.lead_uid.clone()
            } else {
                member_uids.get(&row.agent_id).map(|(u, _)| u.clone())
            };
            if let Some((_, sid)) = member_uids.get(&row.agent_id) {
                sessions.insert(sid.clone(), (key.clone(), Who::Member(mk.clone())));
            }
            let present = if lead {
                obs.lead_alive
            } else if backend == MemberBackend::Pane {
                live.contains(row.agent_id.as_str())
            } else {
                obs.lead_alive
            };
            let prev = t.members.get(&mk);
            let was_live = prev.is_some_and(|p| p.was_live) || present;
            let presence = if present {
                Presence::Present
            } else {
                match prev.map(|p| &p.member.presence) {
                    Some(Presence::Gone { reason, at }) => Presence::Gone { reason: *reason, at: *at },
                    _ => {
                        let reason = if t.shutdown_approved.contains(&mk) {
                            GoneReason::ShutdownApproved
                        } else if prev.is_some_and(|p| p.was_live) || (backend == MemberBackend::InProcess && was_alive) {
                            GoneReason::ProcessExited
                        } else {
                            GoneReason::Unknown
                        };
                        Presence::Gone { reason, at: obs.at }
                    }
                }
            };
            // The pane link is set by the pane table overlay (both must agree).
            let pane: Option<PaneId> = None;
            let member = Member {
                key: mk.clone(),
                team_key: key.clone(),
                session_uid,
                name: crate::render::sanitize::sanitize_display_text(&row.name, crate::contract::display::TextCap::Chars(64)).text,
                agent_id: Some(row.agent_id.clone()),
                role: if lead { MemberRole::Lead } else { MemberRole::Member },
                colour: row.color.as_deref().and_then(colour_of),
                vendor: Vendor::Claude,
                backend,
                joined_at: row.joined_at,
                presence,
                pane,
                plan_mode_required: row.plan_mode_required.unwrap_or(false),
                current_task: None,
            };
            let rec = t.members.entry(mk.clone()).or_insert_with(|| MemberRec {
                member: member.clone(),
                was_live,
                emitted: None,
                roster_pane: None,
                scan_presence: Presence::Present,
                scan_session: None,
            });
            rec.roster_pane = row.tmux_pane_id.clone();
            rec.scan_presence = member.presence.clone();
            rec.scan_session = member.session_uid.clone();
            rec.member = member;
            rec.was_live = was_live;
        }
        // Members removed from the roster stay, as gone.
        if obs.dir.config.is_some() && !obs.dir.roster_stale {
            for (k, rec) in t.members.iter_mut() {
                if !in_roster.contains(k) && rec.member.presence == Presence::Present {
                    rec.member.presence = Presence::Gone { reason: GoneReason::RemovedFromConfig, at: obs.at };
                    rec.scan_presence = rec.member.presence.clone();
                }
            }
        }
        // PaneHost's pane table of a hosted lead (A21.1).
        let table = t.lead_uid.as_ref().and_then(|l| tables.get(l)).cloned();
        overlay_panes(t, table.as_ref(), &mut ev);
        // TeamSeen (re-emitted when the board kind changes: the descriptor).
        let files_have_tasks = obs.tasks.exists && !obs.tasks.tasks.is_empty();
        if files_have_tasks && !t.task_tools {
            switch_to_board(t, &mut ev);
        }
        let seen = (t.name.clone(), t.lead_key.clone(), t.binding);
        if t.seen_emitted.as_ref() != Some(&seen) {
            ev.push(TeamEvent::TeamSeen {
                key: key.clone(),
                name: t.name.clone(),
                raw_id: t.dir_name.clone(),
                project: None,
                lead: t.lead_key.clone(),
                binding: t.binding,
            });
            t.seen_emitted = Some(seen);
        }
        // Inbox hints (bounded per cycle, A33.5(d)).
        let mut cycle = inbox_hints(t, &obs.dir.inboxes, obs.at, &mut ev);
        cycle.on_discovery_thread = std::thread::current().name() == Some("gilde-discovery");
        // Task files (the board while they exist).
        if obs.tasks.exists && !obs.tasks.tasks.is_empty() {
            t.files_present = true;
            t.board_reset_sent = false;
            for f in &obs.tasks.tasks {
                apply_task_file(t, f, obs.at);
            }
        } else if t.files_present {
            // Files deleted 5 s after all complete: the board stays finished.
            t.files_present = false;
            if !t.board_reset_sent {
                ev.push(TeamEvent::TaskBoardReset { key: key.clone(), at: obs.at });
                t.board_reset_sent = true;
            }
        }
        // In-flight: unread entries addressed to in-process members only.
        let in_process: Vec<String> = t
            .members
            .values()
            .filter(|m| m.member.role == MemberRole::Member && m.member.backend == MemberBackend::InProcess)
            .map(|m| m.member.name.clone())
            .collect();
        let in_flight = if in_process.is_empty() {
            None
        } else {
            Some(
                in_process
                    .iter()
                    .filter_map(|n| obs.dir.inboxes.get(n))
                    .flat_map(|v| v.iter())
                    .filter(|e| e.read == Some(false) && e.frame.is_none())
                    .count() as u32,
            )
        };
        if t.in_flight != Some(in_flight) {
            t.in_flight = Some(in_flight);
            ev.push(TeamEvent::InFlight { team: key.clone(), count: in_flight });
        }
        // Lead exit ends the team (Gilde-emitted, R5 §9.3 #5).
        if was_alive && !obs.lead_alive && !t.ended && t.lead_sid.is_some() {
            t.ended = true;
            ev.push(TeamEvent::TeamEnded { key: key.clone(), at: obs.at });
        }
        flush_members(t, &mut ev);
        flush_board(t, &mut ev);
        flush_messages(t, &mut ev);
        let base = TeamDetails {
            external_host: if t.hosted() || pane_backends.is_empty() {
                None
            } else if pane_backends.iter().any(|b| b == "iterm2") {
                Some(ExternalHost::ITerm2)
            } else if pane_backends.iter().any(|b| b == "tmux") {
                Some(ExternalHost::Tmux)
            } else {
                Some(ExternalHost::Unknown)
            },
            pane_hosting: Some(match (t.hosted(), !pane_backends.is_empty()) {
                (true, true) => PaneHostingState::Hosted,
                (true, false) if t.members.values().any(|m| m.member.role == MemberRole::Member) => {
                    PaneHostingState::InProcessFallback
                }
                (false, true) => PaneHostingState::OutsideGilde,
                _ => PaneHostingState::NotApplicable,
            }),
            roster_stale: obs.dir.roster_stale,
            started_at: obs.dir.config.as_ref().and_then(|c| c.created_at),
            config_mtime: obs.dir.config_id.map(|id| id.mtime_ms),
            pane_tiles: Vec::new(),
        };
        let details = table_details(&base, table.as_ref(), t.hosted());
        t.base_details = Some(base);
        cycle.events = ev.len() as u32;
        inbox.observations += 1;
        inbox.processed_total += u64::from(cycle.processed);
        inbox.events_total += u64::from(cycle.events);
        inbox.max_entries = inbox.max_entries.max(cycle.entries);
        inbox.max_processed = inbox.max_processed.max(cycle.processed);
        inbox.on_discovery_thread |= cycle.on_discovery_thread;
        inbox.last = cycle;
        (ev, Some((key, details)))
    }

    fn gone(&self, account: &AccountId, dir_name: &str, at: EpochMs) -> Vec<TeamEvent> {
        let key = TeamKey::derive(TeamSource::ClaudeNative.as_str(), Some(account), dir_name);
        let mut st = self.state.lock();
        let Some(t) = st.teams.get_mut(&key) else { return Vec::new() };
        let mut ev = Vec::new();
        if !t.ended {
            t.ended = true;
            ev.push(TeamEvent::TeamEnded { key: key.clone(), at });
        }
        for rec in t.members.values_mut() {
            if rec.member.backend == MemberBackend::InProcess && rec.member.presence == Presence::Present {
                rec.member.presence = Presence::Gone { reason: GoneReason::ProcessExited, at };
            }
        }
        flush_members(t, &mut ev);
        ev
    }

    // -- PaneHost's pane table (T07, A21.1) ------------------------------------------

    fn on_table(&self, table: LeadPaneTable) -> TableOutcome {
        let mut st = self.state.lock();
        let lead = table.lead.clone();
        // Session facts of teammate panes, only when they changed.
        let mut facts = Vec::new();
        for p in &table.panes {
            let (PaneKind::Teammate { .. }, Some(uid)) = (&p.kind, &p.session) else { continue };
            if let PaneState::Running { since, .. } = &p.state {
                st.pane_spawned.insert(uid.clone(), *since);
            }
            let spawned = st.pane_spawned.get(uid).copied().unwrap_or(p.created_at);
            let Some((g, exit)) = pane_facts(&p.state, spawned) else { continue };
            let sent = st.pane_sent.entry(uid.clone()).or_insert((None, None));
            if sent.0.as_ref() != Some(&g) {
                facts.push((uid.clone(), SourceFacts::Gilde(g.clone())));
                sent.0 = Some(g);
            }
            if let Some(x) = exit {
                if sent.1 != Some(x) {
                    facts.push((uid.clone(), SourceFacts::Process(x)));
                    sent.1 = Some(x);
                }
            }
        }
        // The lead's pane lines: an unknown pane whose teammate command names
        // another claude install (A21.3).
        let lines: Vec<EvidenceLine> = table
            .panes
            .iter()
            .filter(|p| p.listed && matches!(p.kind, PaneKind::Unknown { reason: UnknownReason::BinaryDiffers }))
            .map(|p| {
                let mut l = EvidenceLine::key(DETAILS_TEAMMATE_BINARY_DIFFERS);
                l.params.insert("pane".into(), serde_json::Value::String(p.id.as_str().to_string()));
                l
            })
            .collect();
        let lines_out = if st.lines_sent.get(&lead) != Some(&lines) {
            st.lines_sent.insert(lead.clone(), lines.clone());
            Some((lead.clone(), lines))
        } else {
            None
        };
        st.tables.insert(lead.clone(), table.clone());
        // The hosted team(s) of this lead.
        let mut ev = Vec::new();
        let mut details = Vec::new();
        for t in st.teams.values_mut().filter(|t| t.lead_uid.as_ref() == Some(&lead)) {
            overlay_panes(t, Some(&table), &mut ev);
            flush_members(t, &mut ev);
            if let Some(base) = &t.base_details {
                details.push((t.key.clone(), table_details(base, Some(&table), t.hosted())));
            }
        }
        (ev, details, facts, lines_out)
    }

    // -- transcript rows (T04) -----------------------------------------------------

    fn on_rows(&self, hint: &IdentityHint, rows: &[RowFacts]) -> Vec<TeamEvent> {
        let Some(sid) = hint.vendor_session_id.as_deref() else { return Vec::new() };
        let mut st = self.state.lock();
        let Some((key, who)) = st.sessions.get(sid).cloned() else { return Vec::new() };
        let Some(t) = st.teams.get_mut(&key) else { return Vec::new() };
        let mut ev = Vec::new();
        for r in rows.iter().filter(|r| !r.sidechain) {
            apply_row(t, &who, r, &mut ev);
        }
        flush_board(t, &mut ev);
        flush_messages(t, &mut ev);
        ev
    }

    /// A27.1: where the transcripts showed a message (ids only). World calls
    /// this with its own lock released (lock order, see `assembly`).
    pub fn message_evidence(&self, team: &TeamKey, key: &MessageKey) -> Option<MessageEvidence> {
        let st = self.state.lock();
        st.teams.get(team)?.messages.get(key).map(|m| m.evidence.clone())
    }
}

/// Overlay PaneHost's pane table on a hosted team's members (§7.4.5–§7.4.7):
/// a member links to its pane only when the roster's `tmuxPaneId` and the
/// pane's teammate agent id agree; a running pane is Present, an exited or
/// killed one Gone; the pane's session row wins. A lead that ended ends the
/// team. Placeholders, unknown panes and spawning panes change nothing here.
fn overlay_panes(t: &mut TeamState, table: Option<&LeadPaneTable>, ev: &mut Vec<TeamEvent>) {
    let hosted = t.hosted();
    for rec in t.members.values_mut() {
        rec.member.presence = rec.scan_presence.clone();
        rec.member.session_uid = rec.scan_session.clone();
        rec.member.pane = None;
        let (Some(table), true) = (table, hosted) else { continue };
        if rec.member.backend != MemberBackend::Pane {
            continue;
        }
        let agent = rec.member.agent_id.clone().unwrap_or_default();
        let Some(pane) = table.panes.iter().rev().find(|p| matches!(&p.kind, PaneKind::Teammate { agent_id, .. } if *agent_id == agent)) else {
            continue;
        };
        if rec.roster_pane.as_deref() == Some(pane.id.as_str()) {
            rec.member.pane = Some(pane.id.clone());
        }
        if pane.session.is_some() {
            rec.member.session_uid = pane.session.clone();
        }
        match &pane.state {
            PaneState::Running { .. } => {
                rec.member.presence = Presence::Present;
                rec.was_live = true;
            }
            PaneState::Exited { at, .. } | PaneState::Killed { at } => {
                let reason = if t.shutdown_approved.contains(&rec.member.key) {
                    GoneReason::ShutdownApproved
                } else {
                    GoneReason::ProcessExited
                };
                rec.member.presence = Presence::Gone { reason, at: *at };
            }
            PaneState::Placeholder { .. } | PaneState::Spawning { .. } => {}
        }
    }
    if let Some(table) = table {
        if table.phase == LeadPhase::Ended && !t.ended && hosted {
            t.ended = true;
            ev.push(TeamEvent::TeamEnded { key: t.key.clone(), at: table.at });
        }
    }
}

/// The team details with the pane table's facts: a real tmux server for the
/// lead means OutsideGilde (§7.4.9); a backgrounded lead keeps its panes with
/// `external_host = Background` (§7.4.7); a hosted team's non-member panes
/// are its tiles (A30.1).
fn table_details(base: &TeamDetails, table: Option<&LeadPaneTable>, hosted: bool) -> TeamDetails {
    let mut d = base.clone();
    if let Some(t) = table {
        if t.outside_gilde {
            d.pane_hosting = Some(PaneHostingState::OutsideGilde);
        }
        if t.phase == LeadPhase::Backgrounded {
            d.external_host = Some(ExternalHost::Background);
        }
        if hosted {
            d.pane_tiles = pane_tiles(t);
        }
    }
    d
}

/// A30.1: a hosted lead's non-member panes as tiles, in creation order, at
/// most [`PANE_TILES_CAP`]. A listed pane with no member session bound is a
/// tile: `Starting` while it is a placeholder or spawning (any kind),
/// `Unknown` for an `Unknown`-kind pane, running or exited and kept (with its
/// exit code); `pty` only for a running `Unknown` pane. A pane bound to a
/// member session is never a tile (its member row shows it); a running or
/// exited teammate pane is its member's; killed and unlisted panes are
/// dropped. The tile carries no time of its own, so an unchanged table gives
/// equal tiles (no store churn).
fn pane_tiles(table: &LeadPaneTable) -> Vec<PaneTile> {
    table
        .panes
        .iter()
        .filter(|p| p.listed && p.session.is_none())
        .filter_map(|p| {
            let unknown = matches!(p.kind, PaneKind::Unknown { .. });
            let (state, pty) = match &p.state {
                PaneState::Placeholder { since, spawn_failed } => {
                    (PaneTileState::Starting { since: *since, spawn_failed: *spawn_failed }, None)
                }
                PaneState::Spawning { since } => (PaneTileState::Starting { since: *since, spawn_failed: false }, None),
                PaneState::Running { pty, .. } if unknown => {
                    (PaneTileState::Unknown { running: true, exit_code: None }, Some(pty.clone()))
                }
                PaneState::Exited { code, kept: true, .. } if unknown => {
                    (PaneTileState::Unknown { running: false, exit_code: *code }, None)
                }
                PaneState::Running { .. } | PaneState::Exited { .. } | PaneState::Killed { .. } => return None,
            };
            let title = p
                .title
                .as_deref()
                .map(|t| crate::render::sanitize::sanitize_display_text(t, crate::contract::display::TextCap::Chars(64)).text)
                .filter(|t| !t.trim().is_empty());
            Some(PaneTile { pane_id: p.id.clone(), state, title, pty, created_at: p.created_at })
        })
        .take(PANE_TILES_CAP)
        .collect()
}

/// A pane member's Gilde facts and exit from its pane (A21.1): the pane
/// table is their only Gilde-side source. Running = started and ready; a
/// `kill-pane` (the lead's or the user's Close pane) is a Gilde stop; an exit
/// carries its code and signal (R2 decides Crashed vs Ended).
fn pane_facts(state: &PaneState, spawned: EpochMs) -> Option<(GildeFacts, Option<ProcessFacts>)> {
    let g = |stop: Option<StopCause>| GildeFacts {
        spawned_at: spawned,
        mode: ControlMode::Terminal,
        ready_at: Some(spawned),
        stop_cause: stop,
        signals_sent: Vec::new(),
        start_failure: None,
    };
    match state {
        PaneState::Running { .. } => Some((g(None), None)),
        PaneState::Exited { code, signal, at, .. } => Some((g(None), Some(ProcessFacts::Exited { at: *at, code: *code, signal: *signal }))),
        PaneState::Killed { at } => Some((g(Some(StopCause::User)), Some(ProcessFacts::Gone { at: *at }))),
        PaneState::Placeholder { .. } | PaneState::Spawning { .. } => None,
    }
}

/// What a pane table changes: team events, team details, session facts and
/// the lead's pane lines.
type TableOutcome = (Vec<TeamEvent>, Vec<(TeamKey, TeamDetails)>, Vec<(SessionUid, SourceFacts)>, Option<(SessionUid, Vec<EvidenceLine>)>);

/// Switch from inferred items to the real board (task tools available).
fn switch_to_board(t: &mut TeamState, ev: &mut Vec<TeamEvent>) {
    t.task_tools = true;
    for it in t.items.drain(..) {
        if it.emitted.is_some() {
            ev.push(TeamEvent::TaskRemoved { key: it.task.key });
        }
    }
    // The descriptor changes (observe.tasks = Yes): re-announce the team.
    t.seen_emitted = None;
}

fn new_task(t: &TeamState, native_id: &str, at: EpochMs, source: TaskSource) -> Task {
    Task {
        key: TaskKey::derive(&t.key, native_id),
        team_key: t.key.clone(),
        native_id: Some(native_id.to_string()),
        title: None,
        inferred_k: None,
        status: TaskStatus::Open,
        station: crate::contract::team::Station::ToDo,
        owner: None,
        blocked_by: Vec::new(),
        blocks: Vec::new(),
        created_at: Some(at),
        started_at: None,
        completed_at: None,
        first_seen_in_progress_at: None,
        source,
        inferred: false,
    }
}

fn set_status(task: &mut Task, s: TaskStatus, at: EpochMs, observed_start: bool) {
    if task.status == s {
        return;
    }
    match s {
        TaskStatus::InProgress => {
            if observed_start {
                task.started_at.get_or_insert(at);
            } else {
                task.first_seen_in_progress_at.get_or_insert(at);
            }
        }
        TaskStatus::Done => {
            task.completed_at.get_or_insert(at);
        }
        _ => {}
    }
    task.status = s;
}

fn apply_task_file(t: &mut TeamState, f: &TaskFile, at: EpochMs) {
    let Some(status) = task_status_of(&f.status) else { return };
    let owner = f.owner.as_deref().and_then(|o| t.member_by_name(o.split('@').next().unwrap_or(o)));
    let blocked_by: Vec<TaskKey> = f.blocked_by.iter().map(|b| TaskKey::derive(&t.key, b)).collect();
    let blocks: Vec<TaskKey> = f.blocks.iter().map(|b| TaskKey::derive(&t.key, b)).collect();
    let fresh = new_task(t, &f.id, at, TaskSource::File);
    let entry = t.board.entry(f.id.clone()).or_insert((fresh, None));
    let task = &mut entry.0;
    // A file seen in progress without an observed start: "≥ {t}".
    set_status(task, status, at, false);
    if f.subject.is_some() {
        task.title = f.subject.clone();
    }
    task.owner = owner;
    task.blocked_by = blocked_by;
    task.blocks = blocks;
}

/// Inbox frames: hints only (§7.2.1). Each hint is applied once; an entry
/// already applied with the same `read` flag is skipped, and at most
/// [`INBOX_ENTRIES_PER_CYCLE`] unseen entries — the newest — are applied per
/// observation, in file order (A33.5(d)).
fn inbox_hints(t: &mut TeamState, inboxes: &std::collections::BTreeMap<String, Vec<InboxEntry>>, at: EpochMs, ev: &mut Vec<TeamEvent>) -> InboxCycle {
    let mut cycle = InboxCycle::default();
    let mut todo: Vec<(&str, &InboxEntry, EpochMs, String)> = Vec::new();
    for (recipient, entries) in inboxes {
        for e in entries {
            cycle.entries += 1;
            let ts = e.frame.as_ref().and_then(|f| f.timestamp).or(e.timestamp).unwrap_or(at);
            let hint_id = format!(
                "{recipient}\0{}\0{ts}\0{}\0{}",
                e.from,
                e.frame.as_ref().map(|f| f.kind.as_str()).unwrap_or(""),
                e.msg_id.as_deref().unwrap_or("")
            );
            if t.hints_seen.get(&hint_id).is_some_and(|(read, _)| *read == e.read) {
                cycle.seen += 1;
                continue;
            }
            if ts < t.trim_floor {
                cycle.over_cap += 1;
                continue;
            }
            todo.push((recipient.as_str(), e, ts, hint_id));
        }
    }
    if todo.len() > INBOX_ENTRIES_PER_CYCLE {
        let mut newest: Vec<usize> = (0..todo.len()).collect();
        newest.sort_by_key(|&i| std::cmp::Reverse(todo[i].2));
        let mut keep = vec![false; todo.len()];
        for &i in newest.iter().take(INBOX_ENTRIES_PER_CYCLE) {
            keep[i] = true;
        }
        let mut kept = Vec::with_capacity(INBOX_ENTRIES_PER_CYCLE);
        for (i, item) in todo.into_iter().enumerate() {
            if keep[i] {
                kept.push(item);
            } else {
                cycle.over_cap += 1;
                // Older than the newest 5,000: never needed again.
                t.hints_seen.insert(item.3, (item.1.read, item.2));
            }
        }
        todo = kept;
    }
    for (recipient, e, ts, hint_id) in todo {
        cycle.processed += 1;
        let first = t.hints_seen.insert(hint_id, (e.read, ts)).is_none();
        apply_inbox_entry(t, recipient, e, ts, first, ev);
    }
    prune_hints(t);
    cycle
}

/// Bound the hint memory: hints older than every message held go first.
fn prune_hints(t: &mut TeamState) {
    if t.hints_seen.len() <= HINTS_CAP {
        return;
    }
    let floor = t.trim_floor;
    t.hints_seen.retain(|_, (_, ts)| *ts >= floor);
    if t.hints_seen.len() > HINTS_CAP {
        let mut ts: Vec<EpochMs> = t.hints_seen.values().map(|(_, ts)| *ts).collect();
        ts.sort_unstable();
        let cut = ts[ts.len() - HINTS_CAP / 2];
        t.hints_seen.retain(|_, (_, ts)| *ts >= cut);
    }
}

/// One inbox entry's hint, event and message.
fn apply_inbox_entry(t: &mut TeamState, recipient: &str, e: &InboxEntry, ts: EpochMs, first: bool, ev: &mut Vec<TeamEvent>) {
    let from = t.party_of(&e.from);
    let to = t.party_of(recipient);
    match &e.frame {
        Some(f) if recipient == LEAD_NAME => {
            let member = f.agent_id.as_deref().map(|a| a.split('@').next().unwrap_or(a)).or(Some(e.from.as_str()));
            let mk = member.and_then(|n| t.member_by_name(n));
            match f.kind.as_str() {
                "permission_request" => {
                    if let (true, Some(m)) = (first, mk) {
                        if f.tool_name.as_deref() == Some("AskUserQuestion") {
                            ev.push(TeamEvent::QuestionAttributed { member: m, location: QuestionLocation::Lead, at: ts });
                        } else if let Some(lead) = &t.lead_uid {
                            ev.push(TeamEvent::ApprovalAttributed {
                                member: m,
                                lead: lead.clone(),
                                tool: f.tool_name.clone().unwrap_or_default(),
                                at: ts,
                            });
                        }
                    }
                }
                "shutdown_approved" => {
                    if let Some(m) = mk {
                        t.shutdown_approved.insert(m.clone());
                        if first {
                            ev.push(TeamEvent::MemberGone { key: m, reason: GoneReason::ShutdownApproved, at: ts });
                        }
                    }
                    observe_protocol(t, from, to, MessageKind::ShutdownApproved, None, e, ts);
                }
                "idle_notification" => {
                    if let Party::Member { member } = &from {
                        if first {
                            close_turn(t, member, ts);
                        }
                    }
                    observe_protocol(t, from, to, MessageKind::IdleNotification, f.idle_reason, e, ts);
                }
                "plan_approval_request" => {
                    if let (true, Some(m)) = (first, mk.clone()) {
                        ev.push(TeamEvent::PlanReview { member: m, at: ts });
                    }
                    observe_protocol(t, from, to, MessageKind::PlanApprovalRequest, None, e, ts);
                }
                other => observe_protocol(t, from, to, message_kind_of(other), None, e, ts),
            }
        }
        Some(f) => observe_protocol(t, from, to, message_kind_of(&f.kind), f.idle_reason, e, ts),
        None => {
            // A plain message: Queued while unread (a closed pane's
            // messages stay unread forever, LP-13).
            let status = if e.read == Some(false) { DeliveryStatus::Queued } else { DeliveryStatus::Delivered };
            let key = match &e.msg_id {
                Some(id) => MessageKey::derive(&t.key, id),
                None => MessageKey::derive(&t.key, &format!("inbox\0{}\0{recipient}\0{ts}", e.from)),
            };
            upsert_message(t, key.clone(), from.clone(), to.clone(), ts, MessageKind::Plain, status, e.msg_id.is_none(), None, e.has_summary);
            // A lead plain message opens an inferred item.
            if e.from == LEAD_NAME && !t.task_tools {
                if let Party::Member { member } = &to {
                    let opener = e.msg_id.clone().unwrap_or_else(|| format!("inbox:{ts}"));
                    open_item(t, member, &opener, ts, false);
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn upsert_message(
    t: &mut TeamState,
    key: MessageKey,
    from: Party,
    to: Party,
    at: EpochMs,
    kind: MessageKind,
    status: DeliveryStatus,
    derived: bool,
    idle_reason: Option<IdleReason>,
    has_summary: bool,
) {
    // A33.5(d): a message older than every message held was trimmed (or
    // would be at once): never created again, never re-emitted.
    if at < t.trim_floor && !t.messages.contains_key(&key) {
        return;
    }
    let team_key = t.key.clone();
    let m = t.messages.entry(key.clone()).or_insert_with(|| Msg {
        msg: TeamMessage {
            key,
            team_key,
            from: from.clone(),
            to: to.clone(),
            at,
            kind,
            status,
            in_reply_to: None,
            derived,
            idle_reason,
            has_summary,
        },
        emitted: None,
        evidence: MessageEvidence::default(),
    });
    // Delivery only moves forward (Sent → Queued → Delivered); a later
    // observation never downgrades it.
    let rank = |s: DeliveryStatus| match s {
        DeliveryStatus::Unknown => 0,
        DeliveryStatus::Sent => 1,
        DeliveryStatus::Queued => 2,
        DeliveryStatus::Delivered => 3,
    };
    if rank(status) > rank(m.msg.status) {
        m.msg.status = status;
    }
    if idle_reason.is_some() {
        m.msg.idle_reason = idle_reason;
    }
    m.msg.has_summary |= has_summary;
    m.msg.derived &= derived;
}

/// A protocol frame as a system event of the log ("db-agent went idle").
fn observe_protocol(
    t: &mut TeamState,
    from: Party,
    to: Party,
    kind: MessageKind,
    idle_reason: Option<IdleReason>,
    e: &InboxEntry,
    ts: EpochMs,
) {
    let key = match &e.msg_id {
        Some(id) => MessageKey::derive(&t.key, id),
        None => {
            // An idle notification seen in the lead transcript within 120 s is
            // the same message.
            if let Some(k) = similar_message(t, &from, &to, kind, ts) {
                k
            } else {
                MessageKey::derive(&t.key, &format!("frame\0{kind:?}\0{}\0{ts}", e.from))
            }
        }
    };
    let status = if e.read == Some(false) { DeliveryStatus::Queued } else { DeliveryStatus::Delivered };
    upsert_message(t, key, from, to, ts, kind, status, e.msg_id.is_none(), idle_reason, e.has_summary);
}

fn similar_message(t: &TeamState, from: &Party, to: &Party, kind: MessageKind, at: EpochMs) -> Option<MessageKey> {
    t.messages
        .values()
        .filter(|m| &m.msg.from == from && &m.msg.to == to && m.msg.kind == kind && (m.msg.at - at).abs() <= JOIN_WINDOW_MS)
        .min_by_key(|m| (m.msg.at - at).abs())
        .map(|m| m.msg.key.clone())
}

/// Open one inferred item for a member (§7.2.5).
fn open_item(t: &mut TeamState, member: &MemberKey, opener: &str, at: EpochMs, started: bool) {
    if t.task_tools || t.items.iter().any(|i| i.opened_by == opener && i.task.owner.as_ref() == Some(member)) {
        return;
    }
    t.next_k += 1;
    let task = Task {
        key: TaskKey::derive_inferred(&t.key, member, opener),
        team_key: t.key.clone(),
        native_id: None,
        title: None,
        inferred_k: Some(t.next_k),
        status: if started { TaskStatus::InProgress } else { TaskStatus::Open },
        station: crate::contract::team::Station::Assigned,
        owner: Some(member.clone()),
        blocked_by: Vec::new(),
        blocks: Vec::new(),
        created_at: Some(at),
        started_at: started.then_some(at),
        completed_at: None,
        first_seen_in_progress_at: None,
        source: TaskSource::Inferred,
        inferred: true,
    };
    t.items.push(Item { task, opened_by: opener.to_string(), emitted: None });
}

/// The member's turn ended: every item of that turn is Reported.
fn close_turn(t: &mut TeamState, member: &MemberKey, at: EpochMs) {
    for it in t.items.iter_mut() {
        if it.task.owner.as_ref() == Some(member) && it.task.status == TaskStatus::InProgress {
            it.task.status = TaskStatus::Done;
            it.task.completed_at = Some(at);
        }
    }
}

/// The member's turn-opening row delivered `k` lead messages: the k oldest
/// open items start (more are created when the Sent side was not seen).
fn start_items(t: &mut TeamState, member: &MemberKey, row_id: &str, k: usize, at: EpochMs) {
    let mut started = 0;
    for it in t.items.iter_mut() {
        if started == k {
            break;
        }
        if it.task.owner.as_ref() == Some(member) && it.task.status == TaskStatus::Open {
            it.task.status = TaskStatus::InProgress;
            it.task.started_at = Some(at);
            started += 1;
        }
    }
    for i in started..k {
        open_item(t, member, &format!("{row_id}#{i}"), at, true);
    }
}

fn apply_row(t: &mut TeamState, who: &Who, r: &RowFacts, ev: &mut Vec<TeamEvent>) {
    let at = r.at.unwrap_or(0);
    let row_id = r.uuid.clone().unwrap_or_else(|| format!("row:{at}"));
    // Which board.
    let tools_seen = r.tool_names.iter().any(|n| is_task_tool(n)) || r.tool_uses.iter().any(|u| is_task_tool(&u.name));
    if (tools_seen || !r.task_ops.is_empty()) && !t.task_tools {
        switch_to_board(t, ev);
        let seen = (t.name.clone(), t.lead_key.clone(), t.binding);
        ev.push(TeamEvent::TeamSeen {
            key: t.key.clone(),
            name: t.name.clone(),
            raw_id: t.dir_name.clone(),
            project: None,
            lead: t.lead_key.clone(),
            binding: t.binding,
        });
        t.seen_emitted = Some(seen);
    }
    // Task tool calls (ids from TaskUpdate; TaskCreate's id is in its result
    // text, which the structure facts do not carry yet).
    for op in &r.task_ops {
        let Some(id) = op.task_id.clone().filter(|i| !i.is_empty() && i.len() <= 12 && i.bytes().all(|c| c.is_ascii_digit())) else {
            continue;
        };
        let owner = op.owner.as_deref().and_then(|o| t.member_by_name(o.split('@').next().unwrap_or(o)));
        let blocked_by: Vec<TaskKey> = op.blocked_by.iter().map(|b| TaskKey::derive(&t.key, b)).collect();
        let fresh = new_task(t, &id, at, TaskSource::Transcript);
        let entry = t.board.entry(id.clone()).or_insert((fresh, None));
        let task = &mut entry.0;
        if let Some(s) = op.status.as_deref().and_then(task_status_of) {
            set_status(task, s, at, true);
        }
        if owner.is_some() {
            task.owner = owner;
        }
        for b in blocked_by {
            if !task.blocked_by.contains(&b) {
                task.blocked_by.push(b);
            }
        }
    }
    // SendMessage calls, joined by the result's msg_id.
    for s in &r.send_messages {
        t.sends.insert(s.tool_use_id.clone(), PendingSend { from: t.party_of_who(who), to_name: s.to.clone(), at });
    }
    for id in &r.tool_results {
        if let Some(s) = t.sends.remove(id) {
            let to = s.to_name.as_deref().map(|n| t.party_of(n.split('@').next().unwrap_or(n))).unwrap_or(Party::System);
            let (key, derived) = match &r.send_message_msg_id {
                Some(m) => (MessageKey::derive(&t.key, m), false),
                None => {
                    let o = t.next_ordinal();
                    (MessageKey::derive(&t.key, &format!("send\0{id}\0{o}")), true)
                }
            };
            upsert_message(t, key.clone(), s.from.clone(), to.clone(), s.at, MessageKind::Plain, DeliveryStatus::Sent, derived, None, false);
            if let Some(m) = t.messages.get_mut(&key) {
                m.evidence.tool_use_id = Some(id.clone());
            }
            // A lead plain message to a member opens an inferred item.
            if matches!(who, Who::Lead) {
                if let Party::Member { member } = &to {
                    let opener = r.send_message_msg_id.clone().unwrap_or_else(|| id.clone());
                    if Some(member) != t.lead_key.as_ref() {
                        open_item(t, member, &opener, s.at, false);
                    }
                }
            }
        }
    }
    // Delivered blocks in a turn-opening row of this transcript.
    if r.turn_open.is_some() {
        let me = t.party_of_who(who);
        if let Who::Member(m) = who {
            // The next turn-opening row closes the previous turn's items.
            close_turn(t, m, at);
        }
        let mut lead_blocks = 0usize;
        // The index is the block's position among all of the row's wrapped
        // blocks (A27.1: the body reader finds the same block again).
        for (index, w) in r.wrapped.iter().enumerate().filter(|(_, w)| matches!(w.origin, UserOrigin::Teammate | UserOrigin::Peer)) {
            let from_name = w.teammate_id.as_deref().map(|n| n.split('@').next().unwrap_or(n)).unwrap_or("");
            let from = t.party_of(from_name);
            let key = if w.kind == MessageKind::Plain {
                if from_name == LEAD_NAME {
                    lead_blocks += 1;
                }
                deliver(t, &from, &me, w.msg_id.as_deref(), at, w.has_summary)
            } else {
                let key = match &w.msg_id {
                    Some(id) => MessageKey::derive(&t.key, id),
                    None => similar_message(t, &from, &me, w.kind, at)
                        .unwrap_or_else(|| MessageKey::derive(&t.key, &format!("row\0{row_id}\0{from_name}\0{:?}", w.kind))),
                };
                upsert_message(t, key.clone(), from.clone(), me.clone(), at, w.kind, DeliveryStatus::Delivered, w.msg_id.is_none(), None, w.has_summary);
                if w.kind == MessageKind::IdleNotification {
                    if let Party::Member { member } = &from {
                        close_turn(t, member, at);
                    }
                }
                key
            };
            if let (Some(row_uuid), Some(m)) = (r.uuid.as_ref(), t.messages.get_mut(&key)) {
                m.evidence.delivered = Some(DeliveredBlock { row_uuid: row_uuid.clone(), index });
            }
        }
        if let Who::Member(m) = who {
            if lead_blocks > 0 && !t.task_tools {
                start_items(t, &m.clone(), &row_id, lead_blocks, at);
            }
        }
    }
    // An Esc-interrupted pane turn emits no idle notification: its items close
    // on the interrupt marker (LP-14b).
    if r.turn_end == Some(TurnEndKind::Interrupted) {
        if let Who::Member(m) = who {
            close_turn(t, &m.clone(), at);
        }
    }
}

/// A delivered plain message: by `msg_id`, else the oldest undelivered Sent
/// message of the pair within 120 s (`derived`), else a new derived one.
/// Returns the key of the message it delivered.
fn deliver(t: &mut TeamState, from: &Party, to: &Party, msg_id: Option<&str>, at: EpochMs, has_summary: bool) -> MessageKey {
    if let Some(id) = msg_id {
        let key = MessageKey::derive(&t.key, id);
        upsert_message(t, key.clone(), from.clone(), to.clone(), at, MessageKind::Plain, DeliveryStatus::Delivered, false, None, has_summary);
        return key;
    }
    let candidate = t
        .messages
        .values()
        .filter(|m| {
            &m.msg.from == from
                && &m.msg.to == to
                && m.msg.kind == MessageKind::Plain
                && m.msg.status != DeliveryStatus::Delivered
                && (at - m.msg.at) <= JOIN_WINDOW_MS
                && m.msg.at <= at + JOIN_WINDOW_MS
        })
        .min_by_key(|m| m.msg.at)
        .map(|m| m.msg.key.clone());
    match candidate {
        Some(k) => {
            if let Some(m) = t.messages.get_mut(&k) {
                m.msg.status = DeliveryStatus::Delivered;
                m.msg.derived = true;
            }
            k
        }
        None => {
            let o = t.next_ordinal();
            let key = MessageKey::derive(&t.key, &format!("delivered\0{at}\0{o}"));
            upsert_message(t, key.clone(), from.clone(), to.clone(), at, MessageKind::Plain, DeliveryStatus::Delivered, true, None, has_summary);
            key
        }
    }
}

fn flush_members(t: &mut TeamState, ev: &mut Vec<TeamEvent>) {
    for rec in t.members.values_mut() {
        if rec.emitted.as_ref() != Some(&rec.member) {
            ev.push(TeamEvent::MemberUpserted(rec.member.clone()));
            rec.emitted = Some(rec.member.clone());
        }
    }
}

fn flush_board(t: &mut TeamState, ev: &mut Vec<TeamEvent>) {
    if t.task_tools {
        for (task, emitted) in t.board.values_mut() {
            if emitted.as_ref() != Some(task) {
                ev.push(TeamEvent::TaskUpserted(task.clone()));
                *emitted = Some(task.clone());
            }
        }
    } else {
        for it in t.items.iter_mut() {
            if it.emitted.as_ref() != Some(&it.task) {
                ev.push(TeamEvent::TaskUpserted(it.task.clone()));
                it.emitted = Some(it.task.clone());
            }
        }
    }
}

fn flush_messages(t: &mut TeamState, ev: &mut Vec<TeamEvent>) {
    let mut v: Vec<&mut Msg> = t.messages.values_mut().filter(|m| m.emitted.as_ref() != Some(&m.msg)).collect();
    v.sort_by_key(|m| m.msg.at);
    for m in v {
        ev.push(TeamEvent::MessageObserved(m.msg.clone()));
        m.emitted = Some(m.msg.clone());
    }
    // Bound the adapter's memory (the store keeps the newest 500).
    if t.messages.len() > MESSAGES_CAP {
        let mut ats: Vec<(EpochMs, MessageKey)> = t.messages.values().map(|m| (m.msg.at, m.msg.key.clone())).collect();
        ats.sort();
        for (at, k) in ats.into_iter().take(t.messages.len() - MESSAGES_CAP) {
            t.messages.remove(&k);
            t.trim_floor = t.trim_floor.max(at);
        }
    }
}

impl TeamObserver for ClaudeTeamAdapter {
    fn team(&self, obs: TeamObservation) {
        let (events, details) = self.observe(obs);
        if let Some((key, d)) = details {
            self.hooks.team_details(&key, TeamSource::ClaudeNative, d);
        }
        let batches = self.emit_all(events);
        let mut st = self.state.lock();
        st.inbox.last.world_batches = batches;
        st.inbox.max_world_batches = st.inbox.max_world_batches.max(batches);
    }

    fn team_gone(&self, account: &AccountId, dir_name: &str, at: EpochMs) {
        let events = self.gone(account, dir_name, at);
        self.emit_all(events);
    }
}

impl PaneTableSink for ClaudeTeamAdapter {
    /// Bounded and non-blocking; never calls back into PaneHost.
    fn table(&self, table: LeadPaneTable) {
        let (events, details, facts, lines) = self.on_table(table);
        for (uid, f) in facts {
            self.hooks.session_facts(&uid, f);
        }
        if let Some((lead, l)) = lines {
            self.hooks.session_lines(&lead, l);
        }
        for (key, d) in details {
            self.hooks.team_details(&key, TeamSource::ClaudeNative, d);
        }
        self.emit_all(events);
    }
}

impl RowSink for ClaudeTeamAdapter {
    fn rows(&self, _path: &Path, hint: &IdentityHint, rows: &[RowFacts]) {
        let events = self.on_rows(hint, rows);
        self.emit_all(events);
    }
}

impl TeamAdapter for ClaudeTeamAdapter {
    fn source(&self) -> TeamSource {
        TeamSource::ClaudeNative
    }

    fn capabilities(&self, team: &TeamKey) -> TeamCapabilities {
        let st = self.state.lock();
        match st.teams.get(team) {
            Some(t) => t.caps(),
            None => {
                let mut c = TeamCapabilities::all_native();
                c.act_send.clear();
                c.act_broadcast.clear();
                c.act_spawn.clear();
                c.act_stop.clear();
                c.act_answer_approval = ApprovalRoute::InLeadUi;
                c.act_task_write = TaskWrite::None;
                c.observe_tasks = Support::Unknown;
                c.observe_messages = Support::Unknown;
                c.observe_in_flight = Support::Unknown;
                c.correlation_report = Correlation::PerTurn;
                c.durability_task_board = Durability::Derived;
                c.durability_message_log = Durability::Derived;
                c
            }
        }
    }

    fn subscribe(&self, sink: Arc<dyn TeamEventSink>) {
        *self.sink.write() = Some(sink);
    }

    fn send(&self, team: &TeamKey, to: &MemberKey, _text: &str) -> ActOutcome {
        let st = self.state.lock();
        let Some(t) = st.teams.get(team) else { return ActOutcome::Unsupported { reason: "unknownTeam".into() } };
        let Some(m) = t.members.get(to) else { return ActOutcome::Unsupported { reason: "unknownMember".into() } };
        if matches!(m.member.presence, Presence::Gone { .. }) {
            return ActOutcome::Unsupported { reason: "memberGone".into() };
        }
        if t.hosted() && m.member.backend == MemberBackend::Pane {
            // Typed into the member's pane by PaneHost (`team_send`).
            return ActOutcome::Unsupported { reason: "viaMemberPane".into() };
        }
        match &t.lead_uid {
            Some(lead) => ActOutcome::PrefillLead { lead: lead.clone(), action: PrefillAction::Tell { member: to.clone() } },
            None => ActOutcome::Unsupported { reason: "viaLeadOnly".into() },
        }
    }

    fn broadcast(&self, team: &TeamKey, _text: &str) -> ActOutcome {
        self.prefill(team, PrefillAction::TellAll)
    }

    fn spawn(&self, team: &TeamKey, spec: &MemberSpec) -> ActOutcome {
        self.prefill(team, PrefillAction::Spawn { name: spec.name.clone() })
    }

    fn stop(&self, team: &TeamKey, member: &MemberKey, how: StopHow) -> ActOutcome {
        match how {
            StopHow::Graceful => self.prefill(team, PrefillAction::AskStop { member: member.clone() }),
            StopHow::HardClosePane => ActOutcome::Unsupported { reason: "closePane".into() },
        }
    }

    fn answer_approval(&self, _team: &TeamKey, _req: &RequestId, _d: &Decision) -> ActOutcome {
        // The user answers where the request lives (§7.2.6).
        ActOutcome::Unsupported { reason: "answerWhereItLives".into() }
    }

    fn task_write(&self, team: &TeamKey, _op: TaskOp) -> ActOutcome {
        let tools = self.state.lock().teams.get(team).is_some_and(|t| t.task_tools);
        if tools {
            self.prefill(team, PrefillAction::AddTask)
        } else {
            ActOutcome::Unsupported { reason: "noTaskTools".into() }
        }
    }
}

impl ClaudeTeamAdapter {
    fn prefill(&self, team: &TeamKey, action: PrefillAction) -> ActOutcome {
        let st = self.state.lock();
        match st.teams.get(team).and_then(|t| t.lead_uid.clone()) {
            Some(lead) => ActOutcome::PrefillLead { lead, action },
            None => ActOutcome::Unsupported { reason: "viaLeadOnly".into() },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_and_kinds() {
        assert_eq!(colour_of("Blue"), Some(IdentityColour::Blue));
        assert_eq!(colour_of("magenta"), None);
        assert_eq!(message_kind_of("idle_notification"), MessageKind::IdleNotification);
        assert_eq!(message_kind_of("something_new"), MessageKind::Unknown);
        assert_eq!(task_status_of("completed"), Some(TaskStatus::Done));
        assert_eq!(task_status_of("weird"), None);
    }

    const DIR: &str = "session-a33d0001";

    fn plain(i: usize, at: EpochMs, read: bool) -> InboxEntry {
        InboxEntry {
            from: "alpha".into(),
            timestamp: Some(at),
            read: Some(read),
            kind: Some("message".into()),
            msg_id: Some(format!("m-{i}")),
            color: None,
            has_summary: false,
            frame: None,
        }
    }

    fn row(name: &str, backend: &str, pane: &str, joined: EpochMs) -> crate::discovery::claude::teams::TeamMemberRow {
        crate::discovery::claude::teams::TeamMemberRow {
            agent_id: format!("{name}@{DIR}"),
            name: name.into(),
            agent_type: None,
            model: None,
            color: Some("blue".into()),
            backend_type: Some(backend.into()),
            tmux_pane_id: Some(pane.into()),
            cwd: None,
            joined_at: Some(joined),
            is_active: None,
            plan_mode_required: None,
            mode: None,
        }
    }

    fn obs(account: &AccountId, now: EpochMs, inbox: Vec<InboxEntry>) -> TeamObservation {
        use crate::discovery::claude::teams::{LeadJoin, TasksRead, TeamConfig, TeamDirRead};
        let created: EpochMs = 1_799_980_000_000;
        TeamObservation {
            account: account.clone(),
            dir: TeamDirRead {
                dir_name: DIR.into(),
                config: Some(TeamConfig {
                    name: Some("a33".into()),
                    created_at: Some(created),
                    lead_agent_id: Some(format!("team-lead@{DIR}")),
                    lead_session_id: None,
                    members: vec![row("team-lead", "in-process", "leader", created), row("alpha", "in-process", "in-process", created + 1)],
                    members_truncated: false,
                }),
                config_id: None,
                roster_stale: false,
                torn: 0,
                inboxes: std::collections::BTreeMap::from([(LEAD_NAME.to_string(), inbox)]),
                inboxes_skipped: 0,
            },
            join: LeadJoin { binding: LeadBinding::Exact, lead_session_id: Some("a33d0000-0000-4000-8000-000000000001".into()), lead_pid: Some(7100) },
            lead_alive: true,
            live_members: vec![],
            tasks: TasksRead::default(),
            in_process: vec![],
            visible: true,
            at: now,
        }
    }

    /// A33.5(d): a 10 k-entry inbox costs one bounded cycle — at most 5,000
    /// entries applied (the newest), one World mutation for the events, a
    /// handful in all; an unchanged inbox applies nothing; one new entry and
    /// one `read` flip apply two, with no re-emit of the trimmed messages.
    #[test]
    fn a33_5d_a_10k_inbox_is_bounded_per_cycle() {
        use crate::world::{ManualClock, MemoryMetaStore, World, WorldConfig, WorldSettings};
        let now: EpochMs = 1_800_000_000_000;
        let world = World::new(WorldConfig {
            clock: Arc::new(ManualClock::new(now, 50_000_000)),
            meta_store: Arc::new(MemoryMetaStore::default()),
            settings: WorldSettings::default(),
            home: None,
        });
        let a = world.claude_team_adapter();
        let acct = AccountId::derive(Vendor::Claude, "/h/.claude");
        let key = TeamKey::derive(TeamSource::ClaudeNative.as_str(), Some(&acct), DIR);
        let base = now - 10_000_000;
        let mut inbox: Vec<InboxEntry> = (0..10_000).map(|i| plain(i, base + i as i64 * 1_000, i != 9_999)).collect();

        let m0 = world.mutations();
        a.team(obs(&acct, now, inbox.clone()));
        let c1 = a.inbox_stats().last;
        let m1 = world.mutations();
        assert_eq!((c1.entries, c1.processed, c1.over_cap, c1.seen), (10_000, 5_000, 5_000, 0), "{c1:?}");
        assert_eq!(c1.world_batches, 1, "one World mutation for every event: {c1:?}");
        assert!(m1 - m0 <= 4, "World mutations for one observation: {}", m1 - m0);
        assert!(!c1.on_discovery_thread);
        assert!(world.message_locator(&MessageKey::derive(&key, "m-9999")).is_some(), "the newest is held");
        assert!(world.message_locator(&MessageKey::derive(&key, "m-4999")).is_none(), "beyond the per-cycle cap");

        // The same inbox again: nothing applied, nothing re-emitted.
        a.team(obs(&acct, now + 1, inbox.clone()));
        let c2 = a.inbox_stats().last;
        assert_eq!((c2.processed, c2.seen), (0, 10_000), "{c2:?}");
        assert_eq!(c2.events, 0, "no full re-emit: {c2:?}");
        assert!(world.mutations() - m1 <= 4);

        // One new entry and one `read` flip.
        inbox[9_999].read = Some(true);
        inbox.push(plain(10_000, now, true));
        let m2 = world.mutations();
        a.team(obs(&acct, now + 2, inbox.clone()));
        let c3 = a.inbox_stats().last;
        assert_eq!(c3.processed, 2, "{c3:?}");
        assert_eq!(c3.events, 2, "only the two changed messages: {c3:?}");
        assert!(world.mutations() - m2 <= 4);
        assert!(world.message_locator(&MessageKey::derive(&key, "m-10000")).is_some());
        assert_eq!(world.message_locator(&MessageKey::derive(&key, "m-9999")).map(|l| l.status), Some(DeliveryStatus::Delivered));
        assert_eq!(a.inbox_stats().observations, 3);
    }
}
