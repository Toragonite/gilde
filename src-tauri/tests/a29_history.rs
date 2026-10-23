//! A29.2: History's Read of a session World does not hold — `history_row`
//! (the row `history_query` builds, `NOT_FOUND{session}`, `notBuilt`, the
//! shared `query` bucket) and `convo_subscribe`'s history fallback (World
//! first; claude → Claude transcript, codex → rollout, cursor/agy →
//! `UNSUPPORTED{noReadableConversation}`; `NOT_FOUND` only when both miss),
//! plus the privacy canary. Owner: T29 (T03's history, T04's resolver).
//!
//! Every tree is a synthesized temp dir; no vendor directory is read and no
//! vendor CLI runs.

mod fixtures;
#[path = "fixtures/transcripts/support.rs"]
mod tsupport;
#[path = "fixtures/vendor/support.rs"]
mod vsupport;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};
use tauri::test::{get_ipc_response, mock_builder, MockRuntime, INVOKE_KEY};
use tauri::webview::InvokeRequest;
use tauri::WebviewWindow;

use gilde_code_lib::commands::registry;
use gilde_code_lib::contract::attention::AttentionQueue;
use gilde_code_lib::contract::common::{AbsPath, Vendor};
use gilde_code_lib::contract::content::{ConvoEvent, ConvoSourceKind};
use gilde_code_lib::contract::error::IpcError;
use gilde_code_lib::contract::ids::{QueryId, SessionUid, ViewId};
use gilde_code_lib::contract::ipc::{DismissWhat, HistoryFilters, HistoryQueryReq, HistoryRow};
use gilde_code_lib::contract::services::{EvidenceSink, IdentityKey, IdentityLock, StorePatchSink, WorldApi};
use gilde_code_lib::contract::session::{Session, SessionDetails};
use gilde_code_lib::contract::store::StoreSnapshot;
use gilde_code_lib::contract::team::TeamAdapter;
use gilde_code_lib::contract::ErrorCode;
use gilde_code_lib::discovery::history::{HistoryService, HistorySources, HistoryState, MemoryUids, UidResolver};
use gilde_code_lib::discovery::{cursor, AccountSpec};
use gilde_code_lib::follow::hub::{ContentHub, HistoryLocation, HistoryLocator, ResolvedSource, SourceResolver, WorldResolver};
use gilde_code_lib::follow::tail::{Follower, TranscriptFormat};

use vsupport::CANARY;

// ---------------------------------------------------------------------------
// the scene: one claude transcript, one codex rollout, one cursor chat
// ---------------------------------------------------------------------------

struct Scene {
    h: vsupport::Home,
    uids: Arc<MemoryUids>,
    svc: Arc<HistoryService>,
    claude_path: PathBuf,
    codex_path: PathBuf,
    cwd: PathBuf,
}

