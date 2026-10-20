//! The owner of every live pty session (DESIGN §6-1, §6-5).
//!
//! `PtyManager` is the only place that starts a terminal child, and it starts
//! every one of them the same way:
//!
//! ```text
//! plan_spawn (durable row, fsync)          ← before a process exists
//!   → openpty(real winsize)
//!   → CommandBuilder(env_clear + compose_env)
//!   → slave.spawn_command
//!   → drop(pair.slave)                     ← or the master never sees EOF
//!   → mark_running(pid, pgid)
//! ```
//!
//! That order is not a style choice. The row is written *before* the fork so a
//! crash between fork and bookkeeping still leaves a trail the reaper can find;
//! the slave is dropped *immediately* after the fork because a retained slave fd
//! keeps `read()` on the master alive forever, and a session that never ends is
//! a session the user cannot close.
//!
//! The manager holds no locks across a spawn, and none of its methods block on
//! the child.

use std::collections::BTreeMap;
use std::os::fd::BorrowedFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde::{Deserialize, Serialize};
use tauri::ipc::Channel;

use crate::accounts::AccountSlot;
use crate::agents::{AgentKind, AgentRegistry};
use crate::config::ConfigStore;
use crate::error::{GildeError, Result};
use crate::events::bus::EventBus;
use crate::model::graph::{node_id, EdgeKind, GraphEdge, GraphNode, GraphOp, NodeKind, NodeStatus};
use crate::model::status::{SessionStatus, SessionStatusReport, StatusEvidence};
use crate::status::{ForegroundSource, StatusEngine}; // T16
use crate::store::models::{ProcKind, PtySessionRow};
use crate::store::{dao, now_ms, Db};
use crate::supervise::kill::{kill_tree, KillOpts, KillReport};
use crate::supervise::registry::{mask_argv_vec, EnvBase, SpawnPlan, Supervisor};

use super::ring::{clamp_ring_bytes, DEFAULT_RING_BYTES};
use super::session::{
    sanitize_size, CloseMode, PtyChunk, PtySession, PtySessionInfo, PtySignal, SendPromptOutcome,
    SessionParts, STANDARD_CLOSE_GRACE,
};

/// What `session_open` receives from the frontend (mirrored in `ipc.ts` §8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenSessionReq {
    pub project_id: String,
    pub worktree_id: String,
    pub agent: AgentKind,
    #[serde(default)]
    pub account_id: Option<String>,
    pub cols: u16,
    pub rows: u16,
    #[serde(default)]
    pub title: Option<String>,
    /// First turn, handed over on argv when the agent supports it (§6-6).
    #[serde(default)]
    pub prompt: Option<String>,
    /// Overrides the resolved launch template's argv tail.
    #[serde(default)]
    pub args: Option<Vec<String>>,
}

/// The resolved, backend-side open request.
///
/// `OpenSessionReq` is what the UI can express; this is what the manager needs.
/// Keeping them apart is what lets a test open a fixture binary in a temp dir
/// without inventing a project, a worktree and an agent registry.
#[derive(Debug, Clone)]
pub struct OpenSpec {
    pub session_id: String,
    pub project_id: String,
    pub worktree_id: String,
    pub agent: AgentKind,
    pub account: Option<AccountSlot>,
    pub title: String,
    pub cwd: PathBuf,
    /// `argv[0]` must be an absolute path — DESIGN §6-1 and identity fence 2.
    pub argv: Vec<String>,
    pub env_base: EnvBase,
    pub cols: u16,
    pub rows: u16,
    pub ring_bytes: usize,
    pub idle_after_ms: u64,
    /// Turn off the tty's output post-processing (`OPOST`/`ONLCR`).
    ///
    /// Off by default, i.e. the pty behaves like every other terminal: a lone
    /// `\n` from a naive program becomes `\r\n` and lines start at column zero.
    /// Turning it on gives a byte-transparent tty, which is what the fidelity
    /// test needs and what a caller streaming binary data would want.
    pub raw_output: bool,
}

impl OpenSpec {
    /// A spec with the defaults every field can have on its own.
    pub fn new(agent: AgentKind, cwd: impl Into<PathBuf>, argv: Vec<String>) -> OpenSpec {
        OpenSpec {
            session_id: crate::model::ids::SessionId::new().into_string(),
            project_id: String::new(),
            worktree_id: String::new(),
            agent,
            account: None,
            title: agent.key().to_string(),
            cwd: cwd.into(),
            argv,
            env_base: EnvBase::new(std::env::var("PATH").unwrap_or_default(), home_dir()),
            cols: 80,
            rows: 24,
            ring_bytes: DEFAULT_RING_BYTES,
            idle_after_ms: 1_200,
            raw_output: false,
        }
    }

    pub fn size(mut self, cols: u16, rows: u16) -> OpenSpec {
        self.cols = cols;
        self.rows = rows;
        self
    }

    pub fn raw_output(mut self, raw: bool) -> OpenSpec {
        self.raw_output = raw;
        self
    }

    pub fn ring_bytes(mut self, bytes: usize) -> OpenSpec {
        self.ring_bytes = bytes;
        self
    }
}

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/"))
}

