//! T02c (contract A24.2–A24.4): a copy's parent (`World::set_parent`,
//! persisted as `session_meta.parent_uid`), the tool call a transcript ends in
//! (`World::open_tool_use`, F-05), and a picked folder that is a project at
//! once (`World::register_project`) — end to end through `project_pick`,
//! `newsession_options`, `session_start` and `session_continue` against the
//! real World and fakes of every other port.
//!
//! No process is started, no vendor CLI or vendor directory is touched; the
//! picked folder lives in a hermetic temp home. Waits are event driven; no
//! wall-clock budget is asserted (A10.7).

mod fixtures;
#[path = "world_support.rs"]
mod support;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;

use gilde_code_lib::commands::project::{pick_project_with, FolderPicker, ProjectState};
use gilde_code_lib::contract::account::{Account, AccountSource, Quota, SignIn};
use gilde_code_lib::contract::activity::{ExitInfo, OpenToolUse, ProcessFacts, RegStatus, StopCause, TranscriptFacts};
use gilde_code_lib::contract::common::{AbsPath, CompatState, EpochMs, Notice, PermissionMode, Vendor};
use gilde_code_lib::contract::compat::{CanaryResult, ClaudeCompat, CompatSource};
use gilde_code_lib::contract::error::{ErrorCode, IpcError};
use gilde_code_lib::contract::ids::{
    AccountId, ClientMsgId, ClientRequestId, LocationId, ProcessKey, ProjectId, PtyId, QuitRecordId, RequestId, SessionUid,
};
use gilde_code_lib::contract::interaction::Decision;
use gilde_code_lib::contract::ipc::{
    AnswerOutcome, AppliesFrom, ContinueHow, InterruptOutcome, NewsessionOptionsReq, ScreenKeyOutcome, ScreenPromptAction,
    SendDelivery, SessionContinueReq, SessionStartReq, StartLocation, VendorOption,
};
use gilde_code_lib::contract::project::{LocationKind, ProjectKind};
use gilde_code_lib::contract::services::{
    CompatApi, EvidenceSink, EvidenceUpdate, IdentityKey, LocationFacts, OutgoingMessage, ProcSnapshot, PtyExitSink, PtyHandle, PtyOpenSpec,
    SessionActorApi, SessionDriver, SignalOutcome, SignalPlan, SourceFacts, SpawnRequest, SpawnTicket, StopOutcome, SupervisorApi,
    WorldApi,
};
use gilde_code_lib::contract::session::{BinaryFingerprint, BinaryRef, ControlMode};
use gilde_code_lib::contract::sidecar::AccountEnv;
use gilde_code_lib::contract::store::{Entity, StoreOp};
use gilde_code_lib::convo::driver::ConvoSpec;
use gilde_code_lib::convo::frames::AbandonedAsk;
use gilde_code_lib::ide::paths::PickedRoots;
use gilde_code_lib::pty::ReadyOutcome;
use gilde_code_lib::sessions::actor::ActorDriver;
use gilde_code_lib::sessions::manager::{
    AccountsPort, Binaries, ConvoWiring, EnvSource, LiveProcess, Liveness, QuitItems, ResolvedVendor, SessionDeps,
    SessionTiming, SessionsManager, TerminalStarted, Transports,
};
use gilde_code_lib::sessions::options::MemoryPrefs;
use gilde_code_lib::store::models::QuitItemRow;
use gilde_code_lib::turns::driver::TurnSessionSpec;
use gilde_code_lib::world::{MemoryMetaStore, World, WorldConfig};
use gilde_code_lib::worktree::WorldPlaces;
use support::*;

const DAY_MS: u64 = 24 * 60 * 60 * 1000;

fn abs(p: &str) -> AbsPath {
    AbsPath::parse(p).unwrap()
}

fn cid() -> ClientRequestId {
    ClientRequestId::parse(uuid::Uuid::new_v4().to_string()).unwrap()
}

/// A keyed, live (Watching) session.
fn live(rig: &Rig, n: u32) -> SessionUid {
    let h = claude_hint(Some(&sid(n)), Some(7000 + n));
    rig.ev(&h, alive(7000 + n as i32));
    rig.ev(&h, registry(RegStatus::Idle, rig.now(), None));
    rig.uid_of(&sid(n))
}

fn folder_facts(path: &str) -> LocationFacts {
    LocationFacts {
        project_id: ProjectId::derive(path),
        project_root: abs(path),
        git: false,
        location_id: LocationId::derive(path),
        location_path: abs(path),
        worktree: false,
        branch: None,
    }
}

