//! The session core of a Conversation session: stdout frames → blocks, S1
//! evidence, cards and turn outcomes (contract §6.1.5, §6.1.7), and the
//! command side of §6.1.3–§6.1.4 — all without IO.
//!
//! Owner: T05.
//!
//! [`Core`] is a synchronous state machine. The driver feeds it stdout lines,
//! commands and ticks with the current time ([`Now`]); it writes stdin frames
//! through an [`Outbox`] (which may refuse: the stdin queue is bounded) and
//! returns everything else as [`Effect`]s (evidence for World, content for the
//! ContentHub, compat evidence, hooks). Writes are attempted **before** state
//! changes, so a full stdin queue (`BUSY{stdinFull}`) leaves no half-applied
//! answer.
//!
//! - **Phase** (S1): `session_state_changed` (P9) drives Idle/Running;
//!   `system/init` alone starts a turn (an unprompted turn [R6 §3 LP-05]);
//!   `result` ends it; RequiresAction is derived from any pending card (the
//!   sandbox ask is not bracketed [R6 §8 item 11]). Without
//!   `session_state_changed` (P9 fallback) the first frame after a user frame
//!   starts the turn.
//! - **Outcome** ([`classify`]): by `terminal_reason` and `assistant.error`
//!   (with `is_error`), never by `subtype` alone; a turn in which Gilde sent
//!   `interrupt` or Deny-and-stop, or whose interrupt receipt arrived, is
//!   Interrupted whatever its subtype [F-02].
//! - **Spend** ([`StreamUsage`], A10.4): `message.usage` once per
//!   `message.id` (each field the largest value seen across `message_start`,
//!   `assistant` and `message_delta`, so the final output count wins), T04's
//!   mapping (input = uncached + cache-creation, cached = cache-read); cost
//!   from `result.total_cost_usd`, which is the process' running total
//!   (observed monotonic across turns, and restored on `--resume`): only its
//!   increments count, and a resumed process's first value is the baseline.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use regex::Regex;
use serde::Deserialize;
use serde_json::value::RawValue;
use tokio::sync::oneshot;

use crate::contract::account::Quota;
use crate::contract::activity::{
    ErrorSub, GildeFacts, StartFailure, StopCause, StreamFacts, TurnEnd, TurnOutcome, TurnPhase,
};
use crate::contract::common::{clamp_safe_u64, AbsPath, EpochMs, Notice, PermissionMode};
use crate::contract::compat::CompatCapability;
use crate::contract::content::{Delivery, ImageRef, InteractionOutcome, SystemNote};
use crate::contract::display::TextCap;
use crate::contract::error::IpcError;
use crate::contract::ids::{AccountId, ClientMsgId, RequestId, SessionUid};
use crate::contract::interaction::{Decision, DecisionKind, DecisionMisfit, InteractionKind};
use crate::contract::ipc::{AnswerOutcome, AppliesFrom, InteractionDetailRes, InterruptOutcome, ModelOption, SendDelivery, SendPriority};
use crate::contract::services::{OutgoingMessage, SessionMetaFacts, SourceFacts, UsageFacts, UsageSource};
use crate::contract::session::{BinaryRef, ControlMode, QueuedMessage, UsageToday};
use crate::contract::validate::Validate;
use crate::convo::approvals::{build_request, decision_payload, CardContext, PayloadError, PendingRequest};
use crate::convo::guard::{ModeFlipGuard, SettingsFileGuard, SilentApprovalGuard};
use crate::convo::handshake::{self, parse_initialize, permission_mode_of, InitVerdict, LaunchKind};
use crate::convo::ledger::{AnswerCheck, GildeKind, GildeReply, GildeTable, Ledger, Seen};
use crate::convo::wire::{
    self, control_request, control_response_error, control_response_success, depth_within, user_frame_with_images,
    ControlBody, UserFrameShape, MAX_DEPTH, UNSUPPORTED_ERROR,
};
use crate::convo::Flags;
use crate::render::claude_stream::{self, AssistantMsgIn, EventIn, StreamRenderer};
use crate::render::images;
use crate::render::sanitize::display;
use crate::render::tools::{self, PathBase};
use crate::render::transcript::TokenUsage;

/// `CONFLICT{tooFresh}` below this age of a request (§8.6).
pub const TOO_FRESH_MS: u64 = 300;
/// `turn_force_stop` is offered only after this long without a `result`.
pub const FORCE_STOP_AFTER_MS: u64 = 5_000;
/// The send-now fallback waits at most this long for the turn's `result`.
pub const SEND_NOW_WAIT_MS: u64 = 5_000;
/// S1 facts that only moved `last_frame_at` are re-published at most this often.
pub const FRAME_FACTS_EVERY_MS: u64 = 1_000;
/// Usage reports: at most one per second per session (A10.4).
pub const USAGE_EVERY_MS: u64 = 1_000;
/// `clientMsgId`s remembered for `turn_send` idempotency.
const SENT_KEPT: usize = 256;
/// The interaction detail (`→` expand) cap.
pub const DETAIL_CAP: TextCap = TextCap::Bytes(64 * 1024);

// ---------------------------------------------------------------------------
// time, outbox, effects
// ---------------------------------------------------------------------------

/// The current time: wall clock (facts) and uptime (durations).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Now {
    pub wall: EpochMs,
    pub mono: u64,
}

/// What a stdin frame is (its size cap differs, §6.1.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameClass {
    /// User frames: ≤ 1 MiB.
    User,
    /// A user frame that carries images: ≤ 32 MiB.
    UserImages,
    /// Gilde's control requests and error replies: ≤ 1 MiB.
    Control,
    /// A decision echoing the original input: ≤ the request it answers.
    Answer,
}

impl FrameClass {
    pub fn cap(self) -> usize {
        match self {
            FrameClass::User | FrameClass::Control => wire::FRAME_CAP,
            FrameClass::UserImages => wire::IMAGE_FRAME_CAP,
            FrameClass::Answer => wire::ANSWER_FRAME_CAP,
        }
    }
}

/// Why a frame was not queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteError {
    /// The stdin queue is full: the CLI stopped reading.
    Full,
    /// Larger than its class cap.
    TooLarge,
    /// Stdin is closed (stop sequence, exit).
    Closed,
}

/// Where the core writes stdin frames (one line each, without `\n`).
pub trait Outbox {
    fn try_write(&mut self, line: String, class: FrameClass) -> Result<(), WriteError>;
    /// A decision for one card (`tag` = the card id): dropped before it is
    /// written if [`Outbox::withdraw`] names it meanwhile (§6.1.4 Withdrawal).
    fn try_write_tagged(&mut self, line: String, class: FrameClass, tag: &str) -> Result<(), WriteError> {
        let _ = tag;
        self.try_write(line, class)
    }
    /// The CLI withdrew this card: an answer still queued for it is dropped.
    fn withdraw(&mut self, tag: &str) {
        let _ = tag;
    }
}

/// Most pending cards one session holds; more are counted, not shown (a
/// hostile or broken stream cannot grow memory without bound).
pub const MAX_PENDING: usize = 256;
/// A CLI `request_id` longer than this is never echoed back.
pub const MAX_WIRE_ID: usize = 4 * 1024;

/// The card id of a CLI `request_id`: itself when it is a valid `RequestId`
/// (printable ASCII ≤ 128), else `h_` + the first 16 hex of its SHA-256 — so a
/// long or odd id still gets a card and is answered with the original id.
pub fn card_id_of(wire_id: &str) -> String {
    if RequestId::is_valid_str(wire_id) {
        wire_id.to_owned()
    } else {
        format!("h_{}", crate::contract::ids::hash16(wire_id.as_bytes()))
    }
}

/// Compat evidence for the spawn-time version (§3.10 inputs 3–4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompatEvent {
    Handshake { ok: bool, note: Option<String> },
    Deviation { capability: CompatCapability, note: String },
    Note(String),
}

/// Things the session manager (T10) or the accounts service act on; see
/// `driver::ConvoHooks`.
#[derive(Debug, Clone, PartialEq)]
pub enum HookEvent {
    /// `rate_limit_event` / `get_usage`: the account's quota without a probe.
    Quota(Quota),
    /// `initialize.account.tokenSource == "none"` [F-29].
    SignedOut,
    /// `Session.queued` changed.
    Queued(Vec<QueuedMessage>),
    /// §6.1.11: a guarded tool ran without a seen permission request.
    ApprovalUnconfirmed(String),
    /// A line for the session (`convo.notReading`, `convo.unanswerable`).
    Notice(Notice),
    /// `SessionDetails.version_note` (spawn-time vs reported version) [A1].
    VersionNote(String),
    /// A35.1: the CLI reported a mode (`None` = one Gilde does not know).
    PermissionMode(Option<PermissionMode>),
}

/// Everything the core asks the outside world to do, in order.
#[derive(Debug)]
pub enum Effect {
    /// One source fact for World (the driver adds the identity hint).
    Evidence(SourceFacts),
    /// One update for the ContentHub.
    Content(crate::contract::services::ContentUpdate),
    Compat(CompatEvent),
    Hook(HookEvent),
    /// The handshake failed the compat check: stop the process (close
    /// stdin, then fenced SIGTERM after 1.5 s) with no stop cause.
    StopForCompat,
    /// A `result` ended a turn (the stop sequence and the send-now fallback
    /// wait for it).
    TurnEnded,
}

// ---------------------------------------------------------------------------
// error patterns (resources/error-patterns.json, hot-reloaded)
// ---------------------------------------------------------------------------

/// The bundled patterns.
pub const BUNDLED_ERROR_PATTERNS: &str = include_str!("../../resources/error-patterns.json");

#[derive(Debug, Deserialize)]
struct PatternsFile {
    schema: u32,
    #[serde(rename = "signInErrorCodes", default)]
    sign_in_codes: Vec<String>,
    #[serde(rename = "signIn", default)]
    sign_in: Vec<PatternIn>,
    #[serde(rename = "usageLimitErrorCodes", default)]
    usage_codes: Vec<String>,
    #[serde(rename = "usageLimitStatus", default)]
    usage_status: Vec<i64>,
    #[serde(rename = "usageLimit", default)]
    usage_limit: Vec<PatternIn>,
    #[serde(rename = "usageErrorMarkers", default)]
    usage_error_markers: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct PatternIn {
    regex: String,
}

/// The error-class patterns of §6.1.7 (data, so a wording change is a data fix).
#[derive(Debug, Clone)]
pub struct ErrorPatterns {
    sign_in_codes: Vec<String>,
    sign_in: Vec<Regex>,
    usage_codes: Vec<String>,
    usage_status: Vec<i64>,
    usage_limit: Vec<Regex>,
    pub usage_error_markers: Vec<String>,
}

impl ErrorPatterns {
    pub fn parse(json: &str) -> Result<ErrorPatterns, String> {
        let f: PatternsFile = serde_json::from_str(json).map_err(|e| format!("error-patterns.json: {e}"))?;
        if f.schema != 1 {
            return Err(format!("error-patterns.json: schema {} is not 1", f.schema));
        }
        let compile = |list: Vec<PatternIn>| -> Result<Vec<Regex>, String> {
            list.into_iter()
                .map(|p| {
                    regex::RegexBuilder::new(&p.regex)
                        .size_limit(1 << 20)
                        .build()
                        .map_err(|e| format!("error-patterns.json: bad regex: {e}"))
                })
                .collect()
        };
        Ok(ErrorPatterns {
            sign_in_codes: f.sign_in_codes,
            sign_in: compile(f.sign_in)?,
            usage_codes: f.usage_codes,
            usage_status: f.usage_status,
            usage_limit: compile(f.usage_limit)?,
            usage_error_markers: f.usage_error_markers,
        })
    }

    pub fn bundled() -> ErrorPatterns {
        ErrorPatterns::parse(BUNDLED_ERROR_PATTERNS).expect("the bundled error-patterns.json must parse")
    }

    /// Sign-in needed: an `assistant.error` code or the text.
    pub fn sign_in(&self, code: Option<&str>, text: &str) -> bool {
        code.is_some_and(|c| self.sign_in_codes.iter().any(|s| s == c)) || self.sign_in.iter().any(|r| r.is_match(text))
    }

    /// Usage limit: `Some(resets_at)` when it is one (PROVISIONAL, LP-09c).
    pub fn usage_limit(&self, code: Option<&str>, statuses: &[i64], text: &str) -> Option<Option<EpochMs>> {
        let mut hit = code.is_some_and(|c| self.usage_codes.iter().any(|s| s == c))
            || statuses.iter().any(|s| self.usage_status.contains(s));
        let mut resets = None;
        for r in &self.usage_limit {
            if let Some(c) = r.captures(text) {
                hit = true;
                if resets.is_none() {
                    resets = c
                        .name("epoch_s")
                        .and_then(|m| m.as_str().parse::<i64>().ok())
                        .map(|s| s.saturating_mul(1000))
                        .filter(|ms| crate::contract::common::is_epoch_ms(*ms));
                }
            }
        }
        hit.then_some(resets)
    }
}

/// The patterns with an optional on-disk override that is re-read when it
/// changes ("hot-reloaded like signatures"); a broken override keeps the
/// last good patterns.
#[derive(Debug)]
pub struct PatternSource {
    override_path: Option<PathBuf>,
    current: RwLock<Arc<ErrorPatterns>>,
    stamp: Mutex<Option<crate::convo::guard::FileStamp>>,
    last_check: AtomicU64,
}

impl PatternSource {
    /// Re-`stat` the override at most this often.
    pub const CHECK_EVERY_MS: u64 = 2_000;

    pub fn bundled() -> Arc<PatternSource> {
        Arc::new(PatternSource {
            override_path: None,
            current: RwLock::new(Arc::new(ErrorPatterns::bundled())),
            stamp: Mutex::new(None),
            last_check: AtomicU64::new(0),
        })
    }

    /// The bundled patterns, replaced by `path` whenever it holds a valid file.
    pub fn with_override(path: PathBuf) -> Arc<PatternSource> {
        let s = PatternSource {
            override_path: Some(path),
            current: RwLock::new(Arc::new(ErrorPatterns::bundled())),
            stamp: Mutex::new(None),
            last_check: AtomicU64::new(0),
        };
        s.reload_if_changed();
        Arc::new(s)
    }

    fn reload_if_changed(&self) {
        let Some(path) = &self.override_path else { return };
        let st = crate::convo::guard::stamp(path);
        let mut last = self.stamp.lock();
        if *last == Some(st) {
            return;
        }
        *last = Some(st);
        if !st.exists {
            return;
        }
        match std::fs::read_to_string(path).map_err(|e| e.to_string()).and_then(|t| ErrorPatterns::parse(&t)) {
            Ok(p) => *self.current.write() = Arc::new(p),
            Err(e) => tracing::warn!(target: "convo", error = %e, "error-patterns override ignored"),
        }
    }

    /// The patterns now (`mono` = uptime ms, for the re-check interval).
    pub fn get(&self, mono: u64) -> Arc<ErrorPatterns> {
        let last = self.last_check.load(Ordering::Relaxed);
        if self.override_path.is_some() && (last == 0 || mono.saturating_sub(last) >= Self::CHECK_EVERY_MS) {
            self.last_check.store(mono.max(1), Ordering::Relaxed);
            self.reload_if_changed();
        }
        self.current.read().clone()
    }
}

// ---------------------------------------------------------------------------
// classification
// ---------------------------------------------------------------------------

/// The inputs of one turn's classification.
#[derive(Debug, Clone, Default)]
pub struct ResultFacts<'a> {
    pub terminal_reason: Option<&'a str>,
    pub is_error: Option<bool>,
    pub subtype: Option<&'a str>,
    pub result_text: &'a str,
    pub api_error_status: Option<i64>,
    /// `assistant.error` seen in this turn.
    pub assistant_error: Option<&'a str>,
    /// `api_retry.error_status` seen in this turn.
    pub retry_status: Option<i64>,
    /// Gilde sent `interrupt` / Deny-and-stop in this turn, or the receipt arrived [F-02].
    pub interrupted_by_gilde: bool,
}

/// §6.1.7 result classification. The `bool` is "unknown `terminal_reason`"
/// (counted in diagnostics).
pub fn classify(r: &ResultFacts<'_>, patterns: &ErrorPatterns, account: Option<&AccountId>) -> (TurnOutcome, bool) {
    let reason = r.terminal_reason;
    if matches!(reason, Some("aborted_tools" | "aborted_streaming")) || r.interrupted_by_gilde {
        return (TurnOutcome::Interrupted, false);
    }
    let is_error = r.is_error.unwrap_or_else(|| r.subtype.is_some_and(|s| s != "success"));
    let known = matches!(reason, None | Some("completed" | "api_error" | "aborted_tools" | "aborted_streaming" | "max_turns" | "error"));
    if !is_error {
        return (TurnOutcome::Ok, !known);
    }
    // Sign-in and usage limit are `api_error` classes (§6.1.7); a CLI without
    // `terminal_reason` is judged by the same signals.
    let api = matches!(reason, None | Some("api_error"));
    if api && patterns.sign_in(r.assistant_error, r.result_text) {
        return (TurnOutcome::Error(ErrorSub::SignInNeeded), !known);
    }
    let statuses: Vec<i64> = [r.api_error_status, r.retry_status].into_iter().flatten().collect();
    if let Some(resets_at) = api.then(|| patterns.usage_limit(r.assistant_error, &statuses, r.result_text)).flatten() {
        return (TurnOutcome::Error(ErrorSub::UsageLimit { account_id: account.cloned(), resets_at }), !known);
    }
    (TurnOutcome::Error(ErrorSub::AgentError), !known)
}

// ---------------------------------------------------------------------------
// spend (A10.4)
// ---------------------------------------------------------------------------

/// The `Stream` usage producer of one session.
#[derive(Debug, Default)]
pub struct StreamUsage {
    day: Option<String>,
    per_msg: HashMap<String, TokenUsage>,
    cost_micros: Option<u64>,
    last_cost_usd: Option<f64>,
    resumed: bool,
    /// `get_usage.session.total_cost_usd` read after a resume [A19.5].
    baseline: Option<f64>,
    /// A resumed process's first `total_cost_usd` seen before the baseline.
    unbased_first: Option<f64>,
    last_sent: Option<UsageToday>,
    last_sent_mono: Option<u64>,
}

