//! T-sessions-* through the production transports (contract §6.0–§6.2): the
//! sessions manager with `LiveTransports` — T05's real `ConvoDriver` against
//! `fake_claude`, T06's real `PtyManager`/`PtyDriver` against `fake_agent` —
//! T09's real `Supervisor` (in-memory registry) and its real `compose_env`.
//! World and the other services are recording fakes.
//!
//! No real vendor CLI runs; the environment is hermetic
//! (`DISABLE_AUTOUPDATER=1`, `AGENT_CLI_CREDENTIAL_STORE=memory`, a temp
//! `HOME`/`GILDE_HOME`). Every signal goes through the supervisor's identity
//! fence to a process this test started; nothing is SIGKILLed. Waits are
//! event driven with a generous ceiling (A10.7).

mod fixtures;

#[path = "fixtures/fake_agent/pty_harness.rs"]
mod harness;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::json;

use gilde_code_lib::contract::account::{Account, AccountSource, Quota, SignIn};
use gilde_code_lib::contract::activity::{ProcessFacts, StopCause};
use gilde_code_lib::contract::attention::AttentionQueue;
use gilde_code_lib::contract::common::{AbsPath, CompatState, EpochMs, Notice, Vendor};
use gilde_code_lib::contract::compat::{CanaryResult, ClaudeCompat, CompatCapability, CompatSource};
use gilde_code_lib::contract::error::IpcError;
use gilde_code_lib::contract::ids::{AccountId, ClientMsgId, ClientRequestId, LocationId, ProjectId, QuitRecordId, SessionUid};
use gilde_code_lib::contract::ipc::{
    DismissWhat, SendPriority, SessionStartReq, SessionStopReq, StartLocation, StopMethod, TurnSendReq,
    VendorOption,
};
use gilde_code_lib::contract::project::{Location, LocationKind, Project, ProjectKind};
use gilde_code_lib::contract::services::{
    CompatApi, CompatSink, ContentHubApi, ContentUpdate, EvidenceSink, EvidenceUpdate, IdentityKey, IdentityLock, LocationFacts,
    SessionActorApi, SidecarApi, SourceFacts, StorePatchSink, SupervisorApi, TurnEventSink, WorldApi,
};
use gilde_code_lib::contract::session::{BinaryRef, ControlMode, QueuedMessage, Session, Transport};
use gilde_code_lib::contract::sidecar::{AccountEnv, AuthStatus, ModelInfo, ProbeParams, TurnStartParams};
use gilde_code_lib::contract::store::{BootStatus, IntegrationsState, OrchestratorPresence, StoreSnapshot};
use gilde_code_lib::contract::team::TeamAdapter;
use gilde_code_lib::convo::driver::{ConvoDeps, ConvoTiming, NoHooks};
use gilde_code_lib::convo::frames::PatternSource;
use gilde_code_lib::convo::SystemClock;
use gilde_code_lib::sessions::manager::{
    AccountsPort, Binaries, ControlFacts, DriverDetails, EnvSource, LiveProcess, Liveness, QuitItems, ResolvedVendor,
    SessionBinder, SessionDeps, SessionTiming, SessionsManager,
};
use gilde_code_lib::sessions::options::MemoryPrefs;
use gilde_code_lib::sessions::start::{LiveTransports, SupervisorEnv};
use gilde_code_lib::store::models::QuitItemRow;
use gilde_code_lib::supervise::registry::EnvBase;
use gilde_code_lib::turns::driver::{NoContent, TurnDeps};

use harness::{Harness, CEILING};

const VERSION: &str = "2.1.306";

async fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + CEILING;
    while !f() {
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn abs(p: &Path) -> AbsPath {
    AbsPath::parse(p.to_string_lossy().into_owned()).unwrap()
}

// ---------------------------------------------------------------------------
// recording fakes (World, compat, accounts, …)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct WorldState {
    projects: Mutex<Vec<Project>>,
    locations: Mutex<Vec<Location>>,
    sessions: Mutex<HashMap<SessionUid, Session>>,
    evidence: Mutex<Vec<EvidenceUpdate>>,
    binds: Mutex<Vec<(String, SessionUid)>>,
    controls: Mutex<HashMap<SessionUid, ControlFacts>>,
}

struct FakeWorld(Arc<WorldState>);
struct Sink(Arc<WorldState>);

impl EvidenceSink for Sink {
    fn evidence(&self, update: EvidenceUpdate) {
        self.0.evidence.lock().push(update);
    }
    fn location(&self, _facts: LocationFacts) {}
    fn source_status(&self, _status: gilde_code_lib::contract::store::SourceStatus) {}
    fn account(&self, _account: Account) {}
}

