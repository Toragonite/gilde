//! T-att-queue-*: the attention queue rules Q1–Q8 (§8.1). Every patch is
//! applied to a WebView mirror that asserts counter == sidebar
//! (`need_you`/`also` equal what the store's sessions show, §3.4.9,
//! pixel-office I6). Q3 (a hosted lead represented by its member) is in
//! `world_teams.rs`, where the team exists.

#[path = "world_support.rs"]
mod support;

use gilde_code_lib::contract::activity::{
    ActivityState, ErrorSub, OpenToolUse, ProcessFacts, RegStatus, StreamFacts, TurnEnd, TurnOutcome, TurnPhase,
};
use gilde_code_lib::contract::activity::PrimaryAction;
use gilde_code_lib::contract::attention::{AttentionReason, AttentionSubject};
use gilde_code_lib::contract::common::{Confidence, Vendor};
use gilde_code_lib::contract::ids::{ProcessKey, RequestId, SessionUid};
use gilde_code_lib::contract::interaction::{InteractionKind, PendingInteraction};
use gilde_code_lib::contract::process::{Leftover, LeftoverReason, ProcRole, ProcessRecord};
use gilde_code_lib::contract::services::{LifecycleOp, SourceFacts, WorldApi};
use gilde_code_lib::contract::session::ControlMode;
use gilde_code_lib::world::WorldSettings;
use support::*;

fn card(kind: InteractionKind, at: i64, n: u32) -> PendingInteraction {
    PendingInteraction {
        request_id: RequestId::parse(format!("toolu_{n}")).unwrap(),
        kind,
        received_at: at,
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
        confidence: Confidence::High,
        multi_select: false,
        questions: vec![],
    }
}

/// A Gilde Conversation session with pending cards.
fn convo(rig: &Rig, n: u32, cards: Vec<PendingInteraction>) -> SessionUid {
    let spawn = format!("sp-q-{n}");
    let uid = rig.world.bind_spawn(&spawn, None, Vendor::Claude, Some(account()));
    let h = spawn_hint(&spawn, None, Some(8000 + n));
    let t = rig.now();
    rig.ev(&h, SourceFacts::Gilde(gilde(t, ControlMode::Conversation, Some(t))));
    rig.ev(&h, alive(8000 + n as i32));
    let phase = if cards.is_empty() { TurnPhase::Running } else { TurnPhase::RequiresAction };
    rig.ev(&h, SourceFacts::Stream(StreamFacts { phase, phase_since: t, pending: cards, last_turn_start_at: Some(t), last_turn_end: None, last_frame_at: t }));
    uid
}

fn set_stream(rig: &Rig, n: u32, s: StreamFacts) {
    let h = spawn_hint(&format!("sp-q-{n}"), None, Some(8000 + n));
    rig.ev(&h, SourceFacts::Stream(s));
}

/// A watched Claude session waiting at its owner.
fn watched_waiting(rig: &Rig, n: u32, input: bool) -> SessionUid {
    let h = claude_hint(Some(&sid(n)), Some(8500 + n));
    rig.ev(&h, alive(8500 + n as i32));
    let mut t = transcript(rig.now() - 1_000, Some(rig.now() - 5_000), None);
    t.open_tool_use = Some(OpenToolUse { tool_use_id: format!("toolu_w{n}"), tool_name: if input { "AskUserQuestion".into() } else { "Edit".into() }, at: rig.now() - 1_000, is_question: input });
    rig.ev(&h, SourceFacts::Transcript(t));
    rig.ev(&h, registry(RegStatus::Waiting, rig.now(), Some(if input { "input needed" } else { "permission prompt" })));
    rig.uid_of(&sid(n))
}

fn keys(rig: &Rig) -> Vec<String> {
    rig.world.queue().items.into_iter().map(|i| i.key).collect()
}

