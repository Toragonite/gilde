//! The TeamAdapter registry and team/member/task/message assembly.
//!
//! Owner: T02. Adapters push [`TeamEvent`]s; this module keeps the team
//! state World publishes (§3.4.6–§3.4.8, §3.6, §7.1):
//! * a Team row is published only once it has ≥ 1 member besides the lead;
//! * members never disappear while the team lives (gone members stay with
//!   their last state); lead first, then `joinedAt` order; ≤ 200 per team;
//! * tasks: ≤ 1,000 per team; `station` is a pure function of (status,
//!   owner, an open declared blocker, owner session in band 1–2); a
//!   `Deleted` task is removed from every `blocked_by`/`blocks` list;
//!   inferred tasks never have a title or blockers;
//! * messages: text is never here; the store gets the newest 500 per team;
//! * team-level values: `glyph_state`, `TeamProgress` incl. the Rust-computed
//!   bottlenecks, and the capability descriptor filtered by what is possible
//!   now (a via-lead route needs the lead In Gilde, §7.2.3).
//!
//! Lock order: World never calls into an adapter while it holds the World
//! lock (capabilities are read by the adapter's sink before it locks World),
//! and adapters emit with their own lock released.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::contract::activity::{ActivityState, ErrorSub, TeamFileFacts};
use crate::contract::common::{EpochMs, IdentityColour};
use crate::contract::ids::{MemberKey, MessageKey, ProjectId, SessionUid, TaskKey, TeamKey};
use crate::contract::session::MemberRole;
use crate::contract::store::{Entity, EntityKind};
use crate::contract::team::{
    ExternalHost, IdleReason, LeadBinding, Member, MemberBackend, PaneHostingState, PaneTile, Party, Presence, QuestionLocation,
    SendRoute, Station, StopRoute, Task, TaskStatus, Team, TeamCapabilities, TeamEvent, TeamMessage, TeamProgress,
    TeamSource,
};
use crate::world::patch::Outbox;
use crate::world::teams::bottlenecks::bottlenecks;

/// ≤ 200 members per team (§5.8).
pub const MEMBERS_CAP: usize = 200;
/// ≤ 1,000 tasks per team (§5.8).
pub const TASKS_CAP: usize = 1_000;
/// The store keeps at most 500 messages per team (§3.4.7).
pub const STORE_MESSAGES: usize = 500;
/// Older messages stay in memory for `team_messages_page` up to this many.
pub const MEMORY_MESSAGES: usize = 5_000;

/// Facts an adapter knows about a team beyond `TeamSeen` (Claude: the
/// roster's backends, the parse state, `createdAt`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TeamDetails {
    pub external_host: Option<ExternalHost>,
    pub pane_hosting: Option<PaneHostingState>,
    pub roster_stale: bool,
    pub started_at: Option<EpochMs>,
    /// The team `config.json` mtime: the reference of the isActive rule.
    pub config_mtime: Option<EpochMs>,
    /// A30.1: the hosted lead's non-member panes (from PaneHost's pane
    /// table), creation order, ≤ [`PANE_TILES_CAP`]; empty when not hosted.
    pub pane_tiles: Vec<PaneTile>,
}

/// A30.1: at most this many pane tiles per team.
pub const PANE_TILES_CAP: usize = 16;

/// An attribution of a lead's prompt to a member (§7.2.6, §7.2.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribution {
    pub member: MemberKey,
    pub question: bool,
    pub at: EpochMs,
}

/// What World offers its own adapters. Adapters may call these while they
/// hold their own lock; World never calls an adapter while it holds the
/// World lock (no lock-order inversion).
pub trait TeamHooks: Send + Sync {
    /// The uid of a vendor session key (minted and persisted on first sight).
    fn uid_for(&self, key: &crate::contract::services::IdentityKey) -> Option<SessionUid>;
    /// The session of a live process (orchestrator members, §7.3.1).
    fn uid_by_pid(&self, pid: i32) -> Option<SessionUid>;
    /// Adapter-side facts about a team beyond `TeamSeen`.
    fn team_details(&self, key: &TeamKey, source: TeamSource, details: TeamDetails);
    /// Facts about one session the adapter is the source of (a hosted pane
    /// member's Gilde facts and exit, from PaneHost's pane table, A21.1).
    fn session_facts(&self, uid: &SessionUid, facts: crate::contract::services::SourceFacts);
    /// The pane lines of a hosted lead (e.g. `details.teammateBinaryDiffers`),
    /// replacing the previous ones.
    fn session_lines(&self, uid: &SessionUid, lines: Vec<crate::contract::activity::EvidenceLine>);
    /// Apply one adapter's events (with the descriptor of each `TeamSeen`) as
    /// **one** World mutation — one refresh, A33.5(d). `Some(events)` hands
    /// them back when this is not supported (the adapter emits them one by
    /// one through its sink).
    #[allow(clippy::type_complexity)]
    fn team_events(
        &self,
        source: TeamSource,
        events: Vec<(crate::contract::team::TeamEvent, Option<crate::contract::team::TeamCapabilities>)>,
    ) -> Option<Vec<(crate::contract::team::TeamEvent, Option<crate::contract::team::TeamCapabilities>)>> {
        let _ = source;
        Some(events)
    }
}

