//! Pane processes: `/bin/sh -c` behind the token check (§7.4.5, §9.3 exemption).
//!
//! Owner: T07.
//!
//! This is the one place in Gilde that runs an arbitrary command string. It
//! does so only for a `respawn-pane` that passed every §4.6 check in
//! `server::PaneHost::handle` (peer uid, constant-time per-lead token, the
//! lead's pid, the kernel peer's parent = the lead) and the input rules of
//! `subcommands::check_command` (≤ 64 KiB, no NUL, no control characters).
//! The process is started through `PtyApi::open` (kind `Pane`, `parent_uid` =
//! the lead), which plans it through `SupervisorApi` like every spawn
//! (A12.8): `/bin/sh` is allowlisted for the `Pane` kind only.
//!
//! The command is opaque. It is never parsed into Gilde argv; the only parse
//! is the **teammate-pattern check** [F-07, N-07], which decides whether the
//! pane may ever be linked to a team Member: an absolute claude path whose
//! realpath equals the lead's recorded `BinaryRef.realpath` or its
//! `proc_pidpath`, with `--agent-id <name>@<team>`, `--team-name <team>` and
//! `--parent-session-id <this lead's session id>`, in a command with no shell
//! syntax beyond the observed `cd <dir> && env K=V … <claude> …` shape.
//! Anything else still runs (the lead asked for it) but is an **unknown
//! pane**. The command is stored only as its §9.7 redaction plus a sha-256.
//!
//! Closing (`kill-pane`, Close pane, lead exit) is SIGHUP to the pane's
//! process group, then a fenced SIGTERM after 2 s (`SignalPlan::HupThenTerm`;
//! the plan type has no SIGKILL, C5); `respawn-pane -k` on a running pane
//! uses 1 s.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::contract::activity::{GildeFacts, ProcessFacts, StopCause};
use crate::contract::common::{AbsPath, EpochMs, Vendor};
use crate::contract::error::IpcError;
use crate::contract::ids::{MemberKey, ProcessKey, PtyId, SessionUid, TeamKey};
use crate::contract::process::ProcKind;
use crate::contract::services::{
    EvidenceUpdate, IdentityHint, PtyHandle, PtyOpenSpec, SessionMetaFacts, SignalPlan, SourceFacts,
};
use crate::contract::session::ControlMode;
use crate::contract::team::{MemberBackend, Presence, TeamSource};
use crate::panehost::proto::ShimResponse;
use crate::panehost::server::{now_ms, LeadPhase, PaneCommand, PaneHost, PaneKind, PaneState, UnknownReason};
use crate::panehost::subcommands::texts;

/// `respawn-pane -k` on a running pane: SIGHUP, then SIGTERM after 1 s.
pub const RESPAWN_GRACE_MS: u32 = 1_000;
/// `kill-pane`, Close pane, lead exit: SIGHUP, then SIGTERM after 2 s.
pub const KILL_GRACE_MS: u32 = 2_000;
/// The app answers a `respawn-pane` before the shim's 10 s timeout.
pub const SPAWN_DEADLINE: Duration = Duration::from_millis(9_000);
/// The pane shell (allowlisted for `ProcKind::Pane` only, §9.3).
pub const SH: &str = "/bin/sh";
/// PTY size until the member tile reports its own (§7.4.5).
pub const DEFAULT_COLS: u16 = 120;
pub const DEFAULT_ROWS: u16 = 40;

/// What the teammate-pattern check compares against.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LeadFacts {
    /// The lead's recorded `BinaryRef.realpath`.
    pub realpath: Option<PathBuf>,
    /// The lead's `proc_pidpath` [N-07].
    pub pidpath: Option<PathBuf>,
    /// The lead's session id (the `--session-id` Gilde passed).
    pub session_id: Option<String>,
}

/// The parts of a teammate launch command (§7.4.5 observed shape).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeammateShape {
    pub program: String,
    pub agent_id: String,
    pub team_name: String,
    pub parent_session_id: String,
}

/// No shell syntax beyond words, quotes and at most one `&&`: no `;`, `|`,
/// redirections, substitutions, globs that matter, or expansions inside
/// double quotes. Unterminated quotes fail.
fn plain_shell_words(command: &str) -> bool {
    let mut single = false;
    let mut double = false;
    let mut ands = 0;
    let bytes = command.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if single {
            if c == b'\'' {
                single = false;
            }
        } else if double {
            match c {
                b'"' => double = false,
                b'$' | b'`' | b'\\' => return false,
                _ => {}
            }
        } else {
            match c {
                b'\'' => single = true,
                b'"' => double = true,
                b'&' => {
                    if bytes.get(i + 1) == Some(&b'&') {
                        ands += 1;
                        i += 1;
                    } else {
                        return false;
                    }
                }
                b';' | b'|' | b'<' | b'>' | b'`' | b'$' | b'(' | b')' | b'\\' | b'\n' => return false,
                _ => {}
            }
        }
        i += 1;
    }
    !single && !double && ands <= 1
}

