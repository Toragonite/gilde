//! The PTY status engine — source S5 of contract §3.5.1 (§6.2.2, §6.2.4,
//! §6.2.6, §11.2).
//!
//! ## What it reads
//!
//! ```text
//!   output dynamics   reader thread → on_output()       "bytes are flowing"
//!   the grid          pty::ansi text grid               "what the screen says"
//!   screen content    grid digest → ScreenContent        "the cells have not moved"
//!   OS truth (opt.)   a supplied process snapshot        "that group is burning CPU"
//! ```
//!
//! The engine never scans the machine itself (A12.2: one process scan path);
//! the CPU signal is used only when a snapshot is handed to it.
//!
//! ## The table (first match wins)
//!
//! *settled* = no output **or** an unchanged grid for the stated time.
//!
//! | condition | status | confidence |
//! |---|---|---|
//! | the child was reaped / killed | `Exited` / `Killed` | 1.00 |
//! | no byte yet and < 1.5 s since open | *no change* (Starting) | — |
//! | a screen prompt or setup menu is held (parsed on a grid settled ≥ 300 ms, dropped after 300 ms of no match) | `WaitingForInput` | 0.85 |
//! | foreground group computing (only with a fresh supplied snapshot) | `Working` | 0.75 |
//! | output within 500 ms, not our resize repaint | `Working` | 0.90 |
//! | a `working` pattern on screen, settled ≥ `stallAfterMs` | `Stalled` | 0.60 |
//! | a `working` pattern on screen | `Working` | 0.70 |
//! | the vendor's ready signature, settled ≥ 500 ms | `Idle` | 0.90 |
//! | quiet < `idleAfterMs` | *no change* | — |
//! | main screen only: a legacy `idle` pattern | `Idle` | 0.85 |
//! | settled ≥ 2 s, no option run, no cursor menu (the fallback, N-03) | `Idle` | 0.50 |
//! | otherwise | *no change* | — |
//!
//! `WaitingForInput` is never inferred from the alternate screen alone and
//! never from a `waiting` pattern: only from a parsed prompt (§6.2.6, F-48).
//!
//! ## Ticks (§6.2.7, §11.2 rule 1)
//!
//! One ticker for the whole app. It runs every 500 ms only while some PTY is
//! *hot* — output in the last 10 s, a prompt held, or still starting — and
//! every 5 s otherwise; with no PTY at all it sleeps until one registers.
//! Output on a cold PTY wakes it at once.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use dashmap::DashMap;
use parking_lot::{Mutex, RwLock};

use crate::agents::signatures::{AgentSignatures, CompiledPattern, Signatures};
use crate::contract::activity::{PtyFacts, PtyStatus, ScreenPromptFacts};
use crate::model::status::{PatternClass, SessionStatus, SessionStatusReport, StatusEvidence};
use crate::pty::ansi::{AnsiTail, TermQuery, VISIBLE_LINES};
use crate::store::now_ms;
use crate::supervise::procscan::Snapshot;

use super::evidence::*;
use super::ForegroundSource;

/// How long a terminal session's state is kept after it ends.
pub const PRUNE_AFTER_MS: i64 = 5 * 60 * 1_000;

/// How stale a supplied process snapshot may be before its CPU numbers are
/// ignored.
pub const SNAPSHOT_MAX_AGE_MS: i64 = 2_000;

/// While a status holds, its facts are re-published at most this often when
/// `last_output_at` moved, so World's No-progress clock (R8) sees the output.
pub const FACTS_REFRESH_MS: i64 = 5_000;

// ---------------------------------------------------------------------------
// where the patterns come from
// ---------------------------------------------------------------------------

/// The hot-reloadable signature set, read fresh on every evaluation.
pub trait SignatureSource: Send + Sync {
    fn signatures(&self) -> Arc<Signatures>;
}

impl SignatureSource for crate::agents::AgentRegistry {
    fn signatures(&self) -> Arc<Signatures> {
        crate::agents::AgentRegistry::signatures(self)
    }
}

/// A [`SignatureSource`] holding one compiled set, swappable at will.
#[derive(Debug)]
pub struct StaticSignatures(RwLock<Arc<Signatures>>);

impl StaticSignatures {
    pub fn new(signatures: Signatures) -> StaticSignatures {
        StaticSignatures(RwLock::new(Arc::new(signatures)))
    }

    /// The compiled built-in defaults.
    pub fn defaults() -> StaticSignatures {
        StaticSignatures::new(Signatures::compile(
            &crate::agents::signatures::SignatureFile::defaults(),
        ))
    }

    pub fn set(&self, signatures: Signatures) {
        *self.0.write() = Arc::new(signatures);
    }
}

impl SignatureSource for StaticSignatures {
    fn signatures(&self) -> Arc<Signatures> {
        self.0.read().clone()
    }
}

/// Told about every published change of a session's S5 facts (the PTY
/// manager turns these into `EvidenceUpdate`s for World).
pub trait StatusListener: Send + Sync {
    fn facts_changed(&self, session_id: &str, facts: &PtyFacts);
}

/// A copy of one session's visible grid, for the screen-prompt actions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenView {
    pub lines: Vec<String>,
    pub cols: usize,
    pub rows: usize,
    pub cursor: (usize, usize),
    pub generation: u64,
    pub alt_screen: bool,
    pub app_cursor_keys: bool,
}

// ---------------------------------------------------------------------------
// per-session state
// ---------------------------------------------------------------------------

struct SessionState {
    /// `agent-signatures.json` key.
    agent_key: String,
    idle_after_ms: u64,
    stall_after_ms: u64,
    opened_at: i64,

    tail: AnsiTail,

    status: SessionStatus,
    confidence: f32,
    evidence: Vec<StatusEvidence>,
    since: i64,
    last_change_at: i64,

    // -- output dynamics ----------------------------------------------------
    /// 0 until the first byte.
    last_output_at: i64,
    window_started: i64,
    window_bytes: u64,
    last_rate: u64,

    // -- OS truth (only when supplied) --------------------------------------
    foreground: Option<ForegroundSample>,
    cpu: CpuLatch,

    // -- screen content -----------------------------------------------------
    content: ScreenContent,
    resized_at: i64,

    // -- bell ---------------------------------------------------------------
    attention: bool,
    bell_pending: bool,

    // -- screen prompts (§6.2.4) --------------------------------------------
    /// The prompt the session is held at.
    prompt: Option<ScreenPromptFacts>,
    /// When the grid first stopped matching the held prompt.
    prompt_missing_since: Option<i64>,
    /// Grid generation of the last scan, and what it found.
    scanned_gen: Option<u64>,
    last_scan: Option<ScreenPromptFacts>,

    // -- publication ----------------------------------------------------------
    published: Option<PtyFacts>,
    published_at: i64,

    ended_at: Option<i64>,
}