/// One session's place in a team.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleInfo {
    pub team_key: TeamKey,
    pub member_key: MemberKey,
    pub role: MemberRole,
    pub colour: Option<IdentityColour>,
    pub backend: MemberBackend,
    pub team_hosted: bool,
    /// Lead only: the team has ≥ 1 present pane member.
    pub live_panes: bool,
    /// Member only: the lead's session.
    pub lead_uid: Option<SessionUid>,
    /// The team row is published (Session.team is set only then).
    pub published: bool,
    pub config_mtime: Option<EpochMs>,
    pub source: TeamSource,
}

/// What the graph needs to know about sessions to assemble teams.
pub trait SessionView {
    /// The published state and band of a session.
    fn state(&self, uid: &SessionUid) -> Option<(ActivityState, Option<u8>)>;
    /// The session runs In Gilde.
    fn in_gilde(&self, uid: &SessionUid) -> bool;
    fn project(&self, uid: &SessionUid) -> Option<ProjectId>;
    /// A provisional-clause flag of the session's claude version (§1.3,
    /// §3.10); `None` = absent (fallback behaviour).
    fn feature(&self, uid: &SessionUid, flag: &str) -> Option<bool>;
}

/// P17: team files read without a verified team-store shape.
pub const FLAG_TEAM_STORE_VERIFIED: &str = "teamStoreVerified";
/// P22: one `idle_notification` per completed or failed pane turn.
pub const FLAG_IDLE_PER_TURN: &str = "idlePerTurn";

/// The state of one team.
#[derive(Debug, Clone)]
pub struct TeamRec {
    pub key: TeamKey,
    pub source: TeamSource,
    pub name: String,
    pub raw_id: String,
    pub project: Option<ProjectId>,
    pub lead: Option<MemberKey>,
    pub binding: LeadBinding,
    pub caps: TeamCapabilities,
    pub details: TeamDetails,
    pub members: HashMap<MemberKey, Member>,
    /// S4 facts of members without a session row.
    pub member_facts: HashMap<MemberKey, TeamFileFacts>,
    pub tasks: HashMap<TaskKey, Task>,
    /// Ordered by (at, key).
    pub messages: BTreeMap<(EpochMs, MessageKey), TeamMessage>,
    pub message_at: HashMap<MessageKey, EpochMs>,
    pub in_flight: Option<u32>,
    pub ended_at: Option<EpochMs>,
    /// Lead uid → attributions of its prompts.
    pub attributions: Vec<(SessionUid, Attribution)>,
    pub plan_reviews: Vec<(MemberKey, EpochMs)>,
    pub question_locations: HashMap<MemberKey, QuestionLocation>,
    // what was last published (for diffs)
    pub pub_team: Option<Team>,
    pub pub_members: HashMap<MemberKey, Member>,
    pub pub_tasks: HashMap<TaskKey, Task>,
    pub pub_messages: HashSet<MessageKey>,
    pub pub_message_values: HashMap<MessageKey, TeamMessage>,
    pub recheck_wall: Option<EpochMs>,
}

impl TeamRec {
    fn new(key: TeamKey, source: TeamSource) -> TeamRec {
        TeamRec {
            key,
            source,
            name: String::new(),
            raw_id: String::new(),
            project: None,
            lead: None,
            binding: LeadBinding::None,
            caps: TeamCapabilities::all_native(),
            details: TeamDetails::default(),
            members: HashMap::new(),
            member_facts: HashMap::new(),
            tasks: HashMap::new(),
            messages: BTreeMap::new(),
            message_at: HashMap::new(),
            in_flight: None,
            ended_at: None,
            attributions: Vec::new(),
            plan_reviews: Vec::new(),
            question_locations: HashMap::new(),
            pub_team: None,
            pub_members: HashMap::new(),
            pub_tasks: HashMap::new(),
            pub_messages: HashSet::new(),
            pub_message_values: HashMap::new(),
            recheck_wall: None,
        }
    }

    /// A Team row is shown only with ≥ 1 member besides the lead.
    pub fn visible(&self) -> bool {
        self.members.values().any(|m| m.role == MemberRole::Member)
    }

    fn lead_member(&self) -> Option<&Member> {
        self.lead.as_ref().and_then(|k| self.members.get(k)).or_else(|| self.members.values().find(|m| m.role == MemberRole::Lead))
    }

    pub fn lead_uid(&self) -> Option<SessionUid> {
        self.lead_member().and_then(|m| m.session_uid.clone())
    }