fn is_assignment(word: &str) -> bool {
    let Some((key, _)) = word.split_once('=') else { return false };
    let mut chars = key.chars();
    matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// Parse the teammate launch shape: `[cd <dir> &&] [env] [K=V …] <abs program>
/// <args>` with `--agent-id <name>@<team>`, `--team-name <team>`,
/// `--parent-session-id <id>` (each exactly once, `--f v` or `--f=v`).
pub fn teammate_shape(command: &str) -> Option<TeammateShape> {
    if !plain_shell_words(command) {
        return None;
    }
    let words = shell_words::split(command).ok()?;
    let mut i = 0;
    if words.first().map(String::as_str) == Some("cd") {
        if words.get(2).map(String::as_str) != Some("&&") {
            return None;
        }
        i = 3;
    }
    if words.iter().skip(i).any(|w| w == "&&") {
        return None;
    }
    if words.get(i).map(String::as_str) == Some("env") {
        i += 1;
    }
    while words.get(i).is_some_and(|w| is_assignment(w)) {
        i += 1;
    }
    let program = words.get(i)?.clone();
    if !program.starts_with('/') {
        return None;
    }
    let args = &words[i + 1..];
    let mut agent_id = None;
    let mut team_name = None;
    let mut parent = None;
    let mut j = 0;
    while j < args.len() {
        let w = args[j].as_str();
        let (flag, inline) = match w.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_string())),
            _ => (w, None),
        };
        let slot = match flag {
            "--agent-id" => &mut agent_id,
            "--team-name" => &mut team_name,
            "--parent-session-id" => &mut parent,
            _ => {
                j += 1;
                continue;
            }
        };
        let value = match inline {
            Some(v) => v,
            None => {
                j += 1;
                args.get(j)?.clone()
            }
        };
        if slot.is_some() || value.is_empty() {
            return None;
        }
        *slot = Some(value);
        j += 1;
    }
    let (agent_id, team_name, parent_session_id) = (agent_id?, team_name?, parent?);
    let (name, team) = agent_id.rsplit_once('@')?;
    if name.is_empty() || team != team_name {
        return None;
    }
    Some(TeammateShape { program, agent_id, team_name, parent_session_id })
}

/// Executable regular file.
fn is_exec(p: &Path) -> bool {
    crate::panehost::lead_env::is_executable_file(p)
}

/// Teammate or unknown pane (§7.4.5) [F-07, N-07].
pub fn classify(command: &str, lead: &LeadFacts) -> PaneKind {
    let Some(shape) = teammate_shape(command) else {
        return PaneKind::Unknown { reason: UnknownReason::NotTeammateCommand };
    };
    let resolved = std::fs::canonicalize(&shape.program).ok().filter(|p| is_exec(p));
    let canon = |p: &Option<PathBuf>| p.as_ref().map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.clone()));
    let same_binary = match &resolved {
        Some(r) => canon(&lead.realpath).as_ref() == Some(r) || canon(&lead.pidpath).as_ref() == Some(r),
        None => false,
    };
    if !same_binary {
        return PaneKind::Unknown { reason: UnknownReason::BinaryDiffers };
    }
    if lead.session_id.as_deref() != Some(shape.parent_session_id.as_str()) {
        return PaneKind::Unknown { reason: UnknownReason::OtherParent };
    }
    PaneKind::Teammate { agent_id: shape.agent_id, team_name: shape.team_name }
}

/// The stored form of a pane command: the §9.7 redaction plus sha-256 hex.
pub fn stored_command(command: &str) -> PaneCommand {
    PaneCommand {
        redacted: crate::supervise::redact::redact_command(command),
        sha256: hex::encode(Sha256::digest(command.as_bytes())),
    }
}

/// `/bin/sh` as the canonical path the allowlist names.
fn sh_path() -> AbsPath {
    let canonical = std::fs::canonicalize(SH)
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .filter(|p| p == SH);
    AbsPath::parse(canonical.unwrap_or_else(|| SH.to_string())).expect("/bin/sh is an absolute path")
}

/// Why `resolve_member` is called (it picks the refusal's `cap`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    Send,
    Close,
}

/// A hosted pane member, resolved to its lead and pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberPane {
    pub lead: SessionUid,
    pub pane: u32,
    pub epoch: u64,
    pub member_name: String,
    /// A live lead (running or backgrounded) can still message this pane, so
    /// messages to it may be accepted and never read after a close (LP-13).
    pub unread_possible: bool,
}

/// The outcome of abandoning a spawn that missed its deadline.
enum Abandon {
    Failed,
    AlreadyRunning,
}