impl SessionState {
    fn sized(agent_key: String, idle_after_ms: u64, tail: AnsiTail, now: i64) -> SessionState {
        SessionState {
            agent_key,
            idle_after_ms: idle_after_ms.max(1),
            stall_after_ms: DEFAULT_STALL_AFTER_MS,
            opened_at: now,
            tail,
            status: SessionStatus::Starting,
            confidence: CONF_STARTING,
            evidence: vec![StatusEvidence::OutputQuiet { ms: 0 }],
            since: now,
            last_change_at: now,
            last_output_at: 0,
            window_started: now,
            window_bytes: 0,
            last_rate: 0,
            foreground: None,
            cpu: CpuLatch::default(),
            content: ScreenContent::new(now),
            resized_at: 0,
            attention: false,
            bell_pending: false,
            prompt: None,
            prompt_missing_since: None,
            scanned_gen: None,
            last_scan: None,
            published: None,
            published_at: 0,
            ended_at: None,
        }
    }

    fn with_stall(mut self, stall_after_ms: u64) -> SessionState {
        self.stall_after_ms = stall_after_ms.max(1);
        self
    }

    fn report(&self) -> SessionStatusReport {
        SessionStatusReport {
            status: self.status.clone(),
            confidence: self.confidence,
            evidence: self.evidence.clone(),
            since: self.since,
        }
    }

    fn ended(&self) -> bool {
        self.status.is_terminal() || matches!(self.status, SessionStatus::Detached)
    }

    fn quiet_ms(&self, now: i64) -> u64 {
        let from = if self.last_output_at > 0 {
            self.last_output_at
        } else {
            self.opened_at
        };
        (now - from).max(0) as u64
    }

    /// How long nothing new reached the screen: silence or an unchanged grid,
    /// whichever is longer (a TUI that repaints the same frame is settled).
    fn settled_ms(&self, now: i64) -> u64 {
        self.quiet_ms(now).max(self.content.stable_ms(now))
    }

    /// Hot = the ticker runs at 500 ms for it (§11.2 rule 1).
    fn hot(&self, now: i64) -> bool {
        if self.ended() {
            return false;
        }
        matches!(self.status, SessionStatus::Starting)
            || self.prompt.is_some()
            || (self.last_output_at > 0 && now - self.last_output_at <= HOT_OUTPUT_MS)
    }

    fn observe_output(&mut self, bytes: &[u8], now: i64) {
        self.tail.feed(bytes);
        if self.tail.take_bell() {
            self.attention = true;
            self.bell_pending = true;
        }
        self.last_output_at = now;
        self.window_bytes += bytes.len() as u64;
        let elapsed = (now - self.window_started).max(0);
        if elapsed >= RATE_WINDOW_MS {
            self.last_rate = self.window_bytes.saturating_mul(1_000) / elapsed.max(1) as u64;
            self.window_started = now;
            self.window_bytes = 0;
        }
    }

    fn sample_content(&mut self, now: i64) {
        if !self.content.due(now) {
            return;
        }
        let rows = self.tail.screen_lines();
        let cursor = self.tail.cursor();
        if self.repainting_after_resize(now) {
            // The screen changed because *we* resized it (PINNED §36).
            self.content.reanchor(rows, cursor, now);
            return;
        }
        self.content.observe(rows, cursor, now);
    }

    fn repainting_after_resize(&self, now: i64) -> bool {
        self.resized_at > 0 && now - self.resized_at <= RESIZE_REPAINT_MS
    }

    fn cpu_says_working(&self, now: i64) -> bool {
        if !self.cpu.busy() {
            return false;
        }
        if self.content.stable_ms(now) < ALT_SCREEN_QUIET_MS {
            return true;
        }
        self.foreground
            .map(|sample| sample.cpu_pct >= CPU_STILL_SCREEN_PCT)
            .unwrap_or(true)
    }

    /// Keep the held prompt in step with the grid (§6.2.4): a new prompt is
    /// adopted only on a grid settled ≥ 300 ms; a held prompt is re-checked on
    /// every change and dropped once the grid has not matched it for 300 ms.
    fn track_prompt(&mut self, now: i64, signatures: Option<&Signatures>) {
        let grammar = signatures.and_then(|s| {
            s.prompt_card(&self.agent_key).map(|card| PromptGrammar {
                card,
                trust: s.trust_dialog(&self.agent_key),
                ready: s.ready_signature(&self.agent_key),
            })
        });
        let Some(grammar) = grammar else {
            // A vendor without a grammar never gets a card.
            self.prompt = None;
            self.prompt_missing_since = None;
            return;
        };
        let settled = self.settled_ms(now) >= PROMPT_SETTLE_MS;
        let generation = self.tail.generation();
        if self.scanned_gen != Some(generation) {
            if self.prompt.is_some() || settled {
                let (cols, _) = self.tail.size();
                self.last_scan = parse_screen_prompt(&self.tail.screen_lines(), cols, grammar, now);
                self.scanned_gen = Some(generation);
            } else {
                // The grid moved on and is not worth a scan yet.
                self.last_scan = None;
            }
        }

        match (&self.last_scan, &self.prompt) {
            (Some(found), Some(held)) if found.prompt_rev == held.prompt_rev => {
                let mut found = found.clone();
                found.first_seen_at = held.first_seen_at;
                self.prompt = Some(found);
                self.prompt_missing_since = None;
            }
            (Some(found), _) if settled => {
                self.prompt = Some(found.clone());
                self.prompt_missing_since = None;
            }
            (_, Some(_)) => {
                let since = *self.prompt_missing_since.get_or_insert(now);
                if now - since >= PROMPT_GONE_MS {
                    self.prompt = None;
                    self.prompt_missing_since = None;
                }
            }
            _ => {}
        }
    }

    fn evaluate(&mut self, now: i64, signatures: Option<&Signatures>) -> Option<Verdict> {
        let (verdict, rule) = self.decide(now, signatures);
        tracing::debug!(
            target: "status",
            rule,
            state = verdict.as_ref().map(|v| v.status.kind()).unwrap_or("<unchanged>"),
            held = self.status.kind(),
            quiet_ms = self.quiet_ms(now),
            stable_ms = self.content.stable_ms(now),
            prompt = self.prompt.is_some(),
            alt_screen = self.tail.alt_screen(),
            "evaluate"
        );
        verdict
    }

