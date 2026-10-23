//! G6 probes of the Conversation transport (contract §14.5, A31.3): SP-2
//! hostile card text, HP-2 protocol misuse, SP-8 (stream half) oversized
//! frames. Each probe runs the real `ConvoDriver` against the scenario-driven
//! `fake_claude` (§14.1) over pipes — the rig of `convo_l1.rs` — with the
//! driver's evidence teed into a real `World`, so a card is checked both as it
//! leaves the core and as the store the WebView mirrors holds it. Each probe
//! prints one `PROBE <id>: …` line.
//!
//! No real vendor CLI runs; the environment is hermetic (temp `HOME` and
//! `GILDE_HOME`, `DISABLE_AUTOUPDATER=1`, `AGENT_CLI_CREDENTIAL_STORE=memory`).
//! The fake supervisor signals only the process group of the fake this test
//! started, only with SIGTERM. Waits are event driven with a generous ceiling;
//! nothing asserts a wall-clock budget (A10.7).
//!
//! Owner: T33.

mod fixtures;
#[path = "fixtures/verify/mod.rs"]
mod verify;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::{json, Value};

use gilde_code_lib::contract::account::Quota;
use gilde_code_lib::contract::activity::{ExitInfo, StopCause};
use gilde_code_lib::contract::common::{AbsPath, CompatState, EpochMs, Notice, PermissionMode, Vendor};
use gilde_code_lib::contract::compat::{CanaryResult, ClaudeCompat, CompatCapability, CompatSource};
use gilde_code_lib::contract::content::{BlockKind, BlockPatch, InteractionOutcome, SystemNote};
use gilde_code_lib::contract::error::IpcError;
use gilde_code_lib::contract::ids::{AccountId, ProcessKey, RequestId, SessionUid};
use gilde_code_lib::contract::interaction::{Decision, OptionRole, PendingInteraction};
use gilde_code_lib::contract::ipc::AnswerOutcome;
use gilde_code_lib::contract::services::{
    CompatApi, CompatSink, ContentHubApi, ContentUpdate, EvidenceSink, EvidenceUpdate, IdentityHint, LocationFacts,
    ProcSnapshot, SessionDriver, SessionMetaFacts, SignalOutcome, SignalPlan, SourceFacts, SpawnRequest, SpawnTicket,
    SupervisorApi, WorldApi,
};
use gilde_code_lib::contract::session::{BinaryFingerprint, BinaryRef, QueuedMessage};
use gilde_code_lib::contract::store::SourceStatus;
use gilde_code_lib::convo::driver::{ConvoDeps, ConvoDriver, ConvoHooks, ConvoSpec, ConvoTiming};
use gilde_code_lib::convo::frames::PatternSource;
use gilde_code_lib::convo::handshake::LaunchKind;
use gilde_code_lib::convo::wire::LINE_CAP;
use gilde_code_lib::convo::Clock;
use gilde_code_lib::world::{MemoryMetaStore, World, WorldConfig};

use verify::{Logs, Probe};

const SID: &str = "5b1e7a0c-2f3d-4c6e-9a8b-0c1d2e3f4a5b";
/// Ordinary waits (a frame, a card).
const CEILING: Duration = Duration::from_secs(60);
/// Waits behind a multi-MiB frame (debug builds parse a 64 MiB line slowly).
const BIG_CEILING: Duration = Duration::from_secs(300);
/// `emitBig n` writes `{"type":"assistant","pad":"` + n × `x` + `"}`: the
/// line (without its `\n`) is `n + EMIT_BIG_OVERHEAD` bytes.
const EMIT_BIG_OVERHEAD: usize = r#"{"type":"assistant","pad":""}"#.len();
const MIB: usize = 1024 * 1024;

/// The probes that stream hundreds of MiB run one at a time (the machine's
/// memory is shared with other builds).
static HEAVY: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// ---------------------------------------------------------------------------
// recording fakes (as convo_l1)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FakeSupervisor {
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
    async fn plan_spawn(&self, _req: SpawnRequest) -> Result<SpawnTicket, IpcError> {
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
}

impl FakeCompat {
    /// The shipped 2.1.306 flags (`claude-compat.json`), as convo_l1.
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
        }
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
    fn handshake(&self, _version: &str, _ok: bool, _note: Option<String>) {}
    fn deviation(&self, _version: &str, _capability: CompatCapability, _note: String) {}
    fn note(&self, _version: &str, _note: String) {}
}

