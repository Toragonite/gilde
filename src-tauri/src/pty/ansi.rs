//! The ANSI tail — the third signal source of DESIGN §6-4 (M1-PLAN T16.1).
//!
//! An agent CLI does not tell us what it is doing. It *draws* what it is doing,
//! with escape sequences, on a terminal it believes a human is watching. This
//! module is the smallest thing that can read that drawing back: it feeds the
//! session's bytes through [`vte::Parser`], keeps the printable text and the
//! line structure, throws away CSI / OSC / DCS, and exposes the last few
//! non-empty lines so the signature patterns have something to match against.
//!
//! ## What it is, and what it deliberately is not
//!
//! It is **not** a terminal emulator. There is no screen, no scroll region, no
//! row addressing, no wide-character accounting. It is a *line accumulator with
//! a column cursor*, which is exactly enough for the three shapes agent CLIs
//! actually use:
//!
//! | shape | what the CLI emits | what we do |
//! |---|---|---|
//! | a spinner rewriting one line | `\r` or `CSI 2 K` then new text | overwrite from column 0 |
//! | an ink/React frame redraw | `CSI n A`, `CSI 0 J`, then the whole frame | append the frame; the newest lines win |
//! | a full-screen repaint | `CSI 2 J`, `CSI H`, then the frame | clear the retained lines, then append |
//!
//! Emulating rows properly would mean owning a grid, a scrollback and a
//! resize protocol — a second xterm.js in Rust, for the sake of matching six
//! regexes. The tail is allowed to be approximate because **nothing renders
//! it**: it feeds pattern matching whose every match is reported to the user as
//! evidence, with the matched line quoted, so an approximation that misleads is
//! visible rather than silent.
//!
//! ## Bytes still are not decoded here
//!
//! `src/pty/`'s rule is that no byte reaches a `String` through either of the
//! two bulk conversions `scripts/lint-forbidden.sh` bans by name — they are what
//! would quietly corrupt the delivery path. This module calls neither:
//! [`vte::Parser`] decodes UTF-8 itself, one `char` at a time, and hands us
//! `U+FFFD` for anything invalid. The bytes that go to xterm.js are untouched;
//! what is built here is a *separate*, lossy, never-delivered view whose only
//! consumer is [`crate::status`].
//!
//! ## Bounds
//!
//! DESIGN says "a 64 KiB tail". Rather than re-parsing a 64 KiB window on every
//! evaluation, the parser is fed incrementally and the *retained text* is capped
//! at [`TAIL_BYTES`] (and [`TAIL_LINES`] lines, and [`MAX_LINE_CHARS`] per
//! line). The result is the same lines with none of the repeated work.

use std::collections::VecDeque;

use vte::{Params, Perform};

/// Retained decoded text, in chars. DESIGN §6-4's "64 KiB tail".
pub const TAIL_BYTES: usize = 64 * 1024;

/// Retained completed lines. A frame from an ink-based TUI is tens of lines;
/// 256 keeps several frames without letting a `yes`-style flood grow the heap.
pub const TAIL_LINES: usize = 256;

/// A row longer than this soft-wraps. No terminal is 1024 columns wide; a line
/// that long is a program printing a blob, and it must not be able to make one
/// unbounded `Vec<char>`.
pub const MAX_LINE_CHARS: usize = 1024;

/// How many non-empty lines the status engine looks at (DESIGN §6-4).
pub const VISIBLE_LINES: usize = 8;

/// The parser plus the text it has accumulated.
///
/// Cheap to construct and cheap to feed; one lives inside each session's status
/// state for the life of the session.
pub struct AnsiTail {
    parser: vte::Parser,
    screen: Screen,
}

impl std::fmt::Debug for AnsiTail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnsiTail")
            .field("rows", &self.screen.rows.len())
            .field("alt_screen", &self.screen.alt_screen)
            .field("bell", &self.screen.bell)
            .finish()
    }
}

impl Default for AnsiTail {
    fn default() -> Self {
        AnsiTail::new()
    }
}

impl AnsiTail {
    pub fn new() -> AnsiTail {
        AnsiTail {
            parser: vte::Parser::new(),
            screen: Screen::default(),
        }
    }

