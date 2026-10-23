//! The PTY text grid — the screen the status engine and the screen-prompt
//! reader look at (contract §6.2.4, §6.2.6).
//!
//! An agent CLI does not tell us what it is doing; it *draws* it. This module
//! feeds a session's bytes through [`vte::Parser`] into a fixed-size cell grid
//! that follows xterm closely enough to read a modern TUI back:
//!
//! * absolute and relative cursor addressing (`CUP`, `CUU/CUD/CUF/CUB`, `CHA`,
//!   `VPA`, `CNL/CPL`), erase (`ED`, `EL`, `ECH`), insert/delete of characters
//!   and lines, scroll regions (`DECSTBM`, `SU/SD`, `IND/RI/NEL`), tab stops;
//! * the **alternate screen** (`?47`, `?1047`, `?1049` with its saved cursor);
//! * xterm's **pending-wrap** rule exactly: printing into the last column
//!   leaves the cursor *on* that column with a wrap flag, and only the next
//!   printable character wraps. A `CR`, `LF` or any cursor movement clears the
//!   flag. A grid without this rule misplaces every row that a cell-diff
//!   renderer fills to the right margin (observed on the 2.1.307 TUI [R6 §4]);
//! * wide characters (two cells) and zero-width combining marks;
//! * OSC 0/2 titles (sanitized, **memory only**, A12.4), the BEL latch, and the
//!   two terminal queries a hidden PTY must answer (§6.2.6): DA1 (`CSI c`) and
//!   DSR cursor position (`CSI 6 n`). Every other query stays unanswered, as
//!   xterm.js leaves it by default.
//!
//! ## Bytes still are not decoded here
//!
//! `src/pty/`'s rule is that no byte reaches a `String` through either of the
//! two bulk conversions `scripts/lint-forbidden.sh` bans by name. This module
//! calls neither: [`vte::Parser`] decodes UTF-8 itself, one `char` at a time,
//! and hands us `U+FFFD` for anything invalid. The bytes that go to xterm.js
//! are untouched; the grid is a separate, lossy, never-delivered view whose
//! only consumers are [`crate::status`] and the screen-prompt reader.
//!
//! ## Cost
//!
//! Printing is O(1) per character; a scroll rotates row headers and clears the
//! recycled row in place (no allocation), so a 5 MB `yes` flood costs a few
//! hundred milliseconds on the reader thread, not a quadratic walk.

use unicode_width::UnicodeWidthChar;
use vte::{Params, Perform};

/// Grid size when nobody told us one (contract §6.2.1: "else 120×40").
pub const DEFAULT_COLS: usize = 120;
pub const DEFAULT_ROWS: usize = 40;

/// Size bounds (the `pty_resize` clamp of §4.2.4).
pub const MAX_COLS: usize = 1000;
pub const MAX_ROWS: usize = 500;

/// How many non-empty lines the status engine looks at for its tail patterns.
pub const VISIBLE_LINES: usize = 8;

/// Titles are display-only and capped (OSC 0/2, §6.2.6).
pub const TITLE_MAX_CHARS: usize = 256;

/// Kept for callers of the old line accumulator: the most rows a grid can
/// retain (one screen at the maximum size).
pub const TAIL_LINES: usize = MAX_ROWS;

/// The DA1 reply the core sends for a hidden PTY — xterm.js's own answer
/// (`VT100 with Advanced Video Option`).
pub const DA1_REPLY: &[u8] = b"\x1b[?1;2c";

// ---------------------------------------------------------------------------
// public surface
// ---------------------------------------------------------------------------

/// A terminal query seen in the output, in stream order, with the state the
/// reply needs captured at that moment (a DSR answers with the cursor position
/// *at the query*, not at the end of the chunk).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermQuery {
    /// `CSI c` / `CSI 0 c`.
    PrimaryAttributes,
    /// `CSI 6 n`; 1-based row and column.
    CursorPosition { row: u16, col: u16 },
}

impl TermQuery {
    /// The bytes xterm.js would answer with.
    pub fn reply(&self) -> Vec<u8> {
        match self {
            TermQuery::PrimaryAttributes => DA1_REPLY.to_vec(),
            TermQuery::CursorPosition { row, col } => format!("\x1b[{row};{col}R").into_bytes(),
        }
    }
}

/// The parser plus the grid it drives. One lives in each session's status
/// state for the life of the session. (The name is historical: this used to
/// be a line accumulator over the *tail* of the output.)
pub struct AnsiTail {
    parser: vte::Parser,
    grid: Grid,
}

impl std::fmt::Debug for AnsiTail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnsiTail")
            .field("cols", &self.grid.cols)
            .field("rows", &self.grid.rows)
            .field("alt_screen", &self.grid.alt_active)
            .finish()
    }
}

impl Default for AnsiTail {
    fn default() -> Self {
        AnsiTail::new()
    }
}

impl AnsiTail {
    /// A 120×40 grid (the §6.2.1 default size).
    pub fn new() -> AnsiTail {
        AnsiTail::with_size(DEFAULT_COLS as u16, DEFAULT_ROWS as u16)
    }

    pub fn with_size(cols: u16, rows: u16) -> AnsiTail {
        AnsiTail {
            parser: vte::Parser::new(),
            grid: Grid::new(cols as usize, rows as usize),
        }
    }