#[test]
fn t_att_queue_q1_q2_membership_by_band() {
    let rig = Rig::new();
    rig.subscribe();
    let a = convo(&rig, 1, vec![card(InteractionKind::ToolApproval, rig.now(), 1)]);
    let q = convo(&rig, 2, vec![card(InteractionKind::Question, rig.now(), 2)]);
    let w = watched_waiting(&rig, 3, false);
    let working = convo(&rig, 4, vec![]);
    rig.pump();
    let queue = rig.world.queue();
    let band = |u: &SessionUid| queue.items.iter().find(|i| i.key == format!("q_{}", u.as_str())).map(|i| i.band);
    assert_eq!(band(&a), Some(1));
    assert_eq!(band(&q), Some(2));
    assert_eq!(band(&w), Some(1), "Watching approvals are band 1 too [M8]");
    assert_eq!(band(&working), None, "Working is not in the queue");
    let wi = queue.items.iter().find(|i| i.key == format!("q_{}", w.as_str())).unwrap();
    assert!(wi.watched);
    assert_eq!(wi.primary, PrimaryAction::GoToOwner);
    // The watched card is synthesized with no options (Go to owner only).
    let s = rig.session(&w);
    assert_eq!(s.pending.len(), 1);
    assert_eq!(s.pending[0].kind, InteractionKind::WatchedApproval);
    assert!(s.pending[0].request_id.as_str().starts_with("wa_"));
    assert!(s.pending[0].options.is_empty());
    // Q2: one item per session.
    let mut ks = keys(&rig);
    let n = ks.len();
    ks.dedup();
    assert_eq!(ks.len(), n);
    // Band 3: an error and a crash; band 4: no progress; band 5: new reply.
    let t = rig.now();
    set_stream(&rig, 4, StreamFacts { phase: TurnPhase::Idle, phase_since: t, pending: vec![], last_turn_start_at: Some(t - 10), last_turn_end: Some(TurnEnd { at: t, outcome: TurnOutcome::Error(ErrorSub::AgentError) }), last_frame_at: t });
    rig.pump();
    let it = rig.world.queue().items.into_iter().find(|i| i.key == format!("q_{}", working.as_str())).unwrap();
    assert_eq!((it.band, it.reason), (3, AttentionReason::Error { sub: ErrorSub::AgentError }));
    let c = convo(&rig, 5, vec![]);
    rig.ev(&spawn_hint("sp-q-5", None, Some(8005)), exited(rig.now(), Some(2), None));
    rig.pump();
    let it = rig.world.queue().items.into_iter().find(|i| i.key == format!("q_{}", c.as_str())).unwrap();
    assert_eq!((it.band, it.reason, it.primary), (3, AttentionReason::Crashed, PrimaryAction::ContinueInGilde));
}

#[test]
fn t_att_queue_q4_ranking() {
    let rig = Rig::new();
    rig.subscribe();
    let t = rig.now();
    let late = convo(&rig, 11, vec![card(InteractionKind::ToolApproval, t - 1_000, 11)]);
    let early = convo(&rig, 12, vec![card(InteractionKind::ToolApproval, t - 60_000, 12)]);
    let e1 = convo(&rig, 13, vec![]);
    let e2 = convo(&rig, 14, vec![]);
    set_stream(&rig, 13, StreamFacts { phase: TurnPhase::Idle, phase_since: t, pending: vec![], last_turn_start_at: Some(t - 9), last_turn_end: Some(TurnEnd { at: t, outcome: TurnOutcome::Error(ErrorSub::AgentError) }), last_frame_at: t });
    rig.pump();
    rig.advance(5_000);
    let t2 = rig.now();
    set_stream(&rig, 14, StreamFacts { phase: TurnPhase::Idle, phase_since: t2, pending: vec![], last_turn_start_at: Some(t2 - 9), last_turn_end: Some(TurnEnd { at: t2, outcome: TurnOutcome::Error(ErrorSub::AgentError) }), last_frame_at: t2 });
    rig.pump();
    let k = |u: &SessionUid| format!("q_{}", u.as_str());
    // Band 1 oldest first; band 3 newest first.
    assert_eq!(keys(&rig), vec![k(&early), k(&late), k(&e2), k(&e1)]);
    let q = rig.world.queue();
    assert!(q.items[0].waited_ms >= 60_000, "uptime-based age");
}