    /// Push one run of pty bytes through the parser.
    ///
    /// `vte` 0.15 takes a slice (PINNED §3), so a 64 KiB read is one call and
    /// not a byte loop.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.screen, bytes);
        self.screen.trim();
    }

    /// Every retained row, topmost first, trailing blanks trimmed. Diagnostics
    /// and the fixture tests; the engine wants [`AnsiTail::visible_lines`].
    pub fn screen_lines(&self) -> Vec<String> {
        self.screen
            .rows
            .iter()
            .map(|row| trim_line(row))
            .collect()
    }

    /// The last `n` **non-empty** lines, oldest first.
    ///
    /// Oldest first is what [`crate::agents::signatures::AgentSignatures::match_tail`]
    /// expects: it scans in reverse so the newest line wins a tie between a
    /// stale prompt and a fresh one. The line still being written is included —
    /// a prompt (`│ > │`) and a spinner (`… (esc to interrupt)`) usually have no
    /// trailing newline, and dropping them would blind the matcher to the only
    /// two lines that matter.
    pub fn visible_lines(&self, n: usize) -> Vec<String> {
        let mut out: Vec<String> = Vec::with_capacity(n.min(TAIL_LINES));
        if n == 0 {
            return out;
        }
        for row in self.screen.rows.iter().rev() {
            if out.len() >= n {
                break;
            }
            let line = trim_line(row);
            if !line.is_empty() {
                out.push(line);
            }
        }
        out.reverse();
        out
    }

    /// Whether the child is currently in the alternate screen buffer
    /// (`CSI ? 1049 h` / `l`, and the older `47` / `1047`).
    ///
    /// DESIGN §6-5 uses this to decide whether typing a prompt into the tty is
    /// meaningful and whether "copy plain output" would produce anything.
    pub fn alt_screen(&self) -> bool {
        self.screen.alt_screen
    }

    /// The cursor as `(row, col)` — the row is an index into the retained rows,
    /// the column is in chars.
    ///
    /// Part of the screen digest the status engine keeps (DESIGN §6-4): two
    /// frames with identical text but the cursor in a different cell are not the
    /// same screen, and a composer that moved its caret is a screen the user
    /// just touched.
    pub fn cursor(&self) -> (usize, usize) {
        (self.screen.row, self.screen.col)
    }

    /// Read and clear the BEL latch (DESIGN §6-4).
    ///
    /// `0x07` is the cheapest "the agent wants you" signal that works across all
    /// four CLIs, so it is latched rather than sampled: a bell that arrived
    /// between two evaluations is still reported by the next one.
    pub fn take_bell(&mut self) -> bool {
        std::mem::take(&mut self.screen.bell)
    }

    /// True while no printable text has been seen at all.
    pub fn is_empty(&self) -> bool {
        self.screen.rows.iter().all(|row| row.is_empty())
    }

    /// Retained rows, including blank ones. Diagnostics only.
    pub fn retained_lines(&self) -> usize {
        self.screen.rows.len()
    }

    /// Retained characters. Bounded by [`TAIL_BYTES`]; diagnostics only.
    pub fn retained_chars(&self) -> usize {
        self.screen.retained
    }
}

/// The bounded screen the parser drives.
///
/// A `VecDeque` of rows with a `(row, col)` cursor — the smallest structure that
/// can absorb absolute cursor addressing, which is how both `claude` and `codex`
/// actually draw. Without rows, `CSI 12;4H` puts the twelfth line of a frame on
/// top of the fourth and every box-drawn dialog comes out as one line of
/// interleaved rubbish (observed, then fixed).
///
/// The screen has no fixed height: rows are created as they are addressed and
/// the oldest are dropped once [`TAIL_LINES`] or [`TAIL_BYTES`] is exceeded, so
/// a program that only ever prints and newlines scrolls exactly as it should
/// while a full-screen TUI repaints in place.
#[derive(Debug, Default)]
struct Screen {
    /// Rows, topmost first. Index 0 is the oldest retained row.
    rows: VecDeque<Vec<char>>,
    /// Cursor row, as an index into `rows`.
    row: usize,
    /// Cursor column, in chars.
    col: usize,
    /// Total retained chars, kept **incrementally**.
    ///
    /// It has to be: `trim` runs on every newline, and a `yes`-style flood is
    /// 2.5 million newlines per 5 MB. Re-summing 256 rows each time turns the
    /// reader thread — the one thread that must never make the child wait —
    /// into an O(rows × bytes) loop (measured, then fixed).
    retained: usize,
    /// `ESC 7` / `ESC 8`.
    saved: Option<(usize, usize)>,
    alt_screen: bool,
    bell: bool,
}

