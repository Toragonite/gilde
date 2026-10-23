//! The numbers behind a status, and the rule that a status never ships without
//! them (DESIGN §6-4, M1-PLAN T16.2 / T16.4).
//!
//! Everything the engine decides is expressed as a [`Verdict`]: a status, a
//! confidence, and the observations that produced it. The engine itself holds no
//! magic numbers — they are all here, named, so that "why is this session Idle
//! at 0.4?" is answered by reading one file.
//!
//! ## Why confidence is published at all
//!
//! DESIGN §0's rule is that the app does not pretend to know things. Three of
//! the six ways to reach [`SessionStatus::Idle`] are strong (a matched prompt
//! pattern, a reaped child) and one is weak (nothing has happened for a while
//! and nothing matched). Reporting both as "idle" with a solid green pill would
//! be a lie of omission, so the weak one is reported at [`CONF_FALLBACK_IDLE`],
//! below [`LOW_CONFIDENCE`], and the UI draws it as a hollow dashed pill.

use crate::agents::signatures::{PromptCardPatterns, ReadySignature, TrustDialogPatterns};
use crate::contract::activity::{EvidenceLine, ScreenOptionFact, ScreenPromptFacts};
use crate::contract::common::{Confidence, EpochMs};
use crate::contract::display::TextCap;
use crate::contract::ids::RequestId;
use crate::contract::interaction::{InteractionKind, InteractionOption, OptionRole, PendingInteraction};
use crate::model::status::{SessionStatus, StatusEvidence};

// ---------------------------------------------------------------------------
// the confidence table — DESIGN §6-4, verbatim
// ---------------------------------------------------------------------------

/// The child was reaped, or we killed it. Nothing is more certain than this.
pub const CONF_EXITED: f32 = 1.00;
/// Bytes arrived in the last [`OUTPUT_BURST_MS`].
pub const CONF_OUTPUT_BURST: f32 = 0.90;
/// A `waiting` pattern matched the visible tail.
pub const CONF_WAITING_PATTERN: f32 = 0.85;
/// An `idle` pattern matched the visible tail.
pub const CONF_IDLE_PATTERN: f32 = 0.85;
/// The pty's foreground process group is burning CPU.
pub const CONF_FOREGROUND_CPU: f32 = 0.75;
/// A quiet alternate screen with no CPU behind it (BACKLOG M1.5 15b).
///
/// Three weak observations agreeing — the child took over the screen, it has
/// stopped writing to it, and nothing in its foreground group is computing —
/// which is more than the shrug and less than a vendor's own words, so it sits
/// between [`CONF_STALLED`] and [`CONF_WORKING_PATTERN`]. Above
/// [`LOW_CONFIDENCE`]: the pill is drawn solid, because "it is waiting for you"
/// is the actionable half and the popover carries all three reasons.
pub const CONF_ALT_SCREEN_QUIET: f32 = 0.65;
/// A `working` pattern matched the visible tail.
pub const CONF_WORKING_PATTERN: f32 = 0.70;
/// Quiet for a very long time, no CPU, nothing matched.
pub const CONF_STALLED: f32 = 0.60;
/// Quiet, no CPU, nothing matched, not long enough to call it stalled. This is
/// the honest shrug, and it is deliberately below [`LOW_CONFIDENCE`].
pub const CONF_FALLBACK_IDLE: f32 = 0.40;
/// A session that has not printed anything yet.
pub const CONF_STARTING: f32 = 0.40;

/// Below this the UI draws a hollow, dashed pill (DESIGN §6-4).
pub const LOW_CONFIDENCE: f32 = 0.5;

/// `session_send_prompt`'s gate (DESIGN §6-5): `Idle` alone is not enough.
pub const PROMPT_GATE_CONFIDENCE: f32 = 0.6;

// ---------------------------------------------------------------------------
// the timing table
// ---------------------------------------------------------------------------

/// "Output in the last 500 ms" — the reactivity path.
pub const OUTPUT_BURST_MS: i64 = 500;

/// The window the output rate is averaged over.
pub const RATE_WINDOW_MS: i64 = 500;

/// `Starting` holds for this long, or until the first byte, whichever comes
/// first (DESIGN §6-4's `Starting ──첫 출력 또는 1500ms──► evaluate()`).
pub const STARTING_GRACE_MS: i64 = 1_500;

/// At most one status change per this interval — except `Working` and the
/// terminal statuses, which are immediate (DESIGN §6-4).
pub const DEBOUNCE_MS: i64 = 300;

/// The engine re-evaluates at least this often even with no output.
pub const TICK_MS: u64 = 500;

/// Foreground-group CPU at or above this means the session is computing, no
/// matter how quiet it is (M1-PLAN T16.3, pinned by a test).
pub const CPU_BUSY_PCT: f32 = 3.0;

/// What it takes to keep calling a screen that has **stopped changing**
/// `Working` on the strength of CPU alone (PINNED §35).
///
/// `ps -o %cpu` on macOS is a *decaying average over the process's recent
/// history*, not an instantaneous rate. A TUI that spent 30 ms at 70 % redrawing
/// a full-screen menu still reads 3–4 % a second after it stopped, and
/// [`CPU_BUSY_PCT`] is below that floor — so the CPU rule, which outranks every
/// screen-based rule, held claude's `/model` on `작업 중` for 2.3 s after each
/// repaint, and a person arrowing through the menu never saw anything else.
///
/// So the reading has to mean more when the screen argues with it. Measured on
/// this machine (claude 2.1.266, `tests/fixtures/streams/claude-model-help.cap`
/// and the app's own `proc_sample` rows): every sample taken while the session
/// sat on a finished full-screen view read **≤ 11.5 %**, and every sample taken
/// while it was actually computing read **≥ 18 %**. Fifteen is the middle of
/// that gap. Above it, a still screen is still `Working` — which is what keeps a
/// long `Bash` tool call, quiet and busy behind a frozen frame, out of
/// `WaitingForInput`.
pub const CPU_STILL_SCREEN_PCT: f32 = 15.0;

/// How long an alternate screen has to stay quiet before it is read as a view
/// the user is looking at rather than one the agent is still drawing
/// (BACKLOG M1.5 15b).
///
/// Longer than [`OUTPUT_BURST_MS`] and than the default `idleAfterMs` (1200),
/// so a full-screen TUI that is still painting itself — a picker filtering as
/// the user types, a diff being redrawn — is never called "waiting" mid-frame.
pub const ALT_SCREEN_QUIET_MS: u64 = 1_500;

/// How many consecutive samples below [`CPU_BUSY_PCT`] it takes to believe the
/// group really stopped computing (DESIGN §6-4: "2회 연속 샘플").
pub const CPU_QUIET_SAMPLES: u32 = 2;

/// How often the screen digest is recomputed, at most (DESIGN §6-4).
///
/// [`ScreenContent::observe`] walks the whole retained grid, and `on_output`
/// runs on the reader thread — the one thread that must never make the child
/// wait. A repainting TUI can call it twenty times a second; this throttle
/// makes the cost a function of the wall clock instead of the byte rate, and
/// still gives [`ALT_SCREEN_QUIET_MS`] ten samples to decide with.
pub const CONTENT_SAMPLE_MS: i64 = 150;

/// How many differing cells a screen may have and still count as unchanged.
///
/// A blinking cursor cell, a braille spinner (`⠋⠙⠹`), a seconds counter that
/// has not rolled over yet: none of them mean the child is doing anything, and
/// all of them would otherwise reset the stability clock on every frame. Two is
/// the smallest number that covers a spinner glyph next to its own trailing
/// space, and it is exactly what a menu caret costs when it moves one row (the
/// cell it left plus the cell it arrived on) — see [`ScreenContent::observe`]
/// for why the cursor is one of those two rather than a third.
pub const STABLE_CELL_TOLERANCE: usize = 2;