/// Records every evidence update and forwards it to a real `World`.
struct TeeSink {
    rec: Mutex<Vec<EvidenceUpdate>>,
    world: Arc<dyn EvidenceSink>,
}

impl EvidenceSink for TeeSink {
    fn evidence(&self, update: EvidenceUpdate) {
        self.rec.lock().push(update.clone());
        self.world.evidence(update);
    }
    fn location(&self, f: LocationFacts) {
        self.world.location(f);
    }
    fn source_status(&self, s: SourceStatus) {
        self.world.source_status(s);
    }
    fn account(&self, a: gilde_code_lib::contract::account::Account) {
        self.world.account(a);
    }
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

/// The hook log, and the World the mode report goes to (as T10's
/// `SessionHooks` does through its `SessionBinder`, A35.1).
#[derive(Default)]
struct RecHooks(Mutex<Vec<String>>, Mutex<Option<World>>);
impl ConvoHooks for RecHooks {
    fn permission_mode(&self, uid: &SessionUid, mode: Option<PermissionMode>) {
        self.0.lock().push(format!("permissionMode {}", mode.map(PermissionMode::as_cli).unwrap_or("unknown")));
        if let Some(w) = self.1.lock().as_ref() {
            gilde_code_lib::sessions::SessionBinder::set_permission_mode(w, uid, mode);
        }
    }
    fn queued(&self, _uid: &SessionUid, q: Vec<QueuedMessage>) {
        self.0.lock().push(format!("queued {}", q.len()));
    }
    fn quota(&self, _a: &AccountId, _q: Quota) {
        self.0.lock().push("quota".into());
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

/// The real clocks, with an uptime offset a test can advance (past `tooFresh`).
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
    ev: Arc<TeeSink>,
    hub: Arc<RecHub>,
    hooks: Arc<RecHooks>,
    clock: Arc<TestClock>,
    timing: ConvoTiming,
    account: AccountId,
    world: World,
    uid: SessionUid,
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
        let timing = ConvoTiming { drain_ms: 2_000, interrupt_wait_ms: 300, eof_exit_wait_ms: 1_000, ..ConvoTiming::default() };
        let account = AccountId::parse("a_0123456789abcdef").unwrap();
        let world = World::new(WorldConfig::system(Some(h.home().display().to_string()), Arc::new(MemoryMetaStore::default())));
        // What the session manager does before a spawn (identity rule 1).
        let uid = world.bind_spawn("sp_1", None, Vendor::Claude, Some(account.clone()));
        let ev = Arc::new(TeeSink { rec: Mutex::new(Vec::new()), world: world.sink() });
        Rig {
            claude,
            cwd,
            cfg,
            scenario: scenario_path,
            log,
            h,
            sup: Arc::new(FakeSupervisor::default()),
            compat: Arc::new(FakeCompat::probed()),
            ev,
            hub: Arc::new(RecHub::default()),
            hooks: Arc::new(RecHooks(Mutex::new(Vec::new()), Mutex::new(Some(world.clone())))),
            clock: Arc::new(TestClock { base: Instant::now(), offset: AtomicU64::new(0) }),
            timing,
            account,
            world,
            uid,
        }
    }