    /// Members: lead first, then `joinedAt` order (ties by key).
    pub fn ordered_members(&self) -> Vec<&Member> {
        let mut v: Vec<&Member> = self.members.values().collect();
        v.sort_by(|a, b| {
            let la = a.role != MemberRole::Lead;
            let lb = b.role != MemberRole::Lead;
            la.cmp(&lb)
                .then_with(|| a.joined_at.unwrap_or(i64::MAX).cmp(&b.joined_at.unwrap_or(i64::MAX)))
                .then_with(|| a.key.as_str().cmp(b.key.as_str()))
        });
        v
    }

    fn has_live_panes(&self) -> bool {
        self.members
            .values()
            .any(|m| m.role == MemberRole::Member && m.backend == MemberBackend::Pane && m.presence == Presence::Present)
    }

    /// The member key of a name (current members first, present first).
    pub fn member_by_name(&self, name: &str) -> Option<MemberKey> {
        let mut c: Vec<&Member> = self.members.values().filter(|m| m.name == name).collect();
        c.sort_by_key(|m| (m.presence != Presence::Present, std::cmp::Reverse(m.joined_at)));
        c.first().map(|m| m.key.clone())
    }
}

/// What applying a team event changed outside the team itself.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TeamEffects {
    /// S4 facts for members that have session rows.
    pub session_team_facts: Vec<(SessionUid, TeamFileFacts)>,
    /// Sessions whose team role or position may have changed.
    pub dirty_sessions: Vec<SessionUid>,
}

/// Every team World knows.
#[derive(Debug, Default)]
pub struct TeamsState {
    pub teams: HashMap<TeamKey, TeamRec>,
    pub dirty: HashSet<TeamKey>,
}

/// The pure station function (§3.4.8, pixel-office §2.1, §2.10).
pub fn station(t: &Task, blocked_open: bool, owner_band12: bool) -> Station {
    match t.status {
        TaskStatus::Done => {
            if t.inferred {
                Station::Reported
            } else {
                Station::Done
            }
        }
        TaskStatus::InProgress => {
            if owner_band12 {
                Station::NeedsYou
            } else {
                Station::InProgress
            }
        }
        TaskStatus::Open => {
            if t.inferred {
                Station::Assigned
            } else if blocked_open {
                Station::Blocked
            } else if t.owner.is_some() {
                Station::Assigned
            } else {
                Station::ToDo
            }
        }
        TaskStatus::Deleted => Station::ToDo,
    }
}

/// The glyph rank of ux-ia §4.9 (lower is more urgent).
fn glyph_rank(s: &ActivityState) -> u8 {
    match s {
        ActivityState::NeedsApproval => 0,
        ActivityState::NeedsAnswer => 1,
        ActivityState::Error { .. } | ActivityState::Crashed { .. } => 2,
        ActivityState::NoProgress { .. } => 3,
        ActivityState::NewReply => 4,
        ActivityState::Working => 5,
        ActivityState::Ready => 6,
        _ => 7,
    }
}

/// A member's state from S4 alone (members without a session row).
pub fn member_state_from_facts(f: &TeamFileFacts, gone: bool) -> Option<ActivityState> {
    if gone {
        return Some(ActivityState::Ended);
    }
    if let Some((_, IdleReason::Failed)) = &f.last_idle {
        if f.is_active != Some(true) {
            return Some(ActivityState::Error { sub: ErrorSub::AgentError });
        }
    }
    if f.permission_request.is_some() {
        return Some(ActivityState::NeedsApproval);
    }
    match f.is_active {
        Some(true) => Some(ActivityState::Working),
        Some(false) => Some(ActivityState::Ready),
        None => f.last_idle.as_ref().map(|_| ActivityState::Ready),
    }
}

impl TeamsState {
    fn rec(&mut self, key: &TeamKey, source: TeamSource) -> &mut TeamRec {
        self.teams.entry(key.clone()).or_insert_with(|| TeamRec::new(key.clone(), source))
    }

    fn team_of_member(&self, m: &MemberKey) -> Option<TeamKey> {
        self.teams.iter().find(|(_, t)| t.members.contains_key(m)).map(|(k, _)| k.clone())
    }

    /// Adapter-side details (Claude native).
    pub fn set_details(&mut self, key: &TeamKey, source: TeamSource, details: TeamDetails) {
        let rec = self.rec(key, source);
        if rec.details != details {
            rec.details = details;
            self.dirty.insert(key.clone());
        }
    }

