//! Fake services and a PaneHost rig for the `panehost_*` tests (T07).
//!
//! Nothing here starts or signals a real process: the PTY is a recorder with
//! fake pids above the kernel's pid range, the World is a store snapshot, and
//! every socket lives in the rig's own temp dir (mode 0700).

#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use gilde_code_lib::contract::activity::{ActivityState, ExitInfo, PtyFacts, ScreenPromptFacts};
use gilde_code_lib::contract::attention::AttentionQueue;
use gilde_code_lib::contract::common::{AbsPath, EpochMs, Vendor};
use gilde_code_lib::contract::compat::CompatCapability;
use gilde_code_lib::contract::error::IpcError;
use gilde_code_lib::contract::ids::{MemberKey, PaneId, PtyId, SessionUid, TeamKey};
use gilde_code_lib::contract::ipc::{DismissWhat, ScreenKeyOutcome, ScreenPromptAction};
use gilde_code_lib::contract::services::{
    CompatSink, EvidenceSink, EvidenceUpdate, IdentityKey, IdentityLock, LeadEnv, LocationFacts, PaneHostApi, PtyApi,
    PtyExitSink, PtyHandle, PtyOpenSpec, SignalOutcome, SignalPlan, SpawnTicket, StorePatchSink, WorldApi,
};
use gilde_code_lib::contract::session::{MemberRole, Session, SessionDetails};
use gilde_code_lib::contract::store::StoreSnapshot;
use gilde_code_lib::contract::team::{
    LeadBinding, Member, MemberBackend, PaneHostingState, Presence, Team, TeamAdapter, TeamCapabilities, TeamProgress,
    TeamSource,
};
use gilde_code_lib::panehost::auth::PeerCred;
use gilde_code_lib::panehost::proto::{ShimRequest, ShimResponse, SHIM_PROTOCOL_V};
use gilde_code_lib::panehost::{
    install_shim, LeadEnvSource, LeadFate, LeadPaneTable, PaneHost, PaneHostConfig, PaneHostDeps, PaneTableSink,
};
use serde_json::json;

/// A generous ceiling for event-driven waits (never a budget, A10.7).
pub const CEILING: Duration = Duration::from_secs(30);

pub fn now_ms() -> EpochMs {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as EpochMs)
        .unwrap_or(0)
}

