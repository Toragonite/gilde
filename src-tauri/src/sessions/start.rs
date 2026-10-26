//! Start, retry-as-terminal and sign-in (contract §4.2.2, §4.2.7, §6.0–§6.3)
//! and the dispatch by control mode every launch goes through.
//!
//! Owner: T10.
//!
//! **One launch path.** `session_start`, `session_continue`,
//! `session_retry_as_terminal`, `account_sign_in` and Continue all build a
//! [`LaunchRequest`] and call [`SessionsManager::launch`], which
//!
//! 1. resolves the vendor binary at spawn (realpath, fingerprint, `--version`,
//!    the compat record of **that** version — A1; recorded on the session and
//!    never re-derived for it);
//! 2. composes the §6.0 environment for the session's account (C8: the
//!    default login's variable stays unset);
//! 3. dispatches by control mode — Conversation (`ConvoDriver::spawn`, T05),
//!    Terminal (`PtyApi::open` + `PtyDriver`, T06; hosted leads take
//!    PaneHost's lead env, T07), turn-based (`TurnDriver`, T08);
//! 4. binds the spawn id to the session uid right after the spawn returns
//!    (A20.1) and records the transport (`set_control_detail`, A18.1);
//! 5. starts the session's actor.
//!
//! Every spawn goes through `SupervisorApi` inside the transports (A12.8);
//! nothing here forks a process.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;

use crate::agents::{AgentKind, AgentRegistry};
use crate::contract::account::{Account, SignIn};
use crate::contract::activity::{GildeFacts, ProcessFacts};
use crate::contract::common::{AbsPath, CompatState, PermissionMode, Vendor};
use crate::contract::error::{ErrorCode, IpcError};
use crate::contract::ids::{AccountId, LocationId, ProcessKey, ProjectId, SessionUid};
use crate::contract::ipc::{SendPriority, SessionStartReq, StartLocation, VendorOption};
use crate::contract::project::LocationKind;
use crate::contract::services::{
    EvidenceSink, IdentityHint, OutgoingMessage, PtyExitSink, PtyOpenSpec, SessionMetaFacts,
    SidecarApi, SourceFacts, SupervisorApi,
};
use crate::contract::activity::{ActivityState, DidntStartReason, ErrorSub};
use crate::contract::session::{ControlMode, Transport};
use crate::contract::sidecar::{AccountEnv, ProbeParams};
use crate::convo::driver::{ConvoDeps, ConvoDriver, ConvoSpec};
use crate::convo::frames::AbandonedAsk;
use crate::convo::handshake::LaunchKind;
use crate::agents::launch::FirstPrompt;
use crate::pty::{PtyDriver, PtyManager, ReadyOutcome};
use crate::supervise::registry::{EnvBase, Supervisor};
use crate::turns::driver::{TurnDeps, TurnDriver, TurnSessionSpec};

use super::actor::{
    ActorCtx, ActorDriver, ActorExitSink, ActorHandle, ActorInfo, RehomingContent, SessionHooks,
    SessionSink, TerminalDriver, TurnBasedDriver,
};
use super::manager::{
    Binaries, ControlFacts, ConvoWiring, EnvSource, ProjectPrefs, ResolvedVendor, SessionsManager,
    TerminalStarted, Transports,
};
use super::options::{permission_modes, StaticModels};

/// Teams in a lead without pane hosting (§7.4.9, T09b worklist).
pub use crate::panehost::lead_env::ENV_AGENT_TEAMS;

/// Variables a child never receives from Gilde's own environment (§6.0).
const RESERVED_PREFIXES: [&str; 5] = ["TMUX", "ITERM_", "LC_TERMINAL", "CLAUDE_CODE_MESSAGING_", "GILDE_"];
const RESERVED_NAMES: [&str; 4] = [
    "CLAUDECODE",
    "CLAUDE_CODE_TEAMMATE_COMMAND",
    "CLAUDE_CODE_PROCESS_WRAPPER",
    "CLAUDE_CODE_ENTRYPOINT",
];
/// The account variables of §6.0 (C8).
const ACCOUNT_VARS: [&str; 3] = ["CLAUDE_CONFIG_DIR", "CODEX_HOME", "AGY_HOME"];
/// The markers `compose_env` stamps last (§6.0); kept.
const MARKERS: [&str; 2] = ["GILDE_SPAWN_ID", "GILDE_APP_INSTANCE"];

// ---------------------------------------------------------------------------
// the launch request
// ---------------------------------------------------------------------------

/// What a launch continues or creates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Launch {
    /// A new conversation (claude: `--session-id <uuid minted here>`).
    New,
    /// Resume in place (same uid).
    Resume { id: String },
    /// A copy (`--resume <id> --fork-session`, new uid).
    Fork { id: String },
    /// `claude attach <id>` of a background job (same uid).
    Attach { id: String },
    /// A copy on another account by transcript path (§6.5, until LP-25).
    OtherAccount { transcript: AbsPath },
    /// The CLI's own sign-in (§4.2.7).
    SignIn,
}

/// A resolved start, continue or retry: kept on the actor so that
/// `session_retry_as_terminal` relaunches the same request.
#[derive(Clone)]
pub struct LaunchRequest {
    pub vendor: Vendor,
    pub account: Account,
    pub project_id: Option<ProjectId>,
    pub location_id: Option<LocationId>,
    pub cwd: AbsPath,
    pub model: Option<String>,
    pub permission_mode: PermissionMode,
    pub mode: ControlMode,
    /// A hosted (or in-process) team lead.
    pub team: bool,
    /// **[content]** the first message (memory only).
    pub prompt: Option<String>,
    pub kind: Launch,
    /// A Conversation that this binary cannot run starts in Terminal mode
    /// instead (Continue, Continue all, a start that raced a compat change).
    pub fallback_to_terminal: bool,
    /// F-05: a resumed session that was waiting for an answer.
    pub abandoned: Option<AbandonedAsk>,
}