#[test]
fn t_att_queue_q5_later() {
    let rig = Rig::new();
    rig.subscribe();
    let uid = convo(&rig, 21, vec![]);
    let t = rig.now();
    set_stream(&rig, 21, StreamFacts { phase: TurnPhase::Idle, phase_since: t, pending: vec![], last_turn_start_at: Some(t - 9), last_turn_end: Some(TurnEnd { at: t, outcome: TurnOutcome::Error(ErrorSub::AgentError) }), last_frame_at: t });
    rig.pump();
    assert_eq!(rig.world.queue().also, 1);
    let until = rig.world.queue_later(&uid).unwrap();
    assert_eq!(until, rig.now() + 30 * 60 * 1000);
    rig.pump();
    let q = rig.world.queue();
    assert_eq!((q.need_you, q.also), (0, 0), "a Later-ed item counts toward nothing");
    assert!(q.items[0].later_until.is_some());
    // It returns earlier when the session moves to a more urgent band.
    rig.advance(1_000);
    set_stream(&rig, 21, StreamFacts { phase: TurnPhase::RequiresAction, phase_since: rig.now(), pending: vec![card(InteractionKind::ToolApproval, rig.now(), 21)], last_turn_start_at: Some(rig.now()), last_turn_end: None, last_frame_at: rig.now() });
    rig.pump();
    let q = rig.world.queue();
    assert_eq!(q.need_you, 1);
    assert!(q.items[0].later_until.is_none());
    // Later on a band-1 item expires after 30 min of uptime.
    rig.world.queue_later(&uid).unwrap();
    rig.pump();
    assert_eq!(rig.world.queue().need_you, 0);
    rig.advance(30 * 60 * 1000 + 1);
    rig.pump();
    assert_eq!(rig.world.queue().need_you, 1);
    // No item → NOT_FOUND{queueItem}.
    let none = convo(&rig, 22, vec![]);
    rig.pump();
    let err = rig.world.queue_later(&none).unwrap_err();
    assert_eq!(err.params.get("entity").and_then(|v| v.as_str()), Some("queueItem"));
}

#[test]
fn t_att_queue_q6_auto_clear_in_the_same_tick() {
    let rig = Rig::new();
    rig.subscribe();
    let uid = convo(&rig, 31, vec![card(InteractionKind::ToolApproval, rig.now(), 31)]);
    let w = watched_waiting(&rig, 32, false);
    rig.pump();
    assert_eq!(rig.world.queue().need_you, 2);
    // Answered in Gilde: the next stream fact has no pending card.
    set_stream(&rig, 31, StreamFacts { phase: TurnPhase::Running, phase_since: rig.now(), pending: vec![], last_turn_start_at: Some(rig.now()), last_turn_end: None, last_frame_at: rig.now() });
    // Answered at the owner: a decision row in the transcript after the
    // waiting status (the registry is now behind the rows).
    rig.advance(3_000);
    let h = claude_hint(Some(&sid(32)), Some(8532));
    rig.ev(&h, SourceFacts::Transcript(transcript(rig.now(), Some(rig.now() - 60_000), None)));
    let p = rig.pump();
    assert_eq!(p.len(), 1, "both leave in the same World tick");
    let q = rig.world.queue();
    assert_eq!(q.need_you, 0);
    assert!(!q.items.iter().any(|i| i.key == format!("q_{}", uid.as_str()) || i.key == format!("q_{}", w.as_str())));
}