#[async_trait]
impl WorldApi for FakeWorld {
    fn sink(&self) -> Arc<dyn EvidenceSink> {
        Arc::new(Sink(self.0.clone()))
    }
    fn subscribe(&self, _sink: Box<dyn StorePatchSink>) -> StoreSnapshot {
        self.snapshot()
    }
    fn snapshot(&self) -> StoreSnapshot {
        StoreSnapshot {
            rev: 1,
            projects: self.0.projects.lock().clone(),
            locations: self.0.locations.lock().clone(),
            sessions: self.0.sessions.lock().values().cloned().collect(),
            teams: Vec::new(),
            members: Vec::new(),
            tasks: Vec::new(),
            messages: Vec::new(),
            queue: AttentionQueue { rev: 0, items: Vec::new(), need_you: 0, also: 0, paused_until: None },
            accounts: Vec::new(),
            leftovers: Vec::new(),
            compat: Vec::new(),
            notices: Vec::new(),
            boot: BootStatus { spawn_gate_open: true, first_discovery_done: true, agents_detected: true, memory_only: None, agents: Vec::new(), agents_scanned_at: None, recovery: None },
            sources: Vec::new(),
            integrations: IntegrationsState { orchestrator: OrchestratorPresence::Absent },
        }
    }
    fn session(&self, uid: &SessionUid) -> Option<Session> {
        self.0.sessions.lock().get(uid).cloned()
    }
    fn queue(&self) -> AttentionQueue {
        AttentionQueue { rev: 0, items: Vec::new(), need_you: 0, also: 0, paused_until: None }
    }
    async fn identity_lock(&self, _key: IdentityKey) -> IdentityLock {
        Arc::new(tokio::sync::Mutex::new(())).lock_owned().await
    }
    fn mark_seen(&self, _uid: &SessionUid) -> Result<(), IpcError> {
        Ok(())
    }
    fn dismiss(&self, _uid: &SessionUid, _what: DismissWhat) -> Result<(), IpcError> {
        Ok(())
    }
    fn queue_later(&self, _uid: &SessionUid) -> Result<EpochMs, IpcError> {
        Ok(0)
    }
    fn register_team_adapter(&self, _adapter: Arc<dyn TeamAdapter>) {}
}

impl SessionBinder for FakeWorld {
    fn bind_spawn(&self, spawn_id: &str, uid: Option<&SessionUid>, _v: Vendor, _a: Option<AccountId>) -> SessionUid {
        let uid = uid.cloned().unwrap_or_else(SessionUid::mint);
        self.0.binds.lock().push((spawn_id.to_string(), uid.clone()));
        uid
    }
    fn set_control_detail(&self, uid: &SessionUid, detail: ControlFacts) {
        self.0.controls.lock().insert(uid.clone(), detail);
    }
    fn set_queued(&self, _uid: &SessionUid, _list: Vec<QueuedMessage>) {}
    fn set_details_extra(&self, _uid: &SessionUid, _extra: DriverDetails) {}
}

struct Compat;

#[async_trait]
impl CompatApi for Compat {
    fn list(&self) -> Vec<ClaudeCompat> {
        vec![record()]
    }
    fn lookup(&self, _v: &str) -> Option<ClaudeCompat> {
        Some(record())
    }
    async fn for_binary(&self, _b: &BinaryRef) -> ClaudeCompat {
        record()
    }
    fn feature(&self, _v: &str, _f: &str) -> bool {
        true
    }
    fn app_feature(&self, _f: &str) -> bool {
        false
    }
    fn notices(&self, _v: &str) -> Vec<Notice> {
        Vec::new()
    }
    fn schedule_canary(&self, _b: &BinaryRef) {}
}

impl CompatSink for Compat {
    fn handshake(&self, _version: &str, _ok: bool, _note: Option<String>) {}
    fn deviation(&self, _version: &str, _capability: CompatCapability, _note: String) {}
    fn note(&self, _version: &str, _note: String) {}
}

fn record() -> ClaudeCompat {
    ClaudeCompat {
        version: VERSION.into(),
        conversation: CompatState::Verified,
        pane_hosting: CompatState::Unverified,
        source: CompatSource::Shipped,
        static_canary: CanaryResult::NotRun,
        handshake_ok: Some(true),
        below_minimum: false,
        note: None,
        features: BTreeMap::new(),
    }
}

struct Hub;

impl ContentHubApi for Hub {
    fn publish(&self, _uid: &SessionUid, _update: ContentUpdate) {}
    fn has_subscribers(&self, _uid: &SessionUid) -> bool {
        false
    }
}

struct NoSidecar;

#[async_trait]
impl SidecarApi for NoSidecar {
    async fn ensure_started(&self) -> Result<(), IpcError> {
        Err(IpcError::sidecar_down())
    }
    async fn auth_status(&self, _p: ProbeParams) -> Result<AuthStatus, IpcError> {
        Err(IpcError::sidecar_down())
    }
    async fn models(&self, _p: ProbeParams) -> Result<Vec<ModelInfo>, IpcError> {
        Err(IpcError::sidecar_down())
    }
    async fn quota(&self, _p: ProbeParams) -> Result<Option<Quota>, IpcError> {
        Err(IpcError::sidecar_down())
    }
    async fn turn_start(&self, _p: TurnStartParams, _s: Arc<dyn TurnEventSink>) -> Result<(), IpcError> {
        Err(IpcError::sidecar_down())
    }
    async fn turn_cancel(&self, _t: &str) -> Result<bool, IpcError> {
        Ok(false)
    }
}

struct Bins(HashMap<Vendor, ResolvedVendor>);

#[async_trait]
impl Binaries for Bins {
    async fn installed(&self) -> Result<Vec<VendorOption>, IpcError> {
        Ok(self.0.keys().map(|v| VendorOption { vendor: *v, installed: true, version: None }).collect())
    }
    async fn resolve(&self, vendor: Vendor) -> Result<ResolvedVendor, IpcError> {
        self.0.get(&vendor).cloned().ok_or_else(|| IpcError::unsupported("vendorNotInstalled"))
    }
}