impl Scene {
    fn new() -> Scene {
        let h = vsupport::Home::new();
        let cwd = h.workdir("proj");
        let sid = vsupport::uuid(4_200);
        let claude_path = h.write_transcript(&h.claude, "-proj", &sid, "claude/transcripts/normal.jsonl", &[("CWD", cwd.to_str().unwrap())]);
        let codex = h.home.join(".codex");
        let day = codex.join("sessions/2026/10/21");
        std::fs::create_dir_all(&day).unwrap();
        let cid = vsupport::uuid(4_300);
        let codex_path = day.join(format!("rollout-2026-10-21T13-25-05-{cid}.jsonl"));
        std::fs::write(&codex_path, vsupport::template("codex/rollout.jsonl", &[("SID", &cid), ("CWD", cwd.to_str().unwrap())])).unwrap();
        // One row the rollout renderer turns into a block (the discovery
        // fixture has only rows it skips), with the canary in its text.
        tsupport::append(
            &codex_path,
            format!("{{\"timestamp\":\"2026-10-21T04:25:08.000Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"agent_message\",\"message\":\"{CANARY} codex reply\"}}}}\n").as_bytes(),
        );
        let chats = h.home.join(".cursor/chats");
        let ws = chats.join(cursor::md5_hex(cwd.to_str().unwrap()));
        std::fs::create_dir_all(ws.join("1f2e3d4c-aaaa")).unwrap();
        std::fs::write(ws.join("1f2e3d4c-aaaa/meta.json"), vsupport::template("cursor/meta.json", &[("CWD", cwd.to_str().unwrap())])).unwrap();
        let uids = Arc::new(MemoryUids::default());
        let svc = HistoryService::new(
            HistorySources {
                claude: vec![(h.claude_account(), h.claude.clone())],
                codex: vec![(AccountSpec::new(Vendor::Codex, &codex).id, codex.clone())],
                cursor_chats: Some(chats),
                agy_dir: Some(h.home.join(".gemini/antigravity-cli")),
                home: h.home.clone(),
                app_data: h.app_data.clone(),
            },
            uids.clone(),
            None,
        );
        assert_eq!(svc.refresh_index().total, 3);
        Scene { h, uids, svc, claude_path, codex_path, cwd }
    }

    /// The rows `history_query` returns (newest first), by vendor.
    fn query_rows(&self) -> HashMap<Vendor, HistoryRow> {
        let req = HistoryQueryReq { query_id: QueryId::parse("q-a29").unwrap(), text: String::new(), filters: HistoryFilters::default(), cursor: None, limit: 20 };
        self.svc.query(&req).expect("query").rows.into_iter().map(|r| (r.vendor, r)).collect()
    }

    fn uid_of(&self, vendor: Vendor, sid: &str) -> SessionUid {
        let account = match vendor {
            Vendor::Claude => Some(self.h.claude_account()),
            Vendor::Codex => Some(AccountSpec::new(Vendor::Codex, &self.h.home.join(".codex")).id),
            other => Some(gilde_code_lib::contract::ids::AccountId::derive_default_login(other)),
        };
        self.uids.uid_for(&IdentityKey { vendor, account_id: account, vendor_session_id: sid.to_string() })
    }
}

// ---------------------------------------------------------------------------
// history_row
// ---------------------------------------------------------------------------

#[test]
fn history_row_is_the_row_history_query_builds() {
    let s = Scene::new();
    let rows = s.query_rows();
    assert_eq!(rows.len(), 3);
    for (vendor, row) in &rows {
        assert_eq!(&s.svc.row(&row.uid).expect("found"), row, "{vendor:?}");
    }
}

#[test]
fn history_row_reads_an_unloaded_row_on_demand() {
    // A fresh index: nothing loaded yet, no query ran.
    let s = Scene::new();
    let uid = s.uid_of(Vendor::Claude, &vsupport::uuid(4_200));
    let row = s.svc.row(&uid).expect("found");
    assert_eq!(row.uid, uid);
    assert_eq!(row.vendor, Vendor::Claude);
    assert!(row.started_at.is_some() && row.ended_at.is_some(), "head/tail were read for it");
    assert!(row.project_id.is_some());
    assert_eq!(&row, &s.query_rows()[&Vendor::Claude], "and it is still the query's row");
}

#[test]
fn history_row_is_not_found_for_a_uid_the_index_does_not_know() {
    let s = Scene::new();
    let err = s.svc.row(&SessionUid::mint()).unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
    assert_eq!(err.params["entity"], "session");
    // Known to the uid resolver, unknown to the index.
    let uid = s.uid_of(Vendor::Claude, &vsupport::uuid(9_999));
    let err = s.svc.row(&uid).unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
    assert_eq!(err.params["entity"], "session");
}