impl std::fmt::Debug for LaunchRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaunchRequest")
            .field("vendor", &self.vendor)
            .field("account", &self.account.id)
            .field("mode", &self.mode)
            .field("team", &self.team)
            .field("kind", &self.kind)
            .field("prompt", &self.prompt.as_ref().map(|p| p.len()))
            .finish_non_exhaustive()
    }
}

/// claude: Conversation|Terminal; codex/cursor/agy: TurnBased|Terminal (§4.2.2).
pub fn mode_allowed(vendor: Vendor, mode: ControlMode) -> bool {
    match vendor {
        Vendor::Claude => matches!(mode, ControlMode::Conversation | ControlMode::Terminal),
        _ => matches!(mode, ControlMode::TurnBased | ControlMode::Terminal),
    }
}

/// Why Conversation cannot run on this binary (§3.10): `None` = it can.
pub fn conversation_refusal(compat: Option<&crate::contract::compat::ClaudeCompat>) -> Option<&'static str> {
    let c = compat?;
    if c.below_minimum {
        return Some("conversationMinVersion");
    }
    if c.conversation == CompatState::Failed {
        return Some("conversationUnavailable");
    }
    None
}

/// The driver's own refusal of Conversation (T05: `UNSUPPORTED{conversation |
/// conversationMinVersion}`) → start in Terminal mode instead.
fn convo_unavailable(e: &IpcError) -> bool {
    e.code == ErrorCode::Unsupported
        && matches!(
            e.params.get("cap").and_then(|c| c.as_str()),
            Some("conversation" | "conversationMinVersion" | "conversationUnavailable")
        )
}

/// World says the session can take input (registry idle latched, §6.2.2).
pub(crate) fn ready_like(s: &ActivityState) -> bool {
    matches!(s, ActivityState::Ready | ActivityState::NewReply)
}

/// Error·Didn't start{CompatFailed|MinVersion} (the `session_retry_as_terminal` case).
pub(crate) fn compat_start_failure(s: &ActivityState) -> bool {
    matches!(
        s,
        ActivityState::Error { sub: ErrorSub::DidntStart { reason: DidntStartReason::CompatFailed | DidntStartReason::MinVersion } }
    )
}

fn mint_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn mint_msg_id() -> crate::contract::ids::ClientMsgId {
    crate::contract::ids::ClientMsgId::parse(mint_uuid()).expect("a v4 uuid is a ClientMsgId")
}

fn non_empty(s: &Option<String>) -> Option<String> {
    s.as_ref().filter(|t| !t.is_empty()).cloned()
}

// ---------------------------------------------------------------------------
// session_start
// ---------------------------------------------------------------------------

impl SessionsManager {
    /// `session_start` (§4.2.2): the order of checks of §4.1 rule 3, then the
    /// worktree (if any) and the launch. Idempotency is the caller's.
    pub(crate) async fn start_session(&self, req: SessionStartReq) -> Result<SessionUid, IpcError> {
        self.spawn_gate().await?;
        let snapshot = self.deps.world.snapshot();
        // (b) value rules that need the vendor's lists.
        let mode = req.permission_mode.unwrap_or(PermissionMode::Default);
        if !permission_modes(req.vendor).contains(&mode) {
            if mode == PermissionMode::BypassPermissions {
                return Err(IpcError::refused("bypassNotAllowed"));
            }
            return Err(IpcError::invalid_argument("permissionMode", "range"));
        }
        let model = non_empty(&req.model);
        if let Some(m) = &model {
            if !self.model_known(req.vendor, m) {
                return Err(IpcError::invalid_argument("model", "range"));
            }
        }
        // (c) existence.
        let project = snapshot
            .projects
            .iter()
            .find(|p| p.id == req.project_id)
            .cloned()
            .ok_or_else(|| IpcError::not_found("project"))?;
        let account = self
            .deps
            .accounts
            .all()
            .await
            .into_iter()
            .find(|a| a.id == req.account_id)
            .ok_or_else(|| IpcError::not_found("account"))?;
        if account.vendor != req.vendor {
            return Err(IpcError::invalid_argument("accountId", "kindForKey"));
        }
        let location = match &req.location {
            StartLocation::Here { location_id: Some(id) } | StartLocation::ExistingWorktree { location_id: id } => {
                let loc = snapshot
                    .locations
                    .iter()
                    .find(|l| &l.id == id && l.project_id == project.id && l.exists)
                    .cloned()
                    .ok_or_else(|| IpcError::not_found("location"))?;
                if matches!(req.location, StartLocation::ExistingWorktree { .. }) && loc.kind != LocationKind::Worktree {
                    return Err(IpcError::invalid_argument("location.locationId", "kindForKey"));
                }
                Some(loc)
            }
            StartLocation::Here { location_id: None } => {
                snapshot.locations.iter().find(|l| l.project_id == project.id && l.path == project.root).cloned()
            }
            StartLocation::NewWorktree { .. } => None,
        };
        // (d) capability.
        if !mode_allowed(req.vendor, req.control_mode) {
            return Err(IpcError::unsupported("modeNotAllowed"));
        }
        if req.team && !(req.vendor == Vendor::Claude && req.control_mode == ControlMode::Terminal) {
            return Err(IpcError::unsupported("teamNotAllowed"));
        }
        if !account.usable_for_new {
            return Err(IpcError::refused("accountNotConfirmed"));
        }
        if account.sign_in == SignIn::SignedOut {
            return Err(IpcError::refused("signInNeeded"));
        }
        let resolved = self.resolve_binary(req.vendor).await?;
        if req.control_mode == ControlMode::Conversation {
            if let Some(cap) = conversation_refusal(resolved.compat.as_ref()) {
                return Err(IpcError::unsupported(cap));
            }
        }
        if matches!(req.location, StartLocation::NewWorktree { .. }) && self.deps.git.is_none() {
            return Err(IpcError::refused("worktreeGate"));
        }
        // (e) execution.
        let (cwd, location_id, location_kind) = match &req.location {
            StartLocation::NewWorktree { slug, base } => {
                let git = self.deps.git.clone().ok_or_else(|| IpcError::refused("worktreeGate"))?;
                let facts = git.create_worktree(&project.id, slug, base.as_deref().filter(|b| !b.is_empty())).await?;
                self.deps.world.sink().location(facts.clone());
                (facts.location_path.clone(), Some(facts.location_id.clone()), "newWorktree")
            }
            StartLocation::ExistingWorktree { .. } => {
                let loc = location.clone().expect("checked above");
                (loc.path.clone(), Some(loc.id.clone()), "existingWorktree")
            }
            StartLocation::Here { .. } => match &location {
                Some(loc) => (loc.path.clone(), Some(loc.id.clone()), "here"),
                None => (project.root.clone(), None, "here"),
            },
        };
        let request = LaunchRequest {
            vendor: req.vendor,
            account: account.clone(),
            project_id: Some(project.id.clone()),
            location_id,
            cwd,
            model: model.clone(),
            permission_mode: mode,
            mode: req.control_mode,
            team: req.team,
            prompt: non_empty(&req.prompt),
            kind: Launch::New,
            fallback_to_terminal: true,
            abandoned: None,
        };
        let uid = self.launch(SessionUid::mint(), request, Some(resolved)).await?;
        self.deps
            .prefs
            .put(
                &project.id,
                ProjectPrefs {
                    last_vendor: Some(req.vendor),
                    last_account_id: Some(account.id.clone()),
                    last_model: model,
                    last_control_mode: Some(req.control_mode),
                    last_location_kind: Some(location_kind.to_string()),
                },
            )
            .await;
        Ok(uid)
    }