/// How long after a `TIOCSWINSZ` the repaint that follows it is **ours**
/// (PINNED §36).
///
/// A resize makes the kernel deliver `SIGWINCH`, and every full-screen TUI
/// answers it by clearing the screen and drawing the whole thing again. Those
/// bytes are not news about the agent: the same screen came back, at a new
/// size, because the window changed — and the person who changed it was looking
/// at it.
///
/// Measured over the 1061 resizes in `tests/fixtures/streams/claude-resize-loop.cap`
/// (claude 2.1.266, the app's own reader thread): the first byte of the repaint
/// arrives in **0 ms at the median and 3 ms at p99**, and the last byte lands at
/// **4 ms p90, 292 ms p99**. One [`OUTPUT_BURST_MS`] covers all of it with room
/// to spare, and it is the window the reactivity path already thinks in.
pub const RESIZE_REPAINT_MS: i64 = OUTPUT_BURST_MS;

/// Fallback for `config.status.stallAfterMs`.
pub const DEFAULT_STALL_AFTER_MS: u64 = 120_000;

// ---------------------------------------------------------------------------
// Gilde 2 S5 rules (contract §6.2.2, §6.2.4, §6.2.6, §11.2)
// ---------------------------------------------------------------------------

/// A parsed screen prompt or setup menu on a settled grid.
pub const CONF_SCREEN_PROMPT: f32 = 0.85;
/// The vendor's ready signature on a settled grid (§6.2.6).
pub const CONF_READY_SIGNATURE: f32 = 0.90;
/// The settled-grid fallback for a vendor without a ready signature (N-03):
/// `Likely`, and exactly what §6.2.2 readiness accepts in that case.
pub const CONF_SETTLED_FALLBACK: f32 = 0.50;
/// Readiness threshold with a verified ready signature (§6.2.2).
pub const READY_CONFIDENCE: f32 = 0.6;
/// Readiness threshold without one (§6.2.2, N-03).
pub const READY_CONFIDENCE_FALLBACK: f32 = 0.5;
/// The prompt scanner runs only on a grid settled this long (§6.2.6).
pub const PROMPT_SETTLE_MS: u64 = 300;
/// A card disappears once the grid has not matched it for this long (§6.2.4).
pub const PROMPT_GONE_MS: i64 = 300;
/// A ready signature counts on a grid settled this long.
pub const READY_SETTLE_MS: u64 = 500;
/// The settled-grid fallback needs this long (§6.2.6).
pub const FALLBACK_SETTLE_MS: u64 = 2_000;
/// PTYs with output in this window, or a pending prompt, tick at [`TICK_MS`];
/// everything else at [`QUIET_TICK_MS`] (§6.2.7, §11.2 rule 1).
pub const HOT_OUTPUT_MS: i64 = 10_000;
pub const QUIET_TICK_MS: u64 = 5_000;
/// The prompt grammar scans the bottom 30 rows (§6.2.4).
pub const SCAN_ROWS: usize = 30;
/// A run within the bottom 15 rows can be `High` (§6.2.4).
pub const NEAR_BOTTOM_ROWS: usize = 15;
/// At most two non-option lines between consecutive options (§6.2.4).
pub const MAX_GAP_LINES: usize = 2;
/// The question is within 4 rows above option 1 (§6.2.4).
pub const QUESTION_LOOKBACK: usize = 4;
/// The prompt header reaches up to the nearest box border or rule within this
/// many rows above the question; without one, it is these rows (A34.1).
pub const HEADER_LOOKBACK: usize = 24;
/// The footer is within 3 rows below the run.
pub const FOOTER_LOOKAHEAD: usize = 3;
/// More non-empty rows than this below a run (after its footer) means the run
/// is text on a screen that moved on, not a prompt (false-positive guard).
pub const MAX_TRAILING_ROWS: usize = 4;
/// A setup-menu item longer than this is prose, not a menu item.
pub const MAX_MENU_ITEM_CHARS: usize = 100;
/// Card text caps (§3.4.5).
pub const CARD_TITLE_CHARS: usize = 200;
pub const CARD_LABEL_CHARS: usize = 120;

// ---------------------------------------------------------------------------
// the foreground sample
// ---------------------------------------------------------------------------

/// One reading of the pty's foreground process group (DESIGN §6-4 signal 2).
///
/// The pgid comes from `MasterPty::process_group_leader()` — i.e. `tcgetpgrp`
/// on the master — and the numbers come from T07's `ps` snapshot, summed over
/// every pid in that group. It is *not* the session's own child: when the agent
/// runs `less`, the foreground group is `less`, and that is the group whose CPU
/// tells us whether anything is actually happening.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ForegroundSample {
    pub pgid: i32,
    pub cpu_pct: f32,
    pub rss_bytes: u64,
    /// Unix millis of the `ps` snapshot this came from.
    pub at: i64,
}

impl ForegroundSample {
    pub fn busy(&self) -> bool {
        self.cpu_pct >= CPU_BUSY_PCT
    }

    pub fn evidence(&self) -> StatusEvidence {
        StatusEvidence::ForegroundGroup {
            pgid: self.pgid,
            cpu_pct: self.cpu_pct,
            rss_bytes: self.rss_bytes,
        }
    }
}

/// The two-consecutive-samples rule of DESIGN §6-4, as a latch.
///
/// Set by any sample at or above [`CPU_BUSY_PCT`], cleared only after
/// [`CPU_QUIET_SAMPLES`] consecutive samples below it. Absence of samples
/// neither sets nor clears it: a machine where `ps` is failing must not have
/// every session pinned to `Working`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CpuLatch {
    busy: bool,
    quiet_samples: u32,
}

impl CpuLatch {
    pub fn observe(&mut self, cpu_pct: f32) {
        if cpu_pct >= CPU_BUSY_PCT {
            self.busy = true;
            self.quiet_samples = 0;
        } else {
            self.quiet_samples = self.quiet_samples.saturating_add(1);
            if self.quiet_samples >= CPU_QUIET_SAMPLES {
                self.busy = false;
            }
        }
    }

    pub fn busy(&self) -> bool {
        self.busy
    }
}

// ---------------------------------------------------------------------------
// the fourth signal: does the screen still say the same thing?
// ---------------------------------------------------------------------------

/// The screen the child last settled on, and when it settled there
/// (DESIGN §6-4, BACKLOG 24).
///
/// ## Why the byte counter was not enough
///
/// Claude Code repaints `/model` and `/help` continuously — a cursor-home, a
/// full redraw, ~2 kB/s, forever, with not one cell different between frames.
/// Every byte-rate test therefore says "working" at a screen whose only
/// remaining event is the user pressing a key, and both the alternate-screen
/// rule and the waiting-pattern rule sat behind such a test. So the engine keeps
/// a second, orthogonal clock: not *is it quiet* but *is it still saying the
/// same thing*.
///
/// ## The anchor, and why the comparison is not frame-to-frame
///
/// The kept copy is the screen as of the last time it was judged to have
/// **changed**, not the previous sample. Comparing to the previous frame would
/// let a slow crawl — two cells per sample, a whole new screen a second later —
/// stay "stable" forever, because every step of it is small. Against a fixed
/// anchor the differences accumulate and the crawl is caught on the frame that
/// crosses [`STABLE_CELL_TOLERANCE`], while a spinner cell cycling `⠋⠙⠹` keeps
/// differing from the anchor in exactly one cell and stays stable, which is the
/// whole point.
///
/// Colours and attributes are not in the comparison — the vte grid in
/// [`crate::pty::ansi`] keeps chars, and a re-render that only changed a
/// highlight is not news about the child.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScreenContent {
    /// The rows as of `changed_at`, trailing blanks trimmed.
    anchor: Vec<String>,
    /// The cursor as of `changed_at`.
    cursor: (usize, usize),
    /// Unix millis at which the screen last became something else.
    changed_at: i64,
    /// Unix millis of the last sample; the [`CONTENT_SAMPLE_MS`] throttle.
    sampled_at: i64,
    /// What the last sample measured, for `GILDE_LOG=status=debug` (PINNED §36).
    /// Counted, not judged — the rule reads [`ScreenContent::stable_ms`].
    last: Diff,
}

