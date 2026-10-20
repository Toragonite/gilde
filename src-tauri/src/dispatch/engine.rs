//! The dispatch engine: one prompt, N agents, N worktrees (M1-PLAN T22).
//!
//! The engine is the only place in the app that starts headless work, and four
//! rules shape every function below.
//!
//! **1. The files are the record.** Before a run is handed to the sidecar its
//! `prompt.md` is on disk; every `RunEvent` is appended to `stream.jsonl` as it
//! arrives; the terminal `done` writes `result.json` verbatim (DESIGN §10-2).
//! The `dispatch_run` row and the live `gilde://dispatch/event` stream are
//! conveniences layered on top — losing either costs a UI refresh, not a record.
//!
//! **2. Capabilities are read, never assumed.** Whether a vendor streams comes
//! from `AdapterCapabilities.stream` on the live handshake
//! ([`SidecarManager::can_stream`]), so codex takes the `mesh.run` path today
//! and would take the streaming one the day its adapter flips the flag. No
//! vendor name decides how a run is driven. The one place a vendor is named is
//! [`permission_mode_for`], and it is not a capability guess: it is a
//! *translation table* between four CLIs that genuinely do not share a
//! permission vocabulary (see rule 5).
//!
//! **3. Cancellation is the host's, and works with the sidecar wedged.** A
//! cancel kills every `proc_registry` row carrying the run id and, *in
//! parallel*, sends the courtesy `mesh.cancel`. The kills do not wait on the
//! RPC, which is what makes `tests/dispatch.rs::cancelling_works_while_the_
//! sidecar_is_stopped` pass against a `SIGSTOP`ped sidecar (DESIGN §5-9).
//!
//! **4. "The app broke" is not "the agent failed".** A request in flight when a
//! sidecar generation dies settles as `interrupted`, never `error` — the user
//! is told to resume or re-run rather than left to guess whose fault it was
//! (DESIGN §5-10).
//!
//! **5. A fan-out speaks each vendor's own dialect.** `config.dispatch.
//! permissionMode` is written in claude's vocabulary (`acceptEdits`, DESIGN
//! §11-2 / §13), but `RunTask.permissionMode` is a *passthrough*: agent-mesh
//! hands it to whatever flag each adapter owns — `--permission-mode` for
//! claude, `--sandbox` for codex and cursor, `--mode` for gemini — and those
//! flags do not accept each other's words. Sending one prompt to N vendors is
//! therefore the host's problem to translate, and [`permission_mode_for`] is
//! where it happens. The adapters are never forked.
//!
//! Routing lives behind one door: [`DispatchEngine::route`] is the only caller
//! of `fleet.route`, and the only callers of *it* are the `dispatch_route`
//! command and the `vendor == "auto"` branch of [`DispatchEngine::create`]
//! (DESIGN §9-7, T22 acceptance 10).

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::sync::{broadcast, Notify, Semaphore};

use crate::accounts::AccountSlot;
use crate::agents::AgentKind;
use crate::config::ConfigStore;
use crate::error::{GildeError, Result};
use crate::events::bus::EventBus;
use crate::events::{DispatchEvent, DispatchEventKind, DISPATCH_EVENT};
use crate::model::graph::{node_id, EdgeKind, GraphEdge, GraphNode, GraphOp, NodeKind, NodeStatus};
use crate::model::status::NodeMetrics;
use crate::paths::AppPaths;
use crate::pty::{OpenSessionReq, PtyChunk, PtyManager, PtySessionInfo};
use crate::sidecar::{
    method, Account, RunEvent, RunEventEnvelope, RunResult, RunTask, SidecarCallError,
    SidecarManager,
};
use crate::store::models::{DispatchRunRow, DispatchTaskRow};
use crate::store::{dao, now_ms, Db};
use crate::supervise::kill::{kill_tree, KillOpts, KillReport};
use crate::supervise::registry::Supervisor;
use crate::worktree::{CreateWorktreeReq, WorktreeManager};

use super::model::{
    cost_usd, fold_task_status, store_result_text, usage_json, worktree_task_name, DispatchRun,
    DispatchScope, DispatchSpec, DispatchTarget, DispatchTask, DispatchTaskDetail, RouteDecision,
    RunArtifacts, RunPhase, TaskStatus, WorktreeStrategy, TEXT_COALESCE_MS,
};

/// How long `mesh.stream` may take to answer `{accepted:true}`. The answer is
/// immediate by construction (the sidecar detaches the pump), so anything near
/// this bound is a wedged channel, not a slow agent.
const STREAM_ACCEPT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the courtesy `mesh.cancel` may take. Short on purpose: the real
/// cancellation is the `kill_tree` beside it, and a stopped sidecar must not
/// delay it (DESIGN §5-9).
const CANCEL_RPC_TIMEOUT: Duration = Duration::from_secs(2);

/// Host-side grace on top of the task's own wall clock. The sidecar enforces
/// `timeoutMs` and always emits a terminal event; this is the backstop for the
/// case where it does not, so a run cannot sit in `running` forever.
const RUN_WALL_CLOCK_GRACE: Duration = Duration::from_secs(120);

/// Chars of prompt used to title a task when the caller supplies none.
const TITLE_CAP: usize = 60;

// ---------------------------------------------------------------------------
// engine
// ---------------------------------------------------------------------------

/// A run this process is currently driving.
///
/// Only the cancellation pair: the generation a run belongs to is watched by
/// the driver itself (each holds its own `subscribe_deaths()` receiver), so it
/// does not need to be findable from here.
#[derive(Clone)]
struct LiveRun {
    cancelled: Arc<AtomicBool>,
    /// `notify_one`, so a cancel that lands between two `select!`s is still
    /// observed by the next one.
    cancel: Arc<Notify>,
}

/// Everything one driven run needs. Owned, because the driver outlives the call
/// that created it.
#[derive(Clone)]
struct RunCtx {
    task_id: String,
    run_id: String,
    vendor: String,
    model: Option<String>,
    account: Option<Account>,
    worktree_id: Option<String>,
    cwd: PathBuf,
    prompt: String,
    permission_mode: String,
    timeout_ms: u64,
    artifacts: RunArtifacts,
    /// `Some` when this run resumes another one.
    resume_of: Option<(String, String)>,
    started_at: i64,
}

/// Owns the headless lane. Held in managed state as `Arc<DispatchEngine>`.
pub struct DispatchEngine {
    db: Db,
    paths: Arc<AppPaths>,
    config: Arc<ConfigStore>,
    sidecar: Arc<SidecarManager>,
    supervisor: Arc<Supervisor>,
    worktrees: Arc<WorktreeManager>,
    accounts: Arc<Vec<AccountSlot>>,
    events: Mutex<Option<EventBus>>,
    pty: Mutex<Option<Arc<PtyManager>>>,
    live: DashMap<String, LiveRun>,
    /// `config.dispatch.maxConcurrentRuns`. A fan-out to eight agents does not
    /// put eight CLIs on the machine at once.
    limiter: Arc<Semaphore>,
}

impl std::fmt::Debug for DispatchEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DispatchEngine")
            .field("live_runs", &self.live.len())
            .field("permits", &self.limiter.available_permits())
            .finish()
    }
}

impl DispatchEngine {
    pub fn new(
        db: Db,
        paths: Arc<AppPaths>,
        config: Arc<ConfigStore>,
        sidecar: Arc<SidecarManager>,
        supervisor: Arc<Supervisor>,
        worktrees: Arc<WorktreeManager>,
        accounts: Arc<Vec<AccountSlot>>,
    ) -> Arc<DispatchEngine> {
        let permits = config.get().dispatch.max_concurrent_runs.max(1) as usize;
        Arc::new(DispatchEngine {
            db,
            paths,
            config,
            sidecar,
            supervisor,
            worktrees,
            accounts,
            events: Mutex::new(None),
            pty: Mutex::new(None),
            live: DashMap::new(),
            limiter: Arc::new(Semaphore::new(permits)),
        })
    }

    /// Attach the event bus. Separate from construction because the bus needs a
    /// window and the engine is built during boot.
    pub fn set_events(&self, events: EventBus) {
        *self.events.lock() = Some(events);
    }

    /// Attach the PTY manager, which backs `dispatch_open_terminal` — the
    /// bridge between the two lanes (DESIGN §9-7).
    pub fn set_pty(&self, pty: Arc<PtyManager>) {
        *self.pty.lock() = Some(pty);
    }

    fn bus(&self) -> Option<EventBus> {
        self.events.lock().clone()
    }

    pub fn live_run_count(&self) -> usize {
        self.live.len()
    }

    // -- create --------------------------------------------------------------

    /// Fan one prompt out to N targets (T22 acceptance 1–5).
    ///
    /// Returns as soon as the rows and the worktrees exist; the runs themselves
    /// are driven on their own tasks and report through `gilde://dispatch/event`.
    /// A target that cannot run — not installed, no worktree, no route — is
    /// refused **on its own row** and never fails the other targets
    /// (acceptance 4).
    pub async fn create(self: &Arc<Self>, spec: DispatchSpec) -> Result<DispatchTask> {
        if spec.prompt.trim().is_empty() {
            return Err(GildeError::refused("emptyPrompt"));
        }
        if spec.targets.is_empty() {
            return Err(GildeError::refused("noTargets"));
        }
        let task_slug = crate::worktree::naming::sanitize(&spec.task_slug);
        if task_slug.is_empty() {
            return Err(GildeError::refused("emptySlug"));
        }

        let project_id = spec.project_id.clone();
        let project = self
            .db
            .read(move |conn| dao::project::get(conn, &project_id))
            .await?
            .ok_or_else(|| GildeError::not_found(format!("project {}", spec.project_id)))?;

        let settings = self.config.get();
        let timeout_ms = spec
            .timeout_ms
            .filter(|ms| *ms > 0)
            .unwrap_or_else(|| u64::from(settings.dispatch.default_timeout_minutes) * 60_000);
        let permission_mode = spec
            .permission_mode
            .clone()
            .filter(|m| !m.trim().is_empty())
            .unwrap_or_else(|| settings.dispatch.permission_mode.clone());

        let task_id = uuid::Uuid::now_v7().to_string();
        let created_at = now_ms();
        let title = spec
            .title
            .clone()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| title_from_prompt(&spec.prompt));