struct Accounts(Vec<Account>);

#[async_trait]
impl AccountsPort for Accounts {
    async fn all(&self) -> Vec<Account> {
        self.0.clone()
    }
    async fn env_for(&self, id: &AccountId) -> Option<AccountEnv> {
        let a = self.0.iter().find(|a| &a.id == id)?;
        let key = match (a.vendor, a.source) {
            (_, AccountSource::Ambient) => None,
            (Vendor::Claude, _) => Some("CLAUDE_CONFIG_DIR".to_string()),
            (Vendor::Codex, _) => Some("CODEX_HOME".to_string()),
            _ => None,
        };
        Some(AccountEnv { account_id: a.id.clone(), vendor: a.vendor, env_key: key, config_dir: a.config_dir.clone() })
    }
    fn observed_quota(&self, _id: &AccountId, _q: Quota) {}
    fn signed_out(&self, _id: &AccountId) {}
}

/// T09's real `compose_env` (via `SupervisorEnv`) plus the fixtures' own
/// control variables, which no allowlist would pass.
struct TestEnv {
    real: SupervisorEnv,
    extra: Vec<(String, String)>,
}

impl EnvSource for TestEnv {
    fn compose(&self, vendor: Vendor, account: Option<&AccountEnv>, marker: &str) -> BTreeMap<String, String> {
        let mut env = self.real.compose(vendor, account, marker);
        for (k, v) in &self.extra {
            env.insert(k.clone(), v.clone());
        }
        env
    }
    fn home(&self) -> Option<AbsPath> {
        self.real.home()
    }
}

struct NoLiveness;

#[async_trait]
impl Liveness for NoLiveness {
    async fn find(&self, _v: Vendor, _id: &str) -> Result<Vec<LiveProcess>, IpcError> {
        Ok(Vec::new())
    }
    fn alive(&self, _pid: i32, _start: i64) -> bool {
        false
    }
}

struct NoQuit;

#[async_trait]
impl QuitItems for NoQuit {
    async fn items(&self, _r: &QuitRecordId) -> Result<Vec<QuitItemRow>, IpcError> {
        Ok(Vec::new())
    }
}

// ---------------------------------------------------------------------------
// the rig
// ---------------------------------------------------------------------------

struct Live {
    h: Harness,
    m: Arc<SessionsManager>,
    world: Arc<WorldState>,
    project: ProjectId,
}

fn account(id: &str, vendor: Vendor, source: AccountSource, dir: Option<&Path>) -> Account {
    Account {
        id: AccountId::parse(id).unwrap(),
        vendor,
        config_dir: dir.map(abs),
        label: "Test".into(),
        is_default: true,
        source,
        usable_for_new: true,
        sign_in: SignIn::SignedIn,
        quota: None,
        live_sessions: 0,
    }
}

fn binary_ref(path: &AbsPath, version: &str) -> BinaryRef {
    let meta = std::fs::metadata(path.as_str()).unwrap();
    use std::os::unix::fs::MetadataExt as _;
    BinaryRef {
        realpath: path.clone(),
        fingerprint: gilde_code_lib::contract::session::BinaryFingerprint { size: meta.len(), mtime_ms: meta.mtime() * 1000, inode: meta.ino() },
        version: Some(version.into()),
        pidpath: None,
    }
}

const CLAUDE_WORK: &str = "a_00000000000000c2";
const CODEX: &str = "a_00000000000000d1";

