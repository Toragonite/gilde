//! `NotifyService`, `OwnerFocusService` and `WorldLabels` against fakes
//! (contract §8.3–§8.5, §9.5, §14.2): badge = `need_you`, T-notify-denied,
//! the worker's ordering and its off-the-caller label lookup, click routing,
//! pause values and expiry, the wake grace through the system hub, the probe
//! rate of `notify_test`, and owner focus (pid-reuse fence, window precision
//! only behind the LP-24 flag). Nothing here touches the OS: every sink,
//! emitter, activator and clock is a fake.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use gilde_code_lib::contract::activity::PrimaryAction;
use gilde_code_lib::contract::attention::{
    AttentionItem, AttentionQueue, AttentionReason, AttentionSubject, PAUSED_UNTIL_RESUME,
};
use gilde_code_lib::contract::common::AbsPath;
use gilde_code_lib::contract::error::{ErrorCode, IpcError};
use gilde_code_lib::contract::ids::{EpisodeId, LocationId, MemberKey, ProjectId, SessionUid, TeamKey};
use gilde_code_lib::contract::ipc::{
    DismissWhat, EditorKind, FocusPrecision, IdeEditorsRes, IdeOpenReq, IdeRevealReq, NotifySettings, PauseUntil, Screen,
    ViewState,
};
use gilde_code_lib::contract::services::{
    EvidenceSink, IdeApi, IdentityKey, IdentityLock, NotifyApi, StorePatchSink, WorldApi,
};
use gilde_code_lib::contract::session::{Session, SessionDetails};
use gilde_code_lib::contract::store::{FocusTarget, StoreSnapshot, VisibilityEvent};
use gilde_code_lib::contract::team::TeamAdapter;
use gilde_code_lib::macos::dock::DockSink;
use gilde_code_lib::macos::owner::{Activation, AppActivator, OwnerFocusService, CURSOR_BUNDLE_ID, VSCODE_BUNDLE_ID};
use gilde_code_lib::macos::sleep::{SystemEvent, SystemEvents};
use gilde_code_lib::macos::visibility::{VisibilityEmitter, VisibilityHub};
use gilde_code_lib::notify::policy::MEMORY_ID;
use gilde_code_lib::notify::{
    Authorization, ClickHandler, Clock, FocusEmitter, LabelSource, Locale, Notification, NotificationSink, NotifyConfig,
    NotifyDeps, NotifyService, NotifyState, Now, SessionLabels, WorldLabels,
};
use parking_lot::Mutex;
use serde_json::{json, Value};

const LAUNCH: Now = Now { up: 9_000_000, wall: 1_792_000_000_000 };

// -- fakes ---------------------------------------------------------------------------

struct FakeSink {
    auth: Mutex<Authorization>,
    grant_on_request: bool,
    delivered: Mutex<Vec<Notification>>,
    withdrawn: Mutex<Vec<String>>,
    click: Mutex<Option<ClickHandler>>,
    requests: AtomicUsize,
}

impl FakeSink {
    fn new(auth: Authorization) -> Arc<FakeSink> {
        Arc::new(FakeSink {
            auth: Mutex::new(auth),
            grant_on_request: true,
            delivered: Mutex::new(Vec::new()),
            withdrawn: Mutex::new(Vec::new()),
            click: Mutex::new(None),
            requests: AtomicUsize::new(0),
        })
    }
    fn delivered(&self) -> Vec<Notification> {
        self.delivered.lock().clone()
    }
    fn click(&self, id: &str) {
        let handler = self.click.lock().clone().expect("the service registered a click handler");
        handler(id.to_string());
    }
}

#[async_trait]
impl NotificationSink for FakeSink {
    fn deliver(&self, n: &Notification) -> bool {
        self.delivered.lock().push(n.clone());
        true
    }
    fn withdraw(&self, ids: &[String]) {
        self.withdrawn.lock().extend(ids.iter().cloned());
    }
    fn supports_withdrawal(&self) -> bool {
        true
    }
    fn authorization(&self) -> Authorization {
        *self.auth.lock()
    }
    async fn request_authorization(&self) -> bool {
        self.requests.fetch_add(1, Ordering::SeqCst);
        if self.grant_on_request {
            *self.auth.lock() = Authorization::Granted;
        }
        self.grant_on_request
    }
    fn set_click_handler(&self, handler: ClickHandler) {
        *self.click.lock() = Some(handler);
    }
}

#[derive(Default)]
struct FakeDock(Mutex<Vec<u32>>);
impl DockSink for FakeDock {
    fn set_badge(&self, count: u32) {
        self.0.lock().push(count);
    }
}

