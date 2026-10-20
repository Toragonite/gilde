//! The status inference engine — DESIGN §6-4, M1-PLAN T16.
//!
//! ## Three signal sources, never one
//!
//! ```text
//!   1. output dynamics   reader thread → on_output()      "bytes are flowing"
//!   2. OS truth          tcgetpgrp + ps → tick(&Snapshot)  "that group is burning CPU"
//!   3. visible text      AnsiTail → agent-signatures.json  "that line says [y/N]"
//!   4. screen content    AnsiTail grid digest → ScreenContent
//!                                                         "the cells have not moved"
//! ```
//!
//! Each one is wrong on its own. Output dynamics call a long `Bash` tool call
//! idle — and call a menu that repaints itself busy. The foreground group is
//! quiet while an agent waits on a network request. Pattern matching breaks the
//! day a vendor renames a spinner. Together they degrade instead of failing:
//! when the patterns stop matching, the status keeps working at a lower
//! confidence, and the UI says so.
//!
//! ## The state machine
//!
//! `evaluate()` is DESIGN §6-4's table, in order, first match wins. *Settled*
//! below means `quiet ≥ 1.5 s` **or** `screen unchanged ≥ 1.5 s`, and no cpu —
//! where "cpu" is [`SessionState::cpu_says_working`], which asks for
//! [`CPU_STILL_SCREEN_PCT`] rather than [`CPU_BUSY_PCT`] once the screen has
//! stopped changing, because `ps -o %cpu` is a decaying average and keeps
//! claiming a repaint that already finished (PINNED §35):
//!
//! | condition | status | confidence |
//! |---|---|---|
//! | the child was reaped | `Exited` / `Killed` | 1.00 |
//! | foreground group ≥ 3 % cpu (≥ 15 % on a screen that has stopped) | `Working` | 0.75 |
//! | settled, tail matches `waiting` | `WaitingForInput` | 0.85 |
//! | settled, alternate screen | `WaitingForInput` | 0.65 |
//! | output within 500 ms, and not the repaint we asked for | `Working` | 0.90 |
//! | quiet < `idleAfterMs` | *no change* | — |
//! | tail matches `waiting` | `WaitingForInput` | 0.85 |
//! | tail matches `working` | `Working` | 0.70 |
//! | tail matches `idle` | `Idle` | 0.85 |
//! | quiet ≥ `stallAfterMs`, no cpu | `Stalled` | 0.60 |
//! | otherwise | `Idle` | 0.40 |
//!
//! "the repaint we asked for" is [`SessionState::repainting_after_resize`]
//! (PINNED §36): a `TIOCSWINSZ` makes the kernel deliver `SIGWINCH` and every
//! full-screen TUI answers it by redrawing everything, which is a kilobyte of
//! output that says nothing about the agent. Measured, that mattered a great
//! deal — DESIGN §6-5's literal-key row mounts exactly when the status is
//! `WaitingForInput`, takes two rows off the terminal, and the repaint it
//! provoked put the status back to `Working`, which unmounted it, which resized
//! again: 1061 resizes in fourteen minutes on a menu nobody was touching.
//!
//! It runs on every output chunk and on every 500 ms tick. A status change is
//! debounced to one per 300 ms, except `Working` and the terminal statuses,
//! which are immediate: the first is what makes the UI feel alive, the second is
//! what stops a dead process being drawn as a live one.
//!
//! ## Why the engine outlives any one session
//!
//! One engine holds every session's state, so the 500 ms tick is one task and
//! one `ps` snapshot for the whole app rather than one per terminal tab. The
//! per-session state is created by [`StatusEngine::register`] (from
//! `PtySession::start`) and pruned once a session has been terminal for
//! [`PRUNE_AFTER_MS`].

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use dashmap::DashMap;
use parking_lot::{Mutex, RwLock};

use crate::agents::signatures::{AgentSignatures, CompiledPattern, Signatures};
use crate::events::bus::EventBus;
use crate::events::{self, SessionStatusEvent};
use crate::model::graph::{node_id, GraphOp, NodeKind};
use crate::model::status::{PatternClass, SessionStatus, SessionStatusReport, StatusEvidence};
use crate::pty::ansi::{AnsiTail, VISIBLE_LINES};
use crate::store::{dao, now_ms, Db};
use crate::supervise::procscan::Snapshot;

use super::evidence::*;
use super::ForegroundSource;

/// How long a terminal (or detached) session's state is kept after it ends.
///
/// Long enough that a late `session_status` from a closing tab still gets the
/// truth, short enough that a machine left running for a week does not
/// accumulate one `AnsiTail` per session ever opened.
pub const PRUNE_AFTER_MS: i64 = 5 * 60 * 1_000;

/// `pty_session.last_io_at` is written at most this often per session
/// (M1-PLAN T16.9). The column feeds T12's idle timeout, which is measured in
/// minutes; a 5 s resolution is three orders of magnitude finer than it needs.
pub const IO_PERSIST_MS: i64 = 5_000;

/// How stale the shared `ps` snapshot may get before the ticker takes its own.
///
/// When T12's watchdog is running it feeds the engine through
/// [`StatusEngine::set_snapshot`] every 5 s — but its cadence is *its* business,
/// and the CPU signal is load-bearing here, so the engine tops it up rather than
/// depending on it.
pub const SNAPSHOT_MAX_AGE_MS: i64 = 2_000;

// ---------------------------------------------------------------------------
// where the patterns come from
// ---------------------------------------------------------------------------

/// The hot-reloadable signature set, read fresh on every evaluation.
///
/// Indirection rather than a direct `Arc<AgentRegistry>` for two reasons: the
/// engine has no other reason to know about agent detection, and a test wants to
/// swap the patterns without a `~/Library/Application Support` on disk.
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

    /// The compiled built-in defaults — what a session gets when nothing has
    /// been wired up yet.
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

// ---------------------------------------------------------------------------
// per-session state
// ---------------------------------------------------------------------------

struct SessionState {
    /// `agent-signatures.json` key — `AgentKind::signature_key()`.
    agent_key: String,
    idle_after_ms: u64,
    /// `config.status.stallAfterMs`, copied in at registration so `evaluate`
    /// stays a pure function of the state it is given.
    stall_after_ms: u64,
    opened_at: i64,

    tail: AnsiTail,

    status: SessionStatus,
    confidence: f32,
    evidence: Vec<StatusEvidence>,
    /// Unix millis at which the current status was entered.
    since: i64,
    /// Unix millis of the last *applied* change; the debounce reads this.
    last_change_at: i64,

    // -- output dynamics ----------------------------------------------------
    /// 0 until the first byte. Distinguishes "quiet" from "has never spoken".
    last_output_at: i64,
    window_started: i64,
    window_bytes: u64,
    last_rate: u64,

    // -- io, for `pty_session.last_io_at` -----------------------------------
    last_io_at: i64,
    persisted_io_at: i64,

    // -- OS truth -----------------------------------------------------------
    foreground: Option<ForegroundSample>,
    cpu: CpuLatch,

    // -- screen content -----------------------------------------------------
    /// The vte grid digest and how long it has held (BACKLOG 24).
    content: ScreenContent,
    /// Unix millis of the last `TIOCSWINSZ` we sent; 0 if never (PINNED §36).
    /// The repaint inside [`RESIZE_REPAINT_MS`] of it is ours, not the agent's.
    resized_at: i64,

    // -- bell ---------------------------------------------------------------
    /// Set by a BEL, cleared only by [`StatusEngine::clear_attention`].
    attention: bool,
    /// A bell the UI has not been told about yet.
    bell_pending: bool,

    /// When the session entered a terminal or detached state; drives pruning.
    ended_at: Option<i64>,
}

impl SessionState {
    fn new(agent_key: String, idle_after_ms: u64, now: i64) -> SessionState {
        SessionState {
            agent_key,
            idle_after_ms: idle_after_ms.max(1),
            stall_after_ms: DEFAULT_STALL_AFTER_MS,
            opened_at: now,
            tail: AnsiTail::new(),
            status: SessionStatus::Starting,
            confidence: CONF_STARTING,
            evidence: vec![StatusEvidence::OutputQuiet { ms: 0 }],
            since: now,
            last_change_at: now,
            last_output_at: 0,
            window_started: now,
            window_bytes: 0,
            last_rate: 0,
            last_io_at: now,
            // Zero, not `now`: the first byte a session produces writes
            // `pty_session.last_io_at` straight away. A five-second hole at the
            // start of every session is exactly when T12's idle timer is most
            // likely to read the column.
            persisted_io_at: 0,
            foreground: None,
            cpu: CpuLatch::default(),
            content: ScreenContent::new(now),
            resized_at: 0,
            attention: false,
            bell_pending: false,
            ended_at: None,
        }
    }

    fn report(&self) -> SessionStatusReport {
        SessionStatusReport {
            status: self.status.clone(),
            confidence: self.confidence,
            evidence: self.evidence.clone(),
            since: self.since,
        }
    }