#[test]
fn t_att_queue_q7_counts_and_notify_on_every_change() {
    let rig = Rig::new();
    rig.subscribe();
    convo(&rig, 41, vec![card(InteractionKind::ToolApproval, rig.now(), 41)]);
    watched_waiting(&rig, 42, true);
    convo(&rig, 43, vec![]);
    rig.pump();
    let q = rig.world.queue();
    assert_eq!((q.need_you, q.also), (2, 0));
    let last = rig.notify.queues.lock().last().cloned().unwrap();
    assert_eq!((last.need_you, last.also), (2, 0), "NotifyApi::queue_changed sees what the store shows");
    // Pause shows in the queue.
    rig.world.set_paused_until(Some(rig.now() + 3_600_000));
    rig.pump();
    assert_eq!(rig.world.queue().paused_until, Some(rig.now() + 3_600_000));
    // The rev increases on each change only.
    let rev = rig.world.queue().rev;
    rig.pump();
    assert_eq!(rig.world.queue().rev, rev);
}

#[test]
fn t_att_queue_q8_one_leftovers_item() {
    let rig = Rig::new();
    rig.subscribe();
    for pid in [9001, 9002] {
        rig.sink.lifecycle(LifecycleOp::LeftoverUpsert(Box::new(ProcessRecord {
            key: ProcessKey::new(pid, 1_792_700_000),
            pid: pid as i32,
            start_sec: 1_792_700_000,
            vendor: Some(Vendor::Claude),
            role: ProcRole::Teammate,
            session_uid: None,
            gilde_owned: false,
            rss_bytes: 100_000_000,
            leftover: Some(Leftover { reason: LeftoverReason::TeammateOfGoneLead, since: rig.now(), held_scans: 2 }),
        })));
    }
    rig.pump();
    let q = rig.world.queue();
    assert_eq!(q.items.len(), 1);
    let it = &q.items[0];
    assert_eq!((it.key.as_str(), it.band, it.primary), ("q_leftovers", 6, PrimaryAction::CleanUp));
    assert_eq!(it.subject, AttentionSubject::Leftovers { count: 2, bytes: 200_000_000 });
    assert_eq!((q.need_you, q.also), (0, 1));
}

#[test]
fn watched_new_reply_only_with_the_setting() {
    let rig = Rig::new();
    rig.subscribe();
    let h = claude_hint(Some(&sid(51)), Some(8551));
    rig.ev(&h, alive(8551));
    rig.ev(&h, registry(RegStatus::Busy, rig.now(), None));
    rig.ev(&h, SourceFacts::Transcript(transcript(rig.now(), Some(rig.now() - 100), None)));
    rig.pump();
    rig.advance(5_000);
    rig.ev(&h, registry(RegStatus::Idle, rig.now(), None));
    rig.ev(&h, SourceFacts::Transcript(transcript(rig.now(), Some(rig.now() - 5_000), Some(rig.now()))));
    rig.pump();
    let uid = rig.uid_of(&sid(51));
    assert_eq!(rig.state(&uid), ActivityState::Ready);
    rig.world.set_settings(WorldSettings { watched_band5_allowed: true, ..WorldSettings::default() });
    rig.pump();
    assert_eq!(rig.state(&uid), ActivityState::NewReply);
    assert_eq!(rig.world.queue().items[0].band, 5);
    // Gone → Ended; it leaves the queue.
    rig.ev(&h, SourceFacts::Process(ProcessFacts::Gone { at: rig.now() }));
    rig.pump();
    assert!(rig.world.queue().items.is_empty());
}