    /// A model the vendor offers (the live list seen by `newsession_options`,
    /// else the static fallback; a vendor with neither accepts any id that
    /// passed the pattern rule).
    pub(crate) fn model_known(&self, vendor: Vendor, model: &str) -> bool {
        let seen = self.models_seen(vendor);
        let fallback = StaticModels::for_vendor(vendor);
        if seen.is_empty() && fallback.is_empty() {
            return true;
        }
        seen.iter().any(|m| m == model) || fallback.contains(&model)
    }

    /// Binary resolution at spawn (§6.0) with the detection wait.
    pub(crate) async fn resolve_binary(&self, vendor: Vendor) -> Result<ResolvedVendor, IpcError> {
        match tokio::time::timeout(self.deps.timing.detect_wait, self.deps.binaries.resolve(vendor)).await {
            Ok(r) => r,
            Err(_) => Err(IpcError::not_ready("agentsNotDetected")),
        }
    }

    // -----------------------------------------------------------------------
    // the one launch path
    // -----------------------------------------------------------------------

    /// Launch `req` as session `uid` (see the module docs). `resolved` skips a
    /// second resolution when the caller already resolved the binary.
    pub(crate) async fn launch(&self, uid: SessionUid, req: LaunchRequest, resolved: Option<ResolvedVendor>) -> Result<SessionUid, IpcError> {
        let resolved = match resolved {
            Some(r) => r,
            None => self.resolve_binary(req.vendor).await?,
        };
        match req.mode {
            ControlMode::Conversation => {
                if let Some(cap) = conversation_refusal(resolved.compat.as_ref()) {
                    if !req.fallback_to_terminal {
                        return Err(IpcError::unsupported(cap));
                    }
                    return self.launch_terminal(uid, terminal_of(req), resolved).await;
                }
                match self.launch_convo(uid.clone(), req.clone(), resolved.clone()).await {
                    Err(e) if convo_unavailable(&e) && req.fallback_to_terminal => {
                        self.launch_terminal(uid, terminal_of(req), resolved).await
                    }
                    other => other,
                }
            }
            ControlMode::Terminal => self.launch_terminal(uid, req, resolved).await,
            ControlMode::TurnBased => self.launch_turns(uid, req).await,
        }
    }

    fn actor_ctx(&self, sink: Arc<SessionSink>) -> ActorCtx {
        ActorCtx {
            sink,
            binder: self.deps.binder.clone(),
            panehost: self.deps.panehost.clone(),
            manager: self.me.clone(),
        }
    }

    fn new_sink(&self, uid: &SessionUid, vendor: Vendor, account: &Account, mode: ControlMode) -> Arc<SessionSink> {
        SessionSink::new(
            uid.clone(),
            vendor,
            Some(account.id.clone()),
            mode,
            self.deps.world.sink(),
            self.deps.binder.clone(),
            self.deps.clock.clone(),
        )
    }

    /// The §6.0 environment composer of a session's account.
    async fn account_env(&self, account: &Account) -> Option<AccountEnv> {
        self.deps.accounts.env_for(&account.id).await
    }

