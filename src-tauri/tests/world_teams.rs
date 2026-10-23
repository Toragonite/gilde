//! T-team-* on synthetic facts (§7.1–§7.3): the Claude native adapter fed
//! with T03 team observations and T04 structure rows, the team ↔ lead join,
//! presence, member keys, the message log join, inbox hints, attribution
//! (header → inbox → none), Q3, inferred items, the board reset, Esc turns,
//! no task tools, "never no messages", closed panes, the in-process
//! fallback, forks, send routes, the static no-write check (T-team-nowrite),
//! the §7.1.4 rules with a scripted adapter, team-level values, and the
//! orchestrator mapping.

#[path = "world_support.rs"]
mod support;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use parking_lot::Mutex;

use gilde_code_lib::contract::activity::{ActivityState, PtyStatus, RegStatus};
use gilde_code_lib::contract::common::{EpochMs, Vendor};
use gilde_code_lib::contract::content::UserOrigin;
use gilde_code_lib::contract::ids::{MemberKey, MessageKey, ProcessKey, RequestId, SessionUid, TaskKey, TeamKey};
use gilde_code_lib::contract::interaction::{Decision, InteractionKind};
use gilde_code_lib::contract::services::{IdentityHint, SessionMetaFacts, SourceFacts, WorldApi};
use gilde_code_lib::contract::session::{ControlMode, MemberRole};
use gilde_code_lib::contract::team::{
    ActOutcome, DeliveryStatus, GoneReason, LeadBinding, Member, MemberBackend, MemberSpec, MessageKind, PaneHostingState,
    Party, PrefillAction, Presence, SendRoute, Station, StopHow, Support, Task, TaskOp, TaskSource, TaskStatus,
    TeamAdapter, TeamCapabilities, TeamEvent, TeamEventSink, TeamMessage, TeamSource,
};
use gilde_code_lib::discovery::claude::teams::{
    InboxEntry, InboxFrame, LeadJoin, TaskFile, TasksRead, TeamConfig, TeamDirRead, TeamMemberRow,
};
use gilde_code_lib::discovery::reconcile::{LiveMember, TeamObservation, TeamObserver};
use gilde_code_lib::discovery::safe_read::FileId;
use gilde_code_lib::follow::tail::RowSink;
use gilde_code_lib::render::transcript::{RowFacts, SendMessageFact, TaskOpFact, ToolUseFact, TurnEndKind, WrappedFact};
use gilde_code_lib::world::teams::claude::MESSAGES_UNKNOWN_NOTE;
use gilde_code_lib::world::teams::orchestrator::orchestrator_caps;
use support::*;

const DIR: &str = "session-25ae1157";

fn lead_sid() -> String {
    sid(0x326555f6)
}
fn alpha_sid() -> String {
    sid(0xa1)
}

fn row(name: &str, agent_type: &str, backend: &str, pane: &str, joined: EpochMs, active: Option<bool>) -> TeamMemberRow {
    TeamMemberRow {
        agent_id: format!("{name}@{DIR}"),
        name: name.into(),
        agent_type: Some(agent_type.into()),
        model: None,
        color: Some("blue".into()),
        backend_type: Some(backend.into()),
        tmux_pane_id: Some(pane.into()),
        cwd: None,
        joined_at: Some(joined),
        is_active: active,
        plan_mode_required: Some(false),
        mode: None,
    }
}

struct Scene {
    created: EpochMs,
    members: Vec<TeamMemberRow>,
    live: Vec<LiveMember>,
    inboxes: BTreeMap<String, Vec<InboxEntry>>,
    tasks: TasksRead,
    lead_alive: bool,
    binding: LeadBinding,
    mtime: EpochMs,
}

impl Scene {
    fn new(rig: &Rig, binding: LeadBinding) -> Scene {
        let created = rig.now() - 60_000;
        Scene {
            created,
            members: vec![
                row("team-lead", "team-lead", "in-process", "leader", created, None),
                row("alpha", "general-purpose", "tmux", "%1", created + 10_000, Some(true)),
            ],
            live: vec![LiveMember {
                agent_id: format!("alpha@{DIR}"),
                agent_name: "alpha".into(),
                process: ProcessKey::new(7101, 1_792_700_000),
                pid: 7101,
                session_id: Some(alpha_sid()),
            }],
            inboxes: BTreeMap::new(),
            tasks: TasksRead::default(),
            lead_alive: true,
            binding,
            mtime: created + 10_000,
        }
    }

    fn obs(&self, rig: &Rig) -> TeamObservation {
        TeamObservation {
            account: account(),
            dir: TeamDirRead {
                dir_name: DIR.into(),
                config: Some(TeamConfig {
                    name: Some("auth-split".into()),
                    created_at: Some(self.created),
                    lead_agent_id: Some(format!("team-lead@{DIR}")),
                    lead_session_id: Some("25ae1157".into()),
                    members: self.members.clone(),
                    members_truncated: false,
                }),
                config_id: Some(FileId { size: 1, mtime_ms: self.mtime, inode: 1, dev: 1 }),
                roster_stale: false,
                torn: 0,
                inboxes: self.inboxes.clone(),
                inboxes_skipped: 0,
            },
            join: LeadJoin { binding: self.binding, lead_session_id: Some(lead_sid()), lead_pid: Some(7100) },
            lead_alive: self.lead_alive,
            live_members: self.live.clone(),
            tasks: self.tasks.clone(),
            in_process: vec![],
            visible: true,
            at: rig.now(),
        }
    }
}

fn team_key() -> TeamKey {
    TeamKey::derive("claudeNative", Some(&account()), DIR)
}

fn member_key(name: &str, joined: EpochMs) -> MemberKey {
    MemberKey::derive(&team_key(), &format!("{name}@{DIR}"), Some(joined))
}