        let task_row = DispatchTaskRow {
            id: task_id.clone(),
            project_id: Some(project.id.clone()),
            title: title.clone(),
            prompt: spec.prompt.clone(),
            task_slug: task_slug.clone(),
            base_ref: spec.base_ref.clone(),
            worktree_strategy: spec.worktree_strategy.to_column(),
            permission_mode: Some(permission_mode.clone()),
            timeout_ms: Some(timeout_ms as i64),
            created_at,
            finished_at: None,
            status: TaskStatus::Queued.as_str().to_string(),
        };
        let insert = task_row.clone();
        self.db
            .write(move |conn| dao::dispatch::task_insert(conn, &insert))
            .await?;
        self.upsert_task_node(&task_row, TaskStatus::Queued);

        // One shared worktree is provisioned once, lazily: a task whose every
        // target is refused must not leave an empty worktree behind.
        let mut shared: Option<(String, PathBuf)> = None;

        for target in &spec.targets {
            let run_id = uuid::Uuid::now_v7().to_string();
            let artifacts = RunArtifacts::new(self.paths.dispatch_root(), &task_id, &run_id);
            if let Err(err) = artifacts.ensure() {
                tracing::warn!(target: "dispatch", error = %err, dir = %artifacts.dir().display(),
                    "could not create the artifact directory");
            }
            write_artifact(&artifacts.prompt_md(), spec.prompt.as_bytes());

            // (a) the vendor. `auto` is the ONLY path to `fleet.route`.
            let (vendor, mut route_note) = match self.resolve_vendor(target, &spec.prompt).await {
                Ok(resolved) => resolved,
                Err(err) => {
                    self.refuse(&task_id, &run_id, &target.vendor, target, "route", err)
                        .await;
                    continue;
                }
            };
            if route_note.is_none() {
                route_note = target.route_note.clone();
            }

            // (b) is it installed? Read off the live adapter roster, never a
            //     table in Rust.
            if let Err(err) = self.check_installed(&vendor) {
                self.refuse(&task_id, &run_id, &vendor, target, "notInstalled", err)
                    .await;
                continue;
            }

            // (c) where it works. Always an absolute, existing worktree path.
            let provisioned = self
                .provision_worktree(
                    &spec,
                    &project.id,
                    &task_slug,
                    &vendor,
                    &mut shared,
                )
                .await;
            let (worktree_id, cwd) = match provisioned {
                Ok(pair) => pair,
                Err(err) => {
                    self.refuse(&task_id, &run_id, &vendor, target, "worktree", err)
                        .await;
                    continue;
                }
            };

            let account = self.resolve_account(&vendor, target.account_id.as_deref());
            let account_id = self.persisted_account_id(target.account_id.as_deref()).await;

            let row = DispatchRunRow {
                id: run_id.clone(),
                task_id: task_id.clone(),
                vendor: vendor.clone(),
                model_requested: target.model.clone(),
                model_actual: None,
                account_id,
                worktree_id: Some(worktree_id.clone()),
                spawn_id: None,
                // Our row id *is* the mesh run id: it is what the seam stamps on
                // every `proc_registry` row, which is what makes cancellation a
                // single `by_run_id` query.
                run_id: Some(run_id.clone()),
                conversation_id: None,
                status: RunPhase::Queued.as_str().to_string(),
                note: None,
                route_note: route_note.clone(),
                text: None,
                usage_json: None,
                cost_usd: None,
                started_at: None,
                finished_at: None,
                error: None,
            };
            let insert = row.clone();
            self.db
                .write(move |conn| dao::dispatch::run_insert(conn, &insert))
                .await?;
            self.upsert_run_node(&row, &cwd, RunPhase::Queued, None);

            if let Some(note) = route_note.as_deref() {
                self.emit(&task_id, &run_id, &vendor, DispatchEventKind::Routed,
                    json!({"vendor": vendor, "note": note}));
            }

            let ctx = RunCtx {
                task_id: task_id.clone(),
                run_id: run_id.clone(),
                vendor: vendor.clone(),
                model: target.model.clone(),
                account,
                worktree_id: Some(worktree_id),
                cwd,
                prompt: spec.prompt.clone(),
                permission_mode: permission_mode.clone(),
                timeout_ms,
                artifacts,
                resume_of: None,
                started_at: 0,
            };
            self.spawn_run(ctx);
        }

        // Folded from the rows, never from a local tally: a run that finished
        // while the rest of the fan-out was still being provisioned has already
        // settled itself, and writing a local guess over it would walk the task
        // backwards.
        self.set_task_status_from_rows(&task_id).await;
        let status = self
            .db
            .read({
                let id = task_id.clone();
                move |conn| dao::dispatch::task_get(conn, &id)
            })
            .await
            .ok()
            .flatten()
            .map(|row| TaskStatus::from_str(&row.status))
            .unwrap_or(TaskStatus::Queued);

