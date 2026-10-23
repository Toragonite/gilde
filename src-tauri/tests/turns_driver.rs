//! T08: the turn-based driver (contract §6.3) against a fake sidecar, a
//! recording evidence sink and a recording content hub. No vendor process is
//! ever started: the fake answers `turn.start` and the test emits the turn's
//! events itself.
//!
//! Covers the held message (one, `CONFLICT{queueFull}`, starts on `Done`, stays
//! on `Error`), resume ids (the conversation reported by the vendor, `""` = not
//! resumable), the driver half of **T-turns-newConversation** (a `Session` id
//! that differs from the `resumeId` becomes the session's conversation and the
//! identity hint's vendor id), the codex resume model rule, error mapping,
//! interrupt / stop, unsupported operations and idempotent sends.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;

use gilde_code_lib::contract::account::Quota;
use gilde_code_lib::contract::activity::{
    DidntStartReason, ErrorSub, ProcessFacts, StopCause, TurnOutcome,
};
use gilde_code_lib::contract::common::{AbsPath, PermissionMode, Vendor};
use gilde_code_lib::contract::content::BlockPatch;
use gilde_code_lib::contract::error::{ErrorCode, IpcError, VendorErrorClass};
use gilde_code_lib::contract::ids::{AccountId, BlockId, ClientMsgId, SessionUid};
use gilde_code_lib::contract::ipc::{AppliesFrom, InterruptOutcome, SendDelivery, SendPriority};
use gilde_code_lib::contract::services::{
    ContentHubApi, ContentUpdate, EvidenceSink, EvidenceUpdate, IdentityHint, OutgoingMessage,
    SessionDriver, SidecarApi, SourceFacts, TurnEventSink, UsageSource,
};
use gilde_code_lib::contract::session::UsageToday;
use gilde_code_lib::contract::session::ControlMode;
use gilde_code_lib::contract::sidecar::{
    AuthStatus, ModelInfo, ProbeParams, TurnDoneStatus, TurnEvent, TurnStartParams, TurnUsage,
};
use gilde_code_lib::sidecar::HELPER_RESTARTED;
use gilde_code_lib::turns::driver::{
    TurnDeps, TurnDriver, TurnRenderer, TurnRendererFactory, TurnSessionSpec,
};

// ---------------------------------------------------------------------------
// fakes
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FakeSidecar {
    starts: Mutex<Vec<TurnStartParams>>,
    sinks: Mutex<Vec<Arc<dyn TurnEventSink>>>,
    cancels: Mutex<Vec<String>>,
    fail_next_start: Mutex<Option<IpcError>>,
    /// Events emitted from inside `turn.start`, before it answers.
    emit_on_start: Mutex<Vec<TurnEvent>>,
    models: Mutex<Vec<ModelInfo>>,
    model_calls: AtomicUsize,
}

impl FakeSidecar {
    fn started(&self) -> Vec<TurnStartParams> {
        self.starts.lock().clone()
    }

    fn turn(&self, i: usize) -> TurnStartParams {
        self.starts.lock()[i].clone()
    }

    /// Emit `event` on the `i`-th started turn.
    fn emit(&self, i: usize, seq: u64, event: TurnEvent) {
        let sink = self.sinks.lock()[i].clone();
        sink.event(seq, event);
    }
}

#[async_trait]
impl SidecarApi for FakeSidecar {
    async fn ensure_started(&self) -> Result<(), IpcError> {
        Ok(())
    }
    async fn auth_status(&self, _p: ProbeParams) -> Result<AuthStatus, IpcError> {
        Err(IpcError::unsupported("authStatus"))
    }
    async fn models(&self, _p: ProbeParams) -> Result<Vec<ModelInfo>, IpcError> {
        self.model_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.models.lock().clone())
    }
    async fn quota(&self, _p: ProbeParams) -> Result<Option<Quota>, IpcError> {
        Ok(None)
    }
    async fn turn_start(&self, p: TurnStartParams, sink: Arc<dyn TurnEventSink>) -> Result<(), IpcError> {
        if let Some(err) = self.fail_next_start.lock().take() {
            return Err(err);
        }
        let early: Vec<TurnEvent> = std::mem::take(&mut *self.emit_on_start.lock());
        for (i, event) in early.into_iter().enumerate() {
            sink.event(i as u64 + 1, event);
        }
        self.starts.lock().push(p);
        self.sinks.lock().push(sink);
        Ok(())
    }
    async fn turn_cancel(&self, turn_id: &str) -> Result<bool, IpcError> {
        self.cancels.lock().push(turn_id.to_string());
        Ok(true)
    }
}