/// 2.1.307 with the flags its probes froze (resources/claude-compat.json).
fn compat_307(extra: &[(&str, bool)]) -> gilde_code_lib::contract::compat::ClaudeCompat {
    use gilde_code_lib::contract::common::CompatState;
    use gilde_code_lib::contract::compat::{CanaryResult, ClaudeCompat, CompatSource};
    let mut features: BTreeMap<String, bool> = [
        ("leadPromptsForTeammates", true),
        ("teammateQuestionLocation", true),
        ("idlePerTurn", true),
        ("sendMessageMsgId", true),
        ("inFlightSignals", false),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    for (k, v) in extra {
        features.insert(k.to_string(), *v);
    }
    ClaudeCompat {
        version: "2.1.307".into(),
        conversation: CompatState::Unverified,
        pane_hosting: CompatState::Verified,
        source: CompatSource::Shipped,
        static_canary: CanaryResult::Pass,
        handshake_ok: None,
        below_minimum: false,
        note: None,
        features,
    }
}

/// The lead (a Gilde Terminal session, hosted or not) and the pane member's
/// evidence; the lead runs 2.1.307.
fn sessions(rig: &Rig, hosted: bool) -> (SessionUid, SessionUid) {
    sessions_with(rig, hosted, Some(compat_307(&[])))
}

fn sessions_with(rig: &Rig, hosted: bool, compat: Option<gilde_code_lib::contract::compat::ClaudeCompat>) -> (SessionUid, SessionUid) {
    if let Some(c) = compat {
        rig.sink.compat(vec![c]);
    }
    let lead_h = if hosted { spawn_hint("sp-lead", Some(&lead_sid()), Some(7100)) } else { claude_hint(Some(&lead_sid()), Some(7100)) };
    if hosted {
        rig.world.bind_spawn("sp-lead", None, Vendor::Claude, Some(account()));
        let t = rig.now() - 70_000;
        rig.ev(&lead_h, SourceFacts::Gilde(gilde(t, ControlMode::Terminal, Some(t + 1_000))));
    }
    rig.ev(&lead_h, alive(7100));
    rig.ev(&lead_h, SourceFacts::Meta(Box::new(SessionMetaFacts { claude_version: Some("2.1.307".into()), ..SessionMetaFacts::default() })));
    rig.ev(&lead_h, registry(RegStatus::Busy, rig.now() - 50_000, None));
    rig.ev(&lead_h, SourceFacts::Pty(pty(PtyStatus::Idle, rig.now() - 40_000)));
    let alpha_h = IdentityHint {
        gilde_spawn_id: hosted.then(|| "sp-pane-1".to_string()),
        ..claude_hint(Some(&alpha_sid()), Some(7101))
    };
    rig.ev(&alpha_h, alive(7101));
    (rig.uid_of(&lead_sid()), rig.uid_of(&alpha_sid()))
}

fn alpha_hint(hosted: bool) -> IdentityHint {
    IdentityHint { gilde_spawn_id: hosted.then(|| "sp-pane-1".to_string()), ..claude_hint(Some(&alpha_sid()), Some(7101)) }
}

fn rows(rig: &Rig, vsid: &str, rows: Vec<RowFacts>) {
    let a = rig.world.claude_team_adapter();
    let hint = claude_hint(Some(vsid), None);
    a.rows(Path::new("/x.jsonl"), &hint, &rows);
}

fn observe(rig: &Rig, s: &Scene) {
    rig.world.claude_team_adapter().team(s.obs(rig));
}

fn snap_team(rig: &Rig) -> gilde_code_lib::contract::team::Team {
    rig.world.snapshot().teams.into_iter().find(|t| t.key == team_key()).expect("team published")
}

fn messages(rig: &Rig) -> Vec<TeamMessage> {
    rig.world.snapshot().messages
}

fn tasks(rig: &Rig) -> Vec<Task> {
    rig.world.snapshot().tasks
}

fn send_row(at: EpochMs, tool_use_id: &str, to: &str) -> RowFacts {
    RowFacts {
        row_type: "assistant".into(),
        at: Some(at),
        uuid: Some(format!("u-{tool_use_id}")),
        tool_uses: vec![ToolUseFact { id: tool_use_id.into(), name: "SendMessage".into() }],
        send_messages: vec![SendMessageFact { tool_use_id: tool_use_id.into(), to: Some(to.into()) }],
        ..RowFacts::default()
    }
}

fn result_row(at: EpochMs, tool_use_id: &str, msg_id: Option<&str>) -> RowFacts {
    RowFacts {
        row_type: "user".into(),
        at: Some(at),
        uuid: Some(format!("r-{tool_use_id}")),
        tool_results: vec![tool_use_id.into()],
        send_message_msg_id: msg_id.map(str::to_string),
        ..RowFacts::default()
    }
}

fn opening_row(at: EpochMs, uuid: &str, blocks: Vec<(&str, MessageKind, Option<&str>)>) -> RowFacts {
    RowFacts {
        row_type: "user".into(),
        at: Some(at),
        uuid: Some(uuid.into()),
        turn_open: Some(UserOrigin::Teammate),
        wrapped: blocks
            .into_iter()
            .map(|(from, kind, msg_id)| WrappedFact {
                origin: UserOrigin::Teammate,
                teammate_id: Some(from.into()),
                color: None,
                msg_id: msg_id.map(str::to_string),
                kind,
                has_summary: false,
            })
            .collect(),
        ..RowFacts::default()
    }
}

fn inbox(from: &str, at: EpochMs, read: bool, frame: Option<(&str, Option<&str>)>) -> InboxEntry {
    InboxEntry {
        from: from.into(),
        timestamp: Some(at),
        read: Some(read),
        kind: None,
        msg_id: None,
        color: None,
        has_summary: false,
        frame: frame.map(|(kind, tool)| InboxFrame {
            kind: kind.into(),
            request_id: None,
            agent_id: Some(from.into()),
            tool_name: tool.map(str::to_string),
            idle_reason: None,
            completed_task_id: None,
            completed_status: None,
            timestamp: Some(at),
        }),
    }
}

// ---------------------------------------------------------------------------

#[test]
fn t_team_join_presence_and_member_keys() {
    let rig = Rig::new();
    rig.subscribe();
    let (lead, alpha) = sessions(&rig, false);
    let mut scene = Scene::new(&rig, LeadBinding::Exact);
    observe(&rig, &scene);
    rig.pump();
    let team = snap_team(&rig);
    assert_eq!((team.name.as_str(), team.raw_id.as_str(), team.lead_binding), ("auth-split", DIR, LeadBinding::Exact));
    let snap = rig.world.snapshot();
    // Lead first, then joinedAt order.
    assert_eq!(snap.members.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), vec!["team-lead", "alpha"]);
    let lead_m = &snap.members[0];
    assert_eq!((lead_m.role, lead_m.session_uid.as_ref()), (MemberRole::Lead, Some(&lead)), "joined by the live session id, never by the dir name");
    let a = &snap.members[1];
    assert_eq!((a.presence.clone(), a.session_uid.as_ref(), a.backend), (Presence::Present, Some(&alpha), MemberBackend::Pane));
    assert_eq!(a.key, member_key("alpha", scene.created + 10_000));
    // Session rows carry their team role.
    assert_eq!(rig.session(&alpha).team.unwrap().role, MemberRole::Member);
    // The pane process exits: Gone, still listed; a respawn with a new
    // joinedAt is a different member (names are reused).
    scene.live.clear();
    observe(&rig, &scene);
    scene.members.push(row("alpha", "general-purpose", "tmux", "%2", scene.created + 50_000, Some(true)));
    scene.members.remove(1);
    scene.live.push(LiveMember { agent_id: format!("alpha@{DIR}"), agent_name: "alpha".into(), process: ProcessKey::new(7102, 1_792_700_001), pid: 7102, session_id: None });
    observe(&rig, &scene);
    rig.pump();
    let snap = rig.world.snapshot();
    let alphas: Vec<&Member> = snap.members.iter().filter(|m| m.name == "alpha").collect();
    assert_eq!(alphas.len(), 2, "gone members stay visible");
    assert!(alphas.iter().any(|m| matches!(m.presence, Presence::Gone { reason: GoneReason::ProcessExited, .. })));
    assert!(alphas.iter().any(|m| m.presence == Presence::Present));
    assert_ne!(alphas[0].key, alphas[1].key);
}

#[test]
fn a_resumed_lead_is_a_new_team() {
    let rig = Rig::new();
    rig.subscribe();
    sessions(&rig, false);
    observe(&rig, &Scene::new(&rig, LeadBinding::Exact));
    // The lead resumed: a new team dir; the old team is never merged into it.
    let mut obs = Scene::new(&rig, LeadBinding::Exact).obs(&rig);
    obs.dir.dir_name = "session-77aa0000".into();
    rig.world.claude_team_adapter().team(obs);
    rig.world.claude_team_adapter().team_gone(&account(), DIR, rig.now());
    rig.pump();
    let teams = rig.world.snapshot().teams;
    assert_eq!(teams.len(), 2);
    let old = teams.iter().find(|t| t.raw_id == DIR).unwrap();
    assert!(old.ended_at.is_some());
    assert!(teams.iter().find(|t| t.raw_id == "session-77aa0000").unwrap().ended_at.is_none());
}

#[test]
fn t_team_message_join_with_and_without_msg_id() {
    let rig = Rig::new();
    rig.subscribe();
    sessions(&rig, false);
    observe(&rig, &Scene::new(&rig, LeadBinding::Exact));
    let t = rig.now();
    // With msg_id (frozen LP-16).
    rows(&rig, &lead_sid(), vec![send_row(t, "toolu_s1", "alpha"), result_row(t + 5, "toolu_s1", Some("m-1"))]);
    rig.pump();
    let key = MessageKey::derive(&team_key(), "m-1");
    let m = messages(&rig).into_iter().find(|m| m.key == key).expect("sent");
    assert_eq!((m.status, m.derived), (DeliveryStatus::Sent, false));
    let lead_key = rig.world.snapshot().members.into_iter().find(|m| m.role == MemberRole::Lead).unwrap().key;
    assert_eq!(m.from, Party::Member { member: lead_key });
    rows(&rig, &alpha_sid(), vec![opening_row(t + 1_000, "o-1", vec![("team-lead", MessageKind::Plain, Some("m-1"))])]);
    rig.pump();
    let m = messages(&rig).into_iter().find(|m| m.key == key).unwrap();
    assert_eq!(m.status, DeliveryStatus::Delivered);
    // Without msg_id: joined by order within 120 s, derived (≈).
    rows(&rig, &lead_sid(), vec![send_row(t + 2_000, "toolu_s2", "alpha"), result_row(t + 2_005, "toolu_s2", None)]);
    rows(&rig, &alpha_sid(), vec![opening_row(t + 3_000, "o-2", vec![("team-lead", MessageKind::Plain, None)])]);
    rig.pump();
    let derived: Vec<TeamMessage> = messages(&rig).into_iter().filter(|m| m.derived).collect();
    assert_eq!(derived.len(), 1, "the delivered block joined the sent one");
    assert_eq!(derived[0].status, DeliveryStatus::Delivered);
}