        let mut task = DispatchTask::from(task_row);
        task.status = status;
        Ok(task)
    }

    /// The vendor a target runs as, plus the routing note when there is one.
    ///
    /// The `auto` branch is the single call site of [`DispatchEngine::route`]
    /// inside `create`; every other target returns its own vendor untouched.
    async fn resolve_vendor(
        &self,
        target: &DispatchTarget,
        prompt: &str,
    ) -> Result<(String, Option<String>)> {
        if !target.is_auto() {
            return Ok((target.vendor.clone(), None));
        }
        let decision = self.route(prompt, None).await?;
        Ok((decision.vendor, decision.note))
    }

    /// `installed:false` is a refusal with a reason, not a crash (acceptance 4).
    fn check_installed(&self, vendor: &str) -> Result<()> {
        let adapters = self.sidecar.adapters();
        if adapters.is_empty() {
            // No handshake: the sidecar is down or still starting. Refusing
            // with the sidecar's own error is more actionable than "vendor
            // unknown", which would blame the user's selection.
            return Err(GildeError::SidecarDown);
        }
        match adapters.iter().find(|a| a.vendor == vendor) {
            Some(adapter) if adapter.installed => Ok(()),
            Some(_) => Err(GildeError::refused("notInstalled")),
            None => Err(GildeError::not_found(format!("adapter {vendor}"))),
        }
    }

    /// The worktree one target works in (acceptance 1, 2).
    ///
    /// `PerTarget` gets `<task-slug>-<vendor>` on `gilde/<task-slug>-<vendor>`;
    /// `Shared` provisions once and hands the same pair to everyone;
    /// `Existing` verifies the row is live and its directory is really there.
    async fn provision_worktree(
        &self,
        spec: &DispatchSpec,
        project_id: &str,
        task_slug: &str,
        vendor: &str,
        shared: &mut Option<(String, PathBuf)>,
    ) -> Result<(String, PathBuf)> {
        let per_target = match &spec.worktree_strategy {
            WorktreeStrategy::Existing { worktree_id } => {
                let id = worktree_id.clone();
                let row = self
                    .db
                    .read(move |conn| dao::worktree::get(conn, &id))
                    .await?
                    .ok_or_else(|| GildeError::not_found(format!("worktree {worktree_id}")))?;
                if row.removed_at.is_some() {
                    return Err(GildeError::refused("worktreeRemoved"));
                }
                let cwd = absolute_dir(Path::new(&row.path))?;
                return Ok((row.id, cwd));
            }
            WorktreeStrategy::Shared => {
                if let Some((id, cwd)) = shared.as_ref() {
                    return Ok((id.clone(), cwd.clone()));
                }
                false
            }
            WorktreeStrategy::PerTarget => true,
        };

        let req = CreateWorktreeReq {
            project_id: project_id.to_string(),
            task_name: worktree_task_name(task_slug, vendor, per_target),
            base_ref: spec.base_ref.clone(),
            alias_providers: Vec::new(),
            launch: None,
            post_create_cmd: None,
        };
        let worktree = self.worktrees.create(req).await?;
        let cwd = absolute_dir(Path::new(&worktree.path))?;
        if !per_target {
            *shared = Some((worktree.id.clone(), cwd.clone()));
        }
        Ok((worktree.id, cwd))
    }

    /// Turn an `accountId` into the `Account` the adapters take. `None` is the
    /// ambient CLI login and is the M1 default (DESIGN §10-5).
    fn resolve_account(&self, vendor: &str, account_id: Option<&str>) -> Option<Account> {
        let id = account_id.filter(|id| !id.is_empty())?;
        let slot = self.accounts.iter().find(|slot| slot.id == id)?;
        Some(
            Account::new(vendor)
                .with_label(Some(slot.label.clone()))
                .with_config_dir(slot.config_dir.clone()),
        )
    }

    /// The `dispatch_run.account_id` value. The column has a foreign key onto
    /// `account`, and the ambient slot is synthetic until something imports it —
    /// so an id with no row behind it is stored as NULL rather than failing the
    /// insert. The `Account` still reaches the adapter either way.
    async fn persisted_account_id(&self, account_id: Option<&str>) -> Option<String> {
        let id = account_id.filter(|id| !id.is_empty())?.to_string();
        let probe = id.clone();
        match self.db.read(move |conn| dao::account::get(conn, &probe)).await {
            Ok(Some(_)) => Some(id),
            _ => None,
        }
    }

    /// Record a target that never ran, with the reason on its own row.
    async fn refuse(
        &self,
        task_id: &str,
        run_id: &str,
        vendor: &str,
        target: &DispatchTarget,
        reason: &str,
        err: GildeError,
    ) {
        let detail = err.to_string();
        tracing::info!(target: "dispatch", run = %run_id, vendor, reason, error = %detail,
            "target refused before it ran");
        let at = now_ms();
        let row = DispatchRunRow {
            id: run_id.to_string(),
            task_id: task_id.to_string(),
            vendor: vendor.to_string(),
            model_requested: target.model.clone(),
            model_actual: None,
            account_id: None,
            worktree_id: None,
            spawn_id: None,
            run_id: Some(run_id.to_string()),
            conversation_id: None,
            status: RunPhase::Error.as_str().to_string(),
            note: Some(reason.to_string()),
            route_note: target.route_note.clone(),
            text: None,
            usage_json: None,
            cost_usd: None,
            started_at: Some(at),
            finished_at: Some(at),
            error: Some(detail.clone()),
        };
        let insert = row.clone();
        if let Err(err) = self
            .db
            .write(move |conn| dao::dispatch::run_insert(conn, &insert))
            .await
        {
            tracing::warn!(target: "dispatch", error = %err, "could not record the refused run");
        }
        self.upsert_run_node(&row, Path::new(""), RunPhase::Error, None);
        self.emit(
            task_id,
            run_id,
            vendor,
            DispatchEventKind::Error,
            json!({"reason": reason, "message": detail, "prerefused": true}),
        );
    }

    // -- driving -------------------------------------------------------------

    /// Hand a run to its own task. Takes `&Arc<Self>` because the driver
    /// outlives the call that created it and must observe the same `live` map.
    fn spawn_run(self: &Arc<Self>, ctx: RunCtx) {
        let engine = Arc::clone(self);
        spawn(async move {
            engine.drive(ctx).await;
        });
    }

    async fn drive(self: Arc<Self>, mut ctx: RunCtx) {
        let permit = match self.limiter.clone().acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => return,
        };

        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(Notify::new());
        let generation = self.sidecar.generation();
        self.live.insert(
            ctx.run_id.clone(),
            LiveRun {
                cancelled: cancelled.clone(),
                cancel: cancel.clone(),
            },
        );

        ctx.started_at = now_ms();
        self.mark_started(&ctx).await;

        let outcome = if cancelled.load(Ordering::SeqCst) {
            Outcome::cancelled()
        } else {
            self.execute(&ctx, generation, &cancelled, &cancel).await
        };

        self.settle(&ctx, outcome).await;
        self.live.remove(&ctx.run_id);
        drop(permit);
    }

    /// Choose the lane and run it. Streaming is decided by the adapter's own
    /// capability flag (DESIGN §5-8, acceptance 3).
    async fn execute(
        &self,
        ctx: &RunCtx,
        generation: i64,
        cancelled: &Arc<AtomicBool>,
        cancel: &Arc<Notify>,
    ) -> Outcome {
        let task = self.run_task(ctx);
        let deadline = Duration::from_millis(ctx.timeout_ms) + RUN_WALL_CLOCK_GRACE;

        if ctx.resume_of.is_some() {
            return self.run_blocking(ctx, method::RESUME, self.resume_params(ctx, &task), cancel, deadline)
                .await;
        }
        if self.sidecar.can_stream(&ctx.vendor) {
            // Subscribe *before* the request: an event must not be able to
            // arrive between "accepted" and "listening".
            let events = self.sidecar.subscribe_run_events();
            let deaths = self.sidecar.subscribe_deaths();
            let params = json!({"runId": ctx.run_id, "vendor": ctx.vendor, "task": task});
            match self
                .sidecar
                .call_detailed::<_, Value>(method::STREAM, params, Some(STREAM_ACCEPT_TIMEOUT))
                .await
            {
                Ok(_) => {
                    self.mark_running(ctx).await;
                    self.consume_stream(ctx, generation, events, deaths, cancelled, cancel, deadline)
                        .await
                }
                Err(err) => Outcome::from_call_error(err),
            }
        } else {
            let params = json!({"runId": ctx.run_id, "vendor": ctx.vendor, "task": task});
            self.run_blocking(ctx, method::RUN, params, cancel, deadline)
                .await
        }
    }

    fn run_task(&self, ctx: &RunCtx) -> RunTask {
        RunTask::new(
            ctx.prompt.clone(),
            ctx.cwd.display().to_string(),
            ctx.timeout_ms,
        )
        .with_model(ctx.model.clone())
        .with_account(ctx.account.clone())
        // Translated, never forwarded verbatim: the task's mode is written in
        // claude's vocabulary and the other CLIs reject it outright (rule 5).
        .with_permission_mode(permission_mode_for(&ctx.vendor, &ctx.permission_mode))
    }

    fn resume_params(&self, ctx: &RunCtx, task: &RunTask) -> Value {
        let (conversation_id, message) = ctx
            .resume_of
            .clone()
            .expect("resume_params is only reached for a resume");
        json!({
            "runId": ctx.run_id,
            "vendor": ctx.vendor,
            "conversationId": conversation_id,
            "message": message,
            "opts": {
                "model": task.model,
                "timeoutMs": task.timeout_ms,
                "account": task.account,
                "extra": {"cwd": ctx.cwd.display().to_string()},
            },
        })
    }

    /// The `mesh.run` / `mesh.resume` lane: one request, one `RunResult`.
    ///
    /// The result is turned into the same synthetic `done` event the streaming
    /// lane produces, so `stream.jsonl`, `run_event` and `gilde://dispatch/event`
    /// look identical for both lanes and the UI has one shape to render.
    async fn run_blocking(
        &self,
        ctx: &RunCtx,
        rpc_method: &'static str,
        params: Value,
        cancel: &Arc<Notify>,
        deadline: Duration,
    ) -> Outcome {
        self.mark_running(ctx).await;
        let call = self
            .sidecar
            .call_detailed::<_, RunResult>(rpc_method, params, Some(deadline));
        tokio::pin!(call);

        tokio::select! {
            biased;
            _ = cancel.notified() => Outcome::cancelled(),
            result = &mut call => match result {
                Ok(result) => {
                    // The same synthetic `done` the streaming lane produces, so
                    // both lanes leave identical artifacts and identical events.
                    let event = json!({"type": "done", "result": &result});
                    self.append_stream(ctx, &event);
                    self.mirror_event(ctx, DispatchEventKind::Done, &event).await;
                    self.emit(
                        &ctx.task_id,
                        &ctx.run_id,
                        &ctx.vendor,
                        DispatchEventKind::Done,
                        event,
                    );
                    Outcome::done(result)
                }
                Err(err) => Outcome::from_call_error(err),
            },
        }
    }

    /// The `mesh.stream` lane: `RunEvent`s until exactly one terminal one.
    #[allow(clippy::too_many_arguments)]
    async fn consume_stream(
        &self,
        ctx: &RunCtx,
        generation: i64,
        mut events: broadcast::Receiver<RunEventEnvelope>,
        mut deaths: broadcast::Receiver<crate::sidecar::SidecarDeath>,
        cancelled: &Arc<AtomicBool>,
        cancel: &Arc<Notify>,
        deadline: Duration,
    ) -> Outcome {
        let mut text = TextMerge::default();
        let expires = tokio::time::Instant::now() + deadline;

        loop {
            if cancelled.load(Ordering::SeqCst) {
                self.flush_text(ctx, &mut text).await;
                return Outcome::cancelled();
            }
            let flush_at = text.deadline();

            let step = tokio::select! {
                biased;
                _ = cancel.notified() => Step::Cancelled,
                death = deaths.recv() => match death {
                    Ok(death) if death.generation == generation => Step::Death(death.reason),
                    Ok(_) => continue,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => Step::Death("sidecar gone".into()),
                },
                received = events.recv() => match received {
                    Ok(envelope) => Step::Event(envelope),
                    Err(broadcast::error::RecvError::Lagged(missed)) => Step::Lagged(missed),
                    Err(broadcast::error::RecvError::Closed) => Step::Death("event channel closed".into()),
                },
                _ = tokio::time::sleep_until(flush_at.unwrap_or(expires)), if flush_at.is_some() => Step::Flush,
                _ = tokio::time::sleep_until(expires) => Step::Expired,
            };

            match step {
                Step::Cancelled => {
                    self.flush_text(ctx, &mut text).await;
                    return Outcome::cancelled();
                }
                Step::Death(reason) => {
                    self.flush_text(ctx, &mut text).await;
                    return Outcome::interrupted(reason);
                }
                Step::Lagged(missed) => {
                    // `mesh.event` is lossy by contract (DESIGN §5-1); the file
                    // and the terminal event still settle the run correctly.
                    tracing::warn!(target: "dispatch", run = %ctx.run_id, missed,
                        "dropped stream events under back pressure");
                }
                Step::Flush => self.flush_text(ctx, &mut text).await,
                Step::Expired => {
                    self.flush_text(ctx, &mut text).await;
                    return Outcome::interrupted(format!(
                        "no terminal event within {}s of the task timeout",
                        deadline.as_secs()
                    ));
                }
                Step::Event(envelope) => {
                    if envelope.run_id != ctx.run_id {
                        continue;
                    }
                    if let Some(outcome) = self.on_event(ctx, envelope, &mut text).await {
                        return outcome;
                    }
                }
            }
        }
    }

    /// Handle one `RunEvent`. Returns `Some` for a terminal one.
    async fn on_event(
        &self,
        ctx: &RunCtx,
        envelope: RunEventEnvelope,
        text: &mut TextMerge,
    ) -> Option<Outcome> {
        let value = envelope.event;
        let kind = event_kind(&value);

        // Every event, verbatim, to the file and the mirror row — before any
        // coalescing. The files are the record; the UI stream is the summary.
        self.append_stream(ctx, &value);
        self.mirror_event(ctx, kind, &value).await;

        match kind {
            DispatchEventKind::Text => {
                let chunk = value.get("text").and_then(Value::as_str).unwrap_or("");
                text.push(chunk);
                if text.is_due() {
                    self.flush_text(ctx, text).await;
                }
                None
            }
            DispatchEventKind::Done => {
                self.flush_text(ctx, text).await;
                self.emit(&ctx.task_id, &ctx.run_id, &ctx.vendor, kind, value.clone());
                match serde_json::from_value::<RunEvent>(value) {
                    Ok(RunEvent::Done { result }) => Some(Outcome::done(result)),
                    // A `done` we cannot decode is still terminal: leaving the
                    // run `running` forever would be the worse failure.
                    _ => Some(
                        Outcome::error("a done event did not carry a result").already_emitted(),
                    ),
                }
            }
            DispatchEventKind::Error => {
                self.flush_text(ctx, text).await;
                self.emit(&ctx.task_id, &ctx.run_id, &ctx.vendor, kind, value.clone());
                let message = value
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("the adapter reported an error");
                Some(Outcome::error(message).already_emitted())
            }
            _ => {
                self.flush_text(ctx, text).await;
                self.patch_from_event(ctx, kind, &value);
                self.emit(&ctx.task_id, &ctx.run_id, &ctx.vendor, kind, value);
                None
            }
        }
    }

    async fn flush_text(&self, ctx: &RunCtx, text: &mut TextMerge) {
        if let Some(merged) = text.take() {
            self.emit(
                &ctx.task_id,
                &ctx.run_id,
                &ctx.vendor,
                DispatchEventKind::Text,
                json!({"type": "text", "text": merged}),
            );
            self.patch_activity(ctx);
        }
    }

    // -- persistence ---------------------------------------------------------

    fn append_stream(&self, ctx: &RunCtx, event: &Value) {
        let line = match serde_json::to_string(event) {
            Ok(line) => line,
            Err(err) => {
                tracing::warn!(target: "dispatch", error = %err, "an event would not serialize");
                return;
            }
        };
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(ctx.artifacts.stream_jsonl());
        match file {
            Ok(mut file) => {
                if let Err(err) = writeln!(file, "{line}") {
                    tracing::warn!(target: "dispatch", error = %err, "could not append to stream.jsonl");
                }
            }
            Err(err) => {
                tracing::warn!(target: "dispatch", error = %err, "could not open stream.jsonl");
            }
        }
    }

    async fn mirror_event(&self, ctx: &RunCtx, kind: DispatchEventKind, event: &Value) {
        let run_id = ctx.run_id.clone();
        let kind = kind_str(kind).to_string();
        let payload = serde_json::to_string(event).ok();
        let at = now_ms();
        if let Err(err) = self
            .db
            .write(move |conn| {
                dao::dispatch::run_event_append(conn, &run_id, at, &kind, payload.as_deref())?;
                Ok(())
            })
            .await
        {
            tracing::debug!(target: "dispatch", error = %err, "could not mirror a run event");
        }
    }

    async fn mark_started(&self, ctx: &RunCtx) {
        let run_id = ctx.run_id.clone();
        let at = ctx.started_at;
        let _ = self
            .db
            .write(move |conn| {
                dao::dispatch::run_set_started(conn, &run_id, at)?;
                dao::dispatch::run_update_status(
                    conn,
                    &run_id,
                    RunPhase::Spawning.as_str(),
                    None,
                )?;
                Ok(())
            })
            .await;
        self.patch_run_status(&ctx.run_id, RunPhase::Spawning);
        self.emit(
            &ctx.task_id,
            &ctx.run_id,
            &ctx.vendor,
            DispatchEventKind::Spawned,
            json!({
                "vendor": ctx.vendor,
                "model": ctx.model,
                "cwd": ctx.cwd.display().to_string(),
                "worktreeId": ctx.worktree_id,
                "timeoutMs": ctx.timeout_ms,
            }),
        );
        self.set_task_status_from_rows(&ctx.task_id).await;
    }

    async fn mark_running(&self, ctx: &RunCtx) {
        let run_id = ctx.run_id.clone();
        let _ = self
            .db
            .write(move |conn| {
                dao::dispatch::run_update_status(conn, &run_id, RunPhase::Running.as_str(), None)?;
                Ok(())
            })
            .await;
        self.patch_run_status(&ctx.run_id, RunPhase::Running);
    }

    /// Terminal update. One guarded statement, so a cancel racing the stream's
    /// own terminal event cannot settle the same run twice with two different
    /// answers — whoever gets there first wins and the loser is a no-op.
    async fn settle(&self, ctx: &RunCtx, outcome: Outcome) {
        let phase = outcome.phase;
        let finished_at = now_ms();

        // The verbatim result is the artifact; the column may hold a pointer.
        let mut text_column: Option<String> = None;
        let mut model_actual = None;
        let mut conversation_id = None;
        let mut usage = None;
        let mut cost = None;

        if let Some(result) = outcome.result.as_ref() {
            match serde_json::to_vec_pretty(result) {
                Ok(bytes) => write_artifact(&ctx.artifacts.result_json(), &bytes),
                Err(err) => {
                    tracing::warn!(target: "dispatch", error = %err, "result.json would not serialize")
                }
            }
            model_actual = result.model.clone();
            conversation_id = Some(result.conversation_id.clone());
            cost = cost_usd(result);
            usage = usage_json(result.usage.as_ref(), cost);
            text_column = Some(store_result_text(
                self.paths.runs_dir(),
                &ctx.run_id,
                &result.text,
            ));
        }

        let run_id = ctx.run_id.clone();
        let status = phase.as_str().to_string();
        let error = outcome.error.clone();
        let note = outcome.note.clone();
        let changed = self
            .db
            .write(move |conn| {
                let changed = conn.execute(
                    "UPDATE dispatch_run
                        SET status = ?2, text = ?3, usage_json = ?4, cost_usd = ?5,
                            model_actual = COALESCE(?6, model_actual),
                            conversation_id = COALESCE(?7, conversation_id),
                            note = COALESCE(?8, note),
                            error = ?9, finished_at = ?10
                      WHERE id = ?1 AND status IN ('queued', 'spawning', 'running')",
                    rusqlite::params![
                        run_id,
                        status,
                        text_column,
                        usage,
                        cost,
                        model_actual,
                        conversation_id,
                        note,
                        error,
                        finished_at,
                    ],
                )?;
                Ok(changed)
            })
            .await
            .unwrap_or(0);

        if changed == 0 {
            tracing::debug!(target: "dispatch", run = %ctx.run_id, phase = phase.as_str(),
                "the run was already settled; keeping the first answer");
            return;
        }

        self.patch_settled(ctx, &outcome, finished_at);

        // Exactly one terminal event per run. When the adapter produced one it
        // was forwarded verbatim already; otherwise the host's verdict goes out
        // here, so a card never sits on a spinner because a channel died.
        if !outcome.emitted {
            let (kind, payload) = match phase {
                RunPhase::Cancelled => (
                    DispatchEventKind::Cancelled,
                    json!({"reason": "cancelled"}),
                ),
                RunPhase::Interrupted => (
                    DispatchEventKind::Interrupted,
                    json!({"reason": outcome.error.clone().unwrap_or_default()}),
                ),
                _ => (
                    DispatchEventKind::Error,
                    json!({"message": outcome.error.clone().unwrap_or_default()}),
                ),
            };
            self.emit(&ctx.task_id, &ctx.run_id, &ctx.vendor, kind, payload);
        }

        self.set_task_status_from_rows(&ctx.task_id).await;
        if let Some(worktree_id) = ctx.worktree_id.as_deref() {
            // The card shows `+N −M`; the run just changed them.
            self.worktrees.refresh_and_publish(worktree_id).await;
        }
    }

    // -- queries -------------------------------------------------------------

    /// `dispatch_list`. `Active` filters over a wider window than `limit` so an
    /// old unfinished task is not hidden by newer finished ones.
    pub async fn list(&self, scope: DispatchScope, limit: u32) -> Result<Vec<DispatchTask>> {
        let limit = limit.clamp(1, 500) as i64;
        let read_limit = if scope == DispatchScope::Active {
            (limit * 4).max(200)
        } else {
            limit
        };
        let rows = self
            .db
            .read(move |conn| dao::dispatch::task_list(conn, read_limit))
            .await?;
        let mut tasks: Vec<DispatchTask> = rows.into_iter().map(DispatchTask::from).collect();
        if scope == DispatchScope::Active {
            tasks.retain(|t| !t.status.is_terminal());
        }
        tasks.truncate(limit as usize);
        Ok(tasks)
    }

    /// `dispatch_get` — the task plus its runs, ready to restore a board after
    /// a restart (DESIGN §9-7).
    pub async fn get(&self, task_id: &str) -> Result<DispatchTaskDetail> {
        let id = task_id.to_string();
        let (task, runs) = self
            .db
            .read(move |conn| {
                let task = dao::dispatch::task_get(conn, &id)?;
                let runs = dao::dispatch::runs_of_task(conn, &id)?;
                Ok((task, runs))
            })
            .await?;
        let task = task.ok_or_else(|| GildeError::not_found(format!("dispatch task {task_id}")))?;
        let runs_dir = self.paths.runs_dir().to_path_buf();
        Ok(DispatchTaskDetail {
            task: DispatchTask::from(task),
            runs: runs
                .into_iter()
                .map(|row| DispatchRun::from_row(row, &runs_dir))
                .collect(),
        })
    }

    /// The artifact directory of one run, for "reveal in Finder".
    pub async fn artifact_dir(&self, run_id: &str) -> Result<PathBuf> {
        let row = self.run_row(run_id).await?;
        Ok(RunArtifacts::new(self.paths.dispatch_root(), &row.task_id, &row.id)
            .dir()
            .to_path_buf())
    }

    async fn run_row(&self, run_id: &str) -> Result<DispatchRunRow> {
        let id = run_id.to_string();
        self.db
            .read(move |conn| dao::dispatch::run_get(conn, &id))
            .await?
            .ok_or_else(|| GildeError::not_found(format!("run {run_id}")))
    }

    // -- cancellation (DESIGN §5-9, acceptance 7) ----------------------------

    /// Cancel every unfinished run of a task.
    pub async fn cancel_task(&self, task_id: &str) -> Result<Vec<KillReport>> {
        let id = task_id.to_string();
        let runs = self
            .db
            .read(move |conn| dao::dispatch::runs_of_task(conn, &id))
            .await?;
        if runs.is_empty() {
            return Err(GildeError::not_found(format!("dispatch task {task_id}")));
        }
        let mut reports = Vec::new();
        for row in runs {
            if RunPhase::from_str(&row.status).is_terminal() {
                continue;
            }
            reports.push(self.cancel_run(&row.id).await?);
        }
        self.set_task_status_from_rows(task_id).await;
        Ok(reports)
    }

    /// Cancel one run: kill its processes and, **in parallel**, tell the
    /// sidecar. The kills never wait on the RPC, so a sidecar that is stopped,
    /// hung or dead cannot keep a run alive.
    pub async fn cancel_run(&self, run_id: &str) -> Result<KillReport> {
        let row = self.run_row(run_id).await?;
        let live = self.live.get(run_id).map(|entry| entry.clone());
        if let Some(live) = live.as_ref() {
            live.cancelled.store(true, Ordering::SeqCst);
            live.cancel.notify_one();
        }

        let rows = {
            let id = run_id.to_string();
            self.db
                .read(move |conn| dao::proc::by_run_id(conn, &id))
                .await
                .unwrap_or_default()
        };
        let opts = KillOpts::from_config(&self.config.get().supervision);

        let kills = async {
            let mut reports = Vec::new();
            for proc_row in &rows {
                reports.push(kill_tree(&self.supervisor, &proc_row.spawn_id, opts).await);
            }
            reports
        };
        let courtesy = async {
            // Timed, and its failure is not the caller's problem: this call is
            // the convenience half of the pair.
            let call = self.sidecar.call_detailed::<_, Value>(
                method::CANCEL,
                json!({"runId": run_id}),
                Some(CANCEL_RPC_TIMEOUT),
            );
            match tokio::time::timeout(CANCEL_RPC_TIMEOUT, call).await {
                Ok(Ok(_)) => true,
                Ok(Err(err)) => {
                    tracing::debug!(target: "dispatch", run = %run_id, error = %err,
                        "mesh.cancel did not land; the kill is the authority");
                    false
                }
                Err(_) => {
                    tracing::info!(target: "dispatch", run = %run_id,
                        "mesh.cancel timed out; the kill is the authority");
                    false
                }
            }
        };
        let (reports, _acked) = tokio::join!(kills, courtesy);

        // The driver settles a live run through its cancel notification; a run
        // this process is not driving (an app restart, a queued row) is settled
        // here so a cancel is never a silent no-op.
        if live.is_none() && !RunPhase::from_str(&row.status).is_terminal() {
            let ctx = self.ctx_from_row(&row).await;
            self.settle(&ctx, Outcome::cancelled()).await;
        }

        Ok(merge_reports(run_id, reports))
    }

    // -- resume (DESIGN §9-7) ------------------------------------------------

    /// Continue a finished run as a **new** run in the same task.
    ///
    /// A resume is not an overwrite: the first answer stays on its own row and
    /// its own artifacts, and the graph gets a `resumes` edge between the two.
    /// Returns as soon as the new row exists — a resume can take as long as the
    /// original, and a command may not block on it (DESIGN §3).
    pub async fn resume(
        self: &Arc<Self>,
        run_id: &str,
        message: &str,
    ) -> Result<super::model::RunOutcome> {
        if message.trim().is_empty() {
            return Err(GildeError::refused("emptyMessage"));
        }
        let previous = self.run_row(run_id).await?;
        let conversation_id = previous
            .conversation_id
            .clone()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| GildeError::refused("notResumable"))?;

        let task_id = previous.task_id.clone();
        let id = task_id.clone();
        let task = self
            .db
            .read(move |conn| dao::dispatch::task_get(conn, &id))
            .await?
            .ok_or_else(|| GildeError::not_found(format!("dispatch task {task_id}")))?;

        let cwd = match previous.worktree_id.as_deref() {
            Some(worktree_id) => {
                let id = worktree_id.to_string();
                let row = self
                    .db
                    .read(move |conn| dao::worktree::get(conn, &id))
                    .await?
                    .ok_or_else(|| GildeError::not_found(format!("worktree {worktree_id}")))?;
                absolute_dir(Path::new(&row.path))?
            }
            None => return Err(GildeError::refused("noWorktree")),
        };

        let new_id = uuid::Uuid::now_v7().to_string();
        let artifacts = RunArtifacts::new(self.paths.dispatch_root(), &previous.task_id, &new_id);
        if let Err(err) = artifacts.ensure() {
            tracing::warn!(target: "dispatch", error = %err, "could not create the resume artifacts");
        }
        write_artifact(&artifacts.prompt_md(), message.as_bytes());

        let row = DispatchRunRow {
            id: new_id.clone(),
            task_id: previous.task_id.clone(),
            vendor: previous.vendor.clone(),
            model_requested: previous.model_requested.clone(),
            model_actual: None,
            account_id: previous.account_id.clone(),
            worktree_id: previous.worktree_id.clone(),
            spawn_id: None,
            run_id: Some(new_id.clone()),
            conversation_id: Some(conversation_id.clone()),
            status: RunPhase::Queued.as_str().to_string(),
            note: Some(format!("resume:{run_id}")),
            route_note: previous.route_note.clone(),
            text: None,
            usage_json: None,
            cost_usd: None,
            started_at: None,
            finished_at: None,
            error: None,
        };
        let insert = row.clone();
        self.db
            .write(move |conn| dao::dispatch::run_insert(conn, &insert))
            .await?;
        self.upsert_run_node(&row, &cwd, RunPhase::Queued, None);
        if let Some(bus) = self.bus() {
            bus.push(GraphOp::UpsertEdge {
                edge: GraphEdge::new(
                    EdgeKind::Resumes,
                    node_id(NodeKind::Run, &new_id),
                    node_id(NodeKind::Run, run_id),
                ),
            });
        }

        let settings = self.config.get();
        let ctx = RunCtx {
            task_id: previous.task_id.clone(),
            run_id: new_id.clone(),
            vendor: previous.vendor.clone(),
            model: previous.model_requested.clone(),
            account: self.resolve_account(&previous.vendor, previous.account_id.as_deref()),
            worktree_id: previous.worktree_id.clone(),
            cwd,
            prompt: message.to_string(),
            permission_mode: task
                .permission_mode
                .unwrap_or_else(|| settings.dispatch.permission_mode.clone()),
            timeout_ms: task.timeout_ms.unwrap_or(0).max(0) as u64,
            artifacts,
            resume_of: Some((conversation_id.clone(), message.to_string())),
            started_at: 0,
        };
        let timeout_ms = if ctx.timeout_ms == 0 {
            u64::from(settings.dispatch.default_timeout_minutes) * 60_000
        } else {
            ctx.timeout_ms
        };
        let ctx = RunCtx { timeout_ms, ..ctx };
        self.spawn_run(ctx);

        Ok(super::model::RunOutcome {
            run_id: new_id,
            status: RunPhase::Queued,
            conversation_id: Some(conversation_id),
            note: Some(format!("resume:{run_id}")),
        })
    }

    // -- routing (acceptance 10) ---------------------------------------------

    /// The **only** caller of `fleet.route` in this app.
    ///
    /// Reached from exactly two places: the `dispatch_route` command, which the
    /// composer uses to show a decision *before* running it, and the
    /// `vendor == "auto"` branch of [`DispatchEngine::create`]. Nothing else
    /// routes — silently re-targeting a user's explicit choice is the "built-in
    /// philosophy" DECISIONS forbids.
    pub async fn route(&self, prompt: &str, policy: Option<Value>) -> Result<RouteDecision> {
        let mut params = json!({"task": {"prompt": prompt}});
        if let Some(policy) = policy {
            params["policy"] = policy;
        }
        let decision: RouteDecision = self.sidecar.call(method::ROUTE, params).await?;
        Ok(decision)
    }

    // -- the bridge to the interactive lane (acceptance 11) ------------------

    /// Open a PTY session in the run's worktree.
    ///
    /// A shell, not the vendor's CLI: "open terminal here" answers "let me look
    /// at what it did", and starting a second agent in the same worktree is a
    /// different (and destructive) intention.
    pub async fn open_terminal(
        &self,
        run_id: &str,
        on_chunk: tauri::ipc::Channel<PtyChunk>,
    ) -> Result<PtySessionInfo> {
        let row = self.run_row(run_id).await?;
        let worktree_id = row
            .worktree_id
            .clone()
            .ok_or_else(|| GildeError::refused("noWorktree"))?;
        let probe = worktree_id.clone();
        let worktree = self
            .db
            .read(move |conn| dao::worktree::get(conn, &probe))
            .await?
            .ok_or_else(|| GildeError::not_found(format!("worktree {worktree_id}")))?;

        let pty = self
            .pty
            .lock()
            .clone()
            .ok_or_else(|| GildeError::internal("no pty manager is wired to the dispatch engine"))?;

        // A real winsize: codex renders nothing into a 0x0 pty (PINNED-VERSIONS
        // §9-4), and the frontend resizes to the real geometry on mount anyway.
        let req = OpenSessionReq {
            project_id: worktree.project_id.clone(),
            worktree_id: worktree.id.clone(),
            agent: AgentKind::Shell,
            account_id: None,
            cols: 120,
            rows: 32,
            title: Some(format!("{} · {}", row.vendor, worktree.slug)),
            prompt: None,
            args: None,
        };
        pty.open_request(req, on_chunk).await
    }

    // -- recovery ------------------------------------------------------------

    /// Settle runs this process cannot possibly still be driving.
    ///
    /// A row left `queued`/`spawning`/`running` at boot belonged to an app
    /// instance that is gone. That is the app failing, not the agent, so it is
    /// `interrupted` (DESIGN §5-10) — and settling it is what stops a restored
    /// board from showing a spinner that will never stop.
    pub async fn recover_interrupted_runs(&self) -> Result<u32> {
        let at = now_ms();
        let settled = self
            .db
            .write(move |conn| {
                let changed = conn.execute(
                    "UPDATE dispatch_run
                        SET status = 'interrupted', finished_at = ?1,
                            error = COALESCE(error, 'the app stopped while this run was in flight')
                      WHERE status IN ('queued', 'spawning', 'running')",
                    rusqlite::params![at],
                )?;
                Ok(changed as u32)
            })
            .await?;
        if settled > 0 {
            tracing::info!(target: "dispatch", settled, "runs from a previous instance settled as interrupted");
            let tasks = self
                .db
                .read(|conn| dao::dispatch::task_list(conn, 200))
                .await
                .unwrap_or_default();
            for task in tasks {
                if !TaskStatus::from_str(&task.status).is_terminal() {
                    self.set_task_status_from_rows(&task.id).await;
                }
            }
        }
        Ok(settled)
    }

    // -- task status ---------------------------------------------------------

    async fn set_task_status_from_rows(&self, task_id: &str) {
        let id = task_id.to_string();
        let rows = match self
            .db
            .read(move |conn| dao::dispatch::runs_of_task(conn, &id))
            .await
        {
            Ok(rows) => rows,
            Err(err) => {
                tracing::warn!(target: "dispatch", error = %err, "could not read runs for the task status");
                return;
            }
        };
        let phases: Vec<RunPhase> = rows
            .iter()
            .map(|row| RunPhase::from_str(&row.status))
            .collect();
        self.set_task_status(task_id, fold_task_status(&phases)).await;
    }

    async fn set_task_status(&self, task_id: &str, status: TaskStatus) {
        let id = task_id.to_string();
        let value = status.as_str().to_string();
        let finished_at = status.is_terminal().then(now_ms);
        let _ = self
            .db
            .write(move |conn| {
                match finished_at {
                    Some(at) => dao::dispatch::task_finish(conn, &id, &value, at)?,
                    None => dao::dispatch::task_set_status(conn, &id, &value)?,
                };
                Ok(())
            })
            .await;
        if let Some(bus) = self.bus() {
            bus.push(GraphOp::PatchNode {
                id: node_id(NodeKind::Dispatch, task_id),
                status: Some(task_node_status(status)),
                confidence: Some(1.0),
                evidence: None,
                badges: None,
                metrics: None,
            });
        }
    }

    /// Rebuild a driving context for a run this process is not driving — used
    /// by `cancel_run` to settle a row left behind by a previous instance.
    async fn ctx_from_row(&self, row: &DispatchRunRow) -> RunCtx {
        let cwd = match row.worktree_id.as_deref() {
            Some(id) => {
                let id = id.to_string();
                self.db
                    .read(move |conn| dao::worktree::get(conn, &id))
                    .await
                    .ok()
                    .flatten()
                    .map(|w| PathBuf::from(w.path))
                    .unwrap_or_default()
            }
            None => PathBuf::new(),
        };
        RunCtx {
            task_id: row.task_id.clone(),
            run_id: row.id.clone(),
            vendor: row.vendor.clone(),
            model: row.model_requested.clone(),
            account: None,
            worktree_id: row.worktree_id.clone(),
            cwd,
            prompt: String::new(),
            permission_mode: String::new(),
            timeout_ms: 0,
            artifacts: RunArtifacts::new(self.paths.dispatch_root(), &row.task_id, &row.id),
            resume_of: None,
            started_at: row.started_at.unwrap_or_else(now_ms),
        }
    }

    // -- graph + events ------------------------------------------------------

    fn emit(
        &self,
        task_id: &str,
        run_id: &str,
        vendor: &str,
        kind: DispatchEventKind,
        payload: Value,
    ) {
        if let Some(bus) = self.bus() {
            bus.emit(
                DISPATCH_EVENT,
                DispatchEvent {
                    task_id: task_id.to_string(),
                    run_id: run_id.to_string(),
                    vendor: vendor.to_string(),
                    kind,
                    payload,
                },
            );
        }
    }

    fn upsert_task_node(&self, row: &DispatchTaskRow, status: TaskStatus) {
        let Some(bus) = self.bus() else { return };
        let id = node_id(NodeKind::Dispatch, &row.id);
        let mut node = GraphNode::new(
            &id,
            NodeKind::Dispatch,
            &row.title,
            task_node_status(status),
            now_ms(),
        );
        node.sublabel = Some(row.task_slug.clone());
        if let Some(project_id) = row.project_id.as_deref() {
            let parent = node_id(NodeKind::Project, project_id);
            node.parent_id = Some(parent.clone());
            bus.push(GraphOp::UpsertEdge {
                edge: GraphEdge::new(EdgeKind::Contains, parent, id.clone()),
            });
        }
        bus.push(GraphOp::UpsertNode { node });
    }

    /// A run node hangs under its **worktree**, exactly like a session does, so
    /// "what is happening in this worktree" has one answer (DESIGN §9-5). The
    /// task keeps its own `dispatched-to` edge to the run.
    fn upsert_run_node(&self, row: &DispatchRunRow, cwd: &Path, phase: RunPhase, tool: Option<&str>) {
        let Some(bus) = self.bus() else { return };
        let id = node_id(NodeKind::Run, &row.id);
        let mut node = GraphNode::new(
            &id,
            NodeKind::Run,
            &row.vendor,
            run_node_status(phase),
            now_ms(),
        );
        node.sublabel = Some(run_sublabel(
            row.model_actual.as_deref().or(row.model_requested.as_deref()),
            tool,
        ));
        node.meta.vendor = Some(row.vendor.clone());
        node.meta.account_id = row.account_id.clone();
        node.meta.worktree_id = row.worktree_id.clone();
        if !cwd.as_os_str().is_empty() {
            node.meta.cwd = Some(cwd.display().to_string());
        }
        if let Some(worktree_id) = row.worktree_id.as_deref() {
            let parent = node_id(NodeKind::Worktree, worktree_id);
            node.parent_id = Some(parent.clone());
            bus.push(GraphOp::UpsertEdge {
                edge: GraphEdge::new(EdgeKind::Contains, parent, id.clone()),
            });
        }
        let mut edge = GraphEdge::new(
            EdgeKind::DispatchedTo,
            node_id(NodeKind::Dispatch, &row.task_id),
            id.clone(),
        );
        edge.active = !phase.is_terminal();
        bus.push(GraphOp::UpsertEdge { edge });
        bus.push(GraphOp::UpsertNode { node });
    }

    fn patch_run_status(&self, run_id: &str, phase: RunPhase) {
        let Some(bus) = self.bus() else { return };
        bus.push(GraphOp::PatchNode {
            id: node_id(NodeKind::Run, run_id),
            status: Some(run_node_status(phase)),
            confidence: Some(1.0),
            evidence: None,
            badges: None,
            metrics: None,
        });
    }

    /// Fold one non-terminal event into the run's node (acceptance 6).
    fn patch_from_event(&self, ctx: &RunCtx, kind: DispatchEventKind, event: &Value) {
        let Some(bus) = self.bus() else { return };
        match kind {
            DispatchEventKind::Usage => {
                let usage = event.get("usage");
                let mut metrics = NodeMetrics::default();
                metrics.input_tokens = usage.and_then(|u| u.get("inputTokens")).and_then(Value::as_u64);
                metrics.output_tokens = usage.and_then(|u| u.get("outputTokens")).and_then(Value::as_u64);
                metrics.cached_tokens = usage
                    .and_then(|u| u.get("cachedInputTokens"))
                    .and_then(Value::as_u64);
                metrics.cost_usd = usage.and_then(|u| u.get("costUsd")).and_then(Value::as_f64);
                metrics.elapsed_ms = Some((now_ms() - ctx.started_at).max(0) as u64);
                metrics.last_activity_at = Some(now_ms());
                bus.push_metric(GraphOp::PatchNode {
                    id: node_id(NodeKind::Run, &ctx.run_id),
                    status: None,
                    confidence: None,
                    evidence: None,
                    badges: None,
                    metrics: Some(metrics),
                });
            }
            DispatchEventKind::Tool => {
                // The tool name lives in the sublabel: `NodeMetrics` has no
                // string field, and re-upserting the node is cheap — the
                // topology hash is over ids, so this never re-runs the layout.
                let tool = event.get("name").and_then(Value::as_str);
                let mut node = GraphNode::new(
                    node_id(NodeKind::Run, &ctx.run_id),
                    NodeKind::Run,
                    &ctx.vendor,
                    NodeStatus::Working,
                    now_ms(),
                );
                node.sublabel = Some(run_sublabel(ctx.model.as_deref(), tool));
                node.meta.vendor = Some(ctx.vendor.clone());
                node.meta.worktree_id = ctx.worktree_id.clone();
                node.meta.cwd = Some(ctx.cwd.display().to_string());
                if let Some(worktree_id) = ctx.worktree_id.as_deref() {
                    node.parent_id = Some(node_id(NodeKind::Worktree, worktree_id));
                }
                bus.push(GraphOp::UpsertNode { node });
            }
            _ => self.patch_activity(ctx),
        }
    }

    fn patch_activity(&self, ctx: &RunCtx) {
        let Some(bus) = self.bus() else { return };
        let mut metrics = NodeMetrics::default();
        metrics.elapsed_ms = Some((now_ms() - ctx.started_at).max(0) as u64);
        metrics.last_activity_at = Some(now_ms());
        bus.push_metric(GraphOp::PatchNode {
            id: node_id(NodeKind::Run, &ctx.run_id),
            status: None,
            confidence: None,
            evidence: None,
            badges: None,
            metrics: Some(metrics),
        });
    }

    fn patch_settled(&self, ctx: &RunCtx, outcome: &Outcome, finished_at: i64) {
        let Some(bus) = self.bus() else { return };
        let mut metrics = NodeMetrics::default();
        metrics.elapsed_ms = Some((finished_at - ctx.started_at).max(0) as u64);
        metrics.last_activity_at = Some(finished_at);
        if let Some(result) = outcome.result.as_ref() {
            if let Some(usage) = result.usage.as_ref() {
                metrics.input_tokens = usage.input_tokens;
                metrics.output_tokens = usage.output_tokens;
                metrics.cached_tokens = usage.cached_input_tokens;
            }
            metrics.cost_usd = cost_usd(result);
        }
        bus.push(GraphOp::PatchNode {
            id: node_id(NodeKind::Run, &ctx.run_id),
            status: Some(run_node_status(outcome.phase)),
            confidence: Some(1.0),
            evidence: None,
            badges: None,
            metrics: Some(metrics),
        });
        bus.push(GraphOp::PatchEdge {
            id: GraphEdge::edge_id(
                EdgeKind::DispatchedTo,
                &node_id(NodeKind::Dispatch, &ctx.task_id),
                &node_id(NodeKind::Run, &ctx.run_id),
            ),
            active: false,
        });
    }
}