/// Owner of the live session table.
pub struct PtyManager {
    supervisor: Arc<Supervisor>,
    config: Arc<ConfigStore>,
    db: Db,
    events: Option<EventBus>,
    agents: Option<Arc<AgentRegistry>>,
    accounts: Arc<Vec<AccountSlot>>,
    /// T16: one engine for every session in the app. Created here so a manager
    /// built without one still reports real statuses; `with_status` swaps in the
    /// boot-wired engine (with the signature file, the db and the 500 ms tick).
    status: Arc<StatusEngine>,
    sessions: DashMap<String, Arc<PtySession>>,
    /// M1.5-6: `pty_session` rows that outlived the process that owned them.
    /// Never a live session — nothing here can be written to, signalled or
    /// resized; the only thing left to do with one is read it and dismiss it.
    crashed: DashMap<String, CrashedSession>,
    /// The adoption runs once per process, on whichever `session_list` comes
    /// first. A `OnceCell` rather than a flag so two concurrent calls do not
    /// race one another into a half-filled table.
    crashed_once: tokio::sync::OnceCell<()>,
}

/// A pty session that was alive when the app died (M1.5 BACKLOG 6).
///
/// A SIGKILLed host takes its ptys down with it (the children get SIGHUP), so
/// the `pty_session` row is never marked closed and, before this existed, the
/// session simply vanished from the list on the next launch — together with the
/// tab that showed it. Keeping the row lets the UI say *what happened* instead
/// of showing one fewer session than the user left behind.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CrashedSession {
    /// `pid`/`pgid` are `0` and the sequence numbers are `0`: the ring died with
    /// the process, so there is no scrollback to offer.
    pub info: PtySessionInfo,
    /// Why the row is here. `hostCrash` is the only cause today.
    pub reason: String,
    /// The last status the status engine wrote before the host went away.
    pub last_status: Option<String>,
    /// When this launch noticed. The row itself has no exit timestamp — the
    /// process that would have written one is what died.
    pub noticed_at: i64,
}

impl CrashedSession {
    /// The status report `session_status` answers with.
    ///
    /// `code` and `signal` are both `None` on purpose: nobody reaped this child,
    /// so claiming SIGHUP would be an inference dressed up as a fact
    /// (DESIGN §6-4 — status is never reported without honest evidence).
    fn report(&self) -> SessionStatusReport {
        SessionStatusReport {
            status: SessionStatus::Exited {
                code: None,
                signal: None,
            },
            confidence: 1.0,
            evidence: vec![StatusEvidence::ChildExited { code: None }],
            since: self.noticed_at,
        }
    }
}

impl std::fmt::Debug for PtyManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PtyManager")
            .field("sessions", &self.sessions.len())
            .finish()
    }
}

impl PtyManager {
    pub fn new(supervisor: Arc<Supervisor>, config: Arc<ConfigStore>) -> PtyManager {
        let db = supervisor.db().clone();
        let status = Arc::new(StatusEngine::new()); // T16
        status.set_db(db.clone());
        status.set_stall_after_ms(config.get().status.stall_after_ms);
        PtyManager {
            supervisor,
            config,
            db,
            events: None,
            agents: None,
            accounts: Arc::new(Vec::new()),
            status,
            sessions: DashMap::new(),
            crashed: DashMap::new(),
            crashed_once: tokio::sync::OnceCell::new(),
        }
    }

    pub fn with_events(mut self, events: EventBus) -> PtyManager {
        self.status.set_events(events.clone()); // T16
        self.events = Some(events);
        self
    }

    /// T16: use the app-wide status engine instead of the private one.
    ///
    /// Call it **before** [`PtyManager::with_events`] — the bus is pushed into
    /// whichever engine is current, and boot builds the engine first anyway
    /// because it also owns the ticker.
    pub fn with_status(mut self, status: Arc<StatusEngine>) -> PtyManager {
        status.set_db(self.db.clone());
        status.set_stall_after_ms(self.config.get().status.stall_after_ms);
        if let Some(events) = &self.events {
            status.set_events(events.clone());
        }
        self.status = status;
        self
    }

    /// The engine every session in this manager reports through.
    pub fn status_engine(&self) -> &Arc<StatusEngine> {
        &self.status
    }

    pub fn with_agents(mut self, agents: Arc<AgentRegistry>) -> PtyManager {
        self.agents = Some(agents);
        self
    }

    pub fn with_accounts(mut self, accounts: Arc<Vec<AccountSlot>>) -> PtyManager {
        self.accounts = accounts;
        self
    }

    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    pub fn get(&self, id: &str) -> Result<Arc<PtySession>> {
        self.sessions
            .get(id)
            .map(|entry| entry.value().clone())
            .ok_or_else(|| GildeError::not_found(format!("session {id}")))
    }

    /// Every session this app still shows, oldest first — `session_list`.
    ///
    /// M1.5-6: that includes the rows adopted by
    /// [`PtyManager::adopt_crashed_sessions`] — sessions whose pty died with a
    /// host that was killed rather than closed. They carry `pid == 0`, which is
    /// the honest value for "there is no process", and they report
    /// [`SessionStatus::Exited`] through [`PtyManager::status`], so the session
    /// list, the graph seed and the tab bar all agree that the row is over
    /// instead of quietly losing it.
    pub fn list(&self) -> Vec<PtySessionInfo> {
        let mut out: Vec<PtySessionInfo> =
            self.sessions.iter().map(|e| e.value().info()).collect();
        for entry in self.crashed.iter() {
            if self.sessions.contains_key(entry.key()) {
                continue;
            }
            out.push(entry.value().info.clone());
        }
        out.sort_by_key(|info| info.opened_at);
        out
    }