#[test]
fn t_team_inbox_hints_and_attribution_order() {
    let rig = Rig::new();
    rig.subscribe();
    let (lead, alpha) = sessions(&rig, true);
    let mut scene = Scene::new(&rig, LeadBinding::Hosted);
    scene.members.push(row("beta", "general-purpose", "tmux", "%2", scene.created + 20_000, Some(true)));
    scene.live.push(LiveMember { agent_id: format!("beta@{DIR}"), agent_name: "beta".into(), process: ProcessKey::new(7103, 1_792_700_000), pid: 7103, session_id: Some(sid(0xb2)) });
    rig.ev(&IdentityHint { gilde_spawn_id: Some("sp-pane-2".into()), ..claude_hint(Some(&sid(0xb2)), Some(7103)) }, alive(7103));
    observe(&rig, &scene);
    rig.pump();
    let lead_h = spawn_hint("sp-lead", Some(&lead_sid()), Some(7100));
    // (3) An inbox frame names beta (the header does not).
    let t = rig.now();
    scene.inboxes.insert("team-lead".into(), vec![inbox("beta", t, false, Some(("permission_request", Some("Bash"))))]);
    observe(&rig, &scene);
    let mut p = pty(PtyStatus::WaitingForInput, t);
    p.prompt = Some(prompt("1111111111111111", true, Some("Bash command"), t));
    rig.ev(&lead_h, SourceFacts::Pty(p));
    rig.ev(&lead_h, registry(RegStatus::Waiting, t + 400, Some("permission prompt")));
    rig.pump();
    let beta = rig.uid_of(&sid(0xb2));
    let card = rig.session(&lead).pending[0].clone();
    assert_eq!(card.on_behalf_of, Some(member_key("beta", scene.created + 20_000)));
    // Q3: the lead is represented by the member's item (answer_in = lead).
    let q = rig.world.queue();
    assert!(!q.items.iter().any(|i| i.key == format!("q_{}", lead.as_str())), "the lead has no item of its own");
    let it = q.items.iter().find(|i| i.key == format!("q_{}", beta.as_str())).expect("member item");
    match &it.subject {
        gilde_code_lib::contract::attention::AttentionSubject::Session { answer_in, on_behalf_of, .. } => {
            assert_eq!(answer_in.as_ref(), Some(&lead));
            assert!(on_behalf_of.is_some());
        }
        _ => panic!(),
    }
    assert_eq!(q.need_you, 1, "one request = one queue item");
    // The member shows a mirrored card answered at the lead.
    let mirror = rig.session(&beta).pending[0].clone();
    assert_eq!((mirror.request_id.clone(), mirror.answer_in.clone()), (card.request_id.clone(), Some(lead.clone())));
    // (1) The prompt header wins over the inbox frame.
    rig.advance(1_000);
    let t = rig.now();
    let mut p = pty(PtyStatus::WaitingForInput, t);
    p.prompt = Some(prompt("2222222222222222", true, Some("Bash command · from the alpha agent"), t));
    rig.ev(&lead_h, SourceFacts::Pty(p));
    rig.pump();
    assert_eq!(rig.session(&lead).pending[0].on_behalf_of, Some(member_key("alpha", scene.created + 10_000)));
    assert_eq!(rig.state(&alpha), ActivityState::NeedsApproval);
    // (4) Neither: the lead's own card.
    let rig2 = Rig::new();
    rig2.subscribe();
    let (lead2, _) = sessions(&rig2, true);
    observe(&rig2, &Scene::new(&rig2, LeadBinding::Hosted));
    let mut p = pty(PtyStatus::WaitingForInput, rig2.now());
    p.prompt = Some(prompt("3333333333333333", true, Some("Bash command"), rig2.now()));
    rig2.ev(&spawn_hint("sp-lead", Some(&lead_sid()), Some(7100)), SourceFacts::Pty(p));
    rig2.pump();
    // A prompt seen by the PTY alone publishes after its 300 ms debounce.
    assert!(rig2.session(&lead2).pending.is_empty());
    rig2.advance(301);
    rig2.pump();
    assert_eq!(rig2.session(&lead2).pending[0].on_behalf_of, None);
    assert!(rig2.world.queue().items.iter().any(|i| i.key == format!("q_{}", lead2.as_str())));
}

#[test]
fn t_team_inferred_items_per_turn() {
    let rig = Rig::new();
    rig.subscribe();
    sessions(&rig, false);
    let scene = Scene::new(&rig, LeadBinding::Exact);
    observe(&rig, &scene);
    rig.pump();
    assert_eq!(snap_team(&rig).caps.observe_tasks, Support::Inferred, "no task tools: inferred board (2.1.307 default)");
    let t = rig.now();
    // Two lead plain messages → two items in alpha's tray.
    rows(&rig, &lead_sid(), vec![send_row(t, "toolu_a", "alpha"), result_row(t + 1, "toolu_a", Some("m-a")), send_row(t + 2, "toolu_b", "alpha"), result_row(t + 3, "toolu_b", Some("m-b"))]);
    rig.pump();
    let items = tasks(&rig);
    assert_eq!(items.len(), 2);
    assert!(items.iter().all(|i| i.inferred && i.title.is_none() && i.blocked_by.is_empty() && i.source == TaskSource::Inferred));
    assert!(items.iter().all(|i| i.station == Station::Assigned));
    let mut ks: Vec<u32> = items.iter().filter_map(|i| i.inferred_k).collect();
    ks.sort();
    assert_eq!(ks, vec![1, 2]);
    // alpha's turn-opening row delivers both → In progress.
    rows(&rig, &alpha_sid(), vec![opening_row(t + 1_000, "o-1", vec![("team-lead", MessageKind::Plain, Some("m-a")), ("team-lead", MessageKind::Plain, Some("m-b"))])]);
    rig.pump();
    assert!(tasks(&rig).iter().all(|i| i.status == TaskStatus::InProgress));
    // The idle notification closes every item of the turn → Reported.
    rows(&rig, &lead_sid(), vec![opening_row(t + 5_000, "o-lead", vec![("alpha", MessageKind::IdleNotification, None)])]);
    rig.pump();
    let items = tasks(&rig);
    assert!(items.iter().all(|i| i.status == TaskStatus::Done && i.station == Station::Reported));
    let p = snap_team(&rig).progress;
    assert_eq!((p.done, p.total, p.inferred), (2, 2, true));
    // A turn with no team-lead block opens nothing.
    rows(&rig, &alpha_sid(), vec![opening_row(t + 9_000, "o-2", vec![("beta", MessageKind::Plain, None)])]);
    rig.pump();
    assert_eq!(tasks(&rig).len(), 2);
    // The idle notification is a system event of the log.
    assert!(messages(&rig).iter().any(|m| m.kind == MessageKind::IdleNotification));
}

#[test]
fn t_team_esc_turn_closes_items_and_the_member_is_ready() {
    let rig = Rig::new();
    rig.subscribe();
    let (_lead, alpha) = sessions(&rig, false);
    let scene = Scene::new(&rig, LeadBinding::Exact);
    observe(&rig, &scene);
    let t = rig.now();
    // S4 isActive:true (from discovery) and a turn in progress.
    rig.ev(&alpha_hint(false), SourceFacts::Team(team_facts(Some(true))));
    rig.ev(&alpha_hint(false), SourceFacts::Transcript(transcript(t + 1_000, Some(t + 1_000), None)));
    rows(&rig, &alpha_sid(), vec![opening_row(t + 1_000, "o-1", vec![("team-lead", MessageKind::Plain, None)])]);
    rig.pump();
    assert_eq!(rig.state(&alpha), ActivityState::Working);
    assert_eq!(tasks(&rig)[0].status, TaskStatus::InProgress);
    // Esc: the interrupt marker row; no idle_notification, isActive stays true.
    rig.advance(2_000);
    let t2 = rig.now();
    rows(&rig, &alpha_sid(), vec![RowFacts { row_type: "user".into(), at: Some(t2), uuid: Some("int".into()), turn_end: Some(TurnEndKind::Interrupted), ..RowFacts::default() }]);
    rig.ev(&alpha_hint(false), SourceFacts::Transcript(transcript(t2, Some(t + 1_000), Some(t2))));
    rig.ev(&alpha_hint(false), SourceFacts::Team(team_facts(Some(true))));
    rig.pump();
    assert_eq!(rig.state(&alpha), ActivityState::Ready, "never Working on isActive alone (PM-49)");
    assert_eq!(tasks(&rig)[0].status, TaskStatus::Done);
}

#[test]
fn t_team_no_task_tools_then_task_rows_switch_the_board() {
    let rig = Rig::new();
    rig.subscribe();
    sessions(&rig, false);
    observe(&rig, &Scene::new(&rig, LeadBinding::Exact));
    let t = rig.now();
    rows(&rig, &lead_sid(), vec![send_row(t, "toolu_a", "alpha"), result_row(t + 1, "toolu_a", Some("m-a"))]);
    rig.pump();
    assert_eq!(tasks(&rig).len(), 1);
    // A TaskCreate name in the tool-set attachments → the transcript board.
    rows(&rig, &lead_sid(), vec![RowFacts { row_type: "attachment".into(), at: Some(t + 10), tool_names: vec!["TaskCreate".into(), "TaskUpdate".into()], ..RowFacts::default() }]);
    rig.pump();
    assert_eq!(snap_team(&rig).caps.observe_tasks, Support::Yes);
    assert!(tasks(&rig).is_empty(), "inferred items are not the board any more");
    // TaskUpdate rows build cards (ids, status, owner; never the subject).
    rows(&rig, &alpha_sid(), vec![RowFacts {
        row_type: "assistant".into(),
        at: Some(t + 20),
        task_ops: vec![TaskOpFact { tool: "TaskUpdate".into(), tool_use_id: "toolu_t".into(), task_id: Some("1".into()), status: Some("in_progress".into()), owner: Some("alpha".into()), blocked_by: vec![] }],
        ..RowFacts::default()
    }]);
    rig.pump();
    let ts = tasks(&rig);
    assert_eq!(ts.len(), 1);
    assert_eq!((ts[0].native_id.as_deref(), ts[0].status, ts[0].source), (Some("1"), TaskStatus::InProgress, TaskSource::Transcript));
    assert_eq!(ts[0].started_at, Some(t + 20));
}