impl Screen {
    /// Make `self.row` addressable, growing and then trimming the screen.
    fn ensure_row(&mut self) {
        while self.rows.len() <= self.row {
            self.rows.push_back(Vec::new());
        }
        self.trim();
    }

    /// Drop the oldest rows until both bounds hold, keeping the cursor on the
    /// row it was on.
    fn trim(&mut self) {
        while self.rows.len() > TAIL_LINES || self.retained > TAIL_BYTES {
            match self.rows.pop_front() {
                Some(gone) => self.retained -= gone.len(),
                None => break,
            }
            self.row = self.row.saturating_sub(1);
        }
    }

    /// Shorten one row and keep the counter honest.
    fn truncate_row(&mut self, index: usize, len: usize) {
        if let Some(row) = self.rows.get_mut(index) {
            if len < row.len() {
                self.retained -= row.len() - len;
                row.truncate(len);
            }
        }
    }

    /// Down one row **and back to column zero**.
    ///
    /// Strictly, `LF` only moves down and `CR` is what returns to column zero —
    /// which is why a tty without `ONLCR` staircases. Our ptys always have
    /// `OPOST | ONLCR` on (`pty::manager::set_output_processing`), so the `CR`
    /// arrives anyway and this is a no-op for every session the product opens.
    /// For a `raw_output` session it is a deliberate difference: staircased
    /// text is unreadable, and unreadable text matches no pattern, so the tail
    /// would silently lose its third signal source rather than reproduce a
    /// terminal artefact nobody wants to see.
    fn newline(&mut self) {
        self.row += 1;
        self.col = 0;
        self.ensure_row();
    }

    fn clear_all(&mut self) {
        self.rows.clear();
        self.retained = 0;
        self.row = 0;
        self.col = 0;
        self.saved = None;
    }

    /// `CSI K` — erase in line.
    fn erase_line(&mut self, mode: u16) {
        self.ensure_row();
        let col = self.col;
        let index = self.row;
        match mode {
            0 => self.truncate_row(index, col),
            // Blanking leaves the row the same length, so the counter does not
            // move.
            1 => {
                if let Some(row) = self.rows.get_mut(index) {
                    let upto = col.min(row.len());
                    for cell in &mut row[..upto] {
                        *cell = ' ';
                    }
                }
            }
            _ => self.truncate_row(index, 0),
        }
    }

    /// `CSI J` — erase in display.
    fn erase_display(&mut self, mode: u16) {
        match mode {
            // Cursor to the end of the screen. This is what an ink-style redraw
            // emits after moving the cursor up, and getting it right is what
            // makes the *new* frame replace the old one instead of appending.
            0 => {
                self.erase_line(0);
                while self.rows.len() > self.row + 1 {
                    if let Some(gone) = self.rows.pop_back() {
                        self.retained -= gone.len();
                    }
                }
            }
            // The start of the screen to the cursor.
            1 => {
                for index in 0..self.row.min(self.rows.len()) {
                    self.truncate_row(index, 0);
                }
                self.erase_line(1);
            }
            _ => self.clear_all(),
        }
    }

    fn first_param(params: &Params, default: u16) -> u16 {
        params
            .iter()
            .next()
            .and_then(|group| group.first().copied())
            .filter(|v| *v != 0)
            .unwrap_or(default)
    }

    fn param_at(params: &Params, index: usize, default: u16) -> u16 {
        params
            .iter()
            .nth(index)
            .and_then(|group| group.first().copied())
            .filter(|v| *v != 0)
            .unwrap_or(default)
    }

    fn mode_param(params: &Params) -> u16 {
        params
            .iter()
            .next()
            .and_then(|group| group.first().copied())
            .unwrap_or(0)
    }
}