    async fn launch_convo(&self, uid: SessionUid, req: LaunchRequest, resolved: ResolvedVendor) -> Result<SessionUid, IpcError> {
        let launch = match &req.kind {
            Launch::New => LaunchKind::New { session_id: mint_uuid() },
            Launch::Resume { id } => LaunchKind::Continue { session_id: id.clone() },
            Launch::Fork { id } => LaunchKind::Copy { session_id: id.clone() },
            Launch::OtherAccount { transcript } => LaunchKind::OtherAccount { transcript: transcript.clone() },
            Launch::Attach { .. } | Launch::SignIn => return Err(IpcError::unsupported("continueOption")),
        };
        let known_id = launch.known_session_id().map(str::to_string);
        let sink = self.new_sink(&uid, Vendor::Claude, &req.account, ControlMode::Conversation);
        sink.set_vendor_session_id(known_id.clone());
        let hooks = SessionHooks::new(self.deps.binder.clone(), self.deps.accounts.clone());
        let account_env = self.account_env(&req.account).await;
        let env_source = self.deps.env.clone();
        let composer: crate::convo::driver::EnvComposer =
            Arc::new(move |marker: &str| env_source.compose(Vendor::Claude, account_env.as_ref(), marker));
        let allow_bypass = req.permission_mode == PermissionMode::BypassPermissions;
        let spec = ConvoSpec {
            uid: uid.clone(),
            hint: IdentityHint {
                vendor: Some(Vendor::Claude),
                account_id: Some(req.account.id.clone()),
                vendor_session_id: known_id,
                process: None,
                gilde_spawn_id: None,
            },
            account_id: Some(req.account.id.clone()),
            config_dir: req.account.config_dir.clone(),
            binary: resolved.binary.clone(),
            cwd: req.cwd.clone(),
            home: self.deps.env.home(),
            launch,
            permission_mode: req.permission_mode,
            model: req.model.clone(),
            allow_bypass,
            first_prompt: req.prompt.clone().map(|text| OutgoingMessage { client_msg_id: mint_msg_id(), text, priority: SendPriority::Next }),
            abandoned: req.abandoned.clone(),
            env: composer,
        };
        let driver = self
            .deps
            .transports
            .conversation(spec, ConvoWiring { evidence: sink.clone(), hooks })
            .await?;
        let now = self.now();
        sink.publish(SourceFacts::Meta(Box::new(meta_facts(&req, &resolved, now))));
        self.deps.binder.set_control_detail(&uid, ControlFacts { pty: None, transport: Some(Transport::L1), hosted_team: false });
        let info = ActorInfo {
            vendor: Vendor::Claude,
            account_id: Some(req.account.id.clone()),
            mode: ControlMode::Conversation,
            transport: Transport::L1,
            pty: None,
            pid: None,
            hosted_team: false,
            launched_bypass: allow_bypass,
            launch: Some(req),
            started_at: now,
        };
        let handle = ActorHandle::start(driver, info, self.actor_ctx(sink));
        self.insert_actor(handle);
        Ok(uid)
    }

    async fn launch_terminal(&self, uid: SessionUid, req: LaunchRequest, resolved: ResolvedVendor) -> Result<SessionUid, IpcError> {
        let account_env = self.account_env(&req.account).await;
        let mut env = self.deps.env.compose(req.vendor, account_env.as_ref(), "");
        let mut session_id: Option<String> = None;
        let mut hosted = false;
        let mut in_process_team = false;
        if req.team {
            match self.deps.panehost.as_ref().map(|h| h.lead_env(&uid)) {
                Some(Ok(lead)) => {
                    // T07's rule: `remove` (and the §7.4.1 absent names, with
                    // their wildcards) first, then `set`.
                    crate::panehost::lead_env::apply(&mut env, &lead);
                    hosted = true;
                }
                // §7.4.9: no hosting → the lead runs its teammates in-process.
                _ => {
                    env.insert(ENV_AGENT_TEAMS.to_string(), "1".to_string());
                    in_process_team = true;
                }
            }
        }
        let launch = terminal_launch(&req, &mut session_id)?;
        let mut argv = crate::agents::launch::terminal_argv(&resolved.binary.realpath, &launch)?;
        if in_process_team {
            set_teammate_mode(&mut argv.args, "in-process");
        }
        if req.team && matches!(req.kind, Launch::Resume { .. }) {
            // A resumed hosted lead resumes as a hosted lead with a new, empty
            // team (§6.5 rule 3).
            add_lead_flags(&mut argv.args, if hosted { "tmux" } else { "in-process" }, req.permission_mode);
        }
        let typed = match &argv.prompt {
            FirstPrompt::TypedAfterReady(t) => Some(t.clone()),
            _ => None,
        };
        let spec = PtyOpenSpec {
            kind: argv.kind,
            argv0: argv.argv0.clone(),
            args: argv.args.clone(),
            cwd: req.cwd.clone(),
            env: env.into_iter().collect(),
            cols: crate::pty::session::FALLBACK_COLS,
            rows: crate::pty::session::FALLBACK_ROWS,
            parent_uid: Some(uid.clone()),
        };
        let exit_sink = ActorExitSink::new();
        let TerminalStarted { handle, driver } =
            self.deps.transports.terminal(spec, exit_sink.clone() as Arc<dyn PtyExitSink>).await?;
        let now = self.now();
        let sink = self.new_sink(&uid, req.vendor, &req.account, ControlMode::Terminal);
        if let Some(id) = &session_id {
            sink.set_vendor_session_id(Some(id.clone()));
        }
        let start_sec = crate::supervise::procscan::start_time_of(handle.pid);
        if let Some(s) = start_sec.and_then(|s| u64::try_from(s).ok()) {
            sink.set_process(ProcessKey::new(handle.pid.max(0) as u32, s));
        }
        // A20.1: bind right after the open returns, before our own facts.
        sink.bind(&handle.ticket.spawn_id);
        sink.init_gilde(GildeFacts {
            spawned_at: now,
            mode: ControlMode::Terminal,
            ready_at: None,
            stop_cause: None,
            signals_sent: Vec::new(),
            start_failure: None,
        });
        if let Some(start_sec) = start_sec {
            sink.publish(SourceFacts::Process(ProcessFacts::Alive { pid: handle.pid, start_sec, cpu_active_at: None }));
        }
        let mut resolved = resolved;
        resolved.binary.pidpath = libproc::proc_pid::pidpath(handle.pid).ok().and_then(|p| AbsPath::parse(p).ok());
        sink.publish(SourceFacts::Meta(Box::new(meta_facts(&req, &resolved, now))));
        self.deps.binder.set_control_detail(
            &uid,
            ControlFacts { pty: Some(handle.pty_id.clone()), transport: Some(Transport::Pty), hosted_team: hosted },
        );
        if hosted {
            if let Some(host) = &self.deps.panehost {
                host.lead_started(&uid, handle.pid);
            }
        }
        let info = ActorInfo {
            vendor: req.vendor,
            account_id: Some(req.account.id.clone()),
            mode: ControlMode::Terminal,
            transport: Transport::Pty,
            pty: Some(handle.pty_id.clone()),
            pid: Some(handle.pid),
            hosted_team: hosted,
            launched_bypass: req.permission_mode == PermissionMode::BypassPermissions,
            launch: Some(req),
            started_at: now,
        };
        let actor = ActorHandle::start(driver, info, self.actor_ctx(sink.clone()));
        actor.connect_exit(&exit_sink);
        self.insert_actor(actor.clone());
        self.spawn_ready_watch(uid.clone(), handle.pty_id.clone(), actor, typed);
        Ok(uid)
    }