/// How far apart two screens were, the last time they were compared.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Diff {
    /// Cells that differ from the anchor. Capped at the give-up limit unless
    /// the caller asked for an exact count.
    pub cells: usize,
    /// Rows that differ from the anchor.
    pub rows: usize,
    /// The cursor is not where it was at the anchor.
    pub cursor_moved: bool,
}

impl ScreenContent {
    pub fn new(now: i64) -> ScreenContent {
        ScreenContent {
            anchor: Vec::new(),
            cursor: (0, 0),
            changed_at: now,
            // The first evaluation always samples, whatever `now` happens to be.
            sampled_at: now - CONTENT_SAMPLE_MS,
            last: Diff::default(),
        }
    }

    /// True when a fresh sample is worth the walk over the grid.
    pub fn due(&self, now: i64) -> bool {
        now - self.sampled_at >= CONTENT_SAMPLE_MS
    }

    /// Compare one screen against the anchor, and move the anchor if it really
    /// is a different screen.
    ///
    /// A cursor move counts as **one** changed cell — and as *one of* the
    /// changed cells, not as an extra one on top of them. The cursor is a cell
    /// on the screen like any other, so adding it to the content count double
    /// counts, and the double count had a consequence (PINNED §35): claude's
    /// `/model` caret costs exactly two cells when it moves a row — the cell it
    /// left and the cell it arrived on, measured — and the cursor moves with
    /// it, so `2 + 1 = 3` crossed [`STABLE_CELL_TOLERANCE`] and every arrow key
    /// restarted the stability clock. §34 said a caret move was absorbed; it
    /// never was. `max` is what makes that sentence true.
    ///
    /// The other direction still holds: a caret move plus a *rewritten* row is
    /// dozens of cells, and dozens is not two.
    pub fn observe(&mut self, rows: Vec<String>, cursor: (usize, usize), now: i64) {
        self.observe_to(rows, cursor, now, STABLE_CELL_TOLERANCE);
    }

    /// [`ScreenContent::observe`], with the give-up limit of the cell count in
    /// the caller's hands.
    ///
    /// The rule needs only "more than [`STABLE_CELL_TOLERANCE`]", so the walk
    /// stops as soon as it knows that. `GILDE_LOG=status=debug` wants the real
    /// number, and passing [`usize::MAX`] buys it for the price of one more
    /// walk over a screen — which is a price only paid while somebody is
    /// reading the log (PINNED §36).
    pub fn observe_to(
        &mut self,
        rows: Vec<String>,
        cursor: (usize, usize),
        now: i64,
        limit: usize,
    ) {
        self.sampled_at = now;
        let mut diff = diff_cells(&self.anchor, &rows, limit);
        diff.cursor_moved = cursor != self.cursor;
        self.last = diff;
        let changed = diff.cells.max(usize::from(diff.cursor_moved));
        if changed > STABLE_CELL_TOLERANCE {
            self.anchor = rows;
            self.cursor = cursor;
            self.changed_at = now;
        }
    }

    /// Move the anchor to this screen **without** restarting the clock
    /// (PINNED §36).
    ///
    /// For the one case where the screen is different and yet nothing happened:
    /// the app resized the pty, the child redrew everything at the new size,
    /// and every cell moved. Against the old anchor that is a thousand changed
    /// cells; to the person watching it is the same screen, one row shorter.
    /// Re-anchoring keeps the comparison honest for whatever comes next while
    /// leaving `changed_at` where it was, because nothing new was said.
    pub fn reanchor(&mut self, rows: Vec<String>, cursor: (usize, usize), now: i64) {
        self.sampled_at = now;
        let mut diff = diff_cells(&self.anchor, &rows, usize::MAX);
        diff.cursor_moved = cursor != self.cursor;
        self.last = diff;
        self.anchor = rows;
        self.cursor = cursor;
    }

    /// What the last comparison measured (PINNED §36's debug line).
    pub fn last_diff(&self) -> Diff {
        self.last
    }

    /// How long the screen has been saying the same thing.
    pub fn stable_ms(&self, now: i64) -> u64 {
        (now - self.changed_at).max(0) as u64
    }
}

/// How many cells and rows differ between two screens, giving up once the cell
/// count is past `limit`.
///
/// Cells are compared by position, so an inserted character shifts its row and
/// counts as a change for the whole tail of that row — which is right: a line
/// that grew a word is a line the user can read differently.
fn diff_cells(before: &[String], after: &[String], limit: usize) -> Diff {
    let mut diff = Diff::default();
    let rows = before.len().max(after.len());
    for index in 0..rows {
        let a = before.get(index).map(String::as_str).unwrap_or("");
        let b = after.get(index).map(String::as_str).unwrap_or("");
        if a == b {
            continue;
        }
        diff.rows += 1;
        let mut ac = a.chars();
        let mut bc = b.chars();
        loop {
            match (ac.next(), bc.next()) {
                (None, None) => break,
                (x, y) => {
                    if x != y {
                        diff.cells += 1;
                        if diff.cells > limit {
                            return diff;
                        }
                    }
                }
            }
        }
    }
    diff
}

// ---------------------------------------------------------------------------
// the verdict
// ---------------------------------------------------------------------------

/// What one evaluation concluded. Never constructed without evidence.
#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub status: SessionStatus,
    pub confidence: f32,
    pub evidence: Vec<StatusEvidence>,
}

impl Verdict {
    pub fn new(status: SessionStatus, confidence: f32, evidence: Vec<StatusEvidence>) -> Verdict {
        Verdict {
            status,
            confidence,
            evidence,
        }
    }

    /// Append one more observation — alt-screen state, a pending bell, the
    /// silence that the primary signal did not need but the user will want.
    pub fn with(mut self, extra: StatusEvidence) -> Verdict {
        if !self.evidence.contains(&extra) {
            self.evidence.push(extra);
        }
        self
    }

    /// M1-PLAN T16.4: **every** `gilde://session/status` carries a non-empty
    /// evidence vector. A verdict that somehow reached here with none gets the
    /// one observation that is always true — how long it has been quiet.
    pub fn ensure_evidence(mut self, quiet_ms: u64) -> Verdict {
        if self.evidence.is_empty() {
            self.evidence.push(StatusEvidence::OutputQuiet { ms: quiet_ms });
        }
        self
    }

    pub fn is_low_confidence(&self) -> bool {
        self.confidence < LOW_CONFIDENCE
    }
}

/// True when the UI should draw the hollow, dashed pill.
pub fn is_low_confidence(confidence: f32) -> bool {
    confidence < LOW_CONFIDENCE
}