fn max_tokens(a: &mut TokenUsage, b: &TokenUsage) -> bool {
    let before = *a;
    a.input = a.input.max(b.input);
    a.output = a.output.max(b.output);
    a.cache_creation = a.cache_creation.max(b.cache_creation);
    a.cache_read = a.cache_read.max(b.cache_read);
    *a != before
}

impl StreamUsage {
    /// `resumed`: the process continues an earlier conversation, so its first
    /// `total_cost_usd` includes cost restored from before.
    pub fn new(resumed: bool) -> StreamUsage {
        StreamUsage { resumed, ..StreamUsage::default() }
    }

    fn roll(&mut self, day: &str) {
        if self.day.as_deref() != Some(day) {
            self.day = Some(day.to_owned());
            self.per_msg.clear();
            self.cost_micros = None;
        }
    }

    /// A usage object of `message_id` observed on local day `day`.
    pub fn message(&mut self, day: &str, message_id: &str, u: &TokenUsage) {
        if message_id.is_empty() {
            return;
        }
        self.roll(day);
        if self.per_msg.len() >= 100_000 && !self.per_msg.contains_key(message_id) {
            return;
        }
        let e = self.per_msg.entry(message_id.to_owned()).or_default();
        max_tokens(e, u);
    }

    /// `result.total_cost_usd` (the process' running total).
    pub fn cost(&mut self, day: &str, total_usd: f64) {
        if !total_usd.is_finite() || total_usd < 0.0 {
            return;
        }
        self.roll(day);
        let delta = match (self.last_cost_usd, self.baseline) {
            (Some(prev), _) => (total_usd - prev).max(0.0),
            (None, Some(b)) => (total_usd - b).max(0.0),
            (None, None) if self.resumed => {
                // The baseline is not known yet: counted when it arrives.
                self.unbased_first = Some(total_usd);
                0.0
            }
            (None, None) => total_usd,
        };
        self.last_cost_usd = Some(total_usd);
        let micros = (delta * 1_000_000.0).round() as u64;
        self.cost_micros = Some(self.cost_micros.unwrap_or(0).saturating_add(micros));
    }

    /// The cost restored on `--resume` (`get_usage.session.total_cost_usd`
    /// before any turn) [A19.5]: only increments over it count. Arriving after
    /// the first `result`, it corrects that turn retroactively. Once only, and
    /// only for a resumed process.
    pub fn set_baseline(&mut self, day: &str, baseline_usd: f64) {
        if !self.resumed || self.baseline.is_some() || !baseline_usd.is_finite() || baseline_usd < 0.0 {
            return;
        }
        self.baseline = Some(baseline_usd);
        if let Some(first) = self.unbased_first.take() {
            self.roll(day);
            let micros = ((first - baseline_usd).max(0.0) * 1_000_000.0).round() as u64;
            self.cost_micros = Some(self.cost_micros.unwrap_or(0).saturating_add(micros));
        }
    }

    /// Today's running total; `None` = nothing reported (unknown, never 0).
    pub fn total(&self) -> Option<UsageToday> {
        let day = self.day.clone()?;
        if self.per_msg.is_empty() && self.cost_micros.is_none() {
            return None;
        }
        let mut t = TokenUsage::default();
        for u in self.per_msg.values() {
            t.add(u);
        }
        let tokens = !self.per_msg.is_empty();
        Some(UsageToday {
            day,
            input_tokens: tokens.then(|| clamp_safe_u64(t.input.saturating_add(t.cache_creation))),
            output_tokens: tokens.then(|| clamp_safe_u64(t.output)),
            cached_tokens: tokens.then(|| clamp_safe_u64(t.cache_read)),
            cost_micros: self.cost_micros.map(clamp_safe_u64),
        })
    }

    /// The report due now (changed, ≥ 1 s after the last one), if any.
    pub fn due(&mut self, mono: u64, force: bool) -> Option<UsageToday> {
        let total = self.total()?;
        if self.last_sent.as_ref() == Some(&total) {
            return None;
        }
        if !force && self.last_sent_mono.is_some_and(|t| mono.saturating_sub(t) < USAGE_EVERY_MS) {
            return None;
        }
        self.last_sent = Some(total.clone());
        self.last_sent_mono = Some(mono);
        Some(total)
    }

    /// When a held-back report becomes due.
    pub fn deadline(&self) -> Option<u64> {
        let total = self.total()?;
        if self.last_sent.as_ref() == Some(&total) {
            return None;
        }
        Some(self.last_sent_mono.map_or(0, |t| t + USAGE_EVERY_MS))
    }
}

// ---------------------------------------------------------------------------
// the core
// ---------------------------------------------------------------------------

/// A tool call left open by a session that was waiting when it stopped
/// (quit record `was_waiting`, or a transcript ending in a `tool_use`) [F-05].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbandonedAsk {
    pub tool_use_id: String,
    pub tool_name: String,
}

/// What the core knows at spawn.
#[derive(Debug, Clone)]
pub struct CoreConfig {
    pub uid: SessionUid,
    pub account_id: Option<AccountId>,
    pub launch: LaunchKind,
    /// The spawn-time version (the compat key, A1).
    pub spawn_version: String,
    /// The version's `handshake_ok` is still `None` (§6.1.2 step 3).
    pub handshake_unchecked: bool,
    pub flags: Flags,
    pub permission_mode: PermissionMode,
    pub allow_bypass: bool,
    pub model: Option<String>,
    pub cwd: AbsPath,
    pub home: Option<AbsPath>,
    pub binary: BinaryRef,
    pub settings_paths: Vec<PathBuf>,
    pub child_pid: i32,
    pub abandoned: Option<AbandonedAsk>,
    /// No `initialize` response within this → Didn't start{Timeout} (World's
    /// R4); held user frames are dropped (Cancelled), never sent late.
    pub initialize_timeout_ms: u64,
}

/// Diagnostics counters (never content).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Counters {
    pub unknown_types: BTreeMap<String, u64>,
    pub unparseable: u64,
    pub too_large: u64,
    pub too_deep: u64,
    pub unanswerable: u64,
    pub unknown_ids: u64,
    pub unknown_terminal_reasons: u64,
    pub degraded_cards: u64,
    pub silent_approvals: u64,
}

/// How a cancel request resolves.
#[derive(Debug)]
pub enum CancelPlan {
    Done(bool),
    Wait(oneshot::Receiver<GildeReply>),
}

/// A message written later (held until ready, or the send-now fallback).
#[derive(Debug, Clone)]
struct HeldFrame {
    id: ClientMsgId,
    line: String,
    class: FrameClass,
}

/// The core of one Conversation session (one process).
pub struct Core {
    cfg: CoreConfig,
    patterns: Arc<PatternSource>,
    allow_session_local: bool,
    // readiness
    ready: bool,
    ready_at: Option<EpochMs>,
    spawned_at: EpochMs,
    handshake_id: Option<String>,
    // S1 facts
    base: TurnPhase,
    reported: TurnPhase,
    phase_since: EpochMs,
    last_turn_start_at: Option<EpochMs>,
    last_turn_end: Option<TurnEnd>,
    last_frame_at: EpochMs,
    facts_dirty: bool,
    frame_dirty: bool,
    last_facts_mono: u64,
    awaiting_turn: bool,
    // identity and meta
    session_id: Option<String>,
    init_seen: bool,
    model: Option<String>,
    permission_mode: Option<PermissionMode>,
    capabilities: HashSet<String>,
    init_version: Option<String>,
    models: Vec<ModelOption>,
    meta_dirty: bool,
    // Gilde facts
    stop_cause: Option<StopCause>,
    signals_sent: Vec<i32>,
    start_failure: Option<StartFailure>,
    gilde_dirty: bool,
    // approvals
    ledger: Ledger,
    gilde: GildeTable,
    pending: Vec<PendingRequest>,
    task_labels: HashMap<String, String>,
    denied_lines: HashSet<String>,
    // the current turn
    turn_seq: u64,
    /// The turn (by `turn_seq`) Gilde interrupted or denied-and-stopped [F-02].
    turn_interrupt: Option<u64>,
    /// A user frame was written while idle and its turn has not started yet.
    turn_starting: bool,
    turn_marker_seen: bool,
    turn_assistant_error: Option<String>,
    turn_retry_status: Option<i64>,
    interrupt_sent_mono: Option<u64>,
    // sending
    held: Vec<HeldFrame>,
    after_result: Option<(HeldFrame, u64)>,
    sent: VecDeque<(ClientMsgId, SendDelivery)>,
    queued: Vec<QueuedMessage>,
    lifecycle_seen: HashSet<String>,
    // rendering, spend, guards
    render: StreamRenderer,
    usage: StreamUsage,
    silent: SilentApprovalGuard,
    settings_guard: SettingsFileGuard,
    flip: ModeFlipGuard,
    counters: Counters,
    exited: bool,
    stopping: bool,
    init_deadline: u64,
    init_timed_out: bool,
}

fn now_facts_meta(cfg: &CoreConfig, model: &Option<String>, mode: Option<PermissionMode>, spawned_at: EpochMs) -> SessionMetaFacts {
    SessionMetaFacts {
        cwd: Some(cfg.cwd.clone()),
        started_at: Some(spawned_at),
        model: model.clone(),
        permission_mode: mode,
        binary: Some(cfg.binary.clone()),
        claude_version: Some(cfg.spawn_version.clone()),
        ..SessionMetaFacts::default()
    }
}

impl Core {
    /// A core for a just-spawned process; the effects announce the source, the
    /// Gilde facts (Starting), the metadata and idle stream facts.
    pub fn new(cfg: CoreConfig, patterns: Arc<PatternSource>, now: Now, fx: &mut Vec<Effect>) -> Core {
        let settings_guard = SettingsFileGuard::new(cfg.settings_paths.clone());
        let render = StreamRenderer::new(Some(cfg.cwd.clone()), cfg.home.clone());
        let usage = StreamUsage::new(cfg.launch.resumes());
        let session_id = cfg.launch.known_session_id().map(str::to_owned);
        let mut core = Core {
            allow_session_local: true,
            ready: false,
            ready_at: None,
            spawned_at: now.wall,
            handshake_id: None,
            base: TurnPhase::Idle,
            reported: TurnPhase::Idle,
            phase_since: now.wall,
            last_turn_start_at: None,
            last_turn_end: None,
            last_frame_at: now.wall,
            facts_dirty: true,
            frame_dirty: false,
            last_facts_mono: now.mono,
            awaiting_turn: false,
            session_id,
            init_seen: false,
            model: cfg.model.clone(),
            permission_mode: Some(cfg.permission_mode),
            capabilities: HashSet::new(),
            init_version: None,
            models: Vec::new(),
            meta_dirty: true,
            stop_cause: None,
            signals_sent: Vec::new(),
            start_failure: None,
            gilde_dirty: true,
            ledger: Ledger::new(),
            gilde: GildeTable::new(),
            pending: Vec::new(),
            task_labels: HashMap::new(),
            denied_lines: HashSet::new(),
            turn_seq: 0,
            turn_interrupt: None,
            turn_starting: false,
            turn_marker_seen: false,
            turn_assistant_error: None,
            turn_retry_status: None,
            interrupt_sent_mono: None,
            held: Vec::new(),
            after_result: None,
            sent: VecDeque::new(),
            queued: Vec::new(),
            lifecycle_seen: HashSet::new(),
            render,
            usage,
            silent: SilentApprovalGuard::new(),
            settings_guard,
            flip: ModeFlipGuard::new(),
            counters: Counters::default(),
            exited: false,
            stopping: false,
            init_deadline: now.mono.saturating_add(cfg.initialize_timeout_ms),
            init_timed_out: false,
            cfg,
            patterns,
        };
        fx.push(Effect::Content(StreamRenderer::source()));
        core.flush(now, fx);
        core
    }

    // -- accessors ---------------------------------------------------------------------

    pub fn flags(&self) -> &Flags {
        &self.cfg.flags
    }

    pub fn is_ready(&self) -> bool {
        self.ready
    }

    pub fn has_exited(&self) -> bool {
        self.exited
    }

    /// The vendor session id (known before `system/init` for new and
    /// continued sessions; from `system/init` for a copy).
    pub fn vendor_session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub fn models(&self) -> (Vec<ModelOption>, Option<String>) {
        (self.models.clone(), self.model.clone())
    }

    pub fn queued(&self) -> Vec<QueuedMessage> {
        self.queued.clone()
    }

    pub fn pending_cards(&self) -> Vec<crate::contract::interaction::PendingInteraction> {
        self.pending.iter().map(|p| p.card.clone()).collect()
    }

    pub fn counters(&self) -> Counters {
        let mut c = self.counters.clone();
        c.unknown_ids = self.ledger.unknown_ids;
        c.silent_approvals = self.silent.fired;
        c
    }

    pub fn capabilities(&self) -> &HashSet<String> {
        &self.capabilities
    }

    /// Whether a turn runs or a card is pending.
    pub fn turn_active(&self) -> bool {
        self.base == TurnPhase::Running || !self.pending.is_empty()
    }

    /// `allowSession` for this session after the runtime guards.
    pub fn allow_session(&self) -> bool {
        self.cfg.flags.allow_session && self.allow_session_local
    }

    /// The decision recorded for a request (`interaction_answer.previous`).
    pub fn decision_of(&self, req: &str) -> Option<DecisionKind> {
        match self.ledger.check(req) {
            AnswerCheck::AlreadyAnswered(k) => Some(k),
            _ => None,
        }
    }

    /// The version deviations are recorded against: the one `system/init`
    /// reports, else the spawn-time one (§3.10).
    pub fn effective_version(&self) -> String {
        self.init_version.clone().unwrap_or_else(|| self.cfg.spawn_version.clone())
    }

