//! Stopping sessions (contract §4.2.2 `session_stop_preview`/`session_stop`,
//! §6.1.7, §9.8, §10.2) and the routing of the per-session commands to their
//! actor (§4.2.3).
//!
//! Owner: T10.
//!
//! * **Gilde's own sessions** (`Fenced`): the driver's own stop sequence —
//!   Conversation: interrupt → that turn's `result` (≤ 5 s) → close stdin →
//!   ≤ 1.5 s → fenced SIGTERM → ≤ 2 s; Terminal: SIGHUP, then fenced SIGTERM;
//!   turn-based: cancel the turn and release the session. Survivors are
//!   reported, a vendor CLI is never SIGKILLed (C5). Quit uses the same
//!   sequences with the quit bounds (`stop_for_quit`).
//! * **Watched sessions** (`TermAfterIdentity`): an identity-fenced SIGTERM
//!   through the supervisor to the process(es) that serve the vendor id; a
//!   Claude background job (`ClaudeStop`) is stopped with `claude stop <id>`
//!   (§9.8, ux-ia §5.9). Nothing that cannot be identified is signalled
//!   (`REFUSED{identityUnverified}`).
//! * The `confirmToken` is single use, 60 s, bound to a digest of what the
//!   preview showed (how + the processes); a change → `CONFLICT{changed}`.

use std::process::Stdio;
use std::sync::Arc;

use sha2::Digest as _;

use crate::contract::activity::{ActivityState, ErrorSub, ExitInfo, StopCause};
use crate::contract::common::{Notice, Vendor};
use crate::contract::error::{ErrorCode, IpcError};
use crate::contract::ids::{ConfirmToken, ProcessKey, SessionUid};
use crate::contract::ipc::{
    AppliesFrom, InteractionAnswerReq, InteractionAnswerRes, InteractionDetailReq, InteractionDetailRes,
    ScreenPromptKeyReq, ScreenPromptKeyRes, SendPriority, SessionModelsRes, SessionStopPreviewRes, SessionStopReq,
    SessionStopRes, StopMethod, TurnRetryReq, TurnRetryRes, TurnSendReq, TurnSendRes,
};
use crate::contract::process::ProcKind;
use crate::contract::services::{SignalPlan, SpawnRequest};
use crate::contract::session::{Control, ControlMode, Origin, Session};

use super::actor::ActorHandle;
use super::manager::{LiveProcess, PendingConfirm, SessionsManager};

/// What a previewed stop will do.
enum StopTarget {
    Own(Arc<ActorHandle>),
    Term(Vec<LiveProcess>),
    ClaudeStop { vendor_session_id: String },
}

struct StopPlan {
    how: StopMethod,
    owner_text: Notice,
    digest: String,
    target: StopTarget,
}

fn digest(parts: &[String]) -> String {
    let mut h = sha2::Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update([0u8]);
    }
    hex::encode(&h.finalize()[..8])
}

/// The owner's display name for the watched-stop sheet (ux-ia #113).
fn owner_name(session: &Session) -> String {
    match &session.control {
        Control::Watching { owner: Some(o), .. } if !o.app_name.is_empty() => o.app_name.clone(),
        Control::Watching { origin: Origin::VsCode, .. } => "VS Code".to_string(),
        Control::Watching { origin: Origin::Terminal, .. } => "Terminal".to_string(),
        _ => session.vendor.as_str().to_string(),
    }
}