fn open_call(id: &str, name: &str, at: EpochMs) -> OpenToolUse {
    OpenToolUse { tool_use_id: id.into(), tool_name: name.into(), at, is_question: matches!(name, "AskUserQuestion" | "ExitPlanMode") }
}

fn transcript_with(open: Option<OpenToolUse>, prompt: Option<EpochMs>, end: Option<EpochMs>, last_row: EpochMs) -> SourceFacts {
    SourceFacts::Transcript(TranscriptFacts {
        last_row_at: Some(last_row),
        open_tool_use: open,
        last_turn_end_at: end,
        last_human_prompt_at: prompt,
        last_api_error: None,
    })
}

// ---------------------------------------------------------------------------
// A24.2 — parents
// ---------------------------------------------------------------------------

/// A copy's parent is kept at once, persisted as `session_meta.parent_uid`
/// with the copy's first row (only once it has a vendor id, §3.3.2 rule 5),
/// and read back from that row after a restart.
#[test]
fn set_parent_is_persisted_with_the_copys_row_and_survives_a_restart() {
    let meta = Arc::new(MemoryMetaStore::default());
    let rig = Rig::with_meta(meta.clone());
    rig.subscribe();
    let source = live(&rig, 1);
    // The copy: a new spawn, no vendor id yet (Fork / OtherAccount, §3.3.3).
    let copy = rig.world.bind_spawn("sp_copy", None, Vendor::Claude, Some(account()));
    assert!(rig.world.set_parent(&copy, source.clone()));
    assert_eq!(rig.world.parent_of(&copy), Some(source.clone()));
    rig.pump();
    assert!(meta.rows().iter().all(|r| r.uid != copy), "no session_meta row before the copy has a vendor id");

    // Its own id arrives (system/init of the copy).
    let h = spawn_hint("sp_copy", Some(&sid(2)), Some(7002));
    rig.ev(&h, SourceFacts::Gilde(gilde(rig.now(), ControlMode::Conversation, Some(rig.now()))));
    rig.ev(&h, alive(7002));
    rig.pump();
    let row = meta.rows().into_iter().find(|r| r.uid == copy).expect("the copy's row");
    assert_eq!(row.parent_uid, Some(source.clone()));
    assert_eq!(row.vendor_session_id.as_deref(), Some(sid(2).as_str()));
    let src_row = meta.rows().into_iter().find(|r| r.uid == source).expect("the source's row");
    assert_eq!(src_row.parent_uid, None, "the source has no parent");

    // A new run: the parent comes from the persisted row, before and after
    // the copy shows up again.
    let rig2 = Rig::with_meta(meta.clone());
    rig2.subscribe();
    assert_eq!(rig2.world.parent_of(&copy), Some(source.clone()));
    let h2 = claude_hint(Some(&sid(2)), Some(7012));
    rig2.ev(&h2, alive(7012));
    rig2.ev(&h2, registry(RegStatus::Busy, rig2.now(), None));
    rig2.pump();
    assert_eq!(rig2.uid_of(&sid(2)), copy, "same uid by key");
    assert_eq!(rig2.world.parent_of(&copy), Some(source.clone()));
    let row = meta.rows().into_iter().find(|r| r.uid == copy).unwrap();
    assert_eq!(row.parent_uid, Some(source), "rewriting the row keeps the parent");
}

/// Value rules: a session is never its own parent, a uid World does not know
/// is refused, a chain never loops; a later call replaces the parent; a
/// persisted session not in this run's graph is updated in its row at once.
#[test]
fn set_parent_refuses_self_unknown_and_loops() {
    let meta = Arc::new(MemoryMetaStore::default());
    let rig = Rig::with_meta(meta.clone());
    rig.subscribe();
    let a = live(&rig, 11);
    let b = live(&rig, 12);
    let c = live(&rig, 13);
    rig.pump();

    assert!(!rig.world.set_parent(&a, a.clone()), "never its own parent");
    assert_eq!(rig.world.parent_of(&a), None);
    let unknown = SessionUid::mint();
    assert!(!rig.world.set_parent(&unknown, a.clone()), "an unknown uid is refused");
    assert_eq!(rig.world.parent_of(&unknown), None);

    assert!(rig.world.set_parent(&b, a.clone()));
    assert!(!rig.world.set_parent(&a, b.clone()), "a → b → a loops");
    assert!(rig.world.set_parent(&c, b.clone()));
    assert!(!rig.world.set_parent(&a, c.clone()), "a → c → b → a loops");
    assert_eq!(rig.world.parent_of(&a), None, "a refused call changes nothing");
    assert!(rig.world.set_parent(&c, a.clone()), "a later call replaces the parent");
    assert_eq!(rig.world.parent_of(&c), Some(a.clone()));
    // Keyed sessions are rewritten at once.
    let row_c = meta.rows().into_iter().find(|r| r.uid == c).unwrap();
    assert_eq!(row_c.parent_uid, Some(a.clone()));

    // A session persisted earlier and not seen in this run (history row).
    let dormant = rig.world.uid_resolver().uid_for(&IdentityKey {
        vendor: Vendor::Claude,
        account_id: Some(account()),
        vendor_session_id: sid(19),
    });
    assert!(rig.world.set_parent(&dormant, a.clone()));
    assert_eq!(rig.world.parent_of(&dormant), Some(a.clone()));
    let row = meta.rows().into_iter().find(|r| r.uid == dormant).unwrap();
    assert_eq!(row.parent_uid, Some(a));
}