/// Two reports differ in a way the UI has to be told about.
///
/// A transition obviously counts. So does the prompt text of a
/// `WaitingForInput` (the literal-key buttons of DESIGN §6-5 are built from it),
/// the hint of a `Working`, and crossing the [`LOW_CONFIDENCE`] line in either
/// direction (the pill changes shape). A `Stalled` whose `quiet_ms` merely grew
/// does **not** count — that would be an event every tick, forever.
pub fn is_significant(
    old_status: &SessionStatus,
    old_confidence: f32,
    new_status: &SessionStatus,
    new_confidence: f32,
) -> bool {
    if old_status.kind() != new_status.kind() {
        return true;
    }
    if is_low_confidence(old_confidence) != is_low_confidence(new_confidence) {
        return true;
    }
    match (old_status, new_status) {
        (
            SessionStatus::WaitingForInput { prompt: a },
            SessionStatus::WaitingForInput { prompt: b },
        ) => a != b,
        (SessionStatus::Working { hint: a, .. }, SessionStatus::Working { hint: b, .. }) => a != b,
        (SessionStatus::Failed { reason: a }, SessionStatus::Failed { reason: b }) => a != b,
        (
            SessionStatus::Exited {
                code: ac,
                signal: asig,
            },
            SessionStatus::Exited {
                code: bc,
                signal: bsig,
            },
        ) => ac != bc || asig != bsig,
        _ => false,
    }
}

/// A status change is immediate; everything else waits out [`DEBOUNCE_MS`].
///
/// `Working` is exempt because the reactivity path exists to make the UI feel
/// instant, and `Exited` / `Killed` / `Failed` because a dead process must never
/// be drawn as alive for a third of a second.
pub fn bypasses_debounce(status: &SessionStatus) -> bool {
    matches!(status, SessionStatus::Working { .. }) || status.is_terminal()
}

/// A matched line, cut to something a tooltip can render.
///
/// The whole line is the evidence, but a TUI can draw a 400-column box and the
/// popover is 360 px wide (DESIGN §9-1).
pub const EVIDENCE_LINE_LIMIT: usize = 200;

pub fn clip_line(line: &str) -> String {
    if line.chars().count() <= EVIDENCE_LINE_LIMIT {
        return line.to_string();
    }
    let mut out: String = line.chars().take(EVIDENCE_LINE_LIMIT).collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// the screen-prompt reader (contract §6.2.4)
// ---------------------------------------------------------------------------

/// What the reader takes from the signature file for one vendor.
#[derive(Debug, Clone, Copy)]
pub struct PromptGrammar<'a> {
    pub card: &'a PromptCardPatterns,
    pub trust: Option<&'a TrustDialogPatterns>,
    pub ready: Option<&'a ReadySignature>,
}

fn indent_of(line: &str) -> usize {
    line.chars().take_while(|c| *c == ' ').count()
}

fn width_of(s: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(s)
}

/// A line the TUI wrapped at the right margin: its first word would not have
/// fit after `prev` on a `cols`-wide row (with a little slack for TUIs that
/// keep a right margin). This is what tells a wrapped label ("…/proj from" /
/// "this project") from an option's description ("Red" / "Choose red"), which
/// share their indentation [R6 §4].
fn wraps(prev: &str, line: &str, cols: usize) -> bool {
    let prev_end = width_of(prev.trim_end());
    let first = line.trim_start().split(' ').next().unwrap_or("");
    prev_end + 1 + width_of(first) > cols.saturating_sub(4)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    Label,
    Description,
    Divider,
}

#[derive(Debug, Clone)]
struct RunOption {
    digit: u8,
    /// Char column of the digit: continuation and description lines are
    /// indented deeper than this.
    digit_col: usize,
    label: String,
    description: Option<String>,
    selected: bool,
    /// The previous line of this option, for the wrap test.
    last_line: String,
    last_part: Part,
}

#[derive(Debug, Clone)]
struct Run {
    options: Vec<RunOption>,
    /// Non-option lines since the last option line.
    gap: usize,
    first_row: usize,
    end_row: usize,
}

impl Run {
    fn is_valid(&self) -> bool {
        self.options.len() >= 2
    }
}

fn append(target: &mut String, more: &str) {
    if !target.is_empty() {
        target.push(' ');
    }
    target.push_str(more);
}

/// The last valid option run (the one lowest on the screen) in `rows`.
fn last_run(lines: &[String], from: usize, cols: usize, card: &PromptCardPatterns) -> Option<Run> {
    let mut best: Option<Run> = None;
    let mut cur: Option<Run> = None;
    let finish = |run: Option<Run>, best: &mut Option<Run>| {
        if let Some(run) = run {
            if run.is_valid() {
                *best = Some(run);
            }
        }
    };

    for (row, line) in lines.iter().enumerate().skip(from) {
        if let Some(caps) = card.option.captures(line) {
            let digit = caps
                .get(1)
                .and_then(|m| m.as_str().parse::<u8>().ok())
                .unwrap_or(0);
            let label = caps.get(2).map(|m| m.as_str().trim_end()).unwrap_or("");
            let digit_col = caps
                .get(1)
                .map(|m| line[..m.start()].chars().count())
                .unwrap_or(0);
            let option = RunOption {
                digit,
                digit_col,
                label: label.to_string(),
                description: None,
                selected: card.marker.is_match(line),
                last_line: line.clone(),
                last_part: Part::Label,
            };
            let continues = cur.as_ref().is_some_and(|run| {
                run.gap <= MAX_GAP_LINES
                    && run.options.last().map(|o| o.digit + 1) == Some(digit)
                    && digit <= 9
            });
            if continues {
                let run = cur.as_mut().expect("checked above");
                run.options.push(option);
                run.gap = 0;
                run.end_row = row;
            } else {
                finish(cur.take(), &mut best);
                if digit == 1 {
                    cur = Some(Run {
                        options: vec![option],
                        gap: 0,
                        first_row: row,
                        end_row: row,
                    });
                }
            }
            continue;
        }

        let Some(run) = cur.as_mut() else { continue };
        let ends_run = line.trim().is_empty()
            || run.gap >= MAX_GAP_LINES
            || card.footer.is_match(line);
        if ends_run {
            finish(cur.take(), &mut best);
            continue;
        }
        let last = run.options.last_mut().expect("a run has an option");
        if card.divider.is_match(line) {
            run.gap += 1;
            last.last_part = Part::Divider;
            continue;
        }
        if indent_of(line) <= last.digit_col || last.last_part == Part::Divider {
            // Not under this option: the run is over.
            finish(cur.take(), &mut best);
            continue;
        }
        let text = line.trim();
        if card.continuation.is_match(line) && wraps(&last.last_line, line, cols) {
            match last.last_part {
                Part::Description => append(last.description.get_or_insert_with(String::new), text),
                _ => append(&mut last.label, text),
            }
        } else if card.description.is_match(line) {
            append(last.description.get_or_insert_with(String::new), text);
            last.last_part = Part::Description;
        } else if card.continuation.is_match(line) {
            append(&mut last.label, text);
            last.last_part = Part::Label;
        } else {
            finish(cur.take(), &mut best);
            continue;
        }
        last.last_line = line.clone();
        run.gap += 1;
        run.end_row = row;
    }
    finish(cur, &mut best);
    best
}

/// The first non-empty, non-divider row within `n` rows below `after`.
fn footer_below(
    lines: &[String],
    after: usize,
    n: usize,
    divider: &regex::Regex,
) -> Option<(usize, String)> {
    for (row, line) in lines.iter().enumerate().skip(after + 1).take(n) {
        if line.trim().is_empty() || divider.is_match(line) {
            continue;
        }
        return Some((row, line.trim().to_string()));
    }
    None
}