    /// Apply one adapter event. `caps` is the adapter's descriptor for a
    /// `TeamSeen` (read before World was locked).
    pub fn apply(&mut self, source: TeamSource, ev: TeamEvent, caps: Option<TeamCapabilities>) -> TeamEffects {
        let mut fx = TeamEffects::default();
        match ev {
            TeamEvent::TeamSeen { key, name, raw_id, project, lead, binding } => {
                let rec = self.rec(&key, source);
                rec.source = source;
                rec.name = name;
                rec.raw_id = raw_id;
                if project.is_some() {
                    rec.project = project;
                }
                rec.lead = lead;
                rec.binding = binding;
                if let Some(c) = caps {
                    rec.caps = c;
                }
                fx.dirty_sessions.extend(rec.members.values().filter_map(|m| m.session_uid.clone()));
                self.dirty.insert(key);
            }
            TeamEvent::TeamEnded { key, at } => {
                if let Some(rec) = self.teams.get_mut(&key) {
                    if rec.ended_at.is_none() {
                        rec.ended_at = Some(at);
                    }
                    fx.dirty_sessions.extend(rec.members.values().filter_map(|m| m.session_uid.clone()));
                    self.dirty.insert(key);
                }
            }
            TeamEvent::MemberUpserted(m) => {
                let key = m.team_key.clone();
                let rec = self.rec(&key, source);
                let is_new = !rec.members.contains_key(&m.key);
                if is_new && rec.members.len() >= MEMBERS_CAP {
                    return fx;
                }
                if let Some(old) = rec.members.get(&m.key) {
                    if let Some(u) = &old.session_uid {
                        fx.dirty_sessions.push(u.clone());
                    }
                }
                if let Some(u) = &m.session_uid {
                    fx.dirty_sessions.push(u.clone());
                }
                if m.role == MemberRole::Lead && rec.lead.is_none() {
                    rec.lead = Some(m.key.clone());
                }
                rec.members.insert(m.key.clone(), m);
                self.dirty.insert(key);
            }
            TeamEvent::MemberGone { key, reason, at } => {
                if let Some(t) = self.team_of_member(&key) {
                    let rec = self.teams.get_mut(&t).expect("team found");
                    if let Some(m) = rec.members.get_mut(&key) {
                        if m.presence == Presence::Present {
                            m.presence = Presence::Gone { reason, at };
                        }
                        if let Some(u) = &m.session_uid {
                            fx.dirty_sessions.push(u.clone());
                        }
                    }
                    self.dirty.insert(t);
                }
            }
            TeamEvent::MessageObserved(msg) => {
                let key = msg.team_key.clone();
                let rec = self.rec(&key, source);
                if let Some(at) = rec.message_at.remove(&msg.key) {
                    rec.messages.remove(&(at, msg.key.clone()));
                }
                rec.message_at.insert(msg.key.clone(), msg.at);
                rec.messages.insert((msg.at, msg.key.clone()), msg);
                while rec.messages.len() > MEMORY_MESSAGES {
                    if let Some(((_, k), _)) = rec.messages.pop_first() {
                        rec.message_at.remove(&k);
                    }
                }
                self.dirty.insert(key);
            }
            TeamEvent::MessageStatus { key, status } => {
                for (tk, rec) in self.teams.iter_mut() {
                    if let Some(at) = rec.message_at.get(&key).copied() {
                        if let Some(m) = rec.messages.get_mut(&(at, key.clone())) {
                            m.status = status;
                        }
                        self.dirty.insert(tk.clone());
                        break;
                    }
                }
            }
            TeamEvent::TaskUpserted(mut t) => {
                let key = t.team_key.clone();
                let rec = self.rec(&key, source);
                if !rec.tasks.contains_key(&t.key) && rec.tasks.len() >= TASKS_CAP {
                    return fx;
                }
                if t.inferred {
                    t.title = None;
                    t.blocked_by.clear();
                    t.blocks.clear();
                }
                rec.tasks.insert(t.key.clone(), t);
                self.dirty.insert(key);
            }
            TeamEvent::TaskRemoved { key } => {
                for (tk, rec) in self.teams.iter_mut() {
                    if rec.tasks.remove(&key).is_some() {
                        for t in rec.tasks.values_mut() {
                            t.blocked_by.retain(|k| k != &key);
                            t.blocks.retain(|k| k != &key);
                        }
                        self.dirty.insert(tk.clone());
                        break;
                    }
                }
            }
            TeamEvent::TaskBoardReset { key, .. } => {
                // The board stays "finished", never empty (§7.2.5): nothing
                // is removed; the adapter's event log keeps the cards.
                self.dirty.insert(key);
            }
            TeamEvent::MemberFacts { member, facts } => {
                if let Some(t) = self.team_of_member(&member) {
                    let rec = self.teams.get_mut(&t).expect("team found");
                    match rec.members.get(&member).and_then(|m| m.session_uid.clone()) {
                        Some(uid) => fx.session_team_facts.push((uid, facts)),
                        None => {
                            rec.member_facts.insert(member, facts);
                        }
                    }
                    self.dirty.insert(t);
                }
            }
            TeamEvent::ApprovalAttributed { member, lead, at, .. } => {
                if let Some(t) = self.team_of_member(&member) {
                    let rec = self.teams.get_mut(&t).expect("team found");
                    rec.attributions.push((lead.clone(), Attribution { member, question: false, at }));
                    trim_attributions(&mut rec.attributions);
                    fx.dirty_sessions.push(lead);
                }
            }
            TeamEvent::QuestionAttributed { member, location, at } => {
                if let Some(t) = self.team_of_member(&member) {
                    let rec = self.teams.get_mut(&t).expect("team found");
                    rec.question_locations.insert(member.clone(), location);
                    if location == QuestionLocation::Lead {
                        if let Some(lead) = rec.lead_uid() {
                            rec.attributions.push((lead.clone(), Attribution { member, question: true, at }));
                            trim_attributions(&mut rec.attributions);
                            fx.dirty_sessions.push(lead);
                        }
                    }
                }
            }
            TeamEvent::PlanReview { member, at } => {
                if let Some(t) = self.team_of_member(&member) {
                    let rec = self.teams.get_mut(&t).expect("team found");
                    rec.plan_reviews.push((member, at));
                    if rec.plan_reviews.len() > 64 {
                        rec.plan_reviews.remove(0);
                    }
                    self.dirty.insert(t);
                }
            }
            TeamEvent::InFlight { team, count } => {
                let rec = self.rec(&team, source);
                if rec.in_flight != count {
                    rec.in_flight = count;
                    self.dirty.insert(team);
                }
            }
        }
        fx
    }