// ---------------------------------------------------------------------------
// outcome
// ---------------------------------------------------------------------------

/// How one run ended, before it is written down.
struct Outcome {
    phase: RunPhase,
    result: Option<RunResult>,
    error: Option<String>,
    note: Option<String>,
    /// Whether the adapter's own terminal event has already gone out on
    /// `gilde://dispatch/event`. Exactly one terminal event reaches the UI per run:
    /// when the adapter produced one it is forwarded verbatim, and when it could
    /// not (a dead channel, a cancel, a wall clock) [`DispatchEngine::settle`]
    /// emits the host's verdict instead.
    emitted: bool,
}

impl Outcome {
    fn done(result: RunResult) -> Outcome {
        let phase = RunPhase::from_run_status(result.status);
        let error = (!phase.is_success()).then(|| result.text.clone()).filter(|t| !t.is_empty());
        Outcome {
            phase,
            note: result.note.clone(),
            error,
            result: Some(result),
            emitted: true,
        }
    }

    fn error(message: impl Into<String>) -> Outcome {
        Outcome {
            phase: RunPhase::Error,
            result: None,
            error: Some(message.into()),
            note: None,
            emitted: false,
        }
    }

    /// The app failed, not the agent (DESIGN §5-10 rule 2).
    fn interrupted(reason: impl Into<String>) -> Outcome {
        Outcome {
            phase: RunPhase::Interrupted,
            result: None,
            error: Some(reason.into()),
            note: None,
            emitted: false,
        }
    }