    fn spec(&self) -> ConvoSpec {
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
            uid: self.uid.clone(),
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
            launch: LaunchKind::New { session_id: SID.into() },
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
        ConvoDriver::spawn(self.spec(), self.deps()).await.expect("spawn")
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

    /// The `control_response` frames Gilde wrote, as their `request_id`s.
    fn answers_written(&self) -> Vec<String> {
        self.stdin_frames()
            .into_iter()
            .filter(|f| f["type"] == "control_response")
            .map(|f| f["response"]["request_id"].as_str().map(str::to_owned).unwrap_or_else(|| f["response"]["request_id"].to_string()))
            .collect()
    }

    fn facts(&self) -> Vec<SourceFacts> {
        self.ev.rec.lock().iter().map(|u| u.facts.clone()).collect()
    }

    fn stream(&self) -> Option<gilde_code_lib::contract::activity::StreamFacts> {
        self.facts().into_iter().rev().find_map(|f| if let SourceFacts::Stream(s) = f { Some(s) } else { None })
    }

    fn metas(&self) -> Vec<SessionMetaFacts> {
        self.facts().into_iter().filter_map(|f| if let SourceFacts::Meta(m) = f { Some(*m) } else { None }).collect()
    }

    /// The cards the driver published last (StreamFacts.pending).
    fn pending(&self) -> Vec<PendingInteraction> {
        self.stream().map(|s| s.pending).unwrap_or_default()
    }

    fn pending_ids(&self) -> Vec<String> {
        self.pending().iter().map(|c| c.request_id.as_str().to_owned()).collect()
    }

    fn card(&self, id: &str) -> Option<PendingInteraction> {
        self.pending().into_iter().find(|c| c.request_id.as_str() == id)
    }

    /// The session row of the real World (what the WebView store mirrors).
    fn world_session(&self) -> Option<gilde_code_lib::contract::session::Session> {
        self.world.flush_now();
        self.world.session(&self.uid)
    }

    fn world_card(&self, id: &str) -> Option<PendingInteraction> {
        self.world_session()?.pending.into_iter().find(|c| c.request_id.as_str() == id)
    }

    fn content(&self) -> Vec<ContentUpdate> {
        self.hub.0.lock().clone()
    }

    fn hooks(&self) -> Vec<String> {
        self.hooks.0.lock().clone()
    }

    /// Interaction blocks appended for `id`.
    fn interaction_blocks(&self, id: &str) -> usize {
        self.content()
            .iter()
            .filter(|u| matches!(u, ContentUpdate::Appended(b) if matches!(&b.kind, BlockKind::Interaction { request_id, .. } if request_id.as_str() == id)))
            .count()
    }

    fn row_too_large_blocks(&self) -> usize {
        self.content()
            .iter()
            .filter(|u| match u {
                ContentUpdate::Appended(b) => matches!(b.kind, BlockKind::System { note: SystemNote::RowTooLarge, .. }),
                ContentUpdate::Updated { patch: BlockPatch::Finalized { block }, .. } => {
                    matches!(block.kind, BlockKind::System { note: SystemNote::RowTooLarge, .. })
                }
                _ => false,
            })
            .count()
    }

    fn withdrawn_patches(&self) -> usize {
        self.content()
            .iter()
            .filter(|u| matches!(u, ContentUpdate::Updated { patch: BlockPatch::Interaction { outcome: InteractionOutcome::Withdrawn }, .. }))
            .count()
    }

    async fn wait_up_to(&self, ceiling: Duration, what: &str, cond: impl Fn(&Rig) -> bool) {
        let until = Instant::now() + ceiling;
        while !cond(self) {
            assert!(Instant::now() < until, "timed out waiting for: {what}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn wait(&self, what: &str, cond: impl Fn(&Rig) -> bool) {
        self.wait_up_to(CEILING, what, cond).await;
    }

    fn advance(&self, ms: u64) {
        self.clock.offset.fetch_add(ms, Ordering::SeqCst);
    }

    /// Answer a card Allow once (past the `tooFresh` window) and wait until
    /// the fake read the answer: the session still works end to end.
    async fn allow_once(&self, d: &mut ConvoDriver, id: &str) {
        self.advance(400);
        assert_eq!(d.answer(&RequestId::parse(id).unwrap(), Decision::AllowOnce).await.unwrap(), AnswerOutcome::Answered, "answer {id}");
        self.wait(&format!("the fake read the answer to {id}"), |r| r.answers_written().iter().any(|a| a == id)).await;
    }
}

// ---------------------------------------------------------------------------
// scenario pieces (synthesized from the R6 shapes, as convo_l1)
// ---------------------------------------------------------------------------

fn handshake_ok() -> Vec<Value> {
    let resp = json!({"claude_code_version":"2.1.306","capabilities":["ui_surface_v1"],"session_state":"idle",
        "current_permission_mode":"default","pid":"${pid}","models":[{"value":"default","displayName":"Default"}],
        "account":{"email":"someone@example.com","organization":"Org","subscriptionType":"max","apiProvider":"firstParty"}});
    vec![
        json!({"expect":{"/type":"control_request","/request/subtype":"initialize"},"as":"init"}),
        json!({"emit":{"type":"control_response","response":{"subtype":"success","request_id":"${init/request_id}",
            "response":resp,"pending_permission_requests":[],"pending_user_dialog_requests":[]}}}),
    ]
}

fn sys_init_with(model: &str, mode: Value) -> Value {
    json!({"emit":{"type":"system","subtype":"init","session_id":"${sessionId}","model":model,"permissionMode":mode,
        "capabilities":["interrupt_receipt_v1","interrupt_cancel_queued_v1","interrupt_send_now_v1","msg_lifecycle_v1"],
        "claude_code_version":"2.1.306","tools":["Bash"],"mcp_servers":[]}})
}

fn sys_init() -> Value {
    sys_init_with("claude-haiku-5-5", json!("default"))
}

/// A `can_use_tool` control request; `extra` is merged into `request`.
fn ask(id: Value, tool: &str, tuid: &str, input: Value, extra: Value) -> Value {
    let mut req = json!({"subtype":"can_use_tool","tool_name":tool,"input":input,"tool_use_id":tuid});
    if let Value::Object(e) = extra {
        for (k, v) in e {
            req[k] = v;
        }
    }
    json!({"emit":{"type":"control_request","request_id":id,"request":req}})
}

/// A Bash ask; `suggestions = None` leaves `permission_suggestions` out.
fn bash_ask(id: &str, tuid: &str, cmd: &str, suggestions: Option<Value>) -> Value {
    let extra = match suggestions {
        Some(s) => json!({"permission_suggestions": s}),
        None => json!({}),
    };
    ask(json!(id), "Bash", tuid, json!({"command":cmd,"description":"Run it"}), extra)
}

/// The forwardable suggestion shape (button 2 = "Always allow \"{cmd}\"").
fn add_rules(cmd: &str) -> Value {
    json!([{"type":"addRules","rules":[{"toolName":"Bash","ruleContent":cmd}],"behavior":"allow","destination":"localSettings"}])
}

fn expect_answer(id: &str, timeout_ms: u64) -> Value {
    json!({"expect":{"/type":"control_response","/response/request_id":id},"timeoutMs":timeout_ms})
}

fn scenario(steps: Vec<Vec<Value>>) -> Value {
    json!({"version":"2.1.306","steps": steps.into_iter().flatten().collect::<Vec<_>>()})
}

// ---------------------------------------------------------------------------
// display hygiene
// ---------------------------------------------------------------------------

/// A character that must never reach display text: ESC and every other C0
/// control but `\n`/`\t`, DEL, C1 (incl. the 8-bit CSI/ST), and the bidi
/// embeddings/overrides/isolates.
fn hostile(c: char) -> bool {
    (c < ' ' && c != '\n' && c != '\t')
        || ('\u{7f}'..='\u{9f}').contains(&c)
        || ('\u{202A}'..='\u{202E}').contains(&c)
        || ('\u{2066}'..='\u{2069}').contains(&c)
}

fn all_strings(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.push(s.clone()),
        Value::Array(a) => a.iter().for_each(|x| all_strings(x, out)),
        Value::Object(o) => o.iter().for_each(|(k, x)| {
            out.push(k.clone());
            all_strings(x, out);
        }),
        _ => {}
    }
}

/// Every string of the serialized card (title, tool, detail lines, option
/// labels, always label, questions) — `Err` names the first hostile one.
fn card_is_clean(card: &PendingInteraction) -> Result<usize, String> {
    let mut s = Vec::new();
    all_strings(&serde_json::to_value(card).unwrap(), &mut s);
    match s.iter().find(|x| x.chars().any(hostile)) {
        Some(bad) => Err(format!("{bad:?}")),
        None => Ok(s.len()),
    }
}

fn detail_texts(card: &PendingInteraction) -> Vec<String> {
    card.detail_lines
        .iter()
        .filter(|l| l.key == "card.detailText")
        .filter_map(|l| l.params.get("text").and_then(Value::as_str).map(str::to_owned))
        .collect()
}

fn option_keys(card: &PendingInteraction) -> Vec<String> {
    card.options.iter().map(|o| o.key.clone()).collect()
}

fn button_two_hidden(card: &PendingInteraction) -> bool {
    card.always_label.is_none() && !card.options.iter().any(|o| o.key == "2" || o.role == OptionRole::AllowSession)
}

// ---------------------------------------------------------------------------
// SP-2: ANSI and bidi in decision_reason and mcp_server.name
// ---------------------------------------------------------------------------

/// SP-2: three `can_use_tool` requests from `fake_claude` carry ANSI (CSI,
/// 8-bit CSI, OSC-8 to BEL, OSC to ST, DCS, BEL) and bidi controls
/// (U+202A–U+202E, U+2066–U+2069) in `decision_reason` and `mcp_server.name`
/// (swapped between the two fields, and — same class, other fields — in the
/// title and the MCP tool name). Every string of the published card, of the
/// World's copy of it and of `interaction_detail` is free of them, and the
/// visible text survives.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_sp2_hostile_card_text_is_stripped_before_display() {
    let mut p = Probe::new("SP-2");
    let logs = Logs::global();
    let mark = logs.mark();

    // A: ANSI in decision_reason, bidi in mcp_server.name.
    let reason_a = "\u{1b}[31mDanger\u{1b}[0m: \u{1b}]8;;https://evil.example/\u{7}click here\u{1b}]8;;\u{7}";
    let server_a = "\u{202E}revres\u{202C}-\u{2066}docs\u{2069}";
    // B: bidi in decision_reason, ANSI (8-bit CSI, OSC to ST, DCS, BEL) in mcp_server.name.
    let reason_b = "\u{202A}left\u{202C} \u{202B}right\u{202C} \u{202D}over\u{202C} \u{202E}gnp.exe\u{202C} \u{2067}rli\u{2069} \u{2068}fsi\u{2069}";
    let server_b = "\u{9b}1;31mred\u{9b}0m \u{1b}]0;title\u{1b}\\srv\u{1b}Pq\u{1b}\\\u{7}";
    // C: both kinds mixed in both fields, plus the title and the tool name.
    let reason_c = "\u{1b}[1m\u{2066}both\u{2069}\u{1b}[0m";
    let server_c = "\u{1b}[5m\u{202E}blink\u{1b}[25m";
    let title_c = "\u{1b}[2J\u{1b}[HClear\u{202E}screen";
    let tool_c = "mcp__s\u{202E}rv__x";

    let rig = Rig::new(scenario(vec![
        handshake_ok(),
        vec![
            sys_init(),
            ask(json!("sp2-a"), "mcp__docs__search", "toolu_sp2a", json!({"query":"hello"}),
                json!({"decision_reason": reason_a, "mcp_server": {"name": server_a}})),
            ask(json!("sp2-b"), "mcp__docs__fetch", "toolu_sp2b", json!({"query":"world"}),
                json!({"decision_reason": reason_b, "mcp_server": {"name": server_b}})),
            ask(json!("sp2-c"), tool_c, "toolu_sp2c", json!({"query":"x"}),
                json!({"decision_reason": reason_c, "mcp_server": {"name": server_c}, "description": title_c})),
            expect_answer("sp2-a", 30_000),
        ],
    ]));
    let mut d = rig.spawn().await;
    rig.wait("three cards", |r| r.pending_ids().len() == 3).await;
    rig.wait("three cards in World", |r| ["sp2-a", "sp2-b", "sp2-c"].iter().all(|id| r.world_card(id).is_some())).await;

    let cases: [(&str, &str, &[&str]); 3] = [
        ("A reason ANSI (CSI, OSC-8/BEL) + server bidi (202E,202C,2066,2069)", "sp2-a", &["Danger: click here", "revres-docs"]),
        (
            "B reason bidi (202A-202E, 2067-2069) + server ANSI (8-bit CSI, OSC/ST, DCS, BEL)",
            "sp2-b",
            &["left right over gnp.exe rli fsi", "red srv"],
        ),
        ("C both kinds in both fields + title + MCP tool name", "sp2-c", &["both", "blink"]),
    ];
    for (variant, id, visible) in cases {
        let card = rig.card(id).unwrap_or_else(|| panic!("card {id}"));
        let world = rig.world_card(id).unwrap_or_else(|| panic!("World card {id}"));
        let texts = detail_texts(&card);
        card_is_clean(&card).unwrap_or_else(|bad| panic!("{id}: hostile text left in the card: {bad}"));
        card_is_clean(&world).unwrap_or_else(|bad| panic!("{id}: hostile text left in World's card: {bad}"));
        assert_eq!(world, card, "{id}: World holds the card as the core published it");
        for v in visible {
            assert!(texts.iter().any(|t| t == v), "{id}: the visible text {v:?} survives in {texts:?}");
        }
        let detail = d.interaction_detail(&RequestId::parse(id).unwrap()).await.unwrap();
        let reason = detail.decision_reason.clone().unwrap_or_default();
        assert!(!reason.chars().any(hostile) && !detail.tool_name.chars().any(hostile) && !detail.input_pretty.chars().any(hostile), "{id}: interaction_detail is clean: {detail:?}");
        assert_eq!(reason, visible[0], "{id}: interaction_detail's decision_reason");
        let extra = if id == "sp2-c" {
            assert_eq!(card.title, "Clearscreen", "the title is stripped");
            let tool = card.tool.clone().expect("a tool summary");
            assert_eq!((tool.tool.as_str(), tool.object.as_deref()), ("mcp__srv__x", Some("srv · x")), "the MCP tool name is stripped");
            assert_eq!(detail.tool_name, "mcp__srv__x");
            format!(", title {:?}, tool {:?}", card.title, tool.tool)
        } else {
            String::new()
        };
        p.saw(variant, format!("stripped: {visible:?}{extra}"));
    }
    // The session goes on: a stripped card is still answerable.
    rig.allow_once(&mut d, "sp2-a").await;
    let log_text = logs.since(mark);
    assert!(!log_text.contains('\u{1b}') && !log_text.chars().any(|c| ('\u{202A}'..='\u{202E}').contains(&c) || ('\u{2066}'..='\u{2069}').contains(&c)), "no raw ESC/bidi reaches the logs");
    p.saw("all", "0 ESC/C0/C1/bidi in every card string, World's copy, interaction_detail, logs; sp2-a answered, CLI read it");
    d.stop(StopCause::User).await.unwrap();
    p.finish();
}

// ---------------------------------------------------------------------------
// HP-2: protocol misuse by fake_claude
// ---------------------------------------------------------------------------

/// HP-2: one session survives every misuse in turn — a duplicate
/// `request_id` (while pending, other content; again after it was answered),
/// `control_cancel_request` for unknown ids (a string never seen, a number),
/// `can_use_tool` without usable suggestions (key absent, `[]`, only
/// non-forwardable kinds), `system/init` with an unknown `permissionMode`
/// (from Default, and from Plan), an over-cap line (300 Mi chars, real size;
/// and a can_use_tool-shaped line over the cap) — and still answers a card at
/// the end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_hp2_protocol_misuse_by_fake_claude() {
    let _heavy = HEAVY.lock().await;
    let mut p = Probe::new("HP-2");
    let big = 300 * MIB; // 300 Mi chars of `x` (1 byte each)
    let card_shaped = LINE_CAP + MIB;
    let rig = Rig::new(scenario(vec![
        handshake_ok(),
        vec![
            sys_init(),
            // Phase 1.
            bash_ask("dup-1", "toolu_d1", "ls", Some(add_rules("ls"))),
            bash_ask("dup-1", "toolu_d2", "rm -rf /tmp/dup", Some(add_rules("rm -rf /tmp/dup"))),
            bash_ask("nosug-1", "toolu_n1", "pwd", None),
            bash_ask("empty-1", "toolu_e1", "whoami", Some(json!([]))),
            bash_ask(
                "nonfwd-1",
                "toolu_f1",
                "id",
                Some(json!([{"type":"setMode","mode":"acceptEdits","destination":"session"},
                    {"type":"addDirectories","directories":["/tmp/x"],"destination":"session"},
                    {"type":"addRules","rules":[{"toolName":"Write","ruleContent":"/tmp/x"}],"behavior":"allow","destination":"session"}])),
            ),
            json!({"emit":{"type":"control_cancel_request","request_id":"ghost-404"}}),
            json!({"emit":{"type":"control_cancel_request","request_id":404}}),
            bash_ask("after-1", "toolu_a1", "date", Some(add_rules("date"))),
            // Phase 2 begins once Gilde answered dup-1.
            expect_answer("dup-1", 60_000),
            bash_ask("dup-1", "toolu_d3", "rm -rf /tmp/again", Some(add_rules("rm -rf /tmp/again"))),
            sys_init_with("claude-marker-a", json!("turbo")),
            json!({"emit":{"type":"system","subtype":"status","permissionMode":"plan"}}),
            sys_init_with("claude-marker-b", json!("hyperPlan")),
            // A can_use_tool whose line is over the cap (no card can come of it).
            json!({"emitPartial": "{\"type\":\"control_request\",\"request_id\":\"big-1\",\"request\":{\"subtype\":\"can_use_tool\",\"tool_name\":\"Bash\",\"tool_use_id\":\"toolu_b1\",\"input\":{\"command\":\""}),
            json!({"emitBig": card_shaped}),
            // 300 Mi chars on one line.
            json!({"emitBig": big}),
            bash_ask("post-1", "toolu_p1", "uptime", Some(add_rules("uptime"))),
            expect_answer("post-1", 600_000),
        ],
    ]));
    let mut d = rig.spawn().await;
    rig.wait("the marker card after the misuse", |r| r.pending_ids().iter().any(|i| i == "after-1")).await;