    fn settled(&self) -> bool {
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

    /// Feed a run of pty bytes into the tail and the rate window.
    fn observe_output(&mut self, bytes: &[u8], now: i64) {
        self.tail.feed(bytes);
        if self.tail.take_bell() {
            self.attention = true;
            self.bell_pending = true;
        }
        self.last_output_at = now;
        self.last_io_at = now;
        self.window_bytes += bytes.len() as u64;
        let elapsed = (now - self.window_started).max(0);
        if elapsed >= RATE_WINDOW_MS {
            self.last_rate = self.window_bytes.saturating_mul(1_000) / elapsed.max(1) as u64;
            self.window_started = now;
            self.window_bytes = 0;
        }
    }

    /// Refresh the screen digest, at most once per [`CONTENT_SAMPLE_MS`].
    ///
    /// Called at the top of every evaluation — output-driven and tick-driven
    /// alike — so that every rule below reads one clock that was wound in the
    /// same breath.
    fn sample_content(&mut self, now: i64) {
        if !self.content.due(now) {
            return;
        }
        let rows = self.tail.screen_lines();
        let cursor = self.tail.cursor();
        // The rule only needs "more than the tolerance", so the walk normally
        // gives up at three cells. `GILDE_LOG=status=debug` wants the real
        // number — that is the column PINNED §36 was read off — and pays for a
        // full walk of the grid only while somebody is reading it.
        let limit = if tracing::enabled!(target: "status", tracing::Level::DEBUG) {
            usize::MAX
        } else {
            STABLE_CELL_TOLERANCE
        };
        if self.repainting_after_resize(now) {
            // The screen changed because *we* changed it. Take the new frame as
            // the anchor so the next real change is measured against something
            // current, and leave the clock alone (PINNED §36).
            self.content.reanchor(rows, cursor, now);
            return;
        }
        self.content.observe_to(rows, cursor, now, limit);
    }

    /// True while the bytes arriving are the child answering our `SIGWINCH`
    /// (PINNED §36).
    ///
    /// The measured loop: the app told the pty it was two rows shorter, claude
    /// cleared the screen and drew the same menu again, the reader saw a
    /// kilobyte arrive, rule 4 said `Working`, the `Esc to cancel` key row
    /// unmounted, the terminal grew back by two rows, and the app resized it
    /// again — 1061 times in fourteen minutes, one full repaint every two
    /// seconds, on a screen the user was only reading. Nothing in that loop is
    /// the agent doing anything, and the only place to break it is here: a
    /// repaint we asked for is not evidence about the child.
    fn repainting_after_resize(&self, now: i64) -> bool {
        self.resized_at > 0 && now - self.resized_at <= RESIZE_REPAINT_MS
    }

    fn content_stable_ms(&self, now: i64) -> u64 {
        self.content.stable_ms(now)
    }

    /// The two ways a screen can have stopped happening (DESIGN §6-4,
    /// BACKLOG 24).
    ///
    /// Silence is one of them and used to be the only one. The other is a
    /// screen that keeps being repainted with the same cells: claude's `/model`
    /// and `/help` redraw at ~2 kB/s forever, so the byte counter never goes
    /// quiet and every rule gated on silence — the alternate-screen rule and
    /// the waiting-pattern rule both — stayed unreachable while the user sat in
    /// front of a menu the app called `작업 중`.
    ///
    /// Either observation on its own is enough, because either one on its own
    /// means the same thing: nothing new has reached the user's eyes for
    /// [`ALT_SCREEN_QUIET_MS`].
    fn quiet_for_waiting(&self, now: i64) -> bool {
        self.quiet_ms(now) >= ALT_SCREEN_QUIET_MS
            || self.content_stable_ms(now) >= ALT_SCREEN_QUIET_MS
    }

    /// The foreground group is computing, *and* that reading still means
    /// something given what the screen is doing (PINNED §35).
    ///
    /// `ps -o %cpu` is a decaying average, so the number keeps claiming work
    /// for a second or two after the work stopped. On a screen that is still
    /// moving that is harmless — the screen agrees. On a screen that has not
    /// changed a cell for [`ALT_SCREEN_QUIET_MS`] the two signals disagree, and
    /// the measured disagreement was always the stale one: claude sitting on a
    /// finished `/model` menu reported 11.5 % with `ScreenStable { ms: 1692 }`
    /// in the very same evidence vector, and the CPU rule — which is first —
    /// won. So a still screen raises the bar to [`CPU_STILL_SCREEN_PCT`], which
    /// real computation clears and a cooling repaint does not.
    ///
    /// Used in both places the old `cpu.busy()` was: the rule itself and the
    /// `settled` predicate the two screen rules are gated on. They have to
    /// agree, or demoting one would leave the other blocking.
    fn cpu_says_working(&self, now: i64) -> bool {
        if !self.cpu.busy() {
            return false;
        }
        if self.content_stable_ms(now) < ALT_SCREEN_QUIET_MS {
            return true;
        }
        // No sample and a latched claim: keep the latch's word. The rule below
        // cannot fire without a sample anyway, and inventing a 0 % here would
        // clear a busy group on a `ps` failure.
        self.foreground
            .map(|sample| sample.cpu_pct >= CPU_STILL_SCREEN_PCT)
            .unwrap_or(true)
    }

    /// DESIGN §6-4's table, plus the one debug line that says which row of it
    /// fired and on what numbers (PINNED §36).
    ///
    /// `GILDE_LOG=status=debug` is the whole instrument: with it off this is
    /// [`SessionState::decide`] and a level check, and with it on every
    /// evaluation — output-driven and tick-driven alike — leaves a row of the
    /// table it decided from. §34 and §35 were both fixed against a *harness*
    /// and both still wrong in the packaged app; a rule that cannot be watched
    /// where it runs gets debugged by guessing.
    fn evaluate(&mut self, now: i64, signatures: Option<&Signatures>) -> Option<Verdict> {
        let (verdict, rule) = self.decide(now, signatures);
        self.trace(rule, verdict.as_ref(), now);
        verdict
    }

    /// One line per evaluation: which rule, on which numbers.
    ///
    /// `tracing`'s macros check the callsite's interest before they evaluate a
    /// single field expression, so with the target off this costs one atomic
    /// load — the reason the reader thread can afford to call it on every
    /// chunk.
    fn trace(&self, rule: &'static str, verdict: Option<&Verdict>, now: i64) {
        let diff = self.content.last_diff();
        tracing::debug!(
            target: "status",
            rule,
            state = verdict.map(|v| v.status.kind()).unwrap_or("<unchanged>"),
            held = self.status.kind(),
            confidence = verdict.map(|v| v.confidence).unwrap_or(self.confidence),
            quiet_ms = self.quiet_ms(now),
            content_stable_ms = self.content_stable_ms(now),
            cells_changed_since_anchor = diff.cells,
            rows_changed = diff.rows,
            cursor_moved = diff.cursor_moved,
            bytes_in_window = self.window_bytes,
            rate_bytes_per_s = self.last_rate,
            cpu_pct = self.foreground.map(|s| s.cpu_pct).unwrap_or(f32::NAN),
            cpu_latched = self.cpu.busy(),
            alt_screen = self.tail.alt_screen(),
            resize_grace = self.repainting_after_resize(now),
            rows_retained = self.tail.retained_lines(),
            "evaluate"
        );
    }

    /// DESIGN §6-4's table. `None` means "keep whatever you have"; the
    /// `&'static str` names the row that decided, for [`SessionState::trace`].
    fn decide(
        &mut self,
        now: i64,
        signatures: Option<&Signatures>,
    ) -> (Option<Verdict>, &'static str) {
        if self.settled() {
            return (None, "settled");
        }

        // One reading of the grid, before any rule asks about it.
        self.sample_content(now);

        let quiet = self.quiet_ms(now);

        // `Starting` holds until the first byte or 1500 ms, whichever is first.
        if matches!(self.status, SessionStatus::Starting)
            && self.last_output_at == 0
            && now - self.opened_at < STARTING_GRACE_MS
        {
            return (None, "starting-grace");
        }

        // 1. OS truth, first. A foreground group that is computing is working,
        //    whatever the screen looks like — this is the signal that survives
        //    a vendor renaming its spinner (M1-PLAN T16.3), and putting it
        //    above the two "settled screen" rules is what keeps a TUI that is
        //    genuinely busy behind a full-screen view out of them.
        //
        //    "Computing" is `cpu_says_working`, not the raw latch: on a screen
        //    that has provably stopped, a decaying `ps` average has to clear a
        //    higher bar before it outranks the screen (PINNED §35).
        if self.cpu_says_working(now) {
            if let Some(sample) = self.foreground {
                return (
                    Some(self.decorate(
                        Verdict::new(
                            SessionStatus::Working {
                                since: 0,
                                hint: None,
                            },
                            CONF_FOREGROUND_CPU,
                            vec![sample.evidence(), StatusEvidence::OutputQuiet { ms: quiet }],
                        ),
                        quiet,
                        now,
                    )),
                    "cpu",
                );
            }
        }

        let settled = !self.cpu_says_working(now) && self.quiet_for_waiting(now);

        // 2. A `waiting` signature on a screen that has stopped changing
        //    (BACKLOG 24).
        //
        //    The vendor's own words are the strongest thing we have, so this
        //    outranks the byte rate rather than sitting behind it: `/model`'s
        //    footer says `Esc to cancel` and the frame carrying it has been
        //    identical for a second and a half. Only the `waiting` bucket is
        //    read here — a `working` or `idle` line on a still screen is not
        //    urgent enough to jump the reactivity path, and both are still
        //    matched below by rule 6.
        if settled {
            if let Some(agent) = signatures.and_then(|s| s.for_agent(&self.agent_key)) {
                let lines = self.tail.visible_lines(VISIBLE_LINES);
                if let Some((pattern, line)) = match_bucket(&agent.waiting, &lines) {
                    // Trimmed, like `alt_screen_prompt`: the prompt drives
                    // DESIGN §6-5's literal-key row, and a menu's indentation
                    // is not part of what the child is asking.
                    let line = clip_line(line.trim());
                    let mut evidence = vec![
                        StatusEvidence::PatternMatch {
                            signature_id: pattern.id.clone(),
                            class: pattern.class,
                            line: line.clone(),
                        },
                        StatusEvidence::OutputQuiet { ms: quiet },
                    ];
                    // The CPU reading is half of why the screen counts as
                    // settled, so it belongs in the vector that explains it.
                    if let Some(sample) = self.foreground {
                        evidence.push(sample.evidence());
                    }
                    return (
                        Some(self.decorate(
                            Verdict::new(
                                SessionStatus::WaitingForInput { prompt: line },
                                CONF_WAITING_PATTERN,
                                evidence,
                            ),
                            quiet,
                            now,
                        )),
                        "settled-waiting-pattern",
                    );
                }
            }
        }

        // 3. A settled alternate screen with no CPU behind it (DESIGN §6-4,
        //    BACKLOG M1.5 15b, widened by BACKLOG 24).
        //
        //    `/help`, a full-screen picker, a pager: the child switched to the
        //    alternate buffer, painted a screen and stopped — or kept
        //    repainting the same one, which is the same thing to the person
        //    looking at it. Every other signal reads that as computation: the
        //    spinner line the main screen was showing is still the newest thing
        //    a `working` pattern can match, and the app said `Working` at a
        //    screen that was doing nothing but waiting to be dismissed.
        //
        //    Three observations agree that it is not: the screen is the child's
        //    own, nothing new has reached it for `ALT_SCREEN_QUIET_MS`, and the
        //    foreground group is not burning CPU.
        if settled && self.tail.alt_screen() {
            let mut evidence = vec![
                StatusEvidence::AltScreen { on: true },
                StatusEvidence::OutputQuiet { ms: quiet },
            ];
            if let Some(sample) = self.foreground {
                evidence.push(sample.evidence());
            }
            return (
                Some(self.decorate(
                    Verdict::new(
                        SessionStatus::WaitingForInput {
                            prompt: self.alt_screen_prompt(),
                        },
                        CONF_ALT_SCREEN_QUIET,
                        evidence,
                    ),
                    quiet,
                    now,
                )),
                "settled-alt-screen",
            );
        }

        // 4. Output in the last 500 ms. The reactivity path: a terminal that is
        //    printing something new is working.
        if self.last_output_at > 0
            && now - self.last_output_at <= OUTPUT_BURST_MS
            && !self.repainting_after_resize(now)
        {
            return (
                Some(self.decorate(
                    Verdict::new(
                        SessionStatus::Working {
                            since: 0,
                            hint: None,
                        },
                        CONF_OUTPUT_BURST,
                        vec![StatusEvidence::OutputBurst {
                            bytes_per_s: self.last_rate.max(self.window_bytes),
                        }],
                    ),
                    quiet,
                    now,
                )),
                "output-burst",
            );
        }

        // 5. Not quiet for long enough to conclude anything new.
        if quiet < self.idle_after_ms {
            return (None, "below-idle-after");
        }

        // 6. The visible tail, against the hot-reloaded signature file.
        if let Some(agent) = signatures.and_then(|s| s.for_agent(&self.agent_key)) {
            let lines = self.tail.visible_lines(VISIBLE_LINES);
            if let Some((pattern, line)) = match_by_class(agent, &lines) {
                let line = clip_line(line.trim());
                let matched = StatusEvidence::PatternMatch {
                    signature_id: pattern.id.clone(),
                    class: pattern.class,
                    line: line.clone(),
                };
                let verdict = match pattern.class {
                    PatternClass::Waiting => Verdict::new(
                        SessionStatus::WaitingForInput { prompt: line },
                        CONF_WAITING_PATTERN,
                        vec![matched],
                    ),
                    PatternClass::Working => Verdict::new(
                        SessionStatus::Working {
                            since: 0,
                            hint: Some(line),
                        },
                        CONF_WORKING_PATTERN,
                        vec![matched],
                    ),
                    PatternClass::Idle => {
                        Verdict::new(SessionStatus::Idle, CONF_IDLE_PATTERN, vec![matched])
                    }
                };
                let rule = match pattern.class {
                    PatternClass::Waiting => "tail-waiting",
                    PatternClass::Working => "tail-working",
                    PatternClass::Idle => "tail-idle",
                };
                return (
                    Some(self.decorate(
                        verdict.with(StatusEvidence::OutputQuiet { ms: quiet }),
                        quiet,
                        now,
                    )),
                    rule,
                );
            }
        }

        // 7. Quiet for a very long time with nothing to show for it.
        let stalled = quiet >= self.stall_after_ms && !self.cpu.busy();
        let (verdict, rule) = if stalled {
            (
                Verdict::new(
                    SessionStatus::Stalled { quiet_ms: quiet },
                    CONF_STALLED,
                    vec![StatusEvidence::OutputQuiet { ms: quiet }],
                ),
                "stalled",
            )
        } else {
            // 8. The honest shrug. Below the hollow-pill threshold on purpose.
            (
                Verdict::new(
                    SessionStatus::Idle,
                    CONF_FALLBACK_IDLE,
                    vec![StatusEvidence::OutputQuiet { ms: quiet }],
                ),
                "fallback-idle",
            )
        };
        (Some(self.decorate(verdict, quiet, now)), rule)
    }

    /// The last line an alternate-screen view left on the screen.
    ///
    /// That is where all four CLIs print the key that dismisses it — claude's
    /// `Esc to cancel` / `Esc to close`, a pager's `(END)` — so it is both the
    /// most useful thing to quote in the pill's tooltip and the honest prompt
    /// for DESIGN §6-5's literal-key row. Empty when the view drew nothing,
    /// which `Verdict::ensure_evidence` and the evidence vector both survive.
    fn alt_screen_prompt(&self) -> String {
        self.tail
            .visible_lines(VISIBLE_LINES)
            .last()
            .map(|line| clip_line(line.trim()))
            .unwrap_or_default()
    }

    /// Attach the observations that are true regardless of which branch fired.
    ///
    /// `ScreenStable` is attached whenever it is true, including on a `Working`
    /// verdict: "출력 2.1kB/s · 화면 내용 3초간 동일" is exactly the pair of facts
    /// that made the regression legible, and hiding one of them because it
    /// argues with the other would be the lie DESIGN §0 forbids.
    fn decorate(&self, verdict: Verdict, quiet: u64, now: i64) -> Verdict {
        let mut verdict = verdict;
        if self.tail.alt_screen() {
            verdict = verdict.with(StatusEvidence::AltScreen { on: true });
        }
        let stable = self.content_stable_ms(now);
        if stable >= ALT_SCREEN_QUIET_MS {
            verdict = verdict.with(StatusEvidence::ScreenStable { ms: stable });
        }
        if self.bell_pending {
            verdict = verdict.with(StatusEvidence::Bell);
        }
        verdict.ensure_evidence(quiet)
    }

    /// Apply a verdict, honouring the debounce. Returns `true` when the UI has
    /// to be told.
    fn apply(&mut self, verdict: Verdict, now: i64) -> bool {
        let same_kind = self.status.kind() == verdict.status.kind();

        if !same_kind
            && !bypasses_debounce(&verdict.status)
            && now - self.last_change_at < DEBOUNCE_MS
        {
            // Held. The next evaluation (≤ 500 ms away) will look again; a
            // status that was real then is still real now.
            return false;
        }

        let mut status = verdict.status;
        // A session that is still working has been working since it started,
        // not since this evaluation.
        if let (
            SessionStatus::Working { since, .. },
            SessionStatus::Working {
                since: previous, ..
            },
        ) = (&mut status, &self.status)
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

        // A bell is news even when nothing else changed: it is the whole reason
        // the attention badge exists.
        let bell = std::mem::take(&mut self.bell_pending);
        significant || bell
    }

    /// Force a terminal / detached status, bypassing everything.
    fn settle(&mut self, status: SessionStatus, evidence: Vec<StatusEvidence>, now: i64) -> bool {
        if self.settled() {
            return false;
        }
        self.since = now;
        self.last_change_at = now;
        self.ended_at = Some(now);
        self.status = status;
        self.confidence = CONF_EXITED;
        let mut evidence = evidence;
        if evidence.is_empty() {
            evidence.push(StatusEvidence::OutputQuiet {
                ms: self.quiet_ms(now),
            });
        }
        // A bell that arrived just before the end is still news.
        if std::mem::take(&mut self.bell_pending) {
            evidence.push(StatusEvidence::Bell);
        }
        self.evidence = evidence;
        true
    }
}

impl SessionState {
    fn with_stall(mut self, stall_after_ms: u64) -> SessionState {
        self.stall_after_ms = stall_after_ms.max(1);
        self
    }
}

/// Search the visible tail **class first**, newest line first within a class.
///
/// DESIGN §6-4's table is written class-major — "tail이 waiting 매치", then
/// "tail이 working 매치", then "tail이 idle 매치" — and that is not a detail.
/// Both real TUIs draw their composer *below* their activity line: while a turn
/// runs, `codex` shows
///
/// ```text
/// • Working (0s • esc to interrupt)
/// › Find and fix a bug in @filename        ← newer, and an `idle` match
/// ```
///
/// A newest-line-first scan across all three classes (which is what
/// [`AgentSignatures::match_tail`] does, correctly, for the single-line case)
/// would call that session idle while it is plainly working. Searching by class
/// makes the placeholder harmless and lets the `idle` patterns be broad enough
/// to match a composer that is never truly empty.
///
/// Within one class the newest line still wins, so a fresh prompt beats a stale
/// one — and because the tail is a *screen* rather than a log, an activity line
/// that is over has already been painted away.
fn match_by_class<'a>(
    agent: &'a AgentSignatures,
    lines: &[String],
) -> Option<(&'a CompiledPattern, String)> {
    for bucket in [&agent.waiting, &agent.working, &agent.idle] {
        if let Some(hit) = match_bucket(bucket, lines) {
            return Some(hit);
        }
    }
    None
}