#[test]
fn t_world_notify_paused() {
    use gilde_code_lib::contract::attention::PAUSED_UNTIL_RESUME;
    use gilde_code_lib::contract::store::StoreOp;
    let rig = Rig::new();
    rig.subscribe();
    convo(&rig, 61, vec![card(InteractionKind::ToolApproval, rig.now(), 61)]);
    rig.pump();
    let before = rig.notify.queues.lock().len();
    // "Until I resume" (A16.2).
    rig.sink.notify_paused(Some(PAUSED_UNTIL_RESUME));
    let p = rig.pump();
    let q = p.iter().flat_map(|x| x.ops.clone()).find_map(|o| match o {
        StoreOp::Queue { queue } => Some(queue),
        _ => None,
    });
    assert_eq!(q.expect("a Queue op").paused_until, Some(PAUSED_UNTIL_RESUME));
    assert_eq!(rig.world.queue().paused_until, Some(PAUSED_UNTIL_RESUME));
    assert_eq!(rig.world.queue().need_you, 1, "pausing never hides the queue");
    assert!(rig.notify.queues.lock().len() > before, "NotifyApi::queue_changed after every queue change");
    // The same pause again: no op.
    rig.sink.notify_paused(Some(PAUSED_UNTIL_RESUME));
    assert!(rig.pump().is_empty());
    // A timed pause, then resume.
    let until = rig.now() + 3_600_000;
    rig.sink.notify_paused(Some(until));
    rig.pump();
    assert_eq!(rig.world.queue().paused_until, Some(until));
    rig.sink.notify_paused(None);
    rig.pump();
    assert_eq!(rig.world.queue().paused_until, None);
    assert_eq!(rig.notify.queues.lock().last().unwrap().paused_until, None);
}

/// A NotifyApi that reads World back inside `queue_changed`: it would
/// deadlock if World called it under its own lock.
struct Reentrant {
    world: parking_lot::Mutex<Option<gilde_code_lib::world::World>>,
    revs: parking_lot::Mutex<Vec<u64>>,
}

#[async_trait::async_trait]
impl gilde_code_lib::contract::services::NotifyApi for Reentrant {
    fn view_state(&self, _state: gilde_code_lib::contract::ipc::ViewState) {}
    fn input_activity(&self) {}
    fn queue_changed(&self, queue: &gilde_code_lib::contract::attention::AttentionQueue) {
        let w = self.world.lock().clone();
        if let Some(w) = w {
            // Re-enter World: snapshot and queue take its lock.
            assert_eq!(w.queue().rev, queue.rev);
            let _ = w.snapshot();
        }
        self.revs.lock().push(queue.rev);
    }
    fn visibility(&self) -> gilde_code_lib::contract::store::VisibilityEvent {
        gilde_code_lib::contract::store::VisibilityEvent { frontmost: true, occluded: false, asleep: false }
    }
    fn pause(&self, _until: gilde_code_lib::contract::ipc::PauseUntil) -> Option<i64> {
        None
    }
    fn resume(&self) {}
    async fn permission_request(&self) -> bool {
        false
    }
    async fn test(&self) -> bool {
        false
    }
}

#[test]
fn queue_changed_after_every_change_outside_the_lock() {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rig = Rig::new();
        let n = std::sync::Arc::new(Reentrant { world: parking_lot::Mutex::new(Some(rig.world.clone())), revs: parking_lot::Mutex::new(vec![]) });
        rig.world.set_notify(n.clone());
        rig.subscribe();
        // Several queue changes inside one flush window.
        let a = convo(&rig, 71, vec![card(InteractionKind::ToolApproval, rig.now(), 71)]);
        convo(&rig, 72, vec![card(InteractionKind::Question, rig.now(), 72)]);
        rig.world.queue_later(&a).unwrap();
        rig.sink.notify_paused(Some(rig.now() + 60_000));
        set_stream(&rig, 71, StreamFacts { phase: TurnPhase::Running, phase_since: rig.now(), pending: vec![], last_turn_start_at: Some(rig.now()), last_turn_end: None, last_frame_at: rig.now() });
        rig.pump();
        let revs = n.revs.lock().clone();
        *n.world.lock() = None;
        tx.send((revs, rig.world.queue().rev)).unwrap();
    });
    let (revs, last) = rx.recv_timeout(std::time::Duration::from_secs(60)).expect("no deadlock: notified outside the World lock");
    assert!(!revs.is_empty());
    assert_eq!(revs, (1..=last).collect::<Vec<u64>>(), "every queue change is notified, in order, none skipped");
}