    // -- duplicate request_id while pending --------------------------------------------------
    let ids = rig.pending_ids();
    assert_eq!(ids, ["dup-1", "nosug-1", "empty-1", "nonfwd-1", "after-1"], "one card per request_id");
    let dup = rig.card("dup-1").unwrap();
    let dup_texts = detail_texts(&dup);
    assert!(dup_texts.iter().any(|t| t == "ls") && !dup_texts.iter().any(|t| t.contains("/tmp/dup")), "the first copy is kept: {dup_texts:?}");
    assert_eq!(dup.always_label.as_deref(), Some("ls"));
    assert_eq!(rig.interaction_blocks("dup-1"), 1, "one Interaction block");
    p.saw("dup request_id while pending (other body)", "deduped: 1 card, 1 block, first kept");

    // -- button 2 ----------------------------------------------------------------------------
    assert_eq!(option_keys(&dup), ["1", "2", "3"], "control: forwardable addRules shows button 2");
    for id in ["nosug-1", "empty-1", "nonfwd-1"] {
        let c = rig.card(id).unwrap();
        assert!(button_two_hidden(&c), "{id}: button 2 hidden: {:?} {:?}", c.always_label, c.options);
        assert_eq!(option_keys(&c), ["1", "3"], "{id}");
    }
    p.saw(
        "no usable suggestions (absent; []; setMode/addDirectories/other-tool only)",
        "button 2 hidden: options [1,3] (control [1,2,3])",
    );