/// Poll `f` until it returns `Some`, up to [`CEILING`].
pub fn wait_for<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(start.elapsed() < CEILING, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

// ---------------------------------------------------------------------------
// PTY
// ---------------------------------------------------------------------------

/// Fake pids live above macOS's pid range, so no kernel lookup ever finds a
/// real process for them.
pub const FAKE_PID_BASE: i32 = 3_900_000;

#[derive(Default)]
pub struct FakePty {
    pub opens: Mutex<Vec<(PtyOpenSpec, PtyHandle)>>,
    pub closes: Mutex<Vec<(PtyId, SignalPlan)>>,
    pub pastes: Mutex<Vec<(PtyId, String, bool)>>,
    sinks: Mutex<HashMap<PtyId, Arc<dyn PtyExitSink>>>,
    pub fail_next: AtomicBool,
    pub open_delay: Mutex<Option<Duration>>,
    /// On close, report an exit by SIGHUP (the process died of the signal).
    pub exit_on_close: AtomicBool,
    /// Report every close as leaving a survivor.
    pub survive: AtomicBool,
    next: AtomicI32,
}

impl FakePty {
    pub fn new() -> Arc<FakePty> {
        Arc::new(FakePty::default())
    }

    pub fn open_count(&self) -> usize {
        self.opens.lock().unwrap().len()
    }

    pub fn handle(&self, i: usize) -> PtyHandle {
        self.opens.lock().unwrap()[i].1.clone()
    }

    pub fn spec(&self, i: usize) -> PtyOpenSpec {
        self.opens.lock().unwrap()[i].0.clone()
    }

    /// Report an exit of the process in `pty` (as the PTY layer would).
    pub fn exit(&self, pty: &PtyId, code: Option<i32>, signal: Option<i32>) {
        let sink = self.sinks.lock().unwrap().get(pty).cloned().expect("an opened pty");
        sink.exited(pty, ExitInfo { code, signal }, now_ms());
    }

    pub fn closes(&self) -> Vec<(PtyId, SignalPlan)> {
        self.closes.lock().unwrap().clone()
    }

    pub fn wait_close_of(&self, pty: &PtyId) -> SignalPlan {
        wait_for("a close", || self.closes().into_iter().find(|(p, _)| p == pty).map(|(_, plan)| plan))
    }
}

#[async_trait]
impl PtyApi for FakePty {
    async fn open(&self, spec: PtyOpenSpec, on_exit: Arc<dyn PtyExitSink>) -> Result<PtyHandle, IpcError> {
        let delay = *self.open_delay.lock().unwrap();
        if let Some(d) = delay {
            tokio::time::sleep(d).await;
        }
        if self.fail_next.swap(false, Ordering::SeqCst) {
            return Err(IpcError::spawn_failed());
        }
        let n = self.next.fetch_add(1, Ordering::SeqCst);
        let handle = PtyHandle {
            pty_id: PtyId::mint(),
            pid: FAKE_PID_BASE + n,
            ticket: SpawnTicket { spawn_id: format!("spawn-{n}"), env_marker: format!("marker-{n}") },
        };
        self.sinks.lock().unwrap().insert(handle.pty_id.clone(), on_exit);
        self.opens.lock().unwrap().push((spec, handle.clone()));
        Ok(handle)
    }

    async fn write(&self, _pty: &PtyId, _bytes: &[u8]) -> Result<(), IpcError> {
        Ok(())
    }

    async fn paste(&self, pty: &PtyId, text: &str, submit: bool) -> Result<(), IpcError> {
        self.pastes.lock().unwrap().push((pty.clone(), text.to_string(), submit));
        Ok(())
    }

    async fn resize(&self, _pty: &PtyId, _cols: u16, _rows: u16) -> Result<(), IpcError> {
        Ok(())
    }

    async fn close(&self, pty: &PtyId, plan: SignalPlan) -> Result<SignalOutcome, IpcError> {
        self.closes.lock().unwrap().push((pty.clone(), plan));
        if self.exit_on_close.load(Ordering::SeqCst) {
            let sink = self.sinks.lock().unwrap().get(pty).cloned();
            if let Some(sink) = sink {
                sink.exited(pty, ExitInfo { code: None, signal: Some(1) }, now_ms());
            }
        }
        let survivor = self.survive.load(Ordering::SeqCst);
        Ok(SignalOutcome { exited: !survivor, survivor })
    }

    fn screen_prompt(&self, _pty: &PtyId) -> Option<ScreenPromptFacts> {
        None
    }

    async fn screen_action(&self, _pty: &PtyId, _rev: &str, _a: &ScreenPromptAction) -> Result<ScreenKeyOutcome, IpcError> {
        Err(IpcError::unsupported("fake"))
    }

    fn facts(&self, _pty: &PtyId) -> Option<PtyFacts> {
        None
    }
}

// ---------------------------------------------------------------------------
// World, compat, env, fate, table sink
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct FakeSink {
    pub updates: Mutex<Vec<EvidenceUpdate>>,
}

impl EvidenceSink for FakeSink {
    fn evidence(&self, update: EvidenceUpdate) {
        self.updates.lock().unwrap().push(update);
    }
    fn location(&self, _facts: LocationFacts) {}
    fn source_status(&self, _status: gilde_code_lib::contract::store::SourceStatus) {}
    fn account(&self, _account: gilde_code_lib::contract::account::Account) {}
}

pub struct FakeWorld {
    pub sessions: Mutex<Vec<Session>>,
    pub snapshot: Mutex<StoreSnapshot>,
    pub sink: Arc<FakeSink>,
}

pub fn empty_snapshot() -> StoreSnapshot {
    serde_json::from_value(json!({
        "rev": 0, "projects": [], "locations": [], "sessions": [], "teams": [], "members": [],
        "tasks": [], "messages": [],
        "queue": {"rev": 0, "items": [], "needYou": 0, "also": 0, "pausedUntil": null},
        "accounts": [], "leftovers": [], "compat": [], "notices": [],
        "boot": {"spawnGateOpen": true, "firstDiscoveryDone": true, "agentsDetected": true, "memoryOnly": null, "agents": [], "agentsScannedAt": null, "recovery": null},
        "sources": [], "integrations": {"orchestrator": "absent"}
    }))
    .expect("an empty snapshot")
}

impl FakeWorld {
    pub fn new() -> Arc<FakeWorld> {
        Arc::new(FakeWorld {
            sessions: Mutex::new(Vec::new()),
            snapshot: Mutex::new(empty_snapshot()),
            sink: Arc::new(FakeSink::default()),
        })
    }

    pub fn evidence(&self) -> Vec<EvidenceUpdate> {
        self.sink.updates.lock().unwrap().clone()
    }

    pub fn set_control(&self, uid: &SessionUid, control: serde_json::Value) {
        let mut s = self.sessions.lock().unwrap();
        let session = s.iter_mut().find(|x| &x.uid == uid).expect("a session");
        session.control = serde_json::from_value(control).expect("a control");
    }
}

#[async_trait]
impl WorldApi for FakeWorld {
    fn sink(&self) -> Arc<dyn EvidenceSink> {
        self.sink.clone()
    }
    fn subscribe(&self, _sink: Box<dyn StorePatchSink>) -> StoreSnapshot {
        self.snapshot.lock().unwrap().clone()
    }
    fn snapshot(&self) -> StoreSnapshot {
        self.snapshot.lock().unwrap().clone()
    }
    fn session(&self, uid: &SessionUid) -> Option<Session> {
        self.sessions.lock().unwrap().iter().find(|s| &s.uid == uid).cloned()
    }
    fn queue(&self) -> AttentionQueue {
        self.snapshot.lock().unwrap().queue.clone()
    }
    async fn identity_lock(&self, _key: IdentityKey) -> IdentityLock {
        unimplemented!("PaneHost never takes identity locks")
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

#[derive(Default)]
pub struct FakeCompat {
    pub deviations: Mutex<Vec<(String, CompatCapability, String)>>,
    pub notes: Mutex<Vec<(String, String)>>,
}

impl FakeCompat {
    pub fn deviations(&self) -> Vec<(String, CompatCapability, String)> {
        self.deviations.lock().unwrap().clone()
    }
    pub fn notes(&self) -> Vec<(String, String)> {
        self.notes.lock().unwrap().clone()
    }
}

impl CompatSink for FakeCompat {
    fn handshake(&self, _version: &str, _ok: bool, _note: Option<String>) {}
    fn deviation(&self, version: &str, capability: CompatCapability, note: String) {
        self.deviations.lock().unwrap().push((version.to_string(), capability, note));
    }
    fn note(&self, version: &str, note: String) {
        self.notes.lock().unwrap().push((version.to_string(), note));
    }
}

pub struct FakeEnv {
    pub base: Mutex<BTreeMap<String, String>>,
}

impl LeadEnvSource for FakeEnv {
    fn base_env(&self, _lead: &SessionUid) -> Result<BTreeMap<String, String>, IpcError> {
        Ok(self.base.lock().unwrap().clone())
    }
}

#[derive(Default)]
pub struct FakeFate {
    pub backgrounded: AtomicBool,
}

#[async_trait]
impl LeadFate for FakeFate {
    async fn moved_to_background(&self, _lead: &SessionUid) -> bool {
        self.backgrounded.load(Ordering::SeqCst)
    }
}

/// One A18.1 binding: (spawn id, account, PTY, minted uid).
pub type Binding = (String, Option<gilde_code_lib::contract::ids::AccountId>, PtyId, SessionUid);

/// Records A18.1 bindings and mints a uid each.
#[derive(Default)]
pub struct FakeSessions {
    pub bound: Mutex<Vec<Binding>>,
}

impl gilde_code_lib::panehost::PaneSessions for FakeSessions {
    fn pane_spawned(&self, spawn_id: &str, account: Option<gilde_code_lib::contract::ids::AccountId>, pty: &PtyId) -> Option<SessionUid> {
        let uid = SessionUid::mint();
        self.bound.lock().unwrap().push((spawn_id.to_string(), account, pty.clone(), uid.clone()));
        Some(uid)
    }
}

#[derive(Default)]
pub struct FakeTables {
    pub tables: Mutex<Vec<LeadPaneTable>>,
}

impl PaneTableSink for FakeTables {
    fn table(&self, table: LeadPaneTable) {
        self.tables.lock().unwrap().push(table);
    }
}

// ---------------------------------------------------------------------------
// the rig
// ---------------------------------------------------------------------------

pub const LEAD_SID: &str = "c3c103c3-0000-4000-8000-000000000001";
pub const TEAM: &str = "session-72cb5f46";
pub const VERSION: &str = "2.1.307";

/// One PaneHost with fakes, a hosted lead and (optionally) a listener.
pub struct Rig {
    pub dir: tempfile::TempDir,
    pub root: PathBuf,
    pub host: Arc<PaneHost>,
    pub pty: Arc<FakePty>,
    pub world: Arc<FakeWorld>,
    pub compat: Arc<FakeCompat>,
    pub fate: Arc<FakeFate>,
    pub tables: Arc<FakeTables>,
    pub sessions: Arc<FakeSessions>,
    pub env: Arc<FakeEnv>,
    pub lead: SessionUid,
    pub lead_pid: i32,
    pub lead_env: LeadEnv,
    pub token: String,
    pub claude: PathBuf,
    pub project: PathBuf,
}

/// The pid of this test process's parent (a live process that is not ours:
/// only ever compared, never signalled).
pub fn parent_pid() -> i32 {
    std::os::unix::process::parent_id() as i32
}

pub fn me() -> i32 {
    std::process::id() as i32
}

fn lead_session(uid: &SessionUid, claude: &Path, project: &Path) -> Session {
    let mut details = serde_json::to_value(SessionDetails::default()).unwrap();
    details["controlMode"] = json!("terminal");
    serde_json::from_value(json!({
        "uid": uid, "vendor": "claude", "accountId": null, "vendorSessionId": LEAD_SID,
        "projectId": null, "locationId": null, "cwd": project.to_str().unwrap(),
        "control": {"kind": "inGilde", "mode": "terminal", "pty": null, "hostedTeam": true, "transport": "pty"},
        "activity": {"state": {"kind": "working"}, "since": 1, "confidence": "high", "evidence": null, "episode": "e_0123456789abcdef"},
        "title": null, "team": null, "startedAt": null, "endedAt": null, "lastActivityAt": null,
        "pending": [], "queued": [], "model": null, "permissionMode": "default",
        "binary": {"realpath": claude.to_str().unwrap(), "fingerprint": {"size": 1, "mtimeMs": 1, "inode": 1}, "version": VERSION, "pidpath": null},
        "claudeVersion": VERSION, "memoryBytes": null, "usageToday": null,
        "caps": {"send": "none", "interrupt": false, "forceStop": false, "approve": "none", "setModel": false,
                 "setPermissionMode": [], "continueOptions": [], "stopSession": "none", "terminalView": false,
                 "logView": false, "changes": "none", "ownerFocus": false},
        "details": details, "mergedInto": null
    }))
    .expect("a lead session")
}

impl Rig {
    /// A rig whose lead is `lead_pid` (this process when the shim binary is
    /// its child; this process's parent for direct `handle` calls whose peer
    /// is this process).
    pub fn new(lead_pid: i32) -> Rig {
        Rig::with_deadline(lead_pid, gilde_code_lib::panehost::panes::SPAWN_DEADLINE)
    }

    /// As [`Rig::new`] with another `respawn-pane` spawn deadline.
    pub fn with_deadline(lead_pid: i32, spawn_deadline: Duration) -> Rig {
        let dir = tempfile::Builder::new().prefix("ph").tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let gh = root.join("gh");
        let run = gh.join("run");
        for d in [&gh, &run, &gh.join("shim")] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::set_permissions(d, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
        }
        let socket_path = run.join("ph.sock");
        assert!(socket_path.as_os_str().len() <= 100, "the test socket must stay inside the temp GILDE_HOME");
        let shim_dir = gh.join("shim").join(gilde_code_lib::BUILD_ID);
        install_shim(Path::new(env!("CARGO_BIN_EXE_gilde-tmux")), &shim_dir).unwrap();

        // A stand-in claude install (never executed).
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let claude = bin.join("claude.exe");
        std::fs::write(&claude, b"#!/bin/false\n").unwrap();
        std::fs::set_permissions(&claude, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let project = root.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let empty = root.join("emptybin");
        std::fs::create_dir_all(&empty).unwrap();

        let cfg = PaneHostConfig {
            socket_path,
            shim_dir,
            shim_root: gh.join("shim"),
            build_id: gilde_code_lib::BUILD_ID.to_string(),
            app_uid: gilde_code_lib::panehost::auth::app_uid(),
            spawn_deadline,
        };
        Rig::with_config(cfg, root, dir, lead_pid, claude, project, empty)
    }

    #[allow(clippy::too_many_arguments)]
    fn with_config(
        cfg: PaneHostConfig,
        root: PathBuf,
        dir: tempfile::TempDir,
        lead_pid: i32,
        claude: PathBuf,
        project: PathBuf,
        empty: PathBuf,
    ) -> Rig {
        let pty = FakePty::new();
        let world = FakeWorld::new();
        let compat = Arc::new(FakeCompat::default());
        let fate = Arc::new(FakeFate::default());
        let tables = Arc::new(FakeTables::default());
        let sessions = Arc::new(FakeSessions::default());
        let base: BTreeMap<String, String> = [
            ("PATH", empty.to_str().unwrap()),
            ("HOME", root.to_str().unwrap()),
            ("TERM_PROGRAM", "iTerm.app"),
            ("TMUX", "/tmp/should-never-pass,1,0"),
            ("ITERM_SESSION_ID", "w0t0p0"),
            ("GILDE_SPAWN_ID", "lead-marker"),
            ("GILDE_APP_INSTANCE", "instance-1"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let env = Arc::new(FakeEnv { base: Mutex::new(base) });
        let host = PaneHost::new(
            cfg,
            PaneHostDeps {
                pty: pty.clone(),
                world: world.clone(),
                compat: compat.clone(),
                env: env.clone(),
                fate: fate.clone(),
                sessions: sessions.clone(),
            },
        );
        host.set_table_sink(tables.clone());
        let lead = SessionUid::mint();
        world.sessions.lock().unwrap().push(lead_session(&lead, &claude, &project));
        Rig {
            dir,
            root,
            host,
            pty,
            world,
            compat,
            fate,
            tables,
            sessions,
            env,
            lead,
            lead_pid,
            lead_env: LeadEnv { set: Vec::new(), remove: Vec::new() },
            token: String::new(),
            claude,
            project,
        }
    }

    /// Bind the listener on the current runtime (multi-thread) and register
    /// the lead (`lead_env` + `lead_started`).
    pub fn start(mut self) -> Rig {
        self.host.start(tokio::runtime::Handle::current()).expect("the listener binds");
        self.register_lead();
        self
    }

    /// Register the lead without a listener (direct `handle` calls).
    pub fn without_listener(mut self) -> Rig {
        // `lead_env` refuses without hosting; the direct-call rigs flip it on.
        self.host.start(tokio::runtime::Handle::current()).expect("the listener binds");
        self.register_lead();
        self
    }

    fn register_lead(&mut self) {
        let env = PaneHostApi::lead_env(&*self.host, &self.lead).expect("a lead env");
        self.token = env
            .set
            .iter()
            .find(|(k, _)| k == "GILDE_PANE_HOST_TOKEN")
            .map(|(_, v)| v.clone())
            .expect("a token");
        self.lead_env = env;
        PaneHostApi::lead_started(&*self.host, &self.lead, self.lead_pid);
    }

    pub fn socket(&self) -> PathBuf {
        self.host.config().socket_path.clone()
    }

    pub fn shim(&self) -> PathBuf {
        self.host.config().shim_dir.join("tmux")
    }

    /// A request as the shim would send it for `args` after `-L claude-swarm-<lead>`.
    pub fn req(&self, args: &[&str]) -> ShimRequest {
        let mut argv = vec!["-L".to_string(), format!("claude-swarm-{}", self.lead_pid)];
        argv.extend(args.iter().map(|s| s.to_string()));
        ShimRequest {
            v: SHIM_PROTOCOL_V,
            build: gilde_code_lib::BUILD_ID.to_string(),
            token: self.token.clone(),
            shim_pid: me(),
            shim_ppid: self.lead_pid,
            argv,
            cwd: self.project.to_str().unwrap().to_string(),
        }
    }

    /// The peer of a direct call: this process (its parent is the lead when
    /// the rig was made with [`parent_pid`]).
    pub fn peer(&self) -> PeerCred {
        PeerCred { uid: gilde_code_lib::panehost::auth::app_uid(), pid: me() }
    }

    /// One direct call through the full §4.6 checks.
    pub async fn call(&self, args: &[&str]) -> ShimResponse {
        self.host.handle(self.req(args), self.peer()).await
    }

    /// The teammate launch command of R6 §5 LP-10 for `name`, naming the
    /// rig's claude install.
    pub fn teammate_command(&self, name: &str, colour: &str) -> String {
        self.teammate_command_with(name, colour, self.claude.to_str().unwrap())
    }

    pub fn teammate_command_with(&self, name: &str, colour: &str, bin: &str) -> String {
        format!(
            "cd {} && env CLAUDECODE=1 CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1 CLAUDE_CONFIG_DIR=/u/.claude-g CLAUDE_CODE_ATTRIBUTION_PROJECT='' {bin} --agent-id {name}@{TEAM} --agent-name {name} --team-name {TEAM} --agent-color {colour} --parent-session-id {LEAD_SID} --settings '{{\"permissions\":{{\"defaultMode\":\"default\"}}}}' --model haiku",
            self.project.display()
        )
    }

    pub fn table(&self) -> LeadPaneTable {
        self.host.table(&self.lead).expect("the lead's table")
    }

    /// A hosted Claude team in World's snapshot with `members` pane members
    /// (`name`, pane id), the lead's member bound to the rig's lead session.
    pub fn hosted_team(&self, members: &[(&str, u32)]) -> (TeamKey, Vec<MemberKey>) {
        let team = TeamKey::derive("claudeNative", None, TEAM);
        let lead_key = MemberKey::derive(&team, &format!("team-lead@{TEAM}"), None);
        let mut rows = vec![member(&team, &lead_key, "team-lead", MemberRole::Lead, MemberBackend::InProcess, None, Some(self.lead.clone()))];
        let mut keys = Vec::new();
        for (name, pane) in members {
            let key = MemberKey::derive(&team, &format!("{name}@{TEAM}"), None);
            rows.push(member(&team, &key, name, MemberRole::Member, MemberBackend::Pane, Some(*pane), None));
            keys.push(key);
        }
        let mut snap = self.world.snapshot.lock().unwrap();
        snap.teams.push(Team {
            key: team.clone(),
            source: TeamSource::ClaudeNative,
            name: TEAM.into(),
            raw_id: TEAM.into(),
            project_id: None,
            lead: Some(lead_key),
            lead_binding: LeadBinding::Hosted,
            hosted: true,
            external_host: None,
            caps: TeamCapabilities::all_native(),
            glyph_state: ActivityState::Working,
            progress: TeamProgress { done: 0, total: 0, inferred: true, approx: false, in_flight: None, bottlenecks: vec![] },
            started_at: None,
            ended_at: None,
            pane_hosting: PaneHostingState::Hosted,
            roster_stale: false,
            pane_tiles: Vec::new(),
        });
        snap.members.extend(rows);
        (team, keys)
    }

    pub fn set_member(&self, key: &MemberKey, f: impl FnOnce(&mut Member)) {
        let mut snap = self.world.snapshot.lock().unwrap();
        let m = snap.members.iter_mut().find(|m| &m.key == key).expect("a member");
        f(m);
    }
}

pub fn member(
    team: &TeamKey,
    key: &MemberKey,
    name: &str,
    role: MemberRole,
    backend: MemberBackend,
    pane: Option<u32>,
    session: Option<SessionUid>,
) -> Member {
    Member {
        key: key.clone(),
        team_key: team.clone(),
        session_uid: session,
        name: name.to_string(),
        agent_id: Some(format!("{name}@{TEAM}")),
        role,
        colour: None,
        vendor: Vendor::Claude,
        backend,
        joined_at: None,
        presence: Presence::Present,
        pane: pane.map(PaneId::new),
        plan_mode_required: false,
        current_task: None,
    }
}

/// An absolute path value for `AbsPath` fields.
pub fn abs(p: &Path) -> AbsPath {
    AbsPath::parse(p.to_str().unwrap()).unwrap()
}