/// Non-empty, non-divider rows below `after` to the bottom of the screen.
fn trailing_rows(lines: &[String], after: usize, divider: &regex::Regex) -> usize {
    lines
        .iter()
        .skip(after + 1)
        .filter(|l| !l.trim().is_empty() && !divider.is_match(l))
        .count()
}

/// The question above `first_row` (§6.2.4).
struct Question {
    text: Option<String>,
    /// Where the prompt box's header ends (exclusive): the question's row, or
    /// `first_row` when no question line was found (A34.1).
    anchor: usize,
}

/// The question above `first_row` (§6.2.4). A `☐ <header>` line is never the
/// question; it is part of the box header (A34.1).
fn question_above(lines: &[String], first_row: usize, card: &PromptCardPatterns) -> Question {
    let lo = first_row.saturating_sub(QUESTION_LOOKBACK);
    let found = (lo..first_row)
        .rev()
        .find(|&row| !lines[row].trim().is_empty() && !card.divider.is_match(&lines[row]));
    match found {
        Some(row) if !card.header.is_match(&lines[row]) => Question {
            text: Some(lines[row].trim().to_string()),
            anchor: row,
        },
        _ => Question { text: None, anchor: first_row },
    }
}

/// A **solid** box edge: a `─━═` rule, a line that starts at a box corner, or
/// a divider-regex line — the top (or bottom) edge of a prompt box (A34.1,
/// A38.5). Dashed lines (`╌┄┈╍` …) are never edges: the 2.1.307 TUI draws
/// them **inside** the box around the command it asks about (LP-12 recording
/// `lp12-11-perm-prompt.txt`), so they stay in the header.
fn is_box_edge(line: &str, card: &PromptCardPatterns) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return false;
    }
    if card.divider.is_match(line) {
        return true;
    }
    let corner = |c: char| matches!(c, '╭' | '╮' | '╰' | '╯' | '┌' | '┐' | '└' | '┘' | '╔' | '╗' | '╚' | '╝');
    let flat = |c: char| matches!(c, '─' | '━' | '═');
    let rule = t.chars().filter(|c| flat(*c)).count();
    let starts_at_corner = t.chars().next().is_some_and(corner);
    rule >= 3 && (starts_at_corner || t.chars().all(|c| flat(c) || corner(c)))
}

/// One header line as it enters the digest: trailing whitespace and a
/// leading cursor marker ignored (A34.1).
fn header_line(line: &str, card: &PromptCardPatterns) -> String {
    let line = line.trim_end();
    if let Some(m) = card.marker.find(line) {
        if m.start() == 0 {
            let marked = &line[..m.end()];
            let indent = marked.len() - marked.trim_start().len();
            return format!("{}{}", &line[..indent], line[m.end()..].trim_start());
        }
    }
    line.to_string()
}

/// The prompt box's header (A34.1).
struct BoxHeader {
    /// What enters `prompt_rev`: the rows above the question up to the nearest
    /// solid box edge within [`HEADER_LOOKBACK`] rows, or — when there is none
    /// — the up-to-24 rows directly above.
    digest: String,
    /// What the card shows (A35.2): the same rows, but only when a box edge
    /// bounded them and one of them has text. Rows above the question with no
    /// box around them are not known to belong to the prompt, so they are
    /// never presented as what it approves.
    shown: Option<String>,
}

/// The prompt box's header above `anchor` (the question's row): where the
/// TUI names what the prompt approves (tool, command, path, diff summary), so
/// two prompts for different commands never share a digest (A34.1) and the
/// card can show it (A35.2).
fn header_above(lines: &[String], anchor: usize, card: &PromptCardPatterns) -> BoxHeader {
    let lo = anchor.saturating_sub(HEADER_LOOKBACK);
    let edge = (lo..anchor).rev().find(|&row| is_box_edge(&lines[row], card));
    let top = edge.map(|e| e + 1).unwrap_or(lo);
    let rows: Vec<String> = lines[top..anchor].iter().map(|l| header_line(l, card)).collect();
    let digest = rows.join("\n");
    let shown = (edge.is_some() && rows.iter().any(|r| !r.trim().is_empty())).then(|| digest.clone());
    BoxHeader { digest, shown }
}

/// `prompt_rev` (§6.2.4 as amended by A34.1): the first 16 hex of
/// sha256(header + "\n" + question + "\n" + labels), with trailing whitespace
/// and the cursor marker already gone, so a repaint — or a moved highlight —
/// keeps it stable, while a prompt that approves something else (another
/// command in the header) never shares it.
pub fn prompt_rev(header: &str, question: Option<&str>, labels: &[&str]) -> String {
    let mut text = String::from(header);
    text.push('\n');
    text.push_str(question.unwrap_or("").trim_end());
    text.push('\n');
    text.push_str(
        &labels
            .iter()
            .map(|l| l.trim_end())
            .collect::<Vec<_>>()
            .join("\n"),
    );
    crate::contract::ids::hash16(text.as_bytes())
}

/// Read the screen prompt — a numbered option run, or an unnumbered setup
/// menu — off a grid (contract §6.2.4). `lines` are the grid rows, topmost
/// first; `cols` the grid width.
///
/// Every guard here errs towards **no card**: a prompt the reader did not
/// fully recognise is never offered, so it can never be answered (PM-28):
///
/// * the vendor's ready signature on screen means its composer is up, so
///   nothing on that screen is a prompt;
/// * a numbered run needs ≥ 2 consecutive options from 1, at most two
///   continuation/description/divider lines between options, and either a
///   recognised footer or a selection marker;
/// * a run with more than [`MAX_TRAILING_ROWS`] rows of text below it is
///   history on a screen that moved on (a numbered list in prose);
/// * a setup menu needs ≥ 2 short items around one cursor line.
pub fn parse_screen_prompt(
    lines: &[String],
    cols: usize,
    grammar: PromptGrammar<'_>,
    now: EpochMs,
) -> Option<ScreenPromptFacts> {
    if lines.is_empty() {
        return None;
    }
    if grammar.ready.is_some_and(|ready| ready.matches(lines)) {
        return None;
    }
    let card = grammar.card;
    let n = lines.len();
    let from = n.saturating_sub(SCAN_ROWS);

    if let Some(run) = last_run(lines, from, cols, card) {
        return numbered_prompt(lines, &run, card, now);
    }
    setup_menu(lines, from, grammar, now)
}

fn numbered_prompt(
    lines: &[String],
    run: &Run,
    card: &PromptCardPatterns,
    now: EpochMs,
) -> Option<ScreenPromptFacts> {
    let n = lines.len();
    let footer = footer_below(lines, run.end_row, FOOTER_LOOKAHEAD, &card.divider)
        .filter(|(_, text)| card.footer.is_match(text));
    let below_from = footer.as_ref().map(|(row, _)| *row).unwrap_or(run.end_row);
    if trailing_rows(lines, below_from, &card.divider) > MAX_TRAILING_ROWS {
        return None;
    }
    let selected = run.options.iter().any(|o| o.selected);
    if footer.is_none() && !selected {
        return None;
    }
    let Question { text: question, anchor } = question_above(lines, run.first_row, card);
    let header = header_above(lines, anchor, card);
    let labels: Vec<&str> = run.options.iter().map(|o| o.label.as_str()).collect();
    let first = labels.first().map(|l| l.to_lowercase()).unwrap_or_default();
    let last = labels.last().map(|l| l.to_lowercase()).unwrap_or_default();
    let footer_text = footer.map(|(_, text)| text);
    Some(ScreenPromptFacts {
        prompt_rev: prompt_rev(&header.digest, question.as_deref(), &labels),
        question,
        header: header.shown,
        options: run
            .options
            .iter()
            .map(|o| ScreenOptionFact {
                key: o.digit.to_string(),
                label: o.label.clone(),
                description: o.description.clone(),
                selected: o.selected,
            })
            .collect(),
        allows_note: footer_text.as_deref().is_some_and(|f| card.amend.is_match(f)),
        footer: footer_text,
        permission_shaped: first.starts_with("yes") && last.starts_with("no"),
        setup: false,
        trust_dialog: false,
        near_bottom: run.end_row + NEAR_BOTTOM_ROWS >= n,
        first_seen_at: now,
    })
}