    // -- cancel for unknown ids --------------------------------------------------------------
    let c = d.snapshot().await.unwrap().counters;
    assert_eq!(rig.withdrawn_patches(), 0, "no card withdrawn");
    assert!(c.unknown_ids >= 2, "both cancels counted as unknown ids: {c:?}");
    // Gilde's own writes so far: the answer the test gives next is the only one;
    // stdin is FIFO, so once the fake read it, anything written before is logged.
    rig.allow_once(&mut d, "dup-1").await;
    let frames = rig.stdin_frames();
    let after_init: Vec<&Value> = frames.iter().filter(|f| f["request"]["subtype"] != "initialize").collect();
    assert_eq!(after_init.len(), 1, "nothing but the dup-1 answer was written: {after_init:?}");
    assert_eq!(after_init[0]["response"]["request_id"], "dup-1");
    assert!(!frames.iter().any(|f| f.to_string().contains("ghost-404")), "no reply to the ghost id");
    p.saw(
        "cancel unknown id (\"ghost-404\"; 404)",
        format!("ignored: 5 cards kept, 0 withdrawn, nothing written, unknown_ids={}", c.unknown_ids),
    );

    // -- phase 2: the rest, then the over-cap lines, then the post-1 card ---------------------
    rig.wait_up_to(BIG_CEILING, "the card after the over-cap lines", |r| r.pending_ids().iter().any(|i| i == "post-1")).await;