impl Perform for Screen {
    fn print(&mut self, c: char) {
        if self.col >= MAX_LINE_CHARS {
            // Soft wrap. We do not know the real width — a TUI never reaches
            // this — but a program printing a blob with no newline must not be
            // able to grow one unbounded row.
            self.newline();
        }
        self.ensure_row();
        let col = self.col;
        let mut grew = 0;
        if let Some(row) = self.rows.get_mut(self.row) {
            if col < row.len() {
                row[col] = c;
            } else {
                grew = col + 1 - row.len();
                row.resize(col, ' ');
                row.push(c);
            }
        }
        self.retained += grew;
        self.col += 1;
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            // BEL — latched until the status engine reads it.
            0x07 => self.bell = true,
            // BS
            0x08 => self.col = self.col.saturating_sub(1),
            // HT — the classic 8-column stop.
            0x09 => self.col = (((self.col / 8) + 1) * 8).min(MAX_LINE_CHARS),
            // LF, VT, FF all move down a row.
            0x0a | 0x0b | 0x0c => self.newline(),
            // CR — the spinner's favourite byte: rewrite from column zero.
            0x0d => self.col = 0,
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        let private = intermediates.first().copied() == Some(b'?');
        match (private, action) {
            // The alternate screen buffer, in all three spellings a TUI uses.
            (true, 'h' | 'l') => {
                let on = action == 'h';
                for group in params.iter() {
                    if matches!(group.first(), Some(47 | 1047 | 1049)) {
                        self.alt_screen = on;
                        // Switching buffers means everything the user could see
                        // is replaced; a stale prompt left behind would keep
                        // matching for as long as the TUI stayed quiet.
                        self.clear_all();
                    }
                }
            }
            (true, _) => {}
            // CUU
            (false, 'A') => {
                self.row = self
                    .row
                    .saturating_sub(Screen::first_param(params, 1) as usize)
            }
            // CUD
            (false, 'B') => {
                self.row = self
                    .row
                    .saturating_add(Screen::first_param(params, 1) as usize)
                    .min(TAIL_LINES);
                self.ensure_row();
            }
            // CUF
            (false, 'C') => {
                self.col = self
                    .col
                    .saturating_add(Screen::first_param(params, 1) as usize)
                    .min(MAX_LINE_CHARS)
            }
            // CUB
            (false, 'D') => {
                self.col = self
                    .col
                    .saturating_sub(Screen::first_param(params, 1) as usize)
            }
            // CNL / CPL — down or up, and to column zero.
            (false, 'E') => {
                self.row = self
                    .row
                    .saturating_add(Screen::first_param(params, 1) as usize)
                    .min(TAIL_LINES);
                self.col = 0;
                self.ensure_row();
            }
            (false, 'F') => {
                self.col = 0;
                self.row = self
                    .row
                    .saturating_sub(Screen::first_param(params, 1) as usize);
            }
            // CHA / HPA — absolute column.
            (false, 'G' | '`') => {
                self.col = (Screen::first_param(params, 1) as usize)
                    .saturating_sub(1)
                    .min(MAX_LINE_CHARS)
            }
            // VPA — absolute row.
            (false, 'd') => {
                self.row = (Screen::first_param(params, 1) as usize)
                    .saturating_sub(1)
                    .min(TAIL_LINES);
                self.ensure_row();
            }
            // CUP / HVP.
            //
            // Row 1 is the top of the *screen*, and we do not have one: a TUI
            // that repaints from `CSI H` is addressing the frame it is about to
            // draw, so the frame's row 1 is where the current screen starts.
            (false, 'H' | 'f') => {
                self.row = (Screen::param_at(params, 0, 1) as usize)
                    .saturating_sub(1)
                    .min(TAIL_LINES);
                self.col = (Screen::param_at(params, 1, 1) as usize)
                    .saturating_sub(1)
                    .min(MAX_LINE_CHARS);
                self.ensure_row();
            }
            (false, 'K') => self.erase_line(Screen::mode_param(params)),
            (false, 'J') => self.erase_display(Screen::mode_param(params)),
            _ => {}
        }
    }