/// A numbered line (any number of digits): never a setup-menu item — setup
/// menus have no digits (§6.2.4).
fn is_numbered(line: &str) -> bool {
    let t = line.trim_start().trim_start_matches(['❯', '›', '>']).trim_start();
    let digits = t.chars().take_while(char::is_ascii_digit).count();
    digits > 0 && t[digits..].starts_with(". ")
}

fn is_menu_item(line: &str, marker_indent: usize, card: &PromptCardPatterns) -> bool {
    !line.trim().is_empty()
        && !is_numbered(line)
        && indent_of(line) > marker_indent
        && !card.divider.is_match(line)
        && !card.option.is_match(line)
        && !card.setup_marker.is_match(line)
        && !card.footer.is_match(line)
        && line.trim().chars().count() <= MAX_MENU_ITEM_CHARS
}

fn setup_menu(
    lines: &[String],
    from: usize,
    grammar: PromptGrammar<'_>,
    now: EpochMs,
) -> Option<ScreenPromptFacts> {
    let card = grammar.card;
    let n = lines.len();
    for row in (from..n).rev() {
        let line = &lines[row];
        if card.option.is_match(line) {
            continue;
        }
        if is_numbered(line) {
            continue;
        }
        let Some(caps) = card.setup_marker.captures(line) else {
            continue;
        };
        let marker_indent = caps.get(1).map(|m| m.as_str().chars().count()).unwrap_or(0);
        let label = caps.get(2).map(|m| m.as_str().trim()).unwrap_or("");
        if label.is_empty() || label.chars().count() > MAX_MENU_ITEM_CHARS {
            continue;
        }
        let mut items: Vec<(usize, String, bool)> = vec![(row, label.to_string(), true)];
        let mut top = row;
        while top > from && is_menu_item(&lines[top - 1], marker_indent, card) {
            top -= 1;
            items.insert(0, (top, lines[top].trim().to_string(), false));
        }
        let mut bottom = row;
        while bottom + 1 < n && is_menu_item(&lines[bottom + 1], marker_indent, card) {
            bottom += 1;
            items.push((bottom, lines[bottom].trim().to_string(), false));
        }
        if items.len() < 2 {
            continue;
        }
        let Question { text: question, anchor } = question_above(lines, top, card);
        let header = header_above(lines, anchor, card);
        let footer = footer_below(lines, bottom, FOOTER_LOOKAHEAD, &card.divider)
            .map(|(_, text)| text)
            .filter(|text| {
                card.footer.is_match(text)
                    || grammar
                        .trust
                        .and_then(|t| t.footer.as_ref())
                        .is_some_and(|f| f.is_match(text))
            });
        let labels: Vec<&str> = items.iter().map(|(_, l, _)| l.as_str()).collect();
        let trust_dialog = grammar.trust.is_some_and(|t| {
            labels.iter().any(|l| t.is_trust(l))
                && labels.iter().any(|l| t.is_exit(l))
                && t.footer
                    .as_ref()
                    .is_none_or(|f| footer.as_deref().is_some_and(|text| f.is_match(text)))
        });
        return Some(ScreenPromptFacts {
            prompt_rev: prompt_rev(&header.digest, question.as_deref(), &labels),
            question,
            header: header.shown,
            options: items
                .iter()
                .map(|(_, label, selected)| ScreenOptionFact {
                    key: String::new(),
                    label: label.clone(),
                    description: None,
                    selected: *selected,
                })
                .collect(),
            footer,
            permission_shaped: false,
            setup: true,
            trust_dialog,
            allows_note: false,
            near_bottom: bottom + NEAR_BOTTOM_ROWS >= n,
            first_seen_at: now,
        });
    }
    None
}

/// The highlighted item of a setup menu and the trust option's index (A6).
pub fn trust_positions(facts: &ScreenPromptFacts, trust: &TrustDialogPatterns) -> Option<(usize, usize)> {
    if !facts.setup || !facts.trust_dialog {
        return None;
    }
    let highlighted = facts.options.iter().position(|o| o.selected)?;
    let target = facts.options.iter().position(|o| trust.is_trust(&o.label))?;
    Some((highlighted, target))
}

/// The options a screen-prompt card offers and what each one means (§3.4.5,
/// §6.2.4, A25.1) — the one role mapping every card builder uses, so a card
/// never says more than the grid shows:
///
/// * **permission-shaped** prompt: option 1 `AllowOnce`, the last option
///   `Deny` (it stops the turn, F-53), the rest `Other`. Option 1 is
///   `AllowWithNote` instead when the footer offers "Tab to amend" and option
///   1 is the highlighted one: Tab amends the highlighted option, and the note
///   flow is verified on option 1 only [R6 §4 LP-12]. `AllowWithNote` is
///   still the allow-once digit; it adds "Allow with a note".
/// * **question-shaped** prompt: every option `Answer`.
/// * **setup prompt**: no options (never answered, lead ruling 5), except the
///   recognised folder-trust dialog (A6): one `TrustFolder` option whose key
///   is the trust item's 1-based menu position and whose label is its text.
///   The key is never sent — the core refuses keys on a setup prompt; the
///   option's action is `screen_prompt_key{TrustFolder}`, which finds the
///   trust item again under the write lock.
///
/// Keys are `^[1-9]$` and unique (§3.4.5); anything else is left out.
pub fn card_options(facts: &ScreenPromptFacts) -> Vec<InteractionOption> {
    let clip = |s: &str| crate::render::sanitize::sanitize_display_text(s, TextCap::Chars(CARD_LABEL_CHARS)).text;
    if facts.setup {
        return trust_option(facts)
            .map(|(position, label)| InteractionOption {
                key: position.to_string(),
                label: clip(label),
                role: OptionRole::TrustFolder,
            })
            .into_iter()
            .collect();
    }
    let note = facts.permission_shaped && facts.allows_note && facts.options.first().is_some_and(|o| o.selected);
    let last = facts.options.len().saturating_sub(1);
    let mut keys = std::collections::HashSet::new();
    facts
        .options
        .iter()
        .enumerate()
        .filter(|(_, o)| o.key != "esc" && crate::contract::interaction::is_option_key(&o.key) && keys.insert(o.key.clone()))
        .map(|(i, o)| InteractionOption {
            key: o.key.clone(),
            label: clip(&o.label),
            role: match (facts.permission_shaped, i) {
                (true, 0) if note => OptionRole::AllowWithNote,
                (true, 0) => OptionRole::AllowOnce,
                (true, i) if i == last => OptionRole::Deny,
                (true, _) => OptionRole::Other,
                (false, _) => OptionRole::Answer,
            },
        })
        .collect()
}