#[derive(Default)]
struct Evidence(Mutex<Vec<EvidenceUpdate>>);

impl EvidenceSink for Evidence {
    fn evidence(&self, update: EvidenceUpdate) {
        self.0.lock().push(update);
    }
    fn location(&self, _facts: gilde_code_lib::contract::services::LocationFacts) {}
    fn source_status(&self, _status: gilde_code_lib::contract::store::SourceStatus) {}
    fn account(&self, _account: gilde_code_lib::contract::account::Account) {}
}

impl Evidence {
    fn usage_reports(&self) -> Vec<UsageToday> {
        self.0
            .lock()
            .iter()
            .filter_map(|u| match &u.facts {
                SourceFacts::Usage(f) => {
                    assert_eq!(f.source, UsageSource::Turns);
                    Some(f.today.clone())
                }
                _ => None,
            })
            .collect()
    }

    fn last_turn_end(&self) -> Option<TurnOutcome> {
        self.0.lock().iter().rev().find_map(|u| match &u.facts {
            SourceFacts::Stream(s) => Some(s.last_turn_end.as_ref().map(|e| e.outcome.clone())),
            _ => None,
        })?
    }

    fn last_hint(&self) -> IdentityHint {
        self.0.lock().last().expect("evidence").hint.clone()
    }
}

/// Records the published block ids in order.
#[derive(Default)]
struct Content(Mutex<Vec<String>>);

impl ContentHubApi for Content {
    fn publish(&self, _uid: &SessionUid, update: ContentUpdate) {
        if let ContentUpdate::Updated { block_id, .. } = update {
            self.0.lock().push(block_id.as_str().to_string());
        }
    }
    fn has_subscribers(&self, _uid: &SessionUid) -> bool {
        true
    }
}

/// Renders the prompt as `prompt:<text>` and each event as `event:<kind>` —
/// markers only, so the test can check the publication order.
struct Markers;

struct MarkerRenderer;

fn marker(tag: String) -> ContentUpdate {
    ContentUpdate::Updated {
        block_id: BlockId::parse(tag).expect("block id"),
        patch: BlockPatch::TextDelta { append: String::new() },
    }
}

impl TurnRenderer for MarkerRenderer {
    fn user_prompt(&mut self, text: &str, _id: Option<ClientMsgId>, _at: i64) -> Vec<ContentUpdate> {
        vec![marker(format!("prompt:{text}"))]
    }
    fn on_event(&mut self, event: &TurnEvent, _at: i64) -> Vec<ContentUpdate> {
        let kind = match event {
            TurnEvent::Text { .. } => "text",
            TurnEvent::Tool { .. } => "tool",
            TurnEvent::Usage { .. } => "usage",
            TurnEvent::Session { .. } => "session",
            TurnEvent::Done { .. } => "done",
            TurnEvent::Error { .. } => "error",
        };
        vec![marker(format!("event:{kind}"))]
    }
}

impl TurnRendererFactory for Markers {
    fn for_turn(&self, _turn_id: &str) -> Box<dyn TurnRenderer> {
        Box::new(MarkerRenderer)
    }
}

// ---------------------------------------------------------------------------
// rig
// ---------------------------------------------------------------------------

struct Rig {
    driver: TurnDriver,
    sidecar: Arc<FakeSidecar>,
    evidence: Arc<Evidence>,
    content: Arc<Content>,
    account: AccountId,
    /// The local day the driver sees (pinned).
    day: Arc<Mutex<String>>,
}

fn spec(vendor: Vendor, resume_id: Option<&str>, model: Option<&str>) -> TurnSessionSpec {
    TurnSessionSpec {
        uid: SessionUid::mint(),
        vendor,
        account_id: AccountId::derive_default_login(vendor),
        cwd: AbsPath::parse("/tmp/gilde-turns-fixture").expect("abs path"),
        model: model.map(str::to_string),
        permission_mode: Some(PermissionMode::AcceptEdits),
        resume_id: resume_id.map(str::to_string),
        hint: IdentityHint::default(),
        timeout_ms: Some(600_000),
    }
}