impl PaneHost {
    fn lead_facts(&self, lead: &SessionUid, pidpath: Option<String>) -> LeadFacts {
        let session = self.deps.world.session(lead);
        let binary = session.as_ref().and_then(|s| s.binary.clone());
        LeadFacts {
            realpath: binary.as_ref().map(|b| PathBuf::from(b.realpath.as_str())),
            pidpath: pidpath
                .map(PathBuf::from)
                .or_else(|| binary.as_ref().and_then(|b| b.pidpath.as_ref()).map(|p| PathBuf::from(p.as_str()))),
            session_id: session.and_then(|s| s.vendor_session_id),
        }
    }

    /// The pane's cwd: the lead's cwd (the command starts with `cd <cwd> &&`
    /// anyway [R3 §4.2]), else the shim's, else `HOME`.
    fn pane_cwd(&self, lead: &SessionUid, shim_cwd: &str, env: &[(String, String)]) -> AbsPath {
        if let Some(cwd) = self.deps.world.session(lead).and_then(|s| s.cwd) {
            return cwd;
        }
        if let Some(p) = std::fs::canonicalize(shim_cwd)
            .ok()
            .and_then(|p| p.to_str().map(str::to_string))
            .and_then(|p| AbsPath::parse(p).ok())
        {
            return p;
        }
        env.iter()
            .find(|(k, _)| k == "HOME")
            .and_then(|(_, v)| AbsPath::parse(v.clone()).ok())
            .unwrap_or_else(|| AbsPath::parse("/tmp").expect("absolute"))
    }

    /// The pane env: the lead's composed env minus the token, `TMUX*` and the
    /// lead's own `GILDE_SPAWN_ID` (the PTY stamps a fresh one) (§7.4.5).
    fn pane_env(&self, lead: &SessionUid, lead_set: &[(String, String)]) -> Result<Vec<(String, String)>, IpcError> {
        let base: BTreeMap<String, String> = self.deps.env.base_env(lead)?;
        Ok(crate::panehost::lead_env::pane_env(base, lead_set))
    }

    /// 15: `respawn-pane -k -t %n -- <command>`. Answers 0 only once the
    /// process exists (pid known), before the shim's 10 s timeout.
    pub(crate) async fn respawn(&self, lead: &SessionUid, target: u32, command: String, shim_cwd: &str) -> ShimResponse {
        let now = now_ms();
        let (old_pty, epoch, lead_set, pidpath) = {
            let mut leads = self.leads.lock();
            let Some(l) = leads.get_mut(lead) else {
                drop(leads);
                return self.refuse_vanished("respawn-pane");
            };
            let lead_set = l.lead_set.clone();
            let pidpath = l.pidpath.clone();
            let Some(server) = l.server.as_mut() else {
                return ShimResponse::fail(texts::cant_find_pane(&target.to_string()));
            };
            if let Err(e) = server.listed_pane(target) {
                return ShimResponse::fail(e);
            }
            let pane = server.pane_mut(target).expect("listed pane exists");
            let old = match &pane.state {
                PaneState::Running { pty, .. } => Some(pty.clone()),
                _ => None,
            };
            pane.epoch += 1;
            pane.signalled = false;
            pane.early_exit = None;
            pane.state = PaneState::Spawning { since: now };
            (old, pane.epoch, lead_set, pidpath)
        };
        let kind = classify(&command, &self.lead_facts(lead, pidpath));
        let stored = stored_command(&command);
        tracing::info!(
            target: "panehost",
            pane = target,
            teammate = matches!(kind, PaneKind::Teammate { .. }),
            command_sha256 = %stored.sha256,
            "respawn-pane"
        );

        // `-k`: the running process goes first.
        if let Some(old) = old_pty {
            let _ = self.deps.pty.close(&old, SignalPlan::HupThenTerm { grace_ms: RESPAWN_GRACE_MS }).await;
        }

        let env = match self.pane_env(lead, &lead_set) {
            Ok(env) => env,
            Err(_) => {
                self.abandon_spawn(lead, target, epoch);
                return ShimResponse::fail(texts::RESPAWN_FAILED);
            }
        };
        let spec = PtyOpenSpec {
            kind: ProcKind::Pane,
            argv0: sh_path(),
            args: vec!["-c".to_string(), command],
            cwd: self.pane_cwd(lead, shim_cwd, &env),
            env,
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
            parent_uid: Some(lead.clone()),
        };
        let sink = Arc::new(crate::panehost::exits::PaneExitSink::new(self.me.clone(), lead.clone(), target, epoch));

        // The open runs in its own task so that a result arriving after the
        // deadline is still handled (its process is closed, never adopted).
        let (tx, rx) = tokio::sync::oneshot::channel();
        let pty = self.deps.pty.clone();
        let host = self.me.clone();
        let lead_owned = lead.clone();
        tokio::spawn(async move {
            let res = pty.open(spec, sink).await;
            let accepted = match host.upgrade() {
                Some(h) => h.finish_spawn(&lead_owned, target, epoch, res, kind, stored),
                None => false,
            };
            let _ = tx.send(accepted);
        });
        match tokio::time::timeout(self.cfg.spawn_deadline, rx).await {
            Ok(Ok(true)) => ShimResponse::ok(""),
            _ => match self.abandon_spawn(lead, target, epoch) {
                Abandon::AlreadyRunning => ShimResponse::ok(""),
                Abandon::Failed => ShimResponse::fail(texts::RESPAWN_FAILED),
            },
        }
    }

