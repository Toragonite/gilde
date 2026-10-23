//! The quit record and the C5 stop sweep (§10.2) [M10, C5].
//!
//! Owner: T09.
//!
//! `app_quit{confirmToken}` runs, in this order:
//!
//! 1. the token check — single use, 60 s, bound to a digest of the previewed
//!    set of In-Gilde sessions; a different set is `CONFLICT{changed}`;
//! 2. **the quit record first** (`quit_record` + items, one `synchronous=FULL`
//!    transaction): every In-Gilde session with its control mode, model,
//!    permission mode, `was_waiting` and team-lead flag. A failed write (disk
//!    full, memory-only mode) still quits; it is logged and nothing is offered
//!    next time;
//! 3. **the stop sweep, bounded to [`QUIT_BOUND`] (6 s) in total**, in
//!    parallel, with `Quit{stopped,total,phase}` store ops for the sheet:
//!    every session goes through its driver's stop sequence with the quit
//!    bounds (`SessionActorApi::stop_for_quit`: Conversation = interrupt → the
//!    turn's `result` ≤ 3 s → close stdin → ≤ 1.5 s → fenced SIGTERM → ≤ 1.5 s;
//!    PTYs = SIGHUP → ≤ 1.5 s → fenced SIGTERM → ≤ 1.5 s). A session whose
//!    driver has not finished by [`DRIVER_DEADLINE`] — or that has no driver —
//!    gets the registry fallback: identity-fenced SIGTERM (SIGHUP first for
//!    PTYs and panes) with a 1.5 s grace. What is left of this instance's
//!    registry (helpers) is then swept within the remaining time;
//! 4. **never SIGKILL a vendor process** (the kill policy of
//!    `supervise::kill`). When the bound expires every surviving vendor
//!    process is recorded: `quit_record_item.survivor = 1`, and its
//!    `proc_registry` row is put back to `running` (note
//!    [`NOTE_QUIT_SURVIVOR`]) under the ended instance, so the next boot sweep
//!    finds it (identity-fenced SIGTERM after the 10 s undo window, Leftover
//!    `QuitSurvivor` if it still lives).
//!
//! The pane `sh` wrapper is never SIGKILLed while a vendor descendant lives
//! (F-32) — that, too, is the kill policy, not something this module decides.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use sha2::{Digest as _, Sha256};

use crate::contract::interaction::InteractionKind;
use crate::contract::ipc::{QuitRow, QuitTeamRow, WaitingKind};
use crate::contract::services::SessionActorApi;
use crate::contract::session::{Control, ControlMode, MemberRole, Session};
use crate::contract::store::{QuitPhase, StoreOp, StoreSnapshot};
use crate::contract::{ids::ConfirmToken, ids::QuitRecordId, ids::SessionUid, IpcError};
use crate::events::StoreOpsSink;
use crate::store::dao;
use crate::store::models::{ProcKind, ProcRow, ProcState, QuitItemRow, QuitRecordRow, CAUSE_QUIT};
use crate::store::now_ms;
use crate::supervise::kill::{kill_tree, KillOpts};
use crate::supervise::registry::Supervisor;
use crate::supervise::spawn::is_alive;

/// The whole stop phase of a quit (§10.2 step 4).
pub const QUIT_BOUND: Duration = Duration::from_secs(6);
/// How long a session's driver gets before the registry fallback takes over.
pub const DRIVER_DEADLINE: Duration = Duration::from_millis(4_500);
/// Conversation quit bounds (§10.2, §6.1.7): the interrupted turn's `result`.
pub const QUIT_INTERRUPT_RESULT: Duration = Duration::from_secs(3);
/// After closing stdin / after SIGHUP, and after the fenced SIGTERM.
pub const QUIT_STEP_GRACE: Duration = Duration::from_millis(1_500);
/// Confirm tokens live this long (§4.1 rule 4).
pub const CONFIRM_TTL: Duration = Duration::from_secs(60);
/// The registry note of a vendor process that outlived the quit bound.
pub const NOTE_QUIT_SURVIVOR: &str = "quit survivor";

