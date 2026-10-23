//! `derive(EvidenceSet, now) -> Activity` (§3.5.4) and staleness (§3.5.3).
//!
//! Owner: T02. Pure functions: no clock, no I/O, no locks. World builds the
//! [`EvidenceSet`] and the [`DeriveCtx`] (control, team position and the
//! uptime stamps of every fact that feeds a duration, §3.5.2) and publishes
//! the result.
//!
//! Rule order is the evaluation order of the §3.5.4 table: R0, R1, R4, R2,
//! R3, then R11 (no applicable source), R5 … R10. R4 runs before R2/R3 while a
//! Gilde spawn is not ready (F-14), so a start failure is never filed as
//! Crashed or Ended. Conflicts between sources are resolved by rank only
//! (§3.5.1): for the working/idle decision the first source of the
//! authoritative set *P* that has an opinion decides; waits (R5/R6) and
//! errors (R7) are an "or" over *P*.
//!
//! Readings of the table, pinned here so the tests can name them:
//! * S3 has a **working** opinion while a turn is open (a turn-opening row
//!   newer than the last turn end) and either a row arrived within 30 s
//!   (uptime) or a `tool_use` is still open; a turn that ended is an **idle**
//!   opinion; otherwise S3 has no opinion (R10 "or R9 not met").
//! * The `isActive` quiet rule ("no new row for 30 s while its pane PTY (if
//!   hosted) is Idle") fires only for hosted members, whose PTY Gilde reads;
//!   for external pane members the interrupt-marker part of the rule (the
//!   member transcript's newest turn marker is newer than `config.json`'s
//!   mtime) is the one that applies.
//! * A process that exited before ready with a signal Gilde did not send is
//!   "code ≠ 0" (Didn't start · Exited); `Gone` before ready is the same.

use std::collections::BTreeSet;

use crate::contract::activity::{
    DidntStartReason, ErrorSub, EvidenceLine, EvidenceSet, ExitInfo, ProcessFacts, PtyStatus,
    RegStatus, StartFailure, TurnOutcome, TurnPhase, UnknownWhy,
};
use crate::contract::activity::{ActivityState, PrimaryAction};
use crate::contract::common::{CompatState, Confidence, EpochMs, PermissionMode, Vendor};
use crate::contract::interaction::InteractionKind;
use crate::contract::session::{
    ApproveCap, ChangesCap, ContinueOption, ControlMode, Origin, SendCap, SessionCaps, StopCap,
};
use crate::contract::team::IdleReason;

/// R4: the Starting timeout (uptime; paused while a prompt is on screen).
pub const STARTING_TIMEOUT_MS: u64 = 30_000;
/// R8: "S3 rows within 30 s".
pub const ROWS_FRESH_MS: u64 = 30_000;
/// §3.5.3: `status_updated_at < transcript.last_row_at − 2 s`.
pub const REGISTRY_ROW_SLACK_MS: EpochMs = 2_000;
/// §3.5.3: registry `busy` while the newest row is older than 10 min.
pub const BUSY_STALE_ROWS_MS: u64 = 600_000;
/// §3.5.3: the `isActive` quiet rule.
pub const IS_ACTIVE_QUIET_MS: u64 = 30_000;
/// §3.5.3: a `permission_request` hint is dropped after 10 min.
pub const PERMISSION_REQUEST_TTL_MS: u64 = 600_000;
/// §3.4.1: PTY engine confidence below this is `Likely`.
pub const PTY_LIKELY_BELOW: f32 = 0.6;
/// The default and range of the No-progress setting (§3.5.2).
pub const NO_PROGRESS_DEFAULT_SECS: u32 = 300;
pub const NO_PROGRESS_MIN_SECS: u32 = 60;
pub const NO_PROGRESS_MAX_SECS: u32 = 3_600;

/// The evidence sources of §3.5.1 (plus Gilde's own facts and the marks).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Src {
    /// Structured stream / agent-mesh run events.
    S1,
    /// Claude registry.
    S2,
    /// Transcript rows.
    S3,
    /// Team files.
    S4,
    /// PTY status engine.
    S5,
    /// Process liveness.
    S6,
    /// `GildeFacts` (spawn, stop cause, start failure).
    Gilde,
    /// A wait mirrored from the lead's card (hosted pane member, §7.2.6).
    Delegated,
    /// No source had an opinion (R10 fallback, R11).
    None,
}

/// The control kind as World computed it before deriving (§3.4.4 `control`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ControlKind {
    InGilde(ControlMode),
    Watching,
    NotRunning,
}

/// Where a session sits in a team (drives *P* and the staleness rows).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TeamPosition {
    #[default]
    Solo,
    /// A lead whose team has ≥ 1 present pane member (R6 §5 LP-20).
    LeadWithLivePanes,
    /// Any other lead (in-process members only, or none live).
    Lead,
    /// A pane member whose pane Gilde hosts (§7.4).
    HostedTeammate,
    /// Any other member with a session row (external pane member).
    Teammate,
}

/// A wait the lead shows on a member's behalf (§7.2.6, §8.6): the member is
/// Needs approval / Needs answer with `answer_in` = the lead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegatedWait {
    pub question: bool,
    pub since: EpochMs,
}

/// Uptime stamps World records when a fact arrives or advances (§3.5.2: every
/// fact that feeds a duration carries one). All `*_up` values are uptime ms.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stamps {
    /// When Gilde planned the current process (first `GildeFacts` of a spawn).
    pub spawn_up: Option<u64>,
    /// Starting-timer time already spent with a prompt on screen.
    pub starting_paused_ms: u64,
    /// A prompt has been on screen since (while not ready).
    pub prompt_since_up: Option<u64>,
    /// S1: the last frame.
    pub frame_up: Option<u64>,
    /// S3: `last_row_at` last advanced.
    pub row_up: Option<u64>,
    /// S2: the registry's `statusUpdatedAt` as uptime, stamped when that
    /// value arrived (A33.5(b): the 600 s busy rule without a transcript).
    pub status_up: Option<u64>,
    /// S5: the last output.
    pub output_up: Option<u64>,
    /// S6: CPU-active ≥ 1 % of a core.
    pub cpu_up: Option<u64>,
    /// S4: the `permission_request` hint arrived.
    pub permission_request_up: Option<u64>,
    /// Team `config.json` mtime (wall), the reference of the isActive rule.
    pub config_mtime: Option<EpochMs>,
    /// A ready signal was seen for the current process (latched).
    pub ready_latched: bool,
}

/// What `derive` needs besides the evidence.
#[derive(Debug, Clone, PartialEq)]
pub struct DeriveCtx {
    pub vendor: Vendor,
    pub control: ControlKind,
    pub position: TeamPosition,
    /// A turn-based session Gilde holds between turns (§6.3, R0).
    pub held_turn_based: bool,
    pub delegated: Option<DelegatedWait>,
    pub stamps: Stamps,
    pub now_up: u64,
    pub now_wall: EpochMs,
}