    /// A spawn missed its deadline (or failed before the open): the pane
    /// becomes a failed placeholder ("Teammate did not start") and a late
    /// result of this epoch is rejected.
    fn abandon_spawn(&self, lead: &SessionUid, target: u32, epoch: u64) -> Abandon {
        let outcome = {
            let mut leads = self.leads.lock();
            match leads.get_mut(lead).and_then(|l| l.server.as_mut()).and_then(|s| s.pane_mut(target)) {
                Some(p) if p.epoch == epoch && matches!(p.state, PaneState::Running { .. }) => Abandon::AlreadyRunning,
                Some(p) if p.epoch == epoch => {
                    p.epoch += 1;
                    p.state = PaneState::Placeholder { since: now_ms(), spawn_failed: true };
                    Abandon::Failed
                }
                _ => Abandon::Failed,
            }
        };
        self.emit_table(lead);
        outcome
    }

    /// The open returned. Adopt the process if the pane still waits for this
    /// epoch; otherwise close it at once. Returns whether it was adopted.
    fn finish_spawn(
        &self,
        lead: &SessionUid,
        target: u32,
        epoch: u64,
        res: Result<PtyHandle, IpcError>,
        kind: PaneKind,
        stored: PaneCommand,
    ) -> bool {
        let now = now_ms();
        let mut late: Option<PtyId> = None;
        let mut adopted: Option<(PtyHandle, Option<crate::panehost::server::EarlyExit>)> = None;
        {
            let mut leads = self.leads.lock();
            let server = leads.get_mut(lead).and_then(|l| l.server.as_mut());
            match server {
                Some(server) => {
                    let waiting = server
                        .pane(target)
                        .is_some_and(|p| p.epoch == epoch && matches!(p.state, PaneState::Spawning { .. }));
                    match (&res, waiting) {
                        (Ok(handle), true) => {
                            let pane = server.pane_mut(target).expect("pane exists");
                            pane.kind = kind.clone();
                            pane.command = Some(stored);
                            pane.state = PaneState::Running {
                                pty: handle.pty_id.clone(),
                                pid: handle.pid,
                                spawn_id: handle.ticket.spawn_id.clone(),
                                since: now,
                            };
                            pane.last_proc = Some(crate::panehost::server::ProcRef {
                                pty: handle.pty_id.clone(),
                                pid: handle.pid,
                                spawn_id: handle.ticket.spawn_id.clone(),
                                since: now,
                            });
                            let early = pane.early_exit.take();
                            if let Some((code, signal, at)) = early {
                                crate::panehost::exits::apply_exit(server, target, code, signal, at);
                            }
                            adopted = Some((handle.clone(), early));
                        }
                        (Ok(handle), false) => late = Some(handle.pty_id.clone()),
                        (Err(_), true) => {
                            let pane = server.pane_mut(target).expect("pane exists");
                            pane.epoch += 1;
                            pane.state = PaneState::Placeholder { since: now, spawn_failed: true };
                        }
                        (Err(_), false) => {}
                    }
                }
                None => {
                    if let Ok(handle) = &res {
                        late = Some(handle.pty_id.clone());
                    }
                }
            }
        }
        if let Some(pty) = late {
            tracing::warn!(target: "panehost", pane = target, "a late pane process is closed (its respawn already answered)");
            self.close_detached(vec![(target, pty)], RESPAWN_GRACE_MS);
            return false;
        }
        let accepted = adopted.is_some();
        if let Some((handle, early)) = adopted {
            if matches!(kind, PaneKind::Teammate { .. }) {
                let account = self.deps.world.session(lead).and_then(|s| s.account_id);
                let session = self.deps.sessions.pane_spawned(&handle.ticket.spawn_id, account, &handle.pty_id);
                if session.is_some() {
                    let mut leads = self.leads.lock();
                    if let Some(p) = leads
                        .get_mut(lead)
                        .and_then(|l| l.server.as_mut())
                        .and_then(|s| s.pane_mut(target))
                        .filter(|p| p.epoch == epoch)
                    {
                        p.session = session;
                    }
                }
                self.pane_spawned_evidence(lead, &handle, now);
                if let Some((code, signal, at)) = early {
                    self.pane_exited_evidence(&handle.ticket.spawn_id, handle.pid, now, code, signal, at, false);
                }
            }
        } else if let Err(e) = &res {
            let (code, key, _) = e.log_fields();
            tracing::warn!(target: "panehost", pane = target, code, key, "pane spawn failed");
        }
        self.emit_table(lead);
        accepted
    }