impl Live {
    async fn new(bins: HashMap<Vendor, ResolvedVendor>, extra_env: Vec<(String, String)>, cfg: &Path) -> Live {
        let h = Harness::new().await;
        let world = Arc::new(WorldState::default());
        let cwd = std::fs::canonicalize(&h.cwd).unwrap();
        let project = ProjectId::derive(cwd.to_str().unwrap());
        world.projects.lock().push(Project {
            id: project.clone(),
            name: "project".into(),
            root: abs(&cwd),
            kind: ProjectKind::Folder,
            order: 0,
            folded: false,
            earlier: false,
            last_activity_at: None,
            live_sessions: 0,
            is_temp: true,
        });
        world.locations.lock().push(Location {
            id: LocationId::derive(cwd.to_str().unwrap()),
            project_id: project.clone(),
            path: abs(&cwd),
            kind: LocationKind::Folder,
            branch: None,
            worktree_name: None,
            gilde_created: false,
            exists: true,
        });
        let fw = Arc::new(FakeWorld(world.clone()));
        let api: Arc<dyn SupervisorApi> = h.supervisor.clone();
        let hub: Arc<dyn ContentHubApi> = Arc::new(Hub);
        let compat = Arc::new(Compat);
        let convo = ConvoDeps {
            supervisor: api.clone(),
            compat: compat.clone(),
            compat_sink: compat.clone(),
            evidence: fw.sink(),
            content: hub.clone(),
            hooks: Arc::new(NoHooks),
            clock: Arc::new(SystemClock),
            patterns: PatternSource::bundled(),
            timing: ConvoTiming { drain_ms: 2_000, ..ConvoTiming::default() },
        };
        let sidecar: Arc<dyn SidecarApi> = Arc::new(NoSidecar);
        let transports = LiveTransports {
            convo,
            pty: h.manager.clone(),
            supervisor: api.clone(),
            turns: TurnDeps::new(sidecar.clone(), fw.sink(), hub, Arc::new(NoContent)),
            sidecar: Some(sidecar),
        };
        let path: String = h.hermetic.env().into_iter().find(|(k, _)| k == "PATH").map(|(_, v)| v).unwrap_or_default();
        let mut base = EnvBase::new(path, h.hermetic.home());
        base.lang = Some("en_US.UTF-8".into());
        let env = TestEnv { real: SupervisorEnv::new(h.supervisor.clone(), base), extra: extra_env };
        let deps = SessionDeps {
            world: fw.clone(),
            binder: fw,
            supervisor: api,
            transports: Arc::new(transports),
            binaries: Arc::new(Bins(bins)),
            accounts: Arc::new(Accounts(vec![
                account(CLAUDE_WORK, Vendor::Claude, AccountSource::User, Some(cfg)),
                account(CODEX, Vendor::Codex, AccountSource::Ambient, None),
            ])),
            env: Arc::new(env),
            compat,
            liveness: Arc::new(NoLiveness),
            prefs: Arc::new(MemoryPrefs::default()),
            quit_items: Arc::new(NoQuit),
            git: None,
            panehost: None,
            sidecar: None,
            clock: Arc::new(gilde_code_lib::store::now_ms),
            timing: SessionTiming { liveness_poll: Duration::from_millis(50), ..SessionTiming::default() },
        };
        Live { h, m: SessionsManager::new(deps), world, project }
    }

    fn start_req(&self, vendor: Vendor, account: &str, mode: ControlMode, prompt: Option<&str>) -> SessionStartReq {
        SessionStartReq {
            client_request_id: ClientRequestId::parse(uuid::Uuid::new_v4().to_string()).unwrap(),
            project_id: self.project.clone(),
            vendor,
            account_id: AccountId::parse(account).unwrap(),
            model: None,
            location: StartLocation::Here { location_id: None },
            permission_mode: None,
            control_mode: mode,
            team: false,
            prompt: prompt.map(str::to_string),
        }
    }

    fn evidence_of(&self, spawn_id: &str) -> Vec<SourceFacts> {
        self.world
            .evidence
            .lock()
            .iter()
            .filter(|e| e.hint.gilde_spawn_id.as_deref() == Some(spawn_id))
            .map(|e| e.facts.clone())
            .collect()
    }
}

fn alive(pid: i32) -> bool {
    pid > 0 && unsafe { libc::kill(pid, 0) } == 0
}

// ---------------------------------------------------------------------------
// Terminal mode through PtyManager + PtyDriver
// ---------------------------------------------------------------------------

/// A codex Terminal session end to end: the real PTY opens through the
/// supervisor (parent uid, the spawn id bound right after the open, A20.1),
/// the typed-after-ready first prompt arrives once the vendor's ready screen
/// is up (§6.2.2), `turn_send` is a bracketed paste (§6.2.3),
/// `session_technical` names the redacted argv, and `session_stop` ends it
/// with SIGHUP → fenced SIGTERM (never SIGKILL) and World reads Gilde's stop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_terminal_session_end_to_end() {
    let probe = Harness::new().await;
    let fake = fixtures::fake_agent_path();
    let input_log = probe.hermetic.tmp_dir().join("input.log");
    // A "codex" that is the fake's codex ready screen echoing submitted lines.
    let script = probe.hermetic.bin_dir().join("codex");
    std::fs::write(&script, format!("#!/bin/sh\nexec '{}' tui-idle-ready 150 codex\n", fake.display())).unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    gilde_code_lib::supervise::spawn::allow_executable(&script, &[gilde_code_lib::store::models::ProcKind::Pty]);
    let codex = abs(&std::fs::canonicalize(&script).unwrap());
    gilde_code_lib::supervise::spawn::allow_executable(Path::new(codex.as_str()), &[gilde_code_lib::store::models::ProcKind::Pty]);
    let bins = HashMap::from([(Vendor::Codex, ResolvedVendor { binary: binary_ref(&codex, "0.145.0"), compat: None })]);
    let cfg = probe.hermetic.root().join("cfg-unused");
    std::fs::create_dir_all(&cfg).unwrap();
    let live = Live::new(bins, vec![("FAKE_AGENT_INPUT_LOG".into(), input_log.display().to_string())], &cfg).await;

    let uid = live.m.start(live.start_req(Vendor::Codex, CODEX, ControlMode::Terminal, Some("-first prompt"))).await.unwrap().uid;
    let ctrl = live.world.controls.lock().get(&uid).cloned().unwrap();
    assert_eq!(ctrl.transport, Some(Transport::Pty));
    let pty = ctrl.pty.clone().unwrap();
    let spawn_id = live.world.binds.lock().iter().find(|(_, u)| u == &uid).map(|(s, _)| s.clone()).unwrap();
    let row = live.h.supervisor.row(&spawn_id).await.unwrap().expect("a registry row for the spawn");
    assert_eq!(row.kind, gilde_code_lib::store::models::ProcKind::Pty);

    let typed = |needle: &str| std::fs::read(&input_log).map(|b| String::from_utf8_lossy(&b).contains(needle)).unwrap_or(false);
    eventually("the first prompt typed after ready", || typed("-first prompt")).await;
    let facts = live.evidence_of(&spawn_id);
    assert!(facts.iter().any(|f| matches!(f, SourceFacts::Gilde(g) if g.ready_at.is_some())), "ready_at published");

    live.m
        .send(TurnSendReq {
            uid: uid.clone(),
            client_msg_id: ClientMsgId::parse(uuid::Uuid::new_v4().to_string()).unwrap(),
            text: "second message".into(),
            priority: SendPriority::Next,
        })
        .await
        .unwrap();
    eventually("the second message pasted", || typed("second message")).await;
    let paste = std::fs::read(&input_log).unwrap();
    assert!(paste.windows(6).any(|w| w == b"\x1b[200~"), "bracketed paste");

    let tech = live.m.technical(&uid).await.unwrap();
    assert!(tech.pid > 0);
    assert_eq!(tech.command_redacted[0], codex.as_str());

    let pid = tech.pid;
    let p = live.m.stop_preview(&uid).await.unwrap();
    assert_eq!(p.how, StopMethod::Fenced);
    let res = live.m.stop(SessionStopReq { uid: uid.clone(), confirm_token: p.confirm_token }).await.unwrap();
    assert!(res.stopped && res.survivors == 0, "{res:?}");
    eventually("the child is gone", || !alive(pid)).await;
    eventually("the exit is published", || {
        live.evidence_of(&spawn_id).iter().any(|f| matches!(f, SourceFacts::Process(ProcessFacts::Exited { .. })))
    })
    .await;
    let stop = live.evidence_of(&spawn_id).into_iter().rev().find_map(|f| match f {
        SourceFacts::Gilde(g) => Some(g),
        _ => None,
    });
    assert_eq!(stop.and_then(|g| g.stop_cause), Some(StopCause::User), "World reads Gilde's stop (R1)");
    assert!(live.m.actor(&uid).is_some_and(|a| a.info().pty.as_ref() == Some(&pty)));
}