/// The trust item of a recognised trust dialog (A6), from the facts alone:
/// its 1-based menu position (≤ 9) and its label. `trust_dialog` was decided
/// with the vendor's live `trustDialog` patterns when the grid was read; of
/// the two families it requires ("No, exit" and "Yes, I trust this folder"
/// [R6 §4]) the trust item is the one item that starts with "Yes" — read the
/// way `permission_shaped` reads option 1. None or several such items → `None`
/// (the card keeps "Continue in the terminal", A6's fallback).
fn trust_option(facts: &ScreenPromptFacts) -> Option<(usize, &str)> {
    if !facts.setup || !facts.trust_dialog {
        return None;
    }
    let mut yes = facts
        .options
        .iter()
        .enumerate()
        .filter(|(_, o)| o.label.trim_start().to_lowercase().starts_with("yes"));
    let (index, item) = yes.next()?;
    if yes.next().is_some() || index >= 9 {
        return None;
    }
    Some((index + 1, item.label.as_str()))
}

/// A card's detail lines: ≤ 12 lines and ≤ 2 KiB of text in all (§3.4.5).
pub const CARD_DETAIL_LINES: usize = 12;
pub const CARD_DETAIL_BYTES: usize = 2 * 1024;

const ELLIPSIS: &str = "…";

/// A header row as the card shows it: its own box sides (`│ … │`) dropped;
/// `None` for a blank row or one drawn only of box-drawing characters.
fn shown_row(row: &str) -> Option<&str> {
    let sides = ['│', '┃', '║'];
    let t = row.trim().trim_start_matches(sides).trim_end_matches(sides).trim();
    let drawing = |c: char| ('\u{2500}'..='\u{257F}').contains(&c) || c.is_whitespace();
    (!t.is_empty() && !t.chars().all(drawing)).then_some(t)
}

/// What a screen card shows of the prompt box's header (A35.2), shared by
/// both card builders (this module's [`screen_card`] and World's): the rows
/// in their order as sanitized `card.detailText` lines — what the prompt
/// approves (tool, command, path, host, description) — bounded by
/// [`CARD_DETAIL_LINES`] and [`CARD_DETAIL_BYTES`] with "…" where cut. Empty
/// when no header was captured: nothing is ever invented.
pub fn card_detail_lines(facts: &ScreenPromptFacts) -> Vec<EvidenceLine> {
    let Some(header) = facts.header.as_deref() else {
        return Vec::new();
    };
    let rows: Vec<&str> = header.lines().filter_map(shown_row).collect();
    let mut out = Vec::new();
    let mut used = 0usize;
    for (i, row) in rows.iter().enumerate() {
        let more = i + 1 < rows.len();
        // Keep room for a closing "…" line while rows remain.
        let reserve = if more { ELLIPSIS.len() } else { 0 };
        let room = CARD_DETAIL_BYTES.saturating_sub(used + reserve);
        if more && (out.len() + 1 == CARD_DETAIL_LINES || room <= ELLIPSIS.len()) {
            out.push(EvidenceLine::detail_text(ELLIPSIS));
            break;
        }
        let line = crate::render::sanitize::sanitize_display_text(row, TextCap::Bytes(room));
        if line.text.trim().is_empty() {
            continue;
        }
        used += line.text.len();
        out.push(EvidenceLine::detail_text(line.text));
        if line.truncated {
            break;
        }
    }
    out
}

/// A35.2: a permission-shaped screen card that cannot show what it approves
/// (no header captured) keeps `⌘⏎` inert — `default_to_no` — so the user
/// presses the digit or opens the terminal.
pub fn approves_unseen(facts: &ScreenPromptFacts) -> bool {
    facts.permission_shaped && !facts.setup && card_detail_lines(facts).is_empty()
}