/// Both uids follow a merge (§3.3.3): a parent recorded on the newer uid
/// moves to the older survivor, and a tombstone given as either uid means
/// the survivor.
#[test]
fn set_parent_follows_a_merge() {
    let meta = Arc::new(MemoryMetaStore::default());
    let old = {
        let rig = Rig::with_meta(meta.clone());
        rig.uid_of(&sid(22))
    };
    let rig = Rig::with_meta(meta.clone());
    rig.subscribe();
    let source = live(&rig, 21);
    // A provisional process-only session, given a parent ...
    rig.ev(&claude_hint(None, Some(7022)), alive(7022));
    rig.pump();
    let provisional = rig.snapshot_uids().into_iter().find(|u| *u != source).expect("the provisional row");
    assert!(rig.world.set_parent(&provisional, source.clone()));
    // ... learns the persisted id: the older uid survives with the parent.
    rig.ev(&claude_hint(Some(&sid(22)), Some(7022)), registry(RegStatus::Idle, rig.now(), None));
    rig.pump();
    assert_eq!(rig.session(&provisional).merged_into, Some(old.clone()));
    assert_eq!(rig.world.parent_of(&old), Some(source.clone()));
    assert_eq!(rig.world.parent_of(&provisional), Some(source.clone()), "the tombstone resolves to the survivor");
    assert_eq!(meta.rows().into_iter().find(|r| r.uid == old).unwrap().parent_uid, Some(source.clone()));
    // A tombstone as the parent means the survivor; as the child, too.
    let other = live(&rig, 23);
    assert!(rig.world.set_parent(&other, provisional.clone()));
    assert_eq!(rig.world.parent_of(&other), Some(old.clone()));
    assert!(!rig.world.set_parent(&provisional, old.clone()), "the survivor is the tombstone's own session");
}

// ---------------------------------------------------------------------------
// A24.3 — the open tool call (F-05)
// ---------------------------------------------------------------------------

#[test]
fn open_tool_use_is_the_call_the_transcript_ends_in() {
    let rig = Rig::new();
    rig.subscribe();
    let uid = live(&rig, 31);
    let h = claude_hint(Some(&sid(31)), Some(7031));
    let t = rig.now();
    assert_eq!(rig.world.open_tool_use(&uid), None, "no transcript facts");

    rig.ev(&h, transcript_with(Some(open_call("toolu_01", "AskUserQuestion", t)), Some(t - 5_000), None, t));
    let got = rig.world.open_tool_use(&uid).expect("the open call");
    assert_eq!((got.tool_use_id.as_str(), got.tool_name.as_str(), got.is_question), ("toolu_01", "AskUserQuestion", true));

    // A turn end or a prompt after the call: the transcript no longer ends in it.
    rig.ev(&h, transcript_with(Some(open_call("toolu_01", "Bash", t)), Some(t - 5_000), Some(t + 1), t + 1));
    assert_eq!(rig.world.open_tool_use(&uid), None);
    rig.ev(&h, transcript_with(Some(open_call("toolu_01", "Bash", t)), Some(t + 2), None, t + 2));
    assert_eq!(rig.world.open_tool_use(&uid), None);
    // A prompt or turn end at or before the call does not count against it.
    rig.ev(&h, transcript_with(Some(open_call("toolu_02", "Bash", t + 10)), Some(t + 10), Some(t + 3), t + 10));
    assert_eq!(rig.world.open_tool_use(&uid).map(|o| o.tool_use_id), Some("toolu_02".to_string()));
    // Meaningless values are not a call.
    rig.ev(&h, transcript_with(Some(open_call("", "Bash", t + 20)), None, None, t + 20));
    assert_eq!(rig.world.open_tool_use(&uid), None, "an empty id");
    rig.ev(&h, transcript_with(Some(open_call("toolu_03", "  ", t + 30)), None, None, t + 30));
    assert_eq!(rig.world.open_tool_use(&uid), None, "an empty name");
    rig.ev(&h, transcript_with(None, None, None, t + 40));
    assert_eq!(rig.world.open_tool_use(&uid), None, "every call answered");
    assert_eq!(rig.world.open_tool_use(&SessionUid::mint()), None, "an unknown uid");
}