    /// The spawn ids of every session we still own — what a shutdown hook needs
    /// in order to take the children down with us (T11 / T29).
    pub fn live_spawn_ids(&self) -> Vec<String> {
        self.sessions
            .iter()
            .filter(|e| !e.value().is_detached())
            .map(|e| e.value().spawn_id().to_string())
            .collect()
    }

    // -- sessions that outlived a crashed host (M1.5-6) -----------------------

    /// Adopt every `pty_session` row that was left open by a host that died.
    ///
    /// A row qualifies when it is still open (`closed_at IS NULL`), this manager
    /// does not own a live session with that id, and its registry row has no
    /// live pid. The last clause is what keeps a *detached* session — the one
    /// case where a process is meant to outlive the app — out of the list.
    ///
    /// Idempotent and cheap after the first call; `session_list` awaits it so
    /// the rows are in place before the first answer, and everything reading
    /// [`PtyManager::list`] afterwards (the graph projector included) sees them
    /// without a second code path.
    pub async fn adopt_crashed_sessions(&self) -> usize {
        let db = self.db.clone();
        self.crashed_once
            .get_or_init(|| async move {
                let rows = match db.read(dao::pty_session::list_open).await {
                    Ok(rows) => rows,
                    Err(err) => {
                        tracing::warn!(
                            target: "pty",
                            error = %err,
                            "could not read the open pty_session rows"
                        );
                        return;
                    }
                };
                let at = now_ms();
                for row in rows {
                    if self.sessions.contains_key(&row.id) {
                        continue;
                    }
                    if self.row_still_has_a_process(&row).await {
                        continue;
                    }
                    tracing::info!(
                        target: "pty",
                        session = %row.id,
                        agent = %row.agent_kind,
                        "pty_session row survived a host crash; listing it as exited"
                    );
                    let id = row.id.clone();
                    self.crashed.insert(id, crashed_of_row(row, at));
                }
            })
            .await;
        self.crashed.len()
    }

    /// True when the registry still points at a process that answers `kill(0)`.
    async fn row_still_has_a_process(&self, row: &PtySessionRow) -> bool {
        let Some(spawn_id) = row.spawn_id.clone() else {
            return false;
        };
        let proc = match self
            .db
            .read(move |conn| dao::proc::get(conn, &spawn_id))
            .await
        {
            Ok(Some(proc)) => proc,
            _ => return false,
        };
        if !proc.state.is_live() {
            return false;
        }
        match proc.pid {
            Some(pid) if pid > 0 => crate::supervise::spawn::is_alive(pid as i32),
            _ => false,
        }
    }

    /// The adopted rows, oldest first. Empty until `session_list` has run once.
    pub fn crashed_list(&self) -> Vec<CrashedSession> {
        let mut out: Vec<CrashedSession> = self
            .crashed
            .iter()
            .map(|entry| entry.value().clone())
            .collect();
        out.sort_by_key(|row| row.info.opened_at);
        out
    }

    /// Whether `id` is one of the adopted rows rather than a live session.
    pub fn is_crashed(&self, id: &str) -> bool {
        !self.sessions.contains_key(id) && self.crashed.contains_key(id)
    }

    /// Dismiss one adopted row: close it in the registry so the next launch does
    /// not offer it again, and drop it from the list.
    ///
    /// Returns `false` when `id` is not an adopted row, which is what lets
    /// `session_close` treat "dismiss the dead row" and "end a live session" as
    /// one command without guessing.
    async fn dismiss_crashed(&self, id: &str) -> bool {
        if self.crashed.remove(id).is_none() {
            return false;
        }
        let sid = id.to_string();
        let at = now_ms();
        if let Err(err) = self
            .db
            .write(move |conn| {
                dao::pty_session::mark_closed(conn, &sid, at, None, None).map(|_| ())
            })
            .await
        {
            tracing::warn!(target: "pty", error = %err, "could not close the crashed pty_session row");
        }
        if let Some(bus) = &self.events {
            bus.push(GraphOp::RemoveNode {
                id: node_id(NodeKind::Session, id),
            });
        }
        true
    }

    // -- opening -------------------------------------------------------------