fn rig_with(spec: TurnSessionSpec, sidecar: FakeSidecar) -> Rig {
    let sidecar = Arc::new(sidecar);
    let evidence = Arc::new(Evidence::default());
    let content = Arc::new(Content::default());
    let account = spec.account_id.clone();
    let day = Arc::new(Mutex::new("2026-10-22".to_string()));
    let pinned = day.clone();
    let mut deps = TurnDeps::new(sidecar.clone(), evidence.clone(), content.clone(), Arc::new(Markers));
    deps.day = Arc::new(move |_| pinned.lock().clone());
    Rig {
        driver: TurnDriver::new(spec, deps),
        sidecar,
        evidence,
        content,
        account,
        day,
    }
}

/// A rig whose vendor lists one default model (a codex resume needs one, R2 C6).
fn rig(vendor: Vendor) -> Rig {
    let sidecar = FakeSidecar::default();
    *sidecar.models.lock() = vec![ModelInfo {
        id: "m-default".into(),
        label: None,
        is_default: true,
    }];
    rig_with(spec(vendor, None, None), sidecar)
}

fn msg_id(n: u8) -> ClientMsgId {
    ClientMsgId::parse(format!("1b4e28ba-2fa1-41d2-883f-0016d3cca4{n:02x}")).expect("uuid v4")
}

fn msg(n: u8, text: &str) -> OutgoingMessage {
    OutgoingMessage {
        client_msg_id: msg_id(n),
        text: text.to_string(),
        priority: SendPriority::Next,
    }
}

fn done(status: TurnDoneStatus, conversation_id: &str) -> TurnEvent {
    TurnEvent::Done {
        status,
        conversation_id: conversation_id.to_string(),
        usage: None,
        cost_usd: None,
    }
}

fn session(conversation_id: &str) -> TurnEvent {
    TurnEvent::Session {
        conversation_id: conversation_id.to_string(),
    }
}

fn error(class: VendorErrorClass, message: &str) -> TurnEvent {
    TurnEvent::Error {
        class,
        message: message.to_string(),
    }
}

async fn until(what: &str, f: impl Fn() -> bool) {
    for _ in 0..500 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for: {what}");
}

/// Let the driver's event task drain (for "nothing happens" checks).
async fn settle() {
    tokio::time::sleep(Duration::from_millis(150)).await;
}