    fn decide(&mut self, now: i64, signatures: Option<&Signatures>) -> (Option<Verdict>, &'static str) {
        if self.ended() {
            return (None, "ended");
        }
        self.sample_content(now);
        let quiet = self.quiet_ms(now);
        let settled = self.settled_ms(now);

        if matches!(self.status, SessionStatus::Starting)
            && self.last_output_at == 0
            && now - self.opened_at < STARTING_GRACE_MS
        {
            return (None, "starting-grace");
        }

        // A held screen prompt or setup menu: the only WaitingForInput.
        self.track_prompt(now, signatures);
        if let Some(prompt) = &self.prompt {
            let verdict = Verdict::new(
                SessionStatus::WaitingForInput {
                    prompt: prompt.prompt_rev.clone(),
                },
                CONF_SCREEN_PROMPT,
                vec![
                    StatusEvidence::ScreenPrompt {
                        options: prompt.options.len().min(u8::MAX as usize) as u8,
                        setup: prompt.setup,
                    },
                    StatusEvidence::OutputQuiet { ms: quiet },
                ],
            );
            return (Some(self.decorate(verdict, quiet, now)), "screen-prompt");
        }

        // OS truth, when someone supplied a snapshot.
        if self.cpu_says_working(now) {
            if let Some(sample) = self.foreground {
                let verdict = Verdict::new(
                    SessionStatus::Working { since: 0, hint: None },
                    CONF_FOREGROUND_CPU,
                    vec![sample.evidence(), StatusEvidence::OutputQuiet { ms: quiet }],
                );
                return (Some(self.decorate(verdict, quiet, now)), "cpu");
            }
        }

        // The reactivity path.
        if self.last_output_at > 0
            && now - self.last_output_at <= OUTPUT_BURST_MS
            && !self.repainting_after_resize(now)
        {
            let verdict = Verdict::new(
                SessionStatus::Working { since: 0, hint: None },
                CONF_OUTPUT_BURST,
                vec![StatusEvidence::OutputBurst {
                    bytes_per_s: self.last_rate.max(self.window_bytes),
                }],
            );
            return (Some(self.decorate(verdict, quiet, now)), "output-burst");
        }

        let agent = signatures.and_then(|s| s.for_agent(&self.agent_key));
        let lines = self.tail.visible_lines(VISIBLE_LINES);

        // An activity line still on screen.
        if let Some(agent) = agent {
            if let Some((pattern, line)) = match_bucket(&agent.working, &lines) {
                let matched = StatusEvidence::PatternMatch {
                    signature_id: pattern.id.clone(),
                    class: PatternClass::Working,
                    line: clip_line(line.trim()),
                };
                let (verdict, rule) = if settled >= self.stall_after_ms {
                    (
                        Verdict::new(
                            SessionStatus::Stalled { quiet_ms: settled },
                            CONF_STALLED,
                            vec![matched, StatusEvidence::ScreenStable { ms: settled }],
                        ),
                        "tail-working-stalled",
                    )
                } else {
                    (
                        Verdict::new(
                            SessionStatus::Working {
                                since: 0,
                                hint: Some(clip_line(line.trim())),
                            },
                            CONF_WORKING_PATTERN,
                            vec![matched],
                        ),
                        "tail-working",
                    )
                };
                return (Some(self.decorate(verdict, quiet, now)), rule);
            }
        }

        // The vendor's own input line on a settled grid (§6.2.6).
        let ready = signatures.and_then(|s| s.ready_signature(&self.agent_key));
        if let Some(ready) = ready {
            if settled >= READY_SETTLE_MS && ready.matches(&self.tail.screen_lines()) {
                let verdict = Verdict::new(
                    SessionStatus::Idle,
                    CONF_READY_SIGNATURE,
                    vec![StatusEvidence::ReadySignature, StatusEvidence::OutputQuiet { ms: quiet }],
                );
                return (Some(self.decorate(verdict, quiet, now)), "ready-signature");
            }
        }

        if quiet < self.idle_after_ms {
            return (None, "below-idle-after");
        }

        // Main-screen CLIs (a shell pane) keep their legacy idle patterns; an
        // alternate-screen TUI needs the ready signature (F-48).
        if !self.tail.alt_screen() {
            if let Some(agent) = agent {
                if let Some((pattern, line)) = match_bucket(&agent.idle, &lines) {
                    let verdict = Verdict::new(
                        SessionStatus::Idle,
                        CONF_IDLE_PATTERN,
                        vec![
                            StatusEvidence::PatternMatch {
                                signature_id: pattern.id.clone(),
                                class: PatternClass::Idle,
                                line: clip_line(line.trim()),
                            },
                            StatusEvidence::OutputQuiet { ms: quiet },
                        ],
                    );
                    return (Some(self.decorate(verdict, quiet, now)), "tail-idle");
                }
            }
        }

        // The settled-grid fallback (N-03): no option run, no cursor menu (or
        // the prompt rule above would have fired), nothing moving.
        if settled >= FALLBACK_SETTLE_MS {
            let verdict = Verdict::new(
                SessionStatus::Idle,
                CONF_SETTLED_FALLBACK,
                vec![StatusEvidence::SettledGrid { ms: settled }],
            );
            return (Some(self.decorate(verdict, quiet, now)), "settled-fallback");
        }
        (None, "unsettled")
    }

    fn decorate(&self, verdict: Verdict, quiet: u64, now: i64) -> Verdict {
        let mut verdict = verdict;
        if self.tail.alt_screen() {
            verdict = verdict.with(StatusEvidence::AltScreen { on: true });
        }
        let stable = self.content.stable_ms(now);
        if stable >= ALT_SCREEN_QUIET_MS {
            verdict = verdict.with(StatusEvidence::ScreenStable { ms: stable });
        }
        if self.bell_pending {
            verdict = verdict.with(StatusEvidence::Bell);
        }
        verdict.ensure_evidence(quiet)
    }

    /// Apply a verdict, honouring the 300 ms debounce. Returns `true` when the
    /// status changed in a way listeners must hear about.
    fn apply(&mut self, verdict: Verdict, now: i64) -> bool {
        let same_kind = self.status.kind() == verdict.status.kind();
        if !same_kind && !bypasses_debounce(&verdict.status) && now - self.last_change_at < DEBOUNCE_MS {
            return false;
        }
        let mut status = verdict.status;
        if let (SessionStatus::Working { since, .. }, SessionStatus::Working { since: previous, .. }) =
            (&mut status, &self.status)
        {
            *since = if same_kind { *previous } else { now };
        } else if let SessionStatus::Working { since, .. } = &mut status {
            *since = now;
        }
        let significant = is_significant(&self.status, self.confidence, &status, verdict.confidence);
        if !same_kind {
            self.since = now;
            self.last_change_at = now;
        }
        self.status = status;
        self.confidence = verdict.confidence;
        self.evidence = verdict.evidence;
        let bell = std::mem::take(&mut self.bell_pending);
        significant || bell
    }