    /// Resolve a UI request into a spec and open it.
    pub async fn open_request(
        &self,
        req: OpenSessionReq,
        sink: Channel<PtyChunk>,
    ) -> Result<PtySessionInfo> {
        let agents = self
            .agents
            .as_ref()
            .ok_or_else(|| GildeError::internal("no agent registry is wired to the pty manager"))?;

        // Where. A session always runs in a worktree the database knows about;
        // its recorded path is the one the alias layer and `git` agree on.
        let worktree_id = req.worktree_id.clone();
        let worktree = self
            .db
            .read(move |conn| dao::worktree::get(conn, &worktree_id))
            .await?
            .ok_or_else(|| GildeError::not_found(format!("worktree {}", req.worktree_id)))?;
        if worktree.removed_at.is_some() {
            return Err(GildeError::refused("worktreeRemoved"));
        }
        let cwd = PathBuf::from(&worktree.path);
        if !cwd.is_dir() {
            return Err(GildeError::not_found(worktree.path.clone()));
        }

        // What. The launch template is the only source of an agent's argv; a
        // caller-supplied `args` replaces its tail but never its binary.
        let templates = agents.templates().await?;
        let template = templates
            .iter()
            .find(|t| t.agent == req.agent)
            .ok_or_else(|| GildeError::not_found(format!("agent {}", req.agent.key())))?;
        if template.binary.is_empty() {
            return Err(GildeError::not_found(format!(
                "{} is not installed on this machine",
                req.agent.key()
            )));
        }

        let mut argv = match &req.args {
            Some(args) if !args.is_empty() => {
                let mut argv = vec![template.binary.clone()];
                argv.extend(args.iter().cloned());
                argv
            }
            _ => template.interactive_argv(),
        };
        if let Some(prompt) = req.prompt.as_deref().filter(|p| !p.trim().is_empty()) {
            match template.argv_with_prompt(prompt) {
                Some(with_prompt) if req.args.is_none() => argv = with_prompt,
                // `TypeAfterReady` (and any override that took over argv) has to
                // wait for the ready gate; the UI sends it with
                // `session_send_prompt` once the status engine says idle.
                _ => tracing::info!(
                    target: "pty",
                    agent = template.agent.key(),
                    "this launch template cannot carry a prompt on argv; the ui must type it"
                ),
            }
        }

        let account = req.account_id.as_ref().and_then(|id| {
            self.accounts
                .iter()
                .find(|slot| &slot.id == id)
                .cloned()
        });
        if let (Some(id), None) = (req.account_id.as_ref(), account.as_ref()) {
            tracing::warn!(target: "pty", account = %id, "unknown account slot; using the ambient login");
        }

        let settings = self.config.get();
        let path_report = agents.path_report(false);
        let title = req.title.clone().unwrap_or_else(|| {
            format!("{} · {}", req.agent.key(), worktree.slug)
        });

        let spec = OpenSpec {
            session_id: crate::model::ids::SessionId::new().into_string(),
            project_id: if req.project_id.is_empty() {
                worktree.project_id.clone()
            } else {
                req.project_id.clone()
            },
            worktree_id: req.worktree_id.clone(),
            agent: req.agent,
            account,
            title,
            cwd,
            argv,
            env_base: EnvBase::from_process_env(path_report.joined()),
            cols: req.cols,
            rows: req.rows,
            ring_bytes: clamp_ring_bytes(settings.terminal.scrollback_bytes),
            idle_after_ms: settings.status.idle_after_ms,
            raw_output: false,
        };
        self.open(spec, sink).await
    }

    /// Open a pty, start the child in it, and register the session.
    pub async fn open(&self, spec: OpenSpec, sink: Channel<PtyChunk>) -> Result<PtySessionInfo> {
        let binary = spec
            .argv
            .first()
            .ok_or_else(|| GildeError::spawn("a session needs at least argv[0]"))?
            .clone();
        if !Path::new(&binary).is_absolute() {
            return Err(GildeError::spawn(format!(
                "argv[0] must be an absolute path, got {binary}"
            )));
        }
        let (cols, rows) = sanitize_size(spec.cols, spec.rows);

        // 1. The durable row, before anything can fork.
        let mut plan = SpawnPlan::new(ProcKind::Pty, spec.argv.clone(), spec.cwd.clone())
            .vendor(spec.agent.key())
            .session(spec.session_id.clone());
        if !spec.worktree_id.is_empty() {
            plan = plan.worktree(spec.worktree_id.clone());
        }
        if let Some(dir) = spec.account.as_ref().and_then(|a| a.config_dir.as_ref()) {
            plan = plan.config_dir(dir.clone());
        }
        let guard = Arc::new(self.supervisor.plan_spawn(plan).await?);

        // 2. The pty. Never 0×0: `codex` draws nothing into one (PINNED §9-4).
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| GildeError::pty(format!("openpty failed: {e}")))?;

        if let Some(fd) = pair.master.as_raw_fd() {
            // SAFETY: `pair.master` owns the descriptor for the whole call.
            let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
            if let Err(err) = set_output_processing(borrowed, !spec.raw_output) {
                if spec.raw_output {
                    return Err(err);
                }
                // A tty we could not configure still has the kernel default,
                // which is the same thing we were asking for.
                tracing::warn!(target: "pty", error = %err, "could not set the tty output flags");
            }
        }

        // 3. The command. Explicit environment, never an inherited one (§6-1).
        let mut cmd = CommandBuilder::new(&binary);
        for arg in &spec.argv[1..] {
            cmd.arg(arg);
        }
        cmd.cwd(&spec.cwd);
        cmd.env_clear();
        let env: BTreeMap<String, String> = self.supervisor.compose_env(
            spec.env_base.clone(),
            spec.account.as_ref(),
            guard.env_marker(),
        );
        for (key, value) in &env {
            cmd.env(key, value);
        }