/// One class of patterns against the visible tail, newest line first.
///
/// Split out of [`match_by_class`] because rule 2 of `evaluate` asks the
/// `waiting` bucket on its own: a settled screen is only allowed to jump the
/// reactivity path when the vendor's own words say the child is blocked.
fn match_bucket<'a>(
    bucket: &'a [CompiledPattern],
    lines: &[String],
) -> Option<(&'a CompiledPattern, String)> {
    for line in lines.iter().rev() {
        if let Some(hit) = bucket.iter().find(|p| p.regex.is_match(line)) {
            return Some((hit, line.clone()));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// the engine
// ---------------------------------------------------------------------------

/// One per app. Holds every session's inference state and the shared inputs.
pub struct StatusEngine {
    sessions: DashMap<String, Mutex<SessionState>>,
    /// The pty master of each session, for `tcgetpgrp`. Weak so that a retired
    /// session is dropped on time even if its state has not been pruned yet.
    foreground: DashMap<String, Weak<dyn ForegroundSource>>,
    signatures: RwLock<Option<Arc<dyn SignatureSource>>>,
    events: RwLock<Option<EventBus>>,
    db: RwLock<Option<Db>>,
    snapshot: RwLock<Option<Arc<Snapshot>>>,
    snapshot_at: AtomicU64,
    stall_after_ms: AtomicU64,
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
    /// A working engine with no wiring: the **bundled** signature set, no bus,
    /// no database, no ticker.
    ///
    /// The defaults rather than `None` because they are compiled into the binary
    /// and cost one regex compile: an engine that classified nothing until boot
    /// handed it a source would report every session as a 0.4 shrug, and every
    /// test that opens a pty without the whole app behind it would be testing
    /// the wrong state machine. `set_signatures` swaps in the hot-reloading
    /// source that watches `agent-signatures.json`.
    pub fn new() -> StatusEngine {
        StatusEngine {
            sessions: DashMap::new(),
            foreground: DashMap::new(),
            signatures: RwLock::new(Some(Arc::new(StaticSignatures::defaults()))),
            events: RwLock::new(None),
            db: RwLock::new(None),
            snapshot: RwLock::new(None),
            snapshot_at: AtomicU64::new(0),
            stall_after_ms: AtomicU64::new(DEFAULT_STALL_AFTER_MS),
        }
    }

    // -- wiring --------------------------------------------------------------

    pub fn set_events(&self, events: EventBus) {
        *self.events.write() = Some(events);
    }

    pub fn set_db(&self, db: Db) {
        *self.db.write() = Some(db);
    }

    pub fn set_signatures(&self, source: Arc<dyn SignatureSource>) {
        *self.signatures.write() = Some(source);
    }

    pub fn set_stall_after_ms(&self, ms: u64) {
        self.stall_after_ms.store(ms.max(1), Ordering::Relaxed);
    }

    pub fn has_events(&self) -> bool {
        self.events.read().is_some()
    }

    fn signature_set(&self) -> Option<Arc<Signatures>> {
        self.signatures.read().as_ref().map(|s| s.signatures())
    }

    // -- session lifecycle ---------------------------------------------------

    /// Start tracking a session. Called by `PtySession::start` before the reader
    /// thread exists, so no output can arrive before there is somewhere to put
    /// it.
    pub fn register(&self, session_id: &str, agent_key: &str, idle_after_ms: u64, at: i64) {
        let state = SessionState::new(agent_key.to_string(), idle_after_ms, at)
            .with_stall(self.stall_after_ms.load(Ordering::Relaxed));
        self.sessions
            .insert(session_id.to_string(), Mutex::new(state));
    }

    /// Give the engine a way to ask the kernel which process group owns the tty.
    pub fn attach_foreground(&self, session_id: &str, source: &Arc<dyn ForegroundSource>) {
        self.foreground
            .insert(session_id.to_string(), Arc::downgrade(source));
    }

    pub fn forget(&self, session_id: &str) {
        self.sessions.remove(session_id);
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

    // -- inputs --------------------------------------------------------------

    /// One run of pty bytes, straight off the reader thread (DESIGN §6-2).
    ///
    /// Hot path: this runs once per 64 KiB read on the thread that must never
    /// block the child, so the expensive half of the evaluation (tail lines,
    /// regexes) is unreachable here — output just arrived, so branch 1 of the
    /// table fires and returns before any of it.
    pub fn on_output(&self, session_id: &str, bytes: &[u8]) {
        let now = now_ms();
        let signatures = self.signature_set();
        let emit = {
            let Some(entry) = self.sessions.get(session_id) else {
                return;
            };
            let mut state = entry.lock();
            state.observe_output(bytes, now);
            self.step(&mut state, now, signatures.as_deref())
        };
        self.publish(session_id, emit);
        self.persist_io(session_id);
    }

    /// The pty was resized (PINNED §36).
    ///
    /// `TIOCSWINSZ` makes the kernel deliver `SIGWINCH`, and a full-screen TUI
    /// answers it by redrawing everything. Those bytes look exactly like an
    /// agent that just produced a screenful of output, and they are nothing of
    /// the kind — the app moved the window, and the same screen came back at a
    /// new size. So the engine is told, and for [`RESIZE_REPAINT_MS`] it stops
    /// treating output and screen changes as news.
    ///
    /// Measured (PINNED §36's dump): without this, the literal-key row of
    /// DESIGN §6-5 — which mounts exactly when the status is `WaitingForInput`
    /// — took two rows off the terminal, the resize made claude repaint, the
    /// repaint made the status `Working`, the row unmounted, the terminal grew
    /// back, and the app resized again. Two full repaints every two seconds,
    /// for as long as the session was open, and a pill that was never right for
    /// more than half a second at a time.
    pub fn on_resize(&self, session_id: &str, cols: u16, rows: u16) {
        let now = now_ms();
        let Some(entry) = self.sessions.get(session_id) else {
            return;
        };
        entry.lock().resized_at = now;
        tracing::debug!(target: "status", session = %session_id, cols, rows, "pty resized");
    }

    /// The user typed. Only `last_io_at` cares — what the *user* does is
    /// activity for T12's idle timeout, but it is not evidence about the agent.
    pub fn on_input(&self, session_id: &str) {
        let now = now_ms();
        if let Some(entry) = self.sessions.get(session_id) {
            entry.lock().last_io_at = now;
        }
        self.persist_io(session_id);
    }

    /// The child was reaped (DESIGN §6-5's exit watcher).
    pub fn on_exit(&self, session_id: &str, code: Option<i32>, signal: Option<i32>) {
        let now = now_ms();
        let emit = {
            let Some(entry) = self.sessions.get(session_id) else {
                return;
            };
            let mut state = entry.lock();
            let changed = state.settle(
                SessionStatus::Exited { code, signal },
                vec![StatusEvidence::ChildExited { code }],
                now,
            );
            changed.then(|| state.report())
        };
        self.publish(session_id, emit);
    }

    /// We killed it and nothing survived (`session_close`).
    pub fn on_killed(&self, session_id: &str) {
        let now = now_ms();
        let emit = {
            let Some(entry) = self.sessions.get(session_id) else {
                return;
            };
            let mut state = entry.lock();
            let changed = state.settle(
                SessionStatus::Killed,
                vec![StatusEvidence::ChildExited { code: None }],
                now,
            );
            changed.then(|| state.report())
        };
        self.publish(session_id, emit);
    }

    /// Deliberate orphaning (`CloseMode::Detach`).
    pub fn on_detach(&self, session_id: &str) {
        let now = now_ms();
        let emit = {
            let Some(entry) = self.sessions.get(session_id) else {
                return;
            };
            let mut state = entry.lock();
            let quiet = state.quiet_ms(now);
            let changed = state.settle(
                SessionStatus::Detached,
                vec![StatusEvidence::OutputQuiet { ms: quiet }],
                now,
            );
            changed.then(|| state.report())
        };
        self.publish(session_id, emit);
    }

    /// The session failed to become useful — a launch that produced nothing a
    /// caller could act on. Terminal, like an exit.
    pub fn on_failed(&self, session_id: &str, reason: impl Into<String>) {
        let now = now_ms();
        let reason = reason.into();
        let emit = {
            let Some(entry) = self.sessions.get(session_id) else {
                return;
            };
            let mut state = entry.lock();
            let changed = state.settle(SessionStatus::Failed { reason }, Vec::new(), now);
            changed.then(|| state.report())
        };
        self.publish(session_id, emit);
    }

    // -- attention -----------------------------------------------------------

    /// True while a BEL has not been acknowledged (DESIGN §6-4).
    pub fn attention(&self, session_id: &str) -> bool {
        self.sessions
            .get(session_id)
            .map(|entry| entry.lock().attention)
            .unwrap_or(false)
    }

    /// The tab was looked at. Returns `true` if a badge was actually cleared.
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

    // -- evaluation ----------------------------------------------------------

    /// Re-evaluate one session with no new OS sample. Used by the pty
    /// coalescer's idle tick and by [`StatusEngine::report`].
    pub fn tick_session(&self, session_id: &str) -> bool {
        let now = now_ms();
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
        self.persist_io(session_id);
        changed
    }

    /// The 500 ms tick (DESIGN §6-4), with the OS half of the truth.
    ///
    /// `snap` is T07's `ps` snapshot; the foreground process group of each
    /// session is looked up in it and its CPU and RSS summed.
    pub fn tick(&self, snap: &Snapshot) {
        let now = now_ms();
        let signatures = self.signature_set();
        let ids: Vec<String> = self
            .sessions
            .iter()
            .map(|entry| entry.key().clone())
            .collect();

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
                    // PINNED §36: the CPU half of the recording. A replay with
                    // only the bytes cannot reproduce §35's bug, because half
                    // of it was a decaying `ps` average outranking a screen
                    // that had stopped.
                    crate::pty::dump::marker(
                        &id,
                        format_args!("cpu {:.1}", sample.cpu_pct),
                    );
                }
                self.step(&mut state, now, signatures.as_deref())
            };
            self.publish(&id, emit);
            self.persist_io(&id);
        }
        self.prune(now);
    }

    /// Sum the CPU and RSS of every process in the pty's foreground group.
    fn sample_foreground(&self, session_id: &str, snap: &Snapshot) -> Option<ForegroundSample> {
        let source = self.foreground.get(session_id)?.upgrade()?;
        let pgid = source.foreground_pgid()?;
        let pids = snap.pids_in_group(pgid);
        // A group with no rows is a group that ended between the `tcgetpgrp` and
        // the snapshot. Reporting 0 % would clear the busy latch on a lie, so
        // the sample is simply not taken.
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

    /// Evaluate + apply. Returns the report to publish, if any.
    fn step(
        &self,
        state: &mut SessionState,
        now: i64,
        signatures: Option<&Signatures>,
    ) -> Option<SessionStatusReport> {
        let verdict = state.evaluate(now, signatures)?;
        state.apply(verdict, now).then(|| state.report())
    }

    /// The current answer, re-evaluated first so the `session_send_prompt` gate
    /// never decides on a stale reading (M1-PLAN T16.8).
    pub fn report(&self, session_id: &str) -> SessionStatusReport {
        self.tick_session(session_id);
        match self.sessions.get(session_id) {
            Some(entry) => entry.lock().report(),
            // A session the engine has never heard of — one pruned after it
            // ended, or one opened before the engine was wired up. "Starting,
            // and we are not confident" is the only honest answer, and it is
            // still carrying evidence.
            None => SessionStatusReport {
                status: SessionStatus::Starting,
                confidence: CONF_STARTING,
                evidence: vec![StatusEvidence::OutputQuiet { ms: 0 }],
                since: now_ms(),
            },
        }
    }

    /// Drop the state of sessions that ended long enough ago.
    fn prune(&self, now: i64) {
        let stale: Vec<String> = self
            .sessions
            .iter()
            .filter(|entry| {
                entry
                    .value()
                    .lock()
                    .ended_at
                    .is_some_and(|at| now - at > PRUNE_AFTER_MS)
            })
            .map(|entry| entry.key().clone())
            .collect();
        for id in stale {
            self.forget(&id);
        }
    }

    // -- the ps snapshot -----------------------------------------------------

    /// Hand the engine a snapshot somebody else already paid for (T12).
    pub fn set_snapshot(&self, snap: Arc<Snapshot>) {
        self.snapshot_at.store(now_ms().max(0) as u64, Ordering::Relaxed);
        *self.snapshot.write() = Some(snap);
    }

    pub fn snapshot(&self) -> Option<Arc<Snapshot>> {
        self.snapshot.read().clone()
    }

    /// True when the cached snapshot is too old to base a CPU claim on.
    pub fn snapshot_is_stale(&self, now: i64) -> bool {
        let at = self.snapshot_at.load(Ordering::Relaxed) as i64;
        at == 0 || now - at > SNAPSHOT_MAX_AGE_MS
    }

    // -- persistence ---------------------------------------------------------

    /// `pty_session.last_io_at`, throttled (M1-PLAN T16.9).
    fn persist_io(&self, session_id: &str) {
        let due = {
            let Some(entry) = self.sessions.get(session_id) else {
                return;
            };
            let mut state = entry.lock();
            if state.last_io_at - state.persisted_io_at < IO_PERSIST_MS {
                None
            } else {
                state.persisted_io_at = state.last_io_at;
                Some(state.last_io_at)
            }
        };
        let Some(at) = due else { return };
        let Some(db) = self.db.read().clone() else {
            return;
        };
        let id = session_id.to_string();
        tauri::async_runtime::spawn(async move {
            if let Err(err) = db
                .write(move |conn| dao::pty_session::touch_io(conn, &id, at).map(|_| ()))
                .await
            {
                tracing::debug!(target: "status", error = %err, "could not record last_io_at");
            }
        });
    }

    fn persist_status(&self, session_id: &str, kind: &'static str) {
        let Some(db) = self.db.read().clone() else {
            return;
        };
        let id = session_id.to_string();
        tauri::async_runtime::spawn(async move {
            if let Err(err) = db
                .write(move |conn| dao::pty_session::set_status(conn, &id, kind).map(|_| ()))
                .await
            {
                tracing::debug!(target: "status", error = %err, "could not record last_status");
            }
        });
    }

    // -- publication ---------------------------------------------------------

    /// `gilde://session/status` + the graph patch, with the evidence vector whole.
    fn publish(&self, session_id: &str, report: Option<SessionStatusReport>) {
        let Some(report) = report else { return };
        debug_assert!(
            !report.evidence.is_empty(),
            "M1-PLAN T16.4: a status is never published without evidence"
        );
        self.persist_status(session_id, report.status.kind());

        let Some(bus) = self.events.read().clone() else {
            return;
        };
        bus.push(GraphOp::PatchNode {
            id: node_id(NodeKind::Session, session_id),
            status: Some(node_status_of(&report.status)),
            confidence: Some(report.confidence),
            evidence: Some(report.evidence.clone()),
            badges: None,
            metrics: None,
        });
        bus.emit(
            events::SESSION_STATUS,
            SessionStatusEvent {
                session_id: session_id.to_string(),
                status: report.status,
                confidence: report.confidence,
                evidence: report.evidence,
                since: report.since,
                at: now_ms(),
            },
        );
    }

    // -- the ticker ----------------------------------------------------------

    /// The 500 ms evaluation loop (DESIGN §6-4).
    ///
    /// It takes a `ps` snapshot only when nobody else has provided a fresh one
    /// and only while there is a session to evaluate: an app sitting on an empty
    /// window runs no `ps` at all.
    pub fn spawn_ticker(engine: Arc<StatusEngine>) -> tauri::async_runtime::JoinHandle<()> {
        tauri::async_runtime::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(TICK_MS));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                if engine.is_empty() {
                    continue;
                }
                if engine.snapshot_is_stale(now_ms()) {
                    match tauri::async_runtime::spawn_blocking(
                        crate::supervise::procscan::ps_snapshot,
                    )
                    .await
                    {
                        Ok(Ok(snap)) => engine.set_snapshot(Arc::new(snap)),
                        Ok(Err(err)) => {
                            // The OS signal is gone for this round; the other two
                            // still work, at a lower confidence. Not an error.
                            tracing::debug!(target: "status", error = %err, "no ps snapshot this tick");
                        }
                        Err(err) => {
                            tracing::debug!(target: "status", error = %err, "ps snapshot task failed")
                        }
                    }
                }
                match engine.snapshot() {
                    Some(snap) => engine.tick(&snap),
                    // Evaluate anyway: two of the three signals need no snapshot.
                    None => {
                        let ids: Vec<String> = engine
                            .sessions
                            .iter()
                            .map(|entry| entry.key().clone())
                            .collect();
                        for id in ids {
                            engine.tick_session(&id);
                        }
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::signatures::SignatureFile;
    use crate::supervise::procscan::PsRow;
    use std::path::Path;

    fn engine_with_defaults() -> Arc<StatusEngine> {
        let engine = Arc::new(StatusEngine::new());
        engine.set_signatures(Arc::new(StaticSignatures::defaults()));
        engine
    }

    /// Drive the state directly so a test can pin an exact `now` instead of
    /// sleeping. Everything the engine does is a pure function of these.
    fn state(agent: &str, now: i64) -> SessionState {
        SessionState::new(agent.to_string(), 1_200, now).with_stall(DEFAULT_STALL_AFTER_MS)
    }

    fn signatures() -> Arc<Signatures> {
        Arc::new(Signatures::compile(&SignatureFile::defaults()))
    }

    fn advance(state: &mut SessionState, now: i64, sigs: &Signatures) -> bool {
        match state.evaluate(now, Some(sigs)) {
            Some(verdict) => state.apply(verdict, now),
            None => false,
        }
    }

    #[test]
    fn starting_holds_until_the_first_byte_or_fifteen_hundred_millis() {
        let sigs = signatures();
        let mut s = state("claude", 1_000);
        assert!(!advance(&mut s, 1_400, &sigs));
        assert!(matches!(s.status, SessionStatus::Starting));

        // The grace expires and the table runs: quiet, nothing matched.
        assert!(advance(&mut s, 2_600, &sigs));
        assert!(matches!(s.status, SessionStatus::Idle));
        assert_eq!(s.confidence, CONF_FALLBACK_IDLE);
        assert!(!s.evidence.is_empty());
    }

    #[test]
    fn output_moves_a_session_to_working_immediately() {
        let sigs = signatures();
        let mut s = state("claude", 0);
        s.observe_output(b"hello", 100);
        assert!(advance(&mut s, 100, &sigs));
        match &s.status {
            SessionStatus::Working { since, .. } => assert_eq!(*since, 100),
            other => panic!("expected working, got {other:?}"),
        }
        assert_eq!(s.confidence, CONF_OUTPUT_BURST);
        assert!(matches!(
            s.evidence.first(),
            Some(StatusEvidence::OutputBurst { .. })
        ));

        // More output does not restart the clock.
        s.observe_output(b"more", 300);
        advance(&mut s, 300, &sigs);
        match &s.status {
            SessionStatus::Working { since, .. } => {
                assert_eq!(*since, 100, "working since the first byte, not the last")
            }
            other => panic!("expected working, got {other:?}"),
        }
    }

    #[test]
    fn a_matched_prompt_makes_the_session_idle_with_real_confidence() {
        let sigs = signatures();
        let mut s = state("claude", 0);
        // What claude's input box looks like once the turn is over.
        s.observe_output("│ > │\n".as_bytes(), 100);
        assert!(advance(&mut s, 2_000, &sigs));
        assert!(matches!(s.status, SessionStatus::Idle));
        assert_eq!(s.confidence, CONF_IDLE_PATTERN);
        match s.evidence.first() {
            Some(StatusEvidence::PatternMatch { class, line, .. }) => {
                assert_eq!(*class, PatternClass::Idle);
                assert_eq!(line, "│ > │");
            }
            other => panic!("expected a pattern match, got {other:?}"),
        }
    }

    #[test]
    fn an_activity_line_beats_a_composer_drawn_below_it() {
        // codex's real layout during a turn, and the reason the tail is searched
        // class-first: the composer placeholder is the *newest* line and it is a
        // perfectly good `idle` match.
        let sigs = signatures();
        let mut s = state("codex", 0);
        s.observe_output(
            "• Working (0s • esc to interrupt)\n› Find and fix a bug in @filename\n  gpt-6-astra low · /tmp/x\n"
                .as_bytes(),
            100,
        );
        assert!(advance(&mut s, 2_000, &sigs));
        assert!(
            matches!(s.status, SessionStatus::Working { .. }),
            "expected working, got {:?}",
            s.status
        );
    }

    #[test]
    fn a_permission_dialog_wins_over_the_spinner_on_the_same_screen() {
        let sigs = signatures();
        let mut s = state("claude", 0);
        s.observe_output(
            b"  Working... (esc to interrupt)\nDo you want to proceed?\n  1. Yes\n",
            100,
        );
        assert!(advance(&mut s, 2_000, &sigs));
        match &s.status {
            SessionStatus::WaitingForInput { prompt } => {
                assert!(prompt.contains("Yes") || prompt.contains("proceed"))
            }
            other => panic!("a blocking dialog must not be reported as working: {other:?}"),
        }
        assert_eq!(s.confidence, CONF_WAITING_PATTERN);
    }

    /// BACKLOG M1.5 #2, the screen from the 2026-09-08 session.
    ///
    /// claude was blocked on a selection menu and the pill said 유휴, because the
    /// newest line on that screen ("⏵⏵ auto mode on") is an `idle` match and
    /// nothing on it was a `waiting` match. The menu is the only thing between
    /// the user and their turn, so it has to win — with the evidence that says
    /// which line decided it.
    #[test]
    fn a_selection_menu_is_waiting_for_input_and_says_which_line_decided() {
        let sigs = signatures();
        let mut s = state("claude", 0);
        s.observe_output(
            concat!(
                "  Which approach should I take?\n",
                "  ❯ 1. Rewrite the reader thread\n",
                "    2. Keep the current one\n",
                "  Enter to select · ↑/↓ to navigate · Esc to cancel\n",
                "⏵⏵ auto mode on\n",
            )
            .as_bytes(),
            100,
        );
        assert!(advance(&mut s, 2_000, &sigs));
        match &s.status {
            SessionStatus::WaitingForInput { prompt } => assert!(
                prompt.contains("Enter to select") || prompt.contains("1. Rewrite"),
                "the prompt must quote the menu, got {prompt:?}"
            ),
            other => panic!("a selection menu must not be reported as idle: {other:?}"),
        }
        assert_eq!(s.confidence, CONF_WAITING_PATTERN);
        match s.evidence.first() {
            Some(StatusEvidence::PatternMatch {
                class,
                line,
                signature_id,
            }) => {
                assert_eq!(*class, PatternClass::Waiting);
                assert!(
                    signature_id.starts_with("claude.waiting."),
                    "the evidence names the pattern that fired: {signature_id}"
                );
                assert!(line.contains("Enter to select") || line.contains("1. Rewrite"));
            }
            other => panic!("status is never reported without its evidence: {other:?}"),
        }
        assert!(
            s.evidence
                .iter()
                .any(|e| matches!(e, StatusEvidence::OutputQuiet { .. })),
            "the quiet observation is carried too"
        );
    }

    /// The same screen once the menu has been answered: the composer is back and
    /// the session is idle again. Without this, "always waiting" would pass the
    /// test above just as well as the fix does.
    #[test]
    fn the_composer_after_a_menu_is_idle_again() {
        let sigs = signatures();
        let mut s = state("claude", 0);
        s.observe_output("│ > │\n⏵⏵ auto mode on\n".as_bytes(), 100);
        assert!(advance(&mut s, 2_000, &sigs));
        assert!(
            matches!(s.status, SessionStatus::Idle),
            "expected idle, got {:?}",
            s.status
        );
    }

    #[test]
    fn foreground_cpu_forces_working_no_matter_how_quiet_it_is() {
        // M1-PLAN T16.3, pinned.
        let sigs = signatures();
        let mut s = state("claude", 0);
        // An idle prompt is on screen *and* the session has been silent for two
        // minutes — every other signal says idle.
        s.observe_output("│ > │\n".as_bytes(), 100);
        s.foreground = Some(ForegroundSample {
            pgid: 4_242,
            cpu_pct: 97.5,
            rss_bytes: 512 * 1024 * 1024,
            at: 200,
        });
        s.cpu.observe(97.5);

        assert!(advance(&mut s, 200_000, &sigs));
        assert!(
            matches!(s.status, SessionStatus::Working { .. }),
            "a busy foreground group outranks silence and an idle prompt"
        );
        assert_eq!(s.confidence, CONF_FOREGROUND_CPU);
        assert!(s
            .evidence
            .iter()
            .any(|e| matches!(e, StatusEvidence::ForegroundGroup { cpu_pct, .. } if *cpu_pct > 90.0)));

        // One quiet sample is not enough to let go.
        s.cpu.observe(0.2);
        advance(&mut s, 200_500, &sigs);
        assert!(matches!(s.status, SessionStatus::Working { .. }));

        // Two are.
        s.cpu.observe(0.2);
        assert!(advance(&mut s, 201_000, &sigs));
        assert!(matches!(s.status, SessionStatus::Idle));
        assert_eq!(s.confidence, CONF_IDLE_PATTERN);
    }

    #[test]
    fn a_long_silence_with_nothing_to_show_becomes_stalled() {
        let sigs = signatures();
        let mut s = state("codex", 0);
        s.observe_output(b"some output with no recognisable prompt\n", 100);
        assert!(advance(&mut s, 5_000, &sigs));
        assert!(matches!(s.status, SessionStatus::Idle));
        assert_eq!(s.confidence, CONF_FALLBACK_IDLE);

        assert!(advance(&mut s, 100 + DEFAULT_STALL_AFTER_MS as i64 + 1, &sigs));
        match s.status {
            SessionStatus::Stalled { quiet_ms } => assert!(quiet_ms >= DEFAULT_STALL_AFTER_MS),
            ref other => panic!("expected stalled, got {other:?}"),
        }
        assert_eq!(s.confidence, CONF_STALLED);
    }

    #[test]
    fn the_debounce_holds_everything_but_working_and_death() {
        // Driven through `apply` directly: the debounce is about the *rate* of
        // change, and going through `evaluate` would need a minute of simulated
        // silence between each verdict to produce one.
        let quiet = vec![StatusEvidence::OutputQuiet { ms: 1_500 }];
        let dialog = |at: i64| {
            Verdict::new(
                SessionStatus::WaitingForInput {
                    prompt: "Do you want to proceed?".into(),
                },
                CONF_WAITING_PATTERN,
                quiet.clone(),
            )
            .with(StatusEvidence::OutputQuiet { ms: at as u64 })
        };

        let mut s = state("claude", 0);
        assert!(s.apply(
            Verdict::new(SessionStatus::Idle, CONF_IDLE_PATTERN, quiet.clone()),
            2_000
        ));
        assert!(matches!(s.status, SessionStatus::Idle));

        // 100 ms later: a real change, held.
        assert!(!s.apply(dialog(2_100), 2_100), "held by the debounce");
        assert!(matches!(s.status, SessionStatus::Idle));

        // Nothing is lost — the next evaluation, past the window, lands it.
        assert!(s.apply(dialog(2_400), 2_400));
        assert!(matches!(s.status, SessionStatus::WaitingForInput { .. }));

        // Working never waits: the reactivity path is the whole point.
        assert!(
            s.apply(
                Verdict::new(
                    SessionStatus::Working {
                        since: 0,
                        hint: None
                    },
                    CONF_OUTPUT_BURST,
                    vec![StatusEvidence::OutputBurst { bytes_per_s: 4_096 }],
                ),
                2_450
            ),
            "working is immediate"
        );

        // Neither does death: a reaped child is never drawn as a live one.
        assert!(s.settle(
            SessionStatus::Exited {
                code: Some(0),
                signal: None
            },
            vec![StatusEvidence::ChildExited { code: Some(0) }],
            2_460
        ));
    }

    #[test]
    fn a_terminal_status_is_never_walked_back() {
        let sigs = signatures();
        let mut s = state("claude", 0);
        s.settle(
            SessionStatus::Exited {
                code: Some(3),
                signal: None,
            },
            vec![StatusEvidence::ChildExited { code: Some(3) }],
            10,
        );
        s.observe_output(b"ghost output\n", 20);
        assert!(!advance(&mut s, 20, &sigs), "a dead child cannot go working");
        assert!(!advance(&mut s, 500_000, &sigs));
        assert!(matches!(
            s.status,
            SessionStatus::Exited { code: Some(3), .. }
        ));
    }

    #[test]
    fn every_status_carries_evidence() {
        let sigs = signatures();
        let mut s = state("claude", 0);
        for now in [2_000i64, 2_600, 200_000] {
            advance(&mut s, now, &sigs);
            assert!(
                !s.evidence.is_empty(),
                "status {:?} shipped without evidence",
                s.status
            );
        }
        s.observe_output(b"x", 200_100);
        advance(&mut s, 200_100, &sigs);
        assert!(!s.evidence.is_empty());
    }

    #[test]
    fn a_bell_is_latched_and_reported_even_when_nothing_else_changed() {
        let sigs = signatures();
        let mut s = state("claude", 0);
        s.observe_output("│ > │\n".as_bytes(), 100);
        advance(&mut s, 2_000, &sigs);
        assert!(matches!(s.status, SessionStatus::Idle));
        assert!(!s.attention);

        s.observe_output(b"\x07", 3_000);
        assert!(s.attention, "the bell raised the flag");
        // The next evaluation is a no-op for the status, but the bell still has
        // to reach the UI.
        assert!(advance(&mut s, 5_000, &sigs));
        assert!(s.evidence.contains(&StatusEvidence::Bell));
        // Reported once; the flag itself stays up until the tab is looked at.
        assert!(!advance(&mut s, 6_000, &sigs));
        assert!(s.attention);
    }

    #[test]
    fn alt_screen_shows_up_as_evidence() {
        let sigs = signatures();
        let mut s = state("codex", 0);
        s.observe_output(b"\x1b[?1049h", 100);
        advance(&mut s, 3_000, &sigs);
        assert!(s.evidence.contains(&StatusEvidence::AltScreen { on: true }));
    }

    // -- BACKLOG M1.5 15b: a quiet alternate screen is waiting, not working ---
    //
    // The 2026-09-09 hands-on session: claude's `/help` was on screen and the
    // pill said `Working`. Nothing was working — the child had painted a
    // full-screen view and stopped, and the only thing left to happen was the
    // user pressing Esc. DESIGN §6-4 answers it with three observations that
    // have to agree, and these tests are each one of them being wrong.

    /// A low-CPU foreground sample, i.e. "the group behind this pty is idle".
    fn quiet_group(at: i64) -> ForegroundSample {
        ForegroundSample {
            pgid: 5_150,
            cpu_pct: 0.4,
            rss_bytes: 180 * 1024 * 1024,
            at,
        }
    }

    /// Enter the alternate screen and paint `body` into it.
    fn alt_screen(s: &mut SessionState, body: &str, at: i64) {
        s.observe_output(format!("\x1b[?1049h{body}").as_bytes(), at);
    }

    #[test]
    fn a_quiet_alternate_screen_with_no_cpu_is_waiting_for_input() {
        let sigs = signatures();
        let mut s = state("claude", 0);
        alt_screen(
            &mut s,
            concat!(
                "  Help\n",
                "  /clear      Clear conversation history\n",
                "  /model      Set the model for this session\n",
                "  Esc to close\n",
            ),
            100,
        );
        s.foreground = Some(quiet_group(200));
        s.cpu.observe(0.4);
        s.cpu.observe(0.4);

        assert!(advance(&mut s, 3_000, &sigs));
        match &s.status {
            SessionStatus::WaitingForInput { prompt } => assert_eq!(
                prompt, "Esc to close",
                "the prompt quotes the line that says how to leave"
            ),
            other => panic!("a full-screen view nobody is drawing is not working: {other:?}"),
        }
        // `Esc to close` is a claude `waiting` signature (added in v3 for
        // exactly this footer), so once BACKLOG 24 let the pattern rule see a
        // settled screen it is the pattern — the vendor's own words — that
        // decides, at the higher confidence. The structural 0.65 rule is still
        // what catches a full-screen view with no recognisable wording on it
        // (`a_full_screen_picker_that_matches_no_signature_is_waiting_too`).
        assert_eq!(s.confidence, CONF_WAITING_PATTERN);

        // "alternate screen · quiet · low CPU" — all three, in the vector the
        // evidence popover renders one sentence per item from.
        assert!(s.evidence.contains(&StatusEvidence::AltScreen { on: true }));
        assert!(s
            .evidence
            .iter()
            .any(|e| matches!(e, StatusEvidence::OutputQuiet { ms } if *ms >= ALT_SCREEN_QUIET_MS)));
        assert!(s
            .evidence
            .iter()
            .any(|e| matches!(e, StatusEvidence::ForegroundGroup { cpu_pct, .. }
                if *cpu_pct < CPU_BUSY_PCT)));
    }

    #[test]
    fn a_full_screen_picker_that_matches_no_signature_is_waiting_too() {
        // The half of the report that no pattern can ever reach: a full-screen
        // list with none of the vendor's prompt wordings on it. Before the
        // rule this fell through to the `Idle` shrug at 0.40 or, once a
        // `working` line was still on screen, to `Working`.
        let sigs = signatures();
        let mut s = state("claude", 0);
        alt_screen(
            &mut s,
            "  Search files\n  src/pty/session.rs\n  src/status/engine.rs\n",
            100,
        );
        s.foreground = Some(quiet_group(200));
        s.cpu.observe(0.2);
        s.cpu.observe(0.2);

        assert!(advance(&mut s, 3_000, &sigs));
        assert!(
            matches!(s.status, SessionStatus::WaitingForInput { .. }),
            "expected waiting, got {:?}",
            s.status
        );
        assert_eq!(s.confidence, CONF_ALT_SCREEN_QUIET);
    }

    #[test]
    fn an_alternate_screen_burning_cpu_is_still_working() {
        // The rule sits after the CPU rule, so a TUI that is actually computing
        // — a build scrolling past, a diff being generated — keeps its
        // `Working` pill however quiet the last second happened to be.
        let sigs = signatures();
        let mut s = state("claude", 0);
        alt_screen(&mut s, "  Building…\n  Esc to close\n", 100);
        s.foreground = Some(ForegroundSample {
            pgid: 5_150,
            cpu_pct: 88.0,
            rss_bytes: 512 * 1024 * 1024,
            at: 200,
        });
        s.cpu.observe(88.0);

        assert!(advance(&mut s, 3_000, &sigs));
        assert!(
            matches!(s.status, SessionStatus::Working { .. }),
            "a busy foreground group outranks the alternate screen: {:?}",
            s.status
        );
        assert_eq!(s.confidence, CONF_FOREGROUND_CPU);
    }

    #[test]
    fn an_alternate_screen_that_is_still_painting_is_not_called_waiting_yet() {
        // Quiet, but not for long enough: a picker filtering as the user types
        // must not flicker to `Waiting` between keystrokes. `claude`'s
        // `idleAfterMs` is 1200, so this is the window the rule adds on top.
        let sigs = signatures();
        let mut s = state("claude", 0);
        alt_screen(&mut s, "  Search files\n  src/pty/session.rs\n", 100);
        s.cpu.observe(0.2);
        s.cpu.observe(0.2);

        advance(&mut s, 1_500, &sigs);
        assert!(
            !matches!(s.status, SessionStatus::WaitingForInput { .. }),
            "1400 ms of quiet is not a settled screen: {:?}",
            s.status
        );

        // …and once it is settled (and the 300 ms debounce has run out), it is.
        assert!(advance(&mut s, 2_000, &sigs));
        assert!(matches!(s.status, SessionStatus::WaitingForInput { .. }));
    }

    #[test]
    fn the_main_screen_is_untouched_by_the_alternate_screen_rule() {
        // Same silence, same idle group, no alternate screen: still the honest
        // shrug. Without this, "always waiting" would pass the tests above.
        let sigs = signatures();
        let mut s = state("claude", 0);
        s.observe_output(b"  a line that matches nothing at all\n", 100);
        s.foreground = Some(quiet_group(200));
        s.cpu.observe(0.2);
        s.cpu.observe(0.2);

        assert!(advance(&mut s, 3_000, &sigs));
        assert!(
            matches!(s.status, SessionStatus::Idle),
            "expected the fallback idle, got {:?}",
            s.status
        );
        assert_eq!(s.confidence, CONF_FALLBACK_IDLE);
    }

    // -- BACKLOG 24: a screen that repaints itself is not a screen that works -
    //
    // The 2026-09-09 re-verification put `/model` and `/help` back on `작업 중`
    // with the evidence `출력 2.1kB/s · 대체 화면 사용 중`. Claude Code redraws
    // those views continuously and *identically*, so the byte counter never went
    // quiet, and both rules that would have caught it — the alternate-screen
    // rule and the waiting-pattern rule — sat behind that counter. The fix is a
    // second clock (`ScreenContent`), and these are the frames it was written
    // against.

    /// How often claude repaints a full-screen view.
    const REPAINT_MS: i64 = 200;

    /// One `/model` frame, drawn the way a full-screen redraw is drawn: home
    /// the cursor, erase, paint the whole thing again.
    ///
    /// `spinner` is the only cell a caller can vary, which is what test (d)
    /// uses; `selected` moves the caret between options, which is a real
    /// change and must reset the clock.
    fn model_frame(spinner: char, selected: usize) -> String {
        let options = [
            "1. Default (recommended)",
            "2. Opus 4.6             ",
            "3. Sonnet 4.6           ",
        ];
        let mut out = String::from("\x1b[H\x1b[2J");
        out.push_str("╭──────────────────────────────╮\n");
        out.push_str(&format!("│{spinner} Select Model                │\n"));
        for (i, option) in options.iter().enumerate() {
            let caret = if i == selected { '❯' } else { ' ' };
            out.push_str(&format!("│ {caret} {option}   │\n"));
        }
        out.push_str(
            "│ Enter to set as default · s to use this session only · Esc to cancel │\n",
        );
        out.push_str("╰──────────────────────────────╯\n");
        out
    }

    /// Feed one frame every [`REPAINT_MS`] and report when the session first
    /// said `WaitingForInput`.
    fn repaint(
        s: &mut SessionState,
        sigs: &Signatures,
        frames: usize,
        mut frame: impl FnMut(usize) -> String,
    ) -> Option<i64> {
        let mut first_waiting = None;
        for i in 0..frames {
            let now = (i as i64 + 1) * REPAINT_MS;
            s.observe_output(frame(i).as_bytes(), now);
            advance(s, now, sigs);
            if first_waiting.is_none() && matches!(s.status, SessionStatus::WaitingForInput { .. })
            {
                first_waiting = Some(now);
            }
        }
        first_waiting
    }

    /// Two consecutive low samples, which is what it takes to clear the latch.
    fn idle_group(s: &mut SessionState, at: i64) {
        s.foreground = Some(quiet_group(at));
        s.cpu.observe(0.4);
        s.cpu.observe(0.4);
    }

    /// (a) The reported screen. Identical frames, ~2 kB/s, nothing computing.
    #[test]
    fn a_menu_that_repaints_itself_unchanged_is_waiting_not_working() {
        let sigs = signatures();
        let mut s = state("claude", 0);
        idle_group(&mut s, 100);

        let frame = model_frame(' ', 0);
        let rate = frame.len() as i64 * 1_000 / REPAINT_MS;
        assert!(
            (1_000..=4_000).contains(&rate),
            "the fixture has to be as loud as the real thing (~2 kB/s), got {rate} B/s"
        );

        // Enter the alternate screen on the first frame, exactly as `/model`
        // does, then redraw the same cells forever.
        let first_waiting = repaint(&mut s, &sigs, 15, |i| {
            let frame = model_frame(' ', 0);
            if i == 0 {
                format!("\x1b[?1049h{frame}")
            } else {
                frame
            }
        });

        let at = first_waiting.expect("a menu nobody is drawing is waiting for input");
        assert!(
            at - REPAINT_MS <= 2_000,
            "it must land within two seconds of the first frame, took {}ms",
            at - REPAINT_MS
        );
        match &s.status {
            SessionStatus::WaitingForInput { prompt } => assert!(
                prompt.contains("Esc to cancel"),
                "the prompt quotes the footer, got {prompt:?}"
            ),
            other => panic!("expected waiting, got {other:?}"),
        }
        assert_eq!(s.confidence, CONF_WAITING_PATTERN);

        // Both halves of the reasoning are in the vector: the vendor's wording
        // *and* the observation that made it reachable.
        assert!(
            s.evidence.iter().any(|e| matches!(
                e,
                StatusEvidence::PatternMatch {
                    class: PatternClass::Waiting,
                    ..
                }
            )),
            "the matched line is evidence: {:?}",
            s.evidence
        );
        assert!(
            s.evidence
                .iter()
                .any(|e| matches!(e, StatusEvidence::ScreenStable { ms } if *ms >= ALT_SCREEN_QUIET_MS)),
            "and so is the screen that stopped changing: {:?}",
            s.evidence
        );
    }

    /// (b) The same loudness with something actually happening in it.
    #[test]
    fn a_frame_whose_content_changes_every_tick_is_still_working() {
        let sigs = signatures();
        let mut s = state("claude", 0);
        idle_group(&mut s, 100);

        // A turn streaming text: same byte rate, different cells every frame.
        let first_waiting = repaint(&mut s, &sigs, 15, |i| {
            format!(
                "  … reading src/status/engine.rs, chunk {i} of many, {} bytes so far\n",
                (i + 1) * 4_096
            )
        });

        assert_eq!(
            first_waiting, None,
            "text that keeps arriving is work, however calm the screen looks"
        );
        assert!(
            matches!(s.status, SessionStatus::Working { .. }),
            "expected working, got {:?}",
            s.status
        );
        assert!(
            s.evidence
                .iter()
                .all(|e| !matches!(e, StatusEvidence::ScreenStable { .. })),
            "and nothing claims the screen held still: {:?}",
            s.evidence
        );
    }

    /// (c) `/help`: a full-screen page repainting in the alternate buffer.
    #[test]
    fn a_full_screen_page_repainting_in_the_alternate_buffer_is_waiting() {
        let sigs = signatures();

        // With claude's own footer on it, the pattern rule decides.
        let mut s = state("claude", 0);
        idle_group(&mut s, 100);
        let help = concat!(
            "\x1b[H\x1b[2J",
            "  Help\n",
            "  /clear      Clear conversation history\n",
            "  /model      Set the model for this session\n",
            "  /help       Show this page\n",
            "  Esc to cancel\n",
        );
        let waiting_at = repaint(&mut s, &sigs, 15, |i| {
            if i == 0 {
                format!("\x1b[?1049h{help}")
            } else {
                help.to_string()
            }
        });
        assert!(waiting_at.is_some(), "got {:?}", s.status);
        assert_eq!(s.confidence, CONF_WAITING_PATTERN);

        // With no wording any signature knows, the alternate-screen rule is
        // what catches it — the half of the report no regex can reach.
        let mut s = state("claude", 0);
        idle_group(&mut s, 100);
        let page = concat!(
            "\x1b[H\x1b[2J",
            "  Keyboard\n",
            "  ctrl+r      전사 검색\n",
            "  ctrl+o      출력 펼치기\n",
            "  shift+tab   모드 전환\n",
        );
        let waiting_at = repaint(&mut s, &sigs, 15, |i| {
            if i == 0 {
                format!("\x1b[?1049h{page}")
            } else {
                page.to_string()
            }
        });
        assert!(waiting_at.is_some(), "got {:?}", s.status);
        assert_eq!(s.confidence, CONF_ALT_SCREEN_QUIET);
        assert!(s.evidence.contains(&StatusEvidence::AltScreen { on: true }));
    }

    /// (d) A spinner is not content. One cell cycling `⠋⠙⠹` changes nothing a
    /// human would call a change, and must not hold the clock at zero forever.
    #[test]
    fn a_cycling_spinner_cell_still_counts_as_a_still_screen() {
        let sigs = signatures();
        let mut s = state("claude", 0);
        idle_group(&mut s, 100);

        const SPINNER: [char; 3] = ['⠋', '⠙', '⠹'];
        let first_waiting = repaint(&mut s, &sigs, 15, |i| {
            let frame = model_frame(SPINNER[i % SPINNER.len()], 0);
            if i == 0 {
                format!("\x1b[?1049h{frame}")
            } else {
                frame
            }
        });
        assert!(
            first_waiting.is_some(),
            "a spinning glyph on a menu is still a menu: {:?}",
            s.status
        );
        assert!(s
            .evidence
            .iter()
            .any(|e| matches!(e, StatusEvidence::ScreenStable { ms } if *ms >= ALT_SCREEN_QUIET_MS)));

        // A caret moving between options costs exactly two cells — the one it
        // left and the one it landed on — so it is inside the tolerance too.
        // That is the right answer rather than a hole in it: the hand moving
        // the caret is the *user's*, and a menu being arrowed through is a menu
        // still waiting to be answered.
        let mut moving = state("claude", 0);
        idle_group(&mut moving, 100);
        let first_waiting = repaint(&mut moving, &sigs, 15, |i| {
            let frame = model_frame(' ', i % 3);
            if i == 0 {
                format!("\x1b[?1049h{frame}")
            } else {
                frame
            }
        });
        assert!(
            first_waiting.is_some(),
            "arrowing through a menu is still waiting: {:?}",
            moving.status
        );

        // What the tolerance does *not* swallow: a screen whose text is being
        // rewritten. A picker filtering as the user types redraws just as
        // regularly and is not settled at all.
        let mut filtering = state("claude", 0);
        idle_group(&mut filtering, 100);
        let hits = [
            ("src/status/engine.rs", "src/status/evidence.rs"),
            ("src/pty/ansi.rs", "src/pty/manager.rs"),
            ("src/model/status.rs", "src/model/graph.rs"),
            ("src/agents/signatures.rs", "src/agents/registry.rs"),
        ];
        let first_waiting = repaint(&mut filtering, &sigs, 15, |i| {
            let (first, second) = hits[i % hits.len()];
            let frame = format!(
                "\x1b[H\x1b[2J  Search files\n  ❯ {first}\n    {second}\n  Esc to cancel\n"
            );
            if i == 0 {
                format!("\x1b[?1049h{frame}")
            } else {
                frame
            }
        });
        assert_eq!(
            first_waiting, None,
            "a list still being filtered has not settled: {:?}",
            filtering.status
        );
    }

    /// (e) The guard for the rule this one sits behind: a repainting full-screen
    /// view with a burning foreground group is still `Working`.
    #[test]
    fn a_repainting_alternate_screen_with_busy_cpu_is_still_working() {
        let sigs = signatures();
        let mut s = state("claude", 0);
        s.foreground = Some(ForegroundSample {
            pgid: 5_150,
            cpu_pct: 91.0,
            rss_bytes: 512 * 1024 * 1024,
            at: 100,
        });
        s.cpu.observe(91.0);

        let first_waiting = repaint(&mut s, &sigs, 15, |i| {
            let frame = model_frame(' ', 0);
            if i == 0 {
                format!("\x1b[?1049h{frame}")
            } else {
                frame
            }
        });
        assert_eq!(
            first_waiting, None,
            "a busy group outranks a still screen, alternate buffer or not"
        );
        assert!(
            matches!(s.status, SessionStatus::Working { .. }),
            "expected working, got {:?}",
            s.status
        );
        assert_eq!(s.confidence, CONF_FOREGROUND_CPU);
    }

    #[test]
    fn claude_reads_the_footer_of_its_full_screen_views_as_a_prompt() {
        // The signature half of BACKLOG M1.5 15b: `Esc to cancel` shipped in
        // v2, `Esc to close` — the wording claude's `/help` uses — did not.
        let sigs = signatures();
        let claude = sigs.for_agent("claude").expect("claude");
        for line in ["Esc to close", "  Esc to cancel  "] {
            assert_eq!(
                claude.match_line(line).map(|p| p.class),
                Some(PatternClass::Waiting),
                "{line:?} is a prompt, not a spinner"
            );
        }
    }

    // -- the engine surface --------------------------------------------------

    #[test]
    fn the_engine_registers_evaluates_and_forgets() {
        let engine = engine_with_defaults();
        engine.register("s1", "claude", 50, now_ms());
        assert!(engine.tracks("s1"));

        engine.on_output("s1", b"hello\n");
        let report = engine.report("s1");
        assert!(matches!(report.status, SessionStatus::Working { .. }));
        assert!(!report.evidence.is_empty());

        engine.on_exit("s1", Some(0), None);
        let report = engine.report("s1");
        assert!(matches!(
            report.status,
            SessionStatus::Exited { code: Some(0), .. }
        ));
        assert_eq!(report.confidence, CONF_EXITED);

        engine.forget("s1");
        assert!(!engine.tracks("s1"));
        // An unknown session still answers with evidence rather than a panic.
        assert!(!engine.report("s1").evidence.is_empty());
    }

    #[test]
    fn attention_survives_until_it_is_cleared() {
        let engine = engine_with_defaults();
        engine.register("s1", "claude", 50, now_ms());
        assert!(!engine.attention("s1"));
        engine.on_output("s1", b"done\x07");
        assert!(engine.attention("s1"));
        engine.on_output("s1", b"more output\n");
        assert!(engine.attention("s1"), "more output does not answer a bell");
        assert!(engine.clear_attention("s1"));
        assert!(!engine.attention("s1"));
        assert!(!engine.clear_attention("s1"));
    }

    #[test]
    fn the_engine_sums_the_foreground_group_from_a_ps_snapshot() {
        struct Fixed(i32);
        impl ForegroundSource for Fixed {
            fn foreground_pgid(&self) -> Option<i32> {
                Some(self.0)
            }
        }

        let engine = engine_with_defaults();
        engine.register("s1", "claude", 50, now_ms() - 10_000);
        let source: Arc<dyn ForegroundSource> = Arc::new(Fixed(700));
        engine.attach_foreground("s1", &source);

        let rows = vec![
            PsRow::parse_line("700 1 700 262144 41.5 01:00 me /bin/agent").expect("row"),
            PsRow::parse_line("701 700 700 131072 60.0 01:00 me /bin/agent-child").expect("row"),
            PsRow::parse_line("900 1 900 1024 0.0 01:00 me /bin/other").expect("row"),
        ];
        let snap = Snapshot::from_rows(rows);
        engine.tick(&snap);

        let report = engine.report("s1");
        assert!(
            matches!(report.status, SessionStatus::Working { .. }),
            "101.5% across the group is unmistakably working, got {:?}",
            report.status
        );
        match report
            .evidence
            .iter()
            .find(|e| matches!(e, StatusEvidence::ForegroundGroup { .. }))
        {
            Some(StatusEvidence::ForegroundGroup {
                pgid,
                cpu_pct,
                rss_bytes,
            }) => {
                assert_eq!(*pgid, 700);
                assert!((*cpu_pct - 101.5).abs() < 0.01);
                assert_eq!(*rss_bytes, (262_144 + 131_072) * 1024);
            }
            other => panic!("expected a foreground sample, got {other:?}"),
        }

        // The source going away must not keep the group pinned.
        drop(source);
        assert!(engine.sample_foreground("s1", &snap).is_none());
    }

    #[test]
    fn a_session_the_engine_never_saw_is_a_no_op_not_a_panic() {
        let engine = engine_with_defaults();
        engine.on_output("ghost", b"bytes");
        engine.on_input("ghost");
        engine.on_exit("ghost", None, Some(9));
        engine.on_killed("ghost");
        engine.on_detach("ghost");
        engine.on_failed("ghost", "nothing");
        assert!(!engine.tick_session("ghost"));
        assert!(engine.is_empty());
    }

    #[test]
    fn ended_sessions_are_pruned_and_live_ones_are_not() {
        let engine = engine_with_defaults();
        let long_ago = now_ms() - PRUNE_AFTER_MS - 1_000;
        engine.register("old", "claude", 50, long_ago);
        engine.register("live", "claude", 50, now_ms());
        engine.on_exit("old", Some(0), None);
        if let Some(entry) = engine.sessions.get("old") {
            entry.lock().ended_at = Some(long_ago);
        }

        engine.tick(&Snapshot::from_rows(Vec::new()));
        assert!(!engine.tracks("old"));
        assert!(engine.tracks("live"));
    }

    #[test]
    fn detach_and_kill_are_terminal_and_carry_evidence() {
        let engine = engine_with_defaults();
        engine.register("d", "claude", 50, now_ms());
        engine.on_detach("d");
        let report = engine.report("d");
        assert!(matches!(report.status, SessionStatus::Detached));
        assert!(!report.evidence.is_empty());
        engine.on_output("d", b"still talking\n");
        assert!(matches!(engine.report("d").status, SessionStatus::Detached));

        engine.register("k", "claude", 50, now_ms());
        engine.on_killed("k");
        assert!(matches!(engine.report("k").status, SessionStatus::Killed));
        assert!(!engine.report("k").evidence.is_empty());
    }

    #[test]
    fn editing_the_signatures_changes_the_classification_with_no_restart() {
        // M1-PLAN T16.5, the in-process half: the engine reads the source on
        // every evaluation, so a swap is picked up by the next tick.
        let source = Arc::new(StaticSignatures::defaults());
        let engine = Arc::new(StatusEngine::new());
        engine.set_signatures(source.clone());
        engine.register("s1", "claude", 50, now_ms() - 10_000);
        engine.on_output("s1", b"READY FOR ORDERS\n");
        // Past the 500 ms output-burst window, so the tail actually gets read.
        std::thread::sleep(Duration::from_millis(OUTPUT_BURST_MS as u64 + 120));

        let before = engine.report("s1");
        assert_eq!(before.confidence, CONF_FALLBACK_IDLE, "nothing matched yet");

        let edited: SignatureFile = serde_json::from_str(
            r#"{"version":1,"agents":{"claude":{"idle":["READY FOR ORDERS","(unclosed"]}}}"#,
        )
        .expect("edited file parses");
        let merged = SignatureFile::defaults().merge_over_defaults(&edited);
        let compiled = Signatures::compile(&merged);
        assert_eq!(
            compiled.warnings().len(),
            1,
            "the broken pattern is one warning"
        );
        source.set(compiled);

        let after = engine.report("s1");
        assert_eq!(
            after.confidence, CONF_IDLE_PATTERN,
            "the new pattern classifies, and the broken one did not disable the agent"
        );
        assert!(matches!(after.status, SessionStatus::Idle));
    }

    // -- PINNED §35: the recorded stream, replayed with its own timing --------

    /// One record of a `GCAP1` capture.
    struct CapRecord {
        /// `0` pty output, `1` marker, `2` pty input (PINNED §36).
        kind: u8,
        at_ms: i64,
        payload: Vec<u8>,
    }

    impl CapRecord {
        fn is_output(&self) -> bool {
            self.kind == crate::pty::dump::KIND_OUTPUT
        }
        fn is_marker(&self) -> bool {
            self.kind == crate::pty::dump::KIND_MARKER
        }
        fn is_input(&self) -> bool {
            self.kind == crate::pty::dump::KIND_INPUT
        }
    }

    /// Parse the capture: `GCAP1\n`, then `u8 kind · u32 ms · u32 len · bytes`,
    /// all little endian. See the fixture README.
    fn capture(name: &str) -> Vec<CapRecord> {
        let path = match std::env::var("GILDE_CAP") {
            // The diagnostic points at captures that never became fixtures.
            Ok(other) => std::path::PathBuf::from(other),
            Err(_) => Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/streams")
                .join(name),
        };
        let bytes = std::fs::read(&path)
            .unwrap_or_else(|e| panic!("fixture {} is missing: {e}", path.display()));
        assert!(bytes.starts_with(b"GCAP1\n"), "not a GCAP1 capture");
        let mut out = Vec::new();
        let mut i = 6;
        while i + 9 <= bytes.len() {
            let kind = bytes[i];
            let at = u32::from_le_bytes(bytes[i + 1..i + 5].try_into().expect("4 bytes"));
            let len = u32::from_le_bytes(bytes[i + 5..i + 9].try_into().expect("4 bytes")) as usize;
            i += 9;
            out.push(CapRecord {
                kind,
                at_ms: at as i64,
                payload: bytes[i..i + len].to_vec(),
            });
            i += len;
        }
        out
    }

    /// Every marker whose text is `label`, in order.
    fn markers_at(records: &[CapRecord], label: &str) -> Vec<i64> {
        let found: Vec<i64> = records
            .iter()
            .filter(|r| r.is_marker() && r.payload == label.as_bytes())
            .map(|r| r.at_ms)
            .collect();
        assert!(!found.is_empty(), "no marker {label:?} in the capture");
        found
    }

    /// The one marker whose text is `label`.
    fn marker_at(records: &[CapRecord], label: &str) -> i64 {
        markers_at(records, label)[0]
    }

    /// How long a full-screen view may read `Working` after it was drawn.
    ///
    /// The user-visible contract of PINNED §35: open `/model`, and by the time
    /// you have read the first option the pill says `입력 대기`.
    const SETTLES_WITHIN_MS: i64 = 2_000;

    /// Replay the capture into a session, ticking every [`TICK_MS`] the way the
    /// real ticker does, and report the first moment inside `[from, to]` at
    /// which the session said `WaitingForInput` — plus the status it held at
    /// `to`.
    ///
    /// **Both** recorded inputs are replayed: the pty bytes at the millisecond
    /// they arrived, and the `ps -o %cpu` reading of the foreground group at the
    /// tick it was taken (the `cpu <pct>` markers). A replay of the bytes alone
    /// cannot reproduce the reported bug, because half of it was a decaying CPU
    /// average outranking a screen that had stopped (PINNED §35).
    ///
    /// `probe` sees every evaluation, which is what the `#[ignore]`d diagnostic
    /// below prints.
    fn replay(
        records: &[CapRecord],
        from: i64,
        to: i64,
        mut probe: impl FnMut(&SessionState, i64),
    ) -> (Option<i64>, SessionStatus) {
        let sigs = signatures();
        let mut s = state("claude", 0);
        // Until the recording's first `ps` reading lands.
        idle_group(&mut s, 0);

        let mut first_waiting = None;
        let mut next_tick = 0i64;
        let mut cursor = 0usize;
        // The replay stops with the recording; `to` only bounds the window the
        // first `WaitingForInput` is looked for in.
        let end = records.last().map(|r| r.at_ms).unwrap_or(0);
        let mut now = 0i64;
        while now <= end {
            // Every chunk that arrived at or before `now`, in order.
            while cursor < records.len() && records[cursor].at_ms <= now {
                let record = &records[cursor];
                if record.is_marker() {
                    let text = String::from_utf8_lossy(&record.payload);
                    if let Some(pct) = text
                        .strip_prefix("cpu ")
                        .and_then(|v| v.parse::<f32>().ok())
                    {
                        s.cpu.observe(pct);
                        s.foreground = Some(ForegroundSample {
                            cpu_pct: pct,
                            at: now,
                            ..quiet_group(now)
                        });
                    } else if text.starts_with("resize ") {
                        // PINNED §36: the third recorded input. `TIOCSWINSZ` is
                        // an *app* action with a byte-for-byte visible
                        // consequence, and a replay that dropped it would
                        // replay a stream nothing in the app ever produced.
                        s.resized_at = now;
                    }
                } else if record.is_output() {
                    s.observe_output(&record.payload, now);
                    advance(&mut s, now, &sigs);
                }
                // Kind 2 is what the frontend wrote *to* the child. It is in the
                // recording so the query/reply dialogue can be read; it is not
                // an input to the engine, which never sees the user's keys.
                cursor += 1;
            }
            if now >= next_tick {
                advance(&mut s, now, &sigs);
                next_tick = now + TICK_MS as i64;
                probe(&s, now);
                if first_waiting.is_none()
                    && (from..=to).contains(&now)
                    && matches!(s.status, SessionStatus::WaitingForInput { .. })
                {
                    first_waiting = Some(now);
                }
            }
            // The engine's own resolution: no rule moves faster than a sample.
            now += CONTENT_SAMPLE_MS / 3;
        }
        (first_waiting, s.status.clone())
    }

    /// When the first byte of the repaint that follows `label` arrived — the
    /// moment the screen the assertions talk about started being drawn.
    fn drawn_after(records: &[CapRecord], label: &str) -> i64 {
        let from = marker_at(records, label);
        records
            .iter()
            .find(|r| r.is_output() && r.at_ms >= from)
            .unwrap_or_else(|| panic!("nothing was drawn after {label:?}"))
            .at_ms
    }

    /// (§35-a) The reported screen, from the recording: `/model` reads as
    /// waiting within two seconds of being drawn.
    ///
    /// Green before this change as well as after — the recording's own `ps`
    /// readings stayed under 3 % through this window, and the CPU half of §35
    /// is pinned by (§35-e) instead. What this one guards is the *contract*:
    /// the §33 shape of the engine, where the byte rate outranked every screen
    /// rule, fails it outright.
    #[test]
    fn the_recorded_model_menu_settles_to_waiting_within_two_seconds() {
        let records = capture("claude-model-help.cap");
        let drawn = drawn_after(&records, "RECORD model 8s");
        let (waiting, status) = replay(&records, drawn, drawn + SETTLES_WITHIN_MS, |_, _| {});
        let waiting = waiting.unwrap_or_else(|| {
            panic!("the /model menu never stopped being 작업 중; it ended {status:?}")
        });
        assert!(
            waiting - drawn <= SETTLES_WITHIN_MS,
            "took {}ms to leave Working",
            waiting - drawn
        );
    }

    /// (§35-b) The same for `/help`, whose page arrives in several chunks —
    /// the reactivity path is *right* to hold `Working` while it is still
    /// painting, and has to let go once it stops.
    #[test]
    fn the_recorded_help_page_settles_to_waiting_within_two_seconds() {
        let records = capture("claude-model-help.cap");
        let drawn = drawn_after(&records, "RECORD help 8s");
        let (waiting, status) = replay(&records, drawn, drawn + SETTLES_WITHIN_MS, |_, _| {});
        assert!(
            waiting.is_some(),
            "the /help page never stopped being 작업 중; it ended {status:?}"
        );
    }

    /// (§35-c) The reported *symptom*: a person arrowing through the menu.
    ///
    /// Every keypress is a repaint, so every keypress used to buy another
    /// second and a half of `작업 중` — which is what "작업 중, 1초 전, over and
    /// over" was. After each arrow the session is back to `WaitingForInput`
    /// inside the same two seconds.
    #[test]
    fn arrowing_through_the_recorded_menu_comes_back_to_waiting_each_time() {
        let records = capture("claude-model-help.cap");
        let mut arrows = markers_at(&records, "arrow down");
        arrows.extend(markers_at(&records, "arrow up"));
        assert!(arrows.len() >= 3, "the capture is meant to have three");
        for at in arrows {
            // From the repaint the key caused, not from the key: the child
            // takes a few tens of milliseconds to answer, and those are not
            // ours to account for.
            let drawn = records
                .iter()
                .find(|r| r.is_output() && r.at_ms >= at)
                .map(|r| r.at_ms)
                .unwrap_or(at);
            let (waiting, status) = replay(&records, drawn, drawn + SETTLES_WITHIN_MS, |_, _| {});
            assert!(
                waiting.is_some(),
                "still 작업 중 two seconds after the arrow key at {at}ms: {status:?}"
            );
        }
    }

    /// (§35-d) The caret-only repaint never leaves `WaitingForInput` at all.
    ///
    /// The first arrow in the recording moves the `❯` between two rows and
    /// nothing else — two cells, plus the cursor. That is the case §34 claimed
    /// was absorbed and was not.
    #[test]
    fn the_caret_only_repaint_in_the_recording_never_reads_as_working() {
        let records = capture("claude-model-help.cap");
        let at = marker_at(&records, "arrow down");
        let mut worked_at = None;
        replay(&records, 0, i64::MAX, |s, now| {
            if (at..at + SETTLES_WITHIN_MS).contains(&now)
                && matches!(s.status, SessionStatus::Working { .. })
                && worked_at.is_none()
            {
                worked_at = Some(now);
            }
        });
        assert_eq!(
            worked_at, None,
            "moving the caret one row is the user's hand, not the agent's work"
        );
    }

    /// (§35-e) A cooling `ps` average does not outrank a screen that has
    /// provably stopped — and a real one still does.
    #[test]
    fn a_cooling_repaint_does_not_outrank_a_screen_that_has_stopped() {
        // Measured: claude sitting on a finished `/model` reported 11.5 % with
        // `ScreenStable { ms: 1692 }` in the same evidence vector, because
        // `ps -o %cpu` is a decaying average of the repaint it had just done.
        let sigs = signatures();
        let mut s = state("claude", 0);
        let frame = concat!(
            "\x1b[?1049h\x1b[H\x1b[2J",
            "  Select model\n",
            "  ❯ 1. Default\n",
            "  Esc to cancel\n",
        );
        s.observe_output(frame.as_bytes(), 100);
        for at in [200, 400, 600, 800, 1_000, 1_200, 1_400, 1_600, 1_800] {
            s.foreground = Some(ForegroundSample {
                pgid: 5_150,
                cpu_pct: 11.5,
                rss_bytes: 300 * 1024 * 1024,
                at,
            });
            s.cpu.observe(11.5);
            advance(&mut s, at, &sigs);
        }
        assert!(
            matches!(s.status, SessionStatus::WaitingForInput { .. }),
            "11.5% on a screen that has not changed for 1.7s is a repaint \
             cooling down, got {:?}",
            s.status
        );

        // The guard: real computation still wins, however still the frame is.
        let mut s = state("claude", 0);
        s.observe_output(frame.as_bytes(), 100);
        for at in [200, 400, 600, 800, 1_000, 1_200, 1_400, 1_600, 1_800] {
            s.foreground = Some(ForegroundSample {
                pgid: 5_150,
                cpu_pct: 40.0,
                rss_bytes: 300 * 1024 * 1024,
                at,
            });
            s.cpu.observe(40.0);
            advance(&mut s, at, &sigs);
        }
        assert!(
            matches!(s.status, SessionStatus::Working { .. }),
            "40% is a tool call behind a frozen frame, got {:?}",
            s.status
        );
        assert_eq!(s.confidence, CONF_FOREGROUND_CPU);
    }

    // -- PINNED §36: the packaged app's own recording ------------------------

    /// How long the whole recorded window may spend on `Working` before the
    /// pill is lying to somebody reading a menu.
    ///
    /// The recording is a session sitting on `/model` and `/help` with a person
    /// reading them. Two full repaints of the app's own making arrive every two
    /// seconds; none of them is the agent doing anything.
    const RESIZE_LOOP_WORKING_BUDGET: f32 = 0.15;

    /// (§36-a) The real app's `/model`, recorded through the real reader
    /// thread, settles to `WaitingForInput` within two seconds — and **stays**
    /// there while the resize loop keeps repainting it.
    ///
    /// Before the fix this fails on the second half: the menu reaches
    /// `WaitingForInput` at the top of every two-second cycle and is thrown
    /// back to `Working` by the repaint the app's own `TIOCSWINSZ` provoked.
    #[test]
    fn the_recorded_app_model_menu_settles_and_stays_settled() {
        let records = capture("claude-resize-loop.cap");
        let drawn = drawn_after(&records, "RECORD model 8s");
        let end = marker_at(&records, "END model");
        let (waiting, status) = replay(&records, drawn, drawn + SETTLES_WITHIN_MS, |_, _| {});
        let waiting = waiting.unwrap_or_else(|| {
            panic!("the app's own /model never left 작업 중; it ended {status:?}")
        });
        assert!(
            waiting - drawn <= SETTLES_WITHIN_MS,
            "took {}ms to leave Working",
            waiting - drawn
        );

        // …and then holds it. `waiting + SETTLES_WITHIN_MS` skips the arrow
        // key's own repaint, which is allowed its two seconds like any other.
        let mut samples = 0usize;
        let mut working = 0usize;
        replay(&records, 0, i64::MAX, |s, now| {
            if !(waiting + SETTLES_WITHIN_MS..end).contains(&now) {
                return;
            }
            samples += 1;
            if matches!(s.status, SessionStatus::Working { .. }) {
                working += 1;
            }
        });
        assert!(samples > 10, "the window has to be worth measuring: {samples}");
        let share = working as f32 / samples as f32;
        assert!(
            share <= RESIZE_LOOP_WORKING_BUDGET,
            "the menu read 작업 중 for {:.0}% of the {samples} samples a person \
             spent reading it",
            share * 100.0
        );
    }

    /// (§36-b) The same for `/help`.
    #[test]
    fn the_recorded_app_help_page_settles_and_stays_settled() {
        let records = capture("claude-resize-loop.cap");
        let drawn = drawn_after(&records, "RECORD help 8s");
        let end = marker_at(&records, "END help");
        let (waiting, status) = replay(&records, drawn, drawn + SETTLES_WITHIN_MS, |_, _| {});
        let waiting = waiting
            .unwrap_or_else(|| panic!("the app's own /help never settled; it ended {status:?}"));

        let mut samples = 0usize;
        let mut working = 0usize;
        replay(&records, 0, i64::MAX, |s, now| {
            if !(waiting + SETTLES_WITHIN_MS..end).contains(&now) {
                return;
            }
            samples += 1;
            if matches!(s.status, SessionStatus::Working { .. }) {
                working += 1;
            }
        });
        assert!(samples > 10, "the window has to be worth measuring: {samples}");
        let share = working as f32 / samples as f32;
        assert!(
            share <= RESIZE_LOOP_WORKING_BUDGET,
            "the /help page read 작업 중 for {:.0}% of {samples} samples",
            share * 100.0
        );
    }

    /// (§36-c) The reported symptom, counted: the pill stops changing its mind.
    ///
    /// "작업 중 · 1초 전, refreshed every second" is a *transition count*, and
    /// that is what this measures. The recording carries 62 resizes across 48
    /// seconds; before the fix each one bought another status change.
    #[test]
    fn the_recorded_resize_loop_stops_flipping_the_pill() {
        let records = capture("claude-resize-loop.cap");
        let mut flips = 0usize;
        let mut last: Option<&'static str> = None;
        replay(&records, 0, i64::MAX, |s, _| {
            let kind = s.status.kind();
            if last.is_some_and(|previous| previous != kind) {
                flips += 1;
            }
            last = Some(kind);
        });
        assert!(
            flips <= 8,
            "the pill changed its mind {flips} times in 48 seconds of a person \
             reading two menus"
        );
    }

    /// (§36-d) The guard that keeps the fix from swallowing real work.
    ///
    /// A resize buys [`RESIZE_REPAINT_MS`] of "that repaint was ours" and not
    /// one millisecond more: output that arrives after the window is the agent
    /// again, whatever the app did to the window.
    #[test]
    fn a_resize_excuses_the_repaint_it_caused_and_nothing_after_it() {
        let sigs = signatures();
        let mut s = state("claude", 0);
        idle_group(&mut s, 0);
        // A settled alternate screen: the case the loop was breaking.
        let frame = concat!(
            "\x1b[?1049h\x1b[H\x1b[2J",
            "  Select model\n",
            "  ❯ 1. Default\n",
            "  Esc to cancel\n",
        );
        s.observe_output(frame.as_bytes(), 100);
        for at in (200..=2_000).step_by(200) {
            advance(&mut s, at, &sigs);
        }
        assert!(
            matches!(s.status, SessionStatus::WaitingForInput { .. }),
            "precondition: a still menu is waiting, got {:?}",
            s.status
        );

        // The app takes two rows away; claude clears and redraws the same menu
        // one row shorter. Far enough past the last sample that the digest is
        // due, or the throttle would carry the test rather than the rule.
        s.resized_at = 2_300;
        s.observe_output(
            concat!("\x1b[2J\x1b[H", "  Select model\n", "  ❯ 1. Default\n").as_bytes(),
            2_310,
        );
        advance(&mut s, 2_310, &sigs);
        assert!(
            matches!(s.status, SessionStatus::WaitingForInput { .. }),
            "a repaint the app asked for is not the agent working, got {:?}",
            s.status
        );

        // …and the same bytes past the grace window are.
        s.observe_output(b"The answer is forty-two.\n", 2_900);
        advance(&mut s, 2_900, &sigs);
        assert!(
            matches!(s.status, SessionStatus::Working { .. }),
            "output past the grace window is the agent again, got {:?}",
            s.status
        );
    }

    /// Render the escape sequences in a run of pty bytes, in order.
    ///
    /// Printable runs collapse to `"…"×N`; everything else is named the way the
    /// specs name it, because the point of PINNED §36's dump is to see *which*
    /// sequence keeps arriving.
    fn escapes(bytes: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        let mut i = 0usize;
        let mut text = 0usize;
        let flush = |out: &mut Vec<String>, text: &mut usize| {
            if *text > 0 {
                out.push(format!("txt×{text}"));
                *text = 0;
            }
        };
        while i < bytes.len() {
            let b = bytes[i];
            if b == 0x1b && i + 1 < bytes.len() {
                flush(&mut out, &mut text);
                let second = bytes[i + 1];
                match second {
                    b'[' => {
                        let start = i + 2;
                        let mut j = start;
                        while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                            j += 1;
                        }
                        let body = String::from_utf8_lossy(&bytes[start..j.min(bytes.len())]);
                        let fin = bytes.get(j).map(|c| *c as char).unwrap_or('?');
                        out.push(format!("CSI {body}{fin}"));
                        i = j + 1;
                        continue;
                    }
                    b']' => {
                        let start = i + 2;
                        let mut j = start;
                        while j < bytes.len() && bytes[j] != 0x07 && bytes[j] != 0x1b {
                            j += 1;
                        }
                        let body = String::from_utf8_lossy(&bytes[start..j.min(bytes.len())]);
                        let head: String = body.chars().take(24).collect();
                        out.push(format!("OSC {head}"));
                        i = if bytes.get(j) == Some(&0x1b) { j + 2 } else { j + 1 };
                        continue;
                    }
                    b'P' | b'X' | b'^' | b'_' => {
                        let start = i + 2;
                        let mut j = start;
                        while j + 1 < bytes.len() && !(bytes[j] == 0x1b && bytes[j + 1] == b'\\') {
                            j += 1;
                        }
                        let body = String::from_utf8_lossy(&bytes[start..j.min(bytes.len())]);
                        let head: String = body.chars().take(24).collect();
                        out.push(format!("{} {head}", second as char));
                        i = j + 2;
                        continue;
                    }
                    _ => {
                        out.push(format!("ESC {}", second as char));
                        i += 2;
                        continue;
                    }
                }
            }
            if b == b'\r' || b == b'\n' || b == 0x07 {
                flush(&mut out, &mut text);
                out.push(
                    match b {
                        b'\r' => "CR",
                        b'\n' => "LF",
                        _ => "BEL",
                    }
                    .to_string(),
                );
                i += 1;
                continue;
            }
            text += 1;
            i += 1;
        }
        flush(&mut out, &mut text);
        out
    }

    /// A wall-clock ceiling for a test, enforced from a watchdog thread that
    /// kills the **process** when it expires.
    ///
    /// These diagnostics are `#[ignore]`d, which means they are run by hand —
    /// increasingly, by an agent that starts `cargo test … --nocapture`, pipes
    /// the output somewhere and waits for the exit code. Two of those runs sat
    /// for two and a half hours before anyone noticed, and the agent that
    /// started them reported the session clean.
    ///
    /// Both ways these tests hang are invisible from inside the test thread:
    ///
    ///   1. `--nocapture` writes straight to the inherited stdout. If the
    ///      reader on the other end of that pipe has gone away or stopped
    ///      draining — which is what an orphaned harness looks like — the very
    ///      next `println!` blocks in `write(2)` and never returns.
    ///   2. `replay` walks `now` forward by `CONTENT_SAMPLE_MS / 3` per
    ///      iteration. Any future edit that lets that step reach zero turns the
    ///      loop into a spin with no output at all.
    ///
    /// Neither can be rescued by anything running on the test thread — it is
    /// blocked or spinning — and a panic raised on a watchdog thread only fails
    /// *that* thread. So the watchdog calls [`std::process::exit`] with cargo's
    /// own test-failure code: the run ends, loudly, with a message naming the
    /// test and the limit, instead of holding a pty open until someone looks.
    ///
    /// The guard disarms on drop, so a test that finishes normally never trips
    /// it and never blocks the harness from exiting.
    #[must_use = "the deadline disarms as soon as the guard is dropped"]
    fn deadline(name: &'static str, limit: Duration) -> DeadlineGuard {
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&done);
        std::thread::spawn(move || {
            let started = std::time::Instant::now();
            while started.elapsed() < limit {
                if flag.load(Ordering::SeqCst) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            if flag.load(Ordering::SeqCst) {
                return;
            }
            eprintln!(
                "\n\n*** {name}: still running after {}s — this test has a deadline \
                 because it has hung before (blocked stdout, or a replay loop that \
                 stopped advancing). Killing the process so the run fails instead of \
                 waiting.\n\n",
                limit.as_secs()
            );
            // 101 is what a failing `cargo test` exits with.
            std::process::exit(101);
        });
        DeadlineGuard { done }
    }

    struct DeadlineGuard {
        done: Arc<std::sync::atomic::AtomicBool>,
    }

    impl Drop for DeadlineGuard {
        fn drop(&mut self) {
            self.done.store(true, Ordering::SeqCst);
        }
    }

    /// How long either replay diagnostic may take. Both are pure CPU over a
    /// fixture of a few thousand records and finish in well under a second on
    /// this machine; two minutes is "something is wrong", not "slow laptop".
    const DIAGNOSTIC_DEADLINE: Duration = Duration::from_secs(120);

    /// The dialogue: what the child wrote, what the frontend wrote back, and
    /// how many bytes each 500 ms window carried (PINNED §36).
    ///
    /// Deadlined — see [`deadline`] for why an `#[ignore]`d printer needs one.
    ///
    /// ```text
    /// GILDE_CAP=<dir>/<session>.cap \
    ///   cargo test -p gilde-code --lib -- --ignored --nocapture replay_dialogue
    /// ```
    #[test]
    #[ignore = "diagnostic: prints the pty dialogue of a GCAP1 capture"]
    fn replay_dialogue() {
        use std::collections::BTreeMap;

        let _deadline = deadline("replay_dialogue", DIAGNOSTIC_DEADLINE);
        let records = capture("claude-model-help.cap");
        let end = records.last().map(|r| r.at_ms).unwrap_or(0);

        println!("== per 500 ms window ==");
        let mut window = 0i64;
        while window <= end {
            let mut out_bytes = 0usize;
            let mut in_bytes = 0usize;
            let mut out_seq: Vec<String> = Vec::new();
            let mut in_seq: Vec<String> = Vec::new();
            let mut marks: Vec<String> = Vec::new();
            for r in records
                .iter()
                .filter(|r| r.at_ms >= window && r.at_ms < window + 500)
            {
                if r.is_output() {
                    out_bytes += r.payload.len();
                    out_seq.extend(escapes(&r.payload));
                } else if r.is_input() {
                    in_bytes += r.payload.len();
                    in_seq.extend(escapes(&r.payload));
                } else {
                    marks.push(String::from_utf8_lossy(&r.payload).into_owned());
                }
            }
            if out_bytes > 0 || in_bytes > 0 || !marks.is_empty() {
                println!(
                    "{window:7} out={out_bytes:6}B in={in_bytes:4}B  {}",
                    marks.join(" ")
                );
                if !out_seq.is_empty() {
                    println!("        <- {}", head(&out_seq, 24));
                }
                if !in_seq.is_empty() {
                    println!("        -> {}", head(&in_seq, 24));
                }
            }
            window += 500;
        }

        println!("\n== sequence census ==");
        let mut census: BTreeMap<(char, String), usize> = BTreeMap::new();
        for r in &records {
            let dir = if r.is_output() {
                '<'
            } else if r.is_input() {
                '>'
            } else {
                continue;
            };
            for seq in escapes(&r.payload) {
                *census.entry((dir, seq)).or_default() += 1;
            }
        }
        let mut rows: Vec<_> = census.into_iter().collect();
        rows.sort_by(|a, b| b.1.cmp(&a.1));
        for ((dir, seq), n) in rows.into_iter().take(60) {
            println!("{dir} {n:6}  {seq}");
        }
    }

    fn head(items: &[String], n: usize) -> String {
        let mut shown: Vec<&str> = items.iter().take(n).map(String::as_str).collect();
        if items.len() > n {
            shown.push("…");
        }
        shown.join(" ")
    }

    /// Print, per evaluation, what the grid and the two clocks were doing.
    /// Not an assertion — the notebook that produced PINNED §35.
    ///
    /// Deadlined at [`DIAGNOSTIC_DEADLINE`]. This is the test that hung twice:
    /// it prints one line per evaluation of a multi-thousand-record capture,
    /// which is exactly the shape that stalls forever on a stdout nobody is
    /// draining. See [`deadline`].
    ///
    /// ```text
    /// cargo test -p gilde-code --lib -- --ignored --nocapture replay_diagnostics
    /// ```
    #[test]
    #[ignore = "diagnostic: prints the per-frame analysis behind PINNED §35"]
    fn replay_diagnostics() {
        let _deadline = deadline("replay_diagnostics", DIAGNOSTIC_DEADLINE);
        let records = capture("claude-model-help.cap");
        for r in records.iter().filter(|r| r.is_marker()) {
            println!("{:7} MARK {}", r.at_ms, String::from_utf8_lossy(&r.payload));
        }
        /// Cells and rows that differ, counted the way `changed_cells` counts.
        fn diff(before: &[String], after: &[String]) -> (usize, Vec<usize>) {
            let mut cells = 0usize;
            let mut rows = Vec::new();
            for index in 0..before.len().max(after.len()) {
                let a = before.get(index).map(String::as_str).unwrap_or("");
                let b = after.get(index).map(String::as_str).unwrap_or("");
                if a == b {
                    continue;
                }
                rows.push(index);
                let mut ac = a.chars();
                let mut bc = b.chars();
                loop {
                    match (ac.next(), bc.next()) {
                        (None, None) => break,
                        (x, y) => {
                            if x != y {
                                cells += 1;
                            }
                        }
                    }
                }
            }
            (cells, rows)
        }

        let mut previous: Vec<String> = Vec::new();
        let mut anchor: Vec<String> = Vec::new();
        let mut last_cursor = (0usize, 0usize);
        let (waiting, status) = replay(&records, 0, i64::MAX, |s, now| {
            let rows = s.tail.screen_lines();
            let cursor = s.tail.cursor();
            let (prev_cells, prev_rows) = diff(&previous, &rows);
            let (anchor_cells, anchor_rows) = diff(&anchor, &rows);
            if anchor_cells > STABLE_CELL_TOLERANCE
                || (cursor != last_cursor && anchor_cells + 1 > STABLE_CELL_TOLERANCE)
            {
                anchor = rows.clone();
                last_cursor = cursor;
            }
            println!(
                "{now:7} quiet={:6} stable={:6} rows={:3} \
                 vs_prev={prev_cells:4}c/{:2}r vs_anchor={anchor_cells:4}c/{:2}r \
                 rows_changed={:?} cursor={cursor:?} alt={} {:?} {:.2}",
                s.quiet_ms(now),
                s.content_stable_ms(now),
                rows.len(),
                prev_rows.len(),
                anchor_rows.len(),
                &anchor_rows[..anchor_rows.len().min(8)],
                s.tail.alt_screen(),
                s.status.kind(),
                s.confidence,
            );
            previous = rows;
        });
        println!("first WaitingForInput at {waiting:?}, final {status:?}");
    }
}