#[test]
fn history_row_shares_the_query_rate_bucket() {
    let s = Scene::new();
    let uid = s.query_rows()[&Vendor::Claude].uid.clone();
    let req = |i: usize| HistoryQueryReq { query_id: QueryId::parse(format!("q-{i}")).unwrap(), text: String::new(), filters: HistoryFilters::default(), cursor: None, limit: 1 };
    // Drain the bucket with queries …
    let mut drained = false;
    for i in 0..200 {
        if s.svc.query(&req(i)).is_err() {
            drained = true;
            break;
        }
    }
    assert!(drained, "20/s, burst 20");
    // … and `history_row` is out of tokens too (its own bucket would grant a
    // burst of 20; ten calls take far less than the 500 ms that refill).
    let busy = (0..10).filter(|_| s.svc.row(&uid).is_err_and(|e| e.code == ErrorCode::Busy)).count();
    assert!(busy > 0, "history_row draws from the same bucket");
}

// -- the command through the real IPC layer -----------------------------------

struct App {
    _app: tauri::App<MockRuntime>,
    webview: WebviewWindow<MockRuntime>,
}

fn app(history: Option<Arc<HistoryService>>) -> App {
    let mut builder = mock_builder().invoke_handler(registry::handler());
    if let Some(svc) = history {
        builder = builder.manage(HistoryState(svc));
    }
    let app = builder.build(tauri::generate_context!(test = true)).expect("mock app");
    let webview = tauri::WebviewWindowBuilder::new(&app, "main", Default::default()).build().expect("main window");
    App { _app: app, webview }
}

impl App {
    fn history_row(&self, uid: &SessionUid) -> Result<Value, Value> {
        get_ipc_response(
            &self.webview,
            InvokeRequest {
                cmd: "history_row".into(),
                callback: tauri::ipc::CallbackFn(0),
                error: tauri::ipc::CallbackFn(1),
                url: "tauri://localhost".parse().unwrap(),
                body: tauri::ipc::InvokeBody::Json(json!({ "req": { "uid": uid } })),
                headers: Default::default(),
                invoke_key: INVOKE_KEY.to_string(),
            },
        )
        .map(|b| b.deserialize::<Value>().unwrap_or(Value::Null))
    }
}

#[test]
fn the_history_row_command_answers_found_not_found_and_not_built() {
    let _env = fixtures::Hermetic::new();
    let s = Scene::new();
    let row = s.query_rows()[&Vendor::Codex].clone();

    let a = app(Some(s.svc.clone()));
    let got = a.history_row(&row.uid).expect("found");
    assert_eq!(got, serde_json::to_value(&row).unwrap());
    let err = a.history_row(&SessionUid::mint()).expect_err("unknown");
    assert_eq!(err["code"], "NOT_FOUND");
    assert_eq!(err["params"]["entity"], "session");

    let bare = app(None);
    let err = bare.history_row(&row.uid).expect_err("no service");
    assert_eq!(err["code"], "UNSUPPORTED");
    assert_eq!(err["params"]["cap"], "notBuilt");
}

// ---------------------------------------------------------------------------
// the resolver: World first, then the history index
// ---------------------------------------------------------------------------

/// A World that only answers `session` and `transcript`.
#[derive(Default)]
struct OneWorld {
    sessions: Mutex<Vec<Session>>,
    transcripts: Mutex<HashMap<SessionUid, (AbsPath, Vendor)>>,
}