    /// §6.2.2: readiness = the PTY engine's Idle (T06) or World's latched
    /// ready (claude registry idle for the pid), whichever first; then
    /// `ready_at` and the typed-after-ready first prompt.
    fn spawn_ready_watch(&self, uid: SessionUid, pty: crate::contract::ids::PtyId, actor: Arc<ActorHandle>, typed: Option<String>) {
        let transports = self.deps.transports.clone();
        let world = self.deps.world.clone();
        let timing = self.deps.timing.clone();
        let clock = self.deps.clock.clone();
        tokio::spawn(async move {
            let world_ready = async {
                let deadline = tokio::time::Instant::now() + timing.ready_timeout;
                loop {
                    if world.session(&uid).is_some_and(|s| ready_like(&s.activity.state)) {
                        return true;
                    }
                    if tokio::time::Instant::now() >= deadline {
                        return false;
                    }
                    tokio::time::sleep(timing.liveness_poll).await;
                }
            };
            let ready = tokio::select! {
                o = transports.terminal_ready(&pty, timing.ready_timeout) => o == ReadyOutcome::Ready,
                r = world_ready => r,
            };
            if !ready || !actor.is_live().await {
                return;
            }
            let at = clock();
            actor.sink().update_gilde(|g| {
                if g.ready_at.is_none() {
                    g.ready_at = Some(at);
                }
            });
            if let Some(text) = typed {
                actor.first_prompt(text);
            }
        });
    }

    async fn launch_turns(&self, uid: SessionUid, req: LaunchRequest) -> Result<SessionUid, IpcError> {
        let resume_id = match &req.kind {
            Launch::New => None,
            Launch::Resume { id } => Some(id.clone()),
            _ => return Err(IpcError::unsupported("continueOption")),
        };
        let sink = self.new_sink(&uid, req.vendor, &req.account, ControlMode::TurnBased);
        // A held session has no process: its spawn token is ours (bound now).
        let token = format!("tb_{}", uuid::Uuid::now_v7().simple());
        sink.bind(&token);
        sink.set_vendor_session_id(resume_id.clone());
        sink.expect_vendor_session_id(resume_id.clone());
        let spec = TurnSessionSpec {
            uid: uid.clone(),
            vendor: req.vendor,
            account_id: req.account.id.clone(),
            cwd: req.cwd.clone(),
            model: req.model.clone(),
            permission_mode: Some(req.permission_mode),
            resume_id: resume_id.clone(),
            hint: IdentityHint {
                vendor: Some(req.vendor),
                account_id: Some(req.account.id.clone()),
                vendor_session_id: resume_id,
                process: None,
                gilde_spawn_id: Some(token),
            },
            timeout_ms: None,
        };
        let driver = self.deps.transports.turn_based(spec, sink.clone() as Arc<dyn EvidenceSink>, sink.content_uid());
        self.deps.binder.set_control_detail(&uid, ControlFacts { pty: None, transport: Some(Transport::Turns), hosted_team: false });
        let now = self.now();
        let prompt = req.prompt.clone();
        let info = ActorInfo {
            vendor: req.vendor,
            account_id: Some(req.account.id.clone()),
            mode: ControlMode::TurnBased,
            transport: Transport::Turns,
            pty: None,
            pid: None,
            hosted_team: false,
            launched_bypass: false,
            launch: Some(req),
            started_at: now,
        };
        let actor = ActorHandle::start(driver, info, self.actor_ctx(sink));
        self.insert_actor(actor.clone());
        if let Some(text) = prompt {
            if let Err(e) = actor.send(mint_msg_id(), text, SendPriority::Next, false).await {
                let (code, key, _) = e.log_fields();
                tracing::warn!(target: "sessions", code, key, "the first turn did not start");
            }
        }
        Ok(uid)
    }

    // -----------------------------------------------------------------------
    // session_retry_as_terminal, account_sign_in
    // -----------------------------------------------------------------------

    /// `session_retry_as_terminal` (§4.2.2): only for Error·Didn't start
    /// {CompatFailed|MinVersion} with no vendor transcript; the original start
    /// request again, in Terminal mode, with a fresh `--session-id`, same uid.
    pub(crate) async fn retry_terminal(&self, uid: SessionUid) -> Result<SessionUid, IpcError> {
        let session = self.deps.world.session(&uid).ok_or_else(|| IpcError::not_found("session"))?;
        if !compat_start_failure(&session.activity.state) || self.deps.world.transcript(&uid).is_some() {
            return Err(IpcError::refused("notEligible"));
        }
        let launch = self
            .actor(&uid)
            .and_then(|a| a.info().launch)
            .filter(|l| l.vendor == Vendor::Claude && l.mode == ControlMode::Conversation && l.kind == Launch::New)
            .ok_or_else(|| IpcError::refused("notEligible"))?;
        self.spawn_gate().await?;
        let req = LaunchRequest { mode: ControlMode::Terminal, kind: Launch::New, ..launch };
        self.launch(uid, req, None).await
    }

    /// `account_sign_in` (§4.2.7): a Terminal-mode session running the CLI's
    /// own sign-in with that account's env; Gilde handles no credentials.
    pub(crate) async fn sign_in(&self, account_id: &AccountId) -> Result<SessionUid, IpcError> {
        self.rates.check(super::manager::Rate::Spawn, None)?;
        self.spawn_gate().await?;
        let account = self
            .deps
            .accounts
            .all()
            .await
            .into_iter()
            .find(|a| &a.id == account_id)
            .ok_or_else(|| IpcError::not_found("account"))?;
        let resolved = self.resolve_binary(account.vendor).await?;
        let cwd = self.deps.env.home().ok_or_else(|| IpcError::not_ready("home"))?;
        let req = LaunchRequest {
            vendor: account.vendor,
            account,
            project_id: None,
            location_id: None,
            cwd,
            model: None,
            permission_mode: PermissionMode::Default,
            mode: ControlMode::Terminal,
            team: false,
            prompt: None,
            kind: Launch::SignIn,
            fallback_to_terminal: false,
            abandoned: None,
        };
        self.launch(SessionUid::mint(), req, Some(resolved)).await
    }
}