    fn settle(&mut self, status: SessionStatus, evidence: Vec<StatusEvidence>, now: i64) -> bool {
        if self.ended() {
            return false;
        }
        self.since = now;
        self.last_change_at = now;
        self.ended_at = Some(now);
        self.status = status;
        self.confidence = CONF_EXITED;
        self.prompt = None;
        self.prompt_missing_since = None;
        let mut evidence = evidence;
        if evidence.is_empty() {
            evidence.push(StatusEvidence::OutputQuiet { ms: self.quiet_ms(now) });
        }
        if std::mem::take(&mut self.bell_pending) {
            evidence.push(StatusEvidence::Bell);
        }
        self.evidence = evidence;
        true
    }

    /// The S5 facts for the current state; `None` while starting or after the
    /// end (process facts carry the end, S6).
    fn facts(&self) -> Option<PtyFacts> {
        let status = match &self.status {
            SessionStatus::Idle => PtyStatus::Idle,
            SessionStatus::Working { .. } => PtyStatus::Working,
            SessionStatus::WaitingForInput { .. } => PtyStatus::WaitingForInput,
            SessionStatus::Stalled { .. } => PtyStatus::Stalled,
            _ => return None,
        };
        Some(PtyFacts {
            status,
            confidence: self.confidence,
            last_output_at: (self.last_output_at > 0).then_some(self.last_output_at),
            prompt: self.prompt.clone(),
        })
    }

    /// The facts to publish now, if they differ from what was published in a
    /// way World cares about.
    fn take_publishable(&mut self, now: i64) -> Option<PtyFacts> {
        let facts = self.facts()?;
        let due = match &self.published {
            None => true,
            Some(old) => {
                old.status != facts.status
                    || (old.confidence - facts.confidence).abs() > f32::EPSILON
                    || old.prompt != facts.prompt
                    || (old.last_output_at != facts.last_output_at && now - self.published_at >= FACTS_REFRESH_MS)
            }
        };
        if !due {
            return None;
        }
        self.published = Some(facts.clone());
        self.published_at = now;
        Some(facts)
    }

    fn screen_view(&self) -> ScreenView {
        let (cols, rows) = self.tail.size();
        ScreenView {
            lines: self.tail.screen_lines(),
            cols,
            rows,
            cursor: self.tail.cursor(),
            generation: self.tail.generation(),
            alt_screen: self.tail.alt_screen(),
            app_cursor_keys: self.tail.app_cursor_keys(),
        }
    }
}

/// One class of patterns against the visible tail, newest line first.
fn match_bucket<'a>(bucket: &'a [CompiledPattern], lines: &[String]) -> Option<(&'a CompiledPattern, String)> {
    for line in lines.iter().rev() {
        if let Some(hit) = bucket.iter().find(|p| p.regex.is_match(line)) {
            return Some((hit, line.clone()));
        }
    }
    None
}

/// The readiness threshold for a vendor (§6.2.2, N-03): 0.6 with a verified
/// ready signature, else 0.5 (the settled-grid fallback counts).
pub fn ready_threshold(signatures: &Signatures, agent_key: &str) -> f32 {
    match signatures.ready_signature(agent_key) {
        Some(sig) if sig.verified => READY_CONFIDENCE,
        _ => READY_CONFIDENCE_FALLBACK,
    }
}

/// Kept for tests that build an agent's patterns by hand.
pub fn agent_patterns(signatures: &Signatures, key: &str) -> Option<AgentSignatures> {
    signatures.for_agent(key).cloned()
}

// ---------------------------------------------------------------------------
// the engine
// ---------------------------------------------------------------------------

/// One per app. Holds every session's inference state.
pub struct StatusEngine {
    sessions: DashMap<String, Mutex<SessionState>>,
    watchers: DashMap<String, tokio::sync::watch::Sender<Option<PtyFacts>>>,
    foreground: DashMap<String, Weak<dyn ForegroundSource>>,
    signatures: RwLock<Option<Arc<dyn SignatureSource>>>,
    listener: RwLock<Option<Arc<dyn StatusListener>>>,
    snapshot: RwLock<Option<Arc<Snapshot>>>,
    snapshot_at: AtomicU64,
    stall_after_ms: AtomicU64,
    /// Wakes the ticker: a session registered, or a cold PTY spoke.
    wake: tokio::sync::Notify,
    /// A ticker is running for this engine (exactly one).
    ticking: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for StatusEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StatusEngine")
            .field("sessions", &self.sessions.len())
            .finish()
    }
}

impl Default for StatusEngine {
    fn default() -> Self {
        StatusEngine::new()
    }
}

impl StatusEngine {
    /// A working engine with the **bundled** signature set and no wiring.
    pub fn new() -> StatusEngine {
        StatusEngine {
            sessions: DashMap::new(),
            watchers: DashMap::new(),
            foreground: DashMap::new(),
            signatures: RwLock::new(Some(Arc::new(StaticSignatures::defaults()))),
            listener: RwLock::new(None),
            snapshot: RwLock::new(None),
            snapshot_at: AtomicU64::new(0),
            stall_after_ms: AtomicU64::new(DEFAULT_STALL_AFTER_MS),
            wake: tokio::sync::Notify::new(),
            ticking: std::sync::atomic::AtomicBool::new(false),
        }
    }

    // -- wiring --------------------------------------------------------------

    pub fn set_signatures(&self, source: Arc<dyn SignatureSource>) {
        *self.signatures.write() = Some(source);
    }

    pub fn set_listener(&self, listener: Arc<dyn StatusListener>) {
        *self.listener.write() = Some(listener);
    }

    pub fn set_stall_after_ms(&self, ms: u64) {
        self.stall_after_ms.store(ms.max(1), Ordering::Relaxed);
    }

    pub fn signature_set(&self) -> Option<Arc<Signatures>> {
        self.signatures.read().as_ref().map(|s| s.signatures())
    }

    // -- session lifecycle ---------------------------------------------------

    /// Start tracking a session with the default 120×40 grid.
    pub fn register(&self, session_id: &str, agent_key: &str, idle_after_ms: u64, at: i64) {
        self.register_sized(session_id, agent_key, idle_after_ms, 120, 40, at);
    }

    /// Start tracking a session whose PTY is `cols`×`rows`. Called before the
    /// reader thread exists, so no output can arrive first.
    pub fn register_sized(
        &self,
        session_id: &str,
        agent_key: &str,
        idle_after_ms: u64,
        cols: u16,
        rows: u16,
        at: i64,
    ) {
        let state = SessionState::sized(
            agent_key.to_string(),
            idle_after_ms,
            AnsiTail::with_size(cols, rows),
            at,
        )
        .with_stall(self.stall_after_ms.load(Ordering::Relaxed));
        self.sessions.insert(session_id.to_string(), Mutex::new(state));
        let (tx, _rx) = tokio::sync::watch::channel(None);
        self.watchers.insert(session_id.to_string(), tx);
        self.wake.notify_one();
    }

    pub fn attach_foreground(&self, session_id: &str, source: &Arc<dyn ForegroundSource>) {
        self.foreground.insert(session_id.to_string(), Arc::downgrade(source));
    }