// ---------------------------------------------------------------------------
// Conversation mode through ConvoDriver
// ---------------------------------------------------------------------------

/// A Conversation session end to end with `fake_claude`: the exact §6.1.1
/// launch (`--session-id`, `--permission-mode` always), the real composed env
/// (T-env-defaultAccount: another account names its realpath), the spawn id
/// bound on the driver's first evidence, the first prompt after
/// `initialize`, and `session_stop` = the driver's stop sequence (stdin EOF,
/// the fake exits 0) with no signal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_conversation_session_end_to_end() {
    let probe = Harness::new().await;
    let fake = PathBuf::from(env!("CARGO_BIN_EXE_fake_claude"));
    let link = probe.hermetic.install_fixture("claude", &fake);
    let claude = abs(&std::fs::canonicalize(link).unwrap());
    gilde_code_lib::supervise::spawn::allow_executable(Path::new(claude.as_str()), &[gilde_code_lib::store::models::ProcKind::Convo]);
    let cfg = std::fs::canonicalize({
        let d = probe.hermetic.root().join("claude-work");
        std::fs::create_dir_all(&d).unwrap();
        d
    })
    .unwrap();
    let scenario = probe.hermetic.root().join("scenario.json");
    let log = probe.hermetic.root().join("fake.log");
    let steps = json!({"version": VERSION, "steps": [
        {"expect":{"/type":"control_request","/request/subtype":"initialize"},"as":"init"},
        {"emit":{"type":"control_response","response":{"subtype":"success","request_id":"${init/request_id}",
            "response":{"claude_code_version":VERSION,"capabilities":[],"session_state":"idle","current_permission_mode":"default",
                "pid":"${pid}","models":[{"value":"default","displayName":"Default"}],"account":{"tokenSource":"oauth"}},
            "pending_permission_requests":[],"pending_user_dialog_requests":[]}}},
        {"expect":{"/type":"user"},"as":"first"},
        {"emit":{"type":"system","subtype":"init","session_id":"${sessionId}","model":"claude-haiku-5-5","permissionMode":"default",
            "capabilities":["interrupt_receipt_v1"],"claude_code_version":VERSION,"tools":[],"mcp_servers":[]}},
        {"emit":{"type":"result","subtype":"success","is_error":false,"terminal_reason":"completed","session_id":"${sessionId}",
            "usage":{"input_tokens":1,"output_tokens":1},"permission_denials":[],"user_message_uuids":null,"num_turns":1}},
        {"expectEof": true, "timeoutMs": 20000}
    ]});
    std::fs::write(&scenario, steps.to_string()).unwrap();
    let bins = HashMap::from([(Vendor::Claude, ResolvedVendor { binary: binary_ref(&claude, VERSION), compat: Some(record()) })]);
    let extra = vec![
        ("FAKE_CLAUDE_SCENARIO".to_string(), scenario.display().to_string()),
        ("FAKE_CLAUDE_LOG".to_string(), log.display().to_string()),
    ];
    let live = Live::new(bins, extra, &cfg).await;

    let uid = live.m.start(live.start_req(Vendor::Claude, CLAUDE_WORK, ControlMode::Conversation, Some("hello there"))).await.unwrap().uid;
    assert_eq!(live.world.controls.lock()[&uid].transport, Some(Transport::L1));
    let spawn_id = live.world.binds.lock().iter().find(|(_, u)| u == &uid).map(|(s, _)| s.clone()).expect("bound");
    let row = live.h.supervisor.row(&spawn_id).await.unwrap().expect("the planned row");
    assert_eq!(row.kind, gilde_code_lib::store::models::ProcKind::Convo);
    let fake_log = || std::fs::read_to_string(&log).unwrap_or_default();
    eventually("the first prompt after initialize", || fake_log().contains("hello there")).await;
    let first_line: serde_json::Value = serde_json::from_str(fake_log().lines().next().unwrap()).unwrap();
    let argv: Vec<String> = serde_json::from_value(first_line["argv"].clone()).unwrap_or_default();
    let flags = argv.join(" ");
    assert!(flags.contains("--session-id "), "{flags}");
    assert!(flags.contains("--permission-mode default"), "always passed: {flags}");
    assert!(flags.contains("--permission-prompts host"), "{flags}");
    // The env the child got: the account's realpath (C8), the ticket's spawn
    // marker, the Conversation entrypoint; DISABLE_AUTOUPDATER only when Gilde
    // itself has it (the test harness does), never set by Gilde (§6.0).
    let env = &first_line["env"];
    assert_eq!(env["CLAUDE_CONFIG_DIR"], cfg.to_str().unwrap());
    assert_eq!(env["GILDE_SPAWN_ID"], row.env_marker.as_str());
    assert_eq!(env["CLAUDE_CODE_ENTRYPOINT"], "gilde");
    assert_eq!(env.get("DISABLE_AUTOUPDATER").and_then(|v| v.as_str()), std::env::var("DISABLE_AUTOUPDATER").ok().as_deref());

    let tech = live.m.technical(&uid).await.unwrap();
    let pid = tech.pid;
    assert!(alive(pid));
    let p = live.m.stop_preview(&uid).await.unwrap();
    let res = live.m.stop(SessionStopReq { uid: uid.clone(), confirm_token: p.confirm_token }).await.unwrap();
    assert!(res.stopped && res.survivors == 0, "{res:?}");
    eventually("the CLI exits on stdin EOF", || !alive(pid)).await;
    eventually("the actor ends", || !live.m.actor(&uid).unwrap().sink().is_live()).await;
}