impl SessionsManager {
    async fn stop_plan(&self, uid: &SessionUid) -> Result<StopPlan, IpcError> {
        // Gilde's own live session first: World may not show it yet.
        if let Some(a) = self.actor(uid) {
            if a.is_live().await {
                let info = a.info();
                // Gilde's own session reads `sheet.stopSession` (A24.7, A25.3).
                return Ok(StopPlan {
                    how: StopMethod::Fenced,
                    owner_text: Notice::new("sheet.stopSession"),
                    digest: digest(&["own".into(), uid.as_str().into(), info.started_at.to_string()]),
                    target: StopTarget::Own(a),
                });
            }
        }
        let session = self.deps.world.session(uid).ok_or_else(|| IpcError::not_found("session"))?;
        let Control::Watching { origin, .. } = &session.control else {
            return Err(IpcError::unsupported("notLive"));
        };
        let owner_text = Notice::new("sheet.stopWatched").param("owner", owner_name(&session));
        let vsid = session.vendor_session_id.clone().filter(|v| !v.is_empty());
        if *origin == Origin::Background && session.vendor == Vendor::Claude {
            if let Some(id) = vsid {
                return Ok(StopPlan {
                    how: StopMethod::ClaudeStop,
                    owner_text,
                    digest: digest(&["bg".into(), uid.as_str().into(), id.clone()]),
                    target: StopTarget::ClaudeStop { vendor_session_id: id },
                });
            }
        }
        let procs = match &vsid {
            Some(id) => self.deps.liveness.find(session.vendor, id).await?,
            None => Vec::new(),
        };
        let mut parts = vec!["term".to_string(), uid.as_str().to_string()];
        parts.extend(procs.iter().map(|p| format!("{}:{}", p.pid, p.start_sec)));
        Ok(StopPlan { how: StopMethod::TermAfterIdentity, owner_text, digest: digest(&parts), target: StopTarget::Term(procs) })
    }

    /// `session_stop_preview` (§4.2.2): `UNSUPPORTED{notLive}` for a session
    /// that runs nowhere.
    pub(crate) async fn preview_stop(&self, uid: &SessionUid) -> Result<SessionStopPreviewRes, IpcError> {
        let plan = self.stop_plan(uid).await?;
        let token = ConfirmToken::mint();
        // A48.4: the TTL runs on the monotonic clock.
        let now = self.now_mono();
        {
            let mut confirms = self.confirms.lock();
            confirms.retain(|_, c| c.fresh(now));
            confirms.insert(token.as_str().to_string(), PendingConfirm { uid: uid.clone(), digest: plan.digest.clone(), at: now });
        }
        Ok(SessionStopPreviewRes { how: plan.how, owner_text: plan.owner_text, confirm_token: token })
    }

    /// `session_stop` (§4.2.2).
    pub(crate) async fn stop_confirmed(&self, req: SessionStopReq) -> Result<SessionStopRes, IpcError> {
        let now = self.now_mono();
        let confirm = self.confirms.lock().remove(req.confirm_token.as_str());
        let confirm = confirm
            .filter(|c| c.uid == req.uid && c.fresh(now))
            .ok_or_else(|| IpcError::conflict("changed"))?;
        let plan = match self.stop_plan(&req.uid).await {
            Ok(p) => p,
            Err(e) if e.code == ErrorCode::Unsupported => return Err(IpcError::conflict("changed")),
            Err(e) => return Err(e),
        };
        if plan.digest != confirm.digest {
            return Err(IpcError::conflict("changed"));
        }
        match plan.target {
            StopTarget::Own(actor) => {
                let out = actor.stop(StopCause::User).await?;
                Ok(SessionStopRes { stopped: out.stopped, survivors: out.survivors })
            }
            StopTarget::Term(procs) => self.stop_watched(&procs).await,
            StopTarget::ClaudeStop { vendor_session_id } => self.claude_stop(&req.uid, &vendor_session_id).await,
        }
    }

    /// Fenced SIGTERM (identity: start time ±1 s + argv signature, §9.8); a
    /// fence refusal is `REFUSED{identityUnverified}`; never SIGKILL.
    async fn stop_watched(&self, procs: &[LiveProcess]) -> Result<SessionStopRes, IpcError> {
        if procs.is_empty() {
            return Err(IpcError::refused("identityUnverified"));
        }
        let mut stopped = true;
        let mut survivors = 0u32;
        for p in procs {
            let (Ok(pid), Ok(start)) = (u32::try_from(p.pid), u64::try_from(p.start_sec)) else {
                return Err(IpcError::refused("identityUnverified"));
            };
            let key = ProcessKey::new(pid, start);
            let out = self
                .deps
                .supervisor
                .signal(&key, SignalPlan::Term { grace_ms: self.deps.timing.watched_term_grace_ms })
                .await
                .map_err(|e| match e.code {
                    ErrorCode::Refused | ErrorCode::NotFound => IpcError::refused("identityUnverified"),
                    _ => e,
                })?;
            stopped &= out.exited;
            survivors += u32::from(out.survivor);
        }
        Ok(SessionStopRes { stopped, survivors })
    }

