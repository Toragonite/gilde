//! T-convo-* end to end (contract §6.1, §14.2): the real `ConvoDriver`
//! against the scenario-driven `fake_claude` (§14.1), with recording fakes for
//! the supervisor, compat, World and the ContentHub. Every frame `fake_claude`
//! emits is synthesized here from the R6 shapes (no copied transcripts, F-43).
//! No real vendor CLI runs; the environment is hermetic (`DISABLE_AUTOUPDATER=1`,
//! `AGENT_CLI_CREDENTIAL_STORE=memory`, a temp `HOME`/`GILDE_HOME`).
//!
//! Waits are event driven with a generous ceiling; nothing asserts a
//! wall-clock budget (A10.7). The fake supervisor signals only the process
//! group of the fake this test started, only with SIGTERM.

mod fixtures;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::{json, Value};

use gilde_code_lib::contract::account::Quota;
use gilde_code_lib::contract::activity::{ExitInfo, ProcessFacts, StopCause, TurnOutcome, TurnPhase};
use gilde_code_lib::contract::common::{AbsPath, CompatState, EpochMs, Notice, PermissionMode};
use gilde_code_lib::contract::compat::{CanaryResult, ClaudeCompat, CompatCapability, CompatSource};
use gilde_code_lib::contract::content::{BlockKind, BlockPatch, InteractionOutcome, SystemNote};
use gilde_code_lib::contract::error::{ErrorCode, IpcError};
use gilde_code_lib::contract::ids::{AccountId, ClientMsgId, ProcessKey, RequestId, SessionUid};
use gilde_code_lib::contract::interaction::{Decision, DecisionKind};
use gilde_code_lib::contract::ipc::{AnswerOutcome, InterruptOutcome, SendDelivery, SendPriority};
use gilde_code_lib::contract::process::ProcKind;
use gilde_code_lib::contract::services::{
    CompatApi, CompatSink, ContentHubApi, ContentUpdate, EvidenceSink, EvidenceUpdate, IdentityHint, LocationFacts,
    OutgoingMessage, ProcSnapshot, SessionDriver, SignalOutcome, SignalPlan, SourceFacts, SpawnRequest, SpawnTicket,
    SupervisorApi,
};
use gilde_code_lib::contract::session::{BinaryFingerprint, BinaryRef, QueuedMessage};
use gilde_code_lib::contract::store::SourceStatus;
use gilde_code_lib::convo::driver::{ConvoDeps, ConvoDriver, ConvoHooks, ConvoSpec, ConvoTiming};
use gilde_code_lib::convo::frames::{AbandonedAsk, PatternSource};
use gilde_code_lib::convo::handshake::LaunchKind;
use gilde_code_lib::convo::Clock;

const SID: &str = "0ff2360d-9ebf-4152-ad44-5fb0754c7823";
const CEILING: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// recording fakes
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FakeSupervisor {
    planned: Mutex<Vec<SpawnRequest>>,
    running: Mutex<Vec<i32>>,
    exited: Mutex<Vec<ExitInfo>>,
    signals: Mutex<Vec<(ProcessKey, SignalPlan)>>,
}

fn pid_of(key: &ProcessKey) -> i32 {
    key.as_str().split('_').nth(1).and_then(|p| p.parse().ok()).unwrap_or(-1)
}

fn alive(pid: i32) -> bool {
    pid > 0 && unsafe { libc::kill(pid, 0) } == 0
}

#[async_trait]
impl SupervisorApi for FakeSupervisor {
    async fn spawn_gate(&self) -> Result<(), IpcError> {
        Ok(())
    }
    async fn plan_spawn(&self, req: SpawnRequest) -> Result<SpawnTicket, IpcError> {
        self.planned.lock().push(req);
        Ok(SpawnTicket { spawn_id: "sp_1".into(), env_marker: "mk_1".into() })
    }
    async fn mark_running(&self, _t: &SpawnTicket, pid: i32) -> Result<(), IpcError> {
        self.running.lock().push(pid);
        Ok(())
    }
    async fn mark_exited(&self, _t: &SpawnTicket, exit: ExitInfo) {
        self.exited.lock().push(exit);
    }
    async fn signal(&self, target: &ProcessKey, plan: SignalPlan) -> Result<SignalOutcome, IpcError> {
        self.signals.lock().push((target.clone(), plan));
        let pid = pid_of(target);
        // Only the group of a process this test started (its own group leader).
        assert!(self.running.lock().contains(&pid), "never signal a process the test did not start");
        if unsafe { libc::getpgid(pid) } == pid {
            unsafe {
                libc::killpg(pid, libc::SIGTERM);
            }
        }
        let SignalPlan::Term { grace_ms } = plan else { panic!("Conversation uses Term only") };
        let until = Instant::now() + Duration::from_millis(u64::from(grace_ms));
        while Instant::now() < until && alive(pid) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let exited = !alive(pid);
        Ok(SignalOutcome { exited, survivor: !exited })
    }
    fn is_vendor_cli(&self, _p: &str) -> bool {
        true
    }
    fn redact_argv(&self, argv: &[String]) -> Vec<String> {
        argv.to_vec()
    }
    fn memory_only(&self) -> Option<String> {
        None
    }
    fn last_scan(&self) -> Option<ProcSnapshot> {
        None
    }
}

struct FakeCompat {
    record: ClaudeCompat,
    flags: BTreeMap<String, bool>,
    events: Mutex<Vec<String>>,
}

impl FakeCompat {
    /// The shipped 2.1.306 flags (`claude-compat.json`).
    fn probed() -> FakeCompat {
        let mut flags = BTreeMap::new();
        for f in gilde_code_lib::convo::Flags::NAMES {
            flags.insert(f.to_string(), !matches!(f, "resumeRearm" | "stdinEofCancel" | "ruleAllowMarker"));
        }
        FakeCompat {
            record: ClaudeCompat {
                version: "2.1.306".into(),
                conversation: CompatState::Verified,
                pane_hosting: CompatState::Unverified,
                source: CompatSource::Shipped,
                static_canary: CanaryResult::NotRun,
                handshake_ok: None,
                below_minimum: false,
                note: None,
                features: BTreeMap::new(),
            },
            flags,
            events: Mutex::new(Vec::new()),
        }
    }
    fn events(&self) -> Vec<String> {
        self.events.lock().clone()
    }
}