/// T-env-defaultAccount with T09's real `compose_env`: the default login's
/// variable is unset, another account names its realpath, cursor/agy get
/// none (C8, PM-30).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_env_default_account_real_compose() {
    let h = Harness::new().await;
    let env = SupervisorEnv::new(h.supervisor.clone(), EnvBase::new("/usr/bin:/bin", h.hermetic.home()));
    let default = AccountEnv {
        account_id: AccountId::parse("a_0000000000000001").unwrap(),
        vendor: Vendor::Claude,
        env_key: None,
        config_dir: Some(abs(&h.hermetic.home().join(".claude"))),
    };
    let e = env.compose(Vendor::Claude, Some(&default), "mk");
    assert!(!e.contains_key("CLAUDE_CONFIG_DIR"));
    assert_eq!(e["GILDE_SPAWN_ID"], "mk");
    assert!(e.contains_key("GILDE_APP_INSTANCE"));
    assert_eq!(e["TERM_PROGRAM"], "gilde-code");
    let work = AccountEnv { env_key: Some("CLAUDE_CONFIG_DIR".into()), config_dir: Some(abs(Path::new("/Users/x/.claude-work"))), ..default.clone() };
    assert_eq!(env.compose(Vendor::Claude, Some(&work), "mk")["CLAUDE_CONFIG_DIR"], "/Users/x/.claude-work");
    let agy = AccountEnv { vendor: Vendor::Agy, env_key: Some("AGY_HOME".into()), config_dir: Some(abs(Path::new("/Users/x/.gemini"))), ..default };
    assert!(!env.compose(Vendor::Agy, Some(&agy), "mk").contains_key("AGY_HOME"));
    assert_eq!(env.home().map(|h| h.as_str().to_string()), Some(h.hermetic.home().to_string_lossy().into_owned()));
}

// ---------------------------------------------------------------------------
// screen_prompt_key: prompt identity and key idempotency (A34.1, A34.2,
// A38.6; the G7-02 evidence of g2/g7b ported) — a /bin/sh TUI named
// `claude` in a real PTY behind the real sessions manager; no vendor CLI
// ---------------------------------------------------------------------------

use gilde_code_lib::contract::ids::{PtyId, RequestId};
use gilde_code_lib::contract::ipc::{ScreenKeyOutcome, ScreenPromptAction, ScreenPromptKeyReq};
use gilde_code_lib::contract::ErrorCode;
use gilde_code_lib::contract::services::PtyApi;