/// The result of one derivation.
#[derive(Debug, Clone, PartialEq)]
pub struct Derived {
    pub state: ActivityState,
    /// §3.5.8 "since" for waits (earliest received_at / status_updated_at /
    /// prompt first seen); `None` = the episode start.
    pub since_hint: Option<EpochMs>,
    pub confidence: Confidence,
    pub evidence: Option<EvidenceLine>,
    /// The source whose fact decided (for building `pending` and the S5
    /// debounce).
    pub decided_by: Src,
    /// Another source of *P* supports the same state (an S5 change that is
    /// corroborated is not debounced).
    pub corroborated: bool,
    /// The rule that matched ("R0" … "R11").
    pub rule: &'static str,
    /// The earliest uptime at which the outcome may change by time alone.
    pub recheck_up: Option<u64>,
}

impl Derived {
    fn new(state: ActivityState, decided_by: Src, rule: &'static str) -> Derived {
        Derived {
            state,
            since_hint: None,
            confidence: Confidence::High,
            evidence: None,
            decided_by,
            corroborated: false,
            rule,
            recheck_up: None,
        }
    }

    fn corroborated(mut self, yes: bool) -> Derived {
        self.corroborated = yes;
        self
    }

    fn since(mut self, at: Option<EpochMs>) -> Derived {
        self.since_hint = at;
        self
    }

    fn recheck(mut self, at: Option<u64>) -> Derived {
        self.recheck_up = min_opt(self.recheck_up, at);
        self
    }
}

pub(crate) fn min_opt(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, None) => x,
        (None, y) => y,
    }
}

fn is_alive(p: &ProcessFacts) -> bool {
    matches!(p, ProcessFacts::Alive { .. })
}

/// Clamp the No-progress setting to its range (§3.5.2).
pub fn clamp_no_progress(secs: u32) -> u32 {
    secs.clamp(NO_PROGRESS_MIN_SECS, NO_PROGRESS_MAX_SECS)
}

// ---------------------------------------------------------------------------
// staleness (§3.5.3)
// ---------------------------------------------------------------------------

/// The facts that survive the staleness table (§3.5.3).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Effective {
    /// Registry `status` as activity evidence (`None`: stale or ignored).
    pub registry_status: Option<RegStatus>,
    /// Registry `waiting` usable for R5/R6 (also for a lead with live panes).
    pub registry_waiting: bool,
    /// The registry status was dropped by a staleness row.
    pub registry_stale: bool,
    /// Registry `busy` (fresh) with the newest row older than 10 min, or
    /// with `statusUpdatedAt` older than 10 min and no newer row (A33.5(b)).
    pub busy_stale_rows: bool,
    /// `isActive` (`None`: absent or ignored).
    pub is_active: Option<bool>,
    /// The `permission_request` hint (`None`: absent or cleared).
    pub permission_request: Option<EpochMs>,
}