/// The card a screen prompt becomes (§3.4.5, §6.2.4): `sp_<prompt_rev>`,
/// `High` iff the engine reports `WaitingForInput` at ≥ 0.6 and the run lies
/// within the bottom 15 rows, else `Likely`; its options and their roles are
/// [`card_options`], its detail lines the prompt box's header
/// ([`card_detail_lines`], A35.2). `⌘⏎`/`⌘⌫` are live only on a
/// permission-shaped prompt that is also `High` and shows what it approves
/// (`default_to_no` otherwise).
pub fn screen_card(facts: &ScreenPromptFacts, waiting_confidence: Option<f32>) -> PendingInteraction {
    let high = waiting_confidence.is_some_and(|c| c >= PROMPT_GATE_CONFIDENCE) && facts.near_bottom;
    let clip = |s: &str, n: usize| {
        crate::render::sanitize::sanitize_display_text(s, TextCap::Chars(n)).text
    };
    let request_id = RequestId::parse(format!("sp_{}", facts.prompt_rev))
        .expect("sp_ + 16 hex is a valid request id");
    let options = card_options(facts);
    PendingInteraction {
        request_id,
        kind: if facts.setup {
            InteractionKind::SetupPrompt
        } else {
            InteractionKind::ScreenPrompt
        },
        received_at: facts.first_seen_at,
        on_behalf_of: None,
        answer_in: None,
        title: facts
            .question
            .as_deref()
            .map(|q| clip(q, CARD_TITLE_CHARS))
            .unwrap_or_default(),
        tool: None,
        detail_lines: card_detail_lines(facts),
        options,
        always_label: None,
        default_to_no: !(facts.permission_shaped && high) || approves_unseen(facts),
        degraded: false,
        prompt_rev: Some(facts.prompt_rev.clone()),
        confidence: if high {
            Confidence::High
        } else {
            Confidence::Likely
        },
        multi_select: false,
        questions: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cpu_latch_needs_two_quiet_samples_to_let_go() {
        let mut latch = CpuLatch::default();
        assert!(!latch.busy(), "no sample, no claim");

        latch.observe(42.0);
        assert!(latch.busy());
        latch.observe(0.1);
        assert!(latch.busy(), "one quiet sample is not enough");
        latch.observe(0.1);
        assert!(!latch.busy(), "two consecutive quiet samples end it");

        latch.observe(3.0);
        assert!(latch.busy(), "exactly 3.0% is busy, not idle");
    }

    fn rows(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|l| l.to_string()).collect()
    }

    #[test]
    fn an_identical_repaint_never_moves_the_stability_clock() {
        let frame = rows(&["  Select model", "  ❯ 1. Opus", "  Esc to cancel"]);
        let mut content = ScreenContent::new(0);
        content.observe(frame.clone(), (2, 16), 200);
        assert_eq!(content.stable_ms(200), 0, "the first paint is a change");

        for at in [400, 600, 800, 1_000, 1_800] {
            content.observe(frame.clone(), (2, 16), at);
        }
        assert_eq!(
            content.stable_ms(1_800),
            1_600,
            "eight identical frames are one screen, however many bytes they cost"
        );
    }

    #[test]
    fn a_spinner_cell_and_a_moving_caret_are_still_the_same_screen() {
        let mut content = ScreenContent::new(0);
        content.observe(rows(&["⠋ Working", "  Esc to cancel"]), (0, 1), 200);
        for (glyph, at) in [("⠙", 400), ("⠹", 600), ("⠸", 800)] {
            content.observe(
                rows(&[&format!("{glyph} Working"), "  Esc to cancel"]),
                // …and the caret blinks somewhere else while it spins.
                (0, 2),
                at,
            );
        }
        assert_eq!(
            content.stable_ms(800),
            600,
            "one cycling glyph plus a caret move is under the tolerance"
        );
    }

    #[test]
    fn a_menu_caret_moving_one_row_is_the_same_screen() {
        // PINNED §35, measured through `AnsiTail` on the real capture: one
        // arrow key in claude's `/model` changes **two cells in two rows** —
        // the `❯` leaving row 36 and arriving on row 37 — and moves the cursor
        // with it. §34 counted that as 2 + 1 = 3, crossed the tolerance, and
        // restarted the stability clock on every keypress; the person browsing
        // the menu saw `작업 중` the whole time they were browsing.
        let mut content = ScreenContent::new(0);
        content.observe(
            rows(&["  ❯ 1. Default", "    2. Opus", "  Esc to cancel"]),
            (0, 4),
            200,
        );
        assert_eq!(content.stable_ms(200), 0, "the first paint is a change");

        content.observe(
            rows(&["    1. Default", "  ❯ 2. Opus", "  Esc to cancel"]),
            (1, 4),
            400,
        );
        assert_eq!(
            content.stable_ms(400),
            200,
            "the hand that moved the caret is the user's; the menu is still a \
             menu waiting to be answered"
        );
    }

    #[test]
    fn a_caret_move_that_rewrites_a_row_is_a_different_screen() {
        // The guard for the one above. claude's own menu does this on two of
        // its three rows: moving the selection also rewrites the effort line
        // below it — 37 cells across 3 rows, measured. That is news.
        let mut content = ScreenContent::new(0);
        content.observe(rows(&["  ❯ 1. Default", "  ● high effort"]), (0, 4), 200);
        content.observe(rows(&["    1. Default", "  ● low effort "]), (1, 4), 400);
        assert_eq!(content.stable_ms(400), 0);
    }

    #[test]
    fn a_screen_that_crawls_two_cells_at_a_time_is_not_stable() {
        // The reason the comparison is against a fixed anchor: every step here
        // is inside the tolerance, and the screen is still a different screen.
        let mut content = ScreenContent::new(0);
        content.observe(rows(&["aaaaaaaa"]), (0, 8), 200);
        content.observe(rows(&["bbaaaaaa"]), (0, 8), 400);
        assert_eq!(content.stable_ms(400), 200, "two cells is within tolerance");
        content.observe(rows(&["bbbbaaaa"]), (0, 8), 600);
        assert_eq!(
            content.stable_ms(600),
            0,
            "four cells away from the anchor is a new screen"
        );
    }

    #[test]
    fn a_redraw_at_a_new_size_moves_the_anchor_and_leaves_the_clock_alone() {
        // PINNED §36. The app took two rows off the terminal, the child cleared
        // the screen and drew the same menu again, and every cell moved. To the
        // person reading the menu nothing happened at all.
        let mut content = ScreenContent::new(0);
        content.observe(
            rows(&["  Select model", "  ❯ 1. Default", "  Esc to cancel"]),
            (1, 4),
            200,
        );
        for at in [400, 600, 800, 1_000, 1_200] {
            content.observe(
                rows(&["  Select model", "  ❯ 1. Default", "  Esc to cancel"]),
                (1, 4),
                at,
            );
        }
        assert_eq!(content.stable_ms(1_200), 1_000);

        content.reanchor(rows(&["  Select model", "  ❯ 1. Default"]), (1, 4), 1_400);
        assert_eq!(
            content.stable_ms(1_400),
            1_200,
            "a redraw we asked for does not restart the clock"
        );
        assert!(
            content.last_diff().cells > STABLE_CELL_TOLERANCE,
            "…and the diff is still reported honestly: {:?}",
            content.last_diff()
        );

        // The new frame is the anchor now, so the next real change is measured
        // against what is on screen rather than against a screen that is gone.
        content.observe(rows(&["  Select model", "  ❯ 2. Opus   "]), (1, 4), 1_600);
        assert_eq!(content.stable_ms(1_600), 0);
    }

    #[test]
    fn a_new_line_of_streaming_text_is_a_change() {
        let mut content = ScreenContent::new(0);
        content.observe(rows(&["The answer is"]), (0, 13), 200);
        content.observe(rows(&["The answer is", "forty-two."]), (1, 10), 400);
        assert_eq!(content.stable_ms(400), 0);
    }

    #[test]
    fn the_digest_is_sampled_at_most_once_per_throttle_window() {
        let content = ScreenContent::new(1_000);
        assert!(content.due(1_000), "the first evaluation always samples");
        let mut content = content;
        content.observe(rows(&["x"]), (0, 1), 1_000);
        assert!(!content.due(1_000 + CONTENT_SAMPLE_MS - 1));
        assert!(content.due(1_000 + CONTENT_SAMPLE_MS));
    }

    #[test]
    fn a_verdict_always_carries_evidence() {
        let bare = Verdict::new(SessionStatus::Idle, CONF_FALLBACK_IDLE, Vec::new());
        assert!(bare.evidence.is_empty());
        let fixed = bare.ensure_evidence(4_200);
        assert_eq!(
            fixed.evidence,
            vec![StatusEvidence::OutputQuiet { ms: 4_200 }]
        );
    }

    #[test]
    fn with_does_not_duplicate_an_observation() {
        let verdict = Verdict::new(SessionStatus::Idle, 0.4, vec![StatusEvidence::Bell])
            .with(StatusEvidence::Bell)
            .with(StatusEvidence::AltScreen { on: true });
        assert_eq!(verdict.evidence.len(), 2);
    }

    #[test]
    fn a_growing_stall_is_not_worth_an_event_but_a_new_prompt_is() {
        assert!(!is_significant(
            &SessionStatus::Stalled { quiet_ms: 120_000 },
            CONF_STALLED,
            &SessionStatus::Stalled { quiet_ms: 121_000 },
            CONF_STALLED,
        ));
        assert!(is_significant(
            &SessionStatus::WaitingForInput {
                prompt: "[y/N]".into()
            },
            CONF_WAITING_PATTERN,
            &SessionStatus::WaitingForInput {
                prompt: "Do you want to proceed?".into()
            },
            CONF_WAITING_PATTERN,
        ));
        assert!(
            is_significant(
                &SessionStatus::Idle,
                CONF_FALLBACK_IDLE,
                &SessionStatus::Idle,
                CONF_IDLE_PATTERN,
            ),
            "the pill changes from hollow to solid; the UI has to hear about it"
        );
        assert!(!is_significant(
            &SessionStatus::Idle,
            0.85,
            &SessionStatus::Idle,
            0.90,
        ));
    }

    #[test]
    fn only_working_and_the_terminal_states_skip_the_debounce() {
        assert!(bypasses_debounce(&SessionStatus::Working {
            since: 0,
            hint: None
        }));
        assert!(bypasses_debounce(&SessionStatus::Exited {
            code: Some(0),
            signal: None
        }));
        assert!(bypasses_debounce(&SessionStatus::Killed));
        assert!(!bypasses_debounce(&SessionStatus::Idle));
        assert!(!bypasses_debounce(&SessionStatus::WaitingForInput {
            prompt: String::new()
        }));
        assert!(!bypasses_debounce(&SessionStatus::Stalled { quiet_ms: 1 }));
    }

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn the_fallback_idle_is_below_the_hollow_pill_threshold() {
        assert!(is_low_confidence(CONF_FALLBACK_IDLE));
        assert!(is_low_confidence(CONF_STARTING));
        assert!(!is_low_confidence(CONF_STALLED));
        assert!(!is_low_confidence(CONF_IDLE_PATTERN));
        assert!(
            CONF_FALLBACK_IDLE < PROMPT_GATE_CONFIDENCE,
            "a shrug must never satisfy the send-prompt gate"
        );
    }

    #[test]
    fn a_long_matched_line_is_clipped_for_the_popover() {
        let long = "x".repeat(EVIDENCE_LINE_LIMIT + 50);
        let clipped = clip_line(&long);
        assert_eq!(clipped.chars().count(), EVIDENCE_LINE_LIMIT + 1);
        assert!(clipped.ends_with('…'));
        assert_eq!(clip_line("│ > │"), "│ > │");
    }
}