#[test]
fn t_team_board_reset_keeps_the_finished_board() {
    let rig = Rig::new();
    rig.subscribe();
    sessions(&rig, false);
    let mut scene = Scene::new(&rig, LeadBinding::Exact);
    let file = |id: &str, status: &str, blocked: Vec<&str>| TaskFile {
        id: id.into(),
        subject: Some(format!("CANARY-7f3a task {id}")),
        owner: Some("alpha".into()),
        status: status.into(),
        blocks: vec![],
        blocked_by: blocked.into_iter().map(str::to_string).collect(),
    };
    scene.tasks = TasksRead { exists: true, tasks: vec![file("1", "in_progress", vec![]), file("2", "pending", vec!["1"])], skipped: 0, truncated: false };
    observe(&rig, &scene);
    rig.pump();
    assert_eq!(snap_team(&rig).caps.observe_tasks, Support::Yes);
    let ts = tasks(&rig);
    let t2 = ts.iter().find(|t| t.native_id.as_deref() == Some("2")).unwrap();
    assert_eq!(t2.station, Station::Blocked, "declared blockedBy, not done");
    // All complete, then the files are deleted 5 s later.
    scene.tasks.tasks = vec![file("1", "completed", vec![]), file("2", "completed", vec!["1"])];
    observe(&rig, &scene);
    scene.tasks = TasksRead::default();
    observe(&rig, &scene);
    rig.pump();
    let ts = tasks(&rig);
    assert_eq!(ts.len(), 2, "the board stays finished, never empty");
    assert!(ts.iter().all(|t| t.status == TaskStatus::Done));
    let p = snap_team(&rig).progress;
    assert_eq!((p.done, p.total, p.inferred), (2, 2, false));
}

#[test]
fn t_team_messages_unknown_never_no_messages() {
    let rig = Rig::new();
    rig.subscribe();
    sessions(&rig, false);
    observe(&rig, &Scene::new(&rig, LeadBinding::Exact));
    rig.pump();
    assert!(messages(&rig).is_empty());
    let caps = snap_team(&rig).caps;
    assert_eq!(caps.observe_messages, Support::Partial, "never Yes: an empty log is not 'no messages'");
    assert_eq!(caps.fidelity_note.as_deref(), Some(MESSAGES_UNKNOWN_NOTE));
}

#[test]
fn t_team_closed_pane_messages_stay_unread() {
    let rig = Rig::new();
    rig.subscribe();
    sessions(&rig, true);
    let mut scene = Scene::new(&rig, LeadBinding::Hosted);
    observe(&rig, &scene);
    // The pane closes; the lead keeps messaging it.
    scene.live.clear();
    let t = rig.now();
    scene.inboxes.insert("alpha".into(), vec![inbox("team-lead", t, false, None)]);
    observe(&rig, &scene);
    rig.advance(60_000);
    observe(&rig, &scene);
    rig.pump();
    let m = messages(&rig).into_iter().find(|m| m.kind == MessageKind::Plain).expect("message");
    assert_eq!(m.status, DeliveryStatus::Queued, "accepted and never read (LP-13)");
    let alpha = rig.world.snapshot().members.into_iter().find(|m| m.name == "alpha").unwrap();
    assert!(matches!(alpha.presence, Presence::Gone { .. }));
}

#[test]
fn t_team_in_process_fallback() {
    let rig = Rig::new();
    rig.subscribe();
    sessions(&rig, true);
    let mut scene = Scene::new(&rig, LeadBinding::Hosted);
    scene.members[1] = row("alpha", "general-purpose", "in-process", "in-process", scene.created + 10_000, None);
    scene.live.clear();
    observe(&rig, &scene);
    rig.pump();
    let team = snap_team(&rig);
    assert_eq!(team.pane_hosting, PaneHostingState::InProcessFallback);
    let alpha = rig.world.snapshot().members.into_iter().find(|m| m.name == "alpha").unwrap();
    assert_eq!((alpha.backend, alpha.presence.clone(), alpha.session_uid), (MemberBackend::InProcess, Presence::Present, None));
    // In-flight is counted for in-process recipients only.
    assert_eq!(team.progress.in_flight, Some(0));
    // The lead exits: in-process members are gone with it, the team ends.
    scene.lead_alive = false;
    observe(&rig, &scene);
    rig.pump();
    assert!(snap_team(&rig).ended_at.is_some());
}

#[test]
fn t_team_fork_is_never_a_member() {
    let rig = Rig::new();
    rig.subscribe();
    sessions(&rig, false);
    observe(&rig, &Scene::new(&rig, LeadBinding::Exact));
    // A copy of alpha's conversation runs elsewhere: membership comes from
    // the roster and argv only, never from transcript lineage (PM-38).
    let fork = claude_hint(Some(&sid(0xf0)), Some(7199));
    rig.ev(&fork, alive(7199));
    rig.ev(&fork, SourceFacts::Meta(Box::new(SessionMetaFacts { title: Some("alpha (copy)".into()), ..SessionMetaFacts::default() })));
    rig.pump();
    let uid = rig.uid_of(&sid(0xf0));
    assert!(rig.session(&uid).team.is_none());
    assert_eq!(rig.world.snapshot().members.len(), 2);
}

#[test]
fn t_team_send_routes_and_caps_filtering() {
    let rig = Rig::new();
    rig.subscribe();
    let (lead, _alpha) = sessions(&rig, true);
    let scene = Scene::new(&rig, LeadBinding::Hosted);
    observe(&rig, &scene);
    rig.pump();
    let a = rig.world.claude_team_adapter();
    let alpha_k = member_key("alpha", scene.created + 10_000);
    assert_eq!(a.send(&team_key(), &alpha_k, "hi"), ActOutcome::Unsupported { reason: "viaMemberPane".into() });
    assert_eq!(a.broadcast(&team_key(), "hi"), ActOutcome::PrefillLead { lead: lead.clone(), action: PrefillAction::TellAll });
    assert_eq!(
        a.spawn(&team_key(), &MemberSpec { name: "gamma".into(), vendor: None, model: None }),
        ActOutcome::PrefillLead { lead: lead.clone(), action: PrefillAction::Spawn { name: "gamma".into() } }
    );
    assert_eq!(a.stop(&team_key(), &alpha_k, StopHow::Graceful), ActOutcome::PrefillLead { lead: lead.clone(), action: PrefillAction::AskStop { member: alpha_k.clone() } });
    assert!(matches!(a.stop(&team_key(), &alpha_k, StopHow::HardClosePane), ActOutcome::Unsupported { .. }));
    assert!(matches!(a.answer_approval(&team_key(), &RequestId::parse("x").unwrap(), &Decision::Deny), ActOutcome::Unsupported { .. }));
    assert_eq!(a.task_write(&team_key(), TaskOp::Delete { task: TaskKey::derive(&team_key(), "1") }), ActOutcome::Unsupported { reason: "noTaskTools".into() });
    // The lead is In Gilde (hosted): via-lead and member-pane routes offered.
    let caps = snap_team(&rig).caps;
    assert_eq!(caps.act_send, vec![SendRoute::ViaMemberPane, SendRoute::ViaLead]);
    // An external team whose lead is Watching: nothing via the lead.
    let rig2 = Rig::new();
    rig2.subscribe();
    sessions(&rig2, false);
    observe(&rig2, &Scene::new(&rig2, LeadBinding::Exact));
    rig2.pump();
    assert!(snap_team(&rig2).caps.act_send.is_empty());
    let a2 = rig2.world.claude_team_adapter();
    assert!(matches!(a2.send(&team_key(), &alpha_k, "x"), ActOutcome::PrefillLead { action: PrefillAction::Tell { .. }, .. }));
}

#[test]
fn t_derive_hosted_lead_busy_with_live_panes_through_world() {
    let rig = Rig::new();
    rig.subscribe();
    let (lead, alpha) = sessions(&rig, true);
    observe(&rig, &Scene::new(&rig, LeadBinding::Hosted));
    rig.pump();
    // The lead's registry stays busy while pane teammates live; its PTY is
    // Idle (an S5-only change publishes after its 300 ms).
    rig.advance(400);
    rig.pump();
    assert_eq!(rig.state(&lead), ActivityState::Ready);
    // The member works (its own transcript rows): the lead is exempt from No
    // progress even when its own screen is quiet for long.
    let t = rig.now();
    rig.ev(&alpha_hint(true), SourceFacts::Transcript(transcript(t, Some(t - 1_000), None)));
    rig.ev(&spawn_hint("sp-lead", Some(&lead_sid()), Some(7100)), SourceFacts::Pty(pty(PtyStatus::Working, t)));
    rig.pump();
    assert_eq!(rig.state(&alpha), ActivityState::Working);
    for _ in 0..12 {
        rig.advance(29_000);
        rig.ev(&alpha_hint(true), SourceFacts::Transcript(transcript(rig.now(), Some(t - 1_000), None)));
        rig.pump();
    }
    assert_eq!(rig.state(&lead), ActivityState::Working, "lead_has_working_members from member activity");
    // Team glyph = the most urgent member state.
    assert_eq!(snap_team(&rig).glyph_state, ActivityState::Working);
    let hosted_role = rig.session(&lead).control;
    assert!(matches!(hosted_role, gilde_code_lib::contract::session::Control::InGilde { hosted_team: true, .. }));
}