    /// The member's session row at once: In Gilde · Terminal, identity by the
    /// spawn id (§7.4.5). Labels arrive from the title/colour (pane table) and
    /// the descendant claude's allowlisted flags (process scan), never from
    /// the command.
    fn pane_spawned_evidence(&self, lead: &SessionUid, handle: &PtyHandle, now: EpochMs) {
        let sink = self.deps.world.sink();
        let hint = pane_hint(&handle.ticket.spawn_id, handle.pid);
        sink.evidence(EvidenceUpdate {
            hint: hint.clone(),
            facts: SourceFacts::Gilde(GildeFacts {
                spawned_at: now,
                mode: ControlMode::Terminal,
                ready_at: None,
                stop_cause: None,
                signals_sent: Vec::new(),
                start_failure: None,
            }),
            at: now,
        });
        if let Some(start) = crate::panehost::auth::start_of(handle.pid) {
            sink.evidence(EvidenceUpdate {
                hint: hint.clone(),
                facts: SourceFacts::Process(ProcessFacts::Alive { pid: handle.pid, start_sec: start, cpu_active_at: None }),
                at: now,
            });
        }
        let cwd = self.deps.world.session(lead).and_then(|s| s.cwd);
        sink.evidence(EvidenceUpdate {
            hint,
            facts: SourceFacts::Meta(Box::new(SessionMetaFacts { cwd, started_at: Some(now), ..Default::default() })),
            at: now,
        });
    }