/// `draw <command>` paints a Bash permission prompt (same question and
/// options whatever the command); `key` reads one key; `log` appends to
/// `$T36_LOG`; `interrupted` leaves the prompt like the TUI does after "No"
/// and logs every later byte (the reply's paste).
const PRELUDE: &str = r#"#!/bin/sh
rule='────────────────────────────────────────────────────────────────────────────────'
draw() {
  printf '\033[2J\033[H'
  printf ' Claude Code (fake t36)\r\n'
  i=0
  while [ $i -lt 26 ]; do printf '\r\n'; i=$((i+1)); done
  printf '%s\r\n' "$rule"
  printf ' Bash command\r\n'
  printf '   %s\r\n' "$1"
  printf '   Clean up\r\n'
  printf '\r\n'
  printf ' Do you want to proceed?\r\n'
  printf ' \342\235\257 1. Yes\r\n'
  printf "   2. Yes, and don't ask again for rm commands in /tmp/fake-project\r\n"
  printf '   3. No\r\n'
  printf '\r\n'
  printf ' Esc to cancel \302\267 Tab to amend'
}
key() { /bin/dd bs=1 count=1 2>/dev/null; }
log() { printf '%s\n' "$1" >> "$T36_LOG"; }
interrupted() {
  printf '\033[2J\033[H  Interrupted \302\267 What should Claude do instead?\r\n> '
  exec /bin/cat >> "$T36_LOG"
}
/bin/stty raw -echo
"#;

/// Prompt A, one key; prompt B for another command, one key; then the TUI
/// leaves the prompt.
const A_THEN_B: &str = r#"draw "rm -rf build/cache"
k=$(key); log "A:$k"
draw "rm -rf ~/Documents"
k=$(key); log "B:$k"
interrupted
"#;

/// One prompt that stays up until "3" (a slow TUI): every key is logged.
const STAYS_UP: &str = r#"draw "rm -rf build/cache"
while :; do
  k=$(key); log "K:$k"
  if [ "$k" = "3" ]; then interrupted; fi
done
"#;

/// The prompt, one key, a second of work (the prompt is gone), then the very
/// same prompt again (the same rev): its keys are logged as `B:`.
const LEAVES_AND_RETURNS: &str = r#"draw "rm -rf build/cache"
k=$(key); log "A:$k"
printf '\033[2J\033[H \342\234\273 Working\342\200\246\r\n'
/bin/sleep 1
draw "rm -rf build/cache"
while :; do k=$(key); log "B:$k"; done
"#;

struct ScreenRig {
    /// Owns the temp tree the script and its log live in.
    _probe: Harness,
    live: Live,
    uid: SessionUid,
    pty: PtyId,
    log: PathBuf,
}

impl ScreenRig {
    async fn start(body: &str) -> ScreenRig {
        let probe = Harness::new().await;
        let log = probe.hermetic.tmp_dir().join("t36.log");
        let script = probe.hermetic.bin_dir().join("claude");
        std::fs::write(&script, format!("{PRELUDE}{body}")).unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let claude = abs(&std::fs::canonicalize(&script).unwrap());
        gilde_code_lib::supervise::spawn::allow_executable(Path::new(claude.as_str()), &[gilde_code_lib::store::models::ProcKind::Pty]);
        let bins = HashMap::from([(Vendor::Claude, ResolvedVendor { binary: binary_ref(&claude, VERSION), compat: None })]);
        let cfg = std::fs::canonicalize({
            let d = probe.hermetic.root().join("claude-work");
            std::fs::create_dir_all(&d).unwrap();
            d
        })
        .unwrap();
        let live = Live::new(bins, vec![("T36_LOG".into(), log.display().to_string())], &cfg).await;
        let uid = live.m.start(live.start_req(Vendor::Claude, CLAUDE_WORK, ControlMode::Terminal, None)).await.unwrap().uid;
        let pty = live.world.controls.lock().get(&uid).cloned().unwrap().pty.unwrap();
        ScreenRig { _probe: probe, live, uid, pty, log }
    }

    fn log(&self) -> String {
        String::from_utf8_lossy(&std::fs::read(&self.log).unwrap_or_default()).into_owned()
    }

    fn card(&self) -> Option<gilde_code_lib::contract::activity::ScreenPromptFacts> {
        self.live.h.manager.screen_prompt(&self.pty)
    }

    async fn first_card(&self) -> gilde_code_lib::contract::activity::ScreenPromptFacts {
        eventually("a prompt card", || self.card().is_some()).await;
        self.card().unwrap()
    }

    /// `screen_prompt_key` through the sessions manager, for the card `rev`.
    async fn press(&self, rev: &str, action: ScreenPromptAction) -> Result<ScreenKeyOutcome, IpcError> {
        self.live
            .m
            .screen_key(ScreenPromptKeyReq {
                uid: self.uid.clone(),
                request_id: RequestId::parse(format!("sp_{rev}")).unwrap(),
                prompt_rev: rev.to_string(),
                action,
            })
            .await
            .map(|r| r.outcome)
    }

    async fn close(self) {
        let _ = self.live.h.manager.close(&self.pty, gilde_code_lib::contract::services::SignalPlan::HupThenTerm { grace_ms: 2_000 }).await;
    }
}

fn key(k: &str) -> ScreenPromptAction {
    ScreenPromptAction::Key { key: k.into() }
}

const PASTE: &str = "\u{1b}[200~do not delete my documents\u{1b}[201~\r";