    fn cancelled() -> Outcome {
        Outcome {
            phase: RunPhase::Cancelled,
            result: None,
            error: None,
            note: None,
            emitted: false,
        }
    }

    /// The adapter's own terminal event is already on the wire.
    fn already_emitted(mut self) -> Outcome {
        self.emitted = true;
        self
    }

    /// The distinction that decides which of the two a call failure becomes.
    fn from_call_error(err: SidecarCallError) -> Outcome {
        if err.is_infrastructure() {
            Outcome::interrupted(err.to_string())
        } else {
            Outcome::error(err.to_string())
        }
    }
}

/// One step of the stream loop.
enum Step {
    Event(RunEventEnvelope),
    Lagged(u64),
    Death(String),
    Cancelled,
    Flush,
    Expired,
}

/// The 100 ms `text` merge of `gilde://dispatch/event` (DESIGN §4).
///
/// The sidecar already merges at 50 ms; this second window exists because the
/// host's consumer is a React tree, and one repaint per 100 ms is the budget.
/// Nothing here touches `stream.jsonl` — the file keeps every event verbatim.
#[derive(Default)]
struct TextMerge {
    buffer: String,
    since: Option<tokio::time::Instant>,
}

impl TextMerge {
    fn push(&mut self, chunk: &str) {
        if chunk.is_empty() {
            return;
        }
        if self.since.is_none() {
            self.since = Some(tokio::time::Instant::now());
        }
        self.buffer.push_str(chunk);
    }