    /// Every session's team role.
    pub fn roles(&self) -> HashMap<SessionUid, RoleInfo> {
        let mut out = HashMap::new();
        for rec in self.teams.values() {
            let lead_uid = rec.lead_uid();
            let live_panes = rec.has_live_panes();
            let hosted = rec.binding == LeadBinding::Hosted;
            for m in rec.members.values() {
                let Some(uid) = &m.session_uid else { continue };
                // A newer team's role wins over an ended one's (a resumed lead).
                let info = RoleInfo {
                    team_key: rec.key.clone(),
                    member_key: m.key.clone(),
                    role: m.role,
                    colour: m.colour,
                    backend: m.backend,
                    team_hosted: hosted,
                    live_panes: m.role == MemberRole::Lead && live_panes && rec.ended_at.is_none(),
                    lead_uid: if m.role == MemberRole::Member { lead_uid.clone() } else { None },
                    published: rec.visible(),
                    config_mtime: rec.details.config_mtime,
                    source: rec.source,
                };
                let replace = match out.get(uid) {
                    None => true,
                    Some(prev) => {
                        let prev: &RoleInfo = prev;
                        let prev_ended = self.teams.get(&prev.team_key).and_then(|t| t.ended_at).is_some();
                        prev_ended && rec.ended_at.is_none()
                    }
                };
                if replace {
                    out.insert(uid.clone(), info);
                }
            }
        }
        out
    }

    /// The attributions recorded for a lead since `since` (newest first).
    pub fn attributions_for(&self, lead: &SessionUid, since: EpochMs) -> Vec<Attribution> {
        let mut v: Vec<Attribution> = self
            .teams
            .values()
            .flat_map(|t| t.attributions.iter())
            .filter(|(l, a)| l == lead && a.at >= since)
            .map(|(_, a)| a.clone())
            .collect();
        v.sort_by_key(|a| std::cmp::Reverse(a.at));
        v
    }

    /// The member named `name` in the team led by `lead`.
    pub fn member_of_lead_by_name(&self, lead: &SessionUid, name: &str) -> Option<MemberKey> {
        self.teams
            .values()
            .filter(|t| t.lead_uid().as_ref() == Some(lead) && t.ended_at.is_none())
            .find_map(|t| t.member_by_name(name))
    }

    /// The session of a member.
    pub fn member_session(&self, m: &MemberKey) -> Option<SessionUid> {
        self.teams.values().find_map(|t| t.members.get(m)).and_then(|m| m.session_uid.clone())
    }

    /// Recompute the derived values of every dirty team and publish what
    /// changed. Returns the earliest wall time a bottleneck could fire.
    pub fn publish(&mut self, view: &dyn SessionView, outbox: &mut Outbox, now_wall: EpochMs, all: bool) -> Option<EpochMs> {
        let keys: Vec<TeamKey> = if all {
            self.teams.keys().cloned().collect()
        } else {
            self.dirty.drain().collect()
        };
        if all {
            self.dirty.clear();
        }
        for k in keys {
            if let Some(rec) = self.teams.get_mut(&k) {
                publish_team(rec, view, outbox, now_wall);
            }
        }
        self.teams.values().filter_map(|t| t.recheck_wall).min()
    }

    /// Mark every team whose members include one of these sessions dirty.
    pub fn touch_sessions(&mut self, uids: &HashSet<SessionUid>) {
        for (k, rec) in &self.teams {
            if rec.members.values().any(|m| m.session_uid.as_ref().is_some_and(|u| uids.contains(u))) {
                self.dirty.insert(k.clone());
            }
        }
    }

