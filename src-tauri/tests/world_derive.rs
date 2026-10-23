//! T-derive-*: `derive()` table tests for every rule R0–R11 (§3.5.4) with
//! the staleness rows (§3.5.3), the hosted-lead registry rule
//! (T-derive-hostedLeadBusy), uptime vs wall with a simulated sleep
//! (T-derive-sleep), the §3.5.5 transitions and §3.5.6 entry/exit rows
//! through World, episode boundaries, and the NotRunning invariant
//! [F-14, F-21]. Synthetic facts only.

#[path = "world_support.rs"]
mod support;

use gilde_code_lib::contract::activity::{
    ActivityState, DidntStartReason, ErrorSub, EvidenceSet, ExitInfo, GildeFacts, OpenToolUse, ProcessFacts, PtyFacts,
    PtyStatus, RegStatus, RegistryFacts, StartFailure, StopCause, StreamFacts, TeamFileFacts, TranscriptFacts, TurnEnd,
    TurnOutcome, TurnPhase, UnknownWhy, UserMarks,
};
use gilde_code_lib::contract::common::{Confidence, EpochMs, Vendor};
use gilde_code_lib::contract::ids::RequestId;
use gilde_code_lib::contract::interaction::{InteractionKind, PendingInteraction};
use gilde_code_lib::contract::ipc::DismissWhat;
use gilde_code_lib::contract::services::{IdentityHint, SourceFacts, WorldApi};
use gilde_code_lib::contract::session::{Control, ControlMode};
use gilde_code_lib::contract::team::IdleReason;
use gilde_code_lib::world::derive::{derive, ControlKind, DeriveCtx, Stamps, TeamPosition};
use support::*;

const NOW_UP: u64 = 10_000_000;
const NOW: EpochMs = 1_792_749_600_000;

fn alive_p() -> ProcessFacts {
    ProcessFacts::Alive { pid: 77, start_sec: 1, cpu_active_at: None }
}

fn ev(process: ProcessFacts) -> EvidenceSet {
    EvidenceSet {
        gilde: None,
        process,
        stream: None,
        registry: None,
        transcript: None,
        team: None,
        pty: None,
        marks: UserMarks::default(),
        lead_has_working_members: false,
        watched_band5_allowed: false,
        no_progress_after_secs: 300,
        source_denied: false,
    }
}

fn cx(control: ControlKind) -> DeriveCtx {
    DeriveCtx {
        vendor: Vendor::Claude,
        control,
        position: TeamPosition::Solo,
        held_turn_based: false,
        delegated: None,
        stamps: Stamps::default(),
        now_up: NOW_UP,
        now_wall: NOW,
    }
}

fn ready_gilde(mode: ControlMode) -> GildeFacts {
    GildeFacts { spawned_at: NOW - 60_000, mode, ready_at: Some(NOW - 55_000), stop_cause: None, signals_sent: vec![], start_failure: None }
}

fn starting_gilde(mode: ControlMode) -> GildeFacts {
    GildeFacts { ready_at: None, ..ready_gilde(mode) }
}