    /// Push one run of pty bytes through the parser.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.grid, bytes);
    }

    /// The child was told a new size (`TIOCSWINSZ`).
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.grid.resize(cols as usize, rows as usize);
    }

    /// `(cols, rows)`.
    pub fn size(&self) -> (usize, usize) {
        (self.grid.cols, self.grid.rows)
    }

    /// Every row of the visible screen, topmost first, trailing blanks trimmed.
    pub fn screen_lines(&self) -> Vec<String> {
        self.grid.lines()
    }

    /// The last `n` **non-empty** rows of the visible screen, oldest first.
    pub fn visible_lines(&self, n: usize) -> Vec<String> {
        let mut out: Vec<String> = Vec::with_capacity(n.min(self.grid.rows));
        if n == 0 {
            return out;
        }
        for index in (0..self.grid.rows).rev() {
            if out.len() >= n {
                break;
            }
            let line = self.grid.row_text(index);
            if !line.trim().is_empty() {
                out.push(line);
            }
        }
        out.reverse();
        out
    }

    /// True while the child has the alternate screen buffer up.
    pub fn alt_screen(&self) -> bool {
        self.grid.alt_active
    }

    /// `(row, col)` of the cursor, 0-based.
    pub fn cursor(&self) -> (usize, usize) {
        (self.grid.cur_row, self.grid.cur_col)
    }

    /// Read and clear the BEL latch.
    pub fn take_bell(&mut self) -> bool {
        std::mem::take(&mut self.grid.bell)
    }

    /// The terminal queries seen since the last call, in stream order.
    pub fn take_queries(&mut self) -> Vec<TermQuery> {
        std::mem::take(&mut self.grid.queries)
    }

    /// The latest OSC 0/2 title, sanitized. Display only — never state, never
    /// persisted (titles go stale [R6 §4], A12.4).
    pub fn title(&self) -> Option<&str> {
        self.grid.title.as_deref()
    }

    /// Bumped on every change to the visible cells, the cursor or the active
    /// buffer. Equal generations mean an unchanged screen; a repaint of
    /// identical cells still bumps it (callers compare text when that
    /// matters).
    pub fn generation(&self) -> u64 {
        self.grid.generation
    }

    /// True when nothing visible has been drawn.
    pub fn is_empty(&self) -> bool {
        (0..self.grid.rows).all(|r| self.grid.row_is_blank(r))
    }

    /// Rows of the visible screen (the grid height).
    pub fn retained_lines(&self) -> usize {
        self.grid.rows
    }

    /// Non-blank cells on the visible screen.
    pub fn retained_chars(&self) -> usize {
        self.grid
            .active()
            .iter()
            .map(|row| row.iter().filter(|c| c.ch != ' ' && c.wide != WIDE_SPACER).count())
            .sum()
    }

    /// Whether bracketed paste (`?2004`) is enabled by the child.
    pub fn bracketed_paste(&self) -> bool {
        self.grid.bracketed_paste
    }

    /// Whether the child asked for application cursor keys (`?1`, DECCKM):
    /// arrows are then `ESC O A` rather than `ESC [ A`.
    pub fn app_cursor_keys(&self) -> bool {
        self.grid.app_cursor
    }
}

// ---------------------------------------------------------------------------
// the grid
// ---------------------------------------------------------------------------

const WIDE_NONE: u8 = 0;
/// The left half of a two-cell character.
const WIDE_LEAD: u8 = 1;
/// The right half; renders as nothing.
const WIDE_SPACER: u8 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Cell {
    ch: char,
    /// Zero-width combining marks that follow `ch`.
    zw: Option<Box<str>>,
    wide: u8,
}

impl Cell {
    const BLANK: Cell = Cell {
        ch: ' ',
        zw: None,
        wide: WIDE_NONE,
    };

    fn reset(&mut self) {
        self.ch = ' ';
        self.zw = None;
        self.wide = WIDE_NONE;
    }
}

type Row = Vec<Cell>;

#[derive(Debug, Clone, Copy)]
struct SavedCursor {
    row: usize,
    col: usize,
    pending_wrap: bool,
    origin: bool,
    autowrap: bool,
}

/// The emulator state `vte` drives. Only text and geometry are kept: no
/// colours, no attributes (nothing renders this grid).
#[derive(Debug)]
pub(crate) struct Grid {
    cols: usize,
    rows: usize,
    main: Vec<Row>,
    alt: Vec<Row>,
    alt_active: bool,
    cur_row: usize,
    cur_col: usize,
    pending_wrap: bool,
    autowrap: bool,
    origin: bool,
    insert: bool,
    /// Inclusive scroll region.
    top: usize,
    bottom: usize,
    tabs: Vec<bool>,
    saved_main: Option<SavedCursor>,
    saved_alt: Option<SavedCursor>,
    /// The cursor `?1049h` saved, restored by `?1049l`.
    saved_1049: Option<SavedCursor>,
    last_char: Option<char>,
    title: Option<String>,
    bell: bool,
    bracketed_paste: bool,
    app_cursor: bool,
    queries: Vec<TermQuery>,
    generation: u64,
}

fn blank_row(cols: usize) -> Row {
    vec![Cell::BLANK; cols]
}

fn default_tabs(cols: usize) -> Vec<bool> {
    (0..cols).map(|c| c > 0 && c % 8 == 0).collect()
}

fn clamp_size(cols: usize, rows: usize) -> (usize, usize) {
    (cols.clamp(1, MAX_COLS), rows.clamp(1, MAX_ROWS))
}

impl Grid {
    fn new(cols: usize, rows: usize) -> Grid {
        let (cols, rows) = clamp_size(cols, rows);
        Grid {
            cols,
            rows,
            main: (0..rows).map(|_| blank_row(cols)).collect(),
            alt: (0..rows).map(|_| blank_row(cols)).collect(),
            alt_active: false,
            cur_row: 0,
            cur_col: 0,
            pending_wrap: false,
            autowrap: true,
            origin: false,
            insert: false,
            top: 0,
            bottom: rows - 1,
            tabs: default_tabs(cols),
            saved_main: None,
            saved_alt: None,
            saved_1049: None,
            last_char: None,
            title: None,
            bell: false,
            bracketed_paste: false,
            app_cursor: false,
            queries: Vec::new(),
            generation: 0,
        }
    }

    fn active(&self) -> &Vec<Row> {
        if self.alt_active {
            &self.alt
        } else {
            &self.main
        }
    }

    fn active_mut(&mut self) -> &mut Vec<Row> {
        self.generation = self.generation.wrapping_add(1);
        if self.alt_active {
            &mut self.alt
        } else {
            &mut self.main
        }
    }