    /// Re-point member rows from a merged/rekeyed uid to its successor.
    pub fn remap_session(&mut self, from: &SessionUid, to: &SessionUid) {
        for (k, rec) in self.teams.iter_mut() {
            let mut hit = false;
            for m in rec.members.values_mut() {
                if m.session_uid.as_ref() == Some(from) {
                    m.session_uid = Some(to.clone());
                    hit = true;
                }
            }
            if hit {
                self.dirty.insert(k.clone());
            }
        }
    }

    /// Messages of a team for `team_messages_page` (newest first, before `before`).
    pub fn messages_page(&self, team: &TeamKey, before: EpochMs, count: usize) -> (Vec<TeamMessage>, bool) {
        let Some(rec) = self.teams.get(team) else { return (Vec::new(), false) };
        let mut v: Vec<TeamMessage> =
            rec.messages.iter().rev().filter(|((at, _), _)| *at < before).map(|(_, m)| m.clone()).collect();
        let more = v.len() > count;
        v.truncate(count);
        (v, more)
    }

    /// A27.1: a message World still holds (also one older than the store's
    /// newest 500), its team's source and each end's session and native name.
    pub fn message_ends(&self, key: &MessageKey) -> Option<MessageEnds> {
        let rec = self.teams.values().find(|r| r.message_at.contains_key(key))?;
        let at = rec.message_at.get(key)?;
        let message = rec.messages.get(&(*at, key.clone()))?.clone();
        let end = |p: &Party| match p {
            Party::Member { member } => rec
                .members
                .get(member)
                .map(|m| MessageEnd { session: m.session_uid.clone(), name: Some(native_name(m).to_string()) })
                .unwrap_or_default(),
            Party::User | Party::System => MessageEnd::default(),
        };
        let (from, to) = (end(&message.from), end(&message.to));
        Some(MessageEnds { source: rec.source, message, from, to })
    }
}

/// One end of a team message (A27.1).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MessageEnd {
    /// The member's session (`None` for the user, the system, or a member
    /// without a session row yet).
    pub session: Option<SessionUid>,
    /// The native name a transcript uses for the member (`teammate_id`, the
    /// `SendMessage` `to`): the agent id before `@`, else the display name.
    pub name: Option<String>,
}

/// A message World holds and its two ends (A27.1).
#[derive(Debug, Clone, PartialEq)]
pub struct MessageEnds {
    pub source: TeamSource,
    pub message: TeamMessage,
    pub from: MessageEnd,
    pub to: MessageEnd,
}

/// The agent id before `@` (`alpha@session-…` → `alpha`), else the name.
fn native_name(m: &Member) -> &str {
    m.agent_id.as_deref().and_then(|a| a.split('@').next()).filter(|n| !n.is_empty()).unwrap_or(&m.name)
}

fn trim_attributions(v: &mut Vec<(SessionUid, Attribution)>) {
    if v.len() > 64 {
        let n = v.len() - 64;
        v.drain(..n);
    }
}

/// The capability descriptor filtered by what is possible now (§7.2.3): a
/// via-lead route needs the lead In Gilde; a member-pane route needs a hosted
/// team; a gone lead offers nothing.
fn effective_caps(rec: &TeamRec, lead_in_gilde: bool) -> TeamCapabilities {
    let mut c = rec.caps.clone();
    let hosted = rec.binding == LeadBinding::Hosted;
    let ended = rec.ended_at.is_some();
    let keep_send = |r: &SendRoute| match r {
        SendRoute::Native => !ended,
        SendRoute::ViaLead => lead_in_gilde && !ended,
        SendRoute::ViaMemberPane => hosted && !ended,
    };
    c.act_send.retain(keep_send);
    c.act_broadcast.retain(keep_send);
    c.act_spawn.retain(keep_send);
    c.act_stop.retain(|r| match r {
        StopRoute::Native => !ended,
        StopRoute::ViaLead => lead_in_gilde && !ended,
        StopRoute::ClosePane => hosted && !ended,
    });
    c
}