// ---------------------------------------------------------------------------
// A24.4 — a picked folder is a project at once
// ---------------------------------------------------------------------------

#[test]
fn register_project_publishes_the_project_and_location_with_no_session() {
    let rig = Rig::new();
    rig.subscribe();
    let facts = folder_facts("/Users/tester/picked");
    let id = rig.world.register_project(facts.clone());
    assert_eq!(id, facts.project_id);

    // In the snapshot when the call returns (no session, no sidebar order).
    let snap = rig.world.snapshot();
    let p = snap.projects.iter().find(|p| p.id == id).expect("the project is in the store at once");
    assert_eq!((p.name.as_str(), p.kind, p.live_sessions, p.earlier, p.order), ("picked", ProjectKind::Folder, 0, false, 0));
    let l = snap.locations.iter().find(|l| l.id == facts.location_id).expect("its location");
    assert_eq!((l.project_id.clone(), l.kind, l.exists), (id.clone(), LocationKind::Folder, true));
    // ... and in the next patch.
    let ops: Vec<StoreOp> = rig.pump().into_iter().flat_map(|p| p.ops).collect();
    let upserted = |want: &dyn Fn(&Entity) -> bool| ops.iter().any(|o| matches!(o, StoreOp::Upsert { entity } if want(entity)));
    assert!(upserted(&|e| matches!(e, Entity::Project { value } if value.id == id)));
    assert!(upserted(&|e| matches!(e, Entity::Location { value } if value.id == facts.location_id)));

    // A second pick: appended after the first, nothing duplicated.
    let second = folder_facts("/Users/tester/other");
    rig.world.register_project(second.clone());
    assert_eq!(rig.world.register_project(facts.clone()), id, "picking it again returns the same project");
    let snap = rig.world.snapshot();
    assert_eq!(snap.projects.iter().filter(|p| p.id == id).count(), 1);
    assert_eq!(snap.locations.iter().filter(|l| l.id == facts.location_id).count(), 1);
    let order = |id: &ProjectId| snap.projects.iter().find(|p| &p.id == id).unwrap().order;
    assert_eq!((order(&id), order(&second.project_id)), (0, 1));

    // Unused for more than a day it becomes `earlier` (M3) but stays; a new
    // pick brings it back.
    rig.advance(DAY_MS + 1);
    rig.world.set_project_order(vec![]);
    rig.pump();
    let p = rig.world.snapshot().projects.into_iter().find(|p| p.id == id).expect("kept: World never drops a project");
    assert!(p.earlier);
    rig.world.register_project(facts);
    assert!(!rig.world.snapshot().projects.into_iter().find(|p| p.id == id).unwrap().earlier);

    // A session in it later counts normally.
    let uid = live(&rig, 41);
    rig.ev(&claude_hint(Some(&sid(41)), Some(7041)), meta_cwd("/Users/tester/picked/src"));
    rig.pump();
    assert_eq!(rig.session(&uid).project_id, Some(id.clone()));
    assert_eq!(rig.world.snapshot().projects.into_iter().find(|p| p.id == id).unwrap().live_sessions, 1);
}

// ---------------------------------------------------------------------------
// end to end: project_pick → newsession_options → session_start, and
// session_continue (A24.2/A24.3 through T10's binder)
// ---------------------------------------------------------------------------

struct Picker(PathBuf);

#[async_trait]
impl FolderPicker for Picker {
    async fn pick(&self) -> Option<PathBuf> {
        Some(self.0.clone())
    }
}

struct Driver(ControlMode);