/// The newest member-transcript turn marker: a turn end (incl. the
/// interrupt marker, a `TurnEnd{Interrupted}`) or a turn-opening row.
fn newest_turn_marker(e: &EvidenceSet) -> Option<EpochMs> {
    let t = e.transcript.as_ref()?;
    match (t.last_turn_end_at, t.last_human_prompt_at) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

/// Apply the staleness rows of §3.5.3.
pub fn staleness(e: &EvidenceSet, cx: &DeriveCtx) -> Effective {
    let alive = is_alive(&e.process);
    let mut out = Effective::default();
    if let Some(r) = &e.registry {
        let last_row = e.transcript.as_ref().and_then(|t| t.last_row_at);
        let behind_rows = match (r.status_updated_at, last_row) {
            (Some(s), Some(row)) => s < row - REGISTRY_ROW_SLACK_MS,
            _ => false,
        };
        let stale = behind_rows || !r.peer_protocol_ok || !alive;
        out.registry_stale = stale && r.status.is_some();
        if !stale {
            match r.status {
                Some(RegStatus::Waiting) => out.registry_waiting = true,
                Some(_) if cx.position == TeamPosition::LeadWithLivePanes => {
                    // Stays busy until exit while pane teammates live (LP-20):
                    // only `waiting` and liveness are used; the lead's state
                    // comes from its PTY engine and transcript.
                }
                Some(s) => out.registry_status = Some(s),
                None => {}
            }
            if out.registry_status == Some(RegStatus::Busy) {
                let rows_old = cx
                    .stamps
                    .row_up
                    .is_some_and(|r| cx.now_up.saturating_sub(r) > BUSY_STALE_ROWS_MS);
                // A33.5(b), PM-01: a `busy` older than 600 s with no newer
                // transcript row — also with no transcript at all — is not
                // believed as Working: R8's No progress applies.
                let newer_row = matches!((last_row, r.status_updated_at), (Some(row), Some(st)) if row > st);
                let status_old = !newer_row
                    && cx
                        .stamps
                        .status_up
                        .is_some_and(|st| cx.now_up.saturating_sub(st) > BUSY_STALE_ROWS_MS);
                out.busy_stale_rows = rows_old || status_old;
            }
        }
    }
    if let Some(t) = &e.team {
        // Ignored when the process is not alive (it freezes when a pane dies).
        let mut active = if alive { t.is_active } else { None };
        if active == Some(true) {
            let marker_newer = match (newest_turn_marker(e), cx.stamps.config_mtime) {
                (Some(m), Some(mtime)) => m > mtime,
                _ => false,
            };
            let quiet = cx.position == TeamPosition::HostedTeammate
                && e.pty.as_ref().is_some_and(|p| p.status == PtyStatus::Idle)
                && cx
                    .stamps
                    .row_up
                    .is_none_or(|r| cx.now_up.saturating_sub(r) >= IS_ACTIVE_QUIET_MS);
            if marker_newer || quiet {
                active = None;
            }
        }
        out.is_active = active;
        if let Some((at, _tool)) = &t.permission_request {
            let advanced = e
                .transcript
                .as_ref()
                .and_then(|tr| tr.last_row_at)
                .is_some_and(|row| row > *at);
            let expired = cx
                .stamps
                .permission_request_up
                .is_some_and(|u| cx.now_up.saturating_sub(u) >= PERMISSION_REQUEST_TTL_MS);
            if !advanced && !expired && alive {
                out.permission_request = Some(*at);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// the authoritative set P (§3.5.4 preamble)
// ---------------------------------------------------------------------------

/// *P* for the session's control, in rank order.
pub fn authority(e: &EvidenceSet, cx: &DeriveCtx) -> Vec<Src> {
    match cx.control {
        ControlKind::InGilde(ControlMode::Conversation) | ControlKind::InGilde(ControlMode::TurnBased) => {
            vec![Src::S1]
        }
        ControlKind::InGilde(ControlMode::Terminal) => match (cx.vendor, cx.position) {
            (_, TeamPosition::HostedTeammate) => vec![Src::S3, Src::S5, Src::S4],
            (Vendor::Claude, TeamPosition::LeadWithLivePanes) => vec![Src::S5, Src::S3, Src::S2],
            (Vendor::Claude, _) => vec![Src::S2, Src::S3, Src::S5],
            (_, _) => vec![Src::S3, Src::S5],
        },
        ControlKind::Watching => match (cx.vendor, cx.position) {
            (Vendor::Claude, TeamPosition::HostedTeammate) => vec![Src::S3, Src::S5, Src::S4],
            (Vendor::Claude, _) => vec![Src::S2, Src::S3, Src::S4],
            (Vendor::Codex, _) if e.transcript.is_some() => vec![Src::S3],
            _ => Vec::new(),
        },
        ControlKind::NotRunning => Vec::new(),
    }
}

fn present(e: &EvidenceSet, s: Src) -> bool {
    match s {
        Src::S1 => e.stream.is_some(),
        Src::S2 => e.registry.is_some(),
        Src::S3 => e.transcript.is_some(),
        Src::S4 => e.team.is_some(),
        Src::S5 => e.pty.is_some(),
        _ => false,
    }
}

/// R11's `why` for a live session with no applicable source.
fn unknown_why(e: &EvidenceSet, cx: &DeriveCtx) -> UnknownWhy {
    match cx.vendor {
        Vendor::Cursor | Vendor::Agy => UnknownWhy::ProcessOnly,
        Vendor::Codex => {
            if matches!(cx.control, ControlKind::Watching) {
                UnknownWhy::NoThreadMap
            } else {
                UnknownWhy::ProcessOnly
            }
        }
        Vendor::Claude => match &e.registry {
            Some(r) if !r.peer_protocol_ok => UnknownWhy::UnknownPeerProtocol,
            Some(_) => UnknownWhy::ProcessOnly,
            None if matches!(cx.control, ControlKind::Watching) => UnknownWhy::Unregistered,
            None => UnknownWhy::ProcessOnly,
        },
    }
}

// ---------------------------------------------------------------------------
// ready (§3.5.6 Starting row)
// ---------------------------------------------------------------------------

/// A ready signal for a Gilde spawn other than `ready_at` (§3.5.6): registry
/// idle or busy for the pid, PTY Idle at confidence ≥ 0.6, or a running turn
/// on the stream. World latches it per process ([`Stamps::ready_latched`]).
pub fn ready_signal(e: &EvidenceSet) -> bool {
    if !is_alive(&e.process) {
        return false;
    }
    let registry = e.registry.as_ref().is_some_and(|r| {
        r.peer_protocol_ok && matches!(r.status, Some(RegStatus::Idle) | Some(RegStatus::Busy))
    });
    let pty = e
        .pty
        .as_ref()
        .is_some_and(|p| p.status == PtyStatus::Idle && p.confidence >= PTY_LIKELY_BELOW && p.prompt.is_none());
    let stream = e.stream.as_ref().is_some_and(|s| s.phase == TurnPhase::Running);
    registry || pty || stream
}

fn is_ready(e: &EvidenceSet, cx: &DeriveCtx) -> bool {
    e.gilde.as_ref().is_some_and(|g| g.ready_at.is_some()) || cx.stamps.ready_latched
}

// ---------------------------------------------------------------------------
// derive (§3.5.4)
// ---------------------------------------------------------------------------

/// The last turn's actionable error that outlives its process (R2 exception,
/// LP-09b): Sign-in needed or Usage limit, not dismissed.
fn actionable_exit_error(e: &EvidenceSet) -> Option<(ErrorSub, EpochMs)> {
    let dismissed = |at: EpochMs| e.marks.error_dismissed_at.is_some_and(|d| d >= at);
    let actionable = |s: &ErrorSub| matches!(s, ErrorSub::SignInNeeded | ErrorSub::UsageLimit { .. });
    if let Some(end) = e.stream.as_ref().and_then(|s| s.last_turn_end.as_ref()) {
        if let TurnOutcome::Error(sub) = &end.outcome {
            if actionable(sub) && !dismissed(end.at) {
                return Some((sub.clone(), end.at));
            }
        }
    }
    if let Some(t) = &e.transcript {
        if let Some((at, sub)) = &t.last_api_error {
            let after_prompt = t.last_human_prompt_at.is_none_or(|p| *at > p);
            if actionable(sub) && after_prompt && !dismissed(*at) {
                return Some((sub.clone(), *at));
            }
        }
    }
    None
}

/// Derive the activity of one session (§3.5.4). Staleness (§3.5.3) is
/// applied first.
pub fn derive(e: &EvidenceSet, cx: &DeriveCtx) -> Derived {
    let mut d = derive_rules(e, cx);
    if e.source_denied {
        d.confidence = Confidence::Likely;
        d.evidence = Some(EvidenceLine { key: "evidence.cantRead".to_string(), params: serde_json::Map::new() });
    }
    if d.decided_by == Src::S5 && e.pty.as_ref().is_some_and(|p| p.confidence < PTY_LIKELY_BELOW) {
        d.confidence = Confidence::Likely;
    }
    d
}

fn derive_rules(e: &EvidenceSet, cx: &DeriveCtx) -> Derived {
    let alive = is_alive(&e.process);
    let eff = staleness(e, cx);

    // R0: a held turn-based session; the exit of a turn's process is not a
    // session exit.
    if cx.held_turn_based {
        let running = e
            .stream
            .as_ref()
            .is_some_and(|s| matches!(s.phase, TurnPhase::Running | TurnPhase::RequiresAction));
        if running {
            return working(e, cx, &eff, Src::S1, "R0");
        }
        if let Some(d) = r7_error(e, &[Src::S1]) {
            return Derived { rule: "R0", ..d };
        }
        if let Some(d) = r9_new_reply(e, cx) {
            return Derived { rule: "R0", ..d };
        }
        return Derived::new(ActivityState::Ready, Src::S1, "R0");
    }

    // R1: exit after a Gilde stop cause.
    if !alive && e.gilde.as_ref().is_some_and(|g| g.stop_cause.is_some()) {
        return Derived::new(ActivityState::Stopped, Src::Gilde, "R1");
    }

    // R4: before ready (R1 still first) [F-14].
    if let Some(g) = &e.gilde {
        if !is_ready(e, cx) {
            return r4_not_ready(e, cx, g, alive);
        }
    }

    if !alive {
        // R2's exception: an actionable error outlives its process (R7).
        if let Some((sub, at)) = actionable_exit_error(e) {
            return Derived::new(ActivityState::Error { sub }, Src::S1, "R7").since(Some(at));
        }
        // R2: own children that exited non-zero or by a signal Gilde did not send.
        if let (Some(g), ProcessFacts::Exited { at, code, signal }) = (&e.gilde, &e.process) {
            let foreign_signal = signal.is_some_and(|s| !g.signals_sent.contains(&s));
            let nonzero = code.is_some_and(|c| c != 0);
            let dismissed = e.marks.crash_dismissed_at.is_some_and(|d| d >= *at);
            if (nonzero || foreign_signal) && !dismissed {
                let exit = ExitInfo { code: *code, signal: *signal };
                return Derived::new(ActivityState::Crashed { exit }, Src::S6, "R2");
            }
        }
        // R3: everything else that is not running.
        return Derived::new(ActivityState::Ended, Src::S6, "R3");
    }

    let p = authority(e, cx);
    // R11: alive with no applicable source.
    if cx.delegated.is_none() && !p.iter().any(|s| present(e, *s)) {
        return Derived::new(ActivityState::StatusUnknown { why: unknown_why(e, cx) }, Src::None, "R11");
    }
    if let Some(d) = r5_r6(e, cx, &eff, &p) {
        return d;
    }
    if let Some(d) = r7_error(e, &p) {
        return d;
    }
    let d = match opinion(e, cx, &eff, &p) {
        Some((Opinion::Working, src, agree)) => working(e, cx, &eff, src, "R8").corroborated(agree),
        Some((Opinion::Idle, src, agree)) => match r9_new_reply(e, cx) {
            Some(d) => d,
            None => Derived::new(ActivityState::Ready, src, "R10").corroborated(agree),
        },
        None => match r9_new_reply(e, cx) {
            Some(d) => d,
            None => Derived::new(ActivityState::Ready, Src::None, "R10"),
        },
    };
    d.recheck(opinion_recheck(e, cx, &p))
}

/// R4 for a Gilde spawn that is not ready yet.
fn r4_not_ready(e: &EvidenceSet, cx: &DeriveCtx, g: &crate::contract::activity::GildeFacts, alive: bool) -> Derived {
    if let Some(f) = g.start_failure {
        let reason = match f {
            StartFailure::CompatFailed => DidntStartReason::CompatFailed,
            StartFailure::MinVersion => DidntStartReason::MinVersion,
        };
        if !alive && e.marks.error_dismissed_at.is_some_and(|d| d >= g.spawned_at) {
            return Derived::new(ActivityState::Ended, Src::Gilde, "R4");
        }
        return Derived::new(ActivityState::Error { sub: ErrorSub::DidntStart { reason } }, Src::Gilde, "R4");
    }
    let didnt_start = ActivityState::Error { sub: ErrorSub::DidntStart { reason: DidntStartReason::Exited } };
    match e.process {
        ProcessFacts::Exited { at, code, signal } => {
            let failed = code.is_some_and(|c| c != 0) || (code.is_none() && signal.is_some());
            if !failed || e.marks.error_dismissed_at.is_some_and(|d| d >= at) {
                return Derived::new(ActivityState::Ended, Src::S6, "R4");
            }
            return Derived::new(didnt_start, Src::S6, "R4");
        }
        ProcessFacts::Gone { at } => {
            if e.marks.error_dismissed_at.is_some_and(|d| d >= at) {
                return Derived::new(ActivityState::Ended, Src::S6, "R4");
            }
            return Derived::new(didnt_start, Src::S6, "R4");
        }
        ProcessFacts::HistoryOnly => return Derived::new(ActivityState::Ended, Src::S6, "R4"),
        ProcessFacts::Alive { .. } => {}
    }
    // A screen or setup prompt on screen: R5/R6, the timer paused.
    if let Some(p) = e.pty.as_ref().and_then(|p| p.prompt.as_ref()) {
        let state = if p.permission_shaped && !p.setup {
            ActivityState::NeedsApproval
        } else {
            ActivityState::NeedsAnswer
        };
        return Derived::new(state, Src::S5, "R4").since(Some(p.first_seen_at));
    }
    let spawn_up = cx.stamps.spawn_up.unwrap_or(cx.now_up);
    let deadline = spawn_up + cx.stamps.starting_paused_ms + STARTING_TIMEOUT_MS;
    if cx.now_up > deadline {
        let timed_out_at = g.spawned_at + STARTING_TIMEOUT_MS as EpochMs;
        if e.marks.error_dismissed_at.is_some_and(|d| d >= timed_out_at) {
            return Derived::new(ActivityState::Starting, Src::Gilde, "R4");
        }
        return Derived::new(
            ActivityState::Error { sub: ErrorSub::DidntStart { reason: DidntStartReason::Timeout } },
            Src::Gilde,
            "R4",
        );
    }
    Derived::new(ActivityState::Starting, Src::Gilde, "R4").recheck(Some(deadline + 1))
}

/// R5 (pending approval) then R6 (pending question): an "or" over *P*,
/// checked in *P*'s rank order so the deciding source is the best-ranked one
/// that shows the wait (an S5-only wait is debounced, a corroborated one is
/// not).
fn r5_r6(e: &EvidenceSet, cx: &DeriveCtx, eff: &Effective, p: &[Src]) -> Option<Derived> {
    let open_question = e
        .transcript
        .as_ref()
        .and_then(|t| t.open_tool_use.as_ref())
        .is_some_and(|o| o.is_question);
    let waiting_for_input = e
        .registry
        .as_ref()
        .and_then(|r| r.waiting_for.as_deref())
        .is_some_and(|w| w.to_ascii_lowercase().contains("input"));
    let registry_since = e.registry.as_ref().and_then(|r| r.status_updated_at);
    let pending_since = |s: &crate::contract::activity::StreamFacts| s.pending.iter().map(|i| i.received_at).min();
    let prompt = e.pty.as_ref().and_then(|t| t.prompt.as_ref());
    // R5.
    let mut r5: Vec<(Src, Option<EpochMs>)> = Vec::new();
    for s in p {
        let hit = match s {
            Src::S1 => e
                .stream
                .as_ref()
                .filter(|st| st.pending.iter().any(|i| i.kind == InteractionKind::ToolApproval))
                .map(pending_since),
            Src::S5 => prompt.filter(|pr| pr.permission_shaped && !pr.setup).map(|pr| Some(pr.first_seen_at)),
            Src::S2 => (eff.registry_waiting && !waiting_for_input && !open_question).then_some(registry_since),
            Src::S4 => eff.permission_request.map(Some),
            _ => None,
        };
        if let Some(since) = hit {
            r5.push((*s, since));
        }
    }
    if let Some((s, since)) = r5.first() {
        return Some(Derived::new(ActivityState::NeedsApproval, *s, "R5").since(*since).corroborated(r5.len() > 1));
    }
    if let Some(w) = &cx.delegated {
        if !w.question {
            return Some(Derived::new(ActivityState::NeedsApproval, Src::Delegated, "R5").since(Some(w.since)));
        }
    }
    // R6.
    let mut r6: Vec<(Src, Option<EpochMs>)> = Vec::new();
    for s in p {
        let hit = match s {
            Src::S1 => e
                .stream
                .as_ref()
                .filter(|st| {
                    st.pending
                        .iter()
                        .any(|i| matches!(i.kind, InteractionKind::Question | InteractionKind::PlanApproval))
                })
                .map(pending_since),
            Src::S5 => prompt.map(|pr| Some(pr.first_seen_at)),
            Src::S2 => (eff.registry_waiting && (waiting_for_input || (open_question && p.contains(&Src::S3))))
                .then_some(registry_since),
            _ => None,
        };
        if let Some(since) = hit {
            r6.push((*s, since));
        }
    }
    if let Some((s, since)) = r6.first() {
        return Some(Derived::new(ActivityState::NeedsAnswer, *s, "R6").since(*since).corroborated(r6.len() > 1));
    }
    if let Some(w) = &cx.delegated {
        if w.question {
            return Some(Derived::new(ActivityState::NeedsAnswer, Src::Delegated, "R6").since(Some(w.since)));
        }
    }
    None
}

/// R7: an error since the last turn start in *P*, not dismissed.
fn r7_error(e: &EvidenceSet, p: &[Src]) -> Option<Derived> {
    let dismissed = |at: EpochMs| e.marks.error_dismissed_at.is_some_and(|d| d >= at);
    if p.contains(&Src::S1) {
        if let Some(s) = &e.stream {
            if let Some(end) = &s.last_turn_end {
                if let TurnOutcome::Error(sub) = &end.outcome {
                    let newer_turn = s.last_turn_start_at.is_some_and(|st| st > end.at);
                    let running = matches!(s.phase, TurnPhase::Running);
                    if !newer_turn && !running && !dismissed(end.at) {
                        return Some(
                            Derived::new(ActivityState::Error { sub: sub.clone() }, Src::S1, "R7").since(Some(end.at)),
                        );
                    }
                }
            }
        }
    }
    if p.contains(&Src::S3) {
        if let Some(t) = &e.transcript {
            if let Some((at, sub)) = &t.last_api_error {
                let after_prompt = t.last_human_prompt_at.is_none_or(|pr| *at > pr);
                if after_prompt && !dismissed(*at) {
                    return Some(Derived::new(ActivityState::Error { sub: sub.clone() }, Src::S3, "R7").since(Some(*at)));
                }
            }
        }
    }
    if p.contains(&Src::S4) {
        if let Some(t) = &e.team {
            if let Some((at, reason)) = &t.last_idle {
                let turn_start = e.transcript.as_ref().and_then(|tr| tr.last_human_prompt_at);
                let after = turn_start.is_none_or(|s| *at > s);
                if *reason == IdleReason::Failed && after && !dismissed(*at) {
                    return Some(
                        Derived::new(ActivityState::Error { sub: ErrorSub::AgentError }, Src::S4, "R7").since(Some(*at)),
                    );
                }
            }
        }
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Opinion {
    Working,
    Idle,
}

/// The S3 opinion (see the module doc for the reading).
fn s3_opinion(e: &EvidenceSet, cx: &DeriveCtx) -> Option<Opinion> {
    let t = e.transcript.as_ref()?;
    let prompt_open = match (t.last_human_prompt_at, t.last_turn_end_at) {
        (Some(p), Some(end)) => p > end,
        (Some(_), None) => true,
        (None, _) => false,
    };
    let tool_open = t
        .open_tool_use
        .as_ref()
        .is_some_and(|o| t.last_turn_end_at.is_none_or(|end| o.at >= end));
    if !prompt_open && !tool_open {
        return t.last_turn_end_at.map(|_| Opinion::Idle);
    }
    let fresh = cx.stamps.row_up.is_some_and(|r| cx.now_up.saturating_sub(r) < ROWS_FRESH_MS);
    if fresh || tool_open {
        Some(Opinion::Working)
    } else {
        None
    }
}

/// The working/idle opinion of the first source of *P* that has one, and
/// whether a lower-ranked source agrees.
fn opinion(e: &EvidenceSet, cx: &DeriveCtx, eff: &Effective, p: &[Src]) -> Option<(Opinion, Src, bool)> {
    let mut first: Option<(Opinion, Src)> = None;
    for s in p {
        let o = match s {
            Src::S1 => e.stream.as_ref().map(|st| match st.phase {
                TurnPhase::Running | TurnPhase::RequiresAction => Opinion::Working,
                TurnPhase::Idle => Opinion::Idle,
            }),
            Src::S2 => match eff.registry_status {
                Some(RegStatus::Busy) => Some(Opinion::Working),
                Some(RegStatus::Idle) => Some(Opinion::Idle),
                _ => None,
            },
            Src::S3 => s3_opinion(e, cx),
            Src::S4 => eff.is_active.map(|a| if a { Opinion::Working } else { Opinion::Idle }),
            Src::S5 => e.pty.as_ref().map(|t| match t.status {
                PtyStatus::Working | PtyStatus::Stalled => Opinion::Working,
                PtyStatus::Idle | PtyStatus::WaitingForInput => Opinion::Idle,
            }),
            _ => None,
        };
        if let Some(o) = o {
            match first {
                None => first = Some((o, *s)),
                Some((f, src)) if f == o => return Some((f, src, true)),
                Some(_) => {}
            }
        }
    }
    first.map(|(o, s)| (o, s, false))
}

/// When the S3 30 s window, the 10 min busy rule, the isActive quiet rule or
/// the permission-request TTL could flip the outcome by time alone.
fn opinion_recheck(e: &EvidenceSet, cx: &DeriveCtx, p: &[Src]) -> Option<u64> {
    let mut at = None;
    if let Some(r) = cx.stamps.row_up {
        let age = cx.now_up.saturating_sub(r);
        if p.contains(&Src::S3) && age < ROWS_FRESH_MS {
            at = min_opt(at, Some(r + ROWS_FRESH_MS));
        }
        if p.contains(&Src::S2) && age <= BUSY_STALE_ROWS_MS {
            at = min_opt(at, Some(r + BUSY_STALE_ROWS_MS + 1));
        }
        if p.contains(&Src::S4) && cx.position == TeamPosition::HostedTeammate && age < IS_ACTIVE_QUIET_MS {
            at = min_opt(at, Some(r + IS_ACTIVE_QUIET_MS));
        }
    }
    // A33.5(b): the busy status itself crosses 600 s.
    if let Some(st) = cx.stamps.status_up {
        if p.contains(&Src::S2) && cx.now_up.saturating_sub(st) <= BUSY_STALE_ROWS_MS {
            at = min_opt(at, Some(st + BUSY_STALE_ROWS_MS + 1));
        }
    }
    if p.contains(&Src::S4) {
        if let Some(u) = cx.stamps.permission_request_up {
            if e.team.as_ref().is_some_and(|t| t.permission_request.is_some()) {
                at = min_opt(at, Some(u + PERMISSION_REQUEST_TTL_MS));
            }
        }
    }
    at
}

/// R8: Working, or No progress after `no_progress_after_secs` of quiet
/// (uptime), unless the lead has working members.
fn working(e: &EvidenceSet, cx: &DeriveCtx, eff: &Effective, src: Src, rule: &'static str) -> Derived {
    let n_ms = u64::from(clamp_no_progress(e.no_progress_after_secs)) * 1000;
    let last = [cx.stamps.frame_up, cx.stamps.row_up, cx.stamps.output_up, cx.stamps.cpu_up]
        .into_iter()
        .flatten()
        .max();
    let exempt = e.lead_has_working_members;
    let busy_rule =
        src == Src::S2 && eff.busy_stale_rows && !matches!(cx.position, TeamPosition::LeadWithLivePanes);
    // Under the busy rule the quiet time is that of the registry and the
    // transcript (CPU use is not progress there); there may be no other
    // activity stamp at all (A33.5(b): no transcript).
    let busy_quiet = [cx.stamps.row_up, cx.stamps.status_up]
        .into_iter()
        .flatten()
        .max()
        .map(|since| cx.now_up.saturating_sub(since));
    let secs = |ms: u64| u32::try_from(ms / 1000).unwrap_or(u32::MAX);
    if busy_rule && !exempt {
        let quiet = busy_quiet.or(last.map(|l| cx.now_up.saturating_sub(l))).unwrap_or(0);
        return Derived::new(ActivityState::NoProgress { for_secs: secs(quiet) }, src, rule);
    }
    match last {
        Some(last) => {
            let quiet = cx.now_up.saturating_sub(last);
            if !exempt && quiet >= n_ms {
                return Derived::new(ActivityState::NoProgress { for_secs: secs(quiet) }, src, rule);
            }
            let d = Derived::new(ActivityState::Working, src, rule);
            if exempt {
                d
            } else {
                d.recheck(Some(last + n_ms))
            }
        }
        None => Derived::new(ActivityState::Working, src, rule),
    }
}

/// R9: a turn ended after `last_seen_at` and after the last dismissal;
/// Watching sessions only when the setting allows.
fn r9_new_reply(e: &EvidenceSet, cx: &DeriveCtx) -> Option<Derived> {
    if matches!(cx.control, ControlKind::Watching) && !e.watched_band5_allowed {
        return None;
    }
    let ends = [
        e.stream.as_ref().and_then(|s| s.last_turn_end.as_ref()).map(|t| t.at),
        e.transcript.as_ref().and_then(|t| t.last_turn_end_at),
        e.team.as_ref().and_then(|t| t.last_idle.as_ref()).map(|(at, _)| *at),
    ];
    let end = ends.into_iter().flatten().max()?;
    let seen = e.marks.last_seen_at.is_some_and(|s| s >= end);
    let dismissed = e.marks.new_reply_dismissed_at.is_some_and(|s| s >= end);
    (!seen && !dismissed).then(|| Derived::new(ActivityState::NewReply, Src::S1, "R9").since(Some(end)))
}

// ---------------------------------------------------------------------------
// bands, primary actions and caps (§3.5.7, §3.4.4)
// ---------------------------------------------------------------------------

/// The attention band of a state (§3.5.7), `None` = not in the queue.
pub fn band(state: &ActivityState) -> Option<u8> {
    match state {
        ActivityState::NeedsApproval => Some(1),
        ActivityState::NeedsAnswer => Some(2),
        ActivityState::Error { .. } | ActivityState::Crashed { .. } => Some(3),
        ActivityState::NoProgress { .. } => Some(4),
        ActivityState::NewReply => Some(5),
        _ => None,
    }
}

/// The primary action of §3.5.7 for a state and control.
pub fn primary_action(state: &ActivityState, control: ControlKind, continue_copy: bool) -> PrimaryAction {
    let watching = matches!(control, ControlKind::Watching);
    let not_running = matches!(control, ControlKind::NotRunning);
    match state {
        ActivityState::Starting => PrimaryAction::Stop,
        ActivityState::Working => PrimaryAction::Open,
        ActivityState::NeedsApproval | ActivityState::NeedsAnswer => {
            if watching {
                PrimaryAction::GoToOwner
            } else {
                PrimaryAction::Answer
            }
        }
        ActivityState::Error { sub } => match sub {
            ErrorSub::UsageLimit { .. } if watching => PrimaryAction::GoToOwner,
            ErrorSub::UsageLimit { .. } => PrimaryAction::ContinueOnOtherAccount,
            ErrorSub::SignInNeeded if watching => PrimaryAction::GoToOwner,
            ErrorSub::SignInNeeded => PrimaryAction::SignIn,
            ErrorSub::DidntStart { reason: DidntStartReason::Timeout | DidntStartReason::Exited } => {
                PrimaryAction::ShowLog
            }
            ErrorSub::DidntStart { .. } => PrimaryAction::StartAsTerminal,
            ErrorSub::AgentError if watching => PrimaryAction::GoToOwner,
            ErrorSub::AgentError => PrimaryAction::Retry,
        },
        ActivityState::Crashed { .. } => PrimaryAction::ContinueInGilde,
        ActivityState::NoProgress { .. } => PrimaryAction::Open,
        ActivityState::NewReply => PrimaryAction::Review,
        ActivityState::Ready => {
            if watching || not_running {
                PrimaryAction::Open
            } else {
                PrimaryAction::Message
            }
        }
        ActivityState::Ended | ActivityState::Stopped => {
            if continue_copy {
                PrimaryAction::ContinueCopy
            } else {
                PrimaryAction::Continue
            }
        }
        ActivityState::StatusUnknown { .. } => {
            if watching {
                PrimaryAction::GoToOwner
            } else {
                PrimaryAction::Open
            }
        }
    }
}

/// Inputs of [`caps`] besides control and state.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CapsInput {
    pub vendor_session_id_known: bool,
    pub transcript_known: bool,
    pub origin: Option<Origin>,
    pub has_owner: bool,
    pub launch_permission_mode: Option<PermissionMode>,
    pub pty_known: bool,
    pub git_location: bool,
    pub gilde_spawned: bool,
    /// The Conversation compat state of the session's claude version (§3.10).
    pub conversation_compat: Option<CompatState>,
}

/// `SessionCaps` (§3.4.4): the single source of what is clickable.
pub fn caps(control: ControlKind, vendor: Vendor, state: &ActivityState, i: &CapsInput) -> SessionCaps {
    let busy = matches!(
        state,
        ActivityState::Working
            | ActivityState::NeedsApproval
            | ActivityState::NeedsAnswer
            | ActivityState::NoProgress { .. }
            | ActivityState::Starting
    );
    let mut c = SessionCaps {
        send: SendCap::None,
        interrupt: false,
        force_stop: false,
        approve: ApproveCap::None,
        set_model: false,
        set_permission_mode: Vec::new(),
        continue_options: Vec::new(),
        stop_session: StopCap::None,
        terminal_view: false,
        log_view: i.gilde_spawned,
        changes: ChangesCap::None,
        owner_focus: false,
    };
    match control {
        ControlKind::InGilde(mode) => {
            c.stop_session = StopCap::Fenced;
            c.interrupt = busy;
            match mode {
                ControlMode::Conversation => {
                    c.send = SendCap::Conversation;
                    c.force_stop = true;
                    c.approve = ApproveCap::Structured;
                    c.set_model = true;
                    let mut modes = vec![
                        PermissionMode::Default,
                        PermissionMode::AcceptEdits,
                        PermissionMode::Plan,
                        PermissionMode::DontAsk,
                        PermissionMode::Auto,
                    ];
                    // BypassPermissions only when launched with it [R4 §1.9].
                    if i.launch_permission_mode == Some(PermissionMode::BypassPermissions) {
                        modes.push(PermissionMode::BypassPermissions);
                    }
                    c.set_permission_mode = modes;
                }
                ControlMode::Terminal => {
                    c.send = SendCap::PtyPaste;
                    c.approve = ApproveCap::ScreenKeys;
                    c.terminal_view = i.pty_known;
                }
                ControlMode::TurnBased => {
                    c.send = SendCap::Turn;
                    c.set_model = true;
                }
            }
            if let ActivityState::Error { sub: ErrorSub::UsageLimit { .. } } = state {
                if vendor == Vendor::Claude && i.vendor_session_id_known {
                    c.continue_options.push(ContinueOption::OtherAccount);
                }
            }
            c.changes = if i.git_location { ChangesCap::Full } else { ChangesCap::None };
        }
        ControlKind::Watching => {
            c.approve = ApproveCap::GoToOwner;
            c.owner_focus = i.has_owner;
            c.stop_session = match (vendor, i.origin) {
                (Vendor::Claude, Some(Origin::Background)) => StopCap::ClaudeStop,
                _ => StopCap::TermAfterIdentity,
            };
            if vendor == Vendor::Claude && i.vendor_session_id_known {
                if i.origin == Some(Origin::Background) {
                    c.continue_options.push(ContinueOption::Attach);
                }
                c.continue_options.push(ContinueOption::Fork);
            }
            c.changes = if i.git_location { ChangesCap::ReadOnly } else { ChangesCap::None };
        }
        ControlKind::NotRunning => {
            if i.vendor_session_id_known {
                c.continue_options.push(ContinueOption::Resume);
                if vendor == Vendor::Claude {
                    c.continue_options.push(ContinueOption::Fork);
                }
            }
            match state {
                ActivityState::Error { sub: ErrorSub::UsageLimit { .. } }
                    if vendor == Vendor::Claude && i.vendor_session_id_known =>
                {
                    c.continue_options.push(ContinueOption::OtherAccount);
                }
                ActivityState::Error {
                    sub: ErrorSub::DidntStart { reason: DidntStartReason::CompatFailed | DidntStartReason::MinVersion },
                } if !i.transcript_known && i.gilde_spawned => {
                    c.continue_options.push(ContinueOption::RetryAsTerminal);
                }
                _ => {}
            }
            c.changes = if i.git_location { ChangesCap::Full } else { ChangesCap::None };
        }
    }
    // Keep the list free of duplicates in the order added.
    let mut seen = BTreeSet::new();
    c.continue_options.retain(|o| seen.insert(format!("{o:?}")));
    c
}

// ---------------------------------------------------------------------------
// unit tests: one per staleness row and the subtle rule orderings; the table
// tests of §3.5.4–§3.5.6 live in tests/world_derive.rs
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::activity::{
        GildeFacts, PtyFacts, RegistryFacts, ScreenPromptFacts, StopCause, StreamFacts, TeamFileFacts,
        TranscriptFacts, TurnEnd, UserMarks,
    };

    fn alive() -> ProcessFacts {
        ProcessFacts::Alive { pid: 10, start_sec: 1, cpu_active_at: None }
    }

    fn set(process: ProcessFacts) -> EvidenceSet {
        EvidenceSet {
            gilde: None,
            process,
            stream: None,
            registry: None,
            transcript: None,
            team: None,
            pty: None,
            marks: UserMarks::default(),
            lead_has_working_members: false,
            watched_band5_allowed: false,
            no_progress_after_secs: 300,
            source_denied: false,
        }
    }

    fn cx(control: ControlKind) -> DeriveCtx {
        DeriveCtx {
            vendor: Vendor::Claude,
            control,
            position: TeamPosition::Solo,
            held_turn_based: false,
            delegated: None,
            stamps: Stamps::default(),
            now_up: 1_000_000,
            now_wall: 1_800_000_000_000,
        }
    }

    fn gilde(mode: ControlMode) -> GildeFacts {
        GildeFacts {
            spawned_at: 1_799_999_990_000,
            mode,
            ready_at: Some(1_799_999_991_000),
            stop_cause: None,
            signals_sent: vec![],
            start_failure: None,
        }
    }

    #[test]
    fn r1_stop_cause_wins_over_a_nonzero_exit() {
        let mut e = set(ProcessFacts::Exited { at: 5, code: Some(143), signal: None });
        let mut g = gilde(ControlMode::Conversation);
        g.stop_cause = Some(StopCause::User);
        e.gilde = Some(g);
        let d = derive(&e, &cx(ControlKind::NotRunning));
        assert_eq!(d.state, ActivityState::Stopped);
        assert_eq!(d.rule, "R1");
    }

    #[test]
    fn r4_runs_before_r2_for_an_exit_before_ready() {
        let mut e = set(ProcessFacts::Exited { at: 5, code: Some(1), signal: None });
        let mut g = gilde(ControlMode::Conversation);
        g.ready_at = None;
        e.gilde = Some(g);
        let d = derive(&e, &cx(ControlKind::NotRunning));
        assert_eq!(d.state, ActivityState::Error { sub: ErrorSub::DidntStart { reason: DidntStartReason::Exited } });
        assert_eq!(d.rule, "R4");
    }

    #[test]
    fn registry_behind_the_rows_is_stale() {
        let mut e = set(alive());
        e.registry = Some(RegistryFacts {
            status: Some(RegStatus::Busy),
            waiting_for: None,
            status_updated_at: Some(1_000),
            peer_protocol_ok: true,
        });
        e.transcript = Some(TranscriptFacts {
            last_row_at: Some(10_000),
            open_tool_use: None,
            last_turn_end_at: Some(10_000),
            last_human_prompt_at: Some(9_000),
            last_api_error: None,
        });
        let eff = staleness(&e, &cx(ControlKind::Watching));
        assert_eq!(eff.registry_status, None);
        assert!(eff.registry_stale);
    }

    #[test]
    fn hosted_lead_busy_is_not_activity_but_waiting_is() {
        let mut e = set(alive());
        e.registry = Some(RegistryFacts {
            status: Some(RegStatus::Busy),
            waiting_for: None,
            status_updated_at: Some(20_000),
            peer_protocol_ok: true,
        });
        let mut c = cx(ControlKind::InGilde(ControlMode::Terminal));
        c.position = TeamPosition::LeadWithLivePanes;
        assert_eq!(staleness(&e, &c).registry_status, None);
        e.registry.as_mut().unwrap().status = Some(RegStatus::Waiting);
        assert!(staleness(&e, &c).registry_waiting);
    }

    #[test]
    fn is_active_ignored_for_a_dead_process_and_after_an_interrupt_marker() {
        let mut e = set(alive());
        e.team = Some(TeamFileFacts { is_active: Some(true), in_roster: true, permission_request: None, last_idle: None });
        e.transcript = Some(TranscriptFacts {
            last_row_at: Some(5_000),
            open_tool_use: None,
            last_turn_end_at: Some(5_000),
            last_human_prompt_at: Some(4_000),
            last_api_error: None,
        });
        let mut c = cx(ControlKind::Watching);
        c.stamps.config_mtime = Some(3_000);
        assert_eq!(staleness(&e, &c).is_active, None);
        c.stamps.config_mtime = Some(6_000);
        assert_eq!(staleness(&e, &c).is_active, Some(true));
        e.process = ProcessFacts::Gone { at: 7_000 };
        assert_eq!(staleness(&e, &c).is_active, None);
    }

    #[test]
    fn s5_low_confidence_is_likely() {
        let mut e = set(alive());
        e.pty = Some(PtyFacts { status: PtyStatus::Working, confidence: 0.4, last_output_at: None, prompt: None });
        let d = derive(&e, &cx(ControlKind::InGilde(ControlMode::Terminal)));
        assert_eq!(d.state, ActivityState::Working);
        assert_eq!(d.confidence, Confidence::Likely);
    }

    #[test]
    fn a_setup_prompt_while_starting_pauses_into_needs_answer() {
        let mut e = set(alive());
        let mut g = gilde(ControlMode::Terminal);
        g.ready_at = None;
        e.gilde = Some(g);
        e.pty = Some(PtyFacts {
            status: PtyStatus::WaitingForInput,
            confidence: 0.9,
            last_output_at: None,
            prompt: Some(ScreenPromptFacts {
                prompt_rev: "0123456789abcdef".into(),
                question: None,
                header: None,
                options: vec![],
                footer: None,
                permission_shaped: false,
                setup: true,
                trust_dialog: true,
                allows_note: false,
                near_bottom: true,
                first_seen_at: 1_000,
            }),
        });
        let d = derive(&e, &cx(ControlKind::InGilde(ControlMode::Terminal)));
        assert_eq!(d.state, ActivityState::NeedsAnswer);
        assert_eq!(d.rule, "R4");
    }

    #[test]
    fn interrupted_stream_turn_is_ready_not_error() {
        let mut e = set(alive());
        e.gilde = Some(gilde(ControlMode::Conversation));
        e.marks.last_seen_at = Some(9_000);
        e.stream = Some(StreamFacts {
            phase: TurnPhase::Idle,
            phase_since: 8_000,
            pending: vec![],
            last_turn_start_at: Some(7_000),
            last_turn_end: Some(TurnEnd { at: 8_000, outcome: TurnOutcome::Interrupted }),
            last_frame_at: 8_000,
        });
        let d = derive(&e, &cx(ControlKind::InGilde(ControlMode::Conversation)));
        assert_eq!(d.state, ActivityState::Ready);
    }

    #[test]
    fn band_and_primary_follow_the_table() {
        assert_eq!(band(&ActivityState::NeedsApproval), Some(1));
        assert_eq!(band(&ActivityState::Working), None);
        assert_eq!(
            primary_action(&ActivityState::NeedsApproval, ControlKind::Watching, false),
            PrimaryAction::GoToOwner
        );
        assert_eq!(primary_action(&ActivityState::Ended, ControlKind::NotRunning, true), PrimaryAction::ContinueCopy);
    }

    fn busy(status_at: EpochMs) -> RegistryFacts {
        RegistryFacts { status: Some(RegStatus::Busy), waiting_for: None, status_updated_at: Some(status_at), peer_protocol_ok: true }
    }

    fn rows(last_row_at: EpochMs, prompt_at: EpochMs, end_at: Option<EpochMs>) -> TranscriptFacts {
        TranscriptFacts {
            last_row_at: Some(last_row_at),
            open_tool_use: None,
            last_turn_end_at: end_at,
            last_human_prompt_at: Some(prompt_at),
            last_api_error: None,
        }
    }

    /// A33.5(b), PM-01: a `busy` whose `statusUpdatedAt` is older than 600 s
    /// with no newer transcript row is not Working — also with no transcript.
    #[test]
    fn a33_5b_old_busy_without_newer_rows_is_no_progress() {
        let c0 = cx(ControlKind::Watching);
        let (now_up, now) = (c0.now_up, c0.now_wall);
        // No transcript at all, status 2 days old (stamped at arrival).
        let mut e = set(alive());
        e.registry = Some(busy(now - 2 * 86_400_000));
        let mut c = c0.clone();
        c.stamps.status_up = Some(0);
        assert_eq!(derive(&e, &c).state, ActivityState::NoProgress { for_secs: (now_up / 1000) as u32 });
        // Variant: 601 s old.
        c.stamps.status_up = Some(now_up - 601_000);
        assert_eq!(derive(&e, &c).state, ActivityState::NoProgress { for_secs: 601 });
        // Fresh CPU use is not progress under the busy rule.
        c.stamps.cpu_up = Some(now_up - 1_000);
        assert_eq!(derive(&e, &c).state, ActivityState::NoProgress { for_secs: 601 });
        c.stamps.cpu_up = None;
        // 599 s: still Working, re-derived when it crosses 600 s.
        c.stamps.status_up = Some(now_up - 599_000);
        let d = derive(&e, &c);
        assert_eq!(d.state, ActivityState::Working);
        assert_eq!(d.recheck_up, Some(now_up - 599_000 + BUSY_STALE_ROWS_MS + 1));
        // Rows as old as the status (not newer): No progress.
        e.registry = Some(busy(now - 700_000));
        c.stamps.status_up = Some(now_up - 700_000);
        c.stamps.row_up = Some(now_up - 705_000);
        e.transcript = Some(rows(now - 705_000, now - 710_000, None));
        assert!(matches!(derive(&e, &c).state, ActivityState::NoProgress { .. }));
        // A newer, fresh row: the transcript decides (the registry is stale).
        e.transcript = Some(rows(now - 2_000, now - 3_000, None));
        c.stamps.row_up = Some(now_up - 2_000);
        assert_eq!(derive(&e, &c).state, ActivityState::Working);
        e.transcript = None;
        c.stamps.row_up = None;
        // A lead with working members is exempt; a lead with live panes never
        // uses the busy rule.
        e.lead_has_working_members = true;
        assert_eq!(derive(&e, &c).state, ActivityState::Working);
        e.lead_has_working_members = false;
        let mut lead = c.clone();
        lead.position = TeamPosition::LeadWithLivePanes;
        assert!(!matches!(derive(&e, &lead).state, ActivityState::NoProgress { .. } | ActivityState::Working));
        // No stamp (status absent): believed as before.
        let mut e2 = set(alive());
        e2.registry = Some(RegistryFacts { status_updated_at: None, ..busy(0) });
        assert_eq!(derive(&e2, &c0).state, ActivityState::Working);
    }
}