/// The same request in Terminal mode.
fn terminal_of(req: LaunchRequest) -> LaunchRequest {
    LaunchRequest { mode: ControlMode::Terminal, ..req }
}

/// The §6.2.1 row of a launch. `session_id` receives the id a new claude
/// session gets (`--session-id`, minted here).
pub(crate) fn terminal_launch(
    req: &LaunchRequest,
    session_id: &mut Option<String>,
) -> Result<crate::agents::launch::TerminalLaunch, IpcError> {
    use crate::agents::launch::TerminalLaunch as T;
    let model = req.model.clone();
    let prompt = req.prompt.clone();
    let mode = req.permission_mode;
    Ok(match (req.vendor, &req.kind) {
        (Vendor::Claude, Launch::New) => {
            let id = mint_uuid();
            *session_id = Some(id.clone());
            if req.team {
                T::ClaudeHostedLead { session_id: id, permission_mode: mode, model, prompt }
            } else {
                T::ClaudeNew { session_id: id, permission_mode: mode, model, prompt }
            }
        }
        (Vendor::Claude, Launch::Resume { id }) => {
            *session_id = Some(id.clone());
            T::ClaudeResume { session_id: id.clone(), fork: false, permission_mode: mode, model, prompt }
        }
        (Vendor::Claude, Launch::Fork { id }) => T::ClaudeResume { session_id: id.clone(), fork: true, permission_mode: mode, model, prompt },
        (Vendor::Claude, Launch::Attach { id }) => {
            *session_id = Some(id.clone());
            T::ClaudeAttach { session_id: id.clone() }
        }
        (Vendor::Claude, Launch::SignIn) => T::ClaudeSignIn,
        (Vendor::Codex, Launch::New) => T::Codex { resume: None, model, prompt },
        (Vendor::Codex, Launch::Resume { id }) => {
            *session_id = Some(id.clone());
            T::Codex { resume: Some(id.clone()), model, prompt }
        }
        (Vendor::Codex, Launch::SignIn) => T::CodexSignIn,
        (Vendor::Cursor, Launch::New) => T::Cursor { resume: None, model, prompt },
        (Vendor::Cursor, Launch::Resume { id }) => {
            *session_id = Some(id.clone());
            T::Cursor { resume: Some(id.clone()), model, prompt }
        }
        (Vendor::Cursor, Launch::SignIn) => T::CursorSignIn,
        (Vendor::Agy, Launch::New) => T::Agy { conversation: None, prompt },
        (Vendor::Agy, Launch::Resume { id }) => {
            *session_id = Some(id.clone());
            T::Agy { conversation: Some(id.clone()), prompt }
        }
        (Vendor::Agy, Launch::SignIn) => T::AgySignIn,
        _ => return Err(IpcError::unsupported("continueOption")),
    })
}

/// `--teammate-mode <mode>` in an argv that has it.
fn set_teammate_mode(args: &mut [String], mode: &str) {
    if let Some(i) = args.iter().position(|a| a == "--teammate-mode") {
        if let Some(v) = args.get_mut(i + 1) {
            *v = mode.to_string();
        }
    }
}

/// A resumed hosted lead: `--teammate-mode <m>` and the lead's inline
/// `--settings` before any `--` (§6.2.1, frozen LP-10/LP-11).
fn add_lead_flags(args: &mut Vec<String>, teammate_mode: &str, mode: PermissionMode) {
    if args.iter().any(|a| a == "--teammate-mode") {
        return;
    }
    let at = args.iter().position(|a| a == "--").unwrap_or(args.len());
    let flags = [
        "--teammate-mode".to_string(),
        teammate_mode.to_string(),
        "--settings".to_string(),
        crate::agents::launch::lead_settings_json(mode),
    ];
    for (i, f) in flags.into_iter().enumerate() {
        args.insert(at + i, f);
    }
}

/// The metadata Gilde knows at spawn (§3.4.4): the spawn-time `BinaryRef`
/// and its version select the session's compat record for its whole life (A1).
fn meta_facts(req: &LaunchRequest, resolved: &ResolvedVendor, now: crate::contract::common::EpochMs) -> SessionMetaFacts {
    SessionMetaFacts {
        cwd: Some(req.cwd.clone()),
        started_at: Some(now),
        model: req.model.clone(),
        permission_mode: (req.vendor == Vendor::Claude || req.mode == ControlMode::TurnBased).then_some(req.permission_mode),
        binary: Some(resolved.binary.clone()),
        claude_version: if req.vendor == Vendor::Claude { resolved.binary.version.clone() } else { None },
        ..SessionMetaFacts::default()
    }
}

// ---------------------------------------------------------------------------
// the environment (§6.0, C8)
// ---------------------------------------------------------------------------

/// The account variable rule of §6.0 [C8] and the reserved names, applied to
/// a composed environment: the default login's variable stays **unset**
/// (never set to the default path); another claude/codex account sets its
/// realpath; cursor and agy get no account variable; no reserved name of
/// §6.0 passes through from Gilde's own environment.
pub fn apply_account_rule(env: &mut BTreeMap<String, String>, vendor: Vendor, account: Option<&AccountEnv>) {
    for k in ACCOUNT_VARS {
        env.remove(k);
    }
    env.retain(|k, _| {
        MARKERS.contains(&k.as_str())
            || !(RESERVED_NAMES.contains(&k.as_str()) || RESERVED_PREFIXES.iter().any(|p| k.starts_with(p)))
    });
    let Some(a) = account else { return };
    let (Some(key), Some(dir)) = (a.env_key.as_deref(), a.config_dir.as_ref()) else {
        return;
    };
    let allowed = matches!((vendor, key), (Vendor::Claude, "CLAUDE_CONFIG_DIR") | (Vendor::Codex, "CODEX_HOME"));
    if allowed && a.vendor == vendor {
        env.insert(key.to_string(), dir.as_str().to_string());
    }
}