    fn touch(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    fn row_text(&self, index: usize) -> String {
        let Some(row) = self.active().get(index) else {
            return String::new();
        };
        let mut out = String::with_capacity(row.len());
        for cell in row {
            if cell.wide == WIDE_SPACER {
                continue;
            }
            out.push(cell.ch);
            if let Some(zw) = &cell.zw {
                out.push_str(zw);
            }
        }
        let trimmed = out.trim_end_matches(' ').len();
        out.truncate(trimmed);
        out
    }

    fn row_is_blank(&self, index: usize) -> bool {
        self.active()
            .get(index)
            .is_none_or(|row| row.iter().all(|c| c.ch == ' ' && c.zw.is_none()))
    }

    fn lines(&self) -> Vec<String> {
        (0..self.rows).map(|r| self.row_text(r)).collect()
    }

    // -- geometry -------------------------------------------------------------

    fn resize(&mut self, cols: usize, rows: usize) {
        let (cols, rows) = clamp_size(cols, rows);
        if cols == self.cols && rows == self.rows {
            return;
        }
        let cursor_row = self.cur_row;
        for screen in [&mut self.main, &mut self.alt] {
            for row in screen.iter_mut() {
                row.resize(cols, Cell::BLANK);
                // A wide character cut in half becomes a blank.
                if let Some(last) = row.last_mut() {
                    if last.wide == WIDE_LEAD {
                        last.reset();
                    }
                }
            }
            if rows < screen.len() {
                // Keep the cursor's row on screen: drop from the top as much
                // as needed, then from the bottom (xterm pushes the top into
                // its scrollback, which we do not keep).
                let excess = screen.len() - rows;
                let from_top = (cursor_row + 1).saturating_sub(rows).min(excess);
                screen.drain(..from_top);
                screen.truncate(rows);
            } else {
                while screen.len() < rows {
                    screen.push(blank_row(cols));
                }
            }
        }
        let removed_top = (cursor_row + 1).saturating_sub(rows);
        self.cur_row = cursor_row.saturating_sub(removed_top).min(rows - 1);
        self.cur_col = self.cur_col.min(cols - 1);
        self.cols = cols;
        self.rows = rows;
        self.top = 0;
        self.bottom = rows - 1;
        self.tabs = default_tabs(cols);
        self.pending_wrap = false;
        self.touch();
    }

    fn clamp_cursor(&mut self) {
        self.cur_row = self.cur_row.min(self.rows - 1);
        self.cur_col = self.cur_col.min(self.cols - 1);
    }

    /// The rows a vertical move may reach from the cursor's row: the scroll
    /// region when the cursor is inside it, else the whole screen.
    fn vertical_bounds(&self) -> (usize, usize) {
        if self.cur_row >= self.top && self.cur_row <= self.bottom {
            (self.top, self.bottom)
        } else {
            (0, self.rows - 1)
        }
    }

    fn move_to(&mut self, row: usize, col: usize) {
        let (row_base, row_max) = if self.origin {
            (self.top, self.bottom)
        } else {
            (0, self.rows - 1)
        };
        self.cur_row = (row_base + row).min(row_max);
        self.cur_col = col.min(self.cols - 1);
        self.pending_wrap = false;
        self.touch();
    }

    // -- scrolling ------------------------------------------------------------

    /// Scroll `n` rows up inside `[top, bottom]` (content moves up, blank rows
    /// enter at the bottom).
    fn scroll_up(&mut self, top: usize, bottom: usize, n: usize) {
        if top > bottom || bottom >= self.rows {
            return;
        }
        let span = bottom - top + 1;
        let n = n.min(span);
        let rows = self.active_mut();
        rows[top..=bottom].rotate_left(n);
        for row in &mut rows[bottom + 1 - n..=bottom] {
            row.iter_mut().for_each(Cell::reset);
        }
    }

    /// Scroll `n` rows down inside `[top, bottom]` (blank rows enter at the
    /// top).
    fn scroll_down(&mut self, top: usize, bottom: usize, n: usize) {
        if top > bottom || bottom >= self.rows {
            return;
        }
        let span = bottom - top + 1;
        let n = n.min(span);
        let rows = self.active_mut();
        rows[top..=bottom].rotate_right(n);
        for row in &mut rows[top..top + n] {
            row.iter_mut().for_each(Cell::reset);
        }
    }

    /// `LF` / `IND`: down one row, scrolling the region at its bottom margin.
    fn index(&mut self) {
        self.pending_wrap = false;
        if self.cur_row == self.bottom {
            self.scroll_up(self.top, self.bottom, 1);
        } else if self.cur_row + 1 < self.rows {
            self.cur_row += 1;
        }
        self.touch();
    }

    /// `RI`: up one row, scrolling the region down at its top margin.
    fn reverse_index(&mut self) {
        self.pending_wrap = false;
        if self.cur_row == self.top {
            self.scroll_down(self.top, self.bottom, 1);
        } else if self.cur_row > 0 {
            self.cur_row -= 1;
        }
        self.touch();
    }

    // -- erasing --------------------------------------------------------------

    fn erase_cells(&mut self, row: usize, from: usize, to_exclusive: usize) {
        let cols = self.cols;
        let to = to_exclusive.min(cols);
        if from >= to {
            return;
        }
        let rows = self.active_mut();
        let Some(cells) = rows.get_mut(row) else {
            return;
        };
        // Erasing half of a wide character erases all of it.
        if from > 0 && cells[from].wide == WIDE_SPACER {
            cells[from - 1].reset();
        }
        if to < cols && cells[to].wide == WIDE_SPACER {
            cells[to].reset();
        }
        for cell in &mut cells[from..to] {
            cell.reset();
        }
    }

    fn erase_line(&mut self, mode: u16) {
        let (row, col) = (self.cur_row, self.cur_col);
        match mode {
            0 => self.erase_cells(row, col, self.cols),
            1 => self.erase_cells(row, 0, col + 1),
            _ => self.erase_cells(row, 0, self.cols),
        }
        self.pending_wrap = false;
    }

    fn erase_display(&mut self, mode: u16) {
        let (row, col) = (self.cur_row, self.cur_col);
        match mode {
            0 => {
                self.erase_cells(row, col, self.cols);
                for r in row + 1..self.rows {
                    self.erase_cells(r, 0, self.cols);
                }
            }
            1 => {
                for r in 0..row {
                    self.erase_cells(r, 0, self.cols);
                }
                self.erase_cells(row, 0, col + 1);
            }
            // 2 = all; 3 = the scrollback, which we do not keep — xterm leaves
            // the screen alone for 3, and so do we.
            2 => {
                for r in 0..self.rows {
                    self.erase_cells(r, 0, self.cols);
                }
            }
            _ => {}
        }
        self.pending_wrap = false;
    }

    // -- characters -----------------------------------------------------------

    fn insert_chars(&mut self, n: usize) {
        let (row, col, cols) = (self.cur_row, self.cur_col, self.cols);
        let n = n.min(cols - col);
        let rows = self.active_mut();
        let Some(cells) = rows.get_mut(row) else {
            return;
        };
        cells[col..].rotate_right(n);
        for cell in &mut cells[col..col + n] {
            cell.reset();
        }
        if let Some(last) = cells.last_mut() {
            if last.wide == WIDE_LEAD {
                last.reset();
            }
        }
        self.pending_wrap = false;
    }

    fn delete_chars(&mut self, n: usize) {
        let (row, col, cols) = (self.cur_row, self.cur_col, self.cols);
        let n = n.min(cols - col);
        let rows = self.active_mut();
        let Some(cells) = rows.get_mut(row) else {
            return;
        };
        if cells[col].wide == WIDE_SPACER && col > 0 {
            cells[col - 1].reset();
        }
        cells[col..].rotate_left(n);
        for cell in &mut cells[cols - n..] {
            cell.reset();
        }
        if cells[col].wide == WIDE_SPACER {
            cells[col].reset();
        }
        self.pending_wrap = false;
    }

    fn insert_lines(&mut self, n: usize) {
        if self.cur_row < self.top || self.cur_row > self.bottom {
            return;
        }
        self.scroll_down(self.cur_row, self.bottom, n);
        self.cur_col = 0;
        self.pending_wrap = false;
    }

    fn delete_lines(&mut self, n: usize) {
        if self.cur_row < self.top || self.cur_row > self.bottom {
            return;
        }
        self.scroll_up(self.cur_row, self.bottom, n);
        self.cur_col = 0;
        self.pending_wrap = false;
    }

    fn put_char(&mut self, c: char) {
        // Control characters never reach `print`; anything without a width is
        // treated as narrow rather than dropped.
        let width = c.width().unwrap_or(1);
        if width == 0 {
            self.add_combining(c);
            return;
        }
        let width = width.min(2);

        if self.pending_wrap && self.autowrap {
            self.cur_col = 0;
            self.index();
        }
        self.pending_wrap = false;

        // A wide character that does not fit on this row wraps first.
        if width == 2 && self.cur_col + 1 >= self.cols {
            if self.autowrap && self.cols >= 2 {
                let (row, col) = (self.cur_row, self.cur_col);
                self.erase_cells(row, col, self.cols);
                self.cur_col = 0;
                self.index();
            } else {
                // No room and no wrap: xterm drops it.
                return;
            }
        }

        if self.insert {
            self.insert_chars(width);
        }

        let (row, col, cols) = (self.cur_row, self.cur_col, self.cols);
        {
            let rows = self.active_mut();
            let cells = &mut rows[row];
            // Overwriting either half of a wide character clears the other.
            if cells[col].wide == WIDE_SPACER && col > 0 {
                cells[col - 1].reset();
            }
            if cells[col].wide == WIDE_LEAD && col + 1 < cols {
                cells[col + 1].reset();
            }
            cells[col] = Cell {
                ch: c,
                zw: None,
                wide: if width == 2 { WIDE_LEAD } else { WIDE_NONE },
            };
            if width == 2 {
                if col + 2 < cols && cells[col + 1].wide == WIDE_LEAD {
                    cells[col + 2].reset();
                }
                cells[col + 1] = Cell {
                    ch: ' ',
                    zw: None,
                    wide: WIDE_SPACER,
                };
            }
        }
        self.last_char = Some(c);

        let next = col + width;
        if next >= cols {
            // xterm: the cursor stays on the last column with the wrap flag;
            // only the next printable character wraps.
            self.cur_col = cols - 1;
            self.pending_wrap = self.autowrap;
        } else {
            self.cur_col = next;
        }
    }

    fn add_combining(&mut self, c: char) {
        // The mark belongs to the cell just written: the cursor cell itself
        // when the wrap flag is set, else the one before the cursor.
        let (row, mut col) = (self.cur_row, self.cur_col);
        if !self.pending_wrap {
            if col == 0 {
                return;
            }
            col -= 1;
        }
        let rows = self.active_mut();
        let cells = &mut rows[row];
        if cells[col].wide == WIDE_SPACER && col > 0 {
            col -= 1;
        }
        let cell = &mut cells[col];
        let mut zw = cell.zw.take().map(String::from).unwrap_or_default();
        if zw.chars().count() < 8 {
            zw.push(c);
        }
        cell.zw = Some(zw.into_boxed_str());
    }

    fn tab_forward(&mut self, n: usize) {
        for _ in 0..n.max(1) {
            let mut col = self.cur_col + 1;
            while col < self.cols - 1 && !self.tabs.get(col).copied().unwrap_or(false) {
                col += 1;
            }
            self.cur_col = col.min(self.cols - 1);
        }
        self.pending_wrap = false;
        self.touch();
    }

    fn tab_backward(&mut self, n: usize) {
        for _ in 0..n.max(1) {
            if self.cur_col == 0 {
                break;
            }
            let mut col = self.cur_col - 1;
            while col > 0 && !self.tabs.get(col).copied().unwrap_or(false) {
                col -= 1;
            }
            self.cur_col = col;
        }
        self.pending_wrap = false;
        self.touch();
    }

    // -- modes ----------------------------------------------------------------

    fn save_cursor(&self) -> SavedCursor {
        SavedCursor {
            row: self.cur_row,
            col: self.cur_col,
            pending_wrap: self.pending_wrap,
            origin: self.origin,
            autowrap: self.autowrap,
        }
    }

    fn restore_cursor(&mut self, saved: Option<SavedCursor>) {
        match saved {
            Some(s) => {
                self.cur_row = s.row;
                self.cur_col = s.col;
                self.pending_wrap = s.pending_wrap;
                self.origin = s.origin;
                self.autowrap = s.autowrap;
            }
            None => {
                self.cur_row = 0;
                self.cur_col = 0;
                self.pending_wrap = false;
                self.origin = false;
            }
        }
        self.clamp_cursor();
        self.touch();
    }

    fn decsc(&mut self) {
        let saved = Some(self.save_cursor());
        if self.alt_active {
            self.saved_alt = saved;
        } else {
            self.saved_main = saved;
        }
    }

    fn decrc(&mut self) {
        let saved = if self.alt_active {
            self.saved_alt
        } else {
            self.saved_main
        };
        self.restore_cursor(saved);
    }

    fn clear_screen(&mut self, alt: bool) {
        let screen = if alt { &mut self.alt } else { &mut self.main };
        for row in screen.iter_mut() {
            row.iter_mut().for_each(Cell::reset);
        }
        self.touch();
    }

    fn set_alt(&mut self, on: bool, clear_on_enter: bool, clear_on_leave: bool) {
        if on == self.alt_active {
            return;
        }
        if on {
            self.alt_active = true;
            if clear_on_enter {
                self.clear_screen(true);
            }
        } else {
            if clear_on_leave {
                self.clear_screen(true);
            }
            self.alt_active = false;
        }
        self.top = 0;
        self.bottom = self.rows - 1;
        self.pending_wrap = false;
        self.touch();
    }

    fn dec_mode(&mut self, mode: u16, on: bool) {
        match mode {
            1 => self.app_cursor = on,
            6 => {
                self.origin = on;
                self.move_to(0, 0);
            }
            7 => {
                self.autowrap = on;
                if !on {
                    self.pending_wrap = false;
                }
            }
            47 => self.set_alt(on, false, false),
            1047 => self.set_alt(on, false, true),
            1048 => {
                if on {
                    self.decsc();
                } else {
                    self.decrc();
                }
            }
            1049 => {
                if on {
                    if !self.alt_active {
                        self.saved_1049 = Some(self.save_cursor());
                        self.set_alt(true, true, false);
                    }
                } else if self.alt_active {
                    self.set_alt(false, false, false);
                    let saved = self.saved_1049.take();
                    self.restore_cursor(saved);
                }
            }
            2004 => self.bracketed_paste = on,
            // Cursor visibility, mouse, focus, synchronized output, … carry no
            // text.
            _ => {}
        }
    }

    fn full_reset(&mut self) {
        let (cols, rows) = (self.cols, self.rows);
        let title = self.title.take();
        let generation = self.generation;
        *self = Grid::new(cols, rows);
        // A reset does not retract a title the user already saw.
        self.title = title;
        self.generation = generation.wrapping_add(1);
    }

    fn set_title(&mut self, raw: &[u8]) {
        // Display only: invalid UTF-8 becomes U+FFFD, control characters go,
        // the length is capped.
        let mut text = String::new();
        let mut kept = 0usize;
        'chunks: for chunk in raw.utf8_chunks() {
            for c in chunk.valid().chars().chain(
                (!chunk.invalid().is_empty())
                    .then_some(char::REPLACEMENT_CHARACTER),
            ) {
                if kept >= TITLE_MAX_CHARS {
                    break 'chunks;
                }
                if !c.is_control() {
                    text.push(c);
                    kept += 1;
                }
            }
        }
        let text = text.trim().to_string();
        self.title = if text.is_empty() { None } else { Some(text) };
    }
}