#[derive(Default)]
struct FakeVisEmitter(Mutex<Vec<VisibilityEvent>>);
impl VisibilityEmitter for FakeVisEmitter {
    fn emit(&self, event: VisibilityEvent) {
        self.0.lock().push(event);
    }
}

#[derive(Default)]
struct FakeFocus(Mutex<Vec<FocusTarget>>);
impl FocusEmitter for FakeFocus {
    fn focus_request(&self, target: FocusTarget) {
        self.0.lock().push(target);
    }
}

struct FakeClock(Mutex<Now>);
impl FakeClock {
    fn advance(&self, ms: u64) {
        let mut now = self.0.lock();
        now.up += ms;
        now.wall += ms as i64;
    }
    fn get(&self) -> Now {
        *self.0.lock()
    }
}
impl Clock for FakeClock {
    fn now(&self) -> Now {
        *self.0.lock()
    }
}

/// Labels that also prove where they were resolved: never on the thread that
/// called `queue_changed`, never while the caller's "World lock" is held.
struct FakeLabels {
    caller_thread: Mutex<Option<std::thread::ThreadId>>,
    world_lock: Arc<Mutex<()>>,
    on_caller_thread: AtomicBool,
    lock_was_free: AtomicBool,
    calls: AtomicUsize,
}

impl LabelSource for FakeLabels {
    fn labels(&self, uids: &[SessionUid]) -> HashMap<SessionUid, SessionLabels> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if Some(std::thread::current().id()) == *self.caller_thread.lock() {
            self.on_caller_thread.store(true, Ordering::SeqCst);
        }
        let free = self.world_lock.try_lock_for(Duration::from_secs(10)).is_some();
        self.lock_was_free.store(free, Ordering::SeqCst);
        uids.iter()
            .map(|u| {
                (
                    u.clone(),
                    SessionLabels {
                        project: Some("checkout-service".into()),
                        session: "payment-retry".into(),
                        detail: None,
                        title: Some("PRIVATE_TITLE".into()),
                        summary: Some("SECRET_ARG".into()),
                    },
                )
            })
            .collect()
    }
}

struct Rig {
    service: Arc<NotifyService>,
    sink: Arc<FakeSink>,
    dock: Arc<FakeDock>,
    focus: Arc<FakeFocus>,
    clock: Arc<FakeClock>,
    labels: Arc<FakeLabels>,
    visibility: Arc<VisibilityHub>,
    system: Arc<SystemEvents>,
}

fn rig(auth: Authorization) -> Rig {
    let sink = FakeSink::new(auth);
    let dock = Arc::new(FakeDock::default());
    let focus = Arc::new(FakeFocus::default());
    let clock = Arc::new(FakeClock(Mutex::new(LAUNCH)));
    let labels = Arc::new(FakeLabels {
        caller_thread: Mutex::new(None),
        world_lock: Arc::new(Mutex::new(())),
        on_caller_thread: AtomicBool::new(false),
        lock_was_free: AtomicBool::new(false),
        calls: AtomicUsize::new(0),
    });
    let visibility = VisibilityHub::new(Arc::new(FakeVisEmitter::default()));
    let system = SystemEvents::new();
    visibility.follow(&system);
    let service = NotifyService::start(
        NotifyDeps {
            sink: sink.clone(),
            dock: dock.clone(),
            visibility: visibility.clone(),
            system: system.clone(),
            labels: labels.clone(),
            focus: focus.clone(),
            clock: clock.clone(),
        },
        NotifyConfig { launch: Some(LAUNCH) },
    );
    service.boot_settled();
    Rig { service, sink, dock, focus, clock, labels, visibility, system }
}

fn item(uid: &SessionUid, n: i64, band: u8) -> AttentionItem {
    AttentionItem {
        key: format!("q_{}", uid.as_str()),
        band,
        subject: AttentionSubject::Session { uid: uid.clone(), on_behalf_of: None, answer_in: None },
        reason: if band == 1 { AttentionReason::NeedsApproval } else { AttentionReason::NeedsAnswer },
        since: LAUNCH.wall + 1,
        waited_ms: 0,
        episode: EpisodeId::derive(uid, "needsApproval", n),
        later_until: None,
        primary: PrimaryAction::Answer,
        watched: false,
    }
}

/// Every queue World hands over carries a newer rev (A48.2).
fn queue(items: Vec<AttentionItem>, need_you: u32) -> AttentionQueue {
    static REV: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    AttentionQueue { rev: REV.fetch_add(1, Ordering::SeqCst), items, need_you, also: 0, paused_until: None }
}

// -- badge and the denied path ----------------------------------------------------------