#[async_trait]
impl WorldApi for OneWorld {
    fn sink(&self) -> Arc<dyn EvidenceSink> {
        unimplemented!("not used by the resolver")
    }
    fn subscribe(&self, _sink: Box<dyn StorePatchSink>) -> StoreSnapshot {
        unimplemented!("not used by the resolver")
    }
    fn snapshot(&self) -> StoreSnapshot {
        unimplemented!("not used by the resolver")
    }
    fn session(&self, uid: &SessionUid) -> Option<Session> {
        self.sessions.lock().unwrap().iter().find(|s| &s.uid == uid).cloned()
    }
    fn queue(&self) -> AttentionQueue {
        unimplemented!("not used by the resolver")
    }
    async fn identity_lock(&self, _key: IdentityKey) -> IdentityLock {
        unimplemented!("not used by the resolver")
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
    fn transcript(&self, uid: &SessionUid) -> Option<(AbsPath, Vendor)> {
        self.transcripts.lock().unwrap().get(uid).cloned()
    }
}

fn watching_claude(uid: &SessionUid) -> Session {
    serde_json::from_value(json!({
        "uid": uid, "vendor": "claude", "accountId": null, "vendorSessionId": null,
        "projectId": null, "locationId": null, "cwd": "/w/demo",
        "control": {"kind": "watching", "origin": "terminal", "owner": null},
        "activity": {"state": {"kind": "working"}, "since": 1, "confidence": "high", "evidence": null, "episode": "e_0123456789abcdef"},
        "title": null, "team": null, "startedAt": null, "endedAt": null, "lastActivityAt": null,
        "pending": [], "queued": [], "model": null, "permissionMode": null, "binary": null,
        "claudeVersion": null, "memoryBytes": null, "usageToday": null,
        "caps": {"send": "none", "interrupt": false, "forceStop": false, "approve": "none", "setModel": false,
                 "setPermissionMode": [], "continueOptions": [], "stopSession": "none", "terminalView": false,
                 "logView": false, "changes": "none", "ownerFocus": false},
        "details": serde_json::to_value(SessionDetails::default()).unwrap(), "mergedInto": null
    }))
    .expect("a session")
}

/// A locator with fixed answers that counts how often it is asked.
#[derive(Default)]
struct FakeLocator {
    answers: HashMap<SessionUid, HistoryLocation>,
    asked: AtomicUsize,
}

impl HistoryLocator for FakeLocator {
    fn locate(&self, uid: &SessionUid) -> Option<HistoryLocation> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        self.answers.get(uid).cloned()
    }
}

fn home() -> Option<AbsPath> {
    AbsPath::parse("/Users/tester").ok()
}