fn publish_team(rec: &mut TeamRec, view: &dyn SessionView, outbox: &mut Outbox, now_wall: EpochMs) {
    let lead_uid = rec.lead_uid();
    let lead_in_gilde = lead_uid.as_ref().is_some_and(|u| view.in_gilde(u));
    // Member state (session row first, else S4 facts).
    let state_of = |m: &Member| -> Option<(ActivityState, Option<u8>)> {
        if let Some(u) = &m.session_uid {
            if let Some(s) = view.state(u) {
                return Some(s);
            }
        }
        let gone = matches!(m.presence, Presence::Gone { .. });
        rec.member_facts
            .get(&m.key)
            .and_then(|f| member_state_from_facts(f, gone))
            .map(|s| {
                let b = crate::world::derive::band(&s);
                (s, b)
            })
    };
    let member_states: HashMap<MemberKey, (ActivityState, Option<u8>)> =
        rec.members.values().filter_map(|m| state_of(m).map(|s| (m.key.clone(), s))).collect();
    // Stations.
    let done: HashSet<TaskKey> = rec
        .tasks
        .values()
        .filter(|t| matches!(t.status, TaskStatus::Done | TaskStatus::Deleted))
        .map(|t| t.key.clone())
        .collect();
    let deleted: HashSet<TaskKey> =
        rec.tasks.values().filter(|t| t.status == TaskStatus::Deleted).map(|t| t.key.clone()).collect();
    let keys: Vec<TaskKey> = rec.tasks.keys().cloned().collect();
    for k in keys {
        let t = rec.tasks.get_mut(&k).expect("task");
        t.blocked_by.retain(|b| !deleted.contains(b));
        t.blocks.retain(|b| !deleted.contains(b));
        let blocked_open = !t.inferred && t.blocked_by.iter().any(|b| !done.contains(b));
        let owner_band12 = t
            .owner
            .as_ref()
            .and_then(|o| member_states.get(o))
            .is_some_and(|(_, b)| matches!(b, Some(1) | Some(2)));
        t.station = station(t, blocked_open, owner_band12);
    }
    // Current task per member: the newest InProgress task it owns.
    let mut current: HashMap<MemberKey, (EpochMs, TaskKey)> = HashMap::new();
    for t in rec.tasks.values().filter(|t| t.status == TaskStatus::InProgress) {
        if let Some(o) = &t.owner {
            let at = t.started_at.or(t.first_seen_in_progress_at).or(t.created_at).unwrap_or(0);
            let e = current.entry(o.clone()).or_insert((at, t.key.clone()));
            if at > e.0 {
                *e = (at, t.key.clone());
            }
        }
    }
    for m in rec.members.values_mut() {
        m.current_task = current.get(&m.key).map(|(_, k)| k.clone());
    }
    let tasks: Vec<Task> = rec.tasks.values().cloned().collect();
    let (bn, recheck) = bottlenecks(&tasks, rec.lead.as_ref(), now_wall);
    rec.recheck_wall = recheck;
    let live: Vec<&Task> = tasks.iter().filter(|t| t.status != TaskStatus::Deleted).collect();
    let inferred = (!live.is_empty() && live.iter().all(|t| t.inferred))
        || rec.caps.observe_tasks == crate::contract::team::Support::Inferred;
    // Data-switched clauses of the lead's version (absent = fallback).
    let native = rec.source == TeamSource::ClaudeNative;
    let store_unverified =
        native && lead_uid.as_ref().is_some_and(|u| view.feature(u, FLAG_TEAM_STORE_VERIFIED) == Some(false));
    let idle_per_turn = lead_uid.as_ref().is_some_and(|u| view.feature(u, FLAG_IDLE_PER_TURN) == Some(true));
    let done_n = live.iter().filter(|t| t.status == TaskStatus::Done).count() as u32;
    let approx = store_unverified
        || (inferred && native && !idle_per_turn)
        || (inferred
            && rec.members.values().any(|m| {
                m.role == MemberRole::Member
                    && m.presence == Presence::Present
                    && m.session_uid.is_none()
                    && m.backend == MemberBackend::Pane
            }));
    let progress = TeamProgress {
        done: done_n,
        total: live.len() as u32,
        inferred,
        approx,
        in_flight: if store_unverified { None } else { rec.in_flight },
        bottlenecks: bn,
    };
    let glyph = rec
        .members
        .values()
        .filter_map(|m| member_states.get(&m.key).map(|(s, _)| s.clone()))
        .min_by_key(glyph_rank)
        .filter(|s| glyph_rank(s) < 7)
        .unwrap_or(ActivityState::Ready);
    let project = rec
        .project
        .clone()
        .or_else(|| lead_uid.as_ref().and_then(|u| view.project(u)))
        .or_else(|| rec.members.values().filter_map(|m| m.session_uid.as_ref()).find_map(|u| view.project(u)));
    let hosted = rec.binding == LeadBinding::Hosted;
    let pane_members = rec.members.values().any(|m| m.role == MemberRole::Member && m.backend == MemberBackend::Pane);
    let in_process_members =
        rec.members.values().any(|m| m.role == MemberRole::Member && m.backend == MemberBackend::InProcess);
    let pane_hosting = rec.details.pane_hosting.unwrap_or(match (hosted, pane_members, in_process_members) {
        (true, true, _) => PaneHostingState::Hosted,
        (true, false, true) => PaneHostingState::InProcessFallback,
        (false, true, _) => PaneHostingState::OutsideGilde,
        _ => PaneHostingState::NotApplicable,
    });
    // A backgrounded hosted lead keeps its panes: `Background` (§7.4.7).
    let external_host = match rec.details.external_host {
        Some(ExternalHost::Background) => Some(ExternalHost::Background),
        _ if hosted || !pane_members => None,
        d => Some(d.unwrap_or(ExternalHost::Unknown)),
    };
    let started_at = rec
        .details
        .started_at
        .or_else(|| rec.members.values().filter_map(|m| m.joined_at).min());
    let team = Team {
        key: rec.key.clone(),
        source: rec.source,
        name: rec.name.clone(),
        raw_id: rec.raw_id.clone(),
        project_id: project,
        lead: rec.lead.clone(),
        lead_binding: rec.binding,
        hosted,
        external_host,
        caps: {
            let mut c = effective_caps(rec, lead_in_gilde);
            if store_unverified {
                c.observe_in_flight = crate::contract::team::Support::Unknown;
            }
            c
        },
        glyph_state: glyph,
        progress,
        started_at,
        ended_at: rec.ended_at,
        pane_hosting,
        roster_stale: rec.details.roster_stale,
        // A30.1: hosted teams only, ≤ 16, in creation order.
        pane_tiles: if hosted {
            rec.details.pane_tiles.iter().take(PANE_TILES_CAP).cloned().collect()
        } else {
            Vec::new()
        },
    };
    if !rec.visible() {
        // Not shown yet (§3.4.6): nothing in the store.
        return;
    }
    if rec.pub_team.as_ref() != Some(&team) {
        outbox.upsert(Entity::Team { value: team.clone() });
        rec.pub_team = Some(team);
    }
    for m in rec.ordered_members() {
        if rec.pub_members.get(&m.key) != Some(m) {
            outbox.upsert(Entity::Member { value: m.clone() });
        }
    }
    rec.pub_members = rec.members.clone();
    for t in rec.tasks.values() {
        if rec.pub_tasks.get(&t.key) != Some(t) {
            outbox.upsert(Entity::Task { value: t.clone() });
        }
    }
    for k in rec.pub_tasks.keys() {
        if !rec.tasks.contains_key(k) {
            outbox.remove(EntityKind::Task, k.as_str());
        }
    }
    rec.pub_tasks = rec.tasks.clone();
    // The newest 500 messages.
    let newest: Vec<&TeamMessage> = rec.messages.values().rev().take(STORE_MESSAGES).collect();
    let keep: HashSet<MessageKey> = newest.iter().map(|m| m.key.clone()).collect();
    for m in &newest {
        if rec.pub_message_values.get(&m.key) != Some(*m) {
            outbox.upsert(Entity::Message { value: (*m).clone() });
        }
    }
    for k in rec.pub_messages.iter() {
        if !keep.contains(k) {
            outbox.remove(EntityKind::Message, k.as_str());
        }
    }
    rec.pub_message_values = newest.iter().map(|m| (m.key.clone(), (*m).clone())).collect();
    rec.pub_messages = keep;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::common::Vendor;
    use crate::contract::team::TaskSource;

    fn task(status: TaskStatus, owner: bool, inferred: bool) -> Task {
        let team = TeamKey::derive("claudeNative", None, "t");
        Task {
            key: TaskKey::derive(&team, "1"),
            team_key: team.clone(),
            native_id: None,
            title: None,
            inferred_k: None,
            status,
            station: Station::ToDo,
            owner: owner.then(|| MemberKey::derive(&team, "a@t", None)),
            blocked_by: vec![],
            blocks: vec![],
            created_at: None,
            started_at: None,
            completed_at: None,
            first_seen_in_progress_at: None,
            source: TaskSource::File,
            inferred,
        }
    }

    #[test]
    fn station_table() {
        assert_eq!(station(&task(TaskStatus::Open, false, false), false, false), Station::ToDo);
        assert_eq!(station(&task(TaskStatus::Open, true, false), false, false), Station::Assigned);
        assert_eq!(station(&task(TaskStatus::Open, true, false), true, false), Station::Blocked);
        assert_eq!(station(&task(TaskStatus::InProgress, true, false), false, true), Station::NeedsYou);
        assert_eq!(station(&task(TaskStatus::Done, true, false), false, false), Station::Done);
        // Inferred: {Assigned, InProgress, NeedsYou, Reported} only.
        assert_eq!(station(&task(TaskStatus::Open, true, true), true, false), Station::Assigned);
        assert_eq!(station(&task(TaskStatus::Done, true, true), false, false), Station::Reported);
    }

    #[test]
    fn member_facts_without_a_session() {
        let f = TeamFileFacts { is_active: Some(true), in_roster: true, permission_request: None, last_idle: None };
        assert_eq!(member_state_from_facts(&f, false), Some(ActivityState::Working));
        assert_eq!(member_state_from_facts(&f, true), Some(ActivityState::Ended));
        let g = TeamFileFacts { is_active: Some(false), in_roster: true, permission_request: None, last_idle: Some((1, IdleReason::Failed)) };
        assert_eq!(member_state_from_facts(&g, false), Some(ActivityState::Error { sub: ErrorSub::AgentError }));
        let _ = Vendor::Claude;
    }
}