#[tokio::test]
async fn badge_equals_need_you_on_every_queue_change() {
    let r = rig(Authorization::Granted);
    let a = SessionUid::mint();
    let b = SessionUid::mint();
    r.service.queue_changed(&queue(vec![item(&a, 1, 1), item(&b, 1, 2)], 2));
    r.service.queue_changed(&queue(vec![item(&a, 1, 1), item(&b, 1, 2)], 2));
    // a Later-ed item is already excluded from `need_you` by World: shown as delivered
    r.service.queue_changed(&queue(vec![item(&a, 1, 1), item(&b, 1, 2)], 1));
    r.service.queue_changed(&queue(vec![], 0));
    assert_eq!(*r.dock.0.lock(), vec![2, 1, 0], "written on change only; empty at 0");
}

/// A48.2 (G7c-11): a late, older queue — computed before a newer one but
/// handed over after it — moves neither the badge nor a notification.
#[tokio::test]
async fn a48_2_a_late_older_queue_moves_neither_the_badge_nor_a_notification() {
    let r = rig(Authorization::Granted);
    let a = SessionUid::mint();
    let older = queue(vec![], 0);
    let newer = queue(vec![item(&a, 1, 1)], 1);
    assert!(older.rev < newer.rev);
    r.service.queue_changed(&newer);
    r.service.settle().await;
    assert_eq!(r.sink.delivered().len(), 1, "the newer queue notifies");
    r.service.queue_changed(&older);
    r.service.queue_changed(&newer); // a duplicate of the last one
    r.service.settle().await;
    r.clock.advance(20_000);
    r.service.poke();
    r.service.settle().await;
    assert_eq!(*r.dock.0.lock(), vec![1], "the badge never went back to the older count");
    assert!(r.sink.withdrawn.lock().is_empty(), "the older queue withdrew nothing");
    assert_eq!(r.sink.delivered().len(), 1, "and re-delivered nothing");
    // the next newer queue is applied as usual
    r.service.queue_changed(&queue(vec![], 0));
    r.service.settle().await;
    assert_eq!(*r.dock.0.lock(), vec![1, 0]);
    assert_eq!(r.sink.withdrawn.lock().len(), 1, "resolved by a newer queue: withdrawn");
}

#[tokio::test]
async fn t_notify_denied_queue_and_badge_still_work() {
    let r = rig(Authorization::Denied);
    let a = SessionUid::mint();
    r.service.queue_changed(&queue(vec![item(&a, 1, 1)], 1));
    r.service.settle().await;
    assert!(r.sink.delivered().is_empty(), "nothing is posted without permission");
    assert_eq!(*r.dock.0.lock(), vec![1], "the badge still counts");
    assert!(!r.service.test().await, "the test notification reports not delivered");
    // the user grants later: a new state notifies, the old one never backfills
    assert!(r.service.permission_request().await);
    assert_eq!(r.sink.requests.load(Ordering::SeqCst), 1);
    let b = SessionUid::mint();
    r.clock.advance(1_000);
    r.service.queue_changed(&queue(vec![item(&a, 1, 1), item(&b, 1, 1)], 2));
    r.service.settle().await;
    assert_eq!(r.sink.delivered().len(), 1);
    assert_eq!(*r.dock.0.lock(), vec![1, 2]);
}

#[tokio::test]
async fn nothing_prompts_for_permission_except_the_explicit_request() {
    let r = rig(Authorization::Unknown);
    let a = SessionUid::mint();
    r.service.queue_changed(&queue(vec![item(&a, 1, 1)], 1));
    r.service.pause(PauseUntil::OneHour);
    r.service.resume();
    let _ = r.service.test().await;
    r.service.settle().await;
    assert_eq!(r.sink.requests.load(Ordering::SeqCst), 0);
}

// -- the worker -------------------------------------------------------------------------

#[tokio::test]
async fn labels_are_resolved_on_the_worker_never_under_the_callers_lock() {
    let r = rig(Authorization::Granted);
    *r.labels.caller_thread.lock() = Some(std::thread::current().id());
    let a = SessionUid::mint();
    {
        // World calls queue_changed while holding its own lock
        let _world_lock = r.labels.world_lock.lock();
        r.service.queue_changed(&queue(vec![item(&a, 1, 1)], 1));
    }
    r.service.settle().await;
    assert_eq!(r.labels.calls.load(Ordering::SeqCst), 1);
    assert!(!r.labels.on_caller_thread.load(Ordering::SeqCst), "resolved off the caller's stack");
    assert!(r.labels.lock_was_free.load(Ordering::SeqCst), "never while the caller's lock is held");
    let n = &r.sink.delivered()[0];
    assert_eq!(n.title, "checkout-service");
    assert_eq!(n.body, "payment-retry · 승인 대기", "details hidden by default");
}