#[test]
fn t_team_nowrite_static_check() {
    // §7.1.4 rule 1 / §9.10: World and its adapters never write, connect or
    // spawn. The source must not contain any of these calls.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/world");
    let forbidden = [
        "fs::write",
        "OpenOptions",
        ".write(true)",
        ".create(true)",
        ".append(true)",
        "File::create",
        "fs::rename",
        "fs::remove_",
        "remove_file",
        "remove_dir",
        "fs::copy",
        "UnixStream::connect",
        "TcpStream",
        "process::Command",
        "Command::new",
        "fs::File::open",
        "File::open",
    ];
    let mut files = Vec::new();
    let mut stack = vec![root];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "rs") {
                files.push(p);
            }
        }
    }
    assert!(files.len() >= 13, "every world module is scanned");
    for f in files {
        let src = std::fs::read_to_string(&f).unwrap();
        for bad in forbidden {
            assert!(!src.contains(bad), "{} contains `{bad}`", f.display());
        }
    }
}

// ---------------------------------------------------------------------------
// §7.1.4 with a scripted adapter (the fake of §7.3.4 is cfg(test)/showcase;
// this one has the same all-Native descriptor)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Scripted {
    sink: Mutex<Option<Arc<dyn TeamEventSink>>>,
    acts: Mutex<u32>,
}

impl Scripted {
    fn emit(&self, e: TeamEvent) {
        let s = self.sink.lock().clone().unwrap();
        s.emit(e);
    }
}

impl TeamAdapter for Scripted {
    fn source(&self) -> TeamSource {
        TeamSource::Emulated
    }
    fn capabilities(&self, _team: &TeamKey) -> TeamCapabilities {
        TeamCapabilities::all_native()
    }
    fn subscribe(&self, sink: Arc<dyn TeamEventSink>) {
        *self.sink.lock() = Some(sink);
    }
    fn send(&self, _t: &TeamKey, _to: &MemberKey, _text: &str) -> ActOutcome {
        *self.acts.lock() += 1;
        ActOutcome::Done
    }
    fn broadcast(&self, _t: &TeamKey, _text: &str) -> ActOutcome {
        ActOutcome::Done
    }
    fn spawn(&self, _t: &TeamKey, _s: &MemberSpec) -> ActOutcome {
        ActOutcome::Done
    }
    fn stop(&self, _t: &TeamKey, _m: &MemberKey, _h: StopHow) -> ActOutcome {
        ActOutcome::Done
    }
    fn answer_approval(&self, _t: &TeamKey, _r: &RequestId, _d: &Decision) -> ActOutcome {
        ActOutcome::Done
    }
    fn task_write(&self, _t: &TeamKey, _op: TaskOp) -> ActOutcome {
        ActOutcome::Done
    }
}

fn fake_member(team: &TeamKey, name: &str, role: MemberRole, joined: EpochMs, uid: Option<SessionUid>) -> Member {
    Member {
        key: MemberKey::derive(team, &format!("{name}@t"), Some(joined)),
        team_key: team.clone(),
        session_uid: uid,
        name: name.into(),
        agent_id: Some(format!("{name}@t")),
        role,
        colour: None,
        vendor: Vendor::Claude,
        backend: MemberBackend::Emulated,
        joined_at: Some(joined),
        presence: Presence::Present,
        pane: None,
        plan_mode_required: false,
        current_task: None,
    }
}

fn fake_task(team: &TeamKey, id: &str, owner: &MemberKey, status: TaskStatus) -> Task {
    Task {
        key: TaskKey::derive(team, id),
        team_key: team.clone(),
        native_id: Some(id.into()),
        title: Some(format!("task {id}")),
        inferred_k: None,
        status,
        station: Station::ToDo,
        owner: Some(owner.clone()),
        blocked_by: vec![],
        blocks: vec![],
        created_at: Some(1),
        started_at: None,
        completed_at: None,
        first_seen_in_progress_at: None,
        source: TaskSource::File,
        inferred: false,
    }
}

#[test]
fn adapter_rules_7_1_4_with_a_scripted_team() {
    let rig = Rig::new();
    rig.subscribe();
    let ad = Arc::new(Scripted::default());
    rig.world.register_team_adapter(ad.clone());
    let team = TeamKey::derive("emulated", None, "t1");
    let lead = fake_member(&team, "lead", MemberRole::Lead, 1, None);
    ad.emit(TeamEvent::TeamSeen { key: team.clone(), name: "fake".into(), raw_id: "t1".into(), project: None, lead: Some(lead.key.clone()), binding: LeadBinding::None });
    ad.emit(TeamEvent::MemberUpserted(lead.clone()));
    rig.pump();
    // A Team row is shown only with ≥ 1 member besides the lead.
    assert!(rig.world.snapshot().teams.is_empty());
    let w = rig.world.bind_spawn("sp-fake-w", None, Vendor::Claude, Some(account()));
    let t = rig.now();
    rig.ev(&spawn_hint("sp-fake-w", None, Some(7300)), SourceFacts::Gilde(gilde(t, ControlMode::Conversation, Some(t))));
    rig.ev(&spawn_hint("sp-fake-w", None, Some(7300)), alive(7300));
    let worker = fake_member(&team, "worker", MemberRole::Member, 2, Some(w.clone()));
    ad.emit(TeamEvent::MemberUpserted(worker.clone()));
    rig.pump();
    let snap = rig.world.snapshot();
    assert_eq!(snap.teams.len(), 1);
    assert_eq!(snap.teams[0].caps, TeamCapabilities::all_native());
    // Rule 3: missing data stays None/Unknown.
    assert_eq!(snap.teams[0].progress.in_flight, None);
    // Rule 2: an act is observed later, never acknowledged by inventing state.
    assert_eq!(ad.send(&team, &worker.key, "go"), ActOutcome::Done);
    rig.pump();
    assert!(rig.world.snapshot().messages.is_empty());
    let msg = TeamMessage {
        key: MessageKey::derive(&team, "x1"),
        team_key: team.clone(),
        from: Party::User,
        to: Party::Member { member: worker.key.clone() },
        at: t,
        kind: MessageKind::Plain,
        status: DeliveryStatus::Delivered,
        in_reply_to: None,
        derived: false,
        idle_reason: None,
        has_summary: false,
    };
    ad.emit(TeamEvent::MessageObserved(msg.clone()));
    rig.pump();
    assert_eq!(rig.world.snapshot().messages, vec![msg]);
    // Rule 4: a reused name with a new joinedAt is another member.
    let again = fake_member(&team, "worker", MemberRole::Member, 3, None);
    assert_ne!(again.key, worker.key);
    // Tasks, stations and bottlenecks: two Assigned cards in the worker's tray.
    ad.emit(TeamEvent::TaskUpserted(fake_task(&team, "1", &worker.key, TaskStatus::Open)));
    ad.emit(TeamEvent::TaskUpserted(fake_task(&team, "2", &worker.key, TaskStatus::Open)));
    rig.pump();
    let team_row = rig.world.snapshot().teams[0].clone();
    assert_eq!(team_row.progress.bottlenecks, vec![worker.key.clone()]);
    assert!(rig.world.snapshot().tasks.iter().all(|t| t.station == Station::Assigned));
    // NeedsYou: the owner's session is in band 1.
    let mut started = fake_task(&team, "1", &worker.key, TaskStatus::InProgress);
    started.started_at = Some(t);
    ad.emit(TeamEvent::TaskUpserted(started));
    let pend = gilde_code_lib::contract::interaction::PendingInteraction {
        request_id: RequestId::parse("toolu_ny").unwrap(),
        kind: InteractionKind::ToolApproval,
        received_at: t,
        on_behalf_of: None,
        answer_in: None,
        title: "Bash".into(),
        tool: None,
        detail_lines: vec![],
        options: vec![],
        always_label: None,
        default_to_no: false,
        degraded: false,
        prompt_rev: None,
        confidence: gilde_code_lib::contract::common::Confidence::High,
        multi_select: false,
        questions: vec![],
    };
    rig.ev(
        &spawn_hint("sp-fake-w", None, Some(7300)),
        SourceFacts::Stream(gilde_code_lib::contract::activity::StreamFacts {
            phase: gilde_code_lib::contract::activity::TurnPhase::RequiresAction,
            phase_since: t,
            pending: vec![pend],
            last_turn_start_at: Some(t),
            last_turn_end: None,
            last_frame_at: t,
        }),
    );
    rig.pump();
    let t1 = rig.world.snapshot().tasks.into_iter().find(|x| x.native_id.as_deref() == Some("1")).unwrap();
    assert_eq!(t1.station, Station::NeedsYou);
    assert_eq!(rig.world.snapshot().teams[0].glyph_state, ActivityState::NeedsApproval);
    // A deleted task leaves every blocked_by list.
    let mut blocked = fake_task(&team, "3", &worker.key, TaskStatus::Open);
    blocked.blocked_by = vec![TaskKey::derive(&team, "2")];
    ad.emit(TeamEvent::TaskUpserted(blocked));
    ad.emit(TeamEvent::TaskUpserted(fake_task(&team, "2", &worker.key, TaskStatus::Deleted)));
    rig.pump();
    let t3 = rig.world.snapshot().tasks.into_iter().find(|x| x.native_id.as_deref() == Some("3")).unwrap();
    assert!(t3.blocked_by.is_empty());
    // Rule 5: team end from the adapter's facts.
    ad.emit(TeamEvent::TeamEnded { key: team.clone(), at: t + 5 });
    rig.pump();
    assert_eq!(rig.world.snapshot().teams[0].ended_at, Some(t + 5));
    // No team is ever a queue item of its own (Q2).
    assert!(rig.world.queue().items.iter().all(|i| i.key.starts_with("q_s_") || i.key == "q_leftovers"));
}