static EXIT_AUTHORIZED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Let the next exit request through (a `SIGTERM`/`SIGINT` to Gilde: the exit
/// path then runs the quit of §10.2 itself).
pub fn authorize_exit() {
    EXIT_AUTHORIZED.store(true, std::sync::atomic::Ordering::SeqCst);
}

pub fn exit_authorized() -> bool {
    EXIT_AUTHORIZED.load(std::sync::atomic::Ordering::SeqCst)
}

// ---------------------------------------------------------------------------
// confirm tokens (§4.1 rule 4) — also used by `health_stop`
// ---------------------------------------------------------------------------

/// Single-use, 60 s tokens bound to a digest of the previewed state.
#[derive(Debug, Default)]
pub struct ConfirmTokens {
    issued: Mutex<HashMap<String, (String, Instant)>>,
}

impl ConfirmTokens {
    pub fn new() -> ConfirmTokens {
        ConfirmTokens::default()
    }

    /// Mint a token for `digest`.
    pub fn issue(&self, digest: String) -> ConfirmToken {
        let token = ConfirmToken::mint();
        let mut issued = self.issued.lock();
        issued.retain(|_, (_, at)| at.elapsed() < CONFIRM_TTL);
        issued.insert(token.as_str().to_string(), (digest, Instant::now()));
        token
    }

    /// Consume `token`: `CONFLICT{changed: "token"}` when it is unknown, used or
    /// expired; `CONFLICT{changed: what}` when the state's digest moved.
    pub fn redeem(&self, token: &ConfirmToken, digest_now: &str, what: &str) -> Result<(), IpcError> {
        let entry = self.issued.lock().remove(token.as_str());
        match entry {
            Some((_, at)) if at.elapsed() >= CONFIRM_TTL => Err(IpcError::conflict("token")),
            Some((digest, _)) if digest == digest_now => Ok(()),
            Some(_) => Err(IpcError::conflict(what)),
            None => Err(IpcError::conflict("token")),
        }
    }

    /// The previewed state attached to `token`, without consuming it.
    pub fn peek(&self, token: &ConfirmToken) -> Option<String> {
        self.issued
            .lock()
            .get(token.as_str())
            .filter(|(_, at)| at.elapsed() < CONFIRM_TTL)
            .map(|(d, _)| d.clone())
    }

    /// Whether `token` is outstanding and unexpired.
    pub fn is_live(&self, token: &str) -> bool {
        self.issued
            .lock()
            .get(token)
            .is_some_and(|(_, at)| at.elapsed() < CONFIRM_TTL)
    }

    /// Drop every outstanding token (`app_quit_cancel`).
    pub fn clear(&self) {
        self.issued.lock().clear();
    }
}

/// `sha256` over the sorted parts, hex.
pub fn digest_of<S: AsRef<str>>(parts: &[S]) -> String {
    let mut sorted: Vec<&str> = parts.iter().map(|s| s.as_ref()).collect();
    sorted.sort_unstable();
    let mut h = Sha256::new();
    for p in sorted {
        h.update(p.as_bytes());
        h.update([0u8]);
    }
    hex::encode(h.finalize())
}

// ---------------------------------------------------------------------------
// the preview (§4.2.1 `app_quit_preview`)
// ---------------------------------------------------------------------------

/// What the quit sheet shows, plus the items the record will hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuitPreview {
    pub sessions: Vec<QuitRow>,
    pub elsewhere: u32,
    pub teams: Vec<QuitTeamRow>,
    /// Requests that are lost with their sessions (`sheet.quit.pendingLost`):
    /// Continue never re-asks [N-01].
    pub pending_lost: u32,
    pub items: Vec<QuitItemRow>,
    pub digest: String,
}

/// `approval` for permission prompts, `answer` for questions and plans.
pub fn waiting_of(session: &Session) -> Option<WaitingKind> {
    let mut answer = false;
    for p in &session.pending {
        match p.kind {
            InteractionKind::ToolApproval | InteractionKind::ScreenPrompt => {
                return Some(WaitingKind::Approval)
            }
            InteractionKind::Question | InteractionKind::PlanApproval => answer = true,
            _ => {}
        }
    }
    answer.then_some(WaitingKind::Answer)
}

/// The `quit_record_item.control_mode` spelling.
pub const fn control_mode_str(mode: ControlMode) -> &'static str {
    match mode {
        ControlMode::Conversation => "conversation",
        ControlMode::Terminal => "terminal",
        ControlMode::TurnBased => "turnBased",
    }
}