#[tokio::test]
async fn settings_and_locale_change_the_text() {
    let r = rig(Authorization::Granted);
    r.service.set_locale(Locale::En);
    r.service.set_settings(NotifySettings { watched_sessions: true, sound: true, show_details: true });
    let a = SessionUid::mint();
    r.service.queue_changed(&queue(vec![item(&a, 1, 1)], 1));
    r.service.settle().await;
    let n = &r.sink.delivered()[0];
    assert_eq!(n.body, "PRIVATE_TITLE · Needs approval\nSECRET_ARG");
    assert!(n.sound, "band 1 with the sound on");
}

#[tokio::test]
async fn view_state_and_visibility_reach_the_policy() {
    let r = rig(Authorization::Granted);
    let a = SessionUid::mint();
    r.visibility.set_frontmost(true);
    r.service.view_state(ViewState {
        screen: Screen::Session,
        visible_uids: vec![a.clone()],
        focused_uid: Some(a.clone()),
        zoomed_uid: None,
        team_key: None,
        document_visible: true,
    });
    r.service.queue_changed(&queue(vec![item(&a, 1, 1)], 1));
    r.service.settle().await;
    assert!(r.sink.delivered().is_empty(), "on screen");
    assert_eq!(r.service.visibility(), VisibilityEvent { frontmost: true, occluded: false, asleep: false });
    // Gilde goes to the background: the next new state notifies
    r.visibility.set_frontmost(false);
    let b = SessionUid::mint();
    r.service.queue_changed(&queue(vec![item(&a, 1, 1), item(&b, 1, 1)], 2));
    r.service.settle().await;
    assert_eq!(r.sink.delivered().len(), 1);
}

#[tokio::test]
async fn typing_in_gilde_suppresses() {
    let r = rig(Authorization::Granted);
    r.service.input_activity();
    let a = SessionUid::mint();
    r.service.queue_changed(&queue(vec![item(&a, 1, 1)], 1));
    r.service.settle().await;
    assert!(r.sink.delivered().is_empty());
}

#[tokio::test]
async fn resolution_withdraws_and_clicks_route_to_focus_requests() {
    let r = rig(Authorization::Granted);
    let a = SessionUid::mint();
    r.service.queue_changed(&queue(vec![item(&a, 1, 1)], 1));
    r.service.settle().await;
    let id = r.sink.delivered()[0].id.clone();
    assert!(id.starts_with("gilde.e.e_"), "ids only: {id}");
    r.sink.click(&id);
    r.sink.click("gilde.e.e_from_an_earlier_run");
    r.service.settle().await;
    assert_eq!(*r.focus.0.lock(), vec![FocusTarget::Session { uid: a.clone(), focus_card: true }, FocusTarget::Home]);
    r.service.queue_changed(&queue(vec![], 0));
    r.service.settle().await;
    assert_eq!(*r.sink.withdrawn.lock(), vec![id]);
}

// -- pause --------------------------------------------------------------------------------

#[tokio::test]
async fn pause_values_announcements_and_expiry() {
    let r = rig(Authorization::Granted);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    r.service.on_pause_change(Arc::new(move |v| s.lock().push(v)));
    let one_hour = r.service.pause(PauseUntil::OneHour);
    assert_eq!(one_hour, Some(LAUNCH.wall + 3_600_000));
    let a = SessionUid::mint();
    r.service.queue_changed(&queue(vec![item(&a, 1, 1)], 1));
    r.service.settle().await;
    assert!(r.sink.delivered().is_empty(), "paused");
    assert_eq!(*r.dock.0.lock(), vec![1], "the badge still updates while paused");
    // expiry
    r.clock.advance(3_600_000);
    r.service.poke();
    r.service.settle().await;
    assert_eq!(*seen.lock(), vec![one_hour, None]);
    // until resume
    assert_eq!(r.service.pause(PauseUntil::UntilResume), Some(PAUSED_UNTIL_RESUME));
    r.service.resume();
    let tomorrow = r.service.pause(PauseUntil::TomorrowNine).unwrap();
    let now = r.clock.get().wall;
    assert!(tomorrow > now && tomorrow - now <= 33 * 3_600_000);
    r.service.settle().await;
    // announced by the worker in command order: never a stale value in between
    assert_eq!(*seen.lock(), vec![one_hour, None, Some(PAUSED_UNTIL_RESUME), None, Some(tomorrow)]);
}