/// G7-02 (ported): the user allows prompt A from its card; prompt B — another
/// command — is a new card with its own rev (A34.1); denying B from B's card
/// types "3" into B. A late press aimed at A's card is `STALE_PROMPT`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g7_02_deny_after_allow_reaches_the_next_prompt() {
    let rig = ScreenRig::start(A_THEN_B).await;
    let a = rig.first_card().await;
    assert_eq!(rig.press(&a.prompt_rev, key("1")).await, Ok(ScreenKeyOutcome::Sent), "A allowed");
    eventually("A got 1", || rig.log().contains("A:1")).await;
    eventually("B is a new card", || rig.card().is_some_and(|b| b.prompt_rev != a.prompt_rev)).await;
    let b = rig.card().unwrap();

    let late = rig.press(&a.prompt_rev, key("1")).await;
    assert_eq!(late.map_err(|e| e.code), Err(ErrorCode::StalePrompt), "A's card cannot answer B");
    assert_eq!(rig.press(&b.prompt_rev, key("3")).await, Ok(ScreenKeyOutcome::Sent), "B denied");
    eventually("B got 3", || rig.log().contains("B:3")).await;
    assert!(!rig.log().contains("B:1"), "log: {:?}", rig.log());
    rig.close().await;
}

/// G7-02 variant (ported): "Deny and say why…" on B right after A was
/// allowed is executed — "3", then the reply once B left the grid.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g7_02_deny_with_reply_after_allow_is_executed() {
    let rig = ScreenRig::start(A_THEN_B).await;
    let a = rig.first_card().await;
    assert_eq!(rig.press(&a.prompt_rev, key("1")).await, Ok(ScreenKeyOutcome::Sent));
    eventually("B is a new card", || rig.card().is_some_and(|b| b.prompt_rev != a.prompt_rev)).await;
    let b = rig.card().unwrap();
    let reply = ScreenPromptAction::DenyWithReply { text: "do not delete my documents".into() };
    assert_eq!(rig.press(&b.prompt_rev, reply).await, Ok(ScreenKeyOutcome::Sent));
    eventually("the reply is pasted", || rig.log().contains(PASTE)).await;
    assert_eq!(rig.log(), format!("A:1\nB:3\n{PASTE}"));
    rig.close().await;
}

/// A34.2 on one prompt (the same rev): after Allow, Deny is another action
/// and is executed on its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deny_after_allow_on_the_same_prompt_is_executed() {
    let rig = ScreenRig::start(STAYS_UP).await;
    let p = rig.first_card().await;
    assert_eq!(rig.press(&p.prompt_rev, key("1")).await, Ok(ScreenKeyOutcome::Sent));
    eventually("1 typed", || rig.log().contains("K:1")).await;
    assert_eq!(rig.press(&p.prompt_rev, key("3")).await, Ok(ScreenKeyOutcome::Sent), "not the cached outcome of 1");
    eventually("3 typed", || rig.log().contains("K:3")).await;
    assert_eq!(rig.log(), "K:1\nK:3\n");
    rig.close().await;
}

/// A34.2: a double press within 2 s — sequential or concurrent — types the
/// key once and both calls get the first outcome; "Deny and say why…" right
/// after is another action and is executed (No digit, then the reply).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_double_press_types_once_and_deny_with_reply_after_it_is_executed() {
    let rig = ScreenRig::start(STAYS_UP).await;
    let p = rig.first_card().await;
    let (x, y) = tokio::join!(rig.press(&p.prompt_rev, key("1")), rig.press(&p.prompt_rev, key("1")));
    assert_eq!((x, y), (Ok(ScreenKeyOutcome::Sent), Ok(ScreenKeyOutcome::Sent)));
    assert_eq!(rig.press(&p.prompt_rev, key("1")).await, Ok(ScreenKeyOutcome::Sent), "a third press");
    eventually("1 typed", || rig.log().contains("K:1")).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(rig.log(), "K:1\n", "typed once");

    let reply = ScreenPromptAction::DenyWithReply { text: "do not delete my documents".into() };
    assert_eq!(rig.press(&p.prompt_rev, reply).await, Ok(ScreenKeyOutcome::Sent));
    eventually("the reply is pasted", || rig.log().contains(PASTE)).await;
    assert_eq!(rig.log(), format!("K:1\nK:3\n{PASTE}"));
    rig.close().await;
}

/// A34.2 / A38.6: once the prompt left the grid, an identical press on the
/// same prompt drawn again (same rev) is not served from memory: it is typed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_identical_press_after_the_prompt_left_is_typed_again() {
    let rig = ScreenRig::start(LEAVES_AND_RETURNS).await;
    let p = rig.first_card().await;
    assert_eq!(rig.press(&p.prompt_rev, key("1")).await, Ok(ScreenKeyOutcome::Sent));
    eventually("A got 1", || rig.log().contains("A:1")).await;
    eventually("the card left", || rig.card().is_none()).await;
    eventually("the same prompt is back", || rig.card().is_some()).await;
    let again = rig.card().unwrap();
    assert_eq!(again.prompt_rev, p.prompt_rev, "the same prompt: the same rev");
    assert_ne!(again.first_seen_at, p.first_seen_at, "another appearance");
    assert_eq!(rig.press(&again.prompt_rev, key("1")).await, Ok(ScreenKeyOutcome::Sent));
    eventually("typed again", || rig.log().contains("B:1")).await;
    rig.close().await;
}