fn is_lead(session: &Session) -> bool {
    session
        .team
        .as_ref()
        .is_some_and(|t| t.role == MemberRole::Lead)
}

/// Build the preview from a World snapshot (pure).
pub fn preview_from_snapshot(snap: &StoreSnapshot) -> QuitPreview {
    let mut sessions = Vec::new();
    let mut items = Vec::new();
    let mut elsewhere = 0u32;
    let mut pending_lost = 0u32;
    let mut lead_teams: BTreeMap<String, u32> = BTreeMap::new();
    for s in &snap.sessions {
        if s.merged_into.is_some() {
            continue;
        }
        match &s.control {
            Control::InGilde { mode, .. } => {
                let waiting = waiting_of(s);
                pending_lost += s
                    .pending
                    .iter()
                    .filter(|p| {
                        !matches!(
                            p.kind,
                            InteractionKind::SetupPrompt
                                | InteractionKind::WatchedApproval
                                | InteractionKind::WatchedQuestion
                        )
                    })
                    .count() as u32;
                let lead = is_lead(s);
                if lead {
                    if let Some(t) = &s.team {
                        lead_teams.entry(t.team_key.as_str().to_string()).or_insert(0);
                    }
                }
                sessions.push(QuitRow {
                    uid: s.uid.clone(),
                    project_id: s.project_id.clone(),
                    waiting,
                    team_lead: lead,
                });
                items.push(QuitItemRow {
                    uid: s.uid.as_str().to_string(),
                    control_mode: control_mode_str(*mode).to_string(),
                    permission_mode: s.permission_mode.map(|m| m.as_cli().to_string()),
                    model: s.model.clone(),
                    was_waiting: waiting.map(|w| match w {
                        WaitingKind::Approval => "approval".to_string(),
                        WaitingKind::Answer => "answer".to_string(),
                    }),
                    team_lead: lead,
                    survivor: false,
                });
            }
            Control::Watching { .. } => elsewhere += 1,
            Control::NotRunning { .. } => {}
        }
    }
    for m in &snap.members {
        if let Some(n) = lead_teams.get_mut(m.team_key.as_str()) {
            if m.role != MemberRole::Lead {
                *n += 1;
            }
        }
    }
    let teams = snap
        .teams
        .iter()
        .filter_map(|t| {
            lead_teams.get(t.key.as_str()).map(|n| QuitTeamRow {
                team_key: t.key.clone(),
                members: *n,
            })
        })
        .collect();
    let uids: Vec<&str> = sessions.iter().map(|r| r.uid.as_str()).collect();
    QuitPreview {
        digest: digest_of(&uids),
        sessions,
        elsewhere,
        teams,
        pending_lost,
        items,
    }
}

/// The preview from the registry alone (no World bound yet): one item per
/// distinct session id among this instance's live session rows.
pub fn preview_from_rows(rows: &[ProcRow], parents: &HashMap<String, String>) -> QuitPreview {
    let mut seen = HashSet::new();
    let mut sessions = Vec::new();
    let mut items = Vec::new();
    for row in rows.iter().filter(|r| r.state.is_live() && r.kind.is_session_kind()) {
        let Some(uid) = session_of(row, parents) else { continue };
        if !seen.insert(uid.clone()) {
            continue;
        }
        if let Ok(parsed) = SessionUid::parse(&uid) {
            sessions.push(QuitRow {
                uid: parsed,
                project_id: None,
                waiting: None,
                team_lead: false,
            });
        }
        items.push(QuitItemRow {
            uid,
            control_mode: control_mode_of_kind(row.kind).to_string(),
            permission_mode: None,
            model: None,
            was_waiting: None,
            team_lead: false,
            survivor: false,
        });
    }
    let uids: Vec<&str> = items.iter().map(|i| i.uid.as_str()).collect();
    QuitPreview {
        digest: digest_of(&uids),
        sessions,
        elsewhere: 0,
        teams: Vec::new(),
        pending_lost: 0,
        items,
    }
}