    /// The member's process exited (§7.4.6): `Gone{ProcessExited}` follows
    /// from World's rules; a stop Gilde caused is a stop, never a crash.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn pane_exited_evidence(
        &self,
        spawn_id: &str,
        pid: i32,
        spawned_at: EpochMs,
        code: Option<i32>,
        signal: Option<i32>,
        at: EpochMs,
        signalled: bool,
    ) {
        let sink = self.deps.world.sink();
        let hint = pane_hint(spawn_id, pid);
        if signalled {
            sink.evidence(EvidenceUpdate {
                hint: hint.clone(),
                facts: SourceFacts::Gilde(GildeFacts {
                    spawned_at,
                    mode: ControlMode::Terminal,
                    ready_at: None,
                    stop_cause: Some(StopCause::User),
                    signals_sent: vec![libc::SIGHUP, libc::SIGTERM],
                    start_failure: None,
                }),
                at,
            });
        }
        sink.evidence(EvidenceUpdate { hint, facts: SourceFacts::Process(ProcessFacts::Exited { at, code, signal }), at });
    }

    /// 16: `kill-pane -t %n`. Answers right after signalling (tmux returns
    /// immediately); a kept dead pane is removed; a pane this server already
    /// closed answers 0 again (the lead sends `kill-pane` for every pane at
    /// `/exit`, dead ones included); a pane it never had is `can't find pane`.
    pub(crate) fn kill_pane(&self, lead: &SessionUid, target: u32) -> ShimResponse {
        match self.close_pane_now(lead, target, None) {
            Ok(_) => ShimResponse::ok(""),
            Err(CloseError::Unknown) => ShimResponse::fail(texts::cant_find_pane(&target.to_string())),
            Err(CloseError::Changed) => ShimResponse::ok(""),
        }
    }

    /// Close one pane now: unlist it, mark it Killed and signal its process
    /// in the background. `expect_epoch` = the pane must not have changed
    /// since a preview.
    fn close_pane_now(&self, lead: &SessionUid, target: u32, expect_epoch: Option<u64>) -> Result<bool, CloseError> {
        let now = now_ms();
        let mut signal: Option<PtyId> = None;
        let changed = {
            let mut leads = self.leads.lock();
            let server = leads
                .get_mut(lead)
                .and_then(|l| l.server.as_mut())
                .ok_or(CloseError::Unknown)?;
            if !server.knows(target) {
                return Err(CloseError::Unknown);
            }
            let pane = server.pane_mut(target).expect("known pane");
            if expect_epoch.is_some_and(|e| e != pane.epoch) {
                return Err(CloseError::Changed);
            }
            let was_listed = pane.state.listed();
            match &pane.state {
                PaneState::Running { pty, .. } => {
                    signal = Some(pty.clone());
                    pane.signalled = true;
                    pane.state = PaneState::Killed { at: now };
                }
                PaneState::Placeholder { .. } | PaneState::Spawning { .. } => {
                    pane.state = PaneState::Killed { at: now };
                }
                PaneState::Exited { kept: true, .. } => {
                    pane.state = PaneState::Killed { at: now };
                }
                PaneState::Exited { kept: false, .. } | PaneState::Killed { .. } => {}
            }
            if was_listed {
                server.unlist(target);
            }
            was_listed
        };
        if let Some(pty) = signal {
            self.close_detached(vec![(target, pty)], KILL_GRACE_MS);
        }
        if changed {
            self.emit_table(lead);
            self.end_if_backgrounded_and_empty(lead);
        }
        Ok(changed)
    }

    /// A backgrounded lead's server lives until its last pane is gone.
    pub(crate) fn end_if_backgrounded_and_empty(&self, lead: &SessionUid) {
        let ended = {
            let mut leads = self.leads.lock();
            let empty = leads.get(lead).is_some_and(|l| {
                l.phase == LeadPhase::Backgrounded && !l.server.as_ref().is_some_and(|s| s.has_live_panes())
            });
            if empty {
                leads.remove(lead).map(|l| l.pid)
            } else {
                None
            }
        };
        if let Some(pid) = ended {
            tracing::info!(target: "panehost", "a backgrounded lead's last pane closed: server dropped");
            self.emit_ended(lead, pid);
        }
    }

    /// `lead_exited` (§7.4.7): a lead that moved to the background keeps its
    /// panes [F-19]; otherwise every pane is closed (HUP, TERM after 2 s,
    /// never KILL), the server is dropped and the token revoked. Panes that
    /// survive become Leftover `TeammateOfGoneLead` through the health scan.
    pub(crate) async fn lead_gone(&self, lead: &SessionUid) {
        let op = self.leads.lock().get(lead).map(|l| l.op.clone());
        let Some(op) = op else { return };
        let _turn = op.lock().await;
        let (pending, live) = {
            let leads = self.leads.lock();
            match leads.get(lead) {
                Some(l) => (l.phase == LeadPhase::Pending, l.server.as_ref().is_some_and(|s| s.has_live_panes())),
                None => return,
            }
        };
        if !pending && live && self.deps.fate.moved_to_background(lead).await {
            {
                let mut leads = self.leads.lock();
                if let Some(l) = leads.get_mut(lead) {
                    l.phase = LeadPhase::Backgrounded;
                }
            }
            tracing::info!(target: "panehost", "the lead moved to the background: its panes stay");
            self.emit_table(lead);
            return;
        }
        let (running, pid) = {
            let mut leads = self.leads.lock();
            let Some(l) = leads.remove(lead) else { return };
            // Each running pane with its registry row, so a survivor can be
            // recorded (A40.3).
            let running: Vec<(PtyId, String)> = l
                .server
                .as_ref()
                .map(|s| {
                    s.panes()
                        .filter_map(|p| match &p.state {
                            PaneState::Running { pty, spawn_id, .. } => Some((pty.clone(), spawn_id.clone())),
                            _ => None,
                        })
                        .collect()
                })
                .unwrap_or_default();
            (running, l.pid)
        };
        let mut tasks = Vec::new();
        for (pty, spawn_id) in running {
            let api = self.deps.pty.clone();
            tasks.push(tokio::spawn(async move {
                (spawn_id, api.close(&pty, SignalPlan::HupThenTerm { grace_ms: KILL_GRACE_MS }).await)
            }));
        }
        let mut survivors = 0usize;
        for t in tasks {
            if let Ok((spawn_id, Ok(outcome))) = t.await {
                if outcome.survivor {
                    // A40.3: recorded, not only logged — the health scan shows
                    // the pane as Leftover `TeammateOfGoneLead`, and a quit's
                    // survivor pass still finds its row (`kill_incomplete`).
                    crate::supervise::reaper::note_lead_gone_survivor(&spawn_id);
                    survivors += 1;
                }
            }
        }
        if survivors > 0 {
            tracing::warn!(target: "panehost", survivors, "pane processes survived the lead's exit; recorded as leftovers");
        }
        self.emit_ended(lead, pid);
    }

    /// Resolve a hosted pane member to its lead and pane through World's
    /// snapshot and this host's table. Unknown panes are never members.
    pub fn resolve_member(&self, team: &TeamKey, member: &MemberKey, purpose: Purpose) -> Result<MemberPane, IpcError> {
        let not_pane = || match purpose {
            Purpose::Send => IpcError::unsupported("viaLeadOnly"),
            Purpose::Close => IpcError::unsupported("notAPane"),
        };
        let snap = self.deps.world.snapshot();
        let team_row = snap.teams.iter().find(|t| &t.key == team).ok_or_else(|| IpcError::not_found("team"))?;
        let m = snap
            .members
            .iter()
            .find(|m| &m.key == member && &m.team_key == team)
            .ok_or_else(|| IpcError::not_found("member"))?;
        if team_row.source == TeamSource::Orchestrator {
            return Err(match purpose {
                Purpose::Send => IpcError::unsupported("orchestratorTeam"),
                Purpose::Close => IpcError::unsupported("notAPane"),
            });
        }
        if !team_row.hosted || m.backend != MemberBackend::Pane {
            return Err(not_pane());
        }
        let pane_id = m.pane.as_ref().ok_or_else(not_pane)?;
        let pane: u32 = pane_id
            .as_str()
            .strip_prefix('%')
            .and_then(|d| d.parse().ok())
            .ok_or_else(not_pane)?;
        if purpose == Purpose::Send && matches!(m.presence, Presence::Gone { .. }) {
            return Err(IpcError::unsupported("memberGone"));
        }
        let lead_uid = team_row
            .lead
            .as_ref()
            .and_then(|lk| snap.members.iter().find(|x| &x.key == lk))
            .and_then(|lm| lm.session_uid.clone())
            .ok_or_else(not_pane)?;
        let leads = self.leads.lock();
        let lead_entry = leads.get(&lead_uid).ok_or_else(not_pane)?;
        let unread_possible = matches!(lead_entry.phase, LeadPhase::Running | LeadPhase::Backgrounded);
        let p = lead_entry
            .server
            .as_ref()
            .and_then(|s| s.pane(pane))
            .ok_or_else(not_pane)?;
        let PaneKind::Teammate { agent_id, .. } = &p.kind else {
            return Err(not_pane());
        };
        if m.agent_id.as_ref().is_some_and(|a| a != agent_id) {
            return Err(not_pane());
        }
        let usable = match purpose {
            Purpose::Send => matches!(p.state, PaneState::Running { .. }),
            Purpose::Close => p.state.listed(),
        };
        if !usable {
            return Err(IpcError::unsupported("memberGone"));
        }
        Ok(MemberPane { lead: lead_uid, pane, epoch: p.epoch, member_name: m.name.clone(), unread_possible })
    }

    /// `team_send` to a hosted pane member: bracketed paste + CR (§4.2.6).
    pub async fn send_to_member_pane(&self, team: &TeamKey, member: &MemberKey, text: &str) -> Result<(), IpcError> {
        let mp = self.resolve_member(team, member, Purpose::Send)?;
        let pty = {
            let leads = self.leads.lock();
            match leads.get(&mp.lead).and_then(|l| l.server.as_ref()).and_then(|s| s.pane(mp.pane)).map(|p| &p.state) {
                Some(PaneState::Running { pty, .. }) => pty.clone(),
                _ => return Err(IpcError::unsupported("memberGone")),
            }
        };
        self.deps.pty.paste(&pty, text, true).await
    }

    /// Close pane (`team_member_close`): the same signals as `kill-pane`;
    /// the lead is not told (§7.4.7). `expect_epoch` from a preview →
    /// `CONFLICT{changed: "pane"}` when the pane changed since.
    pub async fn close_member(&self, team: &TeamKey, member: &MemberKey, expect_epoch: Option<u64>) -> Result<(), IpcError> {
        let mp = self.resolve_member(team, member, Purpose::Close)?;
        let op = self.leads.lock().get(&mp.lead).map(|l| l.op.clone());
        let Some(op) = op else {
            return Err(IpcError::unsupported("memberGone"));
        };
        let _turn = op.lock().await;
        match self.close_pane_now(&mp.lead, mp.pane, expect_epoch) {
            Ok(_) => Ok(()),
            Err(CloseError::Changed) => Err(IpcError::conflict("pane")),
            Err(CloseError::Unknown) => Err(IpcError::unsupported("memberGone")),
        }
    }
}