        // 4. Fork, then drop the slave **immediately**.
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| GildeError::spawn(format!("could not start {binary}: {e}")))?;
        drop(pair.slave);

        let pid = child
            .process_id()
            .ok_or_else(|| GildeError::spawn("the pty child reported no pid"))? as i32;
        // `portable-pty` calls `setsid()` in the child, so the child leads its
        // own group. We record what the kernel says rather than assuming it.
        let pgid = crate::supervise::spawn::pgid_of(pid).unwrap_or(pid);
        if pgid != pid {
            tracing::warn!(
                target: "pty",
                pid,
                pgid,
                "the pty child is not its own group leader; killing it will be group-unsafe"
            );
        }
        guard.mark_running(pid, pgid)?;

        // 5. The threads.
        let session = Arc::new(PtySession::start(
            SessionParts {
                id: spec.session_id.clone(),
                spawn_id: guard.spawn_id().to_string(),
                project_id: spec.project_id.clone(),
                worktree_id: spec.worktree_id.clone(),
                agent: spec.agent,
                account_id: spec.account.as_ref().map(|a| a.id.clone()),
                title: spec.title.clone(),
                cwd: spec.cwd.clone(),
                pid,
                pgid,
                cols,
                rows,
                ring_bytes: spec.ring_bytes,
                idle_after_ms: spec.idle_after_ms,
                status: self.status.clone(), // T16
                guard: guard.clone(),
                events: self.events.clone(),
                db: Some(self.db.clone()),
            },
            pair.master,
            child,
            sink,
        )?);

        // T16: the engine can now ask the kernel which group owns the tty. It
        // holds a `Weak`, so this does not keep the master alive past `retire`.
        let foreground: Arc<dyn ForegroundSource> = session.clone();
        self.status
            .attach_foreground(&spec.session_id, &foreground);

        // 6. The durable session row and the graph node. Neither is allowed to
        //    fail the open: the terminal already exists and is already drawing.
        self.record_row(&session, &spec).await;
        self.upsert_node(&session, &spec);

        tracing::info!(
            target: "pty",
            session = %spec.session_id,
            spawn = %guard.spawn_id(),
            agent = spec.agent.key(),
            pid,
            pgid,
            cols,
            rows,
            cwd = %spec.cwd.display(),
            "pty session open"
        );

        let info = session.info();
        self.sessions.insert(spec.session_id.clone(), session);
        Ok(info)
    }

    async fn record_row(&self, session: &Arc<PtySession>, spec: &OpenSpec) {
        let row = PtySessionRow {
            id: spec.session_id.clone(),
            spawn_id: Some(session.spawn_id().to_string()),
            project_id: none_if_empty(&spec.project_id),
            worktree_id: none_if_empty(&spec.worktree_id),
            agent_kind: spec.agent.key().to_string(),
            account_id: spec.account.as_ref().map(|a| a.id.clone()),
            title: spec.title.clone(),
            cwd: spec.cwd.to_string_lossy().into_owned(),
            cols: session.size().0 as i64,
            rows: session.size().1 as i64,
            // Masked, like every other argv we persist (DESIGN §8-7).
            argv: serde_json::to_string(&mask_argv_vec(&spec.argv)).unwrap_or_else(|_| "[]".into()),
            opened_at: now_ms(),
            closed_at: None,
            exit_code: None,
            exit_signal: None,
            last_io_at: None,
            last_status: Some("starting".to_string()),
            detached: false,
        };
        if let Err(err) = self.db.write(move |conn| dao::pty_session::insert(conn, &row)).await {
            tracing::warn!(target: "pty", error = %err, "could not record the pty_session row");
        }
    }

    fn upsert_node(&self, session: &Arc<PtySession>, spec: &OpenSpec) {
        let Some(bus) = &self.events else { return };
        let id = node_id(NodeKind::Session, &spec.session_id);
        let mut node = GraphNode::new(&id, NodeKind::Session, &spec.title, NodeStatus::Idle, now_ms());
        node.sublabel = Some(spec.agent.key().to_string());
        node.meta.pid = Some(session.pid());
        node.meta.pgid = Some(session.pgid());
        node.meta.cwd = Some(spec.cwd.to_string_lossy().into_owned());
        node.meta.vendor = Some(spec.agent.key().to_string());
        node.meta.account_id = spec.account.as_ref().map(|a| a.id.clone());
        if !spec.worktree_id.is_empty() {
            let parent = node_id(NodeKind::Worktree, &spec.worktree_id);
            node.parent_id = Some(parent.clone());
            node.meta.worktree_id = Some(spec.worktree_id.clone());
            bus.push(GraphOp::UpsertEdge {
                edge: GraphEdge::new(EdgeKind::Contains, parent, id.clone()),
            });
        }
        bus.push(GraphOp::UpsertNode { node });
    }

    // -- the io surface ------------------------------------------------------

    /// Replace the delivery channel of a remounted view (DESIGN §6-3 step 1).
    pub fn attach(&self, id: &str, sink: Channel<PtyChunk>) -> Result<()> {
        self.get(id)?.attach(sink);
        Ok(())
    }

    /// 16-byte header + raw ring bytes.
    pub fn snapshot(&self, id: &str) -> Result<Vec<u8>> {
        Ok(self.get(id)?.snapshot())
    }

    pub fn write(&self, id: &str, bytes: &[u8]) -> Result<()> {
        self.get(id)?.write_bytes(bytes)
    }

    pub fn resize(&self, id: &str, cols: u16, rows: u16) -> Result<()> {
        self.get(id)?.resize(cols, rows).map(|_| ())
    }

    pub fn signal(&self, id: &str, sig: PtySignal) -> Result<()> {
        self.get(id)?.signal_foreground(sig).map(|_| ())
    }

    pub fn send_prompt(&self, id: &str, text: &str) -> Result<SendPromptOutcome> {
        Ok(self.get(id)?.send_prompt(text))
    }

    pub fn status(&self, id: &str) -> Result<SessionStatusReport> {
        // M1.5-6: an adopted row has no engine entry — answering `exited` here
        // is what makes the graph seed draw it as over rather than as idle.
        if !self.sessions.contains_key(id) {
            if let Some(entry) = self.crashed.get(id) {
                return Ok(entry.value().report());
            }
        }
        Ok(self.get(id)?.status_report())
    }

    // -- closing -------------------------------------------------------------

    /// End a session the way `mode` asks (DESIGN §6-5).
    ///
    /// Always returns a [`KillReport`]: what was signalled, what was spared and
    /// what is still alive. Never a bare "ok", and never `Child::kill()` —
    /// killing the direct child would leave its own children behind.
    pub async fn close(&self, id: &str, mode: CloseMode) -> Result<KillReport> {
        // M1.5-6: closing an adopted row is a dismissal, not a kill. There is
        // no process to signal, so the report says exactly that instead of
        // pretending something was terminated.
        if self.is_crashed(id) && self.dismiss_crashed(id).await {
            return Ok(KillReport {
                spawn_id: None,
                requested: Vec::new(),
                excluded: Vec::new(),
                survivors: Vec::new(),
                refused_reason: Some("alreadyExited".to_string()),
                dry_run: false,
                escalated: Vec::new(),
                fence: "hostCrash".to_string(),
                at: now_ms(),
                names: Vec::new(),
            });
        }

        let session = self.get(id)?;
        let spawn_id = session.spawn_id().to_string();
        let pid = session.pid();

        match mode {
            CloseMode::Detach => {
                session.detach();
                let sid = spawn_id.clone();
                if let Err(err) = self
                    .db
                    .write_durable(move |conn| {
                        dao::proc::set_keep_alive(conn, &sid, true).map(|_| ())
                    })
                    .await
                {
                    tracing::warn!(target: "pty", error = %err, "could not set keep_alive");
                }
                let sid = id.to_string();
                if let Err(err) = self
                    .db
                    .write(move |conn| dao::pty_session::set_detached(conn, &sid, true).map(|_| ()))
                    .await
                {
                    tracing::warn!(target: "pty", error = %err, "could not record the detach");
                }
                tracing::warn!(
                    target: "pty",
                    session = %id,
                    pid,
                    "session detached; this process will outlive the app"
                );
                Ok(KillReport {
                    spawn_id: Some(spawn_id),
                    requested: Vec::new(),
                    excluded: Vec::new(),
                    survivors: vec![pid],
                    refused_reason: Some("detached".to_string()),
                    dry_run: false,
                    escalated: Vec::new(),
                    fence: "detached".to_string(),
                    at: now_ms(),
                    names: Vec::new(),
                })
            }

            CloseMode::Gentle => {
                // What closing the terminal window does. Opt-in, because the
                // foreground group may be a pager rather than the agent.
                let report = match session.signal_foreground(PtySignal::Hup) {
                    Ok(pgid) => {
                        let exited = session.wait_for_exit(STANDARD_CLOSE_GRACE);
                        KillReport {
                            spawn_id: Some(spawn_id.clone()),
                            requested: vec![pgid],
                            excluded: Vec::new(),
                            survivors: if exited { Vec::new() } else { vec![pid] },
                            refused_reason: None,
                            dry_run: false,
                            escalated: Vec::new(),
                            fence: "foregroundGroup".to_string(),
                            at: now_ms(),
                            names: Vec::new(),
                        }
                    }
                    Err(err) => KillReport::refused(
                        Some(spawn_id.clone()),
                        "foregroundGroup",
                        &format!("{}", err.code()),
                    ),
                };
                self.retire(&session, &report);
                Ok(report)
            }

            CloseMode::Standard => {
                // ^C then ^D: the same two keys a user would press. Only if the
                // TUI ignores both does this become a kill.
                if let Err(err) = session.interrupt_and_eof() {
                    tracing::debug!(target: "pty", error = %err, "could not type ^C/^D; escalating");
                }
                if session.wait_for_exit(STANDARD_CLOSE_GRACE) {
                    let report = KillReport {
                        spawn_id: Some(spawn_id),
                        requested: Vec::new(),
                        excluded: Vec::new(),
                        survivors: Vec::new(),
                        refused_reason: None,
                        dry_run: false,
                        escalated: Vec::new(),
                        fence: "exitedOnInterrupt".to_string(),
                        at: now_ms(),
                        names: Vec::new(),
                    };
                    self.retire(&session, &report);
                    return Ok(report);
                }
                let grace = Duration::from_millis(
                    self.config.get().supervision.kill_grace_ms.max(1_000),
                );
                let report = kill_tree(
                    &self.supervisor,
                    &spawn_id,
                    KillOpts {
                        grace,
                        ..KillOpts::default()
                    },
                )
                .await;
                self.retire(&session, &report);
                Ok(report)
            }
        }
    }

    /// Drop a closed session: stop delivering, forget it, and let `Drop` close
    /// the master (which is safe now that the child is gone).
    fn retire(&self, session: &Arc<PtySession>, report: &KillReport) {
        session.stop_delivery();
        if !session.has_exited() && report.survivors.is_empty() {
            session.mark_killed();
        }
        self.sessions.remove(session.id());
        if let Some(bus) = &self.events {
            bus.push(GraphOp::PatchNode {
                id: node_id(NodeKind::Session, session.id()),
                status: Some(if report.survivors.is_empty() {
                    NodeStatus::Exited
                } else {
                    NodeStatus::Orphan
                }),
                confidence: Some(1.0),
                evidence: None,
                badges: None,
                metrics: None,
            });
        }
    }
}