#[tokio::test]
async fn a_new_pause_racing_an_expiry_is_never_announced_as_unpaused() {
    let r = rig(Authorization::Granted);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    r.service.on_pause_change(Arc::new(move |v| s.lock().push(v)));
    let first = r.service.pause(PauseUntil::OneHour);
    r.service.settle().await;
    // the old pause expires and a new one is set before the worker ticks
    r.clock.advance(3_600_000);
    let second = r.service.pause(PauseUntil::OneHour);
    r.service.poke();
    r.service.settle().await;
    assert_eq!(seen.lock().last().copied(), Some(second), "the final state is paused: {:?}", seen.lock());
    assert_eq!(seen.lock()[0], first);
}

// -- sleep / wake -------------------------------------------------------------------------

#[tokio::test]
async fn the_wake_grace_runs_through_the_system_hub() {
    let r = rig(Authorization::Granted);
    r.system.publish(SystemEvent::WillSleep);
    r.clock.advance(1); // uptime stops while asleep
    r.system.publish(SystemEvent::DidWake);
    assert!(!r.visibility.current().asleep);
    let a = SessionUid::mint();
    r.clock.advance(1_000);
    r.service.queue_changed(&queue(vec![item(&a, 1, 1)], 1));
    r.service.settle().await;
    assert!(r.sink.delivered().is_empty(), "began during sleep: 5 s grace");
    r.clock.advance(4_000);
    r.service.poke();
    r.service.settle().await;
    assert_eq!(r.sink.delivered().len(), 1);
}

// -- health and test -----------------------------------------------------------------------

#[tokio::test]
async fn memory_pressure_and_the_test_notification() {
    let r = rig(Authorization::Granted);
    r.service.health_memory(true);
    r.service.health_memory(true);
    r.service.settle().await;
    let got = r.sink.delivered();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].id, MEMORY_ID);
    assert_eq!(got[0].body, "메모리 압박이 높습니다.");
    assert!(r.service.test().await);
    let test = r.sink.delivered().last().cloned().unwrap();
    assert_eq!(test.title, "Gilde");
    assert_eq!(test.body, "세션이 나를 기다릴 때 알림을 받을 수 있습니다.");
}

#[tokio::test]
async fn notify_test_is_rate_limited_to_one_per_minute() {
    let r = rig(Authorization::Granted);
    let state = NotifyState::new(r.service.clone(), Arc::new(FakeWorld::default()));
    assert!(state.test_permit().is_ok());
    let retry = state.test_permit().expect_err("second call within 60 s");
    assert!(retry > 0 && retry <= 60_000, "{retry}");
    let err = IpcError::busy(retry);
    assert_eq!(err.code, ErrorCode::Busy);
}

// -- a World for owner focus and labels ------------------------------------------------------

#[derive(Default)]
struct FakeWorld {
    snapshot: Mutex<Option<Value>>,
}

impl FakeWorld {
    fn with(snapshot: Value) -> FakeWorld {
        FakeWorld { snapshot: Mutex::new(Some(snapshot)) }
    }
    fn snap(&self) -> StoreSnapshot {
        serde_json::from_value(self.snapshot.lock().clone().expect("a snapshot")).expect("a valid snapshot")
    }
}

#[async_trait]
impl WorldApi for FakeWorld {
    fn sink(&self) -> Arc<dyn EvidenceSink> {
        unimplemented!("not used")
    }
    fn subscribe(&self, _sink: Box<dyn StorePatchSink>) -> StoreSnapshot {
        unimplemented!("not used")
    }
    fn snapshot(&self) -> StoreSnapshot {
        self.snap()
    }
    fn session(&self, uid: &SessionUid) -> Option<Session> {
        self.snapshot.lock().as_ref()?;
        self.snap().sessions.into_iter().find(|s| &s.uid == uid)
    }
    fn queue(&self) -> AttentionQueue {
        unimplemented!("not used")
    }
    async fn identity_lock(&self, _key: IdentityKey) -> IdentityLock {
        unimplemented!("not used")
    }
    fn mark_seen(&self, _uid: &SessionUid) -> Result<(), IpcError> {
        Ok(())
    }
    fn dismiss(&self, _uid: &SessionUid, _what: DismissWhat) -> Result<(), IpcError> {
        Ok(())
    }
    fn queue_later(&self, _uid: &SessionUid) -> Result<i64, IpcError> {
        Ok(0)
    }
    fn register_team_adapter(&self, _adapter: Arc<dyn TeamAdapter>) {}
}