#[async_trait]
impl SessionDriver for Driver {
    fn mode(&self) -> ControlMode {
        self.0
    }
    async fn send(&mut self, _msg: OutgoingMessage) -> Result<SendDelivery, IpcError> {
        Ok(SendDelivery::Sent)
    }
    async fn cancel_queued(&mut self, _id: &ClientMsgId) -> Result<bool, IpcError> {
        Ok(false)
    }
    async fn answer(&mut self, _req: &RequestId, _d: Decision) -> Result<AnswerOutcome, IpcError> {
        Ok(AnswerOutcome::Answered)
    }
    async fn screen_key(&mut self, _req: &RequestId, _rev: &str, _key: ScreenPromptAction) -> Result<ScreenKeyOutcome, IpcError> {
        Ok(ScreenKeyOutcome::Sent)
    }
    async fn interrupt(&mut self, _discard_queued: bool) -> Result<InterruptOutcome, IpcError> {
        Ok(InterruptOutcome::Sent)
    }
    async fn force_stop(&mut self) -> Result<bool, IpcError> {
        Ok(true)
    }
    async fn set_model(&mut self, _m: Option<String>) -> Result<AppliesFrom, IpcError> {
        Ok(AppliesFrom::NextRequest)
    }
    async fn set_permission_mode(&mut self, _m: PermissionMode) -> Result<(), IpcError> {
        Ok(())
    }
    async fn stop(&mut self, _why: StopCause) -> Result<StopOutcome, IpcError> {
        Ok(StopOutcome { stopped: true, survivors: 0 })
    }
}

#[async_trait]
impl ActorDriver for Driver {}

/// Records what would have been spawned; spawns nothing.
#[derive(Default)]
struct FakeTransports {
    convo: Mutex<Vec<ConvoSpec>>,
    pty: Mutex<Vec<PtyOpenSpec>>,
    seq: std::sync::atomic::AtomicU64,
}

#[async_trait]
impl Transports for FakeTransports {
    async fn conversation(&self, spec: ConvoSpec, wiring: ConvoWiring) -> Result<Box<dyn ActorDriver>, IpcError> {
        // Like T05's spawn: the first evidence, carrying the spawn id, goes
        // out before the driver is returned (the session sink binds it).
        let n = self.seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        let mut hint = spec.hint.clone();
        hint.gilde_spawn_id = Some(format!("sp_c{n}"));
        wiring.evidence.evidence(EvidenceUpdate {
            hint,
            facts: SourceFacts::Process(ProcessFacts::Alive { pid: 4242, start_sec: 1, cpu_active_at: None }),
            at: T0,
        });
        self.convo.lock().push(spec);
        Ok(Box::new(Driver(ControlMode::Conversation)))
    }
    async fn terminal(&self, spec: PtyOpenSpec, _on_exit: Arc<dyn PtyExitSink>) -> Result<TerminalStarted, IpcError> {
        let n = self.seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        self.pty.lock().push(spec);
        let handle = PtyHandle {
            pty_id: PtyId::mint(),
            pid: std::process::id() as i32,
            ticket: SpawnTicket { spawn_id: format!("sp_t{n}"), env_marker: format!("mk_t{n}") },
        };
        Ok(TerminalStarted { handle, driver: Box::new(Driver(ControlMode::Terminal)) })
    }
    async fn terminal_ready(&self, _pty: &PtyId, _timeout: Duration) -> ReadyOutcome {
        ReadyOutcome::Ready
    }
    fn turn_based(&self, _spec: TurnSessionSpec, _evidence: Arc<dyn EvidenceSink>, _content: Arc<Mutex<SessionUid>>) -> Box<dyn ActorDriver> {
        Box::new(Driver(ControlMode::TurnBased))
    }
}

fn claude_compat() -> ClaudeCompat {
    ClaudeCompat {
        version: "2.1.306".into(),
        conversation: CompatState::Verified,
        pane_hosting: CompatState::Unverified,
        source: CompatSource::Shipped,
        static_canary: CanaryResult::NotRun,
        handshake_ok: None,
        below_minimum: false,
        note: None,
        features: BTreeMap::new(),
    }
}

fn claude_binary() -> BinaryRef {
    BinaryRef {
        realpath: abs("/opt/claude/2.1.306/claude"),
        fingerprint: BinaryFingerprint { size: 7, mtime_ms: 1, inode: 7 },
        version: Some("2.1.306".into()),
        pidpath: None,
    }
}

struct FakeBinaries;

#[async_trait]
impl Binaries for FakeBinaries {
    async fn installed(&self) -> Result<Vec<VendorOption>, IpcError> {
        Ok(Vendor::ALL
            .iter()
            .map(|v| VendorOption { vendor: *v, installed: *v == Vendor::Claude, version: (*v == Vendor::Claude).then(|| "2.1.306".into()) })
            .collect())
    }
    async fn resolve(&self, vendor: Vendor) -> Result<ResolvedVendor, IpcError> {
        match vendor {
            Vendor::Claude => Ok(ResolvedVendor { binary: claude_binary(), compat: Some(claude_compat()) }),
            _ => Err(IpcError::unsupported("vendorNotInstalled")),
        }
    }
}

struct FakeAccounts;