/// The production [`EnvSource`]: T09's `Supervisor::compose_env` (allowlist,
/// passthrough, markers) plus the C8 rule. `DISABLE_AUTOUPDATER` reaches a
/// child only from Gilde's own environment or `agents.envPassthrough` (§6.0):
/// Gilde never sets it.
pub struct SupervisorEnv {
    supervisor: Arc<Supervisor>,
    base: EnvBase,
}

impl SupervisorEnv {
    pub fn new(supervisor: Arc<Supervisor>, base: EnvBase) -> SupervisorEnv {
        SupervisorEnv { supervisor, base }
    }
}

impl EnvSource for SupervisorEnv {
    fn compose(&self, vendor: Vendor, account: Option<&AccountEnv>, marker: &str) -> BTreeMap<String, String> {
        let slot = account.and_then(|a| {
            let dir = a.config_dir.as_ref().filter(|_| a.env_key.is_some())?;
            Some(crate::accounts::AccountSlot {
                id: a.account_id.as_str().to_string(),
                vendor: vendor.as_str().to_string(),
                label: String::new(),
                config_dir: Some(dir.as_str().to_string()),
                model_hint: None,
                source: crate::store::models::AccountSource::Manual,
                enabled: true,
                is_default: false,
                logged_in: true,
                last_seen_at: None,
                last_quota_at: None,
            })
        });
        let mut env = self.supervisor.compose_env(self.base.clone(), slot.as_ref(), marker);
        apply_account_rule(&mut env, vendor, account);
        if let Ok(v) = std::env::var("DISABLE_AUTOUPDATER") {
            env.entry("DISABLE_AUTOUPDATER".to_string()).or_insert(v);
        }
        env
    }

    fn home(&self) -> Option<AbsPath> {
        AbsPath::parse(self.base.home.to_string_lossy().into_owned()).ok()
    }
}

// ---------------------------------------------------------------------------
// the production transports and binaries
// ---------------------------------------------------------------------------

/// The production [`Transports`]: T05's `ConvoDriver`, T06's PTY manager and
/// `PtyDriver`, T08's `TurnDriver`. `convo`/`turns` are templates: each
/// session gets its own evidence sink, hooks and content routing.
pub struct LiveTransports {
    pub convo: ConvoDeps,
    pub pty: Arc<PtyManager>,
    pub supervisor: Arc<dyn SupervisorApi>,
    pub turns: TurnDeps,
    pub sidecar: Option<Arc<dyn SidecarApi>>,
}

#[async_trait]
impl Transports for LiveTransports {
    async fn conversation(&self, spec: ConvoSpec, wiring: ConvoWiring) -> Result<Box<dyn ActorDriver>, IpcError> {
        let mut deps = self.convo.clone();
        deps.evidence = wiring.evidence;
        deps.hooks = wiring.hooks;
        let driver = ConvoDriver::spawn(spec, deps).await?;
        Ok(Box::new(driver))
    }

    async fn terminal(&self, spec: PtyOpenSpec, on_exit: Arc<dyn PtyExitSink>) -> Result<TerminalStarted, IpcError> {
        let mut argv = vec![spec.argv0.as_str().to_string()];
        argv.extend(spec.args.iter().cloned());
        let handle = crate::contract::services::PtyApi::open(self.pty.as_ref(), spec, on_exit).await?;
        let started_at = self.pty.get(&handle.pty_id).map(|s| s.opened_at()).unwrap_or_else(|_| crate::store::now_ms());
        let technical = crate::contract::ipc::SessionTechnicalRes {
            pid: handle.pid,
            started_at,
            command_redacted: self.supervisor.redact_argv(&argv),
            parent_chain: Vec::new(),
        };
        let driver = TerminalDriver::new(self.pty.clone(), handle.pty_id.clone(), technical);
        Ok(TerminalStarted { handle, driver: Box::new(driver) })
    }

    async fn terminal_ready(&self, pty: &crate::contract::ids::PtyId, timeout: Duration) -> ReadyOutcome {
        PtyDriver::wait_ready(&self.pty, pty, timeout).await
    }

    fn turn_based(&self, spec: TurnSessionSpec, evidence: Arc<dyn EvidenceSink>, content_uid: Arc<Mutex<SessionUid>>) -> Box<dyn ActorDriver> {
        let probe = ProbeParams { vendor: spec.vendor, account_id: spec.account_id.clone() };
        let mut deps = self.turns.clone();
        deps.evidence = evidence;
        deps.content = Arc::new(RehomingContent { inner: deps.content.clone(), uid: content_uid });
        Box::new(TurnBasedDriver::new(TurnDriver::new(spec, deps), self.sidecar.clone(), probe))
    }
}

fn agent_kind(vendor: Vendor) -> AgentKind {
    match vendor {
        Vendor::Claude => AgentKind::Claude,
        Vendor::Codex => AgentKind::Codex,
        Vendor::Cursor => AgentKind::Cursor,
        Vendor::Agy => AgentKind::Antigravity,
    }
}

/// The production [`Binaries`]: T09's agent detection and
/// `agents::resolve_binary` (A1 fingerprint cache, `--version`, compat).
pub struct AgentBinaries {
    agents: Arc<AgentRegistry>,
    compat: Arc<dyn crate::contract::services::CompatApi>,
}

impl AgentBinaries {
    pub fn new(agents: Arc<AgentRegistry>, compat: Arc<dyn crate::contract::services::CompatApi>) -> AgentBinaries {
        AgentBinaries { agents, compat }
    }
}

#[async_trait]
impl Binaries for AgentBinaries {
    async fn installed(&self) -> Result<Vec<VendorOption>, IpcError> {
        // Never a fresh detection once one exists: past its TTL it outlasts
        // the sheet's 1 s, and a sheet that timed out showed "Finding
        // installed agents…" with no vendor and no team.
        let bins = match self.agents.latest() {
            Some(bins) => bins,
            None => self.agents.detect(false).await.map_err(|_| IpcError::not_ready("agentsNotDetected"))?,
        };
        // A29.1: the same mapping as `BootStatus.agents`.
        Ok(crate::lifecycle::boot::agent_rows(&bins))
    }