    /// `claude stop <id>` for a watched background job (§9.8): a `vendorcli`
    /// row through the supervisor, the session account's env.
    async fn claude_stop(&self, uid: &SessionUid, id: &str) -> Result<SessionStopRes, IpcError> {
        self.spawn_gate().await?;
        let resolved = self.resolve_binary(Vendor::Claude).await?;
        let account = self.deps.world.session(uid).and_then(|s| s.account_id);
        let account_env = match &account {
            Some(a) => self.deps.accounts.env_for(a).await,
            None => None,
        };
        let cwd = self.deps.env.home();
        let args = vec!["stop".to_string(), id.to_string()];
        let env_keys: Vec<String> = self.deps.env.compose(Vendor::Claude, account_env.as_ref(), "").into_keys().collect();
        let ticket = self
            .deps
            .supervisor
            .plan_spawn(SpawnRequest {
                kind: ProcKind::Vendorcli,
                argv0: resolved.binary.realpath.clone(),
                args: args.clone(),
                cwd: cwd.clone(),
                env_keys,
                parent_uid: Some(uid.clone()),
                generation: None,
                turn_id: None,
            })
            .await?;
        let env = self.deps.env.compose(Vendor::Claude, account_env.as_ref(), &ticket.env_marker);
        let mut cmd = crate::supervise::spawn::tokio_command_grouped(std::path::Path::new(resolved.binary.realpath.as_str()));
        cmd.args(&args).env_clear().envs(&env).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(false);
        if let Some(dir) = &cwd {
            cmd.current_dir(dir.as_str());
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(_) => {
                self.deps.supervisor.mark_exited(&ticket, ExitInfo { code: None, signal: None }).await;
                return Err(IpcError::spawn_failed());
            }
        };
        let pid = child.id().map(|p| p as i32).unwrap_or(-1);
        let _ = self.deps.supervisor.mark_running(&ticket, pid).await;
        match tokio::time::timeout(self.deps.timing.claude_stop_wait, child.wait()).await {
            Ok(Ok(st)) => {
                let exit = ExitInfo { code: st.code(), signal: crate::supervise::spawn::exit_signal_of(&st) };
                self.deps.supervisor.mark_exited(&ticket, exit).await;
                Ok(SessionStopRes { stopped: st.success(), survivors: 0 })
            }
            _ => {
                // The helper hangs: a fenced SIGTERM, never SIGKILL (C5).
                let start = crate::supervise::procscan::start_time_of(pid).and_then(|s| u64::try_from(s).ok());
                let mut survivors = 1;
                if let (Ok(p), Some(s)) = (u32::try_from(pid), start) {
                    if let Ok(out) = self.deps.supervisor.signal(&ProcessKey::new(p, s), SignalPlan::Term { grace_ms: 2_000 }).await {
                        survivors = u32::from(out.survivor);
                    }
                }
                self.deps.supervisor.mark_exited(&ticket, ExitInfo { code: None, signal: Some(libc::SIGTERM) }).await;
                Ok(SessionStopRes { stopped: false, survivors })
            }
        }
    }

    // -----------------------------------------------------------------------
    // the per-session commands (§4.2.3), routed to the session's actor
    // -----------------------------------------------------------------------

    /// `turn_send`: routes by the session's transport; `UNSUPPORTED{send}`
    /// for a session Gilde does not run (Watching / Not running).
    pub(crate) async fn turn_send(&self, req: TurnSendReq) -> Result<TurnSendRes, IpcError> {
        let actor = self.live_actor(&req.uid, "send").await?;
        actor.send(req.client_msg_id, req.text, req.priority, false).await
    }

    /// `turn_retry`: the Error·Agent error primary action resends the last
    /// user text (memory only); `REFUSED{notInError}` otherwise.
    pub(crate) async fn retry_turn(&self, req: TurnRetryReq) -> Result<TurnRetryRes, IpcError> {
        let actor = match self.actor(&req.uid) {
            Some(a) if a.is_live().await => a,
            _ => {
                return match self.deps.world.session(&req.uid) {
                    None => Err(IpcError::not_found("session")),
                    Some(_) => Err(IpcError::refused("notInError")),
                }
            }
        };
        let in_error = self
            .deps
            .world
            .session(&req.uid)
            .is_some_and(|s| matches!(s.activity.state, ActivityState::Error { sub: ErrorSub::AgentError }));
        if !in_error {
            return Err(IpcError::refused("notInError"));
        }
        let text = actor.last_text().await.ok_or_else(|| IpcError::refused("notInError"))?;
        let res = actor.send(req.client_msg_id, text, SendPriority::Next, true).await?;
        Ok(TurnRetryRes { delivery: res.delivery })
    }