    // Duplicate after it was answered.
    assert_eq!(rig.interaction_blocks("dup-1"), 1, "no second Interaction block for dup-1");
    assert!(!rig.pending_ids().iter().any(|i| i == "dup-1"), "no new dup-1 card");
    p.saw("dup request_id after answer", "deduped: no card, no 2nd answer");

    // Unknown permissionMode.
    let metas = rig.metas();
    let mode_at = |model: &str| metas.iter().filter(|m| m.model.as_deref() == Some(model)).map(|m| m.permission_mode).collect::<Vec<_>>();
    let a = mode_at("claude-marker-a");
    let b = mode_at("claude-marker-b");
    // A33.3: an unknown mode makes the session's mode unknown (None), never
    // the previous one; a known one in between is applied.
    assert_eq!(a.first().copied().flatten(), None, "init \"turbo\" while the mode was Default: unknown, not default: {a:?}");
    assert!(a.contains(&Some(PermissionMode::Plan)), "control: system/status \"plan\" is applied: {a:?}");
    assert_eq!(b.first().copied().flatten(), None, "init \"hyperPlan\" while the mode was Plan: unknown, not plan: {b:?}");
    p.saw(
        "init permissionMode \"turbo\" (was default) / \"hyperPlan\" (was plan)",
        "the driver publishes the mode as unknown (None) both times; status \"plan\" in between applied",
    );
    // A35.1: the report reaches the session row through the hook.
    let world_mode = rig.world_session().and_then(|s| s.permission_mode);
    assert_eq!(world_mode, None, "World's session row after \"hyperPlan\": unknown, not plan");
    assert!(rig.hooks().iter().any(|h| h == "permissionMode unknown"), "the driver reported the unknown mode");
    p.saw("World row after \"hyperPlan\"", "permissionMode none (A35.1 hook; the composer shows Unknown mode)");