#[async_trait]
impl AccountsPort for FakeAccounts {
    async fn all(&self) -> Vec<Account> {
        vec![Account {
            id: account(),
            vendor: Vendor::Claude,
            config_dir: None,
            label: "Default".into(),
            is_default: true,
            source: AccountSource::Ambient,
            usable_for_new: true,
            sign_in: SignIn::SignedIn,
            quota: None,
            live_sessions: 0,
        }]
    }
    async fn env_for(&self, id: &AccountId) -> Option<AccountEnv> {
        Some(AccountEnv { account_id: id.clone(), vendor: Vendor::Claude, env_key: None, config_dir: None })
    }
    fn observed_quota(&self, _id: &AccountId, _quota: Quota) {}
    fn signed_out(&self, _id: &AccountId) {}
}

struct FakeEnv(AbsPath);

impl EnvSource for FakeEnv {
    fn compose(&self, _vendor: Vendor, _account: Option<&AccountEnv>, marker: &str) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("PATH".to_string(), "/usr/bin:/bin".to_string()),
            ("HOME".to_string(), self.0.as_str().to_string()),
            ("GILDE_SPAWN_ID".to_string(), marker.to_string()),
        ])
    }
    fn home(&self) -> Option<AbsPath> {
        Some(self.0.clone())
    }
}

struct FakeCompat;

#[async_trait]
impl CompatApi for FakeCompat {
    fn list(&self) -> Vec<ClaudeCompat> {
        vec![claude_compat()]
    }
    fn lookup(&self, version: &str) -> Option<ClaudeCompat> {
        (version == "2.1.306").then(claude_compat)
    }
    async fn for_binary(&self, _binary: &BinaryRef) -> ClaudeCompat {
        claude_compat()
    }
    fn feature(&self, _version: &str, _flag: &str) -> bool {
        false
    }
    fn app_feature(&self, _flag: &str) -> bool {
        false
    }
    fn notices(&self, _version: &str) -> Vec<Notice> {
        Vec::new()
    }
    fn schedule_canary(&self, _binary: &BinaryRef) {}
}

/// Nothing is live anywhere.
struct NoneLive;

#[async_trait]
impl Liveness for NoneLive {
    async fn find(&self, _vendor: Vendor, _id: &str) -> Result<Vec<LiveProcess>, IpcError> {
        Ok(Vec::new())
    }
    fn alive(&self, _pid: i32, _start_sec: i64) -> bool {
        false
    }
}

struct FakeSupervisor;