    /// `session_set_model`: validated against `session_models` [F-23].
    pub(crate) async fn set_session_model(&self, uid: &SessionUid, model: Option<String>) -> Result<AppliesFrom, IpcError> {
        let actor = self.live_actor(uid, "setModel").await?;
        let model = model.filter(|m| !m.is_empty());
        if let Some(m) = &model {
            let list = actor.models().await?;
            if !list.models.is_empty() && !list.models.iter().any(|o| &o.id == m) {
                return Err(IpcError::invalid_argument("model", "range"));
            }
        }
        actor.set_model(model).await
    }

    /// `interaction_answer` [C12]: `REFUSED{watching}` for a session Gilde
    /// only watches, `REFUSED{defaultToNo}` in the actor (A19.6), the rest of
    /// the outcome semantics in the driver's request ledger.
    pub(crate) async fn answer_interaction(&self, req: InteractionAnswerReq) -> Result<InteractionAnswerRes, IpcError> {
        let actor = match self.actor(&req.uid) {
            Some(a) => a,
            None => {
                return match self.deps.world.session(&req.uid) {
                    None => Err(IpcError::not_found("session")),
                    Some(s) if matches!(s.control, Control::Watching { .. }) => Err(IpcError::refused("watching")),
                    Some(_) => Err(IpcError::not_found("request")),
                }
            }
        };
        let (outcome, previous) = actor.answer(req.request_id, req.decision, req.via).await?;
        Ok(InteractionAnswerRes { outcome, previous })
    }

    /// `screen_prompt_key` (§6.2.4, §4.1 rule 4 as amended by A34.2/A38.6):
    /// an identical call — `requestId` + `promptRev` + **the action** (texts
    /// as sha256) — that arrives while the first still runs, within 2 s,
    /// gets the first outcome and sends nothing. A finished answer is
    /// remembered by the PTY for the same appearance of the prompt only
    /// (`pty::manager::TypedAnswers`); a different action is executed (or
    /// refused by the TOCTOU guard) on its own.
    pub(crate) async fn screen_prompt_key(&self, req: ScreenPromptKeyReq) -> Result<ScreenPromptKeyRes, IpcError> {
        let actor = match self.actor(&req.uid) {
            Some(a) if a.info().mode == ControlMode::Terminal => a,
            Some(_) => return Err(IpcError::unsupported("key")),
            None => {
                return match self.deps.world.session(&req.uid) {
                    None => Err(IpcError::not_found("session")),
                    Some(_) => Err(IpcError::unsupported("key")),
                }
            }
        };
        let key = format!(
            "{}|{}|{}|{}",
            req.uid.as_str(),
            req.request_id.as_str(),
            req.prompt_rev,
            crate::pty::manager::screen_action_key(&req.action)
        );
        let now = self.now_mono();
        let outcome = self
            .screens
            .run(key, now, || actor.screen_key(req.request_id.clone(), req.prompt_rev.clone(), req.action.clone()))
            .await?;
        Ok(ScreenPromptKeyRes { outcome })
    }

    /// `interaction_detail`: the full tool input on demand (≤ 64 KiB).
    pub(crate) async fn interaction_detail(&self, req: &InteractionDetailReq) -> Result<InteractionDetailRes, IpcError> {
        match self.actor(&req.uid) {
            Some(a) => a.detail(req.request_id.clone()).await,
            None => match self.deps.world.session(&req.uid) {
                None => Err(IpcError::not_found("session")),
                Some(_) => Err(IpcError::not_found("request")),
            },
        }
    }

    /// `session_models` [F-23]: Conversation from `initialize`/`system/init`,
    /// turn-based from the helper; `UNSUPPORTED{setModel}` for Terminal.
    pub(crate) async fn session_models(&self, uid: &SessionUid) -> Result<SessionModelsRes, IpcError> {
        let actor = self.live_actor(uid, "setModel").await?;
        actor.models().await
    }
}