    // Over-cap lines.
    let snap = d.snapshot().await.unwrap();
    let notices: Vec<String> = rig.hooks().into_iter().filter(|h| h.starts_with("notice ")).collect();
    assert_eq!(snap.counters.too_large, 2, "both over-cap lines skipped: {:?}", snap.counters);
    assert_eq!(rig.row_too_large_blocks(), 2, "one RowTooLarge block each");
    assert!(!rig.pending_ids().iter().any(|i| i == "big-1") && rig.interaction_blocks("big-1") == 0, "no card from an over-cap line");
    assert!(!snap.exited);
    p.saw(
        format!("{} Mi-char line (real size, {} MiB cap)", big / MIB, LINE_CAP / MIB),
        format!("dropped; the notice is the System{{RowTooLarge}} block (A33.3), Notice hooks {notices:?}"),
    );
    p.saw(format!("can_use_tool-shaped {} MiB line", card_shaped / MIB), "dropped: RowTooLarge, no card");

    // The session still works after all of it.
    rig.allow_once(&mut d, "post-1").await;
    let answers = rig.answers_written();
    assert_eq!(answers, ["dup-1", "post-1"], "exactly one answer per card");
    p.saw("then", "alive: post-1 card answered, CLI read it");
    d.stop(StopCause::User).await.unwrap();
    p.finish();
}

// ---------------------------------------------------------------------------
// SP-8 (stream half): a 70 MiB frame, a frame at the cap and at cap + 1
// ---------------------------------------------------------------------------

/// SP-8, stream half: at the real 64 MiB cap, a 70 MiB frame and a frame of
/// cap + 1 bytes are skipped with `RowTooLarge`; a frame of exactly the cap
/// is parsed. After each, the next frame (a card) arrives in the core and in
/// World and its answer reaches the CLI.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_sp8_oversized_stream_frames_are_skipped_and_the_stream_goes_on() {
    let _heavy = HEAVY.lock().await;
    let mut p = Probe::new("SP-8");
    let over = 70 * MIB; // the frame's line: 70 MiB + 29 bytes
    let at_cap = LINE_CAP - EMIT_BIG_OVERHEAD; // the line: exactly LINE_CAP bytes
    let cap_plus_one = at_cap + 1;
    let rig = Rig::new(scenario(vec![
        handshake_ok(),
        vec![
            sys_init(),
            json!({"emitBig": over}),
            bash_ask("s8-1", "toolu_1", "echo one", None),
            expect_answer("s8-1", 600_000),
            json!({"emitBig": at_cap}),
            bash_ask("s8-2", "toolu_2", "echo two", None),
            expect_answer("s8-2", 600_000),
            json!({"emitBig": cap_plus_one}),
            bash_ask("s8-3", "toolu_3", "echo three", None),
            expect_answer("s8-3", 600_000),
        ],
    ]));
    let mut d = rig.spawn().await;