    pub fn forget(&self, session_id: &str) {
        self.sessions.remove(session_id);
        self.watchers.remove(session_id);
        self.foreground.remove(session_id);
    }

    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    pub fn tracks(&self, session_id: &str) -> bool {
        self.sessions.contains_key(session_id)
    }

    /// The signature key the session was registered with.
    pub fn agent_key(&self, session_id: &str) -> Option<String> {
        self.sessions
            .get(session_id)
            .map(|entry| entry.lock().agent_key.clone())
    }

    // -- inputs --------------------------------------------------------------

    /// One run of pty bytes, straight off the reader thread. Returns the
    /// terminal queries (DA1, DSR) the bytes carried, in order, with the reply
    /// state captured at each (§6.2.6).
    pub fn on_output(&self, session_id: &str, bytes: &[u8]) -> Vec<TermQuery> {
        self.feed_at(session_id, bytes, now_ms())
    }

    /// [`StatusEngine::on_output`] at an explicit clock — the deterministic
    /// replay of recorded streams (no sleeping in tests, A10.7).
    pub fn feed_at(&self, session_id: &str, bytes: &[u8], now: i64) -> Vec<TermQuery> {
        let signatures = self.signature_set();
        let (emit, queries, was_cold) = {
            let Some(entry) = self.sessions.get(session_id) else {
                return Vec::new();
            };
            let mut state = entry.lock();
            let was_cold = !state.hot(now);
            state.observe_output(bytes, now);
            let queries = state.tail.take_queries();
            (self.step(&mut state, now, signatures.as_deref()), queries, was_cold)
        };
        if was_cold {
            self.wake.notify_one();
        }
        self.publish(session_id, emit);
        queries
    }

    /// The pty was resized: the grid follows, and the repaint that answers
    /// `SIGWINCH` is ours, not the agent's (PINNED §36).
    pub fn on_resize(&self, session_id: &str, cols: u16, rows: u16) {
        self.resize_at(session_id, cols, rows, now_ms());
    }

    /// [`StatusEngine::on_resize`] at an explicit clock (replays).
    pub fn resize_at(&self, session_id: &str, cols: u16, rows: u16, now: i64) {
        if let Some(entry) = self.sessions.get(session_id) {
            let mut state = entry.lock();
            state.tail.resize(cols, rows);
            state.resized_at = now;
        }
    }

    /// The user typed. Not evidence about the agent.
    pub fn on_input(&self, _session_id: &str) {}

    pub fn on_exit(&self, session_id: &str, code: Option<i32>, signal: Option<i32>) {
        self.settle(
            session_id,
            SessionStatus::Exited { code, signal },
            vec![StatusEvidence::ChildExited { code }],
        );
    }

    pub fn on_killed(&self, session_id: &str) {
        self.settle(
            session_id,
            SessionStatus::Killed,
            vec![StatusEvidence::ChildExited { code: None }],
        );
    }

    pub fn on_failed(&self, session_id: &str, reason: impl Into<String>) {
        self.settle(session_id, SessionStatus::Failed { reason: reason.into() }, Vec::new());
    }

    fn settle(&self, session_id: &str, status: SessionStatus, evidence: Vec<StatusEvidence>) {
        let now = now_ms();
        if let Some(entry) = self.sessions.get(session_id) {
            entry.lock().settle(status, evidence, now);
        }
    }

    // -- attention -----------------------------------------------------------

    pub fn attention(&self, session_id: &str) -> bool {
        self.sessions
            .get(session_id)
            .map(|entry| entry.lock().attention)
            .unwrap_or(false)
    }

    pub fn clear_attention(&self, session_id: &str) -> bool {
        let Some(entry) = self.sessions.get(session_id) else {
            return false;
        };
        let mut state = entry.lock();
        let had = state.attention;
        state.attention = false;
        state.bell_pending = false;
        had
    }

    // -- reads ----------------------------------------------------------------

    /// The current S5 facts (re-evaluated first).
    pub fn facts(&self, session_id: &str) -> Option<PtyFacts> {
        self.tick_session(session_id);
        self.sessions.get(session_id).and_then(|entry| entry.lock().facts())
    }

    /// The facts as of the last evaluation (no re-evaluation).
    pub fn last_facts(&self, session_id: &str) -> Option<PtyFacts> {
        self.sessions.get(session_id).and_then(|entry| entry.lock().facts())
    }

    /// Facts as they change (the PTY driver waits on this for readiness).
    pub fn subscribe(&self, session_id: &str) -> Option<tokio::sync::watch::Receiver<Option<PtyFacts>>> {
        self.watchers.get(session_id).map(|tx| tx.subscribe())
    }

    /// The held screen prompt, if any.
    pub fn prompt(&self, session_id: &str) -> Option<ScreenPromptFacts> {
        self.sessions
            .get(session_id)
            .and_then(|entry| entry.lock().prompt.clone())
    }

    /// The grid as it is now.
    pub fn screen(&self, session_id: &str) -> Option<ScreenView> {
        self.sessions
            .get(session_id)
            .map(|entry| entry.lock().screen_view())
    }

    /// Parse the grid **now**, ignoring the settle rule — the TOCTOU re-read
    /// of `screen_prompt_key` (§6.2.4). Outer `None` = unknown session.
    pub fn rescan(&self, session_id: &str) -> Option<Option<ScreenPromptFacts>> {
        let signatures = self.signature_set();
        let entry = self.sessions.get(session_id)?;
        let state = entry.lock();
        let Some(sigs) = signatures.as_deref() else {
            return Some(None);
        };
        let Some(card) = sigs.prompt_card(&state.agent_key) else {
            return Some(None);
        };
        let grammar = PromptGrammar {
            card,
            trust: sigs.trust_dialog(&state.agent_key),
            ready: sigs.ready_signature(&state.agent_key),
        };
        let (cols, _) = state.tail.size();
        let found = parse_screen_prompt(&state.tail.screen_lines(), cols, grammar, now_ms());
        Some(found.map(|mut f| {
            if let Some(held) = state.prompt.as_ref().filter(|h| h.prompt_rev == f.prompt_rev) {
                f.first_seen_at = held.first_seen_at;
            }
            f
        }))
    }

    /// The pane title (OSC 0/2), sanitized. Memory only (A12.4).
    pub fn title(&self, session_id: &str) -> Option<String> {
        self.sessions
            .get(session_id)
            .and_then(|entry| entry.lock().tail.title().map(str::to_string))
    }

    /// The readiness threshold of this session's vendor (§6.2.2).
    pub fn ready_threshold(&self, session_id: &str) -> f32 {
        let key = self.agent_key(session_id).unwrap_or_default();
        match self.signature_set() {
            Some(sigs) => ready_threshold(&sigs, &key),
            None => READY_CONFIDENCE_FALLBACK,
        }
    }