    fn base_path(&self) -> PathBase<'_> {
        PathBase { location: Some(self.cfg.cwd.as_str()), home: self.cfg.home.as_ref().map(AbsPath::as_str) }
    }

    // -- facts ---------------------------------------------------------------------------

    fn stream_facts(&self) -> StreamFacts {
        StreamFacts {
            phase: self.reported,
            phase_since: self.phase_since,
            pending: self.pending.iter().map(|p| p.card.clone()).collect(),
            last_turn_start_at: self.last_turn_start_at,
            last_turn_end: self.last_turn_end.clone(),
            last_frame_at: self.last_frame_at,
        }
    }

    fn gilde_facts(&self) -> GildeFacts {
        GildeFacts {
            spawned_at: self.spawned_at,
            mode: ControlMode::Conversation,
            ready_at: self.ready_at,
            stop_cause: self.stop_cause,
            signals_sent: self.signals_sent.clone(),
            start_failure: self.start_failure,
        }
    }

    fn touch_phase(&mut self, now: Now) {
        let r = if self.pending.is_empty() { self.base } else { TurnPhase::RequiresAction };
        if r != self.reported {
            self.reported = r;
            self.phase_since = now.wall;
        }
        self.facts_dirty = true;
    }

    /// The turn an interrupt sent now applies to: the running (or waiting)
    /// turn, or the one a just-written user frame is about to start.
    fn interrupt_target(&self) -> Option<u64> {
        if self.base == TurnPhase::Running || !self.pending.is_empty() {
            Some(self.turn_seq)
        } else if self.turn_starting {
            Some(self.turn_seq + 1)
        } else {
            None
        }
    }

    fn mark_interrupt(&mut self, target: Option<u64>) {
        if target.is_some() {
            self.turn_interrupt = target;
        }
    }

    fn set_base(&mut self, p: TurnPhase, now: Now) {
        if p == self.base {
            return;
        }
        if self.base == TurnPhase::Idle && p == TurnPhase::Running {
            self.last_turn_start_at = Some(now.wall);
            self.turn_seq += 1;
            self.turn_starting = false;
            if self.turn_interrupt.is_some_and(|t| t < self.turn_seq) {
                self.turn_interrupt = None;
            }
            self.turn_marker_seen = false;
            self.turn_assistant_error = None;
            self.turn_retry_status = None;
        }
        self.base = p;
        self.touch_phase(now);
    }

    /// Publish what changed (S1 transitions at once; a bare `last_frame_at`
    /// move at most once a second) and the spend report when due.
    pub fn flush(&mut self, now: Now, fx: &mut Vec<Effect>) {
        if self.gilde_dirty {
            self.gilde_dirty = false;
            fx.push(Effect::Evidence(SourceFacts::Gilde(self.gilde_facts())));
        }
        if self.meta_dirty {
            self.meta_dirty = false;
            fx.push(Effect::Evidence(SourceFacts::Meta(Box::new(now_facts_meta(
                &self.cfg,
                &self.model,
                self.permission_mode,
                self.spawned_at,
            )))));
        }
        let frame_due = self.frame_dirty && now.mono.saturating_sub(self.last_facts_mono) >= FRAME_FACTS_EVERY_MS;
        if self.facts_dirty || frame_due {
            self.facts_dirty = false;
            self.frame_dirty = false;
            self.last_facts_mono = now.mono;
            fx.push(Effect::Evidence(SourceFacts::Stream(self.stream_facts())));
        }
        if let Some(today) = self.usage.due(now.mono, self.exited) {
            fx.push(Effect::Evidence(SourceFacts::Usage(UsageFacts { source: UsageSource::Stream, today })));
        }
    }

    /// The next time [`Core::tick`] has work.
    pub fn next_deadline(&self) -> Option<u64> {
        let mut d: Option<u64> = None;
        let mut at = |t: u64| d = Some(d.map_or(t, |x: u64| x.min(t)));
        if self.frame_dirty {
            at(self.last_facts_mono + FRAME_FACTS_EVERY_MS);
        }
        if let Some(t) = self.usage.deadline() {
            at(t);
        }
        if let Some(t) = self.settings_guard.deadline() {
            at(t);
        }
        if let Some((_, t)) = &self.after_result {
            at(*t);
        }
        if !self.ready && !self.init_timed_out && !self.exited {
            at(self.init_deadline);
        }
        d
    }

    /// Deadline work: throttled facts, spend, the settings-file guard, the
    /// send-now fallback's 5 s bound.
    pub fn tick(&mut self, out: &mut dyn Outbox, now: Now, fx: &mut Vec<Effect>) {
        if !self.ready && !self.init_timed_out && now.mono >= self.init_deadline {
            // §6.1.2 step 6: Didn't start{Timeout}. The held prompt is not sent
            // later behind the user's back.
            self.init_timed_out = true;
            self.drop_held(fx);
        }
        if let Some(path) = self.settings_guard.check(now.mono) {
            self.allow_session_guard_tripped(&format!("a settings file changed after Always allow: {}", file_label(&path)), fx);
        }
        if self.after_result.as_ref().is_some_and(|(_, t)| now.mono >= *t) {
            self.release_after_result(out, now, fx);
        }
        self.flush(now, fx);
    }

    fn allow_session_guard_tripped(&mut self, note: &str, fx: &mut Vec<Effect>) {
        self.allow_session_local = false;
        // "Off at once" (PM-42, F-01): cards already pending lose button 2 too.
        for p in &mut self.pending {
            p.hide_allow_session();
        }
        self.facts_dirty = true;
        fx.push(Effect::Compat(CompatEvent::Deviation {
            capability: CompatCapability::Feature("allowSession".into()),
            note: note.to_owned(),
        }));
    }

    // -- the handshake ---------------------------------------------------------------------

    /// Write `initialize` (the first frame, §6.1.2 step 1).
    pub fn start(&mut self, out: &mut dyn Outbox) -> Result<(), WriteError> {
        let id = self.gilde.mint(&self.ledger);
        out.try_write(control_request(&id, &handshake::initialize_body()), FrameClass::Control)?;
        self.gilde.insert(id.clone(), GildeKind::Initialize, None);
        self.handshake_id = Some(id);
        Ok(())
    }

    fn on_initialize(&mut self, inner: &wire::ResponseInner<'_>, out: &mut dyn Outbox, now: Now, fx: &mut Vec<Effect>) {
        match parse_initialize(inner) {
            InitVerdict::Ok(info) => {
                self.ready = true;
                self.ready_at = Some(now.wall);
                self.gilde_dirty = true;
                if self.cfg.handshake_unchecked {
                    fx.push(Effect::Compat(CompatEvent::Handshake { ok: true, note: None }));
                }
                if info.pid.is_some_and(|p| p != i64::from(self.cfg.child_pid)) {
                    fx.push(Effect::Compat(CompatEvent::Note("initialize pid differs from the spawned process".into())));
                }
                if let Some(v) = info.version.as_deref().filter(|v| *v != self.cfg.spawn_version) {
                    let note = format!("initialize reports {v}, spawned {}", self.cfg.spawn_version);
                    fx.push(Effect::Hook(HookEvent::VersionNote(note.clone())));
                    fx.push(Effect::Compat(CompatEvent::Note(note)));
                }
                if let Some(m) = info.permission_mode {
                    if self.permission_mode != Some(m) {
                        self.permission_mode = Some(m);
                        self.meta_dirty = true;
                        fx.push(Effect::Hook(HookEvent::PermissionMode(Some(m))));
                    }
                } else if info.permission_mode_unknown {
                    self.mode_unknown(now, fx);
                }
                self.models = info.models.clone();
                if info.signed_out {
                    fx.push(Effect::Hook(HookEvent::SignedOut));
                    self.last_turn_end = Some(TurnEnd { at: now.wall, outcome: TurnOutcome::Error(ErrorSub::SignInNeeded) });
                    self.facts_dirty = true;
                }
                let mut carded = 0usize;
                for frame in &info.pending {
                    if let Ok(f) = serde_json::from_str::<wire::ControlRequestIn<'_>>(frame.get()) {
                        if f.subtype().as_deref() == Some("can_use_tool") {
                            if let (Some(id), Some(req)) = (f.id_string(), f.request) {
                                if self.card(&id, req, now, fx) {
                                    carded += 1;
                                }
                            }
                        }
                    }
                }
                if carded == 0 {
                    if let Some(ab) = self.cfg.abandoned.clone() {
                        // F-05: the session was waiting when it stopped and
                        // nothing was re-delivered (LP-04a): the Interaction
                        // block of that call shows it was abandoned.
                        let kind = match ab.tool_name.as_str() {
                            "AskUserQuestion" => InteractionKind::Question,
                            "ExitPlanMode" => InteractionKind::PlanApproval,
                            _ => InteractionKind::ToolApproval,
                        };
                        let rid = format!("ab_{}", crate::render::blocks::tool_use_id(&ab.tool_use_id));
                        let rid = if RequestId::is_valid_str(&rid) { rid } else { "ab_unknown".to_owned() };
                        fx.push(Effect::Content(self.render.interaction(&rid, kind, InteractionOutcome::Abandoned, now.wall)));
                    }
                }
                if self.cfg.launch.resumes() && self.cfg.flags.get_usage_live {
                    // [A19.5] the restored cost is the baseline; asked once,
                    // before the held prompt so it precedes any turn.
                    let _ = self.write_control(out, ControlBody::GetUsage {}, GildeKind::GetUsage { baseline: true }, None);
                }
                for h in std::mem::take(&mut self.held) {
                    let id = h.id.clone();
                    if out.try_write(h.line, h.class).is_ok() {
                        self.awaiting_turn = true;
                        self.turn_starting = true;
                    } else {
                        self.not_reading(fx);
                        for u in self.render.delivery(id.as_str(), Delivery::Cancelled) {
                            fx.push(Effect::Content(u));
                        }
                        self.unqueue(id.as_str(), fx);
                    }
                }
                self.touch_phase(now);
            }
            InitVerdict::CompatFailed(note) => {
                self.start_failure = Some(StartFailure::CompatFailed);
                self.gilde_dirty = true;
                fx.push(Effect::Compat(if self.cfg.handshake_unchecked {
                    CompatEvent::Handshake { ok: false, note: Some(note.to_owned()) }
                } else {
                    CompatEvent::Deviation { capability: CompatCapability::Conversation, note: note.to_owned() }
                }));
                fx.push(Effect::StopForCompat);
            }
        }
    }

    // -- stdout --------------------------------------------------------------------------------

    /// A stdout line longer than the cap was skipped (§6.1.8).
    pub fn on_too_large(&mut self, now: Now, fx: &mut Vec<Effect>) {
        self.counters.too_large += 1;
        self.last_frame_at = now.wall;
        self.frame_dirty = true;
        fx.push(Effect::Content(self.render.system(None, SystemNote::RowTooLarge, None, now.wall)));
        self.flush(now, fx);
    }

    /// One complete stdout line.
    pub fn on_line(&mut self, line: &[u8], out: &mut dyn Outbox, now: Now, fx: &mut Vec<Effect>) {
        if line.iter().all(u8::is_ascii_whitespace) {
            return;
        }
        if !depth_within(line, MAX_DEPTH) {
            self.counters.too_deep += 1;
            return;
        }
        let Ok(text) = std::str::from_utf8(line) else {
            self.counters.unparseable += 1;
            return;
        };
        let Ok(head) = serde_json::from_str::<wire::Head<'_>>(text) else {
            self.counters.unparseable += 1;
            return;
        };
        self.last_frame_at = now.wall;
        self.frame_dirty = true;
        let ty = head.ty.as_deref().unwrap_or("");
        if !self.cfg.flags.session_state_events && self.awaiting_turn && !matches!(ty, "control_response" | "command_lifecycle") {
            // P9 fallback: Running from the first frame after a user frame.
            self.awaiting_turn = false;
            self.set_base(TurnPhase::Running, now);
        }
        match ty {
            "control_request" => self.on_control_request(text, out, now, fx),
            "control_response" => self.on_control_response(text, out, now, fx),
            "control_cancel_request" => self.on_cancel(text, out, now, fx),
            "command_lifecycle" => self.on_lifecycle(text, fx),
            "system" => self.on_system(head.subtype.as_deref().unwrap_or(""), text, now, fx),
            "stream_event" => self.on_stream_event(text, now, fx),
            "assistant" => self.on_assistant(text, now, fx),
            "user" => self.on_user(text, now, fx),
            "result" => self.on_result(text, out, now, fx),
            "rate_limit_event" => {
                if let Ok(ev) = serde_json::from_str::<wire::RateLimitIn>(text) {
                    if let Some(q) = handshake::quota_from_rate_limit(&ev, now.wall) {
                        fx.push(Effect::Hook(HookEvent::Quota(q)));
                    }
                }
            }
            "keep_alive" => {}
            other => self.count_unknown(other),
        }
        self.flush(now, fx);
    }

    fn count_unknown(&mut self, name: &str) {
        let key = display(if name.is_empty() { "?" } else { name }, TextCap::Chars(64));
        if self.counters.unknown_types.len() < 64 || self.counters.unknown_types.contains_key(&key) {
            *self.counters.unknown_types.entry(key.clone()).or_insert(0) += 1;
        }
        tracing::debug!(target: "convo", type_name = %key, "unknown frame ignored");
    }

    fn on_control_request(&mut self, text: &str, out: &mut dyn Outbox, now: Now, fx: &mut Vec<Effect>) {
        let Ok(f) = serde_json::from_str::<wire::ControlRequestIn<'_>>(text) else {
            self.counters.unparseable += 1;
            return;
        };
        let subtype = f.subtype();
        let Some(id) = f.id_string().filter(|id| !id.is_empty() && id.len() <= MAX_WIRE_ID) else {
            // §6.1.9: cannot be answered; conversation Failed for new sessions
            // of this version; the band-4 safety net catches the wait.
            self.counters.unanswerable += 1;
            let subtype_label = display(subtype.as_deref().unwrap_or("?"), TextCap::Chars(64));
            tracing::warn!(target: "convo", subtype = %subtype_label, "control_request without a usable request_id");
            fx.push(Effect::Compat(CompatEvent::Deviation {
                capability: CompatCapability::Conversation,
                note: "control_request without a string request_id".into(),
            }));
            let notice = Notice::new("convo.unanswerable");
            fx.push(Effect::Hook(HookEvent::Notice(notice.clone())));
            fx.push(Effect::Content(crate::contract::services::ContentUpdate::Source {
                source: crate::contract::content::ConvoSourceKind::Stream,
                note: Some(notice),
            }));
            return;
        };
        match subtype.as_deref() {
            Some("can_use_tool") => {
                if let Some(req) = f.request {
                    self.card(&id, req, now, fx);
                }
            }
            // Never answered: Gilde declared no dialog kinds (R4 rule 9).
            Some("request_user_dialog") => self.count_unknown("request_user_dialog"),
            // elicitation, hook_callback, mcp_message and anything else.
            _ => {
                if out.try_write(control_response_error(&id, UNSUPPORTED_ERROR), FrameClass::Control).is_err() {
                    tracing::warn!(target: "convo", "could not queue an unsupported reply");
                }
            }
        }
    }

    /// Card one `can_use_tool` (dedupe by `request_id`); `true` if new.
    fn card(&mut self, wire_id: &str, request: &RawValue, now: Now, fx: &mut Vec<Effect>) -> bool {
        if wire_id.is_empty() || wire_id.len() > MAX_WIRE_ID {
            self.counters.unanswerable += 1;
            return false;
        }
        let card_id = card_id_of(wire_id);
        let id = card_id.as_str();
        if self.ledger.contains(id) {
            return false;
        }
        if self.pending.len() >= MAX_PENDING {
            // Not shown; the band-4 safety net catches the wait.
            self.counters.unanswerable += 1;
            return false;
        }
        let req = {
            let labels = &self.task_labels;
            let label = |a: &str| labels.get(a).cloned();
            let ctx = CardContext {
                allow_session: self.allow_session(),
                plan_answer: self.cfg.flags.plan_answer,
                sandbox_ask: self.cfg.flags.sandbox_ask,
                base: self.base_path(),
                agent_label: &label,
                received_at: now.wall,
                received_mono: now.mono,
            };
            let mut r = build_request(id, request, &ctx);
            r.wire_id = wire_id.to_owned();
            r
        };
        if let Seen::Duplicate(_) = self.ledger.observe(id, req.tool_use_id.as_deref()) {
            return false;
        }
        if req.card.degraded {
            self.counters.degraded_cards += 1;
            fx.push(Effect::Compat(CompatEvent::Note("unreadable permission request".into())));
        }
        if let Some(t) = &req.tool_use_id {
            self.silent.on_prompt(&crate::render::blocks::tool_use_id(t));
        }
        fx.push(Effect::Content(self.render.interaction(id, req.card.kind, InteractionOutcome::Pending, now.wall)));
        self.pending.push(req);
        self.touch_phase(now);
        true
    }

    fn on_control_response(&mut self, text: &str, out: &mut dyn Outbox, now: Now, fx: &mut Vec<Effect>) {
        let Ok(f) = serde_json::from_str::<wire::ControlResponseIn<'_>>(text) else {
            self.counters.unparseable += 1;
            return;
        };
        let Some(inner) = f.response.and_then(|r| serde_json::from_str::<wire::ResponseInner<'_>>(r.get()).ok()) else {
            self.counters.unparseable += 1;
            return;
        };
        let Some(id) = inner.id_string() else {
            self.ledger.unknown_ids += 1;
            return;
        };
        // F-51: a CLI-originated id is the echo of Gilde's own answer, never a
        // reply to one of Gilde's requests.
        let card_id = card_id_of(&id);
        if self.ledger.contains(&card_id) {
            if inner.is_success() {
                self.ledger.ack(&card_id);
            }
            return;
        }
        let Some(p) = self.gilde.take(&id) else {
            self.ledger.unknown_ids += 1;
            return;
        };
        let ok = inner.is_success();
        let reply = match &p.kind {
            GildeKind::Initialize => {
                self.on_initialize(&inner, out, now, fx);
                GildeReply::Ok
            }
            GildeKind::Interrupt { turn, .. } => {
                if ok {
                    // The receipt arrived: the turn it was sent for ends
                    // Interrupted [F-02] (a stale receipt is cleared when the
                    // next turn starts).
                    let t = turn.or_else(|| self.interrupt_target());
                    self.mark_interrupt(t);
                }
                if ok {
                    GildeReply::Ok
                } else {
                    GildeReply::Refused
                }
            }
            GildeKind::GetUsage { baseline } => {
                if ok && *baseline {
                    if let Some(c) = inner.response.and_then(handshake::session_cost_from_usage) {
                        self.usage.set_baseline(&crate::follow::tail::local_day(now.wall), c);
                    }
                }
                let q = if ok { inner.response.and_then(|r| handshake::quota_from_usage(r, now.wall)) } else { None };
                if let Some(q) = &q {
                    fx.push(Effect::Hook(HookEvent::Quota(q.clone())));
                }
                if ok {
                    GildeReply::Usage(q)
                } else {
                    GildeReply::Refused
                }
            }
            GildeKind::SetModel | GildeKind::SetPermissionMode | GildeKind::CancelAsync { .. } => {
                if ok {
                    GildeReply::Ok
                } else {
                    GildeReply::Refused
                }
            }
        };
        if let Some(w) = p.waiter {
            let _ = w.send(reply);
        }
    }

    fn on_cancel(&mut self, text: &str, out: &mut dyn Outbox, now: Now, fx: &mut Vec<Effect>) {
        let Ok(c) = serde_json::from_str::<wire::CancelIn>(text) else {
            self.counters.unparseable += 1;
            return;
        };
        let Some(id) = c.request_id.as_ref().and_then(|v| v.as_str()).map(str::to_owned) else {
            self.ledger.unknown_ids += 1;
            return;
        };
        let id = card_id_of(&id);
        let Some(prev) = self.ledger.withdraw(&id) else { return };
        // An answer still waiting in the stdin queue is dropped.
        out.withdraw(&id);
        if matches!(prev, crate::convo::ledger::LedgerState::Pending | crate::convo::ledger::LedgerState::Answered(_)) {
            // "Answered elsewhere." — no flash: the facts go out once, after this frame.
            self.pending.retain(|p| p.request_id != id);
            fx.push(Effect::Content(self.render.interaction_outcome(&id, InteractionOutcome::Withdrawn)));
            self.touch_phase(now);
        }
    }

    fn on_lifecycle(&mut self, text: &str, fx: &mut Vec<Effect>) {
        let Ok(l) = serde_json::from_str::<wire::LifecycleIn>(text) else { return };
        let (Some(uuid), Some(state)) = (l.command_uuid, l.state) else { return };
        if !self.render.is_ours(&uuid) {
            return;
        }
        self.lifecycle_seen.insert(uuid.clone());
        let d = match state.as_str() {
            "queued" => Some(Delivery::Queued),
            "started" => Some(Delivery::Delivered),
            "cancelled" => Some(Delivery::Cancelled),
            _ => None,
        };
        if let Some(d) = d {
            for u in self.render.delivery(&uuid, d) {
                fx.push(Effect::Content(u));
            }
            if d != Delivery::Queued {
                self.unqueue(&uuid, fx);
            }
        }
    }

    fn unqueue(&mut self, uuid: &str, fx: &mut Vec<Effect>) {
        let before = self.queued.len();
        self.queued.retain(|q| q.client_msg_id.as_str() != uuid);
        if self.queued.len() != before {
            fx.push(Effect::Hook(HookEvent::Queued(self.queued.clone())));
        }
    }

    fn on_system(&mut self, subtype: &str, text: &str, now: Now, fx: &mut Vec<Effect>) {
        match subtype {
            "init" => {
                let Ok(i) = serde_json::from_str::<wire::InitIn>(text) else { return };
                if !self.init_seen {
                    self.init_seen = true;
                    // Only the first init sets identity (§6.1.2 step 4).
                    if let Some(sid) = i.session_id.filter(|s| crate::contract::ids::is_uuid(s, None)) {
                        match &self.session_id {
                            Some(mine) if *mine != sid => {
                                fx.push(Effect::Compat(CompatEvent::Note("system/init session_id differs from --session-id".into())));
                                self.session_id = Some(sid);
                            }
                            None => self.session_id = Some(sid),
                            _ => {}
                        }
                    }
                }
                if let Some(m) = i.model.map(|m| display(&m, TextCap::Chars(128))).filter(|m| !m.is_empty()) {
                    if self.model.as_ref() != Some(&m) {
                        self.model = Some(m);
                        self.meta_dirty = true;
                    }
                }
                if let Some(raw) = i.permission_mode.as_deref() {
                    match permission_mode_of(raw) {
                        Some(pm) => self.mode_seen(pm, now, fx),
                        None => self.mode_unknown(now, fx),
                    }
                }
                if let Some(caps) = i.capabilities {
                    self.capabilities = handshake::string_list(Some(caps)).into_iter().collect();
                }
                if let Some(v) = i.claude_code_version.filter(|v| crate::contract::compat::parse_version(v).is_some()) {
                    if v != self.cfg.spawn_version && self.init_version.as_deref() != Some(v.as_str()) {
                        let note = format!("system/init reports {v}, spawned {}", self.cfg.spawn_version);
                        fx.push(Effect::Hook(HookEvent::VersionNote(note.clone())));
                        fx.push(Effect::Compat(CompatEvent::Note(note)));
                    }
                    self.init_version = Some(v);
                }
                // An unprompted turn starts with init alone (§6.1.5).
                self.awaiting_turn = false;
                self.set_base(TurnPhase::Running, now);
            }
            "session_state_changed" => {
                if !self.cfg.flags.session_state_events {
                    return;
                }
                let Ok(s) = serde_json::from_str::<wire::SessionStateIn>(text) else { return };
                match s.state.as_deref() {
                    Some("idle") => self.set_base(TurnPhase::Idle, now),
                    Some("running" | "requires_action") => {
                        self.awaiting_turn = false;
                        self.set_base(TurnPhase::Running, now);
                    }
                    _ => {}
                }
            }
            "status" => {
                if let Ok(s) = serde_json::from_str::<wire::StatusIn>(text) {
                    if let Some(raw) = s.permission_mode.as_deref() {
                        match permission_mode_of(raw) {
                            Some(pm) => self.mode_seen(pm, now, fx),
                            None => self.mode_unknown(now, fx),
                        }
                    }
                }
            }
            "task_started" | "task_updated" | "task_progress" | "task_notification" => {
                let Ok(t) = serde_json::from_str::<wire::TaskIn>(text) else { return };
                let Some(task_id) = t.task_id.filter(|id| !id.is_empty() && id.len() <= 128) else { return };
                if subtype == "task_started" {
                    if let Some(d) = t.description.as_deref() {
                        let label = display(d, TextCap::Chars(60));
                        if self.task_labels.len() > 1_024 {
                            self.task_labels.clear();
                        }
                        self.task_labels.insert(task_id.clone(), label.clone());
                        if let Some(tu) = t.tool_use_id.as_deref() {
                            self.task_labels.insert(tu.to_owned(), label);
                        }
                    }
                }
                let status_text = t.status.or_else(|| t.patch.and_then(|p| p.status));
                let status = claude_stream::task_status(subtype, status_text.as_deref());
                let label = self.task_labels.get(&task_id).cloned();
                fx.push(Effect::Content(self.render.task_line(&task_id, status, label.as_deref(), now.wall)));
            }
            "api_retry" => {
                let Ok(r) = serde_json::from_str::<wire::ApiRetryIn>(text) else { return };
                if r.error_status.is_some() {
                    self.turn_retry_status = r.error_status;
                }
                let attempt = r.attempt.unwrap_or(0).min(u64::from(u32::MAX)) as u32;
                let key = format!("retry_{}", self.turn_seq);
                fx.push(Effect::Content(self.render.system(Some(&key), SystemNote::ApiRetry { attempt }, None, now.wall)));
            }
            "permission_denied" => {
                let Ok(p) = serde_json::from_str::<wire::PermissionDeniedIn>(text) else { return };
                if let Some(t) = p.tool_use_id.as_deref() {
                    let tu = crate::render::blocks::tool_use_id(t);
                    self.silent.on_prompt(&tu);
                    self.denied_lines.insert(tu);
                }
                let tool = display(p.tool_name.as_deref().unwrap_or("?"), TextCap::Chars(64));
                fx.push(Effect::Content(self.render.system(None, SystemNote::PermissionDenied { tool }, None, now.wall)));
            }
            "compact_boundary" => {
                fx.push(Effect::Content(self.render.system(None, SystemNote::CompactBoundary, None, now.wall)));
            }
            "thinking_tokens" | "background_tasks_changed" => {}
            other => self.count_unknown(&format!("system/{other}")),
        }
    }

    /// A33.3: the CLI reported a mode Gilde does not know: the session's mode
    /// is unknown (`None`), never the previous value. A change right after
    /// Always allow trips the F-01 guard like any other mode change.
    fn mode_unknown(&mut self, now: Now, fx: &mut Vec<Effect>) {
        let Some(_) = self.permission_mode.take() else { return };
        self.meta_dirty = true;
        fx.push(Effect::Hook(HookEvent::PermissionMode(None)));
        if self.flip.on_mode_change(now.mono) {
            self.allow_session_guard_tripped("permission mode changed after Always allow", fx);
        }
    }

    fn mode_seen(&mut self, pm: PermissionMode, now: Now, fx: &mut Vec<Effect>) {
        if self.permission_mode == Some(pm) {
            return;
        }
        let was = self.permission_mode;
        self.permission_mode = Some(pm);
        self.meta_dirty = true;
        fx.push(Effect::Hook(HookEvent::PermissionMode(Some(pm))));
        if was.is_some() && self.flip.on_mode_change(now.mono) {
            // F-01: a mode change right after Always allow that the user did not ask for.
            self.allow_session_guard_tripped("permission mode changed after Always allow", fx);
        }
    }

    fn on_stream_event(&mut self, text: &str, now: Now, fx: &mut Vec<Effect>) {
        let Ok(se) = serde_json::from_str::<wire::StreamEventIn<'_>>(text) else { return };
        let Some(raw) = se.event else { return };
        let Ok(ev) = serde_json::from_str::<EventIn<'_>>(raw.get()) else { return };
        let day = |w| crate::follow::tail::local_day(w);
        match ev.ty.as_deref() {
            Some("message_start") => {
                if let Some(m) = &ev.message {
                    if let (Some(id), Some(u)) = (&m.id, &m.usage) {
                        self.usage.message(&day(now.wall), id, &u.tokens());
                    }
                }
            }
            Some("message_delta") => {
                if let (Some(u), Some(id)) = (&ev.usage, self.current_msg_id()) {
                    self.usage.message(&day(now.wall), &id, &u.tokens());
                }
            }
            _ => {}
        }
        for u in self.render.stream_event(&ev, now.wall) {
            fx.push(Effect::Content(u));
        }
    }

    fn current_msg_id(&self) -> Option<String> {
        self.render.current_message().map(str::to_owned)
    }

    fn on_assistant(&mut self, text: &str, now: Now, fx: &mut Vec<Effect>) {
        let Ok(f) = serde_json::from_str::<wire::MessageFrameIn<'_>>(text) else { return };
        if let Some(code) = f.error.as_ref().and_then(|e| e.as_str()) {
            self.turn_assistant_error = Some(display(code, TextCap::Chars(64)));
        }
        let Some(raw) = f.message else { return };
        let Ok(m) = serde_json::from_str::<AssistantMsgIn>(raw.get()) else { return };
        if let (Some(id), Some(u)) = (&m.id, &m.usage) {
            self.usage.message(&crate::follow::tail::local_day(now.wall), id, &u.tokens());
        }
        let (updates, facts) = self.render.assistant(&m, f.parent(), now.wall);
        for u in updates {
            fx.push(Effect::Content(u));
        }
        for (id, name, key) in facts.tool_uses {
            self.silent.on_tool_use(&id, &name, key.as_deref(), self.permission_mode);
        }
    }

    fn on_user(&mut self, text: &str, now: Now, fx: &mut Vec<Effect>) {
        let Ok(f) = serde_json::from_str::<wire::MessageFrameIn<'_>>(text) else { return };
        let Some(msg) = f.message else { return };
        if f.is_replay == Some(true) {
            match f.uuid.as_deref() {
                Some(u) if self.render.is_ours(u) => {
                    // Fallback for `Delivered` when no lifecycle frame came (§6.1.3).
                    if !self.lifecycle_seen.contains(u) {
                        let u = u.to_owned();
                        for x in self.render.delivery(&u, Delivery::Delivered) {
                            fx.push(Effect::Content(x));
                        }
                        self.unqueue(&u, fx);
                    }
                }
                other => {
                    for x in self.render.foreign_replay(msg, other, now.wall) {
                        fx.push(Effect::Content(x));
                    }
                }
            }
            return;
        }
        let (updates, facts) = self.render.user(msg, f.uuid.as_deref(), f.tool_use_result, now.wall);
        for u in updates {
            fx.push(Effect::Content(u));
        }
        if facts.interrupt_marker {
            self.turn_marker_seen = true;
        }
        for id in facts.tool_results {
            if let Some(tool) = self.silent.on_result(&id) {
                tracing::info!(target: "convo", tool = %tool, "a guarded tool ran without a seen permission request");
                fx.push(Effect::Hook(HookEvent::ApprovalUnconfirmed(tool.clone())));
                fx.push(Effect::Compat(CompatEvent::Note(format!("approval channel unconfirmed: {tool}"))));
            }
        }
    }

    fn on_result(&mut self, text: &str, out: &mut dyn Outbox, now: Now, fx: &mut Vec<Effect>) {
        let Ok(r) = serde_json::from_str::<wire::ResultIn<'_>>(text) else {
            self.counters.unparseable += 1;
            return;
        };
        let is_error_hint = r.is_error.unwrap_or(false) || r.subtype.as_deref().is_some_and(|s| s != "success");
        let result_text: String = match r.result {
            Some(raw) if is_error_hint && raw.get().len() <= 64 * 1024 => serde_json::from_str::<String>(raw.get()).unwrap_or_default(),
            _ => String::new(),
        };
        let patterns = self.patterns.get(now.mono);
        let (outcome, unknown) = classify(
            &ResultFacts {
                terminal_reason: r.terminal_reason.as_deref(),
                is_error: r.is_error,
                subtype: r.subtype.as_deref(),
                result_text: &result_text,
                api_error_status: r.api_error_status,
                assistant_error: self.turn_assistant_error.as_deref(),
                retry_status: self.turn_retry_status,
                interrupted_by_gilde: self.turn_interrupt == Some(self.turn_seq),
            },
            &patterns,
            self.cfg.account_id.as_ref(),
        );
        if unknown {
            self.counters.unknown_terminal_reasons += 1;
        }
        let day = crate::follow::tail::local_day(now.wall);
        if let Some(c) = r.total_cost_usd {
            self.usage.cost(&day, c);
        }
        for u in self.render.close_open(now.wall) {
            fx.push(Effect::Content(u));
        }
        // Several sends may be consumed by one turn (§6.1.3).
        for v in r.user_message_uuids.iter().flatten() {
            if let Some(u) = v.as_str() {
                if self.render.is_ours(u) && self.render.delivery_of(u) != Some(Delivery::Cancelled) {
                    let u = u.to_owned();
                    for x in self.render.delivery(&u, Delivery::Delivered) {
                        fx.push(Effect::Content(x));
                    }
                    self.unqueue(&u, fx);
                }
            }
        }
        // F-26: one line per denial Gilde has no Interaction block for.
        for d in r.permission_denials.iter().flatten().take(64) {
            let Some(t) = d.tool_use_id.as_deref() else { continue };
            let tu = crate::render::blocks::tool_use_id(t);
            if self.ledger.has_tool_use(t) || self.ledger.has_tool_use(&tu) || self.denied_lines.contains(&tu) {
                continue;
            }
            self.denied_lines.insert(tu);
            let tool = display(d.tool_name.as_deref().unwrap_or("?"), TextCap::Chars(64));
            fx.push(Effect::Content(self.render.system(None, SystemNote::PermissionDenied { tool }, None, now.wall)));
        }
        match &outcome {
            TurnOutcome::Interrupted if !self.turn_marker_seen => {
                fx.push(Effect::Content(self.render.system(None, SystemNote::Interrupted, None, now.wall)));
            }
            TurnOutcome::Error(sub) => {
                let line = matches!(sub, ErrorSub::AgentError).then_some(result_text.as_str()).filter(|t| !t.is_empty());
                fx.push(Effect::Content(self.render.system(None, SystemNote::Error { sub: sub.clone() }, line, now.wall)));
            }
            _ => {}
        }
        self.last_turn_end = Some(TurnEnd { at: now.wall, outcome });
        self.turn_interrupt = None;
        self.turn_starting = false;
        self.interrupt_sent_mono = None;
        self.awaiting_turn = false;
        self.set_base(TurnPhase::Idle, now);
        self.facts_dirty = true;
        fx.push(Effect::TurnEnded);
        if self.after_result.is_some() {
            self.release_after_result(out, now, fx);
        }
    }

    // -- commands ---------------------------------------------------------------------------------

    fn write_control(&mut self, out: &mut dyn Outbox, body: ControlBody, kind: GildeKind, waiter: Option<oneshot::Sender<GildeReply>>) -> Result<(), IpcError> {
        let id = self.gilde.mint(&self.ledger);
        match out.try_write(control_request(&id, &body), FrameClass::Control) {
            Ok(()) => {
                self.gilde.insert(id, kind, waiter);
                Ok(())
            }
            Err(e) => Err(self.write_error(e)),
        }
    }

    fn write_error(&mut self, e: WriteError) -> IpcError {
        match e {
            WriteError::Full => IpcError::busy(1_000).param("reason", "stdinFull"),
            WriteError::TooLarge => IpcError::invalid_argument("text", "size"),
            WriteError::Closed => IpcError::unsupported("send"),
        }
    }

    /// The stdin queue is full: the evidence line "claude is not reading input."
    fn not_reading(&self, fx: &mut Vec<Effect>) {
        let notice = Notice::new("convo.notReading");
        fx.push(Effect::Hook(HookEvent::Notice(notice.clone())));
        fx.push(Effect::Content(crate::contract::services::ContentUpdate::Source {
            source: crate::contract::content::ConvoSourceKind::Stream,
            note: Some(notice),
        }));
    }

    /// `turn_send` (§6.1.3).
    pub fn send(&mut self, msg: OutgoingMessage, out: &mut dyn Outbox, now: Now, fx: &mut Vec<Effect>) -> Result<SendDelivery, IpcError> {
        if let Some((_, d)) = self.sent.iter().find(|(id, _)| *id == msg.client_msg_id) {
            return Ok(*d);
        }
        if self.exited || self.stopping {
            return Err(IpcError::unsupported("send"));
        }
        let min_text = if msg.images.is_empty() { 1 } else { 0 };
        crate::contract::validate::text_range("text", &msg.text, min_text, crate::contract::ipc::MAX_PROMPT_BYTES)
            .map_err(IpcError::from)?;
        let now_capable = self.cfg.flags.now_priority && self.capabilities.contains("interrupt_send_now_v1");
        let wants_now = msg.priority == SendPriority::Now;
        let shape = UserFrameShape {
            now: wants_now && now_capable,
            origin: self.cfg.flags.handshake_shape,
            empty_session_id: !self.cfg.flags.handshake_shape,
        };
        let line = user_frame_with_images(&msg.text, &msg.images, msg.client_msg_id.as_str(), shape);
        let class = if msg.images.is_empty() { FrameClass::User } else { FrameClass::UserImages };
        if line.len() > class.cap() {
            return Err(IpcError::invalid_argument(if msg.images.is_empty() { "text" } else { "images" }, "size"));
        }
        let active = self.turn_active();
        let delivery = if !self.ready {
            // Held until the `initialize` response (§6.1.2 step 1).
            self.held.push(HeldFrame { id: msg.client_msg_id.clone(), line, class });
            SendDelivery::Queued
        } else if wants_now && !now_capable && active && self.after_result.is_none() {
            // P9 fallback: interrupt, wait for the turn's result (≤ 5 s), then send.
            let target = self.interrupt_target();
            self.write_control(out, ControlBody::Interrupt { cancel_queued: None }, GildeKind::Interrupt { discard_queued: false, turn: target }, None)
                .inspect_err(|e| {
                    if e.code == crate::contract::error::ErrorCode::Busy {
                        self.not_reading(fx);
                    }
                })?;
            self.mark_interrupt(target);
            self.interrupt_sent_mono = Some(now.mono);
            self.after_result = Some((HeldFrame { id: msg.client_msg_id.clone(), line, class }, now.mono + SEND_NOW_WAIT_MS));
            SendDelivery::Queued
        } else {
            if let Err(e) = out.try_write(line, class) {
                if e == WriteError::Full {
                    self.not_reading(fx);
                }
                return Err(self.write_error(e));
            }
            if !active {
                self.turn_starting = true;
                if !self.cfg.flags.session_state_events {
                    self.awaiting_turn = true;
                }
            }
            if active { SendDelivery::Queued } else { SendDelivery::Sent }
        };
        let d = (delivery == SendDelivery::Queued).then_some(Delivery::Queued);
        // Kept for the view: the sent block shows what was attached.
        let shown: Vec<ImageRef> = msg.images.iter().filter_map(|i| images::global().put(&i.media_type, &i.data)).collect();
        for u in self.render.user_sent(&msg.client_msg_id, &msg.text, shown, now.wall, d) {
            fx.push(Effect::Content(u));
        }
        if delivery == SendDelivery::Queued {
            self.queued.push(QueuedMessage { client_msg_id: msg.client_msg_id.clone(), text: msg.text.clone(), queued_at: now.wall });
            fx.push(Effect::Hook(HookEvent::Queued(self.queued.clone())));
        }
        self.sent.push_back((msg.client_msg_id, delivery));
        while self.sent.len() > SENT_KEPT {
            self.sent.pop_front();
        }
        self.flush(now, fx);
        Ok(delivery)
    }

    fn release_after_result(&mut self, out: &mut dyn Outbox, now: Now, fx: &mut Vec<Effect>) {
        let Some((h, _)) = self.after_result.take() else { return };
        if self.exited || self.stopping {
            return;
        }
        if out.try_write(h.line, h.class).is_err() {
            self.not_reading(fx);
            for u in self.render.delivery(h.id.as_str(), Delivery::Cancelled) {
                fx.push(Effect::Content(u));
            }
            self.unqueue(h.id.as_str(), fx);
        } else {
            self.turn_starting = true;
            if !self.cfg.flags.session_state_events {
                self.awaiting_turn = true;
            }
        }
        self.flush(now, fx);
    }

    /// `turn_cancel_queued` (§6.1.3).
    pub fn cancel_queued(&mut self, id: &ClientMsgId, out: &mut dyn Outbox, fx: &mut Vec<Effect>) -> Result<CancelPlan, IpcError> {
        if !self.queued.iter().any(|q| q.client_msg_id == *id) {
            return if self.sent.iter().any(|(s, _)| s == id) { Ok(CancelPlan::Done(false)) } else { Err(IpcError::not_found("message")) };
        }
        let mut local = false;
        if let Some(pos) = self.held.iter().position(|h| h.id == *id) {
            self.held.remove(pos);
            local = true;
        }
        if self.after_result.as_ref().is_some_and(|(h, _)| h.id == *id) {
            self.after_result = None;
            local = true;
        }
        if local {
            for u in self.render.delivery(id.as_str(), Delivery::Cancelled) {
                fx.push(Effect::Content(u));
            }
            self.unqueue(id.as_str(), fx);
            return Ok(CancelPlan::Done(true));
        }
        let (tx, rx) = oneshot::channel();
        self.write_control(
            out,
            ControlBody::CancelAsyncMessage { message_uuid: id.as_str().to_owned() },
            GildeKind::CancelAsync { message_uuid: id.as_str().to_owned() },
            Some(tx),
        )?;
        Ok(CancelPlan::Wait(rx))
    }

    /// `interaction_answer` (§4.2.3, §6.1.4) [C12]: the outcome and, for
    /// `alreadyAnswered`, the earlier decision.
    pub fn answer(
        &mut self,
        req: &RequestId,
        d: &Decision,
        out: &mut dyn Outbox,
        now: Now,
        fx: &mut Vec<Effect>,
    ) -> Result<(AnswerOutcome, Option<DecisionKind>), IpcError> {
        let id = req.as_str();
        match self.ledger.check(id) {
            AnswerCheck::NotFound => return Err(IpcError::not_found("request")),
            AnswerCheck::Withdrawn => return Ok((AnswerOutcome::Withdrawn, None)),
            AnswerCheck::AlreadyAnswered(k) => return Ok((AnswerOutcome::AlreadyAnswered, Some(k))),
            AnswerCheck::Pending => {}
        }
        let Some(idx) = self.pending.iter().position(|p| p.request_id == id) else {
            return Ok((AnswerOutcome::Withdrawn, None));
        };
        d.validate().map_err(|i| IpcError::invalid_argument(&prefixed("decision", &i.field), i.rule))?;
        d.fits(&self.pending[idx].card).map_err(|m| match m {
            DecisionMisfit::Invalid(i) => IpcError::invalid_argument(&i.field, i.rule),
            DecisionMisfit::Unsupported(cap) => IpcError::unsupported(cap),
        })?;
        if let Decision::DenyWithMessage { message } | Decision::KeepPlanning { message } = d {
            if message.trim().is_empty() {
                return Err(IpcError::invalid_argument("decision.message", "required"));
            }
        }
        if matches!(d, Decision::AllowSession) && !self.allow_session() {
            return Err(IpcError::unsupported("allowSessionHidden"));
        }
        if now.mono.saturating_sub(self.pending[idx].received_mono) < TOO_FRESH_MS {
            return Err(IpcError::conflict("tooFresh"));
        }
        let target = self.interrupt_target();
        let ans = decision_payload(&self.pending[idx], d, self.cfg.flags.deny_and_stop, self.cfg.flags.free_text_answer).map_err(|e| match e {
            PayloadError::Unsupported(cap) => IpcError::unsupported(cap),
            PayloadError::Schema(rule) => {
                tracing::error!(target: "convo", rule, "a decision payload failed the R4 schema; not sent");
                IpcError::internal(&uuid::Uuid::new_v4().simple().to_string())
            }
        })?;
        let wire_id = self.pending[idx].wire_id.clone();
        let class = if ans.echoes_input { FrameClass::Answer } else { FrameClass::Control };
        match out.try_write_tagged(control_response_success(&wire_id, &ans.payload), class, id) {
            Ok(()) => {}
            Err(WriteError::Closed) => return Ok((AnswerOutcome::Withdrawn, None)),
            Err(e) => {
                if e == WriteError::Full {
                    self.not_reading(fx);
                }
                return Err(self.write_error(e));
            }
        }
        let req_done = self.pending.remove(idx);
        self.ledger.mark_answered(id, d.kind());
        fx.push(Effect::Content(self.render.interaction_outcome(id, ans.outcome)));
        match d {
            Decision::DenyAndStop => {
                // [F-02]: this turn ends Interrupted whatever its subtype.
                self.mark_interrupt(target);
                self.interrupt_sent_mono = Some(now.mono);
                if ans.then_interrupt {
                    let _ = self.write_control(
                        out,
                        ControlBody::Interrupt { cancel_queued: None },
                        GildeKind::Interrupt { discard_queued: false, turn: target },
                        None,
                    );
                }
            }
            Decision::AllowSession => {
                self.settings_guard.arm(now.mono);
                self.flip.on_allow_session(now.mono);
                for rules in &req_done.session_rules {
                    for r in rules {
                        self.silent.on_session_rule(&r.tool_name, r.rule_content.as_deref());
                    }
                }
            }
            Decision::ApprovePlan => self.flip.on_user_request(now.mono),
            _ => {}
        }
        self.touch_phase(now);
        self.flush(now, fx);
        Ok((AnswerOutcome::Answered, None))
    }

    /// `turn_interrupt` (§6.1.7).
    pub fn interrupt(&mut self, discard_queued: bool, out: &mut dyn Outbox, now: Now, fx: &mut Vec<Effect>) -> Result<InterruptOutcome, IpcError> {
        if self.exited || self.stopping {
            return Err(IpcError::unsupported("interrupt"));
        }
        if !self.ready {
            return Err(IpcError::not_ready("sessionStarting"));
        }
        let cancel = discard_queued && self.capabilities.contains("interrupt_cancel_queued_v1");
        let target = self.interrupt_target();
        self.write_control(
            out,
            ControlBody::Interrupt { cancel_queued: cancel.then_some(true) },
            GildeKind::Interrupt { discard_queued, turn: target },
            None,
        )
        .inspect_err(|e| {
            if e.code == crate::contract::error::ErrorCode::Busy {
                self.not_reading(fx);
            }
        })?;
        if target.is_some() {
            self.mark_interrupt(target);
            self.interrupt_sent_mono = Some(now.mono);
        }
        if discard_queued {
            if let Some((h, _)) = self.after_result.take() {
                for u in self.render.delivery(h.id.as_str(), Delivery::Cancelled) {
                    fx.push(Effect::Content(u));
                }
                self.unqueue(h.id.as_str(), fx);
            }
        }
        self.flush(now, fx);
        Ok(InterruptOutcome::Interrupting)
    }

    /// Whether `turn_force_stop` is allowed now (≥ 5 s after an interrupt
    /// without a `result`), else `REFUSED{tooEarly}`.
    pub fn force_stop_allowed(&self, now: Now) -> Result<(), IpcError> {
        match self.interrupt_sent_mono {
            Some(t) if now.mono.saturating_sub(t) >= FORCE_STOP_AFTER_MS => Ok(()),
            _ => Err(IpcError::refused("tooEarly")),
        }
    }

    /// `session_set_model` (Conversation: `set_model`, from the next request).
    pub fn set_model(&mut self, m: Option<String>, out: &mut dyn Outbox) -> Result<(oneshot::Receiver<GildeReply>, AppliesFrom), IpcError> {
        if self.exited || self.stopping || !self.ready {
            return Err(IpcError::unsupported("setModel"));
        }
        let m = m.filter(|m| !m.is_empty());
        if let Some(m) = &m {
            if !handshake::valid_model(m) {
                return Err(IpcError::invalid_argument("model", "pattern"));
            }
        }
        let (tx, rx) = oneshot::channel();
        self.write_control(out, ControlBody::SetModel { model: m }, GildeKind::SetModel, Some(tx))?;
        Ok((rx, AppliesFrom::NextRequest))
    }

    /// `session_set_permission_mode`.
    pub fn set_permission_mode(&mut self, m: PermissionMode, out: &mut dyn Outbox, now: Now) -> Result<oneshot::Receiver<GildeReply>, IpcError> {
        if self.exited || self.stopping || !self.ready {
            return Err(IpcError::unsupported("setPermissionMode"));
        }
        if m == PermissionMode::BypassPermissions && !self.cfg.allow_bypass {
            return Err(IpcError::refused("needsLaunchFlag"));
        }
        let (tx, rx) = oneshot::channel();
        self.write_control(out, ControlBody::SetPermissionMode { mode: m.as_cli().to_owned() }, GildeKind::SetPermissionMode, Some(tx))?;
        self.flip.on_user_request(now.mono);
        Ok(rx)
    }

    /// `get_usage` over the live session (P12).
    pub fn get_usage(&mut self, out: &mut dyn Outbox) -> Result<oneshot::Receiver<GildeReply>, IpcError> {
        if !self.cfg.flags.get_usage_live {
            return Err(IpcError::unsupported("getUsage"));
        }
        if self.exited || self.stopping || !self.ready {
            return Err(IpcError::not_ready("sessionStarting"));
        }
        let (tx, rx) = oneshot::channel();
        self.write_control(out, ControlBody::GetUsage {}, GildeKind::GetUsage { baseline: false }, Some(tx))?;
        Ok(rx)
    }

    /// `interaction_detail`: the full input, pretty, ≤ 64 KiB, sanitized.
    pub fn interaction_detail(&self, req: &RequestId) -> Result<InteractionDetailRes, IpcError> {
        let p = self.pending.iter().find(|p| p.request_id == req.as_str()).ok_or_else(|| IpcError::not_found("request"))?;
        let input_pretty = match p.input.as_deref().and_then(|r| serde_json::from_str::<serde_json::Value>(r.get()).ok()) {
            Some(v) => tools::bounded_json(&v, DETAIL_CAP).text,
            None => String::new(),
        };
        Ok(InteractionDetailRes {
            input_pretty,
            tool_name: display(&p.tool_name, TextCap::Chars(64)),
            decision_reason: p.decision_reason.clone(),
        })
    }

    // -- stop and exit -------------------------------------------------------------------------------

    /// The stop sequence begins (§6.1.7): the cause is recorded, pending cards
    /// close `SessionEnded`, and `interrupt` is sent when a turn runs or a card
    /// was pending. `true` = wait for that turn's `result` before closing stdin.
    pub fn stop_begin(&mut self, cause: StopCause, out: &mut dyn Outbox, now: Now, fx: &mut Vec<Effect>) -> bool {
        let active = self.turn_active() || self.turn_starting;
        let target = self.interrupt_target();
        if self.stop_cause.is_none() || cause == StopCause::ForceStop {
            self.stop_cause = Some(cause);
            self.gilde_dirty = true;
        }
        self.stopping = true;
        self.end_cards(now, fx);
        self.drop_held(fx);
        let mut wait = false;
        if active && self.ready && !self.exited && !self.cfg.flags.stdin_eof_cancel {
            wait = self
                .write_control(out, ControlBody::Interrupt { cancel_queued: None }, GildeKind::Interrupt { discard_queued: false, turn: target }, None)
                .is_ok();
            if wait {
                self.mark_interrupt(target);
                self.interrupt_sent_mono = Some(now.mono);
            }
        }
        self.flush(now, fx);
        wait
    }

    /// A signal Gilde sent (fenced SIGTERM): recorded in the Gilde facts.
    pub fn signal_sent(&mut self, sig: i32, now: Now, fx: &mut Vec<Effect>) {
        if !self.signals_sent.contains(&sig) {
            self.signals_sent.push(sig);
            self.gilde_dirty = true;
        }
        self.flush(now, fx);
    }

    fn end_cards(&mut self, now: Now, fx: &mut Vec<Effect>) {
        for id in self.ledger.end_pending() {
            fx.push(Effect::Content(self.render.interaction_outcome(&id, InteractionOutcome::SessionEnded)));
        }
        if !self.pending.is_empty() {
            self.pending.clear();
            self.touch_phase(now);
        }
    }

    /// Held user frames that will never be sent: Cancelled.
    fn drop_held(&mut self, fx: &mut Vec<Effect>) {
        for h in std::mem::take(&mut self.held) {
            for u in self.render.delivery(h.id.as_str(), Delivery::Cancelled) {
                fx.push(Effect::Content(u));
            }
            self.unqueue(h.id.as_str(), fx);
        }
    }

    /// The process exited. `stderr` is the diagnostics ring (only used to
    /// recognise a usage error naming one of Gilde's flags before ready).
    pub fn on_exit(&mut self, argv: &[String], stderr: &str, now: Now, fx: &mut Vec<Effect>) {
        if self.exited {
            return;
        }
        if !self.ready && self.start_failure.is_none() {
            let patterns = self.patterns.get(now.mono);
            if let Some(flag) = handshake::usage_error_flag(stderr, argv, &patterns.usage_error_markers) {
                self.start_failure = Some(StartFailure::CompatFailed);
                self.gilde_dirty = true;
                let note = format!("the CLI rejected {flag}");
                fx.push(Effect::Compat(if self.cfg.handshake_unchecked {
                    CompatEvent::Handshake { ok: false, note: Some(note) }
                } else {
                    CompatEvent::Deviation { capability: CompatCapability::Conversation, note }
                }));
            }
        }
        self.exited = true;
        self.end_cards(now, fx);
        self.gilde.fail_all();
        for u in self.render.close_open(now.wall) {
            fx.push(Effect::Content(u));
        }
        let lost: Vec<String> = self.queued.iter().map(|q| q.client_msg_id.as_str().to_owned()).collect();
        for id in lost {
            for u in self.render.delivery(&id, Delivery::Cancelled) {
                fx.push(Effect::Content(u));
            }
        }
        if !self.queued.is_empty() {
            self.queued.clear();
            fx.push(Effect::Hook(HookEvent::Queued(Vec::new())));
        }
        self.drop_held(fx);
        self.after_result = None;
        self.base = TurnPhase::Idle;
        self.touch_phase(now);
        self.flush(now, fx);
        fx.push(Effect::Content(crate::contract::services::ContentUpdate::Ended));
    }
}