// ---------------------------------------------------------------------------
// the orchestrator mapping (§7.3.1)
// ---------------------------------------------------------------------------

#[test]
fn orchestrator_snapshot_mapping_latest_wins_and_read_only() {
    let rig = Rig::new();
    rig.subscribe();
    // A member's running turn process (Watching · Orchestrator).
    let h = IdentityHint { vendor: Some(Vendor::Codex), ..claude_hint(Some(&sid(0xc0)), Some(7400)) };
    rig.ev(&h, alive(7400));
    rig.pump();
    let team = serde_json::json!({
        "v": 1, "id": "tmabcdef1234", "name": "refactor", "status": "active", "createdAt": rig.now() - 1000,
        "members": [
            {"name": "ana", "index": 1, "role": "dev", "state": "running", "provider": "codex", "turn": {"id": "t1", "n": 1, "startedAt": rig.now(), "pid": 7400}},
            {"name": "bo", "index": 2, "role": "dev", "state": "blocked", "provider": "gemini"}
        ],
        "board": [
            {"id": "b1", "title": "CANARY-7f3a split auth \u{202e}module", "state": "claimed", "assignee": "ana", "createdAt": rig.now() - 500, "updatedAt": rig.now()},
            {"id": "b2", "title": "docs", "state": "done", "assignee": "bo", "createdAt": rig.now() - 900, "updatedAt": rig.now() - 100}
        ]
    });
    let orch = rig.world.orchestrator_adapter();
    assert!(orch.apply_snapshot(None, 1, 5, &team, rig.now()));
    assert!(!orch.apply_snapshot(None, 1, 4, &team, rig.now()), "older (generation, rev) ignored");
    rig.pump();
    let snap = rig.world.snapshot();
    let t = snap.teams.iter().find(|t| t.source == TeamSource::Orchestrator).expect("team");
    assert_eq!(t.caps, orchestrator_caps());
    assert!(t.caps.act_send.is_empty(), "no actions in v1");
    assert_eq!(t.progress.in_flight, None);
    let ana = snap.members.iter().find(|m| m.name == "ana").unwrap();
    assert_eq!((ana.vendor, ana.backend), (Vendor::Codex, MemberBackend::Orchestrator));
    assert!(ana.session_uid.is_some(), "matched to its process by pid");
    let bo = snap.members.iter().find(|m| m.name == "bo").unwrap();
    assert_eq!(bo.vendor, Vendor::Agy);
    let b1 = snap.tasks.iter().find(|x| x.native_id.as_deref() == Some("b1")).unwrap();
    assert_eq!((b1.status, b1.source, b1.inferred), (TaskStatus::InProgress, TaskSource::Orchestrator, false));
    assert!(!b1.title.as_deref().unwrap().contains('\u{202e}'), "titles are sanitized");
    assert!(b1.blocked_by.is_empty(), "never invented");
    // The blocked member without a session row shows an error glyph.
    assert!(matches!(t.glyph_state, ActivityState::Error { .. } | ActivityState::Working));
    // A non-team payload is refused.
    assert!(!orch.apply_snapshot(None, 2, 1, &serde_json::json!({"id": "../../etc"}), rig.now()));
}

#[test]
fn provisional_flags_switch_attribution_and_counts() {
    // P19 absent (an unverified version): the card stays the lead's own,
    // "a teammate" — never a guessed member.
    let rig = Rig::new();
    rig.subscribe();
    let (lead, _alpha) = sessions_with(&rig, true, None);
    observe(&rig, &Scene::new(&rig, LeadBinding::Hosted));
    let mut p = pty(PtyStatus::WaitingForInput, rig.now());
    p.prompt = Some(prompt("4444444444444444", true, Some("Bash command · from the alpha agent"), rig.now()));
    let lead_h = spawn_hint("sp-lead", Some(&lead_sid()), Some(7100));
    rig.ev(&lead_h, SourceFacts::Pty(p));
    rig.ev(&lead_h, registry(RegStatus::Waiting, rig.now(), Some("permission prompt")));
    rig.pump();
    assert_eq!(rig.session(&lead).pending[0].on_behalf_of, None);
    assert!(rig.world.queue().items.iter().any(|i| i.key == format!("q_{}", lead.as_str())));
    // P22 question location off: an AskUserQuestion frame is not attributed.
    let rig = Rig::new();
    rig.subscribe();
    let (lead, _) = sessions_with(&rig, true, Some(compat_307(&[("teammateQuestionLocation", false)])));
    let mut scene = Scene::new(&rig, LeadBinding::Hosted);
    scene.inboxes.insert("team-lead".into(), vec![inbox("alpha", rig.now(), false, Some(("permission_request", Some("AskUserQuestion"))))]);
    observe(&rig, &scene);
    let mut p = pty(PtyStatus::WaitingForInput, rig.now());
    p.prompt = Some(prompt("5555555555555555", false, None, rig.now()));
    let lead_h = spawn_hint("sp-lead", Some(&lead_sid()), Some(7100));
    rig.ev(&lead_h, SourceFacts::Pty(p));
    rig.ev(&lead_h, registry(RegStatus::Waiting, rig.now(), Some("input needed")));
    rig.pump();
    assert_eq!(rig.state(&lead), ActivityState::NeedsAnswer);
    assert_eq!(rig.session(&lead).pending[0].on_behalf_of, None, "location unverified");
    // P17: team-store shape not verified → in-flight unknown, "≈", Details note.
    let rig = Rig::new();
    rig.subscribe();
    let (lead, alpha) = sessions_with(&rig, false, Some(compat_307(&[("teamStoreVerified", false)])));
    let mut scene = Scene::new(&rig, LeadBinding::Exact);
    scene.members.push(row("gamma", "general-purpose", "in-process", "in-process", scene.created + 30_000, None));
    observe(&rig, &scene);
    rig.pump();
    let team = snap_team(&rig);
    assert_eq!(team.caps.observe_in_flight, Support::Unknown);
    assert_eq!(team.progress.in_flight, None);
    assert!(team.progress.approx);
    assert!(rig.session(&lead).details.team_format_unverified);
    let _ = alpha;
}

#[test]
fn teammate_version_differs_from_its_lead() {
    let rig = Rig::new();
    rig.subscribe();
    let (_lead, alpha) = sessions(&rig, false);
    observe(&rig, &Scene::new(&rig, LeadBinding::Exact));
    rig.ev(&alpha_hint(false), SourceFacts::Meta(Box::new(SessionMetaFacts { claude_version: Some("2.1.308".into()), ..SessionMetaFacts::default() })));
    rig.pump();
    assert!(rig.session(&alpha).details.teammate_version_differs);
}

// ---------------------------------------------------------------------------
// PaneHost's pane table of a hosted lead (A21.1)
// ---------------------------------------------------------------------------

mod panes {
    use super::*;
    use gilde_code_lib::contract::activity::ExitInfo;
    use gilde_code_lib::contract::ids::{PaneId, PtyId};
    use gilde_code_lib::contract::session::Control;
    use gilde_code_lib::contract::team::ExternalHost;
    use gilde_code_lib::panehost::{
        LeadPaneTable, LeadPhase, PaneKind, PaneState, PaneTableSink, PaneView, UnknownReason, DETAILS_TEAMMATE_BINARY_DIFFERS,
    };

    pub fn pane(n: u32, kind: PaneKind, state: PaneState, session: Option<SessionUid>, at: EpochMs) -> PaneView {
        PaneView {
            id: PaneId::new(n),
            window: "@0".into(),
            listed: state.listed(),
            title: None,
            colour: None,
            colour_unknown: false,
            remain_on_exit: None,
            state,
            kind,
            command: None,
            created_at: at,
            session,
        }
    }

    pub fn teammate(name: &str) -> PaneKind {
        PaneKind::Teammate { agent_id: format!("{name}@{DIR}"), team_name: DIR.into() }
    }

    pub fn running(n: u32, since: EpochMs) -> PaneState {
        PaneState::Running { pty: PtyId::mint(), pid: 7100 + n as i32, spawn_id: format!("sp-pane-{n}"), since }
    }

    pub fn table(lead: &SessionUid, phase: LeadPhase, panes: Vec<PaneView>, at: EpochMs) -> LeadPaneTable {
        LeadPaneTable { lead: lead.clone(), lead_pid: Some(7100), phase, outside_gilde: false, session: true, panes, at }
    }