#[async_trait]
impl CompatApi for FakeCompat {
    fn list(&self) -> Vec<ClaudeCompat> {
        vec![self.record.clone()]
    }
    fn lookup(&self, _v: &str) -> Option<ClaudeCompat> {
        Some(self.record.clone())
    }
    async fn for_binary(&self, _b: &BinaryRef) -> ClaudeCompat {
        self.record.clone()
    }
    fn feature(&self, _v: &str, flag: &str) -> bool {
        self.flags.get(flag).copied().unwrap_or(false)
    }
    fn app_feature(&self, _f: &str) -> bool {
        false
    }
    fn notices(&self, _v: &str) -> Vec<Notice> {
        Vec::new()
    }
    fn schedule_canary(&self, _b: &BinaryRef) {}
}

impl CompatSink for FakeCompat {
    fn handshake(&self, version: &str, ok: bool, note: Option<String>) {
        self.events.lock().push(format!("handshake {version} {ok} {}", note.unwrap_or_default()));
    }
    fn deviation(&self, version: &str, capability: CompatCapability, note: String) {
        self.events.lock().push(format!("deviation {version} {capability:?} {note}"));
    }
    fn note(&self, version: &str, note: String) {
        self.events.lock().push(format!("note {version} {note}"));
    }
}

#[derive(Default)]
struct RecSink(Mutex<Vec<EvidenceUpdate>>);
impl EvidenceSink for RecSink {
    fn evidence(&self, update: EvidenceUpdate) {
        self.0.lock().push(update);
    }
    fn location(&self, _f: LocationFacts) {}
    fn source_status(&self, _s: SourceStatus) {}
    fn account(&self, _a: gilde_code_lib::contract::account::Account) {}
}

#[derive(Default)]
struct RecHub(Mutex<Vec<ContentUpdate>>);
impl ContentHubApi for RecHub {
    fn publish(&self, _uid: &SessionUid, update: ContentUpdate) {
        self.0.lock().push(update);
    }
    fn has_subscribers(&self, _uid: &SessionUid) -> bool {
        false
    }
}

#[derive(Default)]
struct RecHooks(Mutex<Vec<String>>);
impl ConvoHooks for RecHooks {
    fn queued(&self, _uid: &SessionUid, q: Vec<QueuedMessage>) {
        self.0.lock().push(format!("queued {}", q.len()));
    }
    fn quota(&self, _a: &AccountId, q: Quota) {
        self.0.lock().push(format!("quota {:?}", q.windows.first().and_then(|w| w.used_pct)));
    }
    fn signed_out(&self, _a: &AccountId) {
        self.0.lock().push("signedOut".into());
    }
    fn approval_unconfirmed(&self, _uid: &SessionUid, tool: &str) {
        self.0.lock().push(format!("unconfirmed {tool}"));
    }
    fn notice(&self, _uid: &SessionUid, n: Notice) {
        self.0.lock().push(format!("notice {}", n.key));
    }
}

/// The real clocks, with an uptime offset a test can advance.
struct TestClock {
    base: Instant,
    offset: AtomicU64,
}
impl Clock for TestClock {
    fn wall(&self) -> EpochMs {
        gilde_code_lib::store::now_ms() + self.offset.load(Ordering::SeqCst) as i64
    }
    fn mono_ms(&self) -> u64 {
        self.base.elapsed().as_millis() as u64 + self.offset.load(Ordering::SeqCst)
    }
    fn day(&self, at: EpochMs) -> String {
        gilde_code_lib::follow::tail::local_day(at)
    }
}

// ---------------------------------------------------------------------------
// the rig
// ---------------------------------------------------------------------------

struct Rig {
    h: fixtures::Hermetic,
    claude: PathBuf,
    cwd: PathBuf,
    cfg: PathBuf,
    scenario: PathBuf,
    log: PathBuf,
    sup: Arc<FakeSupervisor>,
    compat: Arc<FakeCompat>,
    ev: Arc<RecSink>,
    hub: Arc<RecHub>,
    hooks: Arc<RecHooks>,
    clock: Arc<TestClock>,
    timing: ConvoTiming,
    account: AccountId,
}