fn pending(kind: InteractionKind, at: EpochMs) -> PendingInteraction {
    PendingInteraction {
        request_id: RequestId::parse(format!("req-{at}")).unwrap(),
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

fn reg(status: RegStatus, at: EpochMs, waiting_for: Option<&str>) -> RegistryFacts {
    RegistryFacts { status: Some(status), waiting_for: waiting_for.map(str::to_string), status_updated_at: Some(at), peer_protocol_ok: true }
}

fn tr(last_row: EpochMs, prompt: Option<EpochMs>, end: Option<EpochMs>) -> TranscriptFacts {
    TranscriptFacts { last_row_at: Some(last_row), open_tool_use: None, last_turn_end_at: end, last_human_prompt_at: prompt, last_api_error: None }
}

fn st(e: &EvidenceSet, c: &DeriveCtx) -> ActivityState {
    derive(e, c).state
}

// ---------------------------------------------------------------------------
// one table test per rule
// ---------------------------------------------------------------------------

#[test]
fn t_derive_r0_held_turn_based_session() {
    // The exit of a turn's process is not a session exit.
    let mut e = ev(ProcessFacts::Exited { at: NOW - 1_000, code: Some(0), signal: None });
    e.gilde = Some(ready_gilde(ControlMode::TurnBased));
    e.marks.last_seen_at = Some(NOW);
    e.stream = Some(StreamFacts { phase: TurnPhase::Idle, phase_since: NOW - 1_000, pending: vec![], last_turn_start_at: Some(NOW - 5_000), last_turn_end: Some(TurnEnd { at: NOW - 1_000, outcome: TurnOutcome::Ok }), last_frame_at: NOW - 1_000 });
    let mut c = cx(ControlKind::InGilde(ControlMode::TurnBased));
    c.held_turn_based = true;
    let d = derive(&e, &c);
    assert_eq!((d.state.clone(), d.rule), (ActivityState::Ready, "R0"));
    // Not seen → New reply.
    e.marks.last_seen_at = Some(NOW - 2_000);
    assert_eq!(st(&e, &c), ActivityState::NewReply);
    // A run error → Error.
    e.stream.as_mut().unwrap().last_turn_end = Some(TurnEnd { at: NOW - 1_000, outcome: TurnOutcome::Error(ErrorSub::AgentError) });
    assert_eq!(st(&e, &c), ActivityState::Error { sub: ErrorSub::AgentError });
    // A turn running → Working (R8).
    e.stream.as_mut().unwrap().phase = TurnPhase::Running;
    c.stamps.frame_up = Some(NOW_UP - 1_000);
    assert_eq!(st(&e, &c), ActivityState::Working);
}

#[test]
fn t_derive_r1_stopped_after_a_gilde_stop_even_with_exit_1_or_143() {
    for (process, cause) in [
        (ProcessFacts::Exited { at: NOW, code: Some(143), signal: None }, StopCause::User),
        (ProcessFacts::Exited { at: NOW, code: Some(1), signal: None }, StopCause::Quit),
        (ProcessFacts::Exited { at: NOW, code: None, signal: Some(15) }, StopCause::ForceStop),
        (ProcessFacts::Gone { at: NOW }, StopCause::User),
        (ProcessFacts::HistoryOnly, StopCause::Quit),
    ] {
        let mut e = ev(process);
        let mut g = ready_gilde(ControlMode::Conversation);
        g.stop_cause = Some(cause);
        e.gilde = Some(g.clone());
        let d = derive(&e, &cx(ControlKind::NotRunning));
        assert_eq!((d.state, d.rule), (ActivityState::Stopped, "R1"), "{process:?} {cause:?}");
        // R1 is first even before ready.
        g.ready_at = None;
        e.gilde = Some(g);
        assert_eq!(st(&e, &cx(ControlKind::NotRunning)), ActivityState::Stopped);
    }
}

#[test]
fn t_derive_r4_start_failures_before_ready() {
    // Compat / min version.
    for (f, r) in [(StartFailure::CompatFailed, DidntStartReason::CompatFailed), (StartFailure::MinVersion, DidntStartReason::MinVersion)] {
        let mut e = ev(alive_p());
        let mut g = starting_gilde(ControlMode::Conversation);
        g.start_failure = Some(f);
        e.gilde = Some(g);
        let d = derive(&e, &cx(ControlKind::InGilde(ControlMode::Conversation)));
        assert_eq!((d.state, d.rule), (ActivityState::Error { sub: ErrorSub::DidntStart { reason: r } }, "R4"));
    }
    // Exit 1 before initialize → Didn't start · Exited, never Crashed (PM-65).
    let mut e = ev(ProcessFacts::Exited { at: NOW, code: Some(1), signal: None });
    e.gilde = Some(starting_gilde(ControlMode::Conversation));
    assert_eq!(st(&e, &cx(ControlKind::NotRunning)), ActivityState::Error { sub: ErrorSub::DidntStart { reason: DidntStartReason::Exited } });
    // A foreign signal before ready is the same.
    e.process = ProcessFacts::Exited { at: NOW, code: None, signal: Some(9) };
    assert_eq!(st(&e, &cx(ControlKind::NotRunning)), ActivityState::Error { sub: ErrorSub::DidntStart { reason: DidntStartReason::Exited } });
    // Exit 0 before ready → Ended.
    e.process = ProcessFacts::Exited { at: NOW, code: Some(0), signal: None };
    assert_eq!(st(&e, &cx(ControlKind::NotRunning)), ActivityState::Ended);
    // Alive: Starting until 30 s of uptime, then Timeout.
    let mut e = ev(alive_p());
    e.gilde = Some(starting_gilde(ControlMode::Terminal));
    let mut c = cx(ControlKind::InGilde(ControlMode::Terminal));
    c.stamps.spawn_up = Some(NOW_UP - 29_000);
    let d = derive(&e, &c);
    assert_eq!(d.state, ActivityState::Starting);
    assert_eq!(d.recheck_up, Some(NOW_UP - 29_000 + 30_000 + 1));
    c.stamps.spawn_up = Some(NOW_UP - 31_000);
    assert_eq!(st(&e, &c), ActivityState::Error { sub: ErrorSub::DidntStart { reason: DidntStartReason::Timeout } });
    // Time spent with a prompt on screen does not count.
    c.stamps.starting_paused_ms = 5_000;
    assert_eq!(st(&e, &c), ActivityState::Starting);
    // A prompt on screen → R5/R6, never a timeout.
    c.stamps.spawn_up = Some(NOW_UP - 300_000);
    e.pty = Some(PtyFacts { status: PtyStatus::WaitingForInput, confidence: 0.9, last_output_at: None, prompt: Some(prompt("aaaaaaaaaaaaaaaa", true, None, NOW - 1_000)) });
    assert_eq!(st(&e, &c), ActivityState::NeedsApproval);
    e.pty.as_mut().unwrap().prompt = Some(prompt("bbbbbbbbbbbbbbbb", false, None, NOW - 1_000));
    assert_eq!(st(&e, &c), ActivityState::NeedsAnswer);
}

#[test]
fn t_derive_r2_crashed_only_for_own_children_after_ready() {
    let mut e = ev(ProcessFacts::Exited { at: NOW, code: Some(1), signal: None });
    e.gilde = Some(ready_gilde(ControlMode::Conversation));
    let d = derive(&e, &cx(ControlKind::NotRunning));
    assert_eq!((d.state, d.rule), (ActivityState::Crashed { exit: ExitInfo { code: Some(1), signal: None } }, "R2"));
    // A signal Gilde did not send.
    e.process = ProcessFacts::Exited { at: NOW, code: None, signal: Some(9) };
    assert!(matches!(st(&e, &cx(ControlKind::NotRunning)), ActivityState::Crashed { .. }));
    // A signal Gilde sent is not a crash.
    let mut g = ready_gilde(ControlMode::Conversation);
    g.signals_sent = vec![15];
    e.gilde = Some(g);
    e.process = ProcessFacts::Exited { at: NOW, code: None, signal: Some(15) };
    assert_eq!(st(&e, &cx(ControlKind::NotRunning)), ActivityState::Ended);
    // Dismissed → Ended.
    e.gilde = Some(ready_gilde(ControlMode::Conversation));
    e.process = ProcessFacts::Exited { at: NOW, code: Some(2), signal: None };
    e.marks.crash_dismissed_at = Some(NOW + 1);
    assert_eq!(st(&e, &cx(ControlKind::NotRunning)), ActivityState::Ended);
    // The CLI exits 1 after "Not logged in": the actionable error stays (LP-09b).
    let mut e = ev(ProcessFacts::Exited { at: NOW, code: Some(1), signal: None });
    e.gilde = Some(ready_gilde(ControlMode::Conversation));
    e.stream = Some(StreamFacts { phase: TurnPhase::Idle, phase_since: NOW - 10, pending: vec![], last_turn_start_at: Some(NOW - 100), last_turn_end: Some(TurnEnd { at: NOW - 10, outcome: TurnOutcome::Error(ErrorSub::SignInNeeded) }), last_frame_at: NOW - 10 });
    assert_eq!(st(&e, &cx(ControlKind::NotRunning)), ActivityState::Error { sub: ErrorSub::SignInNeeded });
    e.stream.as_mut().unwrap().last_turn_end = Some(TurnEnd { at: NOW - 10, outcome: TurnOutcome::Error(ErrorSub::UsageLimit { account_id: None, resets_at: None }) });
    assert_eq!(st(&e, &cx(ControlKind::NotRunning)), ActivityState::Error { sub: ErrorSub::UsageLimit { account_id: None, resets_at: None } });
    // A watched process's exit status is not observable → Ended.
    let e = ev(ProcessFacts::Exited { at: NOW, code: Some(1), signal: None });
    assert_eq!(st(&e, &cx(ControlKind::NotRunning)), ActivityState::Ended);
}

#[test]
fn t_derive_r3_ended() {
    for p in [ProcessFacts::Exited { at: NOW, code: Some(0), signal: None }, ProcessFacts::Gone { at: NOW }, ProcessFacts::HistoryOnly] {
        let d = derive(&ev(p), &cx(ControlKind::NotRunning));
        assert_eq!((d.state, d.rule), (ActivityState::Ended, "R3"));
    }
    let mut e = ev(ProcessFacts::Exited { at: NOW, code: Some(0), signal: None });
    e.gilde = Some(ready_gilde(ControlMode::Terminal));
    assert_eq!(st(&e, &cx(ControlKind::NotRunning)), ActivityState::Ended);
}

#[test]
fn t_derive_r5_needs_approval_from_each_source() {
    // S1.
    let mut e = ev(alive_p());
    e.gilde = Some(ready_gilde(ControlMode::Conversation));
    e.stream = Some(StreamFacts { phase: TurnPhase::RequiresAction, phase_since: NOW - 5, pending: vec![pending(InteractionKind::ToolApproval, NOW - 400), pending(InteractionKind::ToolApproval, NOW - 100)], last_turn_start_at: Some(NOW - 1_000), last_turn_end: None, last_frame_at: NOW - 5 });
    let d = derive(&e, &cx(ControlKind::InGilde(ControlMode::Conversation)));
    assert_eq!((d.state, d.since_hint), (ActivityState::NeedsApproval, Some(NOW - 400)));
    // S5 permission-shaped prompt.
    let mut e = ev(alive_p());
    e.gilde = Some(ready_gilde(ControlMode::Terminal));
    e.pty = Some(PtyFacts { status: PtyStatus::WaitingForInput, confidence: 0.9, last_output_at: Some(NOW - 10), prompt: Some(prompt("cccccccccccccccc", true, None, NOW - 50)) });
    assert_eq!(st(&e, &cx(ControlKind::InGilde(ControlMode::Terminal))), ActivityState::NeedsApproval);
    // S2 waiting, not for input.
    let mut e = ev(alive_p());
    e.registry = Some(reg(RegStatus::Waiting, NOW - 10, Some("permission prompt")));
    assert_eq!(st(&e, &cx(ControlKind::Watching)), ActivityState::NeedsApproval);
    // A stale waiting (behind the rows: answered at the owner) is ignored.
    e.transcript = Some(tr(NOW - 1, Some(NOW - 5_000), None));
    e.registry = Some(reg(RegStatus::Waiting, NOW - 10_000, Some("permission prompt")));
    assert_ne!(st(&e, &cx(ControlKind::Watching)), ActivityState::NeedsApproval);
    // S4 permission_request attributed to this member.
    let mut e = ev(alive_p());
    e.team = Some(TeamFileFacts { is_active: Some(true), in_roster: true, permission_request: Some((NOW - 300, "Bash".into())), last_idle: None });
    e.transcript = Some(tr(NOW - 600, Some(NOW - 2_000), None));
    let mut c = cx(ControlKind::Watching);
    c.position = TeamPosition::Teammate;
    assert_eq!(st(&e, &c), ActivityState::NeedsApproval);
    // Cleared when the member's transcript advanced past it.
    e.transcript = Some(tr(NOW - 100, Some(NOW - 2_000), None));
    assert_ne!(st(&e, &c), ActivityState::NeedsApproval);
}

#[test]
fn t_derive_r6_needs_answer_from_each_source() {
    for k in [InteractionKind::Question, InteractionKind::PlanApproval] {
        let mut e = ev(alive_p());
        e.gilde = Some(ready_gilde(ControlMode::Conversation));
        e.stream = Some(StreamFacts { phase: TurnPhase::RequiresAction, phase_since: NOW, pending: vec![pending(k, NOW - 10)], last_turn_start_at: None, last_turn_end: None, last_frame_at: NOW });
        assert_eq!(st(&e, &cx(ControlKind::InGilde(ControlMode::Conversation))), ActivityState::NeedsAnswer);
    }
    // S5 choice prompt and setup prompt.
    let mut e = ev(alive_p());
    e.gilde = Some(ready_gilde(ControlMode::Terminal));
    e.pty = Some(PtyFacts { status: PtyStatus::WaitingForInput, confidence: 0.9, last_output_at: None, prompt: Some(prompt("dddddddddddddddd", false, None, NOW)) });
    assert_eq!(st(&e, &cx(ControlKind::InGilde(ControlMode::Terminal))), ActivityState::NeedsAnswer);
    let mut setup = prompt("eeeeeeeeeeeeeeee", false, None, NOW);
    setup.setup = true;
    setup.options.iter_mut().for_each(|o| o.key.clear());
    e.pty.as_mut().unwrap().prompt = Some(setup);
    assert_eq!(st(&e, &cx(ControlKind::InGilde(ControlMode::Terminal))), ActivityState::NeedsAnswer);
    // S2 waiting for input.
    let mut e = ev(alive_p());
    e.registry = Some(reg(RegStatus::Waiting, NOW, Some("input needed")));
    assert_eq!(st(&e, &cx(ControlKind::Watching)), ActivityState::NeedsAnswer);
    // S3 open question tool while S2 waiting.
    let mut e = ev(alive_p());
    e.registry = Some(reg(RegStatus::Waiting, NOW, Some("permission prompt")));
    let mut t = tr(NOW - 100, Some(NOW - 5_000), None);
    t.open_tool_use = Some(OpenToolUse { tool_use_id: "toolu_1".into(), tool_name: "AskUserQuestion".into(), at: NOW - 100, is_question: true });
    e.transcript = Some(t);
    assert_eq!(st(&e, &cx(ControlKind::Watching)), ActivityState::NeedsAnswer);
}

#[test]
fn t_derive_r7_error_since_the_last_turn_start() {
    let mut e = ev(alive_p());
    e.gilde = Some(ready_gilde(ControlMode::Conversation));
    let lim = ErrorSub::UsageLimit { account_id: None, resets_at: Some(NOW + 3_600_000) };
    e.stream = Some(StreamFacts { phase: TurnPhase::Idle, phase_since: NOW - 10, pending: vec![], last_turn_start_at: Some(NOW - 1_000), last_turn_end: Some(TurnEnd { at: NOW - 10, outcome: TurnOutcome::Error(lim.clone()) }), last_frame_at: NOW - 10 });
    let c = cx(ControlKind::InGilde(ControlMode::Conversation));
    assert_eq!(st(&e, &c), ActivityState::Error { sub: lim.clone() });
    // Dismissed → Ready.
    e.marks.error_dismissed_at = Some(NOW);
    e.marks.last_seen_at = Some(NOW);
    assert_eq!(st(&e, &c), ActivityState::Ready);
    // The next turn starts → Working.
    e.marks.error_dismissed_at = None;
    let s = e.stream.as_mut().unwrap();
    s.last_turn_start_at = Some(NOW);
    s.phase = TurnPhase::Running;
    assert_eq!(st(&e, &c), ActivityState::Working);
    // S3: an api-error row after the last human prompt.
    let mut e = ev(alive_p());
    let mut t = tr(NOW - 10, Some(NOW - 1_000), Some(NOW - 10));
    t.last_api_error = Some((NOW - 10, ErrorSub::AgentError));
    e.transcript = Some(t.clone());
    assert_eq!(st(&e, &cx(ControlKind::Watching)), ActivityState::Error { sub: ErrorSub::AgentError });
    // ... not when a new human prompt followed it.
    t.last_human_prompt_at = Some(NOW - 5);
    e.transcript = Some(t);
    assert_ne!(st(&e, &cx(ControlKind::Watching)), ActivityState::Error { sub: ErrorSub::AgentError });
    // S4: an idle_notification with idleReason failed is Agent error (§7.2.2).
    let mut e = ev(alive_p());
    e.team = Some(TeamFileFacts { is_active: Some(false), in_roster: true, permission_request: None, last_idle: Some((NOW - 5, IdleReason::Failed)) });
    let mut c = cx(ControlKind::Watching);
    c.position = TeamPosition::Teammate;
    assert_eq!(st(&e, &c), ActivityState::Error { sub: ErrorSub::AgentError });
}

#[test]
fn t_derive_r8_working_from_each_source() {
    // S1 Running.
    let mut e = ev(alive_p());
    e.gilde = Some(ready_gilde(ControlMode::Conversation));
    e.stream = Some(StreamFacts { phase: TurnPhase::Running, phase_since: NOW - 10, pending: vec![], last_turn_start_at: Some(NOW - 10), last_turn_end: None, last_frame_at: NOW - 10 });
    let mut c = cx(ControlKind::InGilde(ControlMode::Conversation));
    c.stamps.frame_up = Some(NOW_UP - 10);
    assert_eq!(st(&e, &c), ActivityState::Working);
    // S2 busy, fresh.
    let mut e = ev(alive_p());
    e.registry = Some(reg(RegStatus::Busy, NOW - 10, None));
    let mut c = cx(ControlKind::Watching);
    c.stamps.row_up = Some(NOW_UP - 10);
    assert_eq!(st(&e, &c), ActivityState::Working);
    // S3 rows within 30 s and no S2.
    let mut e = ev(alive_p());
    e.transcript = Some(tr(NOW - 5_000, Some(NOW - 20_000), None));
    let mut c = cx(ControlKind::Watching);
    c.stamps.row_up = Some(NOW_UP - 5_000);
    let d = derive(&e, &c);
    assert_eq!(d.state, ActivityState::Working);
    assert_eq!(d.recheck_up, Some(NOW_UP - 5_000 + 30_000), "re-derived when the 30 s window closes");
    // S4 isActive.
    let mut e = ev(alive_p());
    e.team = Some(team_facts(Some(true)));
    let mut c = cx(ControlKind::Watching);
    c.position = TeamPosition::Teammate;
    assert_eq!(st(&e, &c), ActivityState::Working);
    // S5 Working.
    let mut e = ev(alive_p());
    e.gilde = Some(ready_gilde(ControlMode::Terminal));
    e.pty = Some(PtyFacts { status: PtyStatus::Working, confidence: 0.9, last_output_at: Some(NOW - 1), prompt: None });
    let mut c = cx(ControlKind::InGilde(ControlMode::Terminal));
    c.stamps.output_up = Some(NOW_UP - 1);
    assert_eq!(st(&e, &c), ActivityState::Working);
}

#[test]
fn t_derive_r8np_no_progress() {
    let mut e = ev(alive_p());
    e.gilde = Some(ready_gilde(ControlMode::Terminal));
    e.pty = Some(PtyFacts { status: PtyStatus::Working, confidence: 0.9, last_output_at: Some(NOW - 400_000), prompt: None });
    let mut c = cx(ControlKind::InGilde(ControlMode::Terminal));
    c.stamps.output_up = Some(NOW_UP - 400_000);
    assert_eq!(st(&e, &c), ActivityState::NoProgress { for_secs: 400 });
    // The setting (60..=3600).
    e.no_progress_after_secs = 600;
    assert_eq!(st(&e, &c), ActivityState::Working);
    e.no_progress_after_secs = 5; // clamped to 60
    c.stamps.output_up = Some(NOW_UP - 61_000);
    assert_eq!(st(&e, &c), ActivityState::NoProgress { for_secs: 61 });
    // CPU-active ≥ 1 % keeps it Working.
    c.stamps.cpu_up = Some(NOW_UP - 1_000);
    assert_eq!(st(&e, &c), ActivityState::Working);
    // A lead with working members is exempt.
    c.stamps.cpu_up = None;
    e.lead_has_working_members = true;
    assert_eq!(st(&e, &c), ActivityState::Working);
    // Registry busy while the newest row is > 10 min old (PM-01), even
    // with the setting at 1 h.
    let mut e = ev(alive_p());
    e.no_progress_after_secs = 3_600;
    e.registry = Some(reg(RegStatus::Busy, NOW - 700_000, None));
    e.transcript = Some(tr(NOW - 700_500, Some(NOW - 800_000), None));
    let mut c = cx(ControlKind::Watching);
    c.stamps.row_up = Some(NOW_UP - 700_500);
    assert!(matches!(st(&e, &c), ActivityState::NoProgress { .. }));
}

#[test]
fn t_derive_r9_new_reply() {
    let mut e = ev(alive_p());
    e.gilde = Some(ready_gilde(ControlMode::Conversation));
    e.stream = Some(StreamFacts { phase: TurnPhase::Idle, phase_since: NOW - 10, pending: vec![], last_turn_start_at: Some(NOW - 100), last_turn_end: Some(TurnEnd { at: NOW - 10, outcome: TurnOutcome::Ok }), last_frame_at: NOW - 10 });
    let c = cx(ControlKind::InGilde(ControlMode::Conversation));
    e.marks.last_seen_at = Some(NOW - 50);
    assert_eq!(derive(&e, &c).rule, "R9");
    e.marks.last_seen_at = Some(NOW - 10);
    assert_eq!(st(&e, &c), ActivityState::Ready, "seen");
    e.marks.last_seen_at = Some(NOW - 50);
    e.marks.new_reply_dismissed_at = Some(NOW);
    assert_eq!(st(&e, &c), ActivityState::Ready, "dismissed");
    // Watching: only when the setting allows.
    let mut e = ev(alive_p());
    e.registry = Some(reg(RegStatus::Idle, NOW - 5, None));
    e.transcript = Some(tr(NOW - 10, Some(NOW - 100), Some(NOW - 10)));
    e.marks.last_seen_at = Some(NOW - 50);
    assert_eq!(st(&e, &cx(ControlKind::Watching)), ActivityState::Ready);
    e.watched_band5_allowed = true;
    assert_eq!(st(&e, &cx(ControlKind::Watching)), ActivityState::NewReply);
}

#[test]
fn t_derive_r10_ready_from_each_source() {
    let mut e = ev(alive_p());
    e.gilde = Some(ready_gilde(ControlMode::Conversation));
    e.stream = Some(StreamFacts { phase: TurnPhase::Idle, phase_since: NOW, pending: vec![], last_turn_start_at: None, last_turn_end: None, last_frame_at: NOW });
    assert_eq!(derive(&e, &cx(ControlKind::InGilde(ControlMode::Conversation))).rule, "R10");
    let mut e = ev(alive_p());
    e.registry = Some(reg(RegStatus::Idle, NOW, None));
    assert_eq!(st(&e, &cx(ControlKind::Watching)), ActivityState::Ready);
    let mut e = ev(alive_p());
    e.team = Some(team_facts(Some(false)));
    let mut c = cx(ControlKind::Watching);
    c.position = TeamPosition::Teammate;
    assert_eq!(st(&e, &c), ActivityState::Ready);
    let mut e = ev(alive_p());
    e.gilde = Some(ready_gilde(ControlMode::Terminal));
    e.pty = Some(PtyFacts { status: PtyStatus::Idle, confidence: 0.9, last_output_at: None, prompt: None });
    assert_eq!(st(&e, &cx(ControlKind::InGilde(ControlMode::Terminal))), ActivityState::Ready);
}

#[test]
fn t_derive_r11_status_unknown() {
    let mut c = cx(ControlKind::Watching);
    for (vendor, why) in [(Vendor::Cursor, UnknownWhy::ProcessOnly), (Vendor::Agy, UnknownWhy::ProcessOnly), (Vendor::Codex, UnknownWhy::NoThreadMap), (Vendor::Claude, UnknownWhy::Unregistered)] {
        c.vendor = vendor;
        let d = derive(&ev(alive_p()), &c);
        assert_eq!((d.state, d.rule), (ActivityState::StatusUnknown { why }, "R11"), "{vendor:?}");
    }
    c.vendor = Vendor::Claude;
    let mut e = ev(alive_p());
    e.registry = Some(RegistryFacts { status: Some(RegStatus::Busy), waiting_for: None, status_updated_at: Some(NOW), peer_protocol_ok: false });
    // An unknown peer protocol with no other source.
    let d = derive(&e, &c);
    assert!(matches!(d.state, ActivityState::Ready | ActivityState::StatusUnknown { .. }));
    // Codex with a mapped rollout is not unknown.
    c.vendor = Vendor::Codex;
    let mut e = ev(alive_p());
    e.transcript = Some(tr(NOW - 1_000, Some(NOW - 2_000), Some(NOW - 1_000)));
    assert!(!matches!(st(&e, &c), ActivityState::StatusUnknown { .. }));
}

// ---------------------------------------------------------------------------
// staleness (T-derive-stale) and the hosted-lead rule
// ---------------------------------------------------------------------------

#[test]
fn t_derive_stale_rows() {
    let c = cx(ControlKind::Watching);
    // registry behind the rows → the next source down decides (S3: turn ended).
    let mut e = ev(alive_p());
    e.registry = Some(reg(RegStatus::Busy, NOW - 100_000, None));
    e.transcript = Some(tr(NOW - 10, Some(NOW - 50), Some(NOW - 10)));
    e.marks.last_seen_at = Some(NOW);
    assert_eq!(st(&e, &c), ActivityState::Ready);
    // peer_protocol_ok = false is stale.
    let mut e = ev(alive_p());
    e.registry = Some(RegistryFacts { status: Some(RegStatus::Waiting), waiting_for: None, status_updated_at: Some(NOW), peer_protocol_ok: false });
    e.transcript = Some(tr(NOW - 10, Some(NOW - 50), Some(NOW - 10)));
    assert_eq!(st(&e, &c), ActivityState::Ready);
    // A low-confidence PTY decides but is Likely.
    let mut e = ev(alive_p());
    e.gilde = Some(ready_gilde(ControlMode::Terminal));
    e.pty = Some(PtyFacts { status: PtyStatus::Idle, confidence: 0.5, last_output_at: None, prompt: None });
    let d = derive(&e, &cx(ControlKind::InGilde(ControlMode::Terminal)));
    assert_eq!((d.state, d.confidence), (ActivityState::Ready, Confidence::Likely));
    // permissionDenied: last facts kept, Likely, the evidence line, never Ended.
    let mut e = ev(alive_p());
    e.registry = Some(reg(RegStatus::Busy, NOW, None));
    e.source_denied = true;
    let d = derive(&e, &c);
    assert_eq!(d.state, ActivityState::Working);
    assert_eq!(d.confidence, Confidence::Likely);
    assert_eq!(d.evidence.unwrap().key, "evidence.cantRead");
    // permission_request dropped after 10 min.
    let mut e = ev(alive_p());
    e.team = Some(TeamFileFacts { is_active: None, in_roster: true, permission_request: Some((NOW - 700_000, "Bash".into())), last_idle: None });
    let mut c = cx(ControlKind::Watching);
    c.position = TeamPosition::Teammate;
    c.stamps.permission_request_up = Some(NOW_UP - 700_000);
    assert_ne!(st(&e, &c), ActivityState::NeedsApproval);
}

#[test]
fn t_derive_hosted_lead_busy() {
    // A hosted lead with live pane teammates: registry busy until exit (LP-20).
    let mut e = ev(alive_p());
    e.gilde = Some(ready_gilde(ControlMode::Terminal));
    e.registry = Some(reg(RegStatus::Busy, NOW - 3_600_000, None));
    e.pty = Some(PtyFacts { status: PtyStatus::Idle, confidence: 0.9, last_output_at: Some(NOW - 3_000_000), prompt: None });
    e.transcript = Some(tr(NOW - 3_000_000, Some(NOW - 3_100_000), Some(NOW - 3_000_000)));
    e.marks.last_seen_at = Some(NOW);
    let mut c = cx(ControlKind::InGilde(ControlMode::Terminal));
    c.position = TeamPosition::LeadWithLivePanes;
    c.stamps.row_up = Some(NOW_UP - 3_000_000);
    c.stamps.output_up = Some(NOW_UP - 3_000_000);
    assert_eq!(st(&e, &c), ActivityState::Ready, "idle at its prompt, never Working/No progress");
    // `waiting` + `waitingFor` stay reliable.
    e.registry = Some(reg(RegStatus::Waiting, NOW - 1, Some("permission prompt")));
    assert_eq!(st(&e, &c), ActivityState::NeedsApproval);
    // The 600 s busy rule does not apply; the lead works from its PTY and is
    // exempt from No progress while members work.
    e.registry = Some(reg(RegStatus::Busy, NOW - 1, None));
    e.pty = Some(PtyFacts { status: PtyStatus::Working, confidence: 0.9, last_output_at: Some(NOW - 900_000), prompt: None });
    c.stamps.output_up = Some(NOW_UP - 900_000);
    c.stamps.row_up = Some(NOW_UP - 900_000);
    e.lead_has_working_members = true;
    assert_eq!(st(&e, &c), ActivityState::Working);
    e.lead_has_working_members = false;
    assert!(matches!(st(&e, &c), ActivityState::NoProgress { .. }));
    // The same lead without live panes uses the registry normally.
    c.position = TeamPosition::Lead;
    e.pty = Some(PtyFacts { status: PtyStatus::Idle, confidence: 0.9, last_output_at: Some(NOW - 1), prompt: None });
    c.stamps.row_up = Some(NOW_UP - 1);
    e.transcript = Some(tr(NOW - 1, Some(NOW - 100), None));
    assert_eq!(st(&e, &c), ActivityState::Working);
}

#[test]
fn t_team_esc_turn_is_ready_not_working() {
    // An Esc-interrupted pane turn: isActive stays true, no idle_notification;
    // the member transcript's interrupt marker is newer than config.json.
    let mut e = ev(alive_p());
    e.team = Some(team_facts(Some(true)));
    e.transcript = Some(tr(NOW - 1_000, Some(NOW - 60_000), Some(NOW - 1_000)));
    e.marks.last_seen_at = Some(NOW);
    for (control, pos) in [
        (ControlKind::Watching, TeamPosition::Teammate),
        (ControlKind::InGilde(ControlMode::Terminal), TeamPosition::HostedTeammate),
    ] {
        let mut c = cx(control);
        c.position = pos;
        c.stamps.config_mtime = Some(NOW - 50_000);
        c.stamps.row_up = Some(NOW_UP - 1_000);
        assert_eq!(st(&e, &c), ActivityState::Ready, "{pos:?}");
    }
    // Hosted: the quiet rule (no row for 30 s while the pane PTY is Idle).
    let mut e = ev(alive_p());
    e.team = Some(team_facts(Some(true)));
    e.pty = Some(PtyFacts { status: PtyStatus::Idle, confidence: 0.9, last_output_at: None, prompt: None });
    e.transcript = Some(tr(NOW - 40_000, Some(NOW - 50_000), None));
    let mut c = cx(ControlKind::InGilde(ControlMode::Terminal));
    c.position = TeamPosition::HostedTeammate;
    c.stamps.config_mtime = Some(NOW - 10_000);
    c.stamps.row_up = Some(NOW_UP - 40_000);
    assert_eq!(st(&e, &c), ActivityState::Ready);
}

// ---------------------------------------------------------------------------
// through World: transitions (§3.5.5), entry/exit (§3.5.6), sleep, episodes
// ---------------------------------------------------------------------------

#[test]
fn t_derive_sleep_does_not_fire_timers() {
    let rig = Rig::new();
    rig.subscribe();
    let h = claude_hint(Some(&sid(1)), Some(4001));
    rig.ev(&h, alive(4001));
    let now = rig.now();
    rig.ev(&h, registry(RegStatus::Busy, now, None));
    rig.ev(&h, SourceFacts::Transcript(transcript(now, Some(now - 1_000), None)));
    rig.pump();
    let uid = rig.uid_of(&sid(1));
    assert_eq!(rig.state(&uid), ActivityState::Working);
    // Eight hours of sleep: wall moves, uptime does not (PM-36).
    rig.clock.sleep(8 * 3_600_000);
    rig.pump();
    assert_eq!(rig.state(&uid), ActivityState::Working);
    // Awake time does count.
    rig.advance(301_000);
    rig.pump();
    assert!(matches!(rig.state(&uid), ActivityState::NoProgress { .. }));
}

#[test]
fn transitions_of_a_gilde_terminal_session() {
    let rig = Rig::new();
    rig.subscribe();
    let uid = rig.world.bind_spawn("sp-term", None, Vendor::Claude, Some(account()));
    let h = spawn_hint("sp-term", Some(&sid(2)), Some(4002));
    let t = rig.now();
    // (none) → Starting.
    rig.ev(&h, SourceFacts::Gilde(gilde(t, ControlMode::Terminal, None)));
    rig.pump();
    assert_eq!(rig.state(&uid), ActivityState::Starting);
    assert!(matches!(rig.session(&uid).control, Control::InGilde { mode: ControlMode::Terminal, .. }));
    // Starting → Working (registry busy for the pid is a ready signal).
    rig.ev(&h, alive(4002));
    rig.advance(2_000);
    let t = rig.now();
    rig.ev(&h, registry(RegStatus::Busy, t, None));
    rig.ev(&h, SourceFacts::Transcript(transcript(t, Some(t - 100), None)));
    rig.pump();
    assert_eq!(rig.state(&uid), ActivityState::Working);
    // No timeout once ready, even after 30 s.
    rig.advance(31_000);
    rig.pump();
    assert_ne!(rig.state(&uid), ActivityState::Error { sub: ErrorSub::DidntStart { reason: DidntStartReason::Timeout } });
    // Working → NeedsApproval (permission prompt on screen) → Working.
    let t = rig.now();
    rig.ev(&h, SourceFacts::Pty(PtyFacts { status: PtyStatus::WaitingForInput, confidence: 0.9, last_output_at: Some(t), prompt: Some(prompt("ffffffffffffffff", true, None, t)) }));
    rig.ev(&h, registry(RegStatus::Waiting, t, Some("permission prompt")));
    rig.pump();
    assert_eq!(rig.state(&uid), ActivityState::NeedsApproval);
    let s = rig.session(&uid);
    assert_eq!(s.pending.len(), 1);
    assert_eq!(s.pending[0].kind, InteractionKind::ScreenPrompt);
    assert_eq!(s.pending[0].request_id.as_str(), "sp_ffffffffffffffff");
    rig.advance(1_000);
    let t = rig.now();
    rig.ev(&h, SourceFacts::Pty(pty(PtyStatus::Working, t)));
    rig.ev(&h, registry(RegStatus::Busy, t, None));
    rig.ev(&h, SourceFacts::Transcript(transcript(t, Some(t - 5_000), None)));
    rig.pump();
    // The card disappears only after 300 ms without the grid.
    rig.advance(400);
    rig.pump();
    assert_eq!(rig.state(&uid), ActivityState::Working);
    assert!(rig.session(&uid).pending.is_empty());
    // Working → NewReply → Ready (seen).
    rig.advance(1_000);
    let t = rig.now();
    rig.ev(&h, registry(RegStatus::Idle, t, None));
    rig.ev(&h, SourceFacts::Transcript(transcript(t, Some(t - 5_000), Some(t))));
    rig.pump();
    assert_eq!(rig.state(&uid), ActivityState::NewReply);
    rig.advance(10);
    rig.world.mark_seen(&uid).unwrap();
    rig.pump();
    assert_eq!(rig.state(&uid), ActivityState::Ready);
    // any live → Crashed (a non-zero exit Gilde did not cause), Crashed → Ended.
    rig.advance(10);
    let t = rig.now();
    rig.ev(&h, exited(t, Some(1), None));
    rig.pump();
    assert_eq!(rig.state(&uid), ActivityState::Crashed { exit: ExitInfo { code: Some(1), signal: None } });
    assert!(matches!(rig.session(&uid).control, Control::NotRunning { .. }));
    rig.advance(10);
    rig.world.dismiss(&uid, DismissWhat::Crash).unwrap();
    rig.pump();
    assert_eq!(rig.state(&uid), ActivityState::Ended);
    assert!(rig.world.dismiss(&uid, DismissWhat::Crash).is_err(), "REFUSED{{notInState}}");
    // Ended → Starting (Resume keeps the uid).
    rig.advance(1_000);
    let t = rig.now();
    rig.ev(&spawn_hint("sp-term", Some(&sid(2)), None), SourceFacts::Gilde(gilde(t, ControlMode::Terminal, None)));
    rig.pump();
    assert_eq!(rig.state(&uid), ActivityState::Starting);
}

#[test]
fn starting_times_out_on_uptime_and_errors_dismiss_to_ready() {
    let rig = Rig::new();
    rig.subscribe();
    let uid = rig.world.bind_spawn("sp-to", None, Vendor::Claude, Some(account()));
    let h = spawn_hint("sp-to", None, Some(4100));
    rig.ev(&h, SourceFacts::Gilde(gilde(rig.now(), ControlMode::Conversation, None)));
    rig.ev(&h, alive(4100));
    rig.pump();
    rig.advance(29_000);
    rig.pump();
    assert_eq!(rig.state(&uid), ActivityState::Starting);
    rig.advance(2_000);
    rig.pump();
    assert_eq!(rig.state(&uid), ActivityState::Error { sub: ErrorSub::DidntStart { reason: DidntStartReason::Timeout } });
    // Error · Agent error of a running Conversation session → dismissed → Ready.
    let uid2 = rig.world.bind_spawn("sp-err", None, Vendor::Claude, Some(account()));
    let h2 = spawn_hint("sp-err", None, Some(4101));
    let t = rig.now();
    rig.ev(&h2, SourceFacts::Gilde(gilde(t, ControlMode::Conversation, Some(t))));
    rig.ev(&h2, alive(4101));
    rig.ev(&h2, SourceFacts::Stream(StreamFacts { phase: TurnPhase::Idle, phase_since: t, pending: vec![], last_turn_start_at: Some(t - 10), last_turn_end: Some(TurnEnd { at: t, outcome: TurnOutcome::Error(ErrorSub::AgentError) }), last_frame_at: t }));
    rig.pump();
    assert_eq!(rig.state(&uid2), ActivityState::Error { sub: ErrorSub::AgentError });
    rig.advance(10);
    rig.world.mark_seen(&uid2).unwrap();
    rig.world.dismiss(&uid2, DismissWhat::Error).unwrap();
    rig.pump();
    assert_eq!(rig.state(&uid2), ActivityState::Ready);
}

#[test]
fn episodes_keep_through_approval_to_answer_and_restart_on_a_new_kind() {
    let rig = Rig::new();
    rig.subscribe();
    let uid = rig.world.bind_spawn("sp-ep", None, Vendor::Claude, Some(account()));
    let h = spawn_hint("sp-ep", None, Some(4200));
    let t = rig.now();
    rig.ev(&h, SourceFacts::Gilde(gilde(t, ControlMode::Conversation, Some(t))));
    rig.ev(&h, alive(4200));
    let mk = |kind, at| SourceFacts::Stream(StreamFacts { phase: TurnPhase::RequiresAction, phase_since: at, pending: vec![pending(kind, at)], last_turn_start_at: Some(t), last_turn_end: None, last_frame_at: at });
    rig.ev(&h, mk(InteractionKind::ToolApproval, t + 1));
    rig.pump();
    let a = rig.session(&uid).activity;
    assert_eq!(a.state, ActivityState::NeedsApproval);
    rig.advance(500);
    rig.ev(&h, mk(InteractionKind::Question, t + 400));
    rig.pump();
    let b = rig.session(&uid).activity;
    assert_eq!(b.state, ActivityState::NeedsAnswer);
    assert_eq!(a.episode, b.episode, "same wait, same notification key");
    rig.advance(500);
    let t2 = rig.now();
    rig.ev(&h, SourceFacts::Stream(StreamFacts { phase: TurnPhase::Running, phase_since: t2, pending: vec![], last_turn_start_at: Some(t), last_turn_end: None, last_frame_at: t2 }));
    rig.pump();
    let c = rig.session(&uid).activity;
    assert_eq!(c.state, ActivityState::Working);
    assert_ne!(c.episode, b.episode);
}

#[test]
fn not_running_invariant_over_exit_combinations() {
    // [F-14, F-21]: Ended/Stopped/Crashed ⇒ NotRunning; NotRunning ⇒ one of
    // Ended, Stopped, Crashed, Didn't start, Sign-in needed, Usage limit.
    let rig = Rig::new();
    rig.subscribe();
    let exits = [
        ProcessFacts::Exited { at: 0, code: Some(0), signal: None },
        ProcessFacts::Exited { at: 0, code: Some(1), signal: None },
        ProcessFacts::Exited { at: 0, code: Some(143), signal: None },
        ProcessFacts::Exited { at: 0, code: None, signal: Some(9) },
        ProcessFacts::Gone { at: 0 },
    ];
    let mut n = 0u32;
    for exit in exits {
        for ready in [false, true] {
            for stop in [None, Some(StopCause::User)] {
                for last_turn in [None, Some(ErrorSub::SignInNeeded), Some(ErrorSub::AgentError)] {
                    n += 1;
                    let spawn = format!("sp-inv-{n}");
                    let uid = rig.world.bind_spawn(&spawn, None, Vendor::Claude, Some(account()));
                    let h = spawn_hint(&spawn, None, Some(5000 + n));
                    let t = rig.now();
                    let mut g = gilde(t - 1_000, ControlMode::Conversation, ready.then_some(t - 900));
                    g.stop_cause = stop;
                    rig.ev(&h, SourceFacts::Gilde(g));
                    rig.ev(&h, alive(5000 + n as i32));
                    if let Some(sub) = &last_turn {
                        rig.ev(&h, SourceFacts::Stream(StreamFacts { phase: TurnPhase::Idle, phase_since: t, pending: vec![], last_turn_start_at: Some(t - 500), last_turn_end: Some(TurnEnd { at: t - 100, outcome: TurnOutcome::Error(sub.clone()) }), last_frame_at: t }));
                    }
                    let exit_at = match exit {
                        ProcessFacts::Exited { code, signal, .. } => ProcessFacts::Exited { at: t, code, signal },
                        _ => ProcessFacts::Gone { at: t },
                    };
                    rig.ev(&h, SourceFacts::Process(exit_at));
                    rig.pump();
                    let s = rig.session(&uid);
                    let not_running = matches!(s.control, Control::NotRunning { .. });
                    let allowed = matches!(
                        s.activity.state,
                        ActivityState::Ended
                            | ActivityState::Stopped
                            | ActivityState::Crashed { .. }
                            | ActivityState::Error { sub: ErrorSub::DidntStart { .. } | ErrorSub::SignInNeeded | ErrorSub::UsageLimit { .. } }
                    );
                    assert!(not_running, "dead process is NotRunning: {exit:?} ready={ready} stop={stop:?}");
                    assert!(allowed, "NotRunning state {:?} for {exit:?} ready={ready} stop={stop:?} last={last_turn:?}", s.activity.state);
                    assert!(s.pending.is_empty(), "pending cleared on exit");
                }
            }
        }
    }
}

#[test]
fn discovered_sessions_never_start_and_history_rows_are_ended() {
    let rig = Rig::new();
    rig.subscribe();
    // (new, observed) → Working/Ready/…, never Starting.
    let h = claude_hint(Some(&sid(0x71)), Some(4300));
    rig.ev(&h, alive(4300));
    rig.ev(&h, registry(RegStatus::Busy, rig.now(), None));
    rig.pump();
    let uid = rig.uid_of(&sid(0x71));
    assert_eq!(rig.state(&uid), ActivityState::Working);
    // (new, history) → Ended.
    let hh = claude_hint(Some(&sid(0x72)), None);
    rig.ev(&hh, SourceFacts::Process(ProcessFacts::HistoryOnly));
    rig.ev(&hh, SourceFacts::Transcript(transcript(rig.now() - 3_600_000, Some(rig.now() - 3_700_000), Some(rig.now() - 3_600_000))));
    rig.pump();
    let hist = rig.uid_of(&sid(0x72));
    let s = rig.session(&hist);
    assert_eq!(s.activity.state, ActivityState::Ended);
    assert!(matches!(s.control, Control::NotRunning { ended_how: gilde_code_lib::contract::session::EndedHow::HistoryOnly, .. }));
    // StatusUnknown → (any): a process-only Claude gains a registry row.
    let p = claude_hint(None, Some(4301));
    rig.ev(&p, alive(4301));
    rig.pump();
    let pu = rig.world.snapshot().sessions.into_iter().find(|s| s.vendor_session_id.is_none()).unwrap().uid;
    assert!(matches!(rig.state(&pu), ActivityState::StatusUnknown { .. }));
    rig.ev(&claude_hint(Some(&sid(0x73)), Some(4301)), registry(RegStatus::Idle, rig.now(), None));
    rig.pump();
    assert_eq!(rig.state(&pu), ActivityState::Ready);
}

#[test]
fn starting_to_ready_without_a_first_prompt() {
    let rig = Rig::new();
    rig.subscribe();
    let uid = rig.world.bind_spawn("sp-empty", None, Vendor::Claude, Some(account()));
    let h = spawn_hint("sp-empty", None, Some(4400));
    let t = rig.now();
    rig.ev(&h, SourceFacts::Gilde(gilde(t, ControlMode::Conversation, None)));
    rig.ev(&h, alive(4400));
    rig.pump();
    assert_eq!(rig.state(&uid), ActivityState::Starting);
    rig.ev(&h, SourceFacts::Gilde(gilde(t, ControlMode::Conversation, Some(t + 500))));
    rig.ev(&h, SourceFacts::Stream(StreamFacts { phase: TurnPhase::Idle, phase_since: t + 500, pending: vec![], last_turn_start_at: None, last_turn_end: None, last_frame_at: t + 500 }));
    rig.pump();
    assert_eq!(rig.state(&uid), ActivityState::Ready);
    let caps = rig.session(&uid).caps;
    assert_eq!(caps.send, gilde_code_lib::contract::session::SendCap::Conversation);
    assert!(caps.force_stop && caps.set_model);
    assert!(!caps.set_permission_mode.contains(&gilde_code_lib::contract::common::PermissionMode::BypassPermissions));
}

#[test]
fn caps_follow_control_and_state() {
    use gilde_code_lib::contract::session::{ApproveCap, ContinueOption, StopCap};
    use gilde_code_lib::world::derive::{caps, CapsInput};
    let watched_bg = caps(
        ControlKind::Watching,
        Vendor::Claude,
        &ActivityState::Working,
        &CapsInput { vendor_session_id_known: true, origin: Some(gilde_code_lib::contract::session::Origin::Background), has_owner: true, ..CapsInput::default() },
    );
    assert_eq!(watched_bg.continue_options, vec![ContinueOption::Attach, ContinueOption::Fork]);
    assert_eq!((watched_bg.approve, watched_bg.stop_session, watched_bg.owner_focus), (ApproveCap::GoToOwner, StopCap::ClaudeStop, true));
    let ended = caps(ControlKind::NotRunning, Vendor::Claude, &ActivityState::Ended, &CapsInput { vendor_session_id_known: true, ..CapsInput::default() });
    assert_eq!(ended.continue_options, vec![ContinueOption::Resume, ContinueOption::Fork]);
    assert_eq!(ended.send, gilde_code_lib::contract::session::SendCap::None);
    let compat_failed = caps(
        ControlKind::NotRunning,
        Vendor::Claude,
        &ActivityState::Error { sub: ErrorSub::DidntStart { reason: DidntStartReason::CompatFailed } },
        &CapsInput { gilde_spawned: true, ..CapsInput::default() },
    );
    assert_eq!(compat_failed.continue_options, vec![ContinueOption::RetryAsTerminal]);
    let cursor = caps(ControlKind::NotRunning, Vendor::Cursor, &ActivityState::Ended, &CapsInput { vendor_session_id_known: true, ..CapsInput::default() });
    assert_eq!(cursor.continue_options, vec![ContinueOption::Resume]);
    let term = caps(ControlKind::InGilde(ControlMode::Terminal), Vendor::Claude, &ActivityState::Working, &CapsInput { pty_known: true, ..CapsInput::default() });
    assert!(term.terminal_view && term.interrupt && !term.force_stop);
    assert_eq!(term.approve, ApproveCap::ScreenKeys);
}

#[test]
fn stopping_a_turn_based_session_between_turns_is_stopped() {
    let rig = Rig::new();
    rig.subscribe();
    let uid = rig.world.bind_spawn("sp-tb", None, Vendor::Codex, None);
    let h = IdentityHint { vendor: Some(Vendor::Codex), account_id: None, vendor_session_id: None, process: None, gilde_spawn_id: Some("sp-tb".into()) };
    let t = rig.now();
    rig.ev(&h, SourceFacts::Gilde(gilde(t, ControlMode::TurnBased, Some(t))));
    rig.ev(&h, SourceFacts::Stream(StreamFacts { phase: TurnPhase::Idle, phase_since: t, pending: vec![], last_turn_start_at: None, last_turn_end: None, last_frame_at: t }));
    rig.pump();
    assert_eq!(rig.state(&uid), ActivityState::Ready, "held between turns");
    assert!(matches!(rig.session(&uid).control, Control::InGilde { mode: ControlMode::TurnBased, .. }));
    let mut g = gilde(t, ControlMode::TurnBased, Some(t));
    g.stop_cause = Some(StopCause::User);
    rig.ev(&h, SourceFacts::Gilde(g));
    rig.pump();
    assert_eq!(rig.state(&uid), ActivityState::Stopped);
    assert!(matches!(rig.session(&uid).control, Control::NotRunning { .. }));
    // A Conversation spawn stopped before its process reported is Stopped too.
    let c = rig.world.bind_spawn("sp-early", None, Vendor::Claude, Some(account()));
    let hc = spawn_hint("sp-early", None, None);
    let mut g = gilde(rig.now(), ControlMode::Conversation, None);
    rig.ev(&hc, SourceFacts::Gilde(g.clone()));
    rig.pump();
    assert_eq!(rig.state(&c), ActivityState::Starting);
    g.stop_cause = Some(StopCause::User);
    rig.ev(&hc, SourceFacts::Gilde(g));
    rig.pump();
    assert_eq!(rig.state(&c), ActivityState::Stopped);
}

/// A35.2 (G7-03) in World's own card builder: a screen card shows the prompt
/// box's header — what it approves — through the helper shared with T06's
/// builder (decoration rows left out); a permission card whose header could
/// not be captured invents no text and keeps ⌘⏎ inert.
#[test]
fn world_screen_cards_show_what_they_approve() {
    let rig = Rig::new();
    rig.subscribe();
    let uid = rig.world.bind_spawn("sp-t36", None, Vendor::Claude, Some(account()));
    let h = spawn_hint("sp-t36", Some(&sid(3)), Some(4003));
    let t = rig.now();
    rig.ev(&h, SourceFacts::Gilde(gilde(t, ControlMode::Terminal, None)));
    rig.ev(&h, alive(4003));
    rig.advance(2_000);
    let card_for = |rev: &str, header: Option<&str>| {
        let t = rig.now();
        let p = prompt(rev, true, header, t);
        rig.ev(&h, SourceFacts::Pty(PtyFacts { status: PtyStatus::WaitingForInput, confidence: 0.9, last_output_at: Some(t), prompt: Some(p) }));
        rig.ev(&h, registry(RegStatus::Waiting, t, Some("permission prompt")));
        rig.pump();
        rig.advance(400);
        rig.pump();
        rig.session(&uid).pending.first().cloned().expect("a screen card")
    };

    let card = card_for("abababababababab", Some(" Bash command\n   rm -rf ~/Documents\n╌╌╌╌╌╌╌╌╌╌\n"));
    assert_eq!(card.request_id.as_str(), "sp_abababababababab");
    let texts: Vec<&str> = card.detail_lines.iter().map(|l| l.params["text"].as_str().unwrap()).collect();
    assert_eq!(texts, vec!["Bash command", "rm -rf ~/Documents"]);
    assert!(card.detail_lines.iter().all(|l| l.key == "card.detailText"));
    assert!(!card.default_to_no, "the command is shown: ⌘⏎ may stay live");

    rig.advance(1_000);
    let blind = card_for("cdcdcdcdcdcdcdcd", None);
    assert_eq!(blind.request_id.as_str(), "sp_cdcdcdcdcdcdcdcd");
    assert!(blind.detail_lines.is_empty(), "no invented text");
    assert!(blind.default_to_no, "⌘⏎ inert");
}