    fn deadline(&self) -> Option<tokio::time::Instant> {
        self.since
            .map(|since| since + Duration::from_millis(TEXT_COALESCE_MS))
    }

    fn is_due(&self) -> bool {
        self.deadline()
            .is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
    }

    fn take(&mut self) -> Option<String> {
        self.since = None;
        if self.buffer.is_empty() {
            return None;
        }
        Some(std::mem::take(&mut self.buffer))
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Spawn onto the ambient runtime when there is one (commands, tests), else
/// onto Tauri's. Same ladder the event bus walks.
fn spawn<F>(future: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn(future);
        }
        Err(_) => {
            tauri::async_runtime::spawn(future);
        }
    }
}

fn write_artifact(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(err) = std::fs::write(path, bytes) {
        tracing::warn!(target: "dispatch", error = %err, path = %path.display(),
            "could not write a dispatch artifact");
    }
}

/// `cwd` is always absolute, normalised and real (DESIGN §5-4). The sidecar
/// rejects anything else with `-32602`; catching it here names the worktree.
fn absolute_dir(path: &Path) -> Result<PathBuf> {
    let canonical = std::fs::canonicalize(path)
        .map_err(|err| GildeError::not_found(format!("{}: {err}", path.display())))?;
    if !canonical.is_dir() {
        return Err(GildeError::not_found(format!("{} is not a directory", canonical.display())));
    }
    Ok(canonical)
}