/// Project a durable `pty_session` row onto the shape the UI already reads.
///
/// `pid`, `pgid` and both sequence numbers are `0`: the process and its ring
/// are gone, and a fabricated pid would be a lie the Inspector would print.
fn crashed_of_row(row: PtySessionRow, at: i64) -> CrashedSession {
    let last_status = row.last_status.clone();
    let info = PtySessionInfo {
        id: row.id,
        spawn_id: row.spawn_id.unwrap_or_default(),
        project_id: row.project_id.unwrap_or_default(),
        worktree_id: row.worktree_id.unwrap_or_default(),
        agent: AgentKind::from_key(&row.agent_kind).unwrap_or(AgentKind::Shell),
        account_id: row.account_id,
        title: row.title,
        cwd: row.cwd,
        cols: row.cols.clamp(0, u16::MAX as i64) as u16,
        rows: row.rows.clamp(0, u16::MAX as i64) as u16,
        pid: 0,
        pgid: 0,
        opened_at: row.opened_at,
        // The `detached` flag described a process that is no longer there.
        detached: false,
        first_retained_seq: 0,
        next_seq: 0,
    };
    CrashedSession {
        info,
        reason: "hostCrash".to_string(),
        last_status,
        noticed_at: at,
    }
}

fn none_if_empty(value: &str) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// Turn the tty's output post-processing on or off.
///
/// `OPOST | ONLCR` is what every terminal emulator gives a child: a program that
/// prints `"a\nb"` is meant to render on two lines starting at column zero, and
/// without `ONLCR` it staircases. It is therefore **on by default** and set
/// explicitly rather than inherited, so that the one transformation between an
/// agent's `write()` and our ring is a documented choice rather than whatever
/// `openpty(3)` happened to default to.
fn set_output_processing(fd: BorrowedFd<'_>, enabled: bool) -> Result<()> {
    use nix::sys::termios::{tcgetattr, tcsetattr, OutputFlags, SetArg};

    let mut termios =
        tcgetattr(fd).map_err(|e| GildeError::pty(format!("tcgetattr on the pty master: {e}")))?;
    let flags = OutputFlags::OPOST | OutputFlags::ONLCR;
    if enabled {
        termios.output_flags.insert(flags);
    } else {
        termios.output_flags.remove(flags);
    }
    tcsetattr(fd, SetArg::TCSANOW, &termios)
        .map_err(|e| GildeError::pty(format!("tcsetattr on the pty master: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spec_defaults_to_a_drawable_size_and_a_sane_tty() {
        let spec = OpenSpec::new(AgentKind::Shell, "/tmp", vec!["/bin/sh".to_string()]);
        assert_eq!((spec.cols, spec.rows), (80, 24));
        assert!(!spec.raw_output, "a terminal post-processes output by default");
        assert_eq!(spec.ring_bytes, DEFAULT_RING_BYTES);
    }

    #[test]
    fn empty_ids_become_null_columns_not_dangling_foreign_keys() {
        assert_eq!(none_if_empty(""), None);
        assert_eq!(none_if_empty("w1"), Some("w1".to_string()));
    }

    // -- M1.5-6: rows left behind by a host that was killed -------------------

    use crate::store::models::{NewPlannedProc, ProcState};

    struct CrashFixture {
        manager: PtyManager,
        db: Db,
        app_instance: String,
        _dir: tempfile::TempDir,
    }

    async fn crash_fixture() -> CrashFixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::open_in_memory().expect("db");
        let config =
            Arc::new(ConfigStore::open_at(&dir.path().join("config.json")).expect("config"));
        let supervisor = Arc::new(
            Supervisor::open_instance(&db, config.clone(), "0.0.0-test")
                .await
                .expect("open_instance"),
        );
        let app_instance = supervisor.app_instance_id().to_string();
        CrashFixture {
            manager: PtyManager::new(supervisor, config),
            db,
            app_instance,
            _dir: dir,
        }
    }

    /// The row a host that never got to run its shutdown sweep leaves behind:
    /// `closed_at IS NULL`, and nothing alive on the other end of it.
    fn open_row(id: &str, spawn_id: Option<&str>) -> PtySessionRow {
        PtySessionRow {
            id: id.to_string(),
            spawn_id: spawn_id.map(|s| s.to_string()),
            // Both are foreign keys; the crash path does not care which project
            // a row belonged to, so the fixture leaves them null.
            project_id: None,
            worktree_id: None,
            agent_kind: "claude".to_string(),
            account_id: None,
            title: "auth-refactor".to_string(),
            cwd: "/tmp/wt".to_string(),
            cols: 120,
            rows: 40,
            argv: "[]".to_string(),
            opened_at: 1_000,
            closed_at: None,
            exit_code: None,
            exit_signal: None,
            last_io_at: Some(2_000),
            last_status: Some("working".to_string()),
            detached: false,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_session_left_open_by_a_crashed_host_is_listed_as_exited() {
        let fx = crash_fixture().await;
        let row = open_row("sess-crash", None);
        fx.db
            .write(move |conn| dao::pty_session::insert(conn, &row))
            .await
            .expect("insert");

        assert!(
            fx.manager.list().is_empty(),
            "nothing is listed before the adoption runs"
        );

        assert_eq!(fx.manager.adopt_crashed_sessions().await, 1);

        let listed = fx.manager.list();
        assert_eq!(listed.len(), 1, "the row is back in the session list");
        assert_eq!(listed[0].id, "sess-crash");
        assert_eq!(listed[0].pid, 0, "there is no process, so no pid is claimed");
        assert_eq!(listed[0].title, "auth-refactor");
        assert!(!listed[0].detached, "a dead process is not a detached one");

        let report = fx.manager.status("sess-crash").expect("status");
        assert!(
            matches!(
                report.status,
                SessionStatus::Exited {
                    code: None,
                    signal: None
                }
            ),
            "the row reports exited without inventing an exit code: {:?}",
            report.status
        );
        assert_eq!(fx.manager.crashed_list()[0].reason, "hostCrash");
        assert!(fx.manager.is_crashed("sess-crash"));

        // Idempotent: a second `session_list` does not duplicate the row.
        assert_eq!(fx.manager.adopt_crashed_sessions().await, 1);
        assert_eq!(fx.manager.list().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_detached_session_whose_process_is_still_alive_is_not_a_crash_row() {
        let fx = crash_fixture().await;
        let row = open_row("sess-detached", Some("spawn-live"));
        fx.db
            .write(move |conn| dao::pty_session::insert(conn, &row))
            .await
            .expect("insert");

        // A registry row pointing at a pid that answers `kill(0)` — our own.
        let planned = NewPlannedProc {
            spawn_id: "spawn-live".to_string(),
            app_instance: fx.app_instance.clone(),
            generation: 1,
            kind: ProcKind::Pty,
            vendor: Some("claude".to_string()),
            session_id: Some("sess-detached".to_string()),
            run_id: None,
            argv: vec!["/bin/sh".to_string()],
            cwd: "/tmp/wt".to_string(),
            config_dir: None,
            worktree_id: None,
            keep_alive: true,
            planned_at: 1_000,
            note: None,
        };
        let me = std::process::id() as i64;
        fx.db
            .write(move |conn| {
                dao::proc::insert_planned(conn, &planned)?;
                dao::proc::mark_running(conn, "spawn-live", me, Some(me), None, 1_001).map(|_| ())
            })
            .await
            .expect("registry row");

        assert_eq!(
            fx.manager.adopt_crashed_sessions().await,
            0,
            "a process that outlived the app is detached, not crashed"
        );
        assert!(fx.manager.list().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dismissing_a_crash_row_closes_it_for_good() {
        let fx = crash_fixture().await;
        let row = open_row("sess-dismiss", Some("spawn-dead"));
        fx.db
            .write(move |conn| dao::pty_session::insert(conn, &row))
            .await
            .expect("insert");
        // The registry row is finished, so nothing is alive behind it.
        let planned = NewPlannedProc {
            spawn_id: "spawn-dead".to_string(),
            app_instance: fx.app_instance.clone(),
            generation: 1,
            kind: ProcKind::Pty,
            vendor: Some("claude".to_string()),
            session_id: Some("sess-dismiss".to_string()),
            run_id: None,
            argv: vec!["/bin/sh".to_string()],
            cwd: "/tmp/wt".to_string(),
            config_dir: None,
            worktree_id: None,
            keep_alive: false,
            planned_at: 1_000,
            note: None,
        };
        fx.db
            .write(move |conn| {
                dao::proc::insert_planned(conn, &planned)?;
                dao::proc::mark_state(conn, "spawn-dead", ProcState::Lost, Some("boot sweep"))
                    .map(|_| ())
            })
            .await
            .expect("registry row");

        assert_eq!(fx.manager.adopt_crashed_sessions().await, 1);

        let report = fx
            .manager
            .close("sess-dismiss", CloseMode::Standard)
            .await
            .expect("dismiss");
        assert_eq!(report.fence, "hostCrash");
        assert!(report.requested.is_empty(), "nothing was signalled");
        assert!(report.survivors.is_empty());
        assert_eq!(report.refused_reason.as_deref(), Some("alreadyExited"));

        assert!(fx.manager.list().is_empty(), "the row is gone from the list");
        let still_open = fx
            .db
            .read(dao::pty_session::list_open)
            .await
            .expect("list_open");
        assert!(
            still_open.is_empty(),
            "and it is closed in the registry, so the next launch will not offer it again"
        );
    }
}