    async fn resolve(&self, vendor: Vendor) -> Result<ResolvedVendor, IpcError> {
        let bins = self.agents.detect(false).await.map_err(|_| IpcError::not_ready("agentsNotDetected"))?;
        let kind = agent_kind(vendor);
        let path = bins
            .iter()
            .find(|b| b.kind == kind && b.installed)
            .and_then(|b| b.abs_path.clone())
            .ok_or_else(|| IpcError::unsupported("vendorNotInstalled"))?;
        let compat = (vendor == Vendor::Claude).then_some(self.compat.as_ref());
        let r = crate::agents::resolve_binary(&path, kind, compat).await?;
        Ok(ResolvedVendor { binary: r.binary, compat: r.compat })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn account_env(vendor: Vendor, key: Option<&str>, dir: Option<&str>) -> AccountEnv {
        AccountEnv {
            account_id: AccountId::parse("a_0123456789abcdef").unwrap(),
            vendor,
            env_key: key.map(str::to_string),
            config_dir: dir.map(|d| AbsPath::parse(d).unwrap()),
        }
    }

    /// T-env-defaultAccount (C8, PM-30).
    #[test]
    fn t_env_default_account_leaves_the_variable_unset() {
        let mut env = env_of(&[("PATH", "/usr/bin"), ("CLAUDE_CONFIG_DIR", "/Users/x/.claude"), ("GILDE_SPAWN_ID", "m")]);
        apply_account_rule(&mut env, Vendor::Claude, Some(&account_env(Vendor::Claude, None, Some("/Users/x/.claude"))));
        assert!(!env.contains_key("CLAUDE_CONFIG_DIR"), "the default login's variable is unset, never the default path");
        assert_eq!(env["GILDE_SPAWN_ID"], "m");

        let mut env = env_of(&[("PATH", "/usr/bin")]);
        apply_account_rule(
            &mut env,
            Vendor::Claude,
            Some(&account_env(Vendor::Claude, Some("CLAUDE_CONFIG_DIR"), Some("/Users/x/.claude-work"))),
        );
        assert_eq!(env["CLAUDE_CONFIG_DIR"], "/Users/x/.claude-work");

        let mut env = env_of(&[]);
        apply_account_rule(&mut env, Vendor::Codex, Some(&account_env(Vendor::Codex, Some("CODEX_HOME"), Some("/Users/x/.codex-2"))));
        assert_eq!(env["CODEX_HOME"], "/Users/x/.codex-2");

        for v in [Vendor::Cursor, Vendor::Agy] {
            let mut env = env_of(&[("AGY_HOME", "/x")]);
            apply_account_rule(&mut env, v, Some(&account_env(v, Some("AGY_HOME"), Some("/Users/x/.gemini"))));
            assert!(env.keys().all(|k| !ACCOUNT_VARS.contains(&k.as_str())), "{v:?} gets no account variable");
        }
    }

    #[test]
    fn reserved_names_never_pass_through_but_the_markers_stay() {
        let mut env = env_of(&[
            ("TMUX", "/tmp/tmux-1/default,1,0"),
            ("TMUX_PANE", "%1"),
            ("ITERM_SESSION_ID", "w0"),
            ("LC_TERMINAL", "iTerm2"),
            ("CLAUDECODE", "1"),
            ("CLAUDE_CODE_ENTRYPOINT", "cli"),
            ("CLAUDE_CODE_MESSAGING_SOCKET", "/x"),
            ("GILDE_OTHER", "x"),
            ("GILDE_SPAWN_ID", "m"),
            ("GILDE_APP_INSTANCE", "i"),
            ("TERM_PROGRAM", "gilde-code"),
            ("HOME", "/Users/x"),
        ]);
        apply_account_rule(&mut env, Vendor::Claude, None);
        let keys: Vec<&str> = env.keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["GILDE_APP_INSTANCE", "GILDE_SPAWN_ID", "HOME", "TERM_PROGRAM"]);
    }

    #[test]
    fn an_account_of_another_vendor_never_sets_a_variable() {
        let mut env = env_of(&[]);
        apply_account_rule(
            &mut env,
            Vendor::Codex,
            Some(&account_env(Vendor::Claude, Some("CLAUDE_CONFIG_DIR"), Some("/Users/x/.claude-work"))),
        );
        assert!(env.is_empty());
    }

    #[test]
    fn a_resumed_lead_gets_the_lead_flags_before_the_prompt_fence() {
        let mut args: Vec<String> = ["--resume", "id", "--permission-mode", "plan"].iter().map(|s| s.to_string()).collect();
        add_lead_flags(&mut args, "tmux", PermissionMode::Plan);
        assert_eq!(&args[4..6], &["--teammate-mode".to_string(), "tmux".to_string()]);
        assert_eq!(args[6], "--settings");
        assert_eq!(args[7], r#"{"permissions":{"defaultMode":"plan"}}"#);
        let mut lead: Vec<String> = ["--teammate-mode", "tmux", "--session-id", "x"].iter().map(|s| s.to_string()).collect();
        set_teammate_mode(&mut lead, "in-process");
        assert_eq!(lead[1], "in-process");
    }

    #[test]
    fn modes_follow_the_vendor() {
        assert!(mode_allowed(Vendor::Claude, ControlMode::Conversation));
        assert!(mode_allowed(Vendor::Claude, ControlMode::Terminal));
        assert!(!mode_allowed(Vendor::Claude, ControlMode::TurnBased));
        for v in [Vendor::Codex, Vendor::Cursor, Vendor::Agy] {
            assert!(mode_allowed(v, ControlMode::TurnBased));
            assert!(mode_allowed(v, ControlMode::Terminal));
            assert!(!mode_allowed(v, ControlMode::Conversation));
        }
    }
}