/// Translate the task's permission mode into the vocabulary `vendor`'s CLI
/// actually accepts, or `None` to send no mode at all.
///
/// One prompt fans out to N vendors under **one** `permissionMode`
/// (`DispatchSpec.permissionMode`, defaulted from `config.dispatch.
/// permissionMode` = `"acceptEdits"`), and that word is claude's.
/// `RunTask.permissionMode` is a passthrough — agent-mesh's own routing guide
/// says a mode "that succeeds on one adapter can error on another" — so each
/// adapter appends it to a different flag with a different dictionary:
///
/// | vendor | flag | accepts |
/// |---|---|---|
/// | `claude` | `--permission-mode` | `default`, `plan`, `acceptEdits`, `bypassPermissions` |
/// | `codex` | `--sandbox` | `read-only`, `workspace-write`, `danger-full-access` |
/// | `cursor` | `--sandbox` | `enabled`, `disabled` |
/// | `gemini` | `--mode` | `accept-edits`, `plan` |
///
/// Verbatim forwarding is what made `codex` die with `error: invalid value
/// 'acceptEdits' for '--sandbox'` (exit 2) before the model was ever asked, so
/// the translation lives here — in the only layer that knows a single task
/// spans vendors. Forking the adapters is not an option: they are a published
/// library and the vendor stays the authority on its own flag.
///
/// Two rules make the table safe to extend:
///
/// * **A word already in the target's dictionary passes through**, so a user
///   who types `workspace-write` for a codex-only task gets exactly that.
/// * **An untranslatable word is dropped, not guessed.** `None` means the CLI
///   applies its own default, which is a defensible answer; inventing a
///   permission level for an unknown vendor is not. Dropping never *widens*
///   permission either — the mappings below only ever move toward the vendor's
///   more restrictive neighbour when the exact rung does not exist.
fn permission_mode_for(vendor: &str, mode: &str) -> Option<String> {
    let mode = mode.trim();
    if mode.is_empty() {
        return None;
    }
    let word = match vendor.trim() {
        // claude defines the vocabulary; anything else is claude's business.
        "claude" => mode,
        "codex" => match mode {
            "read-only" | "plan" | "default" => "read-only",
            "workspace-write" | "acceptEdits" => "workspace-write",
            "danger-full-access" | "bypassPermissions" => "danger-full-access",
            _ => return None,
        },
        // cursor's sandbox is a switch, not a ladder: `disabled` is *no*
        // sandbox, so only the explicit bypass earns it.
        "cursor" | "cursorAgent" => match mode {
            "disabled" | "bypassPermissions" => "disabled",
            "enabled" | "default" | "plan" | "acceptEdits" => "enabled",
            _ => return None,
        },
        "gemini" | "agy" => match mode {
            "plan" => "plan",
            "accept-edits" | "acceptEdits" | "bypassPermissions" => "accept-edits",
            // gemini has no "ask me" rung to map `default` onto.
            _ => return None,
        },
        // A vendor this build does not know: do not guess in its dictionary.
        _ => return None,
    };
    Some(word.to_string())
}

fn title_from_prompt(prompt: &str) -> String {
    let line = prompt.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let trimmed = line.trim();
    if trimmed.chars().count() <= TITLE_CAP {
        return trimmed.to_string();
    }
    let cut: String = trimmed.chars().take(TITLE_CAP).collect();
    format!("{cut}…")
}

/// The `type` tag of a `RunEvent`, mapped to the event kind the UI subscribes
/// to. An unmodelled kind is carried as `text` rather than dropped — the event
/// is still forwarded verbatim in `payload`.
fn event_kind(event: &Value) -> DispatchEventKind {
    match event.get("type").and_then(Value::as_str) {
        Some("text") => DispatchEventKind::Text,
        Some("tool") => DispatchEventKind::Tool,
        Some("usage") => DispatchEventKind::Usage,
        Some("done") => DispatchEventKind::Done,
        Some("error") => DispatchEventKind::Error,
        _ => DispatchEventKind::Text,
    }
}

/// The `run_event.kind` column value.
fn kind_str(kind: DispatchEventKind) -> &'static str {
    match kind {
        DispatchEventKind::Routed => "routed",
        DispatchEventKind::Spawned => "spawned",
        DispatchEventKind::Text => "text",
        DispatchEventKind::Tool => "tool",
        DispatchEventKind::Usage => "usage",
        DispatchEventKind::Done => "done",
        DispatchEventKind::Error => "error",
        DispatchEventKind::Interrupted => "interrupted",
        DispatchEventKind::Cancelled => "cancelled",
    }
}