    // OSC (title, hyperlinks, clipboard), DCS and most single escapes carry no
    // visible text. `Perform`'s defaults already drop them; being explicit
    // documents that it is a decision rather than an omission.
    fn osc_dispatch(&mut self, _params: &[&[u8]], bell_terminated: bool) {
        // `OSC … BEL` is a *terminator*, not a bell. Nothing to latch.
        let _ = bell_terminated;
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, byte: u8) {
        match byte {
            // DECSC / DECRC.
            b'7' => self.saved = Some((self.row, self.col)),
            b'8' => {
                if let Some((row, col)) = self.saved {
                    self.row = row;
                    self.col = col;
                    self.ensure_row();
                }
            }
            // IND / NEL — down one row. Both land at column zero here, for
            // the same reason `newline` does.
            b'D' | b'E' => self.newline(),
            // RI — up one row.
            b'M' => self.row = self.row.saturating_sub(1),
            // RIS — full reset.
            b'c' => {
                self.clear_all();
                self.alt_screen = false;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
impl Screen {
    /// The counter against the truth. Called after every scenario the tests
    /// drive, because an incremental counter that drifts is a memory leak that
    /// only shows up after hours of terminal output.
    fn audit_retained(&self) {
        let real: usize = self.rows.iter().map(|row| row.len()).sum();
        assert_eq!(
            self.retained, real,
            "the retained counter drifted from the real screen size"
        );
    }
}

fn trim_line(chars: &[char]) -> String {
    let end = chars
        .iter()
        .rposition(|c| !c.is_whitespace())
        .map(|i| i + 1)
        .unwrap_or(0);
    chars[..end].iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tail_of(bytes: &[u8]) -> AnsiTail {
        let mut tail = AnsiTail::new();
        tail.feed(bytes);
        tail.screen.audit_retained();
        tail
    }

    #[test]
    fn csi_and_osc_leave_no_trace_in_the_text() {
        let tail = tail_of(
            b"\x1b[1;32mgreen\x1b[0m plain\r\n\x1b]0;a window title\x07after\r\n\x1bP+q544e\x1b\\dcs gone\r\n",
        );
        assert_eq!(
            tail.visible_lines(8),
            vec![
                "green plain".to_string(),
                "after".to_string(),
                "dcs gone".to_string()
            ]
        );
    }

    #[test]
    fn a_carriage_return_rewrites_the_line_the_way_a_spinner_does() {
        // Exactly what a progress line looks like on the wire.
        let tail = tail_of(b"working 1s\rworking 2s\rworking 12s");
        assert_eq!(tail.visible_lines(4), vec!["working 12s".to_string()]);
    }

    #[test]
    fn erase_in_line_clears_what_the_cursor_left_behind() {
        // `\r` alone would leave the tail of the longer previous text.
        let tail = tail_of(b"a very long status line\r\x1b[2Kdone");
        assert_eq!(tail.visible_lines(4), vec!["done".to_string()]);

        // EL 0 truncates from the cursor, which is where `\r` + 4 chars put it.
        let tail = tail_of(b"abcdefgh\rWXYZ\x1b[0K");
        assert_eq!(tail.visible_lines(4), vec!["WXYZ".to_string()]);
    }

    #[test]
    fn a_full_repaint_replaces_the_screen() {
        let tail = tail_of(b"one\r\ntwo\r\n\x1b[2J\x1b[Hthree");
        assert_eq!(tail.visible_lines(8), vec!["three".to_string()]);
    }

    #[test]
    fn an_ink_style_redraw_replaces_the_frame_instead_of_appending_it() {
        // What an ink/React TUI emits every render: go back up over the frame it
        // drew, erase from there to the end of the screen, print the new one.
        // Getting `CSI 0 J` wrong here is the difference between a tail that
        // shows the current frame and one that shows every frame ever drawn.
        let tail = tail_of(
            b"line a\r\nline b\r\nline c\r\n\x1b[3A\x1b[0Jline a\r\nline b'\r\n\xe2\x94\x82 > \xe2\x94\x82",
        );
        assert_eq!(
            tail.visible_lines(8),
            vec!["line a".to_string(), "line b'".to_string(), "│ > │".to_string()]
        );
    }

    #[test]
    fn absolute_cursor_addressing_keeps_rows_apart() {
        // The bug this pins: without rows, `CSI 4;1H` writes the fourth line of
        // a dialog on top of the first and the whole box comes out as one line
        // of interleaved rubbish — which is exactly what the first capture of
        // `codex`'s trust dialog looked like.
        let mut tail = AnsiTail::new();
        tail.feed(b"\x1b[2J");
        tail.feed(b"\x1b[1;1HAllow command?");
        tail.feed(b"\x1b[3;3H1. Yes");
        tail.feed(b"\x1b[4;3H2. No");
        assert_eq!(
            tail.visible_lines(8),
            vec![
                "Allow command?".to_string(),
                "  1. Yes".to_string(),
                "  2. No".to_string()
            ]
        );
        assert_eq!(
            tail.screen_lines(),
            vec![
                "Allow command?".to_string(),
                String::new(),
                "  1. Yes".to_string(),
                "  2. No".to_string()
            ],
            "the blank row between them is a real row, it is just not visible text"
        );
    }

    #[test]
    fn a_saved_cursor_comes_back_to_the_same_cell() {
        let tail = tail_of(b"prompt: \x1b7\r\nnoise\x1b8done");
        // `done` lands back on the first row, where the cursor was saved; the
        // second row keeps what was written there in between.
        assert_eq!(
            tail.visible_lines(4),
            vec!["prompt: done".to_string(), "noise".to_string()]
        );
    }

    #[test]
    fn the_last_eight_non_empty_lines_are_what_comes_out() {
        let mut body = Vec::new();
        for i in 0..40 {
            body.extend_from_slice(format!("line {i}\r\n").as_bytes());
            body.extend_from_slice(b"   \r\n"); // blank-but-for-spaces
        }
        body.extend_from_slice(b"\xe2\x94\x82 > \xe2\x94\x82"); // the prompt, unterminated
        let tail = tail_of(&body);

        let lines = tail.visible_lines(VISIBLE_LINES);
        assert_eq!(lines.len(), VISIBLE_LINES);
        assert_eq!(lines.last().unwrap(), "│ > │", "the prompt is the newest");
        assert_eq!(lines.first().unwrap(), "line 33");
        assert!(
            lines.iter().all(|l| !l.trim().is_empty()),
            "blank lines are skipped, not counted"
        );
    }

    #[test]
    fn alt_screen_tracks_1049_and_clears_what_the_user_stopped_seeing() {
        let mut tail = AnsiTail::new();
        assert!(!tail.alt_screen());
        tail.feed(b"shell text\r\n\x1b[?1049h");
        assert!(tail.alt_screen());
        assert!(
            tail.visible_lines(8).is_empty(),
            "the shell's text is not on the screen the user is looking at"
        );
        tail.feed(b"tui frame\r\n\x1b[?1049l");
        assert!(!tail.alt_screen());

        // The older spellings count too.
        let mut tail = AnsiTail::new();
        tail.feed(b"\x1b[?47h");
        assert!(tail.alt_screen());
        tail.feed(b"\x1b[?47l");
        assert!(!tail.alt_screen());
    }

    #[test]
    fn bel_latches_once_and_is_consumed_by_the_reader() {
        let mut tail = AnsiTail::new();
        assert!(!tail.take_bell());
        tail.feed(b"done\x07\r\n");
        assert!(tail.take_bell(), "the bell is latched");
        assert!(!tail.take_bell(), "and reading it clears the latch");
        assert_eq!(tail.visible_lines(2), vec!["done".to_string()]);
    }

    #[test]
    fn an_osc_terminated_by_bel_is_not_a_bell() {
        let mut tail = AnsiTail::new();
        tail.feed(b"\x1b]0;title\x07text");
        assert!(
            !tail.take_bell(),
            "the 0x07 belongs to the OSC string, not to the user"
        );
    }

    #[test]
    fn a_chunk_boundary_inside_a_sequence_or_a_codepoint_changes_nothing() {
        let whole = b"\xed\x95\x9c\xea\xb8\x80 \x1b[1;31mred\x1b[0m\r\ndone \xf0\x9f\x98\x80";
        let reference = tail_of(whole).visible_lines(8);

        // Split at every possible offset; the parser is a state machine and the
        // reader hands it whatever a 64 KiB read happened to end on.
        for split in 1..whole.len() {
            let mut tail = AnsiTail::new();
            tail.feed(&whole[..split]);
            tail.feed(&whole[split..]);
            assert_eq!(
                tail.visible_lines(8),
                reference,
                "a split at byte {split} changed the tail"
            );
        }
        assert_eq!(reference, vec!["한글 red".to_string(), "done 😀".to_string()]);
    }

    #[test]
    fn invalid_utf8_does_not_panic_and_does_not_stop_the_parse() {
        let mut tail = AnsiTail::new();
        tail.feed(b"before \xff\xfe after\r\n");
        let lines = tail.visible_lines(4);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with("before "));
        assert!(lines[0].ends_with(" after"));
    }

    #[test]
    fn the_retained_counter_survives_every_way_a_row_can_change() {
        let mut tail = AnsiTail::new();
        for chunk in [
            &b"one\r\ntwo\r\nthree"[..],
            b"\r\x1b[2Kthree again",
            b"\x1b[1;1H\x1b[0J",
            b"a\r\nb\r\nc\r\nd",
            b"\x1b[2;1H\x1b[1K",
            b"\x1b[3;1H\x1b[0J",
            b"\x1b[2J",
            b"tail\x1b[?1049h\x1b[?1049l",
        ] {
            tail.feed(chunk);
            tail.screen.audit_retained();
        }
        // And after eviction, which is the case that actually loses count.
        for i in 0..(TAIL_LINES * 3) {
            tail.feed(format!("row {i} with some text on it\r\n").as_bytes());
        }
        tail.screen.audit_retained();
        assert!(tail.retained_chars() <= TAIL_BYTES);
    }

    #[test]
    fn a_flood_of_newlines_stays_bounded_and_costs_a_bounded_amount_of_work() {
        // `yes | head -c 5000000` is the acceptance criterion T15 measures, and
        // it is 2.5 million newlines. This is the shape that made `trim` an
        // O(rows) scan per line — and, with a bare `\n`, the shape that made
        // every row a staircase.
        let mut tail = AnsiTail::new();
        let chunk = b"y\n".repeat(32 * 1024);
        let started = std::time::Instant::now();
        for _ in 0..80 {
            tail.feed(&chunk);
        }
        let elapsed = started.elapsed();
        tail.screen.audit_retained();
        assert!(tail.retained_lines() <= TAIL_LINES);
        assert!(tail.retained_chars() <= TAIL_BYTES);
        assert_eq!(tail.visible_lines(3), vec!["y".to_string(); 3]);
        // Generous by two orders of magnitude — this is a "did the cost become
        // quadratic again" guard, not a benchmark.
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "5 MB of newlines took {elapsed:?} on the reader thread"
        );
        eprintln!("[T16] 5 MB of `yes` through the ansi tail: {elapsed:?}");
    }

    #[test]
    fn the_retained_text_is_bounded_by_lines_and_by_bytes() {
        let mut tail = AnsiTail::new();
        for i in 0..(TAIL_LINES * 4) {
            tail.feed(format!("line {i}\r\n").as_bytes());
        }
        assert!(tail.retained_lines() <= TAIL_LINES);

        // A blob with no newline in it must not grow one unbounded line.
        let mut tail = AnsiTail::new();
        let blob = vec![b'x'; MAX_LINE_CHARS * 3];
        tail.feed(&blob);
        assert!(tail.retained_lines() <= TAIL_LINES);
        assert_eq!(tail.visible_lines(1)[0].chars().count(), MAX_LINE_CHARS);
    }

    #[test]
    fn a_tab_lands_on_the_eight_column_stop() {
        let tail = tail_of(b"ab\tc");
        assert_eq!(tail.visible_lines(1), vec!["ab      c".to_string()]);
    }

    #[test]
    fn a_backspace_walks_the_cursor_back_the_way_a_readline_prompt_does() {
        let tail = tail_of(b"yesn\x08\x08 no");
        assert_eq!(tail.visible_lines(1), vec!["ye no".to_string()]);
    }

    #[test]
    fn an_empty_tail_answers_with_an_empty_vec_not_a_blank_line() {
        let tail = AnsiTail::new();
        assert!(tail.is_empty());
        assert!(tail.visible_lines(8).is_empty());
        assert!(tail_of(b"   \r\n\t\r\n").visible_lines(8).is_empty());
    }
}
