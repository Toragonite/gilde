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

use crate::model::graph::NodeStatus;
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

/// Graph colour for a status (DESIGN §9-5). The graph shows the same truth as
/// the pill or it is worse than useless.
pub fn node_status_of(status: &SessionStatus) -> NodeStatus {
    match status {
        SessionStatus::Starting | SessionStatus::Idle => NodeStatus::Idle,
        SessionStatus::Working { .. } => NodeStatus::Working,
        SessionStatus::WaitingForInput { .. } => NodeStatus::Waiting,
        SessionStatus::Stalled { .. } => NodeStatus::Stalled,
        SessionStatus::Exited { .. } | SessionStatus::Killed => NodeStatus::Exited,
        SessionStatus::Detached => NodeStatus::Detached,
        SessionStatus::Failed { .. } => NodeStatus::Error,
    }
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