impl Rig {
    fn new(scenario: Value) -> Rig {
        let h = fixtures::Hermetic::new();
        let fake = PathBuf::from(env!("CARGO_BIN_EXE_fake_claude"));
        let link = h.install_fixture("claude", &fake);
        let claude = std::fs::canonicalize(link).unwrap();
        let cwd = h.root().join("proj");
        let cfg = h.root().join("cfg");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&cfg).unwrap();
        let cwd = std::fs::canonicalize(cwd).unwrap();
        let cfg = std::fs::canonicalize(cfg).unwrap();
        let scenario_path = h.root().join("scenario.json");
        std::fs::write(&scenario_path, scenario.to_string()).unwrap();
        let log = h.root().join("fake.log");
        let timing = ConvoTiming { drain_ms: 2_000, ..ConvoTiming::default() };
        Rig {
            claude,
            cwd,
            cfg,
            scenario: scenario_path,
            log,
            h,
            sup: Arc::new(FakeSupervisor::default()),
            compat: Arc::new(FakeCompat::probed()),
            ev: Arc::new(RecSink::default()),
            hub: Arc::new(RecHub::default()),
            hooks: Arc::new(RecHooks::default()),
            clock: Arc::new(TestClock { base: Instant::now(), offset: AtomicU64::new(0) }),
            timing,
            account: AccountId::parse("a_0123456789abcdef").unwrap(),
        }
    }

    fn spec(&self, launch: LaunchKind) -> ConvoSpec {
        let mut env: BTreeMap<String, String> = self.h.env().into_iter().collect();
        env.insert("FAKE_CLAUDE_SCENARIO".into(), self.scenario.display().to_string());
        env.insert("FAKE_CLAUDE_LOG".into(), self.log.display().to_string());
        env.insert("CLAUDE_CONFIG_DIR".into(), self.cfg.display().to_string());
        let composer = move |marker: &str| {
            let mut e = env.clone();
            e.insert("GILDE_SPAWN_ID".into(), marker.to_owned());
            e
        };
        ConvoSpec {
            uid: SessionUid::mint(),
            hint: IdentityHint::default(),
            account_id: Some(self.account.clone()),
            config_dir: AbsPath::parse(self.cfg.display().to_string()).ok(),
            binary: BinaryRef {
                realpath: AbsPath::parse(self.claude.display().to_string()).unwrap(),
                fingerprint: BinaryFingerprint { size: 1, mtime_ms: 1, inode: 1 },
                version: Some("2.1.306".into()),
                pidpath: None,
            },
            cwd: AbsPath::parse(self.cwd.display().to_string()).unwrap(),
            home: AbsPath::parse(self.h.home().display().to_string()).ok(),
            launch,
            permission_mode: PermissionMode::Default,
            model: None,
            allow_bypass: false,
            first_prompt: None,
            abandoned: None,
            env: Arc::new(composer),
        }
    }

    fn deps(&self) -> ConvoDeps {
        ConvoDeps {
            supervisor: self.sup.clone(),
            compat: self.compat.clone(),
            compat_sink: self.compat.clone(),
            evidence: self.ev.clone(),
            content: self.hub.clone(),
            hooks: self.hooks.clone(),
            clock: self.clock.clone(),
            patterns: PatternSource::bundled(),
            timing: self.timing.clone(),
        }
    }

    async fn spawn(&self) -> ConvoDriver {
        self.spawn_spec(self.spec(LaunchKind::New { session_id: SID.into() })).await
    }

    async fn spawn_spec(&self, spec: ConvoSpec) -> ConvoDriver {
        ConvoDriver::spawn(spec, self.deps()).await.expect("spawn")
    }

    fn log_entries(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    /// Frames the fake read from stdin, in order.
    fn stdin_frames(&self) -> Vec<Value> {
        self.log_entries().into_iter().filter_map(|e| e.get("in").cloned()).collect()
    }

    fn facts(&self) -> Vec<SourceFacts> {
        self.ev.0.lock().iter().map(|u| u.facts.clone()).collect()
    }

    fn stream(&self) -> Option<gilde_code_lib::contract::activity::StreamFacts> {
        self.facts().into_iter().rev().find_map(|f| if let SourceFacts::Stream(s) = f { Some(s) } else { None })
    }

    fn gilde(&self) -> Option<gilde_code_lib::contract::activity::GildeFacts> {
        self.facts().into_iter().rev().find_map(|f| if let SourceFacts::Gilde(g) = f { Some(g) } else { None })
    }

    fn exited(&self) -> Option<ProcessFacts> {
        self.facts().into_iter().find_map(|f| match f {
            SourceFacts::Process(p @ ProcessFacts::Exited { .. }) => Some(p),
            _ => None,
        })
    }

    fn content(&self) -> Vec<ContentUpdate> {
        self.hub.0.lock().clone()
    }

    fn hooks(&self) -> Vec<String> {
        self.hooks.0.lock().clone()
    }

    async fn wait(&self, what: &str, cond: impl Fn(&Rig) -> bool) {
        let until = Instant::now() + CEILING;
        while !cond(self) {
            assert!(Instant::now() < until, "timed out waiting for: {what}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn advance(&self, ms: u64) {
        self.clock.offset.fetch_add(ms, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// scenario pieces (synthesized from the R6 shapes)
// ---------------------------------------------------------------------------

fn handshake_ok() -> Vec<Value> {
    handshake_with(json!({}), json!([]))
}

fn handshake_with(extra: Value, pending: Value) -> Vec<Value> {
    let mut resp = json!({"claude_code_version":"2.1.306","capabilities":["ui_surface_v1"],"session_state":"idle",
        "current_permission_mode":"default","pid":"${pid}","models":[{"value":"default","displayName":"Default"}],
        "account":{"email":"someone@example.com","organization":"Org","subscriptionType":"max","apiProvider":"firstParty"}});
    if let Value::Object(e) = extra {
        for (k, v) in e {
            resp[k] = v;
        }
    }
    vec![
        json!({"expect":{"/type":"control_request","/request/subtype":"initialize"},"as":"init"}),
        json!({"emit":{"type":"control_response","response":{"subtype":"success","request_id":"${init/request_id}",
            "response":resp,"pending_permission_requests":pending,"pending_user_dialog_requests":[]}}}),
    ]
}

fn sys_init() -> Value {
    json!({"emit":{"type":"system","subtype":"init","session_id":"${sessionId}","model":"claude-haiku-5-5","permissionMode":"default",
        "capabilities":["interrupt_receipt_v1","interrupt_cancel_queued_v1","interrupt_send_now_v1","msg_lifecycle_v1"],
        "claude_code_version":"2.1.306","tools":["Bash"],"mcp_servers":[]}})
}

fn state(s: &str) -> Value {
    json!({"emit":{"type":"system","subtype":"session_state_changed","state":s,"session_id":"${sessionId}"}})
}

fn bash_ask(id: &str, tuid: &str, cmd: &str) -> Value {
    json!({"emit":{"type":"control_request","request_id":id,"request":{"subtype":"can_use_tool","tool_name":"Bash","display_name":"Bash",
        "input":{"command":cmd,"description":"Create empty file"},"description":"Create empty file",
        "permission_suggestions":[
            {"type":"addRules","rules":[{"toolName":"Bash","ruleContent":cmd}],"behavior":"allow","destination":"localSettings"},
            {"type":"addDirectories","directories":["/tmp/x"],"destination":"session"},
            {"type":"setMode","mode":"acceptEdits","destination":"session"}],
        "blocked_path":"/tmp/x","tool_use_id":tuid}}})
}

fn result(extra: Value) -> Value {
    let mut r = json!({"type":"result","subtype":"success","is_error":false,"terminal_reason":"completed","session_id":"${sessionId}",
        "total_cost_usd":0.002,"usage":{"input_tokens":2,"output_tokens":5},"permission_denials":[],"user_message_uuids":null,"num_turns":1});
    if let Value::Object(e) = extra {
        for (k, v) in e {
            r[k] = v;
        }
    }
    json!({ "emit": r })
}

fn scenario(steps: Vec<Vec<Value>>) -> Value {
    json!({"version":"2.1.306","steps": steps.into_iter().flatten().collect::<Vec<_>>()})
}

fn msg(n: u8, text: &str) -> OutgoingMessage {
    OutgoingMessage {
        client_msg_id: ClientMsgId::parse(format!("0192f000-0000-4000-8000-0000000000{n:02x}")).unwrap(),
        text: text.into(),
        priority: SendPriority::Next,
    }
}

fn has_block(updates: &[ContentUpdate], f: impl Fn(&BlockKind) -> bool) -> bool {
    updates.iter().any(|u| match u {
        ContentUpdate::Appended(b) => f(&b.kind),
        ContentUpdate::Updated { patch: BlockPatch::Finalized { block }, .. } => f(&block.kind),
        _ => false,
    })
}

// ---------------------------------------------------------------------------
// T-compat-handshake
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handshake_spawns_the_exact_argv_and_env_through_the_supervisor() {
    let mut rig = Rig::new(scenario(vec![handshake_ok()]));
    rig.timing.eof_exit_wait_ms = 200;
    let mut d = rig.spawn().await;
    rig.wait("ready", |r| r.gilde().is_some_and(|g| g.ready_at.is_some())).await;

    let start = &rig.log_entries()[0];
    let argv: Vec<String> = serde_json::from_value(start["argv"].clone()).unwrap();
    assert_eq!(
        argv.join(" "),
        format!("-p --input-format stream-json --output-format stream-json --verbose --include-partial-messages --replay-user-messages --permission-prompt-tool stdio --permission-prompts host --session-id {SID} --permission-mode default")
    );
    assert_eq!(start["env"]["CLAUDE_CODE_ENTRYPOINT"], "gilde");
    assert_eq!(start["env"]["CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS"], "1");
    assert_eq!(start["env"]["GILDE_SPAWN_ID"], "mk_1", "stamped with the ticket's marker");
    let planned = rig.sup.planned.lock().clone();
    assert_eq!(planned.len(), 1);
    assert_eq!(planned[0].kind, ProcKind::Convo);
    assert_eq!(planned[0].generation, None);
    assert_eq!(planned[0].turn_id, None);
    assert_eq!(planned[0].argv0.as_str(), rig.claude.to_str().unwrap(), "argv[0] is the checked realpath");
    assert!(planned[0].env_keys.contains(&"CLAUDE_CODE_ENTRYPOINT".to_string()));
    assert_eq!(rig.sup.running.lock().len(), 1);
    assert_eq!(rig.compat.events(), vec!["handshake 2.1.306 true ".to_string()]);
    assert!(matches!(rig.content().first(), Some(ContentUpdate::Source { .. })), "Source{{Stream}} first");
    assert!(rig.facts().iter().any(|f| matches!(f, SourceFacts::Process(ProcessFacts::Alive { .. }))));
    let hint = rig.ev.0.lock()[0].hint.clone();
    assert_eq!(hint.vendor_session_id.as_deref(), Some(SID));
    assert_eq!(hint.gilde_spawn_id.as_deref(), Some("sp_1"));
    let initialize = &rig.stdin_frames()[0];
    assert_eq!(initialize["request"], json!({"subtype":"initialize","supportedDialogKinds":[]}));

    // Idle stop: close stdin; the idle CLI exits on EOF; no signal.
    let out = d.stop(StopCause::User).await.unwrap();
    assert!(out.stopped);
    assert_eq!(rig.gilde().unwrap().stop_cause, Some(StopCause::User));
    rig.wait("exit recorded", |r| r.exited().is_some() && !r.sup.exited.lock().is_empty()).await;
    // Well past the EOF wait: still no signal for a process that already exited.
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(rig.sup.signals.lock().is_empty());
    assert!(rig.gilde().unwrap().signals_sent.is_empty());
    // A second stop answers at once.
    assert!(d.stop(StopCause::User).await.unwrap().stopped);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_initialize_error_is_compat_failed_and_the_process_is_stopped() {
    let rig = Rig::new(scenario(vec![vec![
        json!({"expect":{"/request/subtype":"initialize"},"as":"init"}),
        json!({"emit":{"type":"control_response","response":{"subtype":"error","request_id":"${init/request_id}","error":"unsupported option"}}}),
    ]]));
    let _d = rig.spawn().await;
    rig.wait("exit after the compat failure", |r| r.exited().is_some()).await;
    assert_eq!(rig.compat.events()[0], "handshake 2.1.306 false initialize answered with an error");
    let g = rig.gilde().unwrap();
    assert_eq!(g.start_failure, Some(gilde_code_lib::contract::activity::StartFailure::CompatFailed));
    assert_eq!(g.stop_cause, None, "it stays Error·Didn't start, not Stopped");
    assert!(g.ready_at.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_usage_error_naming_our_flag_fails_the_version_but_a_plain_exit_does_not() {
    let rig = Rig::new(json!({"version":"2.1.306","rejectFlag":"--permission-prompts","steps":[]}));
    let _d = rig.spawn().await;
    rig.wait("exit", |r| r.exited().is_some()).await;
    assert!(rig.compat.events()[0].starts_with("handshake 2.1.306 false the CLI rejected --permission-prompts"));

    let rig = Rig::new(scenario(vec![vec![json!({"stderr":"something broke"}), json!({"exitWith":1})]]));
    let _d = rig.spawn().await;
    rig.wait("exit", |r| r.exited().is_some()).await;
    assert!(rig.compat.events().is_empty(), "an exit before initialize is not compat evidence");
    assert!(matches!(rig.exited(), Some(ProcessFacts::Exited { code: Some(1), .. })));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn signed_out_accounts_are_caught_at_initialize() {
    let rig = Rig::new(scenario(vec![handshake_with(json!({"account":{"tokenSource":"none","apiProvider":"firstParty"}}), json!([]))]));
    let mut d = rig.spawn().await;
    rig.wait("signed out", |r| r.hooks().contains(&"signedOut".to_string())).await;
    let s = rig.stream().unwrap();
    assert!(matches!(s.last_turn_end.unwrap().outcome, TurnOutcome::Error(gilde_code_lib::contract::activity::ErrorSub::SignInNeeded)));
    d.stop(StopCause::User).await.unwrap();
}

// ---------------------------------------------------------------------------
// T-convo-golden, T-convo-echoAck, T-ipc-answer-outcomes (driver level)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approvals_go_out_exactly_once_and_the_echo_is_only_an_acknowledgement() {
    let rig = Rig::new(scenario(vec![
        handshake_ok(),
        vec![
            json!({"expect":{"/type":"user"},"as":"u1"}),
            state("running"),
            json!({"emit":{"type":"command_lifecycle","command_uuid":"${u1/uuid}","state":"queued"}}),
            json!({"emit":{"type":"command_lifecycle","command_uuid":"${u1/uuid}","state":"started"}}),
            sys_init(),
            json!({"emit":{"type":"user","message":{"role":"user","content":[{"type":"text","text":"touch it"}]},"uuid":"${u1/uuid}","isReplay":true}}),
            json!({"emit":{"type":"assistant","message":{"id":"msg_1","role":"assistant","content":[{"type":"tool_use","id":"toolu_016x","name":"Bash","input":{"command":"touch lp02-x.txt"}}],"usage":{"input_tokens":2,"output_tokens":16}}}}),
            state("requires_action"),
            bash_ask("req-1", "toolu_016x", "touch lp02-x.txt"),
            json!({"expect":{"/type":"control_response","/response/request_id":"req-1"},"as":"ans"}),
            json!({"emitCaptured":"ans"}),
            state("running"),
            json!({"emit":{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_016x","content":"","is_error":false}]}}}),
            result(json!({"user_message_uuids":["${u1/uuid}"]})),
            state("idle"),
            json!({"expect":{"/request/subtype":"get_usage"},"as":"gu"}),
            json!({"emit":{"type":"control_response","response":{"subtype":"success","request_id":"${gu/request_id}","response":{
                "rate_limits":{"five_hour":{"utilization":37,"resets_at":"2026-10-22T05:00:00Z"},"seven_day":{"utilization":5}},
                "behaviors":{"day":"you edited /Users/x/other-project/a.rs a lot"}}}}}),
        ],
    ]));
    let mut d = rig.spawn().await;
    assert_eq!(d.send(msg(1, "touch it")).await.unwrap(), SendDelivery::Queued, "held until initialize");
    rig.wait("the card", |r| r.stream().is_some_and(|s| !s.pending.is_empty())).await;
    let s = rig.stream().unwrap();
    assert_eq!(s.phase, TurnPhase::RequiresAction);
    assert_eq!(s.pending[0].always_label.as_deref(), Some("touch lp02-x.txt"));
    let req = RequestId::parse("req-1").unwrap();
    assert_eq!(d.answer_with_previous(&RequestId::parse("nope").unwrap(), Decision::AllowOnce).await.unwrap_err().code, ErrorCode::NotFound);
    rig.advance(400); // past the tooFresh window (§8.6)
    assert_eq!(d.answer_with_previous(&req, Decision::AllowSession).await.unwrap(), (AnswerOutcome::Answered, None));
    assert_eq!(d.answer_with_previous(&req, Decision::Deny).await.unwrap(), (AnswerOutcome::AlreadyAnswered, Some(DecisionKind::AllowSession)));
    rig.wait("turn end", |r| r.stream().is_some_and(|s| s.last_turn_end.is_some())).await;
    let q = d.quota().await.unwrap().unwrap();
    assert_eq!(q.windows[0].used_pct, Some(37));
    assert!(!format!("{q:?}").contains("other-project"), "behaviors are dropped at parse time");

    // The exact golden frame, once.
    let answers: Vec<Value> = rig.stdin_frames().into_iter().filter(|f| f["type"] == "control_response").collect();
    assert_eq!(answers.len(), 1, "one response per CLI request id (R4 rule 1)");
    assert_eq!(
        answers[0],
        json!({"type":"control_response","response":{"subtype":"success","request_id":"req-1","response":{
            "behavior":"allow","updatedInput":{"command":"touch lp02-x.txt","description":"Create empty file"},
            "toolUseID":"toolu_016x","decisionClassification":"user_permanent",
            "updatedPermissions":[{"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"touch lp02-x.txt"}],"behavior":"allow","destination":"session"}]}}})
    );
    let user = rig.stdin_frames().into_iter().find(|f| f["type"] == "user").unwrap();
    assert_eq!(user["origin"], json!({"kind":"human"}));
    assert_eq!(user["uuid"], "0192f000-0000-4000-8000-000000000001");
    assert_eq!(rig.stream().unwrap().last_turn_end.unwrap().outcome, TurnOutcome::Ok);
    assert!(rig.hooks().iter().any(|h| h == "quota Some(37)"));
    assert!(rig.content().iter().any(|u| matches!(u, ContentUpdate::Updated { patch: BlockPatch::Interaction { outcome: InteractionOutcome::AllowedForSession }, .. })));
    let snap = d.snapshot().await.unwrap();
    assert!(snap.queued.is_empty());
    assert_eq!(snap.counters.unknown_ids, 0, "the echo was matched to the ledger");
    d.stop(StopCause::User).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_withdrawn_card_answers_withdrawn_and_a_crash_ends_cards() {
    let rig = Rig::new(scenario(vec![
        handshake_ok(),
        vec![
            sys_init(),
            bash_ask("r1", "toolu_1", "ls"),
            json!({"emit":{"type":"control_cancel_request","request_id":"r1"}}),
            bash_ask("r2", "toolu_2", "pwd"),
            json!({"sleepMs":300}),
            json!({"exitWith":3}),
        ],
    ]));
    let mut d = rig.spawn().await;
    rig.wait("exit", |r| r.exited().is_some()).await;
    rig.advance(400);
    assert_eq!(d.answer(&RequestId::parse("r1").unwrap(), Decision::AllowOnce).await.unwrap(), AnswerOutcome::Withdrawn);
    assert_eq!(d.answer(&RequestId::parse("r2").unwrap(), Decision::AllowOnce).await.unwrap(), AnswerOutcome::Withdrawn);
    assert!(matches!(rig.exited(), Some(ProcessFacts::Exited { code: Some(3), .. })));
    let c = rig.content();
    assert!(c.iter().any(|u| matches!(u, ContentUpdate::Updated { patch: BlockPatch::Interaction { outcome: InteractionOutcome::Withdrawn }, .. })));
    assert!(c.iter().any(|u| matches!(u, ContentUpdate::Updated { patch: BlockPatch::Interaction { outcome: InteractionOutcome::SessionEnded }, .. })));
    assert!(matches!(c.last(), Some(ContentUpdate::Ended)));
    assert!(rig.stream().unwrap().pending.is_empty());
    assert_eq!(d.send(msg(2, "x")).await.unwrap_err().code, ErrorCode::Unsupported);
    assert!(rig.gilde().unwrap().stop_cause.is_none(), "a crash is not a Gilde stop");
}

// ---------------------------------------------------------------------------
// T-convo-stopSequence, T-convo-interrupt
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_interrupts_waits_for_the_result_then_closes_stdin() {
    let mut rig = Rig::new(json!({"version":"2.1.306","eofExit":1,"steps":[
        handshake_ok()[0], handshake_ok()[1],
        sys_init(), state("running"), bash_ask("r1", "toolu_1", "touch x"),
        {"expect":{"/request/subtype":"interrupt"},"as":"int"},
        // Stdin must still be open until the turn's result (PM-46).
        {"sleepMs": 300},
        {"assertOpen": true},
        {"emit":{"type":"control_response","response":{"subtype":"success","request_id":"${int/request_id}","response":{"still_queued":[]}}}},
        {"emit":{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user for tool use]"}]}}},
        {"emit":{"type":"result","subtype":"error_during_execution","is_error":true,"terminal_reason":"aborted_tools","permission_denials":[],"total_cost_usd":0.001}},
        {"expectEof":true}
    ]}));
    rig.timing.eof_exit_wait_ms = 200;
    let mut d = rig.spawn().await;
    rig.wait("the card", |r| r.stream().is_some_and(|s| !s.pending.is_empty())).await;
    let out = d.stop(StopCause::User).await.unwrap();
    assert_eq!(out, gilde_code_lib::contract::services::StopOutcome { stopped: true, survivors: 0 });
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(rig.sup.signals.lock().is_empty(), "the CLI exited on EOF; no signal was needed");
    let frames = rig.stdin_frames();
    assert_eq!(frames.last().unwrap()["request"]["subtype"], "interrupt", "interrupt, then EOF — never EOF first (PM-46)");
    assert!(rig.log_entries().iter().any(|e| e.get("eof").is_some()));
    let g = rig.gilde().unwrap();
    assert_eq!(g.stop_cause, Some(StopCause::User));
    assert!(matches!(rig.exited(), Some(ProcessFacts::Exited { code: Some(1), .. })), "exit 1 after an error result = Stopped (stop cause set)");
    assert_eq!(rig.stream().unwrap().last_turn_end.unwrap().outcome, TurnOutcome::Interrupted);
    assert!(rig.content().iter().any(|u| matches!(u, ContentUpdate::Updated { patch: BlockPatch::Interaction { outcome: InteractionOutcome::SessionEnded }, .. })));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn when_eof_does_not_end_the_turn_a_fenced_sigterm_does() {
    let mut rig = Rig::new(json!({"version":"2.1.306","steps":[
        handshake_ok()[0], handshake_ok()[1],
        sys_init(), state("running"), bash_ask("r1", "toolu_1", "touch x"),
        {"expect":{"/request/subtype":"interrupt"}},
        {"expectEof":true, "timeoutMs": 20000},
        // LP-04b: the prompt fails as "stream closed" and the model keeps going.
        {"emit":{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","is_error":true,"content":"Stream closed"}]}}},
        {"sleepMs": 20000},
        {"exitWith": 0}
    ]}));
    rig.timing.interrupt_wait_ms = 200;
    rig.timing.eof_exit_wait_ms = 200;
    let mut d = rig.spawn().await;
    rig.wait("the card", |r| r.stream().is_some_and(|s| !s.pending.is_empty())).await;
    let out = d.stop(StopCause::User).await.unwrap();
    assert!(out.stopped);
    let sigs = rig.sup.signals.lock().clone();
    assert_eq!(sigs.len(), 1);
    assert_eq!(sigs[0].1, SignalPlan::Term { grace_ms: 2_000 });
    assert_eq!(rig.gilde().unwrap().signals_sent, vec![libc::SIGTERM]);
    assert!(matches!(rig.exited(), Some(ProcessFacts::Exited { signal: Some(15), .. })), "SIGTERM only, never SIGKILL");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_process_that_survives_sigterm_is_reported_not_killed() {
    let mut rig = Rig::new(json!({"version":"2.1.306","steps":[
        handshake_ok()[0], handshake_ok()[1],
        {"ignoreTerm": true},
        {"expectEof": true, "timeoutMs": 20000},
        {"sleepMs": 2500},
        {"exitWith": 0}
    ]}));
    rig.timing.eof_exit_wait_ms = 100;
    rig.timing.quit_term_grace_ms = 200;
    let mut d = rig.spawn().await;
    rig.wait("ready", |r| r.gilde().is_some_and(|g| g.ready_at.is_some())).await;
    let out = d.stop(StopCause::Quit).await.unwrap();
    assert_eq!(out, gilde_code_lib::contract::services::StopOutcome { stopped: false, survivors: 1 });
    assert_eq!(rig.sup.signals.lock()[0].1, SignalPlan::Term { grace_ms: 200 }, "quit bounds");
    assert_eq!(d.stop(StopCause::User).await.unwrap().survivors, 1, "a repeated stop reports the survivor at once");
    assert!(!d.force_stop().await.unwrap());
    assert_eq!(rig.sup.signals.lock().len(), 1, "never escalated");
    // The survivor ends by itself later; the exit is still recorded.
    rig.wait("the survivor's own exit", |r| r.exited().is_some()).await;
    assert!(matches!(rig.exited(), Some(ProcessFacts::Exited { code: Some(0), signal: None, .. })));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_then_force_stop_only_after_five_seconds() {
    let rig = Rig::new(scenario(vec![
        handshake_ok(),
        vec![
            sys_init(),
            state("running"),
            json!({"expect":{"/request/subtype":"interrupt","/request/cancel_queued":true},"as":"int"}),
            json!({"emit":{"type":"control_response","response":{"subtype":"success","request_id":"${int/request_id}","response":{"still_queued":[],"cancelled":[]}}}}),
            json!({"sleepMs": 20000}),
        ],
    ]));
    let mut d = rig.spawn().await;
    rig.wait("running", |r| r.stream().is_some_and(|s| s.phase == TurnPhase::Running)).await;
    assert_eq!(d.interrupt(true).await.unwrap(), InterruptOutcome::Interrupting);
    let e = d.force_stop().await.unwrap_err();
    assert_eq!((e.code, e.params["reason"].as_str()), (ErrorCode::Refused, Some("tooEarly")));
    rig.advance(5_000);
    assert!(d.force_stop().await.unwrap());
    assert_eq!(rig.gilde().unwrap().stop_cause, Some(StopCause::ForceStop));
    assert_eq!(rig.sup.signals.lock().len(), 1);
}

// ---------------------------------------------------------------------------
// T-convo-lineCap, T-convo-depth, stdin backpressure
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_and_deep_lines_are_skipped_and_the_stream_goes_on() {
    let mut rig = Rig::new(scenario(vec![
        handshake_ok(),
        vec![
            json!({"emitBig": 3 * 1024 * 1024}),
            json!({"emitDeep": 100}),
            json!({"emitRaw": "this is not json"}),
            json!({"emitPartial": "{\"type\":\"system\",\"subtype\":\"in"}),
            json!({"emitRaw": "it\",\"session_id\":\"x\"}"}),
            state("running"),
        ],
    ]));
    rig.timing.line_cap = 1024 * 1024;
    let mut d = rig.spawn().await;
    rig.wait("running after the junk", |r| r.stream().is_some_and(|s| s.phase == TurnPhase::Running)).await;
    assert!(has_block(&rig.content(), |k| matches!(k, BlockKind::System { note: SystemNote::RowTooLarge, .. })));
    let c = d.snapshot().await.unwrap().counters;
    assert_eq!((c.too_large, c.too_deep, c.unparseable), (1, 1, 1));
    d.stop(StopCause::User).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cli_that_stops_reading_makes_sends_busy() {
    let rig = Rig::new(json!({"version":"2.1.306","pauseAfter":1,"steps":[handshake_ok()[0], handshake_ok()[1]]}));
    let mut d = rig.spawn().await;
    rig.wait("ready", |r| r.gilde().is_some_and(|g| g.ready_at.is_some())).await;
    let mut busy = None;
    for i in 0..200u8 {
        let mut m = msg(i, &"x".repeat(200 * 1024));
        m.client_msg_id = ClientMsgId::parse(format!("0192f000-0000-4000-8000-0000000001{i:02x}")).unwrap();
        match d.send(m).await {
            Ok(_) => {}
            Err(e) => {
                busy = Some(e);
                break;
            }
        }
    }
    let e = busy.expect("the queue fills");
    assert_eq!(e.code, ErrorCode::Busy);
    assert_eq!(e.params["reason"], "stdinFull");
    assert!(rig.hooks().contains(&"notice convo.notReading".to_string()));
    // Stop still works: stdin is closed, the CLI never reads it, a fenced TERM ends it.
    let out = d.stop(StopCause::User).await.unwrap();
    assert!(out.stopped);
}

// ---------------------------------------------------------------------------
// T-convo-rearm, T-convo-unprompted, T-convo-allowSessionGuard
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resume_of_a_waiting_session_shows_the_abandoned_ask() {
    let rig = Rig::new(scenario(vec![handshake_ok()]));
    let mut spec = rig.spec(LaunchKind::Continue { session_id: SID.into() });
    spec.abandoned = Some(AbandonedAsk { tool_use_id: "toolu_old".into(), tool_name: "Bash".into() });
    let mut d = rig.spawn_spec(spec).await;
    rig.wait("ready", |r| r.gilde().is_some_and(|g| g.ready_at.is_some())).await;
    let argv: Vec<String> = serde_json::from_value(rig.log_entries()[0]["argv"].clone()).unwrap();
    assert!(argv.join(" ").contains(&format!("--resume {SID} --permission-mode default")));
    assert!(has_block(&rig.content(), |k| matches!(k, BlockKind::Interaction { request_id, outcome: InteractionOutcome::Abandoned, .. } if request_id.as_str() == "ab_toolu_old")), "A19.2");
    assert!(rig.stdin_frames().iter().any(|f| f["request"]["subtype"] == "get_usage"), "the cost baseline is asked after a resume [A19.5]");
    assert_eq!(rig.stream().unwrap().phase, TurnPhase::Idle, "Ready, nothing waits anywhere");
    d.stop(StopCause::User).await.unwrap();

    // Any pending request the response does carry still becomes a card.
    let pending = json!([{"type":"control_request","request_id":"p1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"},"tool_use_id":"toolu_p"}}]);
    let rig = Rig::new(scenario(vec![handshake_with(json!({}), pending)]));
    let mut d = rig.spawn_spec(rig.spec(LaunchKind::Continue { session_id: SID.into() })).await;
    rig.wait("re-armed card", |r| r.stream().is_some_and(|s| s.pending.len() == 1)).await;
    d.stop(StopCause::User).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unprompted_turn_runs_and_ends_like_any_other() {
    let rig = Rig::new(scenario(vec![
        handshake_ok(),
        vec![
            json!({"emit":{"type":"system","subtype":"task_notification","task_id":"bq1","tool_use_id":"toolu_bg","status":"completed","summary":"done"}}),
            state("running"),
            sys_init(),
            json!({"emit":{"type":"stream_event","event":{"type":"message_start","message":{"id":"msg_u","usage":{"input_tokens":3,"output_tokens":1}}}}}),
            json!({"emit":{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}}),
            json!({"emit":{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"The background job finished."}}}}),
            json!({"emit":{"type":"assistant","message":{"id":"msg_u","role":"assistant","content":[{"type":"text","text":"The background job finished."}],"usage":{"input_tokens":3,"output_tokens":1}}}}),
            json!({"emit":{"type":"stream_event","event":{"type":"content_block_stop","index":0}}}),
            json!({"emit":{"type":"stream_event","event":{"type":"message_delta","usage":{"output_tokens":9}}}}),
            result(json!({"user_message_uuids":null})),
            state("idle"),
        ],
    ]));
    let mut d = rig.spawn().await;
    rig.wait("turn end", |r| r.stream().is_some_and(|s| s.last_turn_end.is_some() && s.phase == TurnPhase::Idle)).await;
    let s = rig.stream().unwrap();
    assert!(s.last_turn_start_at.is_some());
    assert_eq!(s.last_turn_end.unwrap().outcome, TurnOutcome::Ok);
    assert!(has_block(&rig.content(), |k| matches!(k, BlockKind::AssistantText { markdown, streaming: false, .. } if markdown == "The background job finished.")));
    d.stop(StopCause::User).await.unwrap();
    rig.wait("usage", |r| r.facts().iter().any(|f| matches!(f, SourceFacts::Usage(u) if u.today.output_tokens == Some(9)))).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_settings_file_written_after_always_allow_turns_the_feature_off() {
    let rig = Rig::new(json!({}));
    let settings = rig.cwd.join(".claude").join("settings.local.json");
    let s = scenario(vec![
        handshake_ok(),
        vec![
            sys_init(),
            bash_ask("r1", "toolu_1", "ls"),
            json!({"expect":{"/type":"control_response","/response/request_id":"r1"}}),
            json!({"touchFile": settings.display().to_string()}),
        ],
    ]);
    std::fs::write(&rig.scenario, s.to_string()).unwrap();
    let mut d = rig.spawn().await;
    rig.wait("the card", |r| r.stream().is_some_and(|s| !s.pending.is_empty())).await;
    rig.advance(400);
    d.answer(&RequestId::parse("r1").unwrap(), Decision::AllowSession).await.unwrap();
    rig.wait("the guard", |r| r.compat.events().iter().any(|e| e.contains("Feature(\"allowSession\")"))).await;
    assert!(!d.snapshot().await.unwrap().allow_session);
    d.stop(StopCause::User).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_two_mib_write_can_still_be_approved() {
    // A19.4: the decision echoes the input verbatim, so it may exceed 1 MiB.
    let content = "y".repeat(2 * 1024 * 1024);
    let rig = Rig::new(scenario(vec![
        handshake_ok(),
        vec![
            sys_init(),
            json!({"emit":{"type":"control_request","request_id":"w1","request":{"subtype":"can_use_tool","tool_name":"Write",
                "input":{"file_path":"/tmp/big.txt","content":content},"tool_use_id":"toolu_w",
                "permission_suggestions":[{"type":"setMode","mode":"acceptEdits","destination":"session"}]}}}),
            json!({"expect":{"/type":"control_response","/response/request_id":"w1"},"timeoutMs":20000}),
        ],
    ]));
    let mut d = rig.spawn().await;
    rig.wait("the card", |r| r.stream().is_some_and(|s| !s.pending.is_empty())).await;
    assert!(rig.stream().unwrap().pending[0].always_label.is_none(), "Write: no button 2");
    rig.advance(400);
    assert_eq!(d.answer(&RequestId::parse("w1").unwrap(), Decision::AllowOnce).await.unwrap(), AnswerOutcome::Answered);
    rig.wait("the fake read the answer", |r| r.stdin_frames().iter().any(|f| f["type"] == "control_response")).await;
    let ans = rig.stdin_frames().into_iter().find(|f| f["type"] == "control_response").unwrap();
    assert_eq!(ans["response"]["response"]["updatedInput"]["content"].as_str().map(str::len), Some(2 * 1024 * 1024));
    assert_eq!(ans["response"]["response"]["behavior"], "allow");
    d.stop(StopCause::User).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn below_minimum_or_failed_versions_are_refused_before_any_spawn() {
    let mut rig = Rig::new(scenario(vec![handshake_ok()]));
    let mut c = FakeCompat::probed();
    c.record.below_minimum = true;
    rig.compat = Arc::new(c);
    let e = ConvoDriver::spawn(rig.spec(LaunchKind::New { session_id: SID.into() }), rig.deps()).await.err().unwrap();
    assert_eq!(e.params["cap"], "conversationMinVersion");
    let mut c = FakeCompat::probed();
    c.record.conversation = CompatState::Failed;
    rig.compat = Arc::new(c);
    let e = ConvoDriver::spawn(rig.spec(LaunchKind::New { session_id: SID.into() }), rig.deps()).await.err().unwrap();
    assert_eq!(e.params["cap"], "conversation");
    assert!(rig.sup.planned.lock().is_empty(), "nothing planned, nothing spawned");
    let _ = Path::new("/");
}