#[test]
fn world_is_asked_first_and_the_history_index_only_for_a_uid_world_does_not_hold() {
    let uid = SessionUid::mint();
    let world = Arc::new(OneWorld::default());
    world.sessions.lock().unwrap().push(watching_claude(&uid));
    world.transcripts.lock().unwrap().insert(uid.clone(), (AbsPath::parse("/w/live.jsonl").unwrap(), Vendor::Claude));
    let mut fake = FakeLocator::default();
    fake.answers.insert(uid.clone(), HistoryLocation::ClaudeTranscript { path: "/w/old.jsonl".into(), cwd: None });
    let locator = Arc::new(fake);
    let resolver = WorldResolver::new(world.clone(), home()).with_history(locator.clone());

    match resolver.resolve(&uid).unwrap() {
        ResolvedSource::File { path, live, .. } => {
            assert_eq!(path, PathBuf::from("/w/live.jsonl"), "World's file, never the index's");
            assert!(live);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(locator.asked.load(Ordering::SeqCst), 0, "World held the session: the index is not asked");

    // World does not hold the uid: the index answers.
    world.sessions.lock().unwrap().clear();
    assert!(matches!(resolver.resolve(&uid).unwrap(), ResolvedSource::File { live: false, .. }));
    assert_eq!(locator.asked.load(Ordering::SeqCst), 1);
}

#[test]
fn the_history_fallback_maps_each_vendor() {
    let world = Arc::new(OneWorld::default());
    let (claude, codex, cursor_uid, agy, unknown) =
        (SessionUid::mint(), SessionUid::mint(), SessionUid::mint(), SessionUid::mint(), SessionUid::mint());
    let mut fake = FakeLocator::default();
    let cwd = AbsPath::parse("/w/proj").ok();
    fake.answers.insert(claude.clone(), HistoryLocation::ClaudeTranscript { path: "/h/a.jsonl".into(), cwd: cwd.clone() });
    fake.answers.insert(codex.clone(), HistoryLocation::CodexRollout { path: "/h/rollout.jsonl".into(), cwd: None });
    fake.answers.insert(cursor_uid.clone(), HistoryLocation::TitleOnly { vendor: Vendor::Cursor });
    fake.answers.insert(agy.clone(), HistoryLocation::TitleOnly { vendor: Vendor::Agy });
    let resolver = WorldResolver::new(world.clone(), home()).with_history(Arc::new(fake));

    match resolver.resolve(&claude).unwrap() {
        ResolvedSource::File { path, format, base, live } => {
            assert_eq!(path, PathBuf::from("/h/a.jsonl"));
            assert_eq!(format, TranscriptFormat::Claude);
            assert!(!live, "History's Read is read-only, never live");
            assert_eq!(base.location, cwd);
            assert_eq!(base.home, home());
        }
        other => panic!("{other:?}"),
    }
    match resolver.resolve(&codex).unwrap() {
        ResolvedSource::File { format, live, .. } => {
            assert_eq!(format, TranscriptFormat::CodexRollout);
            assert!(!live);
        }
        other => panic!("{other:?}"),
    }
    for uid in [&cursor_uid, &agy] {
        let err = resolver.resolve(uid).unwrap_err();
        assert_eq!(err.code, ErrorCode::Unsupported);
        assert_eq!(err.params["cap"], "noReadableConversation");
    }
    let err = resolver.resolve(&unknown).unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound, "neither World nor the index knows it");
    assert_eq!(err.params["entity"], "session");

    // Without a history index the old answer stands: NOT_FOUND.
    let bare = WorldResolver::new(world, home());
    assert_eq!(bare.resolve(&claude).unwrap_err().code, ErrorCode::NotFound);
}

#[test]
fn a_relative_path_from_the_index_is_never_opened() {
    let uid = SessionUid::mint();
    let mut fake = FakeLocator::default();
    fake.answers.insert(uid.clone(), HistoryLocation::ClaudeTranscript { path: "relative/a.jsonl".into(), cwd: None });
    let resolver = WorldResolver::new(Arc::new(OneWorld::default()), home()).with_history(Arc::new(fake));
    let err = resolver.resolve(&uid).unwrap_err();
    assert_eq!(err.code, ErrorCode::Unsupported);
}

#[test]
fn the_history_service_locates_by_the_index_path_and_title_only_vendors_are_unreadable() {
    let s = Scene::new();
    let rows = s.query_rows();
    let cwd = AbsPath::parse(s.cwd.to_string_lossy().into_owned()).ok();
    assert_eq!(
        s.svc.locate(&rows[&Vendor::Claude].uid),
        Some(HistoryLocation::ClaudeTranscript { path: s.claude_path.clone(), cwd: cwd.clone() })
    );
    assert_eq!(s.svc.locate(&rows[&Vendor::Codex].uid), Some(HistoryLocation::CodexRollout { path: s.codex_path.clone(), cwd }));
    assert_eq!(s.svc.locate(&rows[&Vendor::Cursor].uid), Some(HistoryLocation::TitleOnly { vendor: Vendor::Cursor }));
    assert_eq!(s.svc.locate(&SessionUid::mint()), None);
}

/// `convo_subscribe` for a session only the index knows: Reset with the
/// transcript's blocks, source `transcript`, not live.
#[test]
fn convo_subscribe_reads_an_old_session_through_the_history_index() {
    let s = Scene::new();
    let rows = s.query_rows();
    let resolver = WorldResolver::new(Arc::new(OneWorld::default()), home()).with_history(s.svc.clone());
    let hub = ContentHub::new_manual(Follower::manual(tsupport::manual_config()), Some(Arc::new(resolver)));
    for vendor in [Vendor::Claude, Vendor::Codex] {
        let chan = tsupport::Collect::new();
        let res = hub
            .subscribe(&rows[&vendor].uid, &ViewId::parse(format!("view-{}", vendor.as_str())).unwrap(), None, chan.clone())
            .unwrap_or_else(|e| panic!("{vendor:?}: {e:?}"));
        assert_eq!(res.source, ConvoSourceKind::Transcript, "{vendor:?}");
        assert!(res.note.is_none(), "{vendor:?}: no live-follow note for a read-only view");
        let first = chan.events().first().cloned().expect("an event");
        assert!(matches!(first, ConvoEvent::Reset { .. }), "{vendor:?}: the first event is Reset");
        assert!(!tsupport::reset_blocks(&first).is_empty(), "{vendor:?}: the conversation is shown");
    }
    let err = hub
        .subscribe(&rows[&Vendor::Cursor].uid, &ViewId::parse("view-cursor").unwrap(), None, tsupport::Collect::new())
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Unsupported);
    assert_eq!(err.params["cap"], "noReadableConversation");
    let err = hub.subscribe(&SessionUid::mint(), &ViewId::parse("view-none").unwrap(), None, tsupport::Collect::new()).unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
}