#[async_trait]
impl SupervisorApi for FakeSupervisor {
    async fn spawn_gate(&self) -> Result<(), IpcError> {
        Ok(())
    }
    async fn plan_spawn(&self, _req: SpawnRequest) -> Result<SpawnTicket, IpcError> {
        Err(IpcError::refused("notAllowlisted"))
    }
    async fn mark_running(&self, _t: &SpawnTicket, _pid: i32) -> Result<(), IpcError> {
        Ok(())
    }
    async fn mark_exited(&self, _t: &SpawnTicket, _exit: ExitInfo) {}
    async fn signal(&self, _target: &ProcessKey, _plan: SignalPlan) -> Result<SignalOutcome, IpcError> {
        Ok(SignalOutcome { exited: true, survivor: false })
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

struct NoQuitItems;

#[async_trait]
impl QuitItems for NoQuitItems {
    async fn items(&self, _record: &QuitRecordId) -> Result<Vec<QuitItemRow>, IpcError> {
        Err(IpcError::not_found("quitRecord"))
    }
}

/// T10's manager over the **real** World (as `WorldApi` and `SessionBinder`).
fn manager(world: &Arc<World>, transports: &Arc<FakeTransports>, home: AbsPath) -> Arc<SessionsManager> {
    SessionsManager::new(SessionDeps {
        world: world.clone(),
        binder: world.clone(),
        supervisor: Arc::new(FakeSupervisor),
        transports: transports.clone(),
        binaries: Arc::new(FakeBinaries),
        accounts: Arc::new(FakeAccounts),
        env: Arc::new(FakeEnv(home)),
        compat: Arc::new(FakeCompat),
        liveness: Arc::new(NoneLive),
        prefs: Arc::new(MemoryPrefs::default()),
        quit_items: Arc::new(NoQuitItems),
        git: None,
        panehost: None,
        sidecar: None,
        clock: Arc::new(|| T0),
        timing: SessionTiming {
            ready_timeout: Duration::from_secs(5),
            own_leftover_wait: Duration::from_millis(600),
            liveness_poll: Duration::from_millis(20),
            options_wait: Duration::from_secs(5),
            detect_wait: Duration::from_secs(2),
            claude_stop_wait: Duration::from_secs(2),
            watched_term_grace_ms: 100,
            continue_all_parallel: 3,
        },
    })
}

/// The A24.4 bug T10 found: a folder picked a moment ago, with no session in
/// it, is a project for `newsession_options` and `session_start` at once.
#[tokio::test]
async fn a_picked_folder_with_no_session_starts_a_session() {
    let env = fixtures::Hermetic::new();
    let folder = env.home().join("pick-me");
    std::fs::create_dir_all(&folder).unwrap();
    let home = std::fs::canonicalize(env.home()).unwrap();
    let home_abs = abs(home.to_str().unwrap());
    let world = Arc::new(World::new(WorldConfig::system(Some(home_abs.as_str().to_string()), Arc::new(MemoryMetaStore::default()))));
    let world_api: Arc<dyn WorldApi> = world.clone();
    let state = ProjectState {
        picker: Arc::new(Picker(folder.clone())),
        picked: PickedRoots::new(),
        places: Some(Arc::new(WorldPlaces(world_api))),
        home: home.clone(),
    };

    let picked = pick_project_with(&state, Some(&world)).await.unwrap().project.expect("a project");
    let root = std::fs::canonicalize(&folder).unwrap();
    assert_eq!(picked.root.as_str(), root.to_str().unwrap());
    assert_eq!((picked.kind, picked.live_sessions, picked.earlier), (ProjectKind::Folder, 0, false));
    let snap = world.snapshot();
    assert!(snap.sessions.is_empty(), "no session anywhere");
    assert_eq!(snap.projects.iter().find(|p| p.id == picked.id), Some(&picked), "the answer is the project as World shows it");

    let transports = Arc::new(FakeTransports::default());
    let m = manager(&world, &transports, home_abs);
    let options = m
        .options(&NewsessionOptionsReq { project_id: picked.id.clone(), vendor: Some(Vendor::Claude) })
        .await
        .expect("newsession_options knows the picked project");
    assert!(options.vendors.iter().any(|v| v.vendor == Vendor::Claude && v.installed));

    let started = m
        .start(SessionStartReq {
            client_request_id: cid(),
            project_id: picked.id.clone(),
            vendor: Vendor::Claude,
            account_id: account(),
            model: None,
            location: StartLocation::Here { location_id: None },
            permission_mode: None,
            control_mode: ControlMode::Terminal,
            team: false,
            prompt: None,
        })
        .await
        .expect("session_start in the picked project");
    let spec = transports.pty.lock()[0].clone();
    assert_eq!(spec.cwd.as_str(), root.to_str().unwrap(), "started in the picked folder");
    world.flush_now();
    let s = world.session(&started.uid).expect("World shows the session");
    assert_eq!(s.project_id, Some(picked.id.clone()));
    assert_eq!(world.snapshot().projects.into_iter().find(|p| p.id == picked.id).unwrap().live_sessions, 1);

    // A start in a project nobody picked still answers NOT_FOUND{project}.
    let e = m
        .options(&NewsessionOptionsReq { project_id: ProjectId::derive("/nowhere"), vendor: None })
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound);
}

/// T10 on the real World: Continue fills `ConvoSpec.abandoned` from the call
/// the transcript ends in (F-05), and a copy's parent is recorded in World.
#[tokio::test]
async fn continue_fills_abandoned_and_a_copy_records_its_parent_in_world() {
    let _env = fixtures::Hermetic::new();
    let rig = Rig::new();
    rig.subscribe();
    rig.sink.location(folder_facts("/Users/tester/repo"));
    let h = claude_hint(Some(&sid(51)), Some(7051));
    let t = rig.now();
    rig.ev(&h, alive(7051));
    rig.ev(&h, meta_cwd("/Users/tester/repo"));
    rig.ev(&h, transcript_with(Some(open_call("toolu_ask", "AskUserQuestion", t)), Some(t - 1_000), None, t));
    rig.ev(&h, exited(t + 10, Some(0), None));
    rig.pump();
    let uid = rig.uid_of(&sid(51));
    assert_eq!(rig.world.open_tool_use(&uid).map(|o| o.tool_use_id), Some("toolu_ask".to_string()));

    let world = Arc::new(rig.world.clone());
    let transports = Arc::new(FakeTransports::default());
    let m = manager(&world, &transports, abs("/Users/tester"));
    let req = |how| SessionContinueReq {
        client_request_id: cid(),
        uid: uid.clone(),
        how,
        control_mode: Some(ControlMode::Conversation),
        model: None,
        permission_mode: None,
    };
    let ask = AbandonedAsk { tool_use_id: "toolu_ask".into(), tool_name: "AskUserQuestion".into() };

    // A copy: a new uid whose parent World records (and T10 reads back).
    let copy = m.continue_session(req(ContinueHow::Fork)).await.expect("fork").uid;
    assert_ne!(copy, uid);
    assert_eq!(world.parent_of(&copy), Some(uid.clone()));
    assert_eq!(m.parent_of(&copy), Some(uid.clone()));
    assert_eq!(transports.convo.lock()[0].abandoned, Some(ask.clone()), "the copy carries the same unanswered call");

    // Resume keeps the uid, has no parent, and shows the abandoned ask.
    let resumed = m.continue_session(req(ContinueHow::Resume)).await.expect("resume").uid;
    assert_eq!(resumed, uid);
    assert_eq!(world.parent_of(&uid), None);
    assert_eq!(transports.convo.lock()[1].abandoned, Some(ask));
}

// ---------------------------------------------------------------------------
// project_pick through the command: with and without a managed World
// ---------------------------------------------------------------------------

mod ipc {
    use super::*;
    use gilde_code_lib::commands::registry;
    use serde_json::{json, Value};
    use tauri::test::{get_ipc_response, mock_builder, MockRuntime, INVOKE_KEY};
    use tauri::webview::InvokeRequest;
    use tauri::{Manager as _, WebviewWindow};