    fn member<'a>(snap: &'a gilde_code_lib::contract::store::StoreSnapshot, name: &str) -> Option<&'a Member> {
        snap.members.iter().find(|m| m.name == name)
    }

    /// A hosted team whose roster lists alpha (%1) and beta (%2).
    fn hosted(rig: &Rig) -> (SessionUid, SessionUid, SessionUid, Scene) {
        let (lead, alpha) = sessions(rig, true);
        // PaneSessions (T09b): each teammate pane's spawn is bound right
        // after it starts; alpha's evidence came first (adopted).
        assert_eq!(rig.world.bind_spawn("sp-pane-1", None, Vendor::Claude, Some(account())), alpha);
        let beta = rig.world.bind_spawn("sp-pane-2", None, Vendor::Claude, Some(account()));
        let mut scene = Scene::new(rig, LeadBinding::Hosted);
        scene.members.push(row("beta", "general-purpose", "tmux", "%2", scene.created + 20_000, Some(true)));
        observe(rig, &scene);
        (lead, alpha, beta, scene)
    }

    #[test]
    fn a_hosted_lead_with_three_panes_one_unknown_one_crashed() {
        let rig = Rig::new();
        rig.subscribe();
        let (lead, alpha, beta, _scene) = hosted(&rig);
        let t = rig.now();
        let tbl = table(
            &lead,
            LeadPhase::Running,
            vec![
                pane(1, teammate("alpha"), running(1, t - 5_000), Some(alpha.clone()), t - 6_000),
                pane(2, teammate("beta"), PaneState::Exited { code: Some(1), signal: None, at: t, kept: true }, Some(beta.clone()), t - 6_000),
                pane(3, PaneKind::Unknown { reason: UnknownReason::BinaryDiffers }, running(3, t - 1_000), None, t - 2_000),
            ],
            t,
        );
        rig.world.claude_team_adapter().table(tbl);
        rig.pump();
        let snap = rig.world.snapshot();
        // Tiles: the running teammate is Present and linked to its pane.
        let a = member(&snap, "alpha").unwrap();
        assert_eq!((a.presence.clone(), a.pane.clone(), a.session_uid.clone()), (Presence::Present, Some(PaneId::new(1)), Some(alpha.clone())));
        assert!(matches!(rig.session(&alpha).control, Control::InGilde { mode: ControlMode::Terminal, .. }));
        // The crashed pane: the member is Gone{ProcessExited}; its row is Crashed.
        let b = member(&snap, "beta").unwrap();
        assert!(matches!(b.presence, Presence::Gone { reason: GoneReason::ProcessExited, .. }));
        assert_eq!(rig.state(&beta), ActivityState::Crashed { exit: ExitInfo { code: Some(1), signal: None } });
        // The unknown pane is never a member, never a session, never queued.
        assert_eq!(snap.members.len(), 3);
        assert!(rig.world.queue().items.iter().all(|i| i.key != format!("q_{}", lead.as_str())));
        // Its Details note (A21.3) is the lead's session line.
        let ev = rig.session(&lead).activity.evidence.expect("binary-differs line");
        assert_eq!(ev.key, DETAILS_TEAMMATE_BINARY_DIFFERS);
        assert_eq!(ev.params.get("pane").and_then(|v| v.as_str()), Some("%3"));
        assert_eq!(snap_team(&rig).pane_hosting, PaneHostingState::Hosted);
        // The unknown pane goes: the line goes with it.
        let tbl = table(&lead, LeadPhase::Running, vec![pane(1, teammate("alpha"), running(1, t - 5_000), Some(alpha.clone()), t - 6_000)], t + 10);
        rig.world.claude_team_adapter().table(tbl);
        rig.pump();
        assert_eq!(rig.session(&lead).activity.evidence, None);
    }

    #[test]
    fn lead_has_working_members_from_member_activity() {
        let rig = Rig::new();
        rig.subscribe();
        let (lead, alpha, _beta, _scene) = hosted(&rig);
        let t = rig.now();
        rig.world.claude_team_adapter().table(table(&lead, LeadPhase::Running, vec![pane(1, teammate("alpha"), running(1, t), Some(alpha.clone()), t)], t));
        // The lead's own screen works but goes quiet; alpha keeps writing rows.
        rig.ev(&spawn_hint("sp-lead", Some(&lead_sid()), Some(7100)), SourceFacts::Pty(pty(PtyStatus::Working, t)));
        for _ in 0..12 {
            rig.advance(29_000);
            rig.ev(&alpha_hint(true), SourceFacts::Transcript(transcript(rig.now(), Some(t), None)));
            rig.pump();
        }
        assert_eq!(rig.state(&alpha), ActivityState::Working);
        assert_eq!(rig.state(&lead), ActivityState::Working, "exempt from No progress while a member works");
        // alpha's turn ends: the lead is no longer exempt.
        rig.ev(&alpha_hint(true), SourceFacts::Transcript(transcript(rig.now(), Some(t), Some(rig.now()))));
        rig.pump();
        // (A change decided by the lead's PTY alone publishes after 300 ms.)
        rig.advance(301);
        rig.pump();
        assert!(matches!(rig.state(&lead), ActivityState::NoProgress { .. }));
    }

    /// A placeholder or spawning pane writes no member, session or queue
    /// row: only its team's `paneTiles` change (A30.1, one Team upsert), and
    /// a table that changes nothing visible writes nothing.
    #[test]
    fn a_placeholder_writes_only_its_team_tile() {
        let rig = Rig::new();
        rig.subscribe();
        let (lead, _alpha, _beta, _scene) = hosted(&rig);
        rig.pump();
        rig.advance(1_000);
        rig.pump();
        let t = rig.now();
        let only_team_upserts = |patches: &[StorePatch]| {
            patches.iter().flat_map(|p| p.ops.iter()).all(|op| matches!(op, StoreOp::Upsert { entity } if matches!(entity.as_ref(), Entity::Team { .. })))
        };
        let ph = |n| pane(n, PaneKind::Placeholder, PaneState::Placeholder { since: t, spawn_failed: false }, None, t);
        rig.world.claude_team_adapter().table(table(&lead, LeadPhase::Running, vec![ph(4), ph(5)], t));
        let p = rig.pump();
        assert!(only_team_upserts(&p) && team_upserts(&p) == 1, "a placeholder is a tile on the team row, nothing else: {p:?}");
        assert_eq!(tiles(&rig).len(), 2);
        // The same table again: nothing.
        rig.world.claude_team_adapter().table(table(&lead, LeadPhase::Running, vec![ph(4), ph(5)], t + 1));
        assert!(rig.pump().is_empty());
        // A spawning pane: again only the team row's tiles.
        rig.world.claude_team_adapter().table(table(&lead, LeadPhase::Running, vec![pane(4, teammate("gamma"), PaneState::Spawning { since: t }, None, t)], t + 2));
        let p = rig.pump();
        assert!(only_team_upserts(&p) && team_upserts(&p) == 1, "{p:?}");
        assert_eq!(tiles(&rig).iter().map(|x| x.state.clone()).collect::<Vec<_>>(), vec![PaneTileState::Starting { since: t, spawn_failed: false }]);
        // A table for a lead World has no team for: nothing at all.
        let other = rig.uid_of(&sid(0x55));
        rig.world.claude_team_adapter().table(table(&other, LeadPhase::Pending, vec![], t));
        assert!(rig.pump().is_empty());
    }

    #[test]
    fn the_pane_link_needs_the_roster_and_the_pane_to_agree() {
        let rig = Rig::new();
        rig.subscribe();
        let (lead, alpha, _beta, _scene) = hosted(&rig);
        let t = rig.now();
        // alpha's teammate command runs in %7, but the roster says %1.
        rig.world.claude_team_adapter().table(table(&lead, LeadPhase::Running, vec![pane(7, teammate("alpha"), running(7, t), Some(alpha.clone()), t)], t));
        rig.pump();
        let snap = rig.world.snapshot();
        let a = member(&snap, "alpha").unwrap();
        assert_eq!(a.pane, None, "shows \"Teammate\" until both agree");
        assert_eq!(a.presence, Presence::Present);
    }

    #[test]
    fn kill_pane_is_stopped_and_lead_phases_reach_the_team() {
        let rig = Rig::new();
        rig.subscribe();
        let (lead, alpha, _beta, _scene) = hosted(&rig);
        let t = rig.now();
        let a = |state| table(&lead, LeadPhase::Running, vec![pane(1, teammate("alpha"), state, Some(alpha.clone()), t)], t);
        rig.world.claude_team_adapter().table(a(running(1, t)));
        rig.pump();
        rig.world.claude_team_adapter().table(a(PaneState::Killed { at: t + 50 }));
        rig.pump();
        assert_eq!(rig.state(&alpha), ActivityState::Stopped);
        assert!(matches!(member(&rig.world.snapshot(), "alpha").unwrap().presence, Presence::Gone { .. }));
        // A backgrounded lead keeps its panes: external host Background.
        let mut tb = table(&lead, LeadPhase::Backgrounded, vec![], t + 60);
        rig.world.claude_team_adapter().table(tb.clone());
        rig.pump();
        assert_eq!(snap_team(&rig).external_host, Some(ExternalHost::Background));
        // A real tmux server for this lead: OutsideGilde.
        tb.phase = LeadPhase::Running;
        tb.outside_gilde = true;
        rig.world.claude_team_adapter().table(tb.clone());
        rig.pump();
        assert_eq!(snap_team(&rig).pane_hosting, PaneHostingState::OutsideGilde);
        // The lead ends: the team ends.
        tb.phase = LeadPhase::Ended;
        tb.at = t + 70;
        rig.world.claude_team_adapter().table(tb);
        rig.pump();
        assert_eq!(snap_team(&rig).ended_at, Some(t + 70));
    }

    // -- A30.1: non-member panes as `Team.paneTiles` -----------------------------

    use gilde_code_lib::contract::store::{Entity, StoreOp, StorePatch};
    use gilde_code_lib::contract::team::{PaneTile, PaneTileState};

    fn unknown() -> PaneKind {
        PaneKind::Unknown { reason: UnknownReason::NotTeammateCommand }
    }

    fn tiles(rig: &Rig) -> Vec<PaneTile> {
        snap_team(rig).pane_tiles
    }

    /// Team upserts in these patches.
    fn team_upserts(patches: &[StorePatch]) -> usize {
        patches
            .iter()
            .flat_map(|p| p.ops.iter())
            .filter(|op| matches!(op, StoreOp::Upsert { entity } if matches!(entity.as_ref(), Entity::Team { .. })))
            .count()
    }

    /// T-team-paneTiles: every tile state from a synthetic pane table — a
    /// placeholder and a spawning teammate pane (before binding) are
    /// Starting, `spawnFailed` carried; a running unknown pane is Unknown
    /// with its PTY; an exited, kept unknown pane is Unknown with its exit
    /// code and no PTY; bound, killed and unlisted panes are no tiles; the
    /// title is sanitized display text. Creation order.
    #[test]
    fn t_team_pane_tiles_states() {
        let rig = Rig::new();
        rig.subscribe();
        let (lead, alpha, beta, _scene) = hosted(&rig);
        let t = rig.now();
        let pty5 = PtyId::mint();
        let mut titled = pane(
            5,
            unknown(),
            PaneState::Running { pty: pty5.clone(), pid: 7105, spawn_id: "sp-pane-5".into(), since: t - 1_000 },
            None,
            t - 5_000,
        );
        titled.title = Some("\u{1b}[31mbuild\u{1b}[0m \u{202e}watch".into());
        let mut blank = pane(4, PaneKind::Placeholder, PaneState::Placeholder { since: t - 4_000, spawn_failed: true }, None, t - 4_000);
        blank.title = Some("  ".into());
        let panes = vec![
            // %1 alpha's pane, bound to its session: its member row shows it.
            pane(1, teammate("alpha"), running(1, t - 9_000), Some(alpha.clone()), t - 9_000),
            // %2 beta's pane is still spawning, not bound yet: Starting.
            pane(2, teammate("beta"), PaneState::Spawning { since: t - 7_000 }, None, t - 8_000),
            // %3 a placeholder (no respawn-pane yet): Starting.
            pane(3, PaneKind::Placeholder, PaneState::Placeholder { since: t - 6_000, spawn_failed: false }, None, t - 6_000),
            // %4 a placeholder whose spawn failed.
            blank,
            // %5 an unknown pane running: Unknown with its PTY.
            titled,
            // %6 an unknown pane exited and kept: Unknown with its code, no PTY.
            pane(6, unknown(), PaneState::Exited { code: Some(3), signal: None, at: t - 500, kept: true }, None, t - 3_000),
            // %7 an unknown pane exited and not kept: unlisted, dropped.
            pane(7, unknown(), PaneState::Exited { code: Some(0), signal: None, at: t - 400, kept: false }, None, t - 2_500),
            // %8 a killed placeholder: dropped.
            pane(8, PaneKind::Placeholder, PaneState::Killed { at: t - 300 }, None, t - 2_000),
            // %9 a running teammate pane World could not bind: its member's, never a tile.
            pane(9, teammate("gamma"), running(9, t - 1_500), None, t - 1_500),
        ];
        rig.world.claude_team_adapter().table(table(&lead, LeadPhase::Running, panes.clone(), t));
        rig.pump();
        let got = tiles(&rig);
        let ids: Vec<&str> = got.iter().map(|x| x.pane_id.as_str()).collect();
        assert_eq!(ids, vec!["%2", "%3", "%4", "%5", "%6"], "creation order; bound, killed, unlisted and teammate panes are no tiles");
        assert_eq!(got[0].state, PaneTileState::Starting { since: t - 7_000, spawn_failed: false }, "spawning teammate pane");
        assert_eq!(got[1].state, PaneTileState::Starting { since: t - 6_000, spawn_failed: false });
        assert_eq!(got[2].state, PaneTileState::Starting { since: t - 4_000, spawn_failed: true });
        assert_eq!(got[3].state, PaneTileState::Unknown { running: true, exit_code: None });
        assert_eq!(got[4].state, PaneTileState::Unknown { running: false, exit_code: Some(3) });
        let ptys: Vec<Option<&PtyId>> = got.iter().map(|x| x.pty.as_ref()).collect();
        assert_eq!(ptys, vec![None, None, None, Some(&pty5), None], "a PTY only for the running unknown pane");
        assert_eq!(got[3].title.as_deref(), Some("build watch"), "escape sequences and bidi overrides removed");
        assert_eq!(got[2].title, None, "a blank title is none");
        assert_eq!(got[0].created_at, t - 8_000);
        // beta's pane starts and is bound to its session: no longer a tile.
        let mut panes2 = panes;
        panes2[1] = pane(2, teammate("beta"), running(2, t + 100), Some(beta.clone()), t - 8_000);
        rig.world.claude_team_adapter().table(table(&lead, LeadPhase::Running, panes2, t + 200));
        rig.pump();
        let ids: Vec<String> = tiles(&rig).iter().map(|x| x.pane_id.as_str().to_string()).collect();
        assert_eq!(ids, vec!["%3", "%4", "%5", "%6"]);
        let snap = rig.world.snapshot();
        assert!(member(&snap, "beta").is_some_and(|m| m.pane == Some(PaneId::new(2))), "beta's member row shows the pane");
        // Tiles never enter the queue (§7.4.5).
        assert!(rig.world.queue().items.iter().all(|i| !format!("{:?}", i.subject).contains("%5")));
    }

    /// At most 16 tiles, the first 16 in creation order.
    #[test]
    fn t_team_pane_tiles_cap_and_order() {
        let rig = Rig::new();
        rig.subscribe();
        let (lead, alpha, _beta, _scene) = hosted(&rig);
        let t = rig.now();
        let mut panes = vec![pane(1, teammate("alpha"), running(1, t - 9_000), Some(alpha), t - 9_000)];
        for n in 2..=21u32 {
            panes.push(pane(n, PaneKind::Placeholder, PaneState::Placeholder { since: t, spawn_failed: false }, None, t + i64::from(n)));
        }
        rig.world.claude_team_adapter().table(table(&lead, LeadPhase::Running, panes, t));
        rig.pump();
        let got = tiles(&rig);
        assert_eq!(got.len(), 16);
        let ids: Vec<u32> = got.iter().map(|x| x.pane_id.as_str()[1..].parse().unwrap()).collect();
        assert_eq!(ids, (2..=17).collect::<Vec<u32>>());
    }

    /// A table that changes nothing visible publishes no Team upsert; a
    /// changed one publishes exactly one.
    #[test]
    fn t_team_pane_tiles_do_not_churn() {
        let rig = Rig::new();
        rig.subscribe();
        let (lead, alpha, _beta, _scene) = hosted(&rig);
        let t = rig.now();
        let panes = vec![
            pane(1, teammate("alpha"), running(1, t - 9_000), Some(alpha), t - 9_000),
            pane(2, unknown(), running(2, t - 1_000), None, t - 1_000),
        ];
        let mut tb = table(&lead, LeadPhase::Running, panes, t);
        rig.world.claude_team_adapter().table(tb.clone());
        rig.pump();
        assert_eq!(tiles(&rig).len(), 1);
        // The same table again, later (only `at` differs): no Team upsert.
        tb.at = t + 1_000;
        rig.world.claude_team_adapter().table(tb.clone());
        assert_eq!(team_upserts(&rig.pump()), 0, "an unchanged table does not churn the store");
        // The unknown pane exits and is kept: one Team upsert with the new tile.
        tb.panes[1].state = PaneState::Exited { code: Some(2), signal: None, at: t + 1_500, kept: true };
        tb.at = t + 1_500;
        rig.world.claude_team_adapter().table(tb);
        assert_eq!(team_upserts(&rig.pump()), 1);
        assert_eq!(tiles(&rig)[0].state, PaneTileState::Unknown { running: false, exit_code: Some(2) });
    }

    /// Only hosted teams have tiles: the same table for a lead Gilde does
    /// not host gives none.
    #[test]
    fn t_team_pane_tiles_hosted_teams_only() {
        let rig = Rig::new();
        rig.subscribe();
        let (lead, _alpha) = sessions(&rig, false);
        observe(&rig, &Scene::new(&rig, LeadBinding::Exact));
        let t = rig.now();
        let panes = vec![pane(3, unknown(), running(3, t - 1_000), None, t - 1_000)];
        rig.world.claude_team_adapter().table(table(&lead, LeadPhase::Running, panes, t));
        rig.pump();
        assert!(tiles(&rig).is_empty());
    }
}