fn session_json(uid: &SessionUid, control: Value, extra: &[(&str, Value)]) -> Value {
    let mut s = json!({
        "uid": uid, "vendor": "claude", "accountId": null, "vendorSessionId": null,
        "projectId": null, "locationId": null, "cwd": "/w/demo", "control": control,
        "activity": {"state": {"kind": "working"}, "since": 1, "confidence": "high", "evidence": null, "episode": "e_0123456789abcdef"},
        "title": null, "team": null, "startedAt": null, "endedAt": null, "lastActivityAt": null,
        "pending": [], "queued": [], "model": null, "permissionMode": null, "binary": null,
        "claudeVersion": null, "memoryBytes": null, "usageToday": null,
        "caps": {"send": "none", "interrupt": false, "forceStop": false, "approve": "none", "setModel": false,
                 "setPermissionMode": [], "continueOptions": [], "stopSession": "none", "terminalView": false,
                 "logView": false, "changes": "none", "ownerFocus": true},
        "details": serde_json::to_value(SessionDetails::default()).unwrap(), "mergedInto": null
    });
    for (k, v) in extra {
        s[*k] = v.clone();
    }
    s
}

fn snapshot_json(projects: Value, locations: Value, sessions: Vec<Value>, members: Value) -> Value {
    json!({
        "rev": 1, "projects": projects, "locations": locations, "sessions": sessions, "teams": [],
        "members": members, "tasks": [], "messages": [],
        "queue": {"rev": 0, "items": [], "needYou": 0, "also": 0, "pausedUntil": null},
        "accounts": [], "leftovers": [], "compat": [], "notices": [],
        "boot": {"spawnGateOpen": true, "firstDiscoveryDone": true, "agentsDetected": true, "memoryOnly": null, "agents": [], "agentsScannedAt": null, "recovery": null},
        "sources": [], "integrations": {"orchestrator": "absent"}
    })
}

fn project_json(id: &ProjectId, name: &str, root: &str) -> Value {
    json!({"id": id, "name": name, "root": root, "kind": "git", "order": 0, "folded": false, "earlier": false,
           "lastActivityAt": null, "liveSessions": 1, "isTemp": false})
}

fn watching(pid: i32, bundle: &str) -> Value {
    json!({"kind": "watching", "origin": "vsCode", "owner": {"appPid": pid, "appName": "x", "kind": "vsCode", "bundleId": bundle}})
}

#[derive(Default)]
struct FakeActivator {
    calls: Mutex<Vec<(i32, String)>>,
    answer: Mutex<Option<Activation>>,
}
impl AppActivator for FakeActivator {
    fn activate(&self, pid: i32, bundle_id: &str) -> Activation {
        self.calls.lock().push((pid, bundle_id.to_string()));
        self.answer.lock().unwrap_or(Activation::Activated)
    }
}

struct FakeIde {
    answer: Result<bool, ()>,
    calls: Mutex<Vec<(EditorKind, String)>>,
}
#[async_trait]
impl IdeApi for FakeIde {
    async fn open(&self, _req: &IdeOpenReq) -> Result<(), IpcError> {
        unimplemented!("not used")
    }
    async fn reveal(&self, _req: &IdeRevealReq) -> Result<(), IpcError> {
        unimplemented!("not used")
    }
    fn editors(&self) -> IdeEditorsRes {
        unimplemented!("not used")
    }
    async fn link_open(&self, _url: &str) -> Result<bool, IpcError> {
        unimplemented!("not used")
    }
    async fn focus_editor_window(&self, editor: EditorKind, project_root: &AbsPath) -> Result<bool, IpcError> {
        self.calls.lock().push((editor, project_root.as_str().to_string()));
        self.answer.map_err(|_| IpcError::spawn_failed())
    }
}

fn owner_world(uid: &SessionUid, control: Value) -> (Arc<FakeWorld>, ProjectId) {
    let project = ProjectId::derive("/w/demo");
    let session = session_json(uid, control, &[("projectId", json!(project))]);
    let world = FakeWorld::with(snapshot_json(json!([project_json(&project, "demo", "/w/demo")]), json!([]), vec![session], json!([])));
    (Arc::new(world), project)
}

fn assert_code(err: &IpcError, code: ErrorCode, param: (&str, &str)) {
    assert_eq!(err.code, code, "{err:?}");
    assert_eq!(err.params.get(param.0).and_then(Value::as_str), Some(param.1), "{err:?}");
}

#[tokio::test]
async fn owner_focus_brings_the_owner_app_forward() {
    let uid = SessionUid::mint();
    let (world, _) = owner_world(&uid, watching(4242, "com.apple.Terminal"));
    let activator = Arc::new(FakeActivator::default());
    let service = OwnerFocusService::new(world, activator.clone(), false);
    let res = service.focus(&uid).await.expect("focused");
    assert_eq!(res.precision, FocusPrecision::App);
    assert_eq!(*activator.calls.lock(), vec![(4242, "com.apple.Terminal".to_string())]);
}