/// `interrupted` is deliberately **not** `error`: the graph draws it as a
/// stalled node with a dashed outline, which is the shape of "the app broke,
/// pick it back up" rather than "the agent failed" (DESIGN §9-6).
fn run_node_status(phase: RunPhase) -> NodeStatus {
    match phase {
        RunPhase::Queued | RunPhase::Spawning => NodeStatus::Idle,
        RunPhase::Running => NodeStatus::Working,
        RunPhase::Done => NodeStatus::Done,
        RunPhase::Error => NodeStatus::Error,
        RunPhase::Interrupted => NodeStatus::Stalled,
        RunPhase::Stopped | RunPhase::Cancelled | RunPhase::Incomplete => NodeStatus::Exited,
    }
}

fn task_node_status(status: TaskStatus) -> NodeStatus {
    match status {
        TaskStatus::Queued => NodeStatus::Idle,
        TaskStatus::Running => NodeStatus::Working,
        TaskStatus::Done => NodeStatus::Done,
        TaskStatus::Partial => NodeStatus::Ok,
        TaskStatus::Cancelled => NodeStatus::Exited,
        TaskStatus::Error => NodeStatus::Error,
    }
}

fn run_sublabel(model: Option<&str>, tool: Option<&str>) -> String {
    match (model.filter(|m| !m.is_empty()), tool.filter(|t| !t.is_empty())) {
        (Some(model), Some(tool)) => format!("{model} · {tool}"),
        (Some(model), None) => model.to_string(),
        (None, Some(tool)) => tool.to_string(),
        (None, None) => String::new(),
    }
}

/// Fold the per-process reports of one run into the single [`KillReport`]
/// `dispatch_cancel_run` answers with. A run is one logical thing to the user
/// even when the seam spawned three children for it.
fn merge_reports(run_id: &str, reports: Vec<KillReport>) -> KillReport {
    if reports.len() == 1 {
        return reports.into_iter().next().expect("length checked");
    }
    if reports.is_empty() {
        // Nothing was registered under this run: either it never spawned, or
        // its children already exited. Saying so beats a fabricated success.
        return KillReport::refused(
            Some(run_id.to_string()),
            "gone",
            "no live process was registered for this run",
        );
    }
    let mut merged = KillReport {
        spawn_id: Some(run_id.to_string()),
        requested: Vec::new(),
        excluded: Vec::new(),
        survivors: Vec::new(),
        refused_reason: None,
        dry_run: reports.iter().all(|r| r.dry_run),
        escalated: Vec::new(),
        fence: String::new(),
        at: now_ms(),
        names: Vec::new(),
    };
    let mut fences: Vec<String> = Vec::new();
    for report in &reports {
        merged.requested.extend(report.requested.iter().copied());
        merged.names.extend(report.names.iter().cloned());
        merged.excluded.extend(report.excluded.iter().cloned());
        merged.survivors.extend(report.survivors.iter().copied());
        merged.escalated.extend(report.escalated.iter().copied());
        if !fences.contains(&report.fence) {
            fences.push(report.fence.clone());
        }
    }
    merged.fence = fences.join("+");
    if merged.requested.is_empty() {
        let reasons: Vec<&str> = reports
            .iter()
            .filter_map(|r| r.refused_reason.as_deref())
            .collect();
        merged.refused_reason = Some(if reasons.is_empty() {
            "nothing to signal".to_string()
        } else {
            reasons.join("; ")
        });
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_event_type_is_carried_rather_than_dropped() {
        assert_eq!(event_kind(&json!({"type": "tool"})), DispatchEventKind::Tool);
        assert_eq!(event_kind(&json!({"type": "done"})), DispatchEventKind::Done);
        // A kind this build does not model still reaches the UI with its
        // payload intact.
        assert_eq!(
            event_kind(&json!({"type": "thinking", "text": "…"})),
            DispatchEventKind::Text
        );
    }

    #[test]
    fn interrupted_and_error_are_different_colours() {
        assert_eq!(run_node_status(RunPhase::Error), NodeStatus::Error);
        assert_eq!(run_node_status(RunPhase::Interrupted), NodeStatus::Stalled);
        assert_ne!(
            run_node_status(RunPhase::Interrupted),
            run_node_status(RunPhase::Error)
        );
    }

    #[test]
    fn a_gone_channel_interrupts_a_run_and_an_adapter_failure_errors_it() {
        let gone = Outcome::from_call_error(SidecarCallError::Gone { generation: 3 });
        assert_eq!(gone.phase, RunPhase::Interrupted);

        let down = Outcome::from_call_error(SidecarCallError::Down);
        assert_eq!(down.phase, RunPhase::Interrupted);

        let adapter = Outcome::from_call_error(SidecarCallError::Rpc(
            crate::sidecar::RpcErrorObject {
                code: -32001,
                message: "claude exited 1".into(),
                data: None,
            },
        ));
        assert_eq!(adapter.phase, RunPhase::Error);
    }

    #[test]
    fn an_adapter_status_decides_the_phase_of_a_finished_run() {
        let result: RunResult = serde_json::from_value(json!({
            "vendor": "claude", "conversationId": "c", "text": "done", "status": "ok"
        }))
        .expect("decode");
        assert_eq!(Outcome::done(result).phase, RunPhase::Done);

        let incomplete: RunResult = serde_json::from_value(json!({
            "vendor": "codex", "conversationId": "", "text": "timed out", "status": "incomplete"
        }))
        .expect("decode");
        let outcome = Outcome::done(incomplete);
        assert_eq!(outcome.phase, RunPhase::Incomplete);
        assert_eq!(outcome.error.as_deref(), Some("timed out"));
    }

    #[test]
    fn the_text_merge_holds_for_a_hundred_milliseconds_and_then_releases() {
        let mut merge = TextMerge::default();
        assert!(merge.deadline().is_none());
        merge.push("hello ");
        merge.push("world");
        assert!(merge.deadline().is_some());
        assert_eq!(merge.take().as_deref(), Some("hello world"));
        assert!(merge.take().is_none(), "an empty window emits nothing");
        assert!(merge.deadline().is_none());
    }

    #[test]
    fn merging_kill_reports_never_fabricates_a_success() {
        let empty = merge_reports("r1", Vec::new());
        assert!(empty.is_refused());

        let one = KillReport::refused(Some("s1".into()), "verified", "already gone");
        assert_eq!(merge_reports("r1", vec![one.clone()]), one);

        let a = KillReport {
            spawn_id: Some("s1".into()),
            requested: vec![10],
            excluded: Vec::new(),
            survivors: Vec::new(),
            refused_reason: None,
            dry_run: false,
            escalated: vec![10],
            fence: "verified".into(),
            at: 1,
            names: Vec::new(),
        };
        let b = KillReport::refused(Some("s2".into()), "gone", "no such pid");
        let merged = merge_reports("r1", vec![a, b]);
        assert_eq!(merged.requested, vec![10]);
        assert_eq!(merged.escalated, vec![10]);
        assert!(!merged.is_refused(), "one real kill is not a refusal");
        assert!(merged.fence.contains("verified"));
    }

    #[test]
    fn a_title_falls_back_to_the_first_line_of_the_prompt() {
        assert_eq!(title_from_prompt("\n\nfix the resize bug\nmore"), "fix the resize bug");
        let long = "x".repeat(TITLE_CAP + 10);
        let title = title_from_prompt(&long);
        assert!(title.ends_with('…'));
        assert_eq!(title.chars().count(), TITLE_CAP + 1);
    }

    #[test]
    fn one_permission_mode_is_translated_into_each_vendors_own_vocabulary() {
        // The regression: `acceptEdits` reached codex as `--sandbox
        // acceptEdits` and clap killed the run with exit 2 before the model
        // was asked.
        assert_eq!(permission_mode_for("claude", "acceptEdits").as_deref(), Some("acceptEdits"));
        assert_eq!(permission_mode_for("codex", "acceptEdits").as_deref(), Some("workspace-write"));
        assert_eq!(permission_mode_for("cursor", "acceptEdits").as_deref(), Some("enabled"));
        assert_eq!(permission_mode_for("gemini", "acceptEdits").as_deref(), Some("accept-edits"));

        // The read-only rung and the bypass rung, per vendor.
        assert_eq!(permission_mode_for("codex", "plan").as_deref(), Some("read-only"));
        assert_eq!(permission_mode_for("gemini", "plan").as_deref(), Some("plan"));
        assert_eq!(
            permission_mode_for("codex", "bypassPermissions").as_deref(),
            Some("danger-full-access")
        );
        assert_eq!(
            permission_mode_for("cursor", "bypassPermissions").as_deref(),
            Some("disabled"),
            "only an explicit bypass turns cursor's sandbox off"
        );

        // A word already in the target's dictionary is not re-translated.
        for (vendor, word) in [
            ("codex", "workspace-write"),
            ("codex", "danger-full-access"),
            ("cursor", "enabled"),
            ("gemini", "accept-edits"),
        ] {
            assert_eq!(permission_mode_for(vendor, word).as_deref(), Some(word));
        }

        // Untranslatable is dropped, never guessed: the CLI's own default is
        // a defensible answer, a word from another vendor's dictionary is not.
        assert_eq!(permission_mode_for("codex", "somethingElse"), None);
        assert_eq!(permission_mode_for("gemini", "default"), None);
        assert_eq!(permission_mode_for("a-vendor-we-do-not-know", "acceptEdits"), None);
        assert_eq!(permission_mode_for("codex", "   "), None, "an empty mode is no mode");
    }

    #[test]
    fn a_run_sublabel_shows_the_model_and_the_tool_it_is_in() {
        assert_eq!(run_sublabel(Some("opus-4-8"), Some("Edit")), "opus-4-8 · Edit");
        assert_eq!(run_sublabel(Some("opus-4-8"), None), "opus-4-8");
        assert_eq!(run_sublabel(None, Some("Bash")), "Bash");
        assert_eq!(run_sublabel(None, None), "");
    }
}