// ---------------------------------------------------------------------------
// vte
// ---------------------------------------------------------------------------

fn param(params: &Params, index: usize, default: u16) -> u16 {
    params
        .iter()
        .nth(index)
        .and_then(|group| group.first().copied())
        .filter(|v| *v != 0)
        .unwrap_or(default)
}

fn raw_param(params: &Params, index: usize) -> Option<u16> {
    params.iter().nth(index).and_then(|group| group.first().copied())
}

impl Perform for Grid {
    fn print(&mut self, c: char) {
        self.put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x07 => self.bell = true,
            0x08 => {
                // From the wrap position xterm lands on the column before the
                // last, like any other backspace.
                self.cur_col = self.cur_col.saturating_sub(1);
                self.pending_wrap = false;
                self.touch();
            }
            0x09 => self.tab_forward(1),
            0x0a..=0x0c => self.index(),
            0x0d => {
                self.cur_col = 0;
                self.pending_wrap = false;
                self.touch();
            }
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore {
            return;
        }
        let marker = intermediates.first().copied();
        match (marker, action) {
            (Some(b'?'), 'h' | 'l') => {
                let on = action == 'h';
                for group in params.iter() {
                    if let Some(mode) = group.first() {
                        self.dec_mode(*mode, on);
                    }
                }
            }
            // Insert mode (IRM).
            (None, 'h' | 'l') => {
                for group in params.iter() {
                    if group.first() == Some(&4) {
                        self.insert = action == 'h';
                    }
                }
            }
            (None, 'A') => {
                let (lo, _) = self.vertical_bounds();
                let n = param(params, 0, 1) as usize;
                self.cur_row = self.cur_row.saturating_sub(n).max(lo.min(self.cur_row));
                self.pending_wrap = false;
                self.touch();
            }
            (None, 'B' | 'e') => {
                let (_, hi) = self.vertical_bounds();
                let n = param(params, 0, 1) as usize;
                self.cur_row = (self.cur_row + n).min(hi.max(self.cur_row));
                self.pending_wrap = false;
                self.touch();
            }
            (None, 'C' | 'a') => {
                let n = param(params, 0, 1) as usize;
                self.cur_col = (self.cur_col + n).min(self.cols - 1);
                self.pending_wrap = false;
                self.touch();
            }
            (None, 'D') => {
                let n = param(params, 0, 1) as usize;
                self.cur_col = self.cur_col.saturating_sub(n);
                self.pending_wrap = false;
                self.touch();
            }
            (None, 'E') => {
                let (_, hi) = self.vertical_bounds();
                let n = param(params, 0, 1) as usize;
                self.cur_row = (self.cur_row + n).min(hi.max(self.cur_row));
                self.cur_col = 0;
                self.pending_wrap = false;
                self.touch();
            }
            (None, 'F') => {
                let (lo, _) = self.vertical_bounds();
                let n = param(params, 0, 1) as usize;
                self.cur_row = self.cur_row.saturating_sub(n).max(lo.min(self.cur_row));
                self.cur_col = 0;
                self.pending_wrap = false;
                self.touch();
            }
            (None, 'G' | '`') => {
                self.cur_col = (param(params, 0, 1) as usize - 1).min(self.cols - 1);
                self.pending_wrap = false;
                self.touch();
            }
            (None, 'd') => {
                let row = param(params, 0, 1) as usize - 1;
                let col = self.cur_col;
                self.move_to(row, col);
            }
            (None, 'H' | 'f') => {
                let row = param(params, 0, 1) as usize - 1;
                let col = param(params, 1, 1) as usize - 1;
                self.move_to(row, col);
            }
            (None, 'J') => self.erase_display(raw_param(params, 0).unwrap_or(0)),
            (None, 'K') => self.erase_line(raw_param(params, 0).unwrap_or(0)),
            (None, 'X') => {
                let n = param(params, 0, 1) as usize;
                let (row, col) = (self.cur_row, self.cur_col);
                self.erase_cells(row, col, col + n);
                self.pending_wrap = false;
            }
            (None, '@') => self.insert_chars(param(params, 0, 1) as usize),
            (None, 'P') => self.delete_chars(param(params, 0, 1) as usize),
            (None, 'L') => self.insert_lines(param(params, 0, 1) as usize),
            (None, 'M') => self.delete_lines(param(params, 0, 1) as usize),
            (None, 'S') => {
                let n = param(params, 0, 1) as usize;
                self.scroll_up(self.top, self.bottom, n);
            }
            (None, 'T') => {
                let n = param(params, 0, 1) as usize;
                self.scroll_down(self.top, self.bottom, n);
            }
            (None, 'r') => {
                let top = param(params, 0, 1) as usize - 1;
                let bottom = (param(params, 1, self.rows as u16) as usize).min(self.rows) - 1;
                if top < bottom {
                    self.top = top;
                    self.bottom = bottom;
                } else {
                    self.top = 0;
                    self.bottom = self.rows - 1;
                }
                self.move_to(0, 0);
            }
            (None, 's') if params.is_empty() => self.decsc(),
            (None, 'u') if params.is_empty() => self.decrc(),
            (None, 'I') => self.tab_forward(param(params, 0, 1) as usize),
            (None, 'Z') => self.tab_backward(param(params, 0, 1) as usize),
            (None, 'g') => match raw_param(params, 0).unwrap_or(0) {
                0 => {
                    if let Some(stop) = self.tabs.get_mut(self.cur_col) {
                        *stop = false;
                    }
                }
                3 => self.tabs.iter_mut().for_each(|t| *t = false),
                _ => {}
            },
            (None, 'b') => {
                if let Some(c) = self.last_char {
                    let n = (param(params, 0, 1) as usize).min(self.cols * self.rows);
                    for _ in 0..n {
                        self.put_char(c);
                    }
                }
            }
            // DA1: `CSI c` / `CSI 0 c`.
            (None, 'c') if raw_param(params, 0).unwrap_or(0) == 0 => {
                self.queries.push(TermQuery::PrimaryAttributes);
            }
            // DSR: `CSI 6 n` (cursor position).
            (None, 'n') if raw_param(params, 0) == Some(6) => {
                let row = if self.origin {
                    self.cur_row.saturating_sub(self.top)
                } else {
                    self.cur_row
                };
                self.queries.push(TermQuery::CursorPosition {
                    row: (row + 1).min(u16::MAX as usize) as u16,
                    col: (self.cur_col + 1).min(u16::MAX as usize) as u16,
                });
            }
            // SGR, DA2/DA3, XTVERSION, kitty keyboard, window ops, …: no text,
            // and no reply from the core (§6.2.6).
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        if ignore || !intermediates.is_empty() {
            // Charset designations (`ESC ( B`), DECALN (`ESC # 8`), …
            return;
        }
        match byte {
            b'7' => self.decsc(),
            b'8' => self.decrc(),
            b'D' => self.index(),
            b'E' => {
                self.cur_col = 0;
                self.index();
            }
            b'M' => self.reverse_index(),
            b'H' => {
                if let Some(stop) = self.tabs.get_mut(self.cur_col) {
                    *stop = true;
                }
            }
            b'c' => self.full_reset(),
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        // `OSC … BEL` is a terminator, never a bell. OSC 52 (clipboard) and
        // OSC 8 (hyperlinks) are ignored here; links go through `link_open`.
        match params.first().copied() {
            Some(b"0") | Some(b"2") => {
                // A title may contain `;` — vte splits on it.
                let mut raw: Vec<u8> = Vec::new();
                for (i, part) in params.iter().skip(1).enumerate() {
                    if i > 0 {
                        raw.push(b';');
                    }
                    raw.extend_from_slice(part);
                }
                self.set_title(&raw);
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(cols: u16, rows: u16, bytes: &[u8]) -> AnsiTail {
        let mut tail = AnsiTail::with_size(cols, rows);
        tail.feed(bytes);
        tail
    }

    fn tail_of(bytes: &[u8]) -> AnsiTail {
        grid(120, 40, bytes)
    }

    #[test]
    fn csi_and_osc_leave_no_trace_in_the_text() {
        let tail = tail_of(
            b"\x1b[1;32mgreen\x1b[0m plain\r\n\x1b]0;a window title\x07after\r\n\x1bP+q544e\x1b\\dcs gone\r\n",
        );
        assert_eq!(
            tail.visible_lines(8),
            vec!["green plain".to_string(), "after".to_string(), "dcs gone".to_string()]
        );
        assert_eq!(tail.title(), Some("a window title"));
    }

    #[test]
    fn a_carriage_return_rewrites_the_line_the_way_a_spinner_does() {
        let tail = tail_of(b"working 1s\rworking 2s\rworking 12s");
        assert_eq!(tail.visible_lines(4), vec!["working 12s".to_string()]);
    }

    #[test]
    fn erase_in_line_clears_what_the_cursor_left_behind() {
        let tail = tail_of(b"a very long status line\r\x1b[2Kdone");
        assert_eq!(tail.visible_lines(4), vec!["done".to_string()]);
        let tail = tail_of(b"abcdefgh\rWXYZ\x1b[0K");
        assert_eq!(tail.visible_lines(4), vec!["WXYZ".to_string()]);
        let tail = tail_of(b"abcdefgh\x1b[4G\x1b[1K");
        assert_eq!(tail.visible_lines(4), vec!["    efgh".to_string()]);
    }

    #[test]
    fn a_full_repaint_replaces_the_screen() {
        let tail = tail_of(b"one\r\ntwo\r\n\x1b[2J\x1b[Hthree");
        assert_eq!(tail.visible_lines(8), vec!["three".to_string()]);
    }

    #[test]
    fn an_ink_style_redraw_replaces_the_frame_instead_of_appending_it() {
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
        let mut tail = AnsiTail::with_size(40, 6);
        tail.feed(b"\x1b[2J");
        tail.feed(b"\x1b[1;1HAllow command?");
        tail.feed(b"\x1b[3;3H1. Yes");
        tail.feed(b"\x1b[4;3H2. No");
        assert_eq!(
            tail.screen_lines(),
            vec![
                "Allow command?".to_string(),
                String::new(),
                "  1. Yes".to_string(),
                "  2. No".to_string(),
                String::new(),
                String::new(),
            ]
        );
    }

    #[test]
    fn a_saved_cursor_comes_back_to_the_same_cell() {
        let tail = tail_of(b"prompt: \x1b7\r\nnoise\x1b8done");
        assert_eq!(
            tail.visible_lines(4),
            vec!["prompt: done".to_string(), "noise".to_string()]
        );
    }

    #[test]
    fn output_scrolls_and_the_last_rows_are_what_comes_out() {
        let mut body = Vec::new();
        for i in 0..40 {
            body.extend_from_slice(format!("line {i}\r\n").as_bytes());
            body.extend_from_slice(b"   \r\n");
        }
        body.extend_from_slice("│ > │".as_bytes());
        let tail = grid(80, 24, &body);
        let lines = tail.visible_lines(VISIBLE_LINES);
        assert_eq!(lines.len(), VISIBLE_LINES);
        assert_eq!(lines.last().unwrap(), "│ > │");
        assert_eq!(lines.first().unwrap(), "line 33");
        assert_eq!(tail.screen_lines().len(), 24, "the grid has exactly its rows");
    }

    #[test]
    fn the_alternate_screen_keeps_the_main_screen_and_1049_restores_the_cursor() {
        let mut tail = AnsiTail::with_size(40, 6);
        tail.feed(b"shell text\r\n$ ");
        let before = tail.cursor();
        tail.feed(b"\x1b[?1049h");
        assert!(tail.alt_screen());
        assert!(tail.visible_lines(8).is_empty(), "the alternate buffer starts blank");
        tail.feed(b"\x1b[3;1Htui frame");
        assert_eq!(tail.visible_lines(8), vec!["tui frame".to_string()]);
        tail.feed(b"\x1b[?1049l");
        assert!(!tail.alt_screen());
        assert_eq!(
            tail.visible_lines(8),
            vec!["shell text".to_string(), "$".to_string()],
            "the main screen is intact after the alternate screen closes"
        );
        assert_eq!(tail.cursor(), before, "1049 restores the cursor it saved");

        let mut tail = AnsiTail::new();
        tail.feed(b"\x1b[?47h");
        assert!(tail.alt_screen());
        tail.feed(b"\x1b[?47l");
        assert!(!tail.alt_screen());
    }

    /// T-pty-pendingWrap (unit half): a row filled to the right margin and
    /// followed by `CR LF` must not produce an empty row. Without xterm's
    /// pending-wrap rule the cursor would already be on the next row after the
    /// last column, and `CR LF` would skip one more.
    #[test]
    fn pending_wrap_keeps_a_full_row_and_crlf_on_one_line() {
        let rule = "─".repeat(10);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"\x1b[1;1H");
        bytes.extend_from_slice(rule.as_bytes());
        bytes.extend_from_slice(b"\r\n");
        bytes.extend_from_slice("❯ ready".as_bytes());
        bytes.extend_from_slice(b"\r\n");
        bytes.extend_from_slice(rule.as_bytes());
        bytes.extend_from_slice(b"\r\nfooter");
        let tail = grid(10, 6, &bytes);
        assert_eq!(
            tail.screen_lines(),
            vec![
                rule.clone(),
                "❯ ready".to_string(),
                rule.clone(),
                "footer".to_string(),
                String::new(),
                String::new(),
            ]
        );
    }

    #[test]
    fn pending_wrap_wraps_only_on_the_next_printable_character() {
        let tail = grid(5, 3, b"abcdeX");
        assert_eq!(tail.screen_lines(), vec!["abcde", "X", ""]);
        // Any cursor movement cancels the pending wrap.
        let tail = grid(5, 3, b"abcde\x1b[1GX");
        assert_eq!(tail.screen_lines(), vec!["Xbcde", "", ""]);
        // A backspace from the wrap position moves back one column.
        let tail = grid(5, 3, b"abcde\x08X");
        assert_eq!(tail.screen_lines(), vec!["abcXe", "", ""]);
        // With autowrap off the last column is overwritten.
        let tail = grid(5, 3, b"\x1b[?7labcdeXY");
        assert_eq!(tail.screen_lines(), vec!["abcdY", "", ""]);
        // A full last row followed by a printable character scrolls once.
        let tail = grid(3, 2, b"\x1b[2;1HxyzQ");
        assert_eq!(tail.screen_lines(), vec!["xyz", "Q"]);
    }

    #[test]
    fn cell_diff_moves_land_where_xterm_puts_them() {
        // The 2.1.307 renderer: CUP, then CUD/CHA/CUF to patch cells.
        let tail = grid(
            20,
            5,
            b"\x1b[2J\x1b[H\x1b[2;3Hhello\x1b[2B\x1b[1Gworld\x1b[1A\x1b[4Cmid",
        );
        assert_eq!(tail.screen_lines(), vec!["", "  hello", "         mid", "world", ""]);
    }

    #[test]
    fn scroll_regions_scroll_only_inside_their_margins() {
        let mut tail = AnsiTail::with_size(10, 5);
        tail.feed(b"\x1b[1;1Htop\x1b[5;1Hbottom");
        tail.feed(b"\x1b[2;4r\x1b[2;1Ha\r\nb\r\nc\r\nd");
        assert_eq!(tail.screen_lines(), vec!["top", "b", "c", "d", "bottom"]);
        tail.feed(b"\x1b[2;1H\x1bM");
        assert_eq!(tail.screen_lines(), vec!["top", "", "b", "c", "bottom"]);
        tail.feed(b"\x1b[3;1H\x1b[1L");
        assert_eq!(tail.screen_lines(), vec!["top", "", "", "b", "bottom"]);
        tail.feed(b"\x1b[2;1H\x1b[2M");
        assert_eq!(tail.screen_lines(), vec!["top", "b", "", "", "bottom"]);
    }

    #[test]
    fn wide_characters_take_two_cells_and_combining_marks_join_their_base() {
        let tail = grid(10, 2, "한글ab".as_bytes());
        assert_eq!(tail.screen_lines()[0], "한글ab");
        assert_eq!(tail.cursor(), (0, 6));
        // A wide character that does not fit wraps whole.
        let tail = grid(5, 2, "abcd한".as_bytes());
        assert_eq!(tail.screen_lines(), vec!["abcd", "한"]);
        // e + U+0301 stays one cell.
        let tail = grid(10, 2, "e\u{301}x".as_bytes());
        assert_eq!(tail.screen_lines()[0], "e\u{301}x");
        assert_eq!(tail.cursor(), (0, 2));
        // Overwriting half of a wide character clears the other half.
        let tail = grid(10, 2, "한\x1b[1;2Hx".as_bytes());
        assert_eq!(tail.screen_lines()[0], " x");
    }

    #[test]
    fn insert_and_delete_characters_shift_the_row() {
        let tail = grid(10, 1, b"abcdef\x1b[1;3H\x1b[2@");
        assert_eq!(tail.screen_lines()[0], "ab  cdef");
        let tail = grid(10, 1, b"abcdef\x1b[1;3H\x1b[2P");
        assert_eq!(tail.screen_lines()[0], "abef");
        let tail = grid(10, 1, b"abcdef\x1b[1;2H\x1b[3X");
        assert_eq!(tail.screen_lines()[0], "a   ef");
        let tail = grid(10, 1, b"abc\x1b[1;2H\x1b[4hX\x1b[4l");
        assert_eq!(tail.screen_lines()[0], "aXbc");
    }

    #[test]
    fn terminal_queries_are_captured_with_the_cursor_at_the_query() {
        let mut tail = AnsiTail::with_size(80, 24);
        tail.feed(b"\x1b[3;5H\x1b[c\x1b[6n\x1b[>0q\x1b[?u\x1b[>c\x1b[10;1H\x1b[0c");
        assert_eq!(
            tail.take_queries(),
            vec![
                TermQuery::PrimaryAttributes,
                TermQuery::CursorPosition { row: 3, col: 5 },
                TermQuery::PrimaryAttributes,
            ],
            "only DA1 and DSR are answered; XTVERSION, kitty keyboard and DA2 are not"
        );
        assert!(tail.take_queries().is_empty(), "taking clears them");
        assert_eq!(TermQuery::PrimaryAttributes.reply(), b"\x1b[?1;2c".to_vec());
        assert_eq!(TermQuery::CursorPosition { row: 3, col: 5 }.reply(), b"\x1b[3;5R".to_vec());
    }

    #[test]
    fn osc_titles_are_sanitized_and_osc_52_is_ignored() {
        let mut tail = AnsiTail::new();
        tail.feed("\x1b]0;✳ Claude Code\x07".as_bytes());
        assert_eq!(tail.title(), Some("✳ Claude Code"));
        tail.feed(b"\x1b]2;a;b\x1b\\");
        assert_eq!(tail.title(), Some("a;b"), "a title keeps its semicolons");
        tail.feed(b"\x1b]52;c;aGVsbG8=\x07");
        assert_eq!(tail.title(), Some("a;b"), "a clipboard write is not a title");
        let long = "x".repeat(1000);
        tail.feed(format!("\x1b]0;{long}\x07").as_bytes());
        assert_eq!(tail.title().map(|t| t.chars().count()), Some(TITLE_MAX_CHARS));
    }

    #[test]
    fn bel_latches_once_and_an_osc_terminator_is_not_a_bell() {
        let mut tail = AnsiTail::new();
        assert!(!tail.take_bell());
        tail.feed(b"done\x07\r\n");
        assert!(tail.take_bell());
        assert!(!tail.take_bell());
        tail.feed(b"\x1b]0;title\x07text");
        assert!(!tail.take_bell());
    }

    #[test]
    fn a_chunk_boundary_inside_a_sequence_or_a_codepoint_changes_nothing() {
        let whole = b"\xed\x95\x9c\xea\xb8\x80 \x1b[1;31mred\x1b[0m\r\ndone \xf0\x9f\x98\x80\x1b[3;2Hx";
        let reference = tail_of(whole).screen_lines();
        for split in 1..whole.len() {
            let mut tail = AnsiTail::new();
            tail.feed(&whole[..split]);
            tail.feed(&whole[split..]);
            assert_eq!(tail.screen_lines(), reference, "a split at byte {split} changed the grid");
        }
        assert_eq!(reference[0], "한글 red");
        assert_eq!(reference[1], "done 😀");
        assert_eq!(reference[2], " x");
    }

    #[test]
    fn invalid_utf8_does_not_panic_and_does_not_stop_the_parse() {
        let tail = tail_of(b"before \xff\xfe after\r\n");
        let lines = tail.visible_lines(4);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with("before "));
        assert!(lines[0].ends_with(" after"));
    }

    #[test]
    fn a_flood_of_newlines_stays_bounded() {
        // Functional only (A10.7): the grid stays its size and keeps the newest
        // rows; the throughput budget is a G5 benchmark.
        let mut tail = AnsiTail::with_size(80, 24);
        let chunk = b"y\r\n".repeat(32 * 1024);
        for _ in 0..8 {
            tail.feed(&chunk);
        }
        assert_eq!(tail.screen_lines().len(), 24);
        assert_eq!(tail.visible_lines(3), vec!["y".to_string(); 3]);
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
    fn an_empty_grid_answers_with_an_empty_vec_not_a_blank_line() {
        let tail = AnsiTail::new();
        assert!(tail.is_empty());
        assert!(tail.visible_lines(8).is_empty());
        assert!(tail_of(b"   \r\n\t\r\n").visible_lines(8).is_empty());
    }

    #[test]
    fn resize_keeps_the_cursor_row_on_screen_and_clamps_the_cursor() {
        let mut tail = AnsiTail::with_size(10, 4);
        tail.feed(b"a\r\nb\r\nc\r\nd");
        tail.resize(6, 2);
        assert_eq!(tail.screen_lines(), vec!["c", "d"]);
        assert_eq!(tail.cursor(), (1, 1));
        tail.resize(8, 4);
        assert_eq!(tail.screen_lines(), vec!["c", "d", "", ""]);
        assert_eq!(tail.size(), (8, 4));
    }

    #[test]
    fn rep_repeats_the_last_character_and_ris_resets() {
        let tail = grid(10, 2, b"-\x1b[4b");
        assert_eq!(tail.screen_lines()[0], "-----");
        let tail = grid(10, 2, b"junk\x1bc");
        assert!(tail.is_empty());
    }

    #[test]
    fn the_generation_moves_on_every_visible_change() {
        let mut tail = AnsiTail::new();
        let g0 = tail.generation();
        tail.feed(b"x");
        let g1 = tail.generation();
        assert!(g1 > g0);
        tail.feed(b"\x1b[1m");
        assert_eq!(tail.generation(), g1, "an SGR changes no cell");
    }
}