#[tokio::test]
async fn owner_focus_refuses_sessions_without_an_owner() {
    let uid = SessionUid::mint();
    let activator = Arc::new(FakeActivator::default());
    let in_gilde = json!({"kind": "inGilde", "mode": "conversation", "pty": null, "hostedTeam": false, "transport": "l1"});
    let unknown_owner = json!({"kind": "watching", "origin": "terminal", "owner": null});
    for control in [in_gilde, unknown_owner, watching(1, "com.apple.Terminal"), watching(std::process::id() as i32, "x"), watching(4242, " ")] {
        let (world, _) = owner_world(&uid, control.clone());
        let service = OwnerFocusService::new(world, activator.clone(), true);
        let err = service.focus(&uid).await.expect_err("no owner");
        assert_code(&err, ErrorCode::Unsupported, ("cap", "noOwner"));
    }
    assert!(activator.calls.lock().is_empty(), "nothing was activated");
    let (world, _) = owner_world(&uid, watching(4242, "x"));
    let service = OwnerFocusService::new(world, activator, false);
    let err = service.focus(&SessionUid::mint()).await.expect_err("unknown session");
    assert_code(&err, ErrorCode::NotFound, ("entity", "session"));
}

#[tokio::test]
async fn owner_focus_never_activates_a_reused_pid() {
    let uid = SessionUid::mint();
    for answer in [Activation::NotRunning, Activation::NotThatApp] {
        let (world, _) = owner_world(&uid, watching(4242, "com.apple.Terminal"));
        let activator = Arc::new(FakeActivator::default());
        *activator.answer.lock() = Some(answer);
        let service = OwnerFocusService::new(world, activator, false);
        let err = service.focus(&uid).await.expect_err("owner gone");
        assert_code(&err, ErrorCode::Unsupported, ("cap", "noOwner"));
    }
    let (world, _) = owner_world(&uid, watching(4242, "com.apple.Terminal"));
    let activator = Arc::new(FakeActivator::default());
    *activator.answer.lock() = Some(Activation::Refused);
    let err = OwnerFocusService::new(world, activator, false).focus(&uid).await.expect_err("refused");
    assert_eq!(err.code, ErrorCode::Internal);
}

#[tokio::test]
async fn window_precision_only_behind_the_lp24_flag_with_the_ide_bound() {
    let uid = SessionUid::mint();
    // flag on, VS Code, IDE answers true → window
    let (world, _) = owner_world(&uid, watching(4242, VSCODE_BUNDLE_ID));
    let activator = Arc::new(FakeActivator::default());
    let service = OwnerFocusService::new(world.clone(), activator.clone(), true);
    let res = service.focus(&uid).await.unwrap();
    assert_eq!(res.precision, FocusPrecision::App, "no IdeApi bound yet → app");
    let ide = Arc::new(FakeIde { answer: Ok(true), calls: Mutex::new(Vec::new()) });
    assert!(service.set_ide(ide.clone()));
    assert_eq!(service.focus(&uid).await.unwrap().precision, FocusPrecision::Window);
    assert_eq!(*ide.calls.lock(), vec![(EditorKind::VsCode, "/w/demo".to_string())]);
    assert_eq!(activator.calls.lock().len(), 1, "the window path did not also activate the app");
    // flag off → never asks the IDE
    let off = OwnerFocusService::new(world, activator.clone(), false);
    let ide_off = Arc::new(FakeIde { answer: Ok(true), calls: Mutex::new(Vec::new()) });
    off.set_ide(ide_off.clone());
    assert_eq!(off.focus(&uid).await.unwrap().precision, FocusPrecision::App);
    assert!(ide_off.calls.lock().is_empty());
    // the candidate fails or declines → the app fallback (Cursor)
    let (cursor_world, _) = owner_world(&uid, watching(4243, CURSOR_BUNDLE_ID));
    for answer in [Ok(false), Err(())] {
        let svc = OwnerFocusService::new(cursor_world.clone(), activator.clone(), true);
        let ide = Arc::new(FakeIde { answer, calls: Mutex::new(Vec::new()) });
        svc.set_ide(ide.clone());
        assert_eq!(svc.focus(&uid).await.unwrap().precision, FocusPrecision::App);
        assert_eq!(ide.calls.lock()[0].0, EditorKind::Cursor);
    }
}

#[tokio::test]
async fn owner_focus_follows_a_merged_session_once() {
    let old = SessionUid::mint();
    let new = SessionUid::mint();
    let project = ProjectId::derive("/w/demo");
    let tomb = session_json(&old, json!({"kind": "watching", "origin": "terminal", "owner": null}), &[("mergedInto", json!(new))]);
    let live = session_json(&new, watching(4242, "com.apple.Terminal"), &[("projectId", json!(project))]);
    let world = Arc::new(FakeWorld::with(snapshot_json(json!([project_json(&project, "demo", "/w/demo")]), json!([]), vec![tomb, live], json!([]))));
    let activator = Arc::new(FakeActivator::default());
    let res = OwnerFocusService::new(world, activator.clone(), false).focus(&old).await.unwrap();
    assert_eq!(res.precision, FocusPrecision::App);
    assert_eq!(activator.calls.lock()[0].0, 4242);
}