    // (variant, the next card, the line's length without `\n`, too_large and
    // RowTooLarge blocks expected so far)
    let variants = [
        ("70 MiB frame", "s8-1", over + EMIT_BIG_OVERHEAD, 1u64),
        ("exactly the cap", "s8-2", at_cap + EMIT_BIG_OVERHEAD, 1),
        ("cap + 1", "s8-3", cap_plus_one + EMIT_BIG_OVERHEAD, 2),
    ];
    assert_eq!(variants[1].2, LINE_CAP, "the at-cap line is exactly the cap");
    for (variant, id, line_len, want) in variants {
        rig.wait_up_to(BIG_CEILING, &format!("the card after the {variant}"), |r| r.pending_ids().iter().any(|i| i == id)).await;
        rig.wait("World shows the card", |r| r.world_card(id).is_some()).await;
        let c = d.snapshot().await.unwrap().counters;
        assert_eq!(c.too_large, want, "{variant}: too_large");
        assert_eq!(rig.row_too_large_blocks() as u64, want, "{variant}: one RowTooLarge block per skipped line");
        assert_eq!(c.unparseable, 0, "{variant}: nothing unparseable");
        rig.allow_once(&mut d, id).await;
        let skipped = line_len > LINE_CAP;
        let what = if skipped { "skipped, RowTooLarge" } else { "parsed, no RowTooLarge" };
        p.saw(format!("{variant} ({line_len} B)"), format!("{what}; next card in core + World, answered"));
    }
    let snap = d.snapshot().await.unwrap();
    assert!(!snap.exited && snap.ready, "the session is alive");
    assert_eq!(rig.answers_written(), ["s8-1", "s8-2", "s8-3"]);
    d.stop(StopCause::User).await.unwrap();
    p.finish();
}