    /// The legacy report (status, confidence, evidence) for diagnostics.
    pub fn report(&self, session_id: &str) -> SessionStatusReport {
        self.tick_session(session_id);
        self.last_report(session_id)
    }

    /// The report as of the last evaluation (no re-evaluation).
    pub fn last_report(&self, session_id: &str) -> SessionStatusReport {
        match self.sessions.get(session_id) {
            Some(entry) => entry.lock().report(),
            None => SessionStatusReport {
                status: SessionStatus::Starting,
                confidence: CONF_STARTING,
                evidence: vec![StatusEvidence::OutputQuiet { ms: 0 }],
                since: now_ms(),
            },
        }
    }

    // -- evaluation ----------------------------------------------------------

    /// Re-evaluate one session with no new input.
    pub fn tick_session(&self, session_id: &str) -> bool {
        self.tick_at(session_id, now_ms())
    }

    /// [`StatusEngine::tick_session`] at an explicit clock (replays).
    pub fn tick_at(&self, session_id: &str, now: i64) -> bool {
        let signatures = self.signature_set();
        let emit = {
            let Some(entry) = self.sessions.get(session_id) else {
                return false;
            };
            let mut state = entry.lock();
            self.step(&mut state, now, signatures.as_deref())
        };
        let changed = emit.is_some();
        self.publish(session_id, emit);
        changed
    }

    /// Re-evaluate every session (the ticker), feeding CPU from a supplied
    /// snapshot when one is fresh.
    pub fn tick_all(&self) {
        let now = now_ms();
        let snapshot = self.snapshot().filter(|_| !self.snapshot_is_stale(now));
        match snapshot {
            Some(snap) => self.tick(&snap),
            None => {
                let ids: Vec<String> = self.sessions.iter().map(|e| e.key().clone()).collect();
                for id in ids {
                    self.tick_session(&id);
                }
                self.prune(now);
            }
        }
    }

    /// One evaluation of every session with the OS half of the truth from
    /// `snap` (a snapshot somebody else took; the engine never scans).
    pub fn tick(&self, snap: &Snapshot) {
        let now = now_ms();
        let signatures = self.signature_set();
        let ids: Vec<String> = self.sessions.iter().map(|e| e.key().clone()).collect();
        for id in ids {
            let sample = self.sample_foreground(&id, snap);
            let emit = {
                let Some(entry) = self.sessions.get(&id) else {
                    continue;
                };
                let mut state = entry.lock();
                if let Some(sample) = sample {
                    state.cpu.observe(sample.cpu_pct);
                    state.foreground = Some(sample);
                }
                self.step(&mut state, now, signatures.as_deref())
            };
            self.publish(&id, emit);
        }
        self.prune(now);
    }

    fn sample_foreground(&self, session_id: &str, snap: &Snapshot) -> Option<ForegroundSample> {
        let source = self.foreground.get(session_id)?.upgrade()?;
        let pgid = source.foreground_pgid()?;
        let pids = snap.pids_in_group(pgid);
        if pids.is_empty() {
            return None;
        }
        let mut cpu = 0.0f32;
        let mut rss = 0u64;
        for pid in pids {
            if let Some(row) = snap.get(pid) {
                cpu += row.cpu_pct;
                rss = rss.saturating_add(row.rss_bytes());
            }
        }
        Some(ForegroundSample {
            pgid,
            cpu_pct: cpu,
            rss_bytes: rss,
            at: snap.at(),
        })
    }

    /// Evaluate + apply; the facts to publish, if any.
    fn step(&self, state: &mut SessionState, now: i64, signatures: Option<&Signatures>) -> Option<PtyFacts> {
        if let Some(verdict) = state.evaluate(now, signatures) {
            state.apply(verdict, now);
        }
        state.take_publishable(now)
    }

    fn publish(&self, session_id: &str, facts: Option<PtyFacts>) {
        let Some(facts) = facts else { return };
        if let Some(tx) = self.watchers.get(session_id) {
            tx.send_replace(Some(facts.clone()));
        }
        let listener = self.listener.read().clone();
        if let Some(listener) = listener {
            listener.facts_changed(session_id, &facts);
        }
    }

    fn prune(&self, now: i64) {
        let stale: Vec<String> = self
            .sessions
            .iter()
            .filter(|e| e.value().lock().ended_at.is_some_and(|at| now - at > PRUNE_AFTER_MS))
            .map(|e| e.key().clone())
            .collect();
        for id in stale {
            self.forget(&id);
        }
    }

    // -- supplied process snapshots -----------------------------------------

    /// Hand the engine a snapshot somebody else already paid for.
    pub fn set_snapshot(&self, snap: Arc<Snapshot>) {
        self.snapshot_at.store(now_ms().max(0) as u64, Ordering::Relaxed);
        *self.snapshot.write() = Some(snap);
    }

    pub fn snapshot(&self) -> Option<Arc<Snapshot>> {
        self.snapshot.read().clone()
    }

    pub fn snapshot_is_stale(&self, now: i64) -> bool {
        let at = self.snapshot_at.load(Ordering::Relaxed) as i64;
        at == 0 || now - at > SNAPSHOT_MAX_AGE_MS
    }

    // -- the ticker ----------------------------------------------------------

    /// How long the ticker sleeps next: 500 ms with a hot PTY, 5 s with only
    /// cold ones, `None` (until woken) with none (§11.2 rule 1).
    pub fn next_tick(&self, now: i64) -> Option<Duration> {
        if self.sessions.is_empty() {
            return None;
        }
        let hot = self.sessions.iter().any(|e| e.value().lock().hot(now));
        Some(Duration::from_millis(if hot { TICK_MS } else { QUIET_TICK_MS }))
    }

    /// Start the ticker unless one runs already (a manager with a private
    /// engine calls this on its first open).
    pub fn ensure_ticker(engine: &Arc<StatusEngine>) {
        if !engine.ticking.load(Ordering::Acquire) {
            drop(StatusEngine::spawn_ticker(engine.clone()));
        }
    }