fn prefixed(prefix: &str, field: &str) -> String {
    if field.is_empty() {
        prefix.to_owned()
    } else {
        format!("{prefix}.{field}")
    }
}

/// A settings path for a note: only its last two components (no home path).
fn file_label(p: &std::path::Path) -> String {
    let parts: Vec<_> = p.components().rev().take(2).collect();
    parts.into_iter().rev().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::common::Confidence;
    use crate::contract::content::{BlockKind, BlockPatch};
    use crate::contract::interaction::QuestionAnswer;
    use crate::contract::services::ContentUpdate;
    use crate::contract::session::BinaryFingerprint;
    use serde_json::{json, Value};

    const SID: &str = "0ff2360d-9ebf-4152-ad44-5fb0754c7823";

    /// The flags `claude-compat.json` ships for 2.1.306 (P6/P7 are fallbacks there).
    fn probed() -> Flags {
        let mut f = Flags::all(true);
        f.resume_rearm = false;
        f.stdin_eof_cancel = false;
        f.rule_allow_marker = false;
        f
    }

    #[derive(Default)]
    struct VecOut {
        lines: Vec<(String, FrameClass)>,
        full: bool,
        closed: bool,
    }

    impl Outbox for VecOut {
        fn try_write(&mut self, line: String, class: FrameClass) -> Result<(), WriteError> {
            if self.closed {
                return Err(WriteError::Closed);
            }
            if self.full {
                return Err(WriteError::Full);
            }
            if line.len() > class.cap() {
                return Err(WriteError::TooLarge);
            }
            self.lines.push((line, class));
            Ok(())
        }
    }

    impl VecOut {
        fn frames(&self) -> Vec<Value> {
            self.lines.iter().map(|(l, _)| serde_json::from_str(l).unwrap()).collect()
        }
        fn last(&self) -> Value {
            serde_json::from_str(&self.lines.last().unwrap().0).unwrap()
        }
        fn subtypes(&self) -> Vec<String> {
            self.frames()
                .iter()
                .map(|f| f.pointer("/request/subtype").and_then(Value::as_str).or(f["type"].as_str()).unwrap_or("").to_owned())
                .collect()
        }
    }

    fn cfg(flags: Flags) -> CoreConfig {
        CoreConfig {
            uid: SessionUid::mint(),
            account_id: AccountId::parse("a_0123456789abcdef").ok(),
            launch: LaunchKind::New { session_id: SID.into() },
            spawn_version: "2.1.306".into(),
            handshake_unchecked: true,
            flags,
            permission_mode: PermissionMode::Default,
            allow_bypass: false,
            model: None,
            cwd: AbsPath::parse("/w/p").unwrap(),
            home: AbsPath::parse("/Users/u").ok(),
            binary: BinaryRef {
                realpath: AbsPath::parse("/opt/claude/bin/claude.exe").unwrap(),
                fingerprint: BinaryFingerprint { size: 1, mtime_ms: 1, inode: 1 },
                version: Some("2.1.306".into()),
                pidpath: None,
            },
            settings_paths: Vec::new(),
            child_pid: 4242,
            abandoned: None,
            initialize_timeout_ms: 30_000,
        }
    }

    struct T {
        core: Core,
        out: VecOut,
        fx: Vec<Effect>,
        now: Now,
    }

    impl T {
        fn new(c: CoreConfig) -> T {
            let now = Now { wall: 1_000_000, mono: 10_000 };
            let mut fx = Vec::new();
            let mut core = Core::new(c, PatternSource::bundled(), now, &mut fx);
            let mut out = VecOut::default();
            core.start(&mut out).unwrap();
            T { core, out, fx, now }
        }

        fn with(flags: Flags) -> T {
            T::new(cfg(flags))
        }

        fn advance(&mut self, ms: u64) {
            self.now.mono += ms;
            self.now.wall += ms as i64;
        }

        fn line(&mut self, v: Value) {
            let s = v.to_string();
            self.core.on_line(s.as_bytes(), &mut self.out, self.now, &mut self.fx);
        }

        fn init_id(&self) -> String {
            self.out.frames()[0]["request_id"].as_str().unwrap().to_owned()
        }

        fn ready(&mut self) {
            self.ready_with(json!({}), json!([]));
        }

        fn ready_with(&mut self, extra: Value, pending: Value) {
            let mut resp = json!({"claude_code_version":"2.1.306","capabilities":["ui_surface_v1"],"session_state":"idle",
                "current_permission_mode":"default","pid":4242,"models":[{"value":"default"}],
                "account":{"email":"x@example.com","apiProvider":"firstParty"}});
            if let Value::Object(e) = extra {
                for (k, v) in e {
                    resp[k] = v;
                }
            }
            let id = self.init_id();
            self.line(json!({"type":"control_response","response":{"subtype":"success","request_id":id,"response":resp,
                "pending_permission_requests":pending,"pending_user_dialog_requests":[]}}));
            assert!(self.core.is_ready());
        }

        fn sys_init(&mut self, caps: &[&str]) {
            self.line(json!({"type":"system","subtype":"init","session_id":SID,"model":"claude-haiku-5-5","permissionMode":"default",
                "capabilities":caps,"claude_code_version":"2.1.306","tools":["Bash"],"mcp_servers":[]}));
        }

        fn state(&mut self, s: &str) {
            self.line(json!({"type":"system","subtype":"session_state_changed","state":s,"session_id":SID}));
        }

        fn ask(&mut self, id: &str, tool: &str, input: Value, tuid: &str) {
            self.line(json!({"type":"control_request","request_id":id,"request":{"subtype":"can_use_tool","tool_name":tool,
                "display_name":tool,"input":input,"tool_use_id":tuid,
                "permission_suggestions":[{"type":"addRules","rules":[{"toolName":tool,"ruleContent":"ls"}],"behavior":"allow","destination":"localSettings"},
                                          {"type":"setMode","mode":"acceptEdits","destination":"session"}]}}));
        }

        fn result(&mut self, v: Value) {
            let mut r = json!({"type":"result","subtype":"success","is_error":false,"terminal_reason":"completed","session_id":SID,
                "total_cost_usd":0.0,"usage":{},"permission_denials":[],"user_message_uuids":null});
            if let Value::Object(e) = v {
                for (k, x) in e {
                    r[k] = x;
                }
            }
            self.line(r);
        }

        fn answer(&mut self, id: &str, d: Decision) -> Result<(AnswerOutcome, Option<DecisionKind>), IpcError> {
            self.core.answer(&RequestId::parse(id).unwrap(), &d, &mut self.out, self.now, &mut self.fx)
        }

        fn last_stream(&self) -> StreamFacts {
            self.fx
                .iter()
                .rev()
                .find_map(|e| match e {
                    Effect::Evidence(SourceFacts::Stream(s)) => Some(s.clone()),
                    _ => None,
                })
                .expect("stream facts")
        }

        fn compat(&self) -> Vec<CompatEvent> {
            self.fx.iter().filter_map(|e| if let Effect::Compat(c) = e { Some(c.clone()) } else { None }).collect()
        }

        fn hooks(&self) -> Vec<HookEvent> {
            self.fx.iter().filter_map(|e| if let Effect::Hook(h) = e { Some(h.clone()) } else { None }).collect()
        }

        fn content(&self) -> Vec<ContentUpdate> {
            self.fx.iter().filter_map(|e| if let Effect::Content(c) = e { Some(c.clone()) } else { None }).collect()
        }
    }

    fn bash(cmd: &str) -> Value {
        json!({"command": cmd, "description": "d"})
    }

    // -- T-compat-handshake (core part) ------------------------------------------------

    #[test]
    fn handshake_success_marks_ready_and_flushes_held_frames() {
        let mut t = T::with(probed());
        assert_eq!(t.out.subtypes(), ["initialize"]);
        let first = OutgoingMessage {
            client_msg_id: ClientMsgId::parse("0192f000-0000-4000-8000-000000000001").unwrap(),
            text: "hi".into(),
            priority: SendPriority::Next,
            images: Vec::new(),
        };
        assert_eq!(t.core.send(first, &mut t.out, t.now, &mut t.fx).unwrap(), SendDelivery::Queued);
        assert_eq!(t.out.lines.len(), 1, "user frames are held until initialize answers");
        t.ready();
        assert_eq!(t.out.subtypes(), ["initialize", "user"]);
        assert!(t.compat().contains(&CompatEvent::Handshake { ok: true, note: None }));
        let g = t.fx.iter().rev().find_map(|e| if let Effect::Evidence(SourceFacts::Gilde(g)) = e { Some(g.clone()) } else { None }).unwrap();
        assert!(g.ready_at.is_some());
    }

    #[test]
    fn handshake_failures_are_compat_failed_and_stop_the_process() {
        for resp in [
            json!({"subtype":"error","request_id":"X","error":"bad"}),
            json!({"subtype":"success","request_id":"X","response":{"claude_code_version":"2.1.306"}}),
        ] {
            let mut t = T::with(probed());
            let mut r = resp.clone();
            r["request_id"] = json!(t.init_id());
            t.line(json!({"type":"control_response","response":r}));
            assert!(!t.core.is_ready());
            assert!(matches!(t.compat()[..], [CompatEvent::Handshake { ok: false, .. }]));
            assert!(t.fx.iter().any(|e| matches!(e, Effect::StopForCompat)));
            let g = t.fx.iter().rev().find_map(|e| if let Effect::Evidence(SourceFacts::Gilde(g)) = e { Some(g.clone()) } else { None }).unwrap();
            assert_eq!(g.start_failure, Some(StartFailure::CompatFailed));
        }
        // A version whose handshake was already checked records a deviation instead.
        let mut c = cfg(probed());
        c.handshake_unchecked = false;
        let mut t = T::new(c);
        let id = t.init_id();
        t.line(json!({"type":"control_response","response":{"subtype":"error","request_id":id,"error":"x"}}));
        assert!(matches!(&t.compat()[..], [CompatEvent::Deviation { capability: CompatCapability::Conversation, .. }]));
    }

    // F-29: pid cross-check and tokenSource → signed out before any turn.
    #[test]
    fn handshake_pid_and_token_source_checks() {
        let mut t = T::with(probed());
        t.ready_with(json!({"pid": 7, "account": {"tokenSource":"none","apiProvider":"firstParty"}}), json!([]));
        assert!(t.compat().iter().any(|c| matches!(c, CompatEvent::Note(n) if n.contains("pid"))));
        assert!(t.hooks().contains(&HookEvent::SignedOut));
        let s = t.last_stream();
        assert_eq!(s.last_turn_end.unwrap().outcome, TurnOutcome::Error(ErrorSub::SignInNeeded));
    }

    #[test]
    fn a_usage_error_naming_our_flag_is_compat_evidence_but_other_exits_are_not() {
        let argv = vec!["/x/claude".to_string(), "--permission-prompts".to_string(), "host".to_string()];
        let mut t = T::with(probed());
        t.core.on_exit(&argv, "error: unknown option '--permission-prompts'", t.now, &mut t.fx);
        assert!(matches!(&t.compat()[..], [CompatEvent::Handshake { ok: false, .. }]));
        let mut t = T::with(probed());
        t.core.on_exit(&argv, "segfault", t.now, &mut t.fx);
        assert!(t.compat().is_empty(), "an exit without a usage error is not compat evidence");
    }

    // -- T-ipc-answer-outcomes (driver level) ------------------------------------------

    #[test]
    fn answer_outcomes_follow_c12() {
        let mut t = T::with(probed());
        t.ready();
        assert_eq!(t.answer("nope", Decision::AllowOnce).unwrap_err().params["entity"], "request");
        t.ask("r1", "Bash", bash("ls"), "toolu_1");
        t.ask("r1", "Bash", bash("ls"), "toolu_1"); // duplicate delivery
        assert_eq!(t.core.pending_cards().len(), 1);
        t.advance(400);
        assert_eq!(t.answer("r1", Decision::AllowOnce).unwrap(), (AnswerOutcome::Answered, None));
        assert_eq!(t.out.last()["response"]["request_id"], "r1");
        let n = t.out.lines.len();
        assert_eq!(t.answer("r1", Decision::Deny).unwrap(), (AnswerOutcome::AlreadyAnswered, Some(DecisionKind::AllowOnce)));
        assert_eq!(t.out.lines.len(), n, "never two responses for one request id (R4 rule 1)");
        // Withdrawn by the CLI.
        t.ask("r2", "Bash", bash("pwd"), "toolu_2");
        t.line(json!({"type":"control_cancel_request","request_id":"r2"}));
        t.advance(400);
        assert_eq!(t.answer("r2", Decision::AllowOnce).unwrap(), (AnswerOutcome::Withdrawn, None));
        assert!(t.content().iter().any(|u| matches!(u, ContentUpdate::Updated { patch: BlockPatch::Interaction { outcome: InteractionOutcome::Withdrawn }, .. })));
        // The answer/withdraw race: answered, then the cancel crosses it.
        t.ask("r3", "Bash", bash("id"), "toolu_3");
        t.advance(400);
        t.answer("r3", Decision::AllowOnce).unwrap();
        t.line(json!({"type":"control_cancel_request","request_id":"r3"}));
        assert_eq!(t.answer("r3", Decision::AllowOnce).unwrap(), (AnswerOutcome::Withdrawn, None));
        // The process ended.
        t.ask("r4", "Bash", bash("w"), "toolu_4");
        t.core.on_exit(&[], "", t.now, &mut t.fx);
        assert_eq!(t.answer("r4", Decision::AllowOnce).unwrap(), (AnswerOutcome::Withdrawn, None));
        assert!(t.content().iter().any(|u| matches!(u, ContentUpdate::Updated { patch: BlockPatch::Interaction { outcome: InteractionOutcome::SessionEnded }, .. })));
        // Unknown cancels are ignored and counted.
        t.line(json!({"type":"control_cancel_request","request_id":"ghost"}));
        assert!(t.core.counters().unknown_ids >= 1);
    }

    // T-convo-tooFresh.
    #[test]
    fn too_fresh_answers_are_refused() {
        let mut t = T::with(probed());
        t.ready();
        t.ask("r1", "Bash", bash("rm -rf build"), "toolu_1");
        t.advance(100);
        assert_eq!(t.answer("r1", Decision::AllowOnce).unwrap_err().params["changed"], "tooFresh");
        t.advance(250);
        assert_eq!(t.answer("r1", Decision::AllowOnce).unwrap().0, AnswerOutcome::Answered);
    }

    #[test]
    fn misfits_and_hidden_button_two() {
        let mut t = T::with(probed());
        t.ready();
        t.line(json!({"type":"control_request","request_id":"w1","request":{"subtype":"can_use_tool","tool_name":"Write",
            "input":{"file_path":"/w/p/a","content":"x"},"tool_use_id":"toolu_w","permission_suggestions":[{"type":"setMode","mode":"acceptEdits","destination":"session"}]}}));
        t.advance(400);
        assert_eq!(t.answer("w1", Decision::AllowSession).unwrap_err().params["cap"], "allowSessionHidden");
        assert_eq!(t.answer("w1", Decision::ApprovePlan).unwrap_err().params["rule"], "kindForCard");
        assert_eq!(t.core.pending_cards().len(), 1, "a refused decision changes nothing");
    }

    // -- T-convo-echoAck ---------------------------------------------------------------

    #[test]
    fn the_echo_of_our_answer_is_an_acknowledgement_not_a_reply() {
        let mut t = T::with(probed());
        t.ready();
        t.ask("r1", "Bash", bash("ls"), "toolu_1");
        t.advance(400);
        t.answer("r1", Decision::AllowOnce).unwrap();
        let sent = t.out.last();
        // The CLI echoes exactly what we sent.
        t.line(sent);
        assert!(t.core.ledger.acked("r1"));
        // A Gilde-originated request still gets its own reply.
        let mut rx = t.core.get_usage(&mut t.out).unwrap();
        let gid = t.out.last()["request_id"].as_str().unwrap().to_owned();
        t.line(json!({"type":"control_response","response":{"subtype":"success","request_id":gid,"response":{
            "rate_limits":{"five_hour":{"utilization":37,"resets_at":"2026-10-22T05:00:00Z"}},"behaviors":{"day":"/Users/x/elsewhere"}}}}));
        match rx.try_recv().unwrap() {
            GildeReply::Usage(Some(q)) => assert_eq!(q.windows[0].used_pct, Some(37)),
            other => panic!("{other:?}"),
        }
        assert!(t.hooks().iter().any(|h| matches!(h, HookEvent::Quota(_))));
        // An id in neither table is ignored and counted.
        t.line(json!({"type":"control_response","response":{"subtype":"success","request_id":"stranger","response":{}}}));
        assert_eq!(t.core.counters().unknown_ids, 1);
    }

    // -- T-convo-classify ----------------------------------------------------------------

    #[test]
    fn classification_by_terminal_reason_and_assistant_error() {
        let p = ErrorPatterns::bundled();
        let acct = AccountId::parse("a_0123456789abcdef").unwrap();
        let c = |r: ResultFacts<'_>| classify(&r, &p, Some(&acct));
        let base = || ResultFacts { terminal_reason: Some("completed"), is_error: Some(false), subtype: Some("success"), ..Default::default() };
        assert_eq!(c(base()).0, TurnOutcome::Ok);
        assert_eq!(c(ResultFacts { terminal_reason: Some("aborted_tools"), is_error: Some(true), subtype: Some("error_during_execution"), ..base() }).0, TurnOutcome::Interrupted);
        assert_eq!(c(ResultFacts { terminal_reason: Some("aborted_streaming"), ..base() }).0, TurnOutcome::Interrupted);
        // F-02: Gilde interrupted this turn — whatever the subtype says.
        assert_eq!(c(ResultFacts { terminal_reason: Some("weird"), is_error: Some(true), subtype: Some("error_during_execution"), interrupted_by_gilde: true, ..base() }).0, TurnOutcome::Interrupted);
        // LP-09b: logged out, seen as success + is_error.
        let signin = ResultFacts { terminal_reason: Some("api_error"), is_error: Some(true), subtype: Some("success"), assistant_error: Some("authentication_failed"), result_text: "Not logged in · Please run /login", ..base() };
        assert_eq!(c(signin).0, TurnOutcome::Error(ErrorSub::SignInNeeded));
        assert_eq!(c(ResultFacts { terminal_reason: Some("api_error"), is_error: Some(true), result_text: "Not logged in · Please run /login", ..base() }).0, TurnOutcome::Error(ErrorSub::SignInNeeded));
        // Usage limit (PROVISIONAL patterns): a 429 or the text, reset time from the text.
        assert_eq!(
            c(ResultFacts { terminal_reason: Some("api_error"), is_error: Some(true), api_error_status: Some(429), ..base() }).0,
            TurnOutcome::Error(ErrorSub::UsageLimit { account_id: Some(acct.clone()), resets_at: None })
        );
        assert_eq!(
            c(ResultFacts { terminal_reason: Some("api_error"), is_error: Some(true), result_text: "Claude AI usage limit reached|1792643400", ..base() }).0,
            TurnOutcome::Error(ErrorSub::UsageLimit { account_id: Some(acct.clone()), resets_at: Some(1_792_643_400_000) })
        );
        assert_eq!(c(ResultFacts { terminal_reason: Some("api_error"), is_error: Some(true), retry_status: Some(429), ..base() }).0, TurnOutcome::Error(ErrorSub::UsageLimit { account_id: Some(acct.clone()), resets_at: None }));
        assert_eq!(c(ResultFacts { terminal_reason: Some("completed"), is_error: Some(true), result_text: "boom", ..base() }).0, TurnOutcome::Error(ErrorSub::AgentError));
        let (o, unknown) = c(ResultFacts { terminal_reason: Some("something_new"), is_error: Some(false), ..base() });
        assert_eq!(o, TurnOutcome::Ok);
        assert!(unknown);
        assert_eq!(c(ResultFacts { terminal_reason: Some("something_new"), is_error: Some(true), ..base() }).0, TurnOutcome::Error(ErrorSub::AgentError));
    }

    #[test]
    fn deny_and_stop_and_the_receipt_make_the_turn_interrupted() {
        // DenyAndStop recorded in the ledger → Interrupted whatever the subtype.
        let mut t = T::with(probed());
        t.ready();
        t.sys_init(&[]);
        t.ask("r1", "Bash", bash("touch x"), "toolu_1");
        t.advance(400);
        t.answer("r1", Decision::DenyAndStop).unwrap();
        assert_eq!(t.out.last()["response"]["response"]["interrupt"], true);
        t.result(json!({"subtype":"error_during_execution","is_error":true,"terminal_reason":"something_else"}));
        assert_eq!(t.last_stream().last_turn_end.unwrap().outcome, TurnOutcome::Interrupted);
        // No error line, an Interrupted line instead.
        assert!(!t.content().iter().any(|u| matches!(u, ContentUpdate::Appended(b) if matches!(b.kind, BlockKind::System { note: SystemNote::Error { .. }, .. }))));
        // The interrupt receipt alone (the request is written without marking
        // the turn, so only the receipt can).
        t.sys_init(&[]);
        let seq = t.core.turn_seq;
        t.core
            .write_control(&mut t.out, ControlBody::Interrupt { cancel_queued: None }, GildeKind::Interrupt { discard_queued: false, turn: Some(seq) }, None)
            .unwrap();
        assert_eq!(t.core.turn_interrupt, None);
        let gid = t.out.last()["request_id"].as_str().unwrap().to_owned();
        t.line(json!({"type":"control_response","response":{"subtype":"success","request_id":gid,"response":{"still_queued":[]}}}));
        t.result(json!({"subtype":"success","is_error":true,"terminal_reason":"completed"}));
        assert_eq!(t.last_stream().last_turn_end.unwrap().outcome, TurnOutcome::Interrupted);
        // P5 fallback: Deny then a separate interrupt request.
        let mut f = probed();
        f.deny_and_stop = false;
        let mut t = T::with(f);
        t.ready();
        t.sys_init(&[]);
        t.ask("r9", "Bash", bash("x"), "toolu_9");
        t.advance(400);
        t.answer("r9", Decision::DenyAndStop).unwrap();
        let fr = t.out.frames();
        assert_eq!(fr[fr.len() - 2]["response"]["response"].get("interrupt"), None);
        assert_eq!(fr[fr.len() - 1]["request"]["subtype"], "interrupt");
    }

    // -- phase, lifecycle, unprompted turns ---------------------------------------------

    #[test]
    fn requires_action_also_comes_from_an_unbracketed_pending_card() {
        let mut t = T::with(probed());
        t.ready();
        t.sys_init(&[]);
        t.state("running");
        t.line(json!({"type":"control_request","request_id":"sb","request":{"subtype":"can_use_tool","tool_name":"SandboxNetworkAccess",
            "input":{"host":"example.com"},"tool_use_id":"5b0c2a7e-1111-4222-8333-944455556666",
            "permission_suggestions":[{"type":"addRules","rules":[{"toolName":"WebFetch","ruleContent":"domain:example.com"}],"behavior":"allow","destination":"localSettings"}]}}));
        let s = t.last_stream();
        assert_eq!(s.phase, TurnPhase::RequiresAction);
        assert_eq!(s.pending[0].always_label.as_deref(), Some("example.com"));
        assert_eq!(s.pending[0].confidence, Confidence::High);
        t.advance(400);
        t.answer("sb", Decision::AllowOnce).unwrap();
        assert_eq!(t.last_stream().phase, TurnPhase::Running);
    }

    #[test]
    fn unprompted_turns_run_from_init_alone() {
        let mut t = T::with(probed());
        t.ready();
        assert_eq!(t.last_stream().phase, TurnPhase::Idle);
        t.advance(5);
        t.sys_init(&[]);
        let s = t.last_stream();
        assert_eq!(s.phase, TurnPhase::Running);
        assert!(s.last_turn_start_at.is_some());
        t.result(json!({"user_message_uuids": null}));
        let s = t.last_stream();
        assert_eq!(s.phase, TurnPhase::Idle);
        assert_eq!(s.last_turn_end.unwrap().outcome, TurnOutcome::Ok);
        // Later inits do not change identity.
        t.line(json!({"type":"system","subtype":"init","session_id":"11111111-2222-4333-8444-555555555555","permissionMode":"plan"}));
        assert_eq!(t.core.vendor_session_id(), Some(SID));
    }

    #[test]
    fn without_session_state_events_the_phase_comes_from_frames() {
        let mut f = probed();
        f.session_state_events = false;
        let mut t = T::with(f);
        t.ready();
        let m = OutgoingMessage { client_msg_id: ClientMsgId::parse("0192f000-0000-4000-8000-000000000001").unwrap(), text: "go".into(), priority: SendPriority::Next, images: Vec::new() };
        t.core.send(m, &mut t.out, t.now, &mut t.fx).unwrap();
        t.state("idle"); // ignored with the flag off…
        assert_eq!(t.last_stream().phase, TurnPhase::Running, "…and the first frame after the user frame starts the turn");
        t.result(json!({}));
        assert_eq!(t.last_stream().phase, TurnPhase::Idle);
    }

    // T-convo-lifecycle and T-convo-coalescedSends.
    #[test]
    fn delivery_comes_from_command_lifecycle_with_the_replay_as_fallback() {
        let mut t = T::with(probed());
        t.ready();
        t.sys_init(&[]);
        t.state("running");
        let a = ClientMsgId::parse("0192f000-0000-4000-8000-00000000000a").unwrap();
        let b = ClientMsgId::parse("0192f000-0000-4000-8000-00000000000b").unwrap();
        for id in [&a, &b] {
            let d = t.core.send(OutgoingMessage { client_msg_id: id.clone(), text: "q".into(), priority: SendPriority::Next, images: Vec::new() }, &mut t.out, t.now, &mut t.fx).unwrap();
            assert_eq!(d, SendDelivery::Queued);
        }
        assert_eq!(t.core.queued().len(), 2);
        // Idempotent by clientMsgId.
        assert_eq!(t.core.send(OutgoingMessage { client_msg_id: a.clone(), text: "q".into(), priority: SendPriority::Next, images: Vec::new() }, &mut t.out, t.now, &mut t.fx).unwrap(), SendDelivery::Queued);
        assert_eq!(t.out.subtypes().iter().filter(|s| *s == "user").count(), 2);
        t.line(json!({"type":"command_lifecycle","command_uuid":a.as_str(),"state":"queued"}));
        t.line(json!({"type":"command_lifecycle","command_uuid":a.as_str(),"state":"started"}));
        assert_eq!(t.core.queued().len(), 1);
        assert!(t.hooks().iter().any(|h| matches!(h, HookEvent::Queued(q) if q.len() == 1)));
        // b: no lifecycle; the result's user_message_uuids consume it.
        t.result(json!({"user_message_uuids":[a.as_str(), b.as_str()]}));
        assert!(t.core.queued().is_empty());
        assert_eq!(t.core.render.delivery_of(b.as_str()), Some(Delivery::Delivered));
        // The replay alone (no lifecycle frame) marks Delivered.
        let c = ClientMsgId::parse("0192f000-0000-4000-8000-00000000000c").unwrap();
        t.core.send(OutgoingMessage { client_msg_id: c.clone(), text: "x".into(), priority: SendPriority::Next, images: Vec::new() }, &mut t.out, t.now, &mut t.fx).unwrap();
        t.line(json!({"type":"user","message":{"role":"user","content":[{"type":"text","text":"x"}]},"uuid":c.as_str(),"isReplay":true,"origin":{"kind":"human"}}));
        assert_eq!(t.core.render.delivery_of(c.as_str()), Some(Delivery::Delivered));
        // Cancelled.
        let d = ClientMsgId::parse("0192f000-0000-4000-8000-00000000000d").unwrap();
        t.sys_init(&[]);
        t.core.send(OutgoingMessage { client_msg_id: d.clone(), text: "y".into(), priority: SendPriority::Next, images: Vec::new() }, &mut t.out, t.now, &mut t.fx).unwrap();
        match t.core.cancel_queued(&d, &mut t.out, &mut t.fx).unwrap() {
            CancelPlan::Wait(_) => {}
            other => panic!("{other:?}"),
        }
        assert_eq!(t.out.last()["request"], json!({"subtype":"cancel_async_message","message_uuid":d.as_str()}));
        t.line(json!({"type":"command_lifecycle","command_uuid":d.as_str(),"state":"cancelled"}));
        assert_eq!(t.core.render.delivery_of(d.as_str()), Some(Delivery::Cancelled));
        assert_eq!(t.core.cancel_queued(&ClientMsgId::parse("0192f000-0000-4000-8000-0000000000ff").unwrap(), &mut t.out, &mut t.fx).unwrap_err().params["entity"], "message");
    }

    #[test]
    fn send_now_by_capability_or_the_fallback() {
        let mut t = T::with(probed());
        t.ready();
        t.sys_init(&["interrupt_send_now_v1"]);
        let m = |n: u8| OutgoingMessage {
            client_msg_id: ClientMsgId::parse(format!("0192f000-0000-4000-8000-0000000000{n:02x}")).unwrap(),
            text: "now".into(),
            priority: SendPriority::Now,
            images: Vec::new(),
        };
        t.core.send(m(1), &mut t.out, t.now, &mut t.fx).unwrap();
        assert_eq!(t.out.last()["priority"], "now");
        // Without the capability: interrupt, then the frame after the result.
        let mut t = T::with(probed());
        t.ready();
        t.sys_init(&[]);
        t.core.send(m(2), &mut t.out, t.now, &mut t.fx).unwrap();
        assert_eq!(t.out.last()["request"]["subtype"], "interrupt");
        t.result(json!({"subtype":"error_during_execution","is_error":true,"terminal_reason":"aborted_tools"}));
        let last = t.out.last();
        assert_eq!(last["type"], "user");
        assert!(last.get("priority").is_none());
        assert_eq!(t.last_stream().last_turn_end.unwrap().outcome, TurnOutcome::Interrupted);
    }

    // -- backpressure ------------------------------------------------------------------------

    #[test]
    fn a_full_stdin_queue_is_busy_and_changes_nothing() {
        let mut t = T::with(probed());
        t.ready();
        t.ask("r1", "Bash", bash("ls"), "toolu_1");
        t.advance(400);
        t.out.full = true;
        let e = t.answer("r1", Decision::AllowOnce).unwrap_err();
        assert_eq!(e.code, crate::contract::error::ErrorCode::Busy);
        assert_eq!(e.params["reason"], "stdinFull");
        assert!(t.hooks().iter().any(|h| matches!(h, HookEvent::Notice(n) if n.key == "convo.notReading")));
        assert_eq!(t.core.pending_cards().len(), 1, "the card stays answerable");
        t.out.full = false;
        assert_eq!(t.answer("r1", Decision::AllowOnce).unwrap().0, AnswerOutcome::Answered);
        let huge = OutgoingMessage { client_msg_id: ClientMsgId::parse("0192f000-0000-4000-8000-000000000001").unwrap(), text: "\u{1}".repeat(200 * 1024), priority: SendPriority::Next, images: Vec::new() };
        assert_eq!(t.core.send(huge, &mut t.out, t.now, &mut t.fx).unwrap_err().params["rule"], "size", "an escaped frame over 1 MiB");
    }

    // -- R4 rule 9 and §6.1.9 ------------------------------------------------------------------

    #[test]
    fn other_cli_requests_and_unanswerable_ones() {
        let mut t = T::with(probed());
        t.ready();
        let n = t.out.lines.len();
        t.line(json!({"type":"control_request","request_id":"e1","request":{"subtype":"elicitation","mcp_server_name":"x","message":"m"}}));
        assert_eq!(t.out.last(), json!({"type":"control_response","response":{"subtype":"error","request_id":"e1","error":"unsupported"}}));
        t.line(json!({"type":"control_request","request_id":"h1","request":{"subtype":"hook_callback","callback_id":"c"}}));
        t.line(json!({"type":"control_request","request_id":"d1","request":{"subtype":"request_user_dialog","dialog_kind":"x"}}));
        assert_eq!(t.out.lines.len(), n + 2, "request_user_dialog is never answered");
        t.line(json!({"type":"control_request","request_id":42,"request":{"subtype":"can_use_tool","tool_name":"Bash","input":{}}}));
        assert!(t.compat().iter().any(|c| matches!(c, CompatEvent::Deviation { capability: CompatCapability::Conversation, .. })));
        assert!(t.hooks().iter().any(|h| matches!(h, HookEvent::Notice(n) if n.key == "convo.unanswerable")));
        assert_eq!(t.out.lines.len(), n + 2);
        // A degraded card is answerable and only noted.
        t.line(json!({"type":"control_request","request_id":"g1","request":{"subtype":"can_use_tool","tool_name":7,"input":{"a":1},"tool_use_id":"t"}}));
        assert!(t.core.pending_cards()[0].degraded);
        assert!(t.compat().iter().any(|c| matches!(c, CompatEvent::Note(n) if n == "unreadable permission request")));
        // rate_limit_event → the account's quota, no probe.
        t.line(json!({"type":"rate_limit_event","rate_limit_info":{"status":"allowed","unifiedWindows":{"five_hour":{"utilization":0.25,"resetsAt":1792643400}}}}));
        assert!(t.hooks().iter().any(|h| matches!(h, HookEvent::Quota(q) if q.windows[0].used_pct == Some(25))));
        // A string id that is not a valid RequestId still gets a card (hashed
        // card id) and is answered with the CLI's own id.
        let odd = "x".repeat(200);
        t.line(json!({"type":"control_request","request_id":odd,"request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"},"tool_use_id":"t_odd"}}));
        let card = t.core.pending_cards().into_iter().find(|c| c.request_id.as_str().starts_with("h_")).expect("hashed card");
        t.advance(400);
        t.answer(card.request_id.as_str(), Decision::Deny).unwrap();
        assert_eq!(t.out.last()["response"]["request_id"], json!(odd));
        // A long-but-string id on another subtype still gets its error reply.
        t.line(json!({"type":"control_request","request_id":odd,"request":{"subtype":"elicitation"}}));
        assert_eq!(t.out.last()["response"]["subtype"], "error");
        // Unknown frames, too deep frames, garbage.
        t.line(json!({"type":"future_frame"}));
        t.line(json!({"type":"system","subtype":"future_sub"}));
        let deep = format!(r#"{{"type":"system","a":{}{}}}"#, "[".repeat(80), "]".repeat(80));
        t.core.on_line(deep.as_bytes(), &mut t.out, t.now, &mut t.fx);
        t.core.on_line(b"not json", &mut t.out, t.now, &mut t.fx);
        let c = t.core.counters();
        assert_eq!(c.unknown_types.get("future_frame"), Some(&1));
        assert_eq!(c.unknown_types.get("system/future_sub"), Some(&1));
        assert_eq!(c.too_deep, 1);
        assert_eq!(c.unparseable, 1);
    }

    // -- F-26, F-28, §6.1.11, F-01 ---------------------------------------------------------------

    #[test]
    fn denial_lines_only_for_denials_without_a_card() {
        let mut t = T::with(probed());
        t.ready();
        t.sys_init(&[]);
        t.ask("r1", "Bash", bash("touch x"), "toolu_mine");
        t.advance(400);
        t.answer("r1", Decision::Deny).unwrap();
        t.result(json!({"permission_denials":[{"tool_name":"Bash","tool_use_id":"toolu_mine","tool_input":{}},{"tool_name":"Write","tool_use_id":"toolu_auto","tool_input":{}}]}));
        let lines: Vec<String> = t
            .content()
            .iter()
            .filter_map(|u| match u {
                ContentUpdate::Appended(b) => match &b.kind {
                    BlockKind::System { note: SystemNote::PermissionDenied { tool }, .. } => Some(tool.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(lines, ["Write"]);
    }

    #[test]
    fn subagent_cards_are_named_by_their_task() {
        let mut t = T::with(probed());
        t.ready();
        t.line(json!({"type":"system","subtype":"task_started","task_id":"a88d9245b5fb706fe","tool_use_id":"toolu_agent","description":"Create lp07-d.txt file","task_type":"local_agent","prompt":"secret prompt"}));
        t.line(json!({"type":"control_request","request_id":"s1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"touch d"},
            "tool_use_id":"toolu_sub","agent_id":"a88d9245b5fb706fe"}}));
        t.line(json!({"type":"control_request","request_id":"s2","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"touch e"},
            "tool_use_id":"toolu_sub2","agent_id":"a84dbe27c6bb4e06d"}}));
        let cards = t.core.pending_cards();
        assert_eq!(cards.len(), 2, "parallel subagent asks stack");
        assert_eq!(cards[0].detail_lines[0], crate::contract::activity::EvidenceLine::detail_text("Create lp07-d.txt file"));
        assert_eq!(cards[1].detail_lines[0], crate::contract::activity::EvidenceLine::key("card.aSubagent"), "the key, never English text [A19.3]");
        let task_lines = t.content().iter().filter(|u| matches!(u, ContentUpdate::Appended(b) if matches!(b.kind, BlockKind::System { note: SystemNote::BackgroundTask { .. }, .. }))).count();
        assert_eq!(task_lines, 1);
    }

    #[test]
    fn the_silent_approval_guard_flags_but_never_blocks() {
        let mut t = T::with(probed());
        t.ready();
        t.sys_init(&[]);
        t.line(json!({"type":"assistant","message":{"id":"msg_1","role":"assistant","content":[{"type":"tool_use","id":"toolu_s","name":"Bash","input":{"command":"rm x"}}]}}));
        t.line(json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_s","content":"ok"}]}}));
        assert!(t.hooks().contains(&HookEvent::ApprovalUnconfirmed("Bash".into())));
        assert!(t.compat().iter().any(|c| matches!(c, CompatEvent::Note(n) if n.contains("unconfirmed"))));
        assert!(!t.compat().iter().any(|c| matches!(c, CompatEvent::Deviation { .. })), "P13 fallback: flag only");
    }

    /// A33.3: an unknown `permissionMode` — in the `initialize` response, in
    /// `system/init`, in a later `system/status` — makes the mode unknown
    /// (`None`) and publishes it, never keeps the previous value; a known one
    /// sets it again; an absent field changes nothing.
    #[test]
    fn unknown_permission_modes_make_the_mode_unknown_never_the_previous_one() {
        let meta_mode = |t: &mut T| {
            let now = t.now;
            t.core.flush(now, &mut t.fx);
            t.fx.iter()
                .rev()
                .find_map(|e| match e {
                    Effect::Evidence(SourceFacts::Meta(m)) => Some(m.permission_mode),
                    _ => None,
                })
                .expect("meta facts")
        };
        let mut t = T::with(probed());
        assert_eq!(t.core.permission_mode, Some(PermissionMode::Default), "the launch mode");
        t.ready_with(json!({"current_permission_mode": "turbo"}), json!([]));
        assert_eq!(t.core.permission_mode, None, "initialize: unknown, not the launch mode");
        assert_eq!(meta_mode(&mut t), None);
        t.line(json!({"type":"system","subtype":"init","session_id":SID,"permissionMode":"plan"}));
        assert_eq!(meta_mode(&mut t), Some(PermissionMode::Plan));
        t.line(json!({"type":"system","subtype":"init","session_id":SID,"permissionMode":"hyperPlan"}));
        assert_eq!((t.core.permission_mode, meta_mode(&mut t)), (None, None), "system/init: unknown, not plan");
        t.line(json!({"type":"system","subtype":"status","permissionMode":"acceptEdits"}));
        assert_eq!(meta_mode(&mut t), Some(PermissionMode::AcceptEdits));
        t.line(json!({"type":"system","subtype":"status","permissionMode":"warp-9"}));
        assert_eq!((t.core.permission_mode, meta_mode(&mut t)), (None, None), "a later status: unknown");
        t.line(json!({"type":"system","subtype":"status","permissionMode":"dontAsk"}));
        t.line(json!({"type":"system","subtype":"init","session_id":SID,"model":"claude-haiku-5-5"}));
        assert_eq!(t.core.permission_mode, Some(PermissionMode::DontAsk), "an absent field changes nothing");
    }

    #[test]
    fn a_mode_flip_after_allow_session_turns_button_two_off() {
        let mut t = T::with(probed());
        t.ready();
        t.sys_init(&[]);
        t.ask("r0", "Bash", bash("pwd"), "toolu_0");
        t.ask("r1", "Bash", bash("ls"), "toolu_1");
        assert!(t.core.pending_cards()[0].always_label.is_some());
        t.advance(400);
        t.answer("r1", Decision::AllowSession).unwrap();
        let p = &t.out.last()["response"]["response"];
        assert_eq!(p["updatedPermissions"][0]["destination"], "session");
        assert!(!p.to_string().contains("setMode"));
        t.advance(500);
        t.line(json!({"type":"system","subtype":"status","permissionMode":"acceptEdits"}));
        assert!(t.compat().iter().any(|c| matches!(c, CompatEvent::Deviation { capability: CompatCapability::Feature(f), .. } if f == "allowSession")));
        let still = t.core.pending_cards();
        assert!(still[0].always_label.is_none(), "the card already pending loses button 2 at once");
        assert!(still[0].options.iter().all(|o| o.key != "2"));
        assert!(t.last_stream().pending[0].always_label.is_none(), "and World hears it");
        assert_eq!(t.answer("r0", Decision::AllowSession).unwrap_err().params["cap"], "allowSessionHidden");
        t.ask("r2", "Bash", bash("ls"), "toolu_2");
        assert!(t.core.pending_cards().iter().all(|c| c.always_label.is_none()), "button 2 is gone for this session");
    }

    #[test]
    fn the_settings_file_guard_through_the_core() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = cfg(probed());
        c.cwd = AbsPath::parse(dir.path().to_str().unwrap()).unwrap();
        c.settings_paths = crate::convo::guard::settings_paths(dir.path(), None);
        let mut t = T::new(c);
        t.ready();
        t.ask("r1", "Bash", bash("ls"), "toolu_1");
        t.advance(400);
        t.answer("r1", Decision::AllowSession).unwrap();
        std::fs::create_dir_all(dir.path().join(".claude")).unwrap();
        std::fs::write(dir.path().join(".claude/settings.local.json"), b"{}").unwrap();
        t.advance(300);
        t.core.tick(&mut t.out, t.now, &mut t.fx);
        assert!(t.compat().iter().any(|c| matches!(c, CompatEvent::Deviation { capability: CompatCapability::Feature(f), note } if f == "allowSession" && note.contains("settings.local.json"))));
        assert!(!t.core.allow_session());
    }

    // -- spend (A10.4) ----------------------------------------------------------------------------

    #[test]
    fn stream_usage_counts_each_message_once_with_its_final_numbers() {
        let mut u = StreamUsage::new(false);
        let tok = |i, o, cc, cr| TokenUsage { input: i, output: o, cache_creation: cc, cache_read: cr };
        u.message("2026-10-23", "msg_1", &tok(2, 16, 7375, 15892)); // message_start
        u.message("2026-10-23", "msg_1", &tok(2, 16, 7375, 15892)); // assistant (repeat)
        u.message("2026-10-23", "msg_1", &tok(2, 89, 7375, 15892)); // message_delta (final)
        u.message("2026-10-23", "msg_2", &tok(2, 50, 1905, 23267));
        u.cost("2026-10-23", 0.0023);
        u.cost("2026-10-23", 0.0029);
        let t = u.total().unwrap();
        assert_eq!(t.input_tokens, Some(2 + 7375 + 2 + 1905), "uncached + cache-creation (T04's mapping)");
        assert_eq!(t.cached_tokens, Some(15892 + 23267));
        assert_eq!(t.output_tokens, Some(89 + 50));
        assert_eq!(t.cost_micros, Some(2900), "the running total, from the process' cumulative cost");
        assert!(u.due(0, false).is_some());
        u.message("2026-10-23", "msg_3", &tok(1, 1, 0, 0));
        assert!(u.due(500, false).is_none(), "≤ 1 report per second");
        assert!(u.due(1_000, false).is_some());
        // A new local day is a new total.
        u.message("2026-10-24", "msg_4", &tok(1, 2, 3, 4));
        assert_eq!(u.total().unwrap().day, "2026-10-24");
        assert_eq!(u.total().unwrap().output_tokens, Some(2));
        assert_eq!(u.total().unwrap().cost_micros, None, "no cost reported yet today");
        // A resumed process: the first value is a baseline (restored cost).
        let mut r = StreamUsage::new(true);
        r.cost("2026-10-23", 0.0024);
        assert_eq!(r.total().unwrap().cost_micros, Some(0));
        r.cost("2026-10-23", 0.0030);
        assert_eq!(r.total().unwrap().cost_micros, Some(600));
        assert!(StreamUsage::new(false).total().is_none(), "nothing reported = unknown");
    }

    #[test]
    fn usage_reaches_world_as_stream_facts() {
        let mut t = T::with(probed());
        t.ready();
        t.sys_init(&[]);
        t.line(json!({"type":"stream_event","event":{"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":2,"output_tokens":3,"cache_read_input_tokens":10,"cache_creation_input_tokens":5}}}}));
        t.line(json!({"type":"stream_event","event":{"type":"message_delta","usage":{"output_tokens":40}}}));
        t.result(json!({"total_cost_usd":0.5}));
        t.advance(1_100);
        t.core.tick(&mut t.out, t.now, &mut t.fx);
        let u = t.fx.iter().rev().find_map(|e| if let Effect::Evidence(SourceFacts::Usage(u)) = e { Some(u.clone()) } else { None }).unwrap();
        assert_eq!(u.source, UsageSource::Stream);
        assert_eq!(u.today.output_tokens, Some(40));
        assert_eq!(u.today.input_tokens, Some(7));
        assert_eq!(u.today.cost_micros, Some(500_000));
    }

    // -- stop, exit, re-arm ------------------------------------------------------------------------

    #[test]
    fn the_stop_sequence_interrupts_a_turn_and_ends_cards() {
        let mut t = T::with(probed());
        t.ready();
        t.sys_init(&[]);
        t.ask("r1", "Bash", bash("ls"), "toolu_1");
        assert!(t.core.stop_begin(StopCause::User, &mut t.out, t.now, &mut t.fx));
        assert_eq!(t.out.last()["request"]["subtype"], "interrupt");
        assert!(t.core.pending_cards().is_empty());
        assert!(t.content().iter().any(|u| matches!(u, ContentUpdate::Updated { patch: BlockPatch::Interaction { outcome: InteractionOutcome::SessionEnded }, .. })));
        let g = t.fx.iter().rev().find_map(|e| if let Effect::Evidence(SourceFacts::Gilde(g)) = e { Some(g.clone()) } else { None }).unwrap();
        assert_eq!(g.stop_cause, Some(StopCause::User));
        t.result(json!({"subtype":"error_during_execution","is_error":true,"terminal_reason":"aborted_tools"}));
        assert!(t.fx.iter().any(|e| matches!(e, Effect::TurnEnded)));
        // Idle sessions close stdin at once.
        let mut t = T::with(probed());
        t.ready();
        assert!(!t.core.stop_begin(StopCause::Quit, &mut t.out, t.now, &mut t.fx));
        // P7 switched on (EOF would cancel a pending prompt): no interrupt first.
        let mut f = probed();
        f.stdin_eof_cancel = true;
        let mut t = T::with(f);
        t.ready();
        t.sys_init(&[]);
        let n = t.out.lines.len();
        assert!(!t.core.stop_begin(StopCause::User, &mut t.out, t.now, &mut t.fx));
        assert_eq!(t.out.lines.len(), n);
    }

    #[test]
    fn force_stop_waits_five_seconds_after_an_interrupt() {
        let mut t = T::with(probed());
        t.ready();
        t.sys_init(&[]);
        assert_eq!(t.core.force_stop_allowed(t.now).unwrap_err().params["reason"], "tooEarly");
        t.core.interrupt(true, &mut t.out, t.now, &mut t.fx).unwrap();
        assert!(t.out.last()["request"].get("cancel_queued").is_none(), "cancel_queued needs the capability");
        t.advance(4_000);
        assert!(t.core.force_stop_allowed(t.now).is_err());
        t.advance(1_000);
        assert!(t.core.force_stop_allowed(t.now).is_ok());
    }

    #[test]
    fn rearm_cards_from_initialize_and_the_abandoned_block() {
        let mut c = cfg(probed());
        c.launch = LaunchKind::Continue { session_id: SID.into() };
        c.abandoned = Some(AbandonedAsk { tool_use_id: "toolu_old".into(), tool_name: "Bash".into() });
        let mut t = T::new(c.clone());
        t.ready_with(json!({}), json!([]));
        assert!(t.content().iter().any(|u| matches!(u, ContentUpdate::Appended(b)
            if matches!(&b.kind, BlockKind::Interaction { request_id, outcome: InteractionOutcome::Abandoned, .. } if request_id.as_str() == "ab_toolu_old"))));
        let mut t = T::new(c);
        let pending = json!([{"type":"control_request","request_id":"p1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"},"tool_use_id":"toolu_p"}},
                             {"type":"control_request","request_id":"p1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"},"tool_use_id":"toolu_p"}}]);
        t.ready_with(json!({}), pending);
        assert_eq!(t.core.pending_cards().len(), 1, "deduped by request_id");
        assert!(!t.content().iter().any(|u| matches!(u, ContentUpdate::Appended(b) if matches!(&b.kind, BlockKind::Interaction { request_id, .. } if request_id.as_str().starts_with("ab_")))));
    }

    #[test]
    fn exit_closes_cards_and_ends_the_view() {
        let mut t = T::with(probed());
        t.ready();
        t.sys_init(&[]);
        t.ask("r1", "AskUserQuestion", json!({"questions":[{"question":"Which?","header":"h","options":[{"label":"a"},{"label":"b"}],"multiSelect":false}]}), "toolu_q");
        let m = OutgoingMessage { client_msg_id: ClientMsgId::parse("0192f000-0000-4000-8000-000000000001").unwrap(), text: "later".into(), priority: SendPriority::Next, images: Vec::new() };
        t.core.send(m, &mut t.out, t.now, &mut t.fx).unwrap();
        t.core.on_exit(&[], "", t.now, &mut t.fx);
        assert!(t.core.pending_cards().is_empty());
        assert!(t.core.queued().is_empty());
        assert!(matches!(t.content().last(), Some(ContentUpdate::Ended)));
        let s = t.last_stream();
        assert_eq!(s.phase, TurnPhase::Idle);
        assert!(s.pending.is_empty());
        t.advance(400);
        let ans = Decision::Answer { answers: vec![QuestionAnswer { question_index: 0, labels: vec!["a".into()], free_text: None }] };
        assert_eq!(t.answer("r1", ans).unwrap().0, AnswerOutcome::Withdrawn);
    }

    #[test]
    fn an_interrupt_marks_only_the_turn_it_was_sent_for() {
        // An interrupt sent while idle with only a re-armed card pending must
        // not make the next, unrelated turn "Interrupted".
        let pending = json!([{"type":"control_request","request_id":"p1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"},"tool_use_id":"toolu_p"}}]);
        let mut t = T::with(probed());
        t.ready_with(json!({}), pending);
        t.core.interrupt(false, &mut t.out, t.now, &mut t.fx).unwrap();
        t.line(json!({"type":"control_cancel_request","request_id":"p1"}));
        t.sys_init(&[]);
        t.result(json!({"terminal_reason":"api_error","is_error":true,"result":"Not logged in · Please run /login"}));
        assert_eq!(t.last_stream().last_turn_end.unwrap().outcome, TurnOutcome::Error(ErrorSub::SignInNeeded));
        // An interrupt between the send and the turn's first frame belongs to that turn.
        let mut f = probed();
        f.session_state_events = false;
        let mut t = T::with(f);
        t.ready();
        t.core.send(OutgoingMessage { client_msg_id: ClientMsgId::parse("0192f000-0000-4000-8000-000000000001").unwrap(), text: "go".into(), priority: SendPriority::Next, images: Vec::new() }, &mut t.out, t.now, &mut t.fx).unwrap();
        t.core.interrupt(false, &mut t.out, t.now, &mut t.fx).unwrap();
        t.result(json!({"subtype":"error_during_execution","is_error":true,"terminal_reason":null}));
        assert_eq!(t.last_stream().last_turn_end.unwrap().outcome, TurnOutcome::Interrupted);
    }

    #[test]
    fn whitespace_messages_and_api_error_only_classes() {
        let mut t = T::with(probed());
        t.ready();
        t.ask("r1", "Bash", bash("ls"), "toolu_1");
        t.advance(400);
        let e = t.answer("r1", Decision::DenyWithMessage { message: "   ".into() }).unwrap_err();
        assert_eq!((e.params["field"].as_str(), e.params["rule"].as_str()), (Some("decision.message"), Some("required")));
        let p = ErrorPatterns::bundled();
        let r = ResultFacts { terminal_reason: Some("max_turns"), is_error: Some(true), retry_status: Some(429), ..Default::default() };
        assert_eq!(classify(&r, &p, None).0, TurnOutcome::Error(ErrorSub::AgentError), "a transient 429 retry does not make max_turns a usage limit");
    }

    #[test]
    fn no_initialize_within_the_timeout_drops_the_held_prompt() {
        let mut t = T::with(probed());
        let m = OutgoingMessage { client_msg_id: ClientMsgId::parse("0192f000-0000-4000-8000-000000000001").unwrap(), text: "hi".into(), priority: SendPriority::Next, images: Vec::new() };
        t.core.send(m, &mut t.out, t.now, &mut t.fx).unwrap();
        assert_eq!(t.core.queued().len(), 1);
        t.advance(30_000);
        t.core.tick(&mut t.out, t.now, &mut t.fx);
        assert!(t.core.queued().is_empty());
        assert_eq!(t.core.render.delivery_of("0192f000-0000-4000-8000-000000000001"), Some(Delivery::Cancelled));
        t.ready();
        assert_eq!(t.out.subtypes(), ["initialize"], "a late initialize never sends the dropped prompt");
    }

    #[test]
    fn only_decisions_echoing_the_input_may_exceed_one_mib() {
        let mut t = T::with(probed());
        t.ready();
        let big = "x".repeat(2 * 1024 * 1024);
        t.ask("big", "Bash", json!({"command": big}), "toolu_big");
        t.advance(400);
        t.answer("big", Decision::AllowOnce).unwrap();
        let (line, class) = t.out.lines.last().unwrap();
        assert_eq!(*class, FrameClass::Answer);
        assert!(line.len() > wire::FRAME_CAP, "a large input can still be approved [A19.4]");
        assert_eq!(t.out.last()["response"]["response"]["updatedInput"]["command"].as_str().map(str::len), Some(2 * 1024 * 1024));
        t.ask("big2", "Bash", json!({"command": "x".repeat(2 * 1024 * 1024)}), "toolu_big2");
        t.advance(400);
        t.answer("big2", Decision::DenyWithMessage { message: "no".into() }).unwrap();
        assert_eq!(t.out.lines.last().unwrap().1, FrameClass::Control, "a deny never echoes the input: 1 MiB");
        assert_eq!(FrameClass::Control.cap(), wire::FRAME_CAP);
        assert_eq!(FrameClass::User.cap(), wire::FRAME_CAP);
        assert_eq!(FrameClass::Answer.cap(), wire::LINE_CAP);
        let mut o = VecOut::default();
        assert_eq!(o.try_write("y".repeat(wire::FRAME_CAP + 1), FrameClass::Control), Err(WriteError::TooLarge));
    }

    #[test]
    fn a_resumed_process_counts_cost_over_its_restored_baseline() {
        let usage_reply = |t: &mut T, cost: f64| {
            let gid = t
                .out
                .frames()
                .iter()
                .find(|f| f["request"]["subtype"] == "get_usage")
                .and_then(|f| f["request_id"].as_str().map(str::to_owned))
                .expect("a baseline get_usage after initialize");
            t.line(json!({"type":"control_response","response":{"subtype":"success","request_id":gid,"response":{"session":{"total_cost_usd":cost},"rate_limits":{}}}}));
        };
        let cost = |t: &T| {
            t.fx.iter().rev().find_map(|e| if let Effect::Evidence(SourceFacts::Usage(u)) = e { u.today.cost_micros } else { None })
        };
        let resumed = || {
            let mut c = cfg(probed());
            c.launch = LaunchKind::Continue { session_id: SID.into() };
            T::new(c)
        };
        // Baseline before the first turn.
        let mut t = resumed();
        t.ready();
        assert_eq!(t.out.subtypes(), ["initialize", "get_usage"]);
        usage_reply(&mut t, 0.0024);
        t.sys_init(&[]);
        t.result(json!({"total_cost_usd": 0.0030}));
        t.advance(1_100);
        t.core.tick(&mut t.out, t.now, &mut t.fx);
        assert_eq!(cost(&t), Some(600), "only the increment over the restored cost");
        // The baseline arrives after the first result: that turn is corrected.
        let mut t = resumed();
        t.ready();
        t.sys_init(&[]);
        t.result(json!({"total_cost_usd": 0.0030}));
        usage_reply(&mut t, 0.0024);
        t.advance(1_100);
        t.core.tick(&mut t.out, t.now, &mut t.fx);
        assert_eq!(cost(&t), Some(600));
        // A new session sends no baseline request.
        let mut t = T::with(probed());
        t.ready();
        assert_eq!(t.out.subtypes(), ["initialize"]);
    }

    #[test]
    fn patterns_hot_reload_from_an_override() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("error-patterns.json");
        let src = PatternSource::with_override(path.clone());
        assert!(src.get(1).sign_in(None, "Not logged in"));
        std::fs::write(&path, r#"{"schema":1,"signIn":[{"regex":"(?i)totally different"}]}"#).unwrap();
        assert!(src.get(PatternSource::CHECK_EVERY_MS + 2).sign_in(None, "Totally different"));
        std::fs::write(&path, "garbage").unwrap();
        assert!(src.get(2 * PatternSource::CHECK_EVERY_MS + 4).sign_in(None, "totally different"), "a broken file keeps the last good patterns");
    }
}