    fn invoke(w: &WebviewWindow<MockRuntime>, cmd: &str, req: Value) -> Result<Value, Value> {
        get_ipc_response(
            w,
            InvokeRequest {
                cmd: cmd.into(),
                callback: tauri::ipc::CallbackFn(0),
                error: tauri::ipc::CallbackFn(1),
                url: "tauri://localhost".parse().unwrap(),
                body: tauri::ipc::InvokeBody::Json(json!({ "req": req })),
                headers: Default::default(),
                invoke_key: INVOKE_KEY.to_string(),
            },
        )
        .map(|b| b.deserialize::<Value>().unwrap_or(Value::Null))
    }

    fn app_with(state: ProjectState, world: Option<Arc<World>>) -> (tauri::App<MockRuntime>, WebviewWindow<MockRuntime>) {
        let app = mock_builder().invoke_handler(registry::handler()).build(tauri::generate_context!(test = true)).unwrap();
        app.manage(state);
        if let Some(w) = world {
            app.manage(w);
        }
        let w = tauri::WebviewWindowBuilder::new(&app, "main", Default::default()).build().unwrap();
        (app, w)
    }

    /// With no `Arc<World>` managed (main before the composition root binds
    /// one) `project_pick` answers as before; with it managed the picked
    /// project is in World's store when the command returns (A24.4).
    #[test]
    fn project_pick_with_and_without_a_managed_world() {
        let env = fixtures::Hermetic::new();
        let folder = env.home().join("cmd-pick");
        std::fs::create_dir_all(&folder).unwrap();
        let home = std::fs::canonicalize(env.home()).unwrap();
        let new_world = || Arc::new(World::new(WorldConfig::system(Some(home.to_string_lossy().into_owned()), Arc::new(MemoryMetaStore::default()))));
        let state = |world: &Arc<World>| ProjectState {
            picker: Arc::new(Picker(folder.clone())),
            picked: PickedRoots::new(),
            places: Some(Arc::new(WorldPlaces(world.clone() as Arc<dyn WorldApi>))),
            home: home.clone(),
        };

        let plain = new_world();
        let (_app, w) = app_with(state(&plain), None);
        let p = invoke(&w, "project_pick", json!({})).expect("project_pick without a managed World");
        assert_eq!((p["project"]["kind"].as_str(), p["project"]["name"].as_str()), (Some("folder"), Some("cmd-pick")));
        assert_eq!((p["project"]["earlier"].as_bool(), p["project"]["liveSessions"].as_u64()), (Some(false), Some(0)));

        let world = new_world();
        let (_app2, w2) = app_with(state(&world), Some(world.clone()));
        let p2 = invoke(&w2, "project_pick", json!({})).expect("project_pick with the managed World");
        assert_eq!(p2["project"]["id"], p["project"]["id"], "the same folder is the same project");
        let id = p2["project"]["id"].as_str().unwrap().to_string();
        let shown = world.snapshot().projects.into_iter().find(|x| x.id.as_str() == id).expect("in World's store at once");
        assert_eq!(serde_json::to_value(&shown).unwrap(), p2["project"], "the answer is the project as World shows it");
        assert!(world.snapshot().sessions.is_empty());
    }
}