/// World's preview plus the registry's sessions World does not show yet
/// (same uid = World's row wins); the digest covers every recorded uid.
pub fn merge_previews(mut world: QuitPreview, registry: QuitPreview) -> QuitPreview {
    let known: HashSet<String> = world.items.iter().map(|i| i.uid.clone()).collect();
    for item in registry.items {
        if known.contains(&item.uid) {
            continue;
        }
        if let Ok(uid) = SessionUid::parse(&item.uid) {
            world.sessions.push(QuitRow {
                uid,
                project_id: None,
                waiting: None,
                team_lead: false,
            });
        }
        world.items.push(item);
    }
    let uids: Vec<&str> = world.sessions.iter().map(|r| r.uid.as_str()).collect();
    world.digest = digest_of(&uids);
    world
}

/// The session a registry row belongs to: `parent_uid`, else `session_id`.
pub fn session_of(row: &ProcRow, parents: &HashMap<String, String>) -> Option<String> {
    parents
        .get(&row.spawn_id)
        .cloned()
        .or_else(|| row.session_id.clone())
        .filter(|s| !s.is_empty())
}

/// The control mode a session row's kind implies (crash records, fallbacks).
pub const fn control_mode_of_kind(kind: ProcKind) -> &'static str {
    match kind {
        ProcKind::Convo => "conversation",
        ProcKind::Turn => "turnBased",
        _ => "terminal",
    }
}

// ---------------------------------------------------------------------------
// the quit itself
// ---------------------------------------------------------------------------

/// What one quit did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct QuitReport {
    /// `None` when the record could not be written (it still quit).
    pub record_id: Option<String>,
    pub total: u32,
    pub stopped: u32,
    /// Session uids whose vendor process outlived the bound.
    pub survivor_sessions: Vec<String>,
    /// Vendor pids still alive when the bound expired.
    pub survivor_pids: Vec<i32>,
    pub elapsed_ms: u64,
}

/// The quit coordinator (`app_quit_preview` / `app_quit` / `app_quit_cancel`).
pub struct QuitCoordinator {
    supervisor: Arc<Supervisor>,
    ops: Arc<dyn StoreOpsSink>,
    tokens: ConfirmTokens,
    previews: Mutex<HashMap<String, QuitPreview>>,
    in_progress: std::sync::atomic::AtomicBool,
    done: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for QuitCoordinator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuitCoordinator").finish_non_exhaustive()
    }
}