    /// The one evaluation loop. Never takes a process scan of its own.
    pub fn spawn_ticker(engine: Arc<StatusEngine>) -> tauri::async_runtime::JoinHandle<()> {
        engine.ticking.store(true, Ordering::Release);
        tauri::async_runtime::spawn(async move {
            loop {
                match engine.next_tick(now_ms()) {
                    None => engine.wake.notified().await,
                    Some(delay) => {
                        tokio::select! {
                            _ = tokio::time::sleep(delay) => {}
                            _ = engine.wake.notified() => {}
                        }
                    }
                }
                engine.tick_all();
            }
        })
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::signatures::SignatureFile;

    fn signatures() -> Arc<Signatures> {
        Arc::new(Signatures::compile(&SignatureFile::defaults()))
    }

    fn state(agent: &str, now: i64) -> SessionState {
        SessionState::sized(agent.to_string(), 1_200, AnsiTail::with_size(80, 24), now)
            .with_stall(DEFAULT_STALL_AFTER_MS)
    }

    fn advance(state: &mut SessionState, now: i64, sigs: &Signatures) {
        if let Some(verdict) = state.evaluate(now, Some(sigs)) {
            state.apply(verdict, now);
        }
    }

    /// Evaluate at a steady cadence from `from` to `to`, the way the ticker
    /// does.
    fn run(state: &mut SessionState, from: i64, to: i64, sigs: &Signatures) {
        let mut t = from;
        while t <= to {
            advance(state, t, sigs);
            t += 100;
        }
    }

    const CLAUDE_IDLE: &str = "\x1b[?1049h\x1b[2J\x1b[H Claude Code (fake)\x1b[20;1H────────\r\n❯ Try \"hello\"\x1b[22;1H────────\x1b[23;1H  ⏸ manual mode on · ? for shortcuts";

    const PERMISSION: &str = "\x1b[?1049h\x1b[2J\x1b[H\x1b[14;1H Bash command\x1b[16;1H Do you want to proceed?\r\n ❯ 1. Yes\r\n   2. Yes, and switch to auto mode\r\n   3. No\r\n\r\n Esc to cancel";

    #[test]
    fn starting_holds_for_the_grace_then_the_fallback_needs_a_settled_grid() {
        let sigs = signatures();
        let mut s = state("agy", 1_000);
        advance(&mut s, 1_400, &sigs);
        assert!(matches!(s.status, SessionStatus::Starting));
        // Past the grace with nothing on screen: settled ≥ 2 s → the fallback.
        run(&mut s, 1_500, 3_100, &sigs);
        assert!(matches!(s.status, SessionStatus::Idle), "{:?}", s.status);
        assert_eq!(s.confidence, CONF_SETTLED_FALLBACK);
    }

    #[test]
    fn output_moves_a_session_to_working_immediately() {
        let sigs = signatures();
        let mut s = state("claude", 0);
        s.observe_output(b"hello", 100);
        advance(&mut s, 100, &sigs);
        assert!(matches!(s.status, SessionStatus::Working { since: 100, .. }));
        assert_eq!(s.confidence, CONF_OUTPUT_BURST);
    }

    /// T-pty-alternateIdle (signature path): an alternate-screen TUI is Idle
    /// at 0.9 only with its ready signature on a settled grid.
    #[test]
    fn the_ready_signature_on_a_settled_alternate_screen_is_idle_at_point_nine() {
        let sigs = signatures();
        let mut s = state("claude", 0);
        s.observe_output(CLAUDE_IDLE.as_bytes(), 100);
        advance(&mut s, 100, &sigs);
        assert!(matches!(s.status, SessionStatus::Working { .. }));
        run(&mut s, 200, 1_000, &sigs);
        assert!(matches!(s.status, SessionStatus::Idle), "{:?}", s.status);
        assert_eq!(s.confidence, CONF_READY_SIGNATURE);
        assert!(s.evidence.contains(&StatusEvidence::ReadySignature));
        let facts = s.facts().expect("facts");
        assert_eq!(facts.status, PtyStatus::Idle);
        assert!(facts.prompt.is_none());
    }

    /// T-pty-alternateIdle (no-signature path, F-48): the alternate screen
    /// alone is never WaitingForInput; a vendor without a signature reaches
    /// Idle at 0.5 once the grid has been settled for 2 s.
    #[test]
    fn an_alternate_screen_alone_is_never_waiting_and_falls_back_to_idle_at_half() {
        let sigs = signatures();
        let mut s = state("gemini", 0);
        s.observe_output(b"\x1b[?1049h\x1b[2J\x1b[Hsome full-screen view\x1b[20;1Hpress q", 100);
        run(&mut s, 100, 2_000, &sigs);
        assert!(!matches!(s.status, SessionStatus::WaitingForInput { .. }));
        run(&mut s, 2_100, 2_400, &sigs);
        assert!(matches!(s.status, SessionStatus::Idle), "{:?}", s.status);
        assert_eq!(s.confidence, CONF_SETTLED_FALLBACK);
        assert_eq!(ready_threshold(&sigs, "gemini"), READY_CONFIDENCE_FALLBACK);
        assert_eq!(ready_threshold(&sigs, "claude"), READY_CONFIDENCE);
        assert_eq!(
            ready_threshold(&sigs, "cursor"),
            READY_CONFIDENCE_FALLBACK,
            "an unverified signature lets the fallback count for readiness"
        );
    }

    #[test]
    fn a_settled_permission_prompt_is_waiting_with_parsed_facts() {
        let sigs = signatures();
        let mut s = state("claude", 0);
        s.observe_output(PERMISSION.as_bytes(), 100);
        advance(&mut s, 150, &sigs);
        assert!(
            !matches!(s.status, SessionStatus::WaitingForInput { .. }),
            "a prompt is only read off a grid settled ≥ 300 ms"
        );
        run(&mut s, 200, 900, &sigs);
        assert!(matches!(s.status, SessionStatus::WaitingForInput { .. }), "{:?}", s.status);
        let facts = s.facts().expect("facts");
        let prompt = facts.prompt.expect("prompt");
        assert!(prompt.permission_shaped);
        assert_eq!(prompt.options.len(), 3);
        assert_eq!(prompt.question.as_deref(), Some("Do you want to proceed?"));
        assert_eq!(prompt.first_seen_at, 400);
    }

    #[test]
    fn the_card_stays_through_repaints_and_leaves_300_ms_after_the_prompt_is_gone() {
        let sigs = signatures();
        let mut s = state("claude", 0);
        s.observe_output(PERMISSION.as_bytes(), 100);
        run(&mut s, 100, 900, &sigs);
        let rev = s.prompt.as_ref().expect("held").prompt_rev.clone();

        // An identical repaint keeps the same card.
        s.observe_output(PERMISSION.as_bytes(), 1_000);
        advance(&mut s, 1_000, &sigs);
        assert_eq!(s.prompt.as_ref().map(|p| p.prompt_rev.clone()), Some(rev));

        // The TUI answers and starts working: the grid stops matching.
        s.observe_output(b"\x1b[2J\x1b[H\xe2\x9c\xa2 Working\xe2\x80\xa6 (esc to interrupt)", 1_100);
        advance(&mut s, 1_100, &sigs);
        assert!(s.prompt.is_some(), "not gone before 300 ms of no match");
        s.observe_output(b"\x1b[1;3H.", 1_250);
        advance(&mut s, 1_250, &sigs);
        assert!(s.prompt.is_some());
        s.observe_output(b"\x1b[1;3H:", 1_420);
        advance(&mut s, 1_420, &sigs);
        assert!(s.prompt.is_none(), "gone once 300 ms passed without a match");
        assert!(matches!(s.status, SessionStatus::Working { .. }));
    }

    #[test]
    fn the_composer_on_screen_means_no_prompt_even_with_a_numbered_list_above_it() {
        let sigs = signatures();
        let mut s = state("claude", 0);
        let screen = "\x1b[?1049h\x1b[2J\x1b[H⏺ Here is the plan:\r\n  1. Read the file\r\n  2. Fix the bug\r\n  3. Run the tests\x1b[20;1H────────\r\n❯ \x1b[22;1H────────\x1b[23;1H  ⏸ manual mode on · ? for shortcuts";
        s.observe_output(screen.as_bytes(), 100);
        run(&mut s, 100, 2_000, &sigs);
        assert!(s.prompt.is_none());
        assert!(matches!(s.status, SessionStatus::Idle), "{:?}", s.status);
    }

    #[test]
    fn a_working_line_holds_working_and_stalls_only_after_stall_after() {
        let sigs = signatures();
        let mut s = state("claude", 0).with_stall(5_000);
        s.observe_output("\x1b[?1049h\x1b[10;1H✢ Thinking… (esc to interrupt)".as_bytes(), 100);
        run(&mut s, 100, 3_000, &sigs);
        assert!(matches!(s.status, SessionStatus::Working { .. }), "{:?}", s.status);
        run(&mut s, 3_100, 5_300, &sigs);
        assert!(matches!(s.status, SessionStatus::Stalled { .. }), "{:?}", s.status);
    }

    #[test]
    fn a_shell_on_the_main_screen_keeps_its_idle_pattern() {
        let sigs = signatures();
        let mut s = state("shell", 0);
        s.idle_after_ms = 400;
        s.observe_output(b"user@host ~ % ", 100);
        run(&mut s, 100, 700, &sigs);
        assert!(matches!(s.status, SessionStatus::Idle), "{:?}", s.status);
        assert_eq!(s.confidence, CONF_IDLE_PATTERN);
    }

    #[test]
    fn exit_is_terminal_and_clears_the_prompt() {
        let sigs = signatures();
        let mut s = state("claude", 0);
        s.observe_output(PERMISSION.as_bytes(), 100);
        run(&mut s, 100, 900, &sigs);
        assert!(s.settle(SessionStatus::Exited { code: Some(0), signal: None }, vec![], 1_000));
        assert!(s.prompt.is_none());
        assert!(s.facts().is_none(), "the end is a process fact (S6), not S5");
        assert!(!s.settle(SessionStatus::Killed, vec![], 1_100), "terminal is terminal");
    }

    #[test]
    fn facts_are_published_on_change_and_refreshed_while_output_flows() {
        let sigs = signatures();
        let mut s = state("claude", 0);
        s.observe_output(b"x", 100);
        advance(&mut s, 100, &sigs);
        assert!(s.take_publishable(100).is_some(), "the first verdict publishes");
        s.observe_output(b"y", 200);
        advance(&mut s, 200, &sigs);
        assert!(s.take_publishable(200).is_none(), "same status, too soon");
        s.observe_output(b"z", 5_300);
        advance(&mut s, 5_300, &sigs);
        assert!(
            s.take_publishable(5_300).is_some(),
            "last_output_at moved and the refresh interval passed"
        );
    }

    #[test]
    fn the_ticker_cadence_follows_the_hottest_pty() {
        let engine = StatusEngine::new();
        assert_eq!(engine.next_tick(0), None, "no PTY, no tick");
        engine.register_sized("a", "claude", 1_200, 80, 24, 1_000);
        assert_eq!(
            engine.next_tick(1_000),
            Some(Duration::from_millis(TICK_MS)),
            "starting is hot"
        );
        {
            let entry = engine.sessions.get("a").unwrap();
            let mut s = entry.lock();
            s.status = SessionStatus::Idle;
            s.last_output_at = 1_000;
        }
        assert_eq!(engine.next_tick(5_000), Some(Duration::from_millis(TICK_MS)));
        assert_eq!(
            engine.next_tick(1_000 + HOT_OUTPUT_MS + 1),
            Some(Duration::from_millis(QUIET_TICK_MS)),
            "no output in 10 s and no prompt: 5 s ticks"
        );
    }

    #[test]
    fn on_output_returns_the_terminal_queries_in_order() {
        let engine = StatusEngine::new();
        engine.register_sized("q", "claude", 1_200, 80, 24, now_ms());
        let queries = engine.on_output("q", b"\x1b[2;3H\x1b[c\x1b[6n");
        assert_eq!(
            queries,
            vec![
                TermQuery::PrimaryAttributes,
                TermQuery::CursorPosition { row: 2, col: 3 }
            ]
        );
        assert!(engine.on_output("unknown", b"\x1b[c").is_empty());
    }

    /// A Bash permission prompt for `command`, inside a box under a rule.
    fn prompt_for(command: &str) -> String {
        format!(
            "\x1b[?1049h\x1b[2J\x1b[H\x1b[13;1H────────────────────\x1b[14;1H Bash command\x1b[15;1H   {command}\x1b[16;1H Do you want to proceed?\r\n ❯ 1. Yes\r\n   2. Yes, and switch to auto mode\r\n   3. No\r\n\r\n Esc to cancel"
        )
    }

    /// A34.1: a prompt that approves another command — same question, same
    /// options — is another rev and a **new** card: `first_seen_at` restarts
    /// (so the WebView's 700 ms guard applies again); a repaint keeps both.
    #[test]
    fn a_prompt_for_another_command_is_a_new_card_with_a_new_first_seen_at() {
        let sigs = signatures();
        let mut s = state("claude", 0);
        s.observe_output(prompt_for("rm -rf build/cache").as_bytes(), 100);
        run(&mut s, 100, 900, &sigs);
        let a = s.prompt.clone().expect("held A");
        assert_eq!(a.first_seen_at, 400);

        s.observe_output(prompt_for("rm -rf ~/Documents").as_bytes(), 1_000);
        run(&mut s, 1_000, 2_000, &sigs);
        let b = s.prompt.clone().expect("held B");
        assert_ne!(b.prompt_rev, a.prompt_rev, "the header (the command) is part of the digest");
        assert_eq!(b.question, a.question);
        assert!(b.first_seen_at >= 1_000, "a new rev restarts first_seen_at: {}", b.first_seen_at);

        s.observe_output(prompt_for("rm -rf ~/Documents").as_bytes(), 2_100);
        run(&mut s, 2_100, 2_600, &sigs);
        assert_eq!(
            s.prompt.as_ref().map(|p| (p.prompt_rev.clone(), p.first_seen_at)),
            Some((b.prompt_rev.clone(), b.first_seen_at)),
            "a repaint keeps the rev and the card"
        );
    }
}