// ---------------------------------------------------------------------------
// privacy canary
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct LogBuf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogBuf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuf {
    type Writer = LogBuf;
    fn make_writer(&'a self) -> LogBuf {
        self.clone()
    }
}

fn capture<R>(f: impl FnOnce() -> R) -> (R, String) {
    let buf = LogBuf::default();
    let sub = tracing_subscriber::fmt().with_max_level(tracing::Level::TRACE).with_ansi(false).with_writer(buf.clone()).finish();
    let r = tracing::subscriber::with_default(sub, f);
    let logs = String::from_utf8_lossy(&buf.0.lock().unwrap()).into_owned();
    (r, logs)
}

#[test]
fn t_a29_canary_no_transcript_text_in_a_log_an_error_or_a_row_field_but_the_title() {
    let ((rows, errors, answers), logs) = capture(|| {
        let s = Scene::new();
        let rows = s.query_rows();
        let mut errors: Vec<IpcError> = Vec::new();
        let mut answers: Vec<Value> = Vec::new();
        for row in rows.values() {
            let got = s.svc.row(&row.uid).expect("row");
            answers.push(serde_json::to_value(&got).unwrap());
        }
        errors.push(s.svc.row(&SessionUid::mint()).unwrap_err());
        let resolver = WorldResolver::new(Arc::new(OneWorld::default()), home()).with_history(s.svc.clone());
        let hub = ContentHub::new_manual(Follower::manual(tsupport::manual_config()), Some(Arc::new(resolver)));
        for (vendor, row) in &rows {
            match hub.subscribe(&row.uid, &ViewId::parse(format!("v-{}", vendor.as_str())).unwrap(), None, tsupport::Collect::new()) {
                Ok(res) => answers.push(serde_json::to_value(&res).unwrap()),
                Err(e) => errors.push(e),
            }
        }
        errors.push(hub.subscribe(&SessionUid::mint(), &ViewId::parse("v-none").unwrap(), None, tsupport::Collect::new()).unwrap_err());
        (rows, errors, answers)
    });
    assert!(!logs.contains(CANARY), "transcript text reached a log:\n{logs}");
    assert!(errors.len() >= 2);
    for e in &errors {
        let text = serde_json::to_string(e).unwrap();
        assert!(!text.contains(CANARY), "transcript text in an IpcError: {text}");
    }
    for mut a in answers {
        // A row's title is its one [content] field (memory only, the UI's
        // label); nothing else of the transcript is in an answer.
        if let Some(o) = a.as_object_mut() {
            o.remove("title");
        }
        let text = a.to_string();
        assert!(!text.contains(CANARY), "transcript text in an answer: {text}");
    }
    assert_eq!(rows.len(), 3);
}