fn changed(err: &IpcError) -> Option<&str> {
    err.params.get("changed").and_then(|v| v.as_str())
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_starts_a_turn_with_every_field() {
    let mut r = rig(Vendor::Cursor);
    assert_eq!(r.driver.mode(), ControlMode::TurnBased);
    assert!(!r.driver.is_running(), "held: no process until the first message");

    assert_eq!(r.driver.send(msg(1, "hello")).await.expect("send"), SendDelivery::Sent);
    assert!(r.driver.is_running());
    let p = r.sidecar.turn(0);
    assert_eq!(p.vendor, Vendor::Cursor);
    assert_eq!(p.account_id, r.account);
    assert_eq!(p.cwd.as_str(), "/tmp/gilde-turns-fixture");
    assert_eq!(p.permission_mode, Some(PermissionMode::AcceptEdits));
    assert_eq!(p.prompt, "hello");
    assert_eq!(p.resume_id, None);
    assert_eq!(p.timeout_ms, Some(600_000));
    assert!(p.turn_id.starts_with("tn_"));

    // The Gilde facts say TurnBased; the hint names vendor and account.
    let gilde = r.evidence.0.lock().iter().find_map(|u| match &u.facts {
        SourceFacts::Gilde(g) => Some(g.mode),
        _ => None,
    });
    assert_eq!(gilde, Some(ControlMode::TurnBased));
    let hint = r.evidence.last_hint();
    assert_eq!(hint.vendor, Some(Vendor::Cursor));
    assert_eq!(hint.account_id, Some(r.account.clone()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_message_is_held_while_a_turn_runs_and_a_second_is_queue_full() {
    let mut r = rig(Vendor::Codex);
    r.driver.send(msg(1, "first")).await.expect("first");
    assert_eq!(r.driver.send(msg(2, "second")).await.expect("held"), SendDelivery::Queued);
    let err = r.driver.send(msg(3, "third")).await.expect_err("queue full");
    assert_eq!((err.code, changed(&err)), (ErrorCode::Conflict, Some("queueFull")));
    assert_eq!(r.driver.held_message().map(|q| q.client_msg_id), Some(msg_id(2)));

    // Idempotent by clientMsgId: a repeat returns the first outcome, nothing new.
    assert_eq!(r.driver.send(msg(1, "first")).await.expect("repeat"), SendDelivery::Sent);
    assert_eq!(r.driver.send(msg(2, "second")).await.expect("repeat"), SendDelivery::Queued);
    assert_eq!(r.sidecar.started().len(), 1);

    // Cancel: only the held id.
    let err = r.driver.cancel_queued(&msg_id(3)).await.expect_err("not held");
    assert_eq!(err.code, ErrorCode::NotFound);
    assert!(r.driver.cancel_queued(&msg_id(2)).await.expect("cancel"));
    assert_eq!(r.driver.held_message(), None);

    // Nothing starts when the turn ends.
    r.sidecar.emit(0, 1, done(TurnDoneStatus::Ok, "c-1"));
    until("turn end", || !r.driver.is_running()).await;
    settle().await;
    assert_eq!(r.sidecar.started().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn done_starts_the_held_message_resuming_the_reported_conversation() {
    let mut r = rig(Vendor::Codex);
    r.driver.send(msg(1, "first")).await.expect("first");
    r.sidecar.emit(0, 1, session("conv-1"));
    r.driver.send(msg(2, "second")).await.expect("held");
    r.sidecar.emit(0, 2, done(TurnDoneStatus::Ok, "conv-1"));

    until("the held turn", || r.sidecar.started().len() == 2).await;
    let p = r.sidecar.turn(1);
    assert_eq!(p.prompt, "second");
    assert_eq!(p.resume_id.as_deref(), Some("conv-1"));
    assert_eq!(p.model.as_deref(), Some("m-default"), "a codex resume carries the vendor default");
    assert_eq!(r.driver.held_message(), None);
    assert_eq!(r.driver.conversation_id().as_deref(), Some("conv-1"));
    assert!(r.driver.resumable());
    assert_eq!(r.evidence.last_turn_end(), Some(TurnOutcome::Ok));
    assert_eq!(r.evidence.last_hint().vendor_session_id.as_deref(), Some("conv-1"));

    // The held turn is claimed at once: a message sent now is held behind it.
    assert!(r.driver.is_running());
    assert_eq!(r.driver.send(msg(3, "third")).await.expect("held"), SendDelivery::Queued);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn error_keeps_the_held_message_and_maps_the_error() {
    let mut r = rig(Vendor::Codex);
    r.driver.send(msg(1, "first")).await.expect("first");
    r.sidecar.emit(0, 1, session("conv-1"));
    r.driver.send(msg(2, "second")).await.expect("held");
    r.sidecar.emit(0, 2, error(VendorErrorClass::Quota, "limit"));
    until("turn end", || !r.driver.is_running()).await;
    settle().await;
    assert_eq!(r.sidecar.started().len(), 1, "no turn after an Error");
    assert_eq!(r.driver.held_message().map(|q| q.client_msg_id), Some(msg_id(2)));
    assert_eq!(
        r.evidence.last_turn_end(),
        Some(TurnOutcome::Error(ErrorSub::UsageLimit {
            account_id: Some(r.account.clone()),
            resets_at: None,
        }))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_error_class_maps_to_its_sub_state() {
    async fn outcome_of(before_session: bool, event: TurnEvent) -> Option<TurnOutcome> {
        let mut r = rig(Vendor::Agy);
        r.driver.send(msg(1, "go")).await.expect("send");
        let mut seq = 1;
        if !before_session {
            r.sidecar.emit(0, seq, session("conv"));
            seq += 1;
        }
        r.sidecar.emit(0, seq, event);
        until("turn end", || !r.driver.is_running()).await;
        settle().await;
        r.evidence.last_turn_end()
    }
    assert_eq!(
        outcome_of(false, error(VendorErrorClass::Auth, "x")).await,
        Some(TurnOutcome::Error(ErrorSub::SignInNeeded))
    );
    assert_eq!(
        outcome_of(true, error(VendorErrorClass::Exec, "exited 1")).await,
        Some(TurnOutcome::Error(ErrorSub::DidntStart {
            reason: DidntStartReason::Exited
        })),
        "no Session event ever arrived"
    );
    assert_eq!(
        outcome_of(false, error(VendorErrorClass::Unknown, "x")).await,
        Some(TurnOutcome::Error(ErrorSub::AgentError))
    );
    assert_eq!(
        outcome_of(true, error(VendorErrorClass::Exec, HELPER_RESTARTED)).await,
        Some(TurnOutcome::Error(ErrorSub::AgentError)),
        "the helper restarting is not the agent failing to start"
    );
    assert_eq!(
        outcome_of(false, done(TurnDoneStatus::Stopped, "conv")).await,
        Some(TurnOutcome::Interrupted)
    );
    assert_eq!(
        outcome_of(false, done(TurnDoneStatus::Incomplete, "conv")).await,
        Some(TurnOutcome::Ok)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_turns_new_conversation_driver_half() {
    // Continuing conversation "old"; the vendor answers with "new" (§6.3, PM-23).
    let mut r = rig_with(spec(Vendor::Cursor, Some("old"), None), FakeSidecar::default());
    r.driver.send(msg(1, "continue")).await.expect("send");
    assert_eq!(r.sidecar.turn(0).resume_id.as_deref(), Some("old"));
    r.sidecar.emit(0, 1, session("new"));
    until("the new id", || r.driver.conversation_id().as_deref() == Some("new")).await;
    // World sees the new vendor id on the hint (it forks a new uid with the line).
    until("the hint", || r.evidence.last_hint().vendor_session_id.as_deref() == Some("new")).await;
    r.sidecar.emit(0, 2, done(TurnDoneStatus::Ok, "new"));
    until("turn end", || !r.driver.is_running()).await;

    // Later turns continue the new conversation.
    r.driver.send(msg(2, "next")).await.expect("send");
    assert_eq!(r.sidecar.turn(1).resume_id.as_deref(), Some("new"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_conversation_id_is_not_resumable() {
    let mut r = rig(Vendor::Agy);
    r.driver.send(msg(1, "one")).await.expect("send");
    r.sidecar.emit(0, 1, done(TurnDoneStatus::Ok, ""));
    until("turn end", || !r.driver.is_running()).await;
    assert!(!r.driver.resumable());
    r.driver.send(msg(2, "two")).await.expect("send");
    assert_eq!(r.sidecar.turn(1).resume_id, None, "the next turn starts a new conversation");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_codex_resume_without_a_model_takes_the_vendor_default() {
    let sidecar = FakeSidecar::default();
    *sidecar.models.lock() = vec![
        ModelInfo { id: "m-small".into(), label: None, is_default: false },
        ModelInfo { id: "m-default".into(), label: None, is_default: true },
    ];
    let mut r = rig_with(spec(Vendor::Codex, Some("thread-1"), None), sidecar);
    r.driver.send(msg(1, "resume")).await.expect("send");
    assert_eq!(r.sidecar.turn(0).model.as_deref(), Some("m-default"));
    assert_eq!(r.sidecar.model_calls.load(Ordering::SeqCst), 1);

    // A fresh codex conversation needs no model.
    let mut fresh = rig(Vendor::Codex);
    fresh.driver.send(msg(1, "new")).await.expect("send");
    assert_eq!(fresh.sidecar.turn(0).model, None);
    assert_eq!(fresh.sidecar.model_calls.load(Ordering::SeqCst), 0);

    // No model to be had: the send fails and nothing runs.
    let mut none = rig_with(spec(Vendor::Codex, Some("thread-1"), None), FakeSidecar::default());
    let err = none.driver.send(msg(1, "resume")).await.expect_err("no model");
    assert_eq!(err.code, ErrorCode::InvalidArgument);
    assert!(!none.driver.is_running());
    assert!(none.sidecar.started().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_failure_is_returned_and_nothing_runs() {
    let mut r = rig(Vendor::Cursor);
    *r.sidecar.fail_next_start.lock() = Some(IpcError::sidecar_down());
    let err = r.driver.send(msg(1, "hello")).await.expect_err("down");
    assert_eq!(err.code, ErrorCode::SidecarDown);
    assert!(!r.driver.is_running());
    assert!(r.content.0.lock().is_empty(), "the prompt is not shown for a turn that never started");
    // A failed send is not remembered: the same message can be retried.
    assert_eq!(r.driver.send(msg(1, "hello")).await.expect("retry"), SendDelivery::Sent);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_prompt_precedes_events_that_arrive_before_turn_start_answers() {
    let sidecar = FakeSidecar::default();
    *sidecar.emit_on_start.lock() = vec![session("c"), TurnEvent::Text { delta: "hi".into() }];
    let mut r = rig_with(spec(Vendor::Cursor, None, None), sidecar);
    r.driver.send(msg(1, "ask")).await.expect("send");
    until("the early events", || r.content.0.lock().len() == 3).await;
    assert_eq!(
        *r.content.0.lock(),
        vec!["prompt:ask".to_string(), "event:session".to_string(), "event:text".to_string()]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_cancels_the_turn_and_may_discard_the_held_message() {
    let mut r = rig(Vendor::Codex);
    r.driver.send(msg(1, "first")).await.expect("first");
    r.driver.send(msg(2, "second")).await.expect("held");
    let turn = r.sidecar.turn(0).turn_id;

    assert_eq!(r.driver.interrupt(true).await.expect("interrupt"), InterruptOutcome::Stopped);
    assert_eq!(*r.sidecar.cancels.lock(), vec![turn.clone()]);
    assert_eq!(r.driver.held_message(), None, "discardQueued drops it");
    r.sidecar.emit(0, 1, done(TurnDoneStatus::Stopped, ""));
    until("turn end", || !r.driver.is_running()).await;
    settle().await;
    assert_eq!(r.sidecar.started().len(), 1);
    assert_eq!(r.evidence.last_turn_end(), Some(TurnOutcome::Interrupted));

    // Without discardQueued the held message runs next.
    r.driver.send(msg(3, "third")).await.expect("send");
    r.driver.send(msg(4, "fourth")).await.expect("held");
    r.driver.interrupt(false).await.expect("interrupt");
    assert!(r.driver.held_message().is_some());
    r.sidecar.emit(1, 1, done(TurnDoneStatus::Stopped, ""));
    until("the held turn", || r.sidecar.started().len() == 3).await;
    assert_eq!(r.sidecar.turn(2).prompt, "fourth");

    // Interrupting an idle session signals nothing.
    r.sidecar.emit(2, 1, done(TurnDoneStatus::Ok, "c"));
    until("turn end", || !r.driver.is_running()).await;
    let before = r.sidecar.cancels.lock().len();
    r.driver.interrupt(false).await.expect("idle");
    assert_eq!(r.sidecar.cancels.lock().len(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_cancels_drops_the_held_message_and_refuses_new_turns() {
    let mut r = rig(Vendor::Cursor);
    r.driver.send(msg(1, "first")).await.expect("first");
    r.driver.send(msg(2, "second")).await.expect("held");
    let out = r.driver.stop(StopCause::User).await.expect("stop");
    assert!(out.stopped);
    assert_eq!(out.survivors, 0);
    assert_eq!(r.sidecar.cancels.lock().len(), 1);
    assert_eq!(r.driver.held_message(), None);
    let err = r.driver.send(msg(3, "after")).await.expect_err("stopped");
    assert_eq!(err.code, ErrorCode::Unsupported);

    {
        let facts = r.evidence.0.lock();
        assert!(facts.iter().any(|u| matches!(u.facts, SourceFacts::Process(ProcessFacts::Gone { .. }))));
        assert!(facts.iter().any(|u| matches!(&u.facts, SourceFacts::Gilde(g) if g.stop_cause == Some(StopCause::User))));
    }

    // The stopped turn's Done does not start anything.
    r.sidecar.emit(0, 1, done(TurnDoneStatus::Stopped, ""));
    settle().await;
    assert_eq!(r.sidecar.started().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conversation_only_operations_are_unsupported_and_the_model_applies_next_turn() {
    let mut r = rig(Vendor::Codex);
    let err = r.driver.force_stop().await.expect_err("force stop");
    assert_eq!(err.code, ErrorCode::Unsupported);
    let err = r
        .driver
        .set_permission_mode(PermissionMode::Plan)
        .await
        .expect_err("fixed at start");
    assert_eq!(err.code, ErrorCode::Unsupported);

    r.driver.send(msg(1, "one")).await.expect("send");
    assert_eq!(
        r.driver.set_model(Some("m-2".into())).await.expect("model"),
        AppliesFrom::NextTurn
    );
    assert_eq!(r.sidecar.turn(0).model, None, "the running turn keeps its model");
    r.sidecar.emit(0, 1, done(TurnDoneStatus::Ok, "c"));
    until("turn end", || !r.driver.is_running()).await;
    r.driver.send(msg(2, "two")).await.expect("send");
    assert_eq!(r.sidecar.turn(1).model.as_deref(), Some("m-2"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn events_of_a_replaced_turn_are_ignored() {
    let mut r = rig(Vendor::Agy);
    r.driver.send(msg(1, "one")).await.expect("send");
    r.sidecar.emit(0, 1, done(TurnDoneStatus::Ok, "c"));
    until("turn end", || !r.driver.is_running()).await;
    r.driver.send(msg(2, "two")).await.expect("send");
    // A late event of turn 1 must not end turn 2.
    r.sidecar.emit(0, 2, error(VendorErrorClass::Exec, "late"));
    settle().await;
    assert!(r.driver.is_running());
    assert_eq!(r.evidence.last_turn_end(), Some(TurnOutcome::Ok));
}

fn usage(input: u64, output: u64) -> TurnUsage {
    TurnUsage {
        input_tokens: Some(input),
        output_tokens: Some(output),
        cached_input_tokens: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spend_is_a_deduplicated_running_total_sent_at_most_once_a_second() {
    let mut r = rig(Vendor::Codex);
    r.driver.send(msg(1, "one")).await.expect("send");
    r.sidecar.emit(0, 1, TurnEvent::Usage { usage: usage(10, 1) });
    r.sidecar.emit(0, 2, TurnEvent::Usage { usage: usage(20, 2) });
    r.sidecar.emit(
        0,
        3,
        TurnEvent::Done {
            status: TurnDoneStatus::Ok,
            conversation_id: "c".into(),
            usage: Some(usage(20, 2)),
            cost_usd: None,
        },
    );
    until("the first report", || !r.evidence.usage_reports().is_empty()).await;
    let first = r.evidence.usage_reports();
    assert_eq!(first.len(), 1, "inside the first second only one report: {first:?}");
    // The held-back change follows when the second is over.
    until("the trailing report", || {
        r.evidence.usage_reports().last().map(|t| t.input_tokens) == Some(Some(20))
    })
    .await;
    let reports = r.evidence.usage_reports();
    assert!(reports.len() <= 2, "{reports:?}");
    let last = reports.last().expect("report").clone();
    assert_eq!((last.input_tokens, last.output_tokens), (Some(20), Some(2)), "the turn counted once");
    assert_eq!(last.cost_micros, None, "no vendor cost: absent");
    assert_eq!(last.day, "2026-10-22");

    // A second turn adds to today's total.
    r.driver.send(msg(2, "two")).await.expect("send");
    r.sidecar.emit(
        1,
        1,
        TurnEvent::Done {
            status: TurnDoneStatus::Ok,
            conversation_id: "c".into(),
            usage: Some(usage(5, 0)),
            cost_usd: Some(0.25),
        },
    );
    until("the day total", || {
        r.evidence.usage_reports().last().map(|t| t.input_tokens) == Some(Some(25))
    })
    .await;
    let last = r.evidence.usage_reports().last().expect("report").clone();
    assert_eq!((last.output_tokens, last.cost_micros), (Some(2), Some(250_000)));

    // A new local day starts a new total.
    *r.day.lock() = "2026-10-23".to_string();
    r.driver.send(msg(3, "three")).await.expect("send");
    r.sidecar.emit(2, 1, TurnEvent::Usage { usage: usage(3, 1) });
    until("the new day", || {
        r.evidence.usage_reports().last().map(|t| t.day.clone()) == Some("2026-10-23".to_string())
    })
    .await;
    let last = r.evidence.usage_reports().last().expect("report").clone();
    assert_eq!((last.input_tokens, last.output_tokens, last.cost_micros), (Some(3), Some(1), None));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_whose_vendor_reports_no_usage_sends_no_spend() {
    let mut r = rig(Vendor::Agy);
    r.driver.send(msg(1, "one")).await.expect("send");
    r.sidecar.emit(0, 1, done(TurnDoneStatus::Ok, "c"));
    until("turn end", || !r.driver.is_running()).await;
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    assert!(r.evidence.usage_reports().is_empty(), "absent = unknown, never 0");
}