impl QuitCoordinator {
    pub fn new(supervisor: Arc<Supervisor>, ops: Arc<dyn StoreOpsSink>) -> QuitCoordinator {
        QuitCoordinator {
            supervisor,
            ops,
            tokens: ConfirmTokens::new(),
            previews: Mutex::new(HashMap::new()),
            in_progress: std::sync::atomic::AtomicBool::new(false),
            done: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Mint the token for a preview and remember the previewed items.
    pub fn issue(&self, preview: QuitPreview) -> ConfirmToken {
        let token = self.tokens.issue(preview.digest.clone());
        let mut previews = self.previews.lock();
        previews.retain(|k, _| self.tokens.is_live(k));
        previews.insert(token.as_str().to_string(), preview);
        token
    }

    /// `app_quit_cancel`: every outstanding token is void.
    pub fn cancel(&self) {
        self.tokens.clear();
        self.previews.lock().clear();
    }

    /// Whether `app_quit` completed (the exit may proceed).
    pub fn is_done(&self) -> bool {
        self.done.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Whether a quit sweep is running.
    pub fn in_progress(&self) -> bool {
        self.in_progress.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Check `token` against `current` (the preview recomputed now).
    pub fn redeem(&self, token: &ConfirmToken, current: &QuitPreview) -> Result<QuitPreview, IpcError> {
        self.tokens.redeem(token, &current.digest, "sessions")?;
        self.previews.lock().remove(token.as_str());
        Ok(current.clone())
    }

    /// Steps 2–4 of §10.2 for an already redeemed preview. Never fails: every
    /// step reports and moves on (a quit must always end in an exit).
    pub async fn run(
        &self,
        preview: QuitPreview,
        sessions: Option<Arc<dyn SessionActorApi>>,
    ) -> QuitReport {
        use std::sync::atomic::Ordering;
        self.in_progress.store(true, Ordering::SeqCst);
        let started = Instant::now();
        let deadline = started + QUIT_BOUND;
        let total = preview.items.len() as u32;
        let mut report = QuitReport {
            total,
            ..QuitReport::default()
        };

        // ---- step 3: the record, before any signal.
        self.ops.ops(vec![StoreOp::Quit { stopped: 0, total, phase: QuitPhase::Recording }]);
        report.record_id = write_record(&self.supervisor, CAUSE_QUIT, &preview.items).await;

        // ---- step 4: stop, in parallel, bounded.
        self.ops.ops(vec![StoreOp::Quit { stopped: 0, total, phase: QuitPhase::Stopping }]);
        let stopped = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let mut finished: HashSet<String> = HashSet::new();
        let mut driver_survivors: HashSet<String> = HashSet::new();
        if let Some(api) = sessions.as_ref() {
            let mut set = tokio::task::JoinSet::new();
            for item in &preview.items {
                let Ok(uid) = SessionUid::parse(&item.uid) else { continue };
                let api = api.clone();
                let ops = self.ops.clone();
                let stopped = stopped.clone();
                set.spawn(async move {
                    let out = api.stop_for_quit(&uid).await;
                    let n = stopped.fetch_add(1, Ordering::SeqCst) + 1;
                    ops.ops(vec![StoreOp::Quit { stopped: n, total, phase: QuitPhase::Stopping }]);
                    (uid.as_str().to_string(), out)
                });
            }
            let driver_deadline = tokio::time::Instant::from_std(started + DRIVER_DEADLINE);
            loop {
                match tokio::time::timeout_at(driver_deadline, set.join_next()).await {
                    Ok(Some(Ok((uid, out)))) => {
                        match out {
                            Ok(o) if o.survivors > 0 => {
                                driver_survivors.insert(uid.clone());
                            }
                            Ok(_) => {}
                            Err(e) => {
                                let (code, key, len) = e.log_fields();
                                tracing::warn!(target: "app", code, key, detail_len = len, "a session's quit stop failed; the registry fallback stops it");
                                continue;
                            }
                        }
                        finished.insert(uid);
                    }
                    Ok(Some(Err(_))) => {}
                    Ok(None) => break,
                    Err(_) => {
                        tracing::warn!(target: "app", "session drivers did not finish within the quit deadline; falling back to fenced SIGTERM");
                        set.abort_all();
                        break;
                    }
                }
            }
        }

        // ---- the registry fallback and the helper sweep, within the bound.
        let me = self.supervisor.app_instance_id().to_string();
        let (rows, parents) = live_rows_and_parents(&self.supervisor, &me).await;
        let mut fallback = tokio::task::JoinSet::new();
        for row in rows {
            let session = session_of(&row, &parents);
            if let Some(s) = session.as_ref().filter(|s| finished.contains(*s)) {
                // A driver that reported its session stopped with no survivor
                // while the row's process still runs did not stop it (no live
                // actor holds that uid): the fenced fallback does, so a quit
                // survivor (A39.1(b)) is always one a stop was sent to. A
                // hosted pane is always stopped here — SIGHUP to its group,
                // then a fenced SIGTERM — whether or not its lead's session
                // already finished (A40.3).
                let pane = row.kind == ProcKind::Pane;
                if !pane && (driver_survivors.contains(s) || !row_process_runs(&row)) {
                    continue;
                }
            }
            // `keep_alive` rows outlive the app on purpose; the sidecar is
            // stopped by its manager (`mesh.shutdown`) right after this.
            if row.keep_alive || row.kind == ProcKind::Sidecar {
                continue;
            }
            if row.state == ProcState::Planned {
                let sup = self.supervisor.clone();
                let id = row.spawn_id.clone();
                let _ = sup
                    .db()
                    .write(move |conn| {
                        dao::proc::mark_state(conn, &id, ProcState::Lost, Some("host quit"))?;
                        Ok(())
                    })
                    .await;
                continue;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let hup = matches!(row.kind, ProcKind::Pty | ProcKind::Pane | ProcKind::Attach);
            let step = QUIT_STEP_GRACE.min(if hup { remaining / 2 } else { remaining });
            let opts = KillOpts::default().grace(step).hup_first(hup);
            let sup = self.supervisor.clone();
            fallback.spawn(async move {
                let rep = kill_tree(&sup, &row.spawn_id, opts).await;
                (row.spawn_id.clone(), rep)
            });
        }
        let hard = tokio::time::Instant::from_std(deadline + Duration::from_millis(500));
        loop {
            match tokio::time::timeout_at(hard, fallback.join_next()).await {
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => {
                    fallback.abort_all();
                    break;
                }
            }
        }

        // ---- survivors (§10.2 step 4): every vendor process still alive is
        //      recorded — the row's own process (A39.1(b): one of Gilde's own
        //      vendor rows whatever its exec path classifies as) and, for a
        //      session's row, its descendants and its process group (A40.3: a
        //      pane's pid is its `/bin/sh` wrapper, and its teammate may
        //      outlive it). A row whose own process lives stays `running` for
        //      the next boot sweep.
        let (rows, parents) = all_rows_and_parents(&self.supervisor, &me).await;
        let snap = crate::supervise::procscan::ps_snapshot().ok();
        let mut survivor_sessions: HashSet<String> = driver_survivors;
        for row in rows {
            let session = session_of(&row, &parents);
            let found = survivors_of_row(&row, session.is_some(), snap.as_ref());
            if found.pids.is_empty() {
                continue;
            }
            for pid in found.pids {
                if !report.survivor_pids.contains(&pid) {
                    report.survivor_pids.push(pid);
                }
            }
            if let Some(s) = session {
                survivor_sessions.insert(s);
            }
            if !found.row_process_lives {
                continue;
            }
            let id = row.spawn_id.clone();
            let _ = self
                .supervisor
                .db()
                .write(move |conn| {
                    dao::proc::mark_state(conn, &id, ProcState::Running, Some(NOTE_QUIT_SURVIVOR))?;
                    Ok(())
                })
                .await;
        }
        if let Some(record) = report.record_id.clone() {
            let uids: Vec<String> = survivor_sessions.iter().cloned().collect();
            let _ = self
                .supervisor
                .db()
                .write_durable(move |conn| {
                    for uid in &uids {
                        dao::quit_record::mark_survivor(conn, &record, uid)?;
                    }
                    Ok(())
                })
                .await;
        }
        let mut s: Vec<String> = survivor_sessions.into_iter().collect();
        s.sort();
        report.survivor_sessions = s;
        report.stopped = total.saturating_sub(report.survivor_sessions.len() as u32);
        report.elapsed_ms = started.elapsed().as_millis() as u64;
        self.ops.ops(vec![StoreOp::Quit {
            stopped: report.stopped,
            total,
            phase: QuitPhase::Done,
        }]);
        tracing::info!(
            target: "app",
            total,
            stopped = report.stopped,
            survivors = report.survivor_pids.len(),
            elapsed_ms = report.elapsed_ms,
            "quit sweep finished"
        );
        self.in_progress.store(false, Ordering::SeqCst);
        self.done.store(true, Ordering::SeqCst);
        report
    }
}

/// Write a quit/crash record with its items in one durable transaction.
/// `None` when it could not be written (logged).
pub async fn write_record(sup: &Supervisor, cause: &str, items: &[QuitItemRow]) -> Option<String> {
    write_record_with_id(sup, QuitRecordId::mint().as_str(), cause, items).await
}

/// [`write_record`] with a pre-minted id (the crash record's id is minted at
/// boot so `app_bootstrap.launch` can name it before the sweep writes it).
pub async fn write_record_with_id(
    sup: &Supervisor,
    id: &str,
    cause: &str,
    items: &[QuitItemRow],
) -> Option<String> {
    if items.is_empty() {
        return None;
    }
    let record = QuitRecordRow {
        id: id.to_string(),
        cause: cause.to_string(),
        created_at: now_ms(),
        resolved_at: None,
    };
    let items = items.to_vec();
    let rid = record.id.clone();
    match sup
        .db()
        .write_durable(move |conn| dao::quit_record::insert(conn, &record, &items))
        .await
    {
        Ok(()) => Some(rid),
        Err(err) => {
            tracing::warn!(target: "app", error = %err, cause, "the quit record could not be written; nothing will be offered next time");
            None
        }
    }
}

/// The surviving vendor processes of one row (§10.2 step 4).
#[derive(Debug, Default)]
struct RowSurvivors {
    pids: Vec<i32>,
    /// The row's own process is one of them (the row stays `running`).
    row_process_lives: bool,
}

/// A row's own process counts when the row is live and the process is one of
/// Gilde's own vendor rows or classifies as a vendor CLI (A39.1). For a
/// session's row (A40.3), its live descendants and the members of the
/// process group it leads count when they classify as a vendor CLI — also
/// once the leader is gone, as long as its pid was not recycled (a group id
/// is not reused while members remain).
fn survivors_of_row(
    row: &ProcRow,
    owned_by_session: bool,
    snap: Option<&crate::supervise::procscan::Snapshot>,
) -> RowSurvivors {
    use crate::supervise::kill::is_gone;
    use crate::supervise::procscan::vendor_of_pid;
    let mut out = RowSurvivors::default();
    let Some(pid) = row.pid.map(|p| p as i32).filter(|p| *p > 0) else { return out };
    let live_state = matches!(row.state, ProcState::Running | ProcState::Adopted | ProcState::KillIncomplete);
    let runs = row_process_runs(row);
    let own_vendor = crate::supervise::registry::is_own_vendor_row(row.kind, row.vendor.as_deref(), owned_by_session);
    if live_state && runs && (own_vendor || vendor_of_pid(pid).is_some()) {
        out.pids.push(pid);
        out.row_process_lives = true;
    }
    let Some(snap) = snap.filter(|_| row.kind.is_session_kind()) else { return out };
    let mut others = Vec::new();
    if runs {
        others.extend(snap.descendants_of(pid));
    }
    // The leader exited (a zombie included): the group id is still the
    // row's. A live pid that is not the row's process was recycled: skip.
    if row.pgid == row.pid && (runs || is_gone(pid)) {
        others.extend(snap.pids_in_group(pid));
    }
    for other in others {
        if other == pid || out.pids.contains(&other) || is_gone(other) {
            continue;
        }
        if vendor_of_pid(other).is_some() {
            out.pids.push(other);
        }
    }
    out
}

/// Whether the process a row recorded still runs: alive, not a zombie, and
/// the kernel start time the row recorded.
fn row_process_runs(row: &ProcRow) -> bool {
    let Some(pid) = row.pid.map(|p| p as i32).filter(|p| *p > 0) else { return false };
    is_alive(pid)
        && !crate::supervise::kill::is_gone(pid)
        && crate::supervise::procscan::start_time_of(pid) == row.proc_start_ts
}

async fn live_rows_and_parents(sup: &Supervisor, me: &str) -> (Vec<ProcRow>, HashMap<String, String>) {
    let (rows, parents) = all_rows_and_parents(sup, me).await;
    (rows.into_iter().filter(|r| r.state.is_live()).collect(), parents)
}

async fn all_rows_and_parents(sup: &Supervisor, me: &str) -> (Vec<ProcRow>, HashMap<String, String>) {
    let id = me.to_string();
    sup.db()
        .read(move |conn| {
            let rows = dao::proc::rows_of_instance(conn, &id)?;
            let parents = dao::proc::parents_of_instance(conn, &id)?.into_iter().collect();
            Ok((rows, parents))
        })
        .await
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_single_use_and_bound_to_the_digest() {
        let t = ConfirmTokens::new();
        let tok = t.issue("d1".into());
        assert!(t.redeem(&tok, "d1", "sessions").is_ok());
        let err = t.redeem(&tok, "d1", "sessions").unwrap_err();
        assert_eq!(err.params["changed"], "token");
        let tok = t.issue("d1".into());
        let err = t.redeem(&tok, "d2", "sessions").unwrap_err();
        assert_eq!(err.params["changed"], "sessions");
        let tok = t.issue("d1".into());
        t.clear();
        assert!(t.redeem(&tok, "d1", "sessions").is_err());
    }

    #[test]
    fn digests_ignore_order() {
        assert_eq!(digest_of(&["b", "a"]), digest_of(&["a", "b"]));
        assert_ne!(digest_of(&["a"]), digest_of(&["a", "b"]));
    }

    #[test]
    fn control_modes_have_record_spellings() {
        assert_eq!(control_mode_str(ControlMode::TurnBased), "turnBased");
        assert_eq!(control_mode_of_kind(ProcKind::Convo), "conversation");
        assert_eq!(control_mode_of_kind(ProcKind::Pane), "terminal");
    }
}