// -- WorldLabels ---------------------------------------------------------------------------------

#[test]
fn world_labels_are_content_free_and_keep_content_apart() {
    let project = ProjectId::derive("/w/shop");
    let location = LocationId::derive("/w/shop-wt/payment-retry");
    let team = TeamKey::derive("claudeNative", None, "auth-split");
    let member_key = MemberKey::derive(&team, "db-agent@auth-split", None);
    let worktree_session = SessionUid::mint();
    let member_session = SessionUid::mint();
    let plain_session = SessionUid::mint();
    let lead = SessionUid::mint();
    let pending = json!({
        "requestId": "r1", "kind": "toolApproval", "receivedAt": 1, "onBehalfOf": null, "answerIn": null,
        "title": "Run a command", "tool": {"tool": "Bash", "verb": "run", "object": "npm run migrate"},
        "detailLines": [], "options": [], "alwaysLabel": null, "defaultToNo": false, "degraded": false,
        "promptRev": null, "confidence": "high", "multiSelect": false, "questions": []
    });
    let mut lead_pending = pending.clone();
    lead_pending["onBehalfOf"] = json!(member_key);
    lead_pending["tool"]["object"] = json!("rm -rf build");
    let in_gilde = json!({"kind": "inGilde", "mode": "conversation", "pty": null, "hostedTeam": false, "transport": "l1"});
    let sessions = vec![
        session_json(&worktree_session, in_gilde.clone(), &[
            ("projectId", json!(project)), ("locationId", json!(location)),
            ("title", json!("first prompt PRIVATE")), ("pending", json!([pending])),
        ]),
        session_json(&member_session, in_gilde.clone(), &[
            ("projectId", json!(project)),
            ("team", json!({"teamKey": team, "memberKey": member_key, "role": "member", "colour": null})),
        ]),
        session_json(&plain_session, in_gilde.clone(), &[("vendor", json!("codex")), ("startedAt", json!(1_792_000_000_000i64))]),
        session_json(&lead, in_gilde, &[("pending", json!([lead_pending]))]),
    ];
    let locations = json!([{"id": location, "projectId": project, "path": "/w/shop-wt/payment-retry", "kind": "worktree",
                            "branch": "payment-retry", "worktreeName": "payment-retry", "gildeCreated": true, "exists": true}]);
    let members = json!([{"key": member_key, "teamKey": team, "sessionUid": member_session, "name": "db-agent",
                          "agentId": null, "role": "member", "colour": null, "vendor": "claude", "backend": "pane",
                          "joinedAt": null, "presence": {"kind": "present"}, "pane": null, "planModeRequired": false,
                          "currentTask": null}]);
    let world = Arc::new(FakeWorld::with(snapshot_json(json!([project_json(&project, "shop\u{1b}[31m", "/w/shop")]), locations, sessions, members)));
    let labels = WorldLabels::new(world).labels(&[worktree_session.clone(), member_session.clone(), plain_session.clone()]);
    let wt = &labels[&worktree_session];
    assert_eq!(wt.project.as_deref(), Some("shop"), "sanitized project name");
    assert_eq!(wt.detail.as_deref(), Some("payment-retry"), "the worktree name, details on only (A48.1)");
    assert_eq!(wt.title.as_deref(), Some("first prompt PRIVATE"), "content kept apart");
    assert_eq!(wt.summary.as_deref(), Some("npm run migrate"));
    let member = &labels[&member_session];
    assert_eq!(member.detail.as_deref(), Some("db-agent"), "the teammate name, details on only (A48.1)");
    assert_eq!(member.summary.as_deref(), Some("rm -rf build"), "the card the lead shows on its behalf");
    // A48.1: two claude sessions of `shop` → project + vendor + ordinal.
    let mut shared = vec![wt.session.as_str(), member.session.as_str()];
    shared.sort_unstable();
    assert_eq!(shared, ["shop · claude 1", "shop · claude 2"]);
    let plain = &labels[&plain_session];
    assert_eq!(plain.session, "codex", "no project, the only codex session: the vendor alone");
    let detail = plain.detail.as_deref().unwrap();
    assert!(detail.starts_with("codex · "), "vendor word + start time: {detail}");
    assert_eq!(detail.len(), "codex · 00:00".len());
    assert_eq!(plain.project, None);
}