/// Why a close could not run.
enum CloseError {
    Unknown,
    Changed,
}

/// The identity hint of a pane's session row: Claude, by spawn id, plus the
/// pane process key when its start time is readable.
fn pane_hint(spawn_id: &str, pid: i32) -> IdentityHint {
    IdentityHint {
        vendor: Some(Vendor::Claude),
        gilde_spawn_id: Some(spawn_id.to_string()),
        process: crate::panehost::auth::start_of(pid)
            .filter(|s| *s >= 0 && pid > 0)
            .map(|s| ProcessKey::new(pid as u32, s as u64)),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const OBSERVED: &str = "cd /p && env CLAUDECODE=1 CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1 CLAUDE_CONFIG_DIR=/u/.claude-g CLAUDE_CODE_ATTRIBUTION_PROJECT='' {BIN} --agent-id alpha@session-72cb5f46 --agent-name alpha --team-name session-72cb5f46 --agent-color blue --parent-session-id c3c103c3-0000 --settings '{\"permissions\":{\"defaultMode\":\"default\"}}' --model haiku";

    fn observed(bin: &str) -> String {
        OBSERVED.replace("{BIN}", bin)
    }

    #[test]
    fn the_observed_command_has_the_teammate_shape() {
        let s = teammate_shape(&observed("/x/claude.exe")).expect("shape");
        assert_eq!(s.program, "/x/claude.exe");
        assert_eq!(s.agent_id, "alpha@session-72cb5f46");
        assert_eq!(s.team_name, "session-72cb5f46");
        assert_eq!(s.parent_session_id, "c3c103c3-0000");
        assert!(teammate_shape("/x/claude --agent-id=a@t --team-name=t --parent-session-id=s").is_some());
    }

    #[test]
    fn shell_syntax_beyond_the_shape_is_not_a_teammate() {
        let base = observed("/x/claude.exe");
        for bad in [
            format!("{base}; curl x"),
            format!("{base} | tee y"),
            format!("{base} > out"),
            format!("{base} && rm -rf z"),
            format!("{base} $(id)"),
            format!("{base} `id`"),
            format!("{base} \"$HOME\""),
            format!("{base} &"),
            format!("{base} 'unterminated"),
            "claude --agent-id a@t --team-name t --parent-session-id s".to_string(),
            "/x/claude --agent-id a@t --team-name other --parent-session-id s".to_string(),
            "/x/claude --agent-id a@t --agent-id b@t --team-name t --parent-session-id s".to_string(),
            "/x/claude --agent-id @t --team-name t --parent-session-id s".to_string(),
            "/x/claude --team-name t --parent-session-id s".to_string(),
            "cd /p /x/claude --agent-id a@t --team-name t --parent-session-id s".to_string(),
            "/bin/sh -i".to_string(),
        ] {
            assert!(teammate_shape(&bad).is_none(), "{bad}");
        }
    }

    fn fake_bin(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, b"#!/bin/false\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::canonicalize(p).unwrap()
    }

    /// N-07: the lead's realpath **or** its proc_pidpath; another install is
    /// an unknown pane with "teammate binary differs from the lead's".
    #[test]
    fn the_binary_must_be_the_leads() {
        let dir = tempfile::tempdir().unwrap();
        let lead_bin = fake_bin(dir.path(), "claude.exe");
        let other = fake_bin(dir.path(), "other-claude");
        let link = dir.path().join("claude");
        std::os::unix::fs::symlink(&lead_bin, &link).unwrap();
        let facts = LeadFacts { realpath: Some(lead_bin.clone()), pidpath: None, session_id: Some("c3c103c3-0000".into()) };
        assert!(matches!(classify(&observed(lead_bin.to_str().unwrap()), &facts), PaneKind::Teammate { .. }));
        assert!(
            matches!(classify(&observed(link.to_str().unwrap()), &facts), PaneKind::Teammate { .. }),
            "a launcher path resolving to the same binary"
        );
        assert_eq!(
            classify(&observed(other.to_str().unwrap()), &facts),
            PaneKind::Unknown { reason: UnknownReason::BinaryDiffers }
        );
        let by_pidpath = LeadFacts { realpath: None, pidpath: Some(lead_bin.clone()), session_id: Some("c3c103c3-0000".into()) };
        assert!(matches!(classify(&observed(lead_bin.to_str().unwrap()), &by_pidpath), PaneKind::Teammate { .. }));
        let missing = dir.path().join("nope");
        assert_eq!(
            classify(&observed(missing.to_str().unwrap()), &facts),
            PaneKind::Unknown { reason: UnknownReason::BinaryDiffers }
        );
        let other_lead = LeadFacts { session_id: Some("another".into()), ..facts.clone() };
        assert_eq!(
            classify(&observed(lead_bin.to_str().unwrap()), &other_lead),
            PaneKind::Unknown { reason: UnknownReason::OtherParent }
        );
        assert_eq!(classify("echo hi", &facts), PaneKind::Unknown { reason: UnknownReason::NotTeammateCommand });
    }

    /// F-03: the stored command keeps no secret and no prompt.
    #[test]
    fn the_stored_command_is_redacted_and_hashed() {
        let cmd = "cd /p && env AWS_SECRET_ACCESS_KEY=hunter2 ANTHROPIC_BASE_URL=https://x /x/claude --settings '{\"apiKeyHelper\":\"k\"}' --agent-id a@t sk-ant-abcdefghijklmnopqrstuvwxyz0123";
        let s = stored_command(cmd);
        for secret in ["hunter2", "https://x", "apiKeyHelper", "sk-ant-abcdefghijklmnopqrstuvwxyz0123"] {
            assert!(!s.redacted.contains(secret), "{secret} leaked into {}", s.redacted);
        }
        assert_eq!(s.sha256.len(), 64);
        assert_eq!(s.sha256, stored_command(cmd).sha256);
    }

    #[test]
    fn the_pane_shell_is_the_allowlisted_path() {
        assert_eq!(sh_path().as_str(), "/bin/sh");
        assert!(crate::supervise::spawn::is_allowed(Path::new(sh_path().as_str()), ProcKind::Pane));
    }
}
