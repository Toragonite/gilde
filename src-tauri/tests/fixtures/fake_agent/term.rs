//! Terminal plumbing for the TUI modes of `fake_agent` (`tui.rs`, `team.rs`).
//!
//! What lives here:
//!
//!   * the window size (`TIOCGWINSZ` on fd 1, 120×40 when stdout is not a tty)
//!     and the `SIGWINCH` / `SIGTERM` / `SIGHUP` flags;
//!   * raw mode on a tty stdin, restored on every orderly exit;
//!   * the stdin reader that appends **every raw byte** to
//!     `$FAKE_AGENT_INPUT_LOG` (tests assert exact key sequences from it);
//!   * the key decoder (arrows, a lone `ESC` after 50 ms, Enter, Tab,
//!     bracketed paste, backspace, UTF-8 text);
//!   * [`Frame`], whose renderer deliberately exercises a terminal emulator:
//!     full-width rows rely on xterm's pending-wrap rule, indentation is
//!     cursor-forward instead of spaces, and one row per frame is reached by a
//!     relative move. Gilde's grid emulator is tested end-to-end against it.
//!   * [`run`], the event loop every TUI mode shares.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::{Duration, Instant};

use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};
use nix::sys::termios::{cfmakeraw, tcgetattr, tcsetattr, SetArg, Termios};

/// Size used when stdout is not a terminal.
pub const DEFAULT_ROWS: u16 = 40;
pub const DEFAULT_COLS: u16 = 120;

/// How long a lone `ESC` waits for a follow-up byte before it is the Esc key.
pub const ESC_WAIT: Duration = Duration::from_millis(50);

/// Upper bound on one `poll(2)` wait, so a flag set by a signal that raced the
/// poll is noticed even if `EINTR` was missed.
const MAX_WAIT: Duration = Duration::from_millis(250);

/// What an alternate-screen TUI writes first (R6 §4 renderer facts): the
/// alternate screen, bracketed paste, focus events, mouse modes, `?2031`.
pub const ALT_ENTER: &str =
    "\x1b[?1049h\x1b[?2004h\x1b[?1004h\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h\x1b[?2031h";
/// The reverse, ending with `?1049l` (leave the alternate screen).
pub const ALT_LEAVE: &str =
    "\x1b[?2031l\x1b[?1006l\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?1004l\x1b[?2004l\x1b[?1049l";

// ---------------------------------------------------------------------------
// signals
// ---------------------------------------------------------------------------

static WINCH: AtomicBool = AtomicBool::new(false);
/// The terminating signal received (`SIGTERM` or `SIGHUP`), 0 if none.
static TERM_SIG: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_winch(_sig: libc::c_int) {
    WINCH.store(true, Ordering::SeqCst);
}

extern "C" fn on_term(sig: libc::c_int) {
    TERM_SIG.store(sig, Ordering::SeqCst);
}

/// Install the `SIGWINCH`, `SIGTERM` and `SIGHUP` handlers. No `SA_RESTART`,
/// so a blocked `poll(2)` returns `EINTR` and the loop reacts at once. The
/// handlers only store into atomics (async-signal-safe).
pub fn install_signal_handlers() {
    let winch = SigAction::new(
        SigHandler::Handler(on_winch),
        SaFlags::empty(),
        SigSet::empty(),
    );
    let term = SigAction::new(
        SigHandler::Handler(on_term),
        SaFlags::empty(),
        SigSet::empty(),
    );
    // SAFETY: both handlers perform a single atomic store.
    unsafe {
        let _ = sigaction(Signal::SIGWINCH, &winch);
        let _ = sigaction(Signal::SIGTERM, &term);
        let _ = sigaction(Signal::SIGHUP, &term);
    }
}

/// The terminating signal received so far, if any.
pub fn term_signal() -> Option<i32> {
    match TERM_SIG.load(Ordering::SeqCst) {
        0 => None,
        s => Some(s),
    }
}

/// Whether a `SIGWINCH` arrived since the last call (clears the flag).
pub fn take_winch() -> bool {
    WINCH.swap(false, Ordering::SeqCst)
}

/// Die of `sig` with its default disposition, so the exit status is the one an
/// unhandled signal gives (cleanup has already run).
pub fn reraise(sig: i32) -> ! {
    // SAFETY: plain libc calls on our own process.
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
    std::process::exit(128 + sig)
}

// ---------------------------------------------------------------------------
// size, raw mode, output
// ---------------------------------------------------------------------------

/// `(rows, cols)` of the terminal on fd 1, or 40×120 when it is not one.
pub fn window_size() -> (u16, u16) {
    // SAFETY: TIOCGWINSZ fills a plain `winsize` struct we own.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) };
    if rc == 0 && ws.ws_row > 0 && ws.ws_col > 0 {
        (ws.ws_row, ws.ws_col)
    } else {
        (DEFAULT_ROWS, DEFAULT_COLS)
    }
}

/// Raw mode on a tty stdin; a no-op otherwise. Restored by [`RawMode::restore`]
/// or on drop.
pub struct RawMode {
    saved: Option<Termios>,
}

impl RawMode {
    pub fn enable() -> RawMode {
        // SAFETY: isatty on fd 0 has no side effects.
        if unsafe { libc::isatty(0) } != 1 {
            return RawMode { saved: None };
        }
        let stdin = io::stdin();
        match tcgetattr(&stdin) {
            Ok(saved) => {
                let mut raw = saved.clone();
                cfmakeraw(&mut raw);
                // TCSANOW, never TCSAFLUSH: keys a test typed early must stay
                // in the input queue.
                let _ = tcsetattr(&stdin, SetArg::TCSANOW, &raw);
                RawMode { saved: Some(saved) }
            }
            Err(_) => RawMode { saved: None },
        }
    }

    pub fn restore(&mut self) {
        if let Some(saved) = self.saved.take() {
            let _ = tcsetattr(io::stdin(), SetArg::TCSANOW, &saved);
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Write `bytes` to fd 1 in one go and flush (raw fd, no `LineWriter`).
pub fn out(bytes: &[u8]) {
    let mut o = crate::raw_stdout();
    let _ = o.write_all(bytes);
    let _ = o.flush();
}

/// [`out`] for text.
pub fn out_str(s: &str) {
    out(s.as_bytes());
}

/// Print one line with an explicit `\r\n` (raw mode turns off `ONLCR`).
pub fn line(s: &str) {
    out_str(&format!("{s}\r\n"));
}

// ---------------------------------------------------------------------------
// input
// ---------------------------------------------------------------------------

/// stdin with the input log. Reads never block longer than asked.
pub struct Input {
    log: Option<File>,
    open: bool,
}

impl Input {
    pub fn new() -> Input {
        let log = std::env::var_os("FAKE_AGENT_INPUT_LOG")
            .and_then(|path| OpenOptions::new().create(true).append(true).open(path).ok());
        Input { log, open: true }
    }

    /// Wait up to `timeout` (capped) for stdin and return what one `read(2)`
    /// gives — possibly nothing (timeout, signal, EOF). Every byte read is
    /// appended to the input log and flushed before it is returned.
    pub fn read_timeout(&mut self, timeout: Duration) -> Vec<u8> {
        let timeout = timeout.min(MAX_WAIT);
        if !self.open {
            // EOF on stdin: keep running (timers, signals), read nothing.
            std::thread::sleep(timeout);
            return Vec::new();
        }
        let mut pfd = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
        // SAFETY: one pollfd we own.
        let rc = unsafe { libc::poll(&mut pfd, 1, ms) };
        if rc <= 0 {
            return Vec::new();
        }
        let mut buf = [0u8; 4096];
        // SAFETY: reading into a stack buffer of the stated length.
        let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
        if n > 0 {
            let bytes = buf[..n as usize].to_vec();
            self.log(&bytes);
            return bytes;
        }
        if n == 0 {
            self.open = false;
        } else {
            let err = io::Error::last_os_error();
            if !matches!(err.raw_os_error(), Some(libc::EINTR) | Some(libc::EAGAIN)) {
                // EIO: the terminal went away.
                self.open = false;
            }
        }
        Vec::new()
    }

    /// Read everything stdin has right now without waiting.
    pub fn drain_now(&mut self) -> Vec<u8> {
        let mut all = Vec::new();
        loop {
            let chunk = self.read_timeout(Duration::ZERO);
            if chunk.is_empty() {
                return all;
            }
            all.extend_from_slice(&chunk);
        }
    }

    fn log(&mut self, bytes: &[u8]) {
        if let Some(f) = self.log.as_mut() {
            let _ = f.write_all(bytes);
            let _ = f.flush();
        }
    }
}

// ---------------------------------------------------------------------------
// keys
// ---------------------------------------------------------------------------

/// A decoded key. Digits and letters arrive as [`Key::Char`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    Esc,
    Enter,
    Tab,
    Backspace,
    Char(char),
    /// Bracketed-paste content (never a submit by itself).
    Paste(String),
}

enum Decoded {
    Key(Key, usize),
    /// Bytes that mean nothing to us (focus/mouse reports, `\n`, controls).
    Skip(usize),
    /// A prefix of something longer. `times_out` = it may be flushed after
    /// [`ESC_WAIT`] (everything except an open bracketed paste).
    Incomplete {
        times_out: bool,
    },
}

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn decode_one(buf: &[u8]) -> Decoded {
    let b = buf[0];
    match b {
        0x1b => {
            if buf.len() == 1 {
                return Decoded::Incomplete { times_out: true };
            }
            match buf[1] {
                b'[' => {
                    if buf.starts_with(PASTE_START) {
                        let body = &buf[PASTE_START.len()..];
                        return match find(body, PASTE_END) {
                            Some(end) => Decoded::Key(
                                Key::Paste(String::from_utf8_lossy(&body[..end]).into_owned()),
                                PASTE_START.len() + end + PASTE_END.len(),
                            ),
                            None => Decoded::Incomplete { times_out: false },
                        };
                    }
                    // CSI: parameter and intermediate bytes, then a final byte.
                    let mut i = 2;
                    while i < buf.len() && (0x20..=0x3f).contains(&buf[i]) {
                        i += 1;
                    }
                    if i >= buf.len() {
                        // `ESC [ 2 0` may still become a paste start.
                        return Decoded::Incomplete { times_out: true };
                    }
                    let fin = buf[i];
                    if !(0x40..=0x7e).contains(&fin) {
                        return Decoded::Skip(i);
                    }
                    let params = &buf[2..i];
                    let key = match (params, fin) {
                        (b"" | b"1", b'A') => Some(Key::Up),
                        (b"" | b"1", b'B') => Some(Key::Down),
                        (b"" | b"1", b'C') => Some(Key::Right),
                        (b"" | b"1", b'D') => Some(Key::Left),
                        _ => None,
                    };
                    match key {
                        Some(k) => Decoded::Key(k, i + 1),
                        None => Decoded::Skip(i + 1),
                    }
                }
                b'O' => {
                    if buf.len() < 3 {
                        return Decoded::Incomplete { times_out: true };
                    }
                    match buf[2] {
                        b'A' => Decoded::Key(Key::Up, 3),
                        b'B' => Decoded::Key(Key::Down, 3),
                        b'C' => Decoded::Key(Key::Right, 3),
                        b'D' => Decoded::Key(Key::Left, 3),
                        _ => Decoded::Skip(3),
                    }
                }
                // ESC followed by anything else: the Esc key, then that byte.
                _ => Decoded::Key(Key::Esc, 1),
            }
        }
        b'\r' => Decoded::Key(Key::Enter, 1),
        b'\t' => Decoded::Key(Key::Tab, 1),
        0x7f | 0x08 => Decoded::Key(Key::Backspace, 1),
        0x00..=0x1f => Decoded::Skip(1),
        _ => {
            let width = match b {
                0x20..=0x7e => 1,
                0xc2..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf4 => 4,
                _ => return Decoded::Skip(1),
            };
            if buf.len() < width {
                return Decoded::Incomplete { times_out: true };
            }
            match std::str::from_utf8(&buf[..width]) {
                Ok(s) => match s.chars().next() {
                    Some(c) => Decoded::Key(Key::Char(c), width),
                    None => Decoded::Skip(width),
                },
                Err(_) => Decoded::Skip(1),
            }
        }
    }
}

/// Turns the raw byte stream into [`Key`]s.
#[derive(Default)]
pub struct Decoder {
    buf: Vec<u8>,
    /// When the current incomplete prefix was first seen.
    waiting_since: Option<Instant>,
}

impl Decoder {
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Every key that is complete at `now`. A lone `ESC` (or another prefix
    /// that may time out) that has waited [`ESC_WAIT`] is flushed: `ESC`
    /// becomes [`Key::Esc`], any other dangling byte is dropped.
    pub fn drain(&mut self, now: Instant) -> Vec<Key> {
        let mut keys = Vec::new();
        while !self.buf.is_empty() {
            match decode_one(&self.buf) {
                Decoded::Key(k, n) => {
                    keys.push(k);
                    self.buf.drain(..n);
                    self.waiting_since = None;
                }
                Decoded::Skip(n) => {
                    self.buf.drain(..n);
                    self.waiting_since = None;
                }
                Decoded::Incomplete { times_out } => {
                    let since = *self.waiting_since.get_or_insert(now);
                    if times_out && now.duration_since(since) >= ESC_WAIT {
                        if self.buf[0] == 0x1b {
                            keys.push(Key::Esc);
                        }
                        self.buf.drain(..1);
                        self.waiting_since = None;
                        continue;
                    }
                    break;
                }
            }
        }
        if self.buf.is_empty() {
            self.waiting_since = None;
        }
        keys
    }

    /// When [`Decoder::drain`] must run again to flush a pending prefix.
    pub fn deadline(&self) -> Option<Instant> {
        self.waiting_since.map(|s| s + ESC_WAIT)
    }
}

/// Paste or typed text as one composer line: line breaks and tabs become
/// spaces, other control characters are dropped.
pub fn sanitize(text: &str) -> String {
    let unified = text.replace("\r\n", "\n");
    unified
        .chars()
        .filter_map(|c| match c {
            '\r' | '\n' | '\t' => Some(' '),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// frames
// ---------------------------------------------------------------------------

/// One full screen: 1-based row → text. Rows outside the screen are dropped
/// at render time; text is cut at the screen width. Every character the TUI
/// modes draw is one column wide, so width = `chars().count()`.
#[derive(Default, Debug, Clone)]
pub struct Frame {
    rows: BTreeMap<i32, String>,
}

impl Frame {
    pub fn new() -> Frame {
        Frame::default()
    }

    /// Set row `row` (1-based). Rows < 1 are ignored.
    pub fn set(&mut self, row: i32, text: impl Into<String>) {
        if row >= 1 {
            self.rows.insert(row, text.into());
        }
    }

    /// Place `lines` so that the last one lands on `last_row`. Empty strings
    /// are blank rows (left cleared, not written). Returns the first row.
    pub fn place_bottom(&mut self, lines: &[String], last_row: i32) -> i32 {
        let top = last_row - lines.len() as i32 + 1;
        for (i, l) in lines.iter().enumerate() {
            if !l.is_empty() {
                self.set(top + i as i32, l.clone());
            }
        }
        top
    }

    /// The bytes of this frame on a `rows`×`cols` screen:
    ///
    ///   * `ESC[2J ESC[H`, then each row positioned with `ESC[<row>;1H`;
    ///   * a row exactly `cols` wide (rules, box edges) is followed by `\r\n`
    ///     and **no** `ESC[K`, and the next row is written with no cursor
    ///     positioning at all — it lands on the right row only under xterm's
    ///     pending-wrap rule (an emulator that wraps eagerly puts it one row
    ///     lower). Never on the last row (that would scroll);
    ///   * leading spaces are a cursor-forward `ESC[<n>C`, not spaces;
    ///   * the first row that directly follows a written row is reached with
    ///     `ESC[1B ESC[1G` instead of absolute positioning (one per frame; if
    ///     the frame has no such pair, a cleared blank row is written above
    ///     one row to create it);
    ///   * every other row ends with `ESC[K`.
    pub fn render(&self, rows: u16, cols: u16) -> Vec<u8> {
        let max_row = i32::from(rows);
        let cols = usize::from(cols);
        let mut rows_map: BTreeMap<i32, String> = self
            .rows
            .iter()
            .filter(|(r, _)| **r <= max_row)
            .map(|(r, t)| (*r, t.chars().take(cols).collect::<String>()))
            .collect();
        let width = |t: &String| t.chars().count();
        let has_pair = rows_map.iter().any(|(r, _)| {
            rows_map
                .get(&(r - 1))
                .is_some_and(|prev| width(prev) < cols)
        });
        if !has_pair {
            let candidate = rows_map
                .keys()
                .copied()
                .find(|r| *r >= 2 && !rows_map.contains_key(&(r - 1)));
            if let Some(r) = candidate {
                rows_map.insert(r - 1, String::new());
            }
        }

        let mut out = String::from("\x1b[2J\x1b[H");
        let mut cursor_at_col1_of: Option<i32> = None;
        let mut prev: Option<(i32, usize)> = None;
        let mut relative_done = false;
        for (&r, text) in &rows_map {
            let w = width(text);
            if cursor_at_col1_of == Some(r) {
                // Pending wrap + `\r\n` left the cursor at column 1 of `r`.
            } else if !relative_done && matches!(prev, Some((pr, pw)) if pr + 1 == r && pw < cols) {
                out.push_str("\x1b[1B\x1b[1G");
                relative_done = true;
            } else {
                let _ = write!(out, "\x1b[{r};1H");
            }
            cursor_at_col1_of = None;
            let lead = text.chars().take_while(|c| *c == ' ').count();
            if lead > 0 && lead < w {
                let _ = write!(out, "\x1b[{lead}C");
                out.push_str(&text[lead..]);
            } else {
                out.push_str(text);
            }
            if w >= cols {
                if r < max_row {
                    out.push_str("\r\n");
                    cursor_at_col1_of = Some(r + 1);
                }
            } else {
                out.push_str("\x1b[K");
            }
            prev = Some((r, w));
        }
        out.into_bytes()
    }
}

/// `cols` copies of `─` (U+2500).
pub fn rule(cols: u16) -> String {
    "─".repeat(usize::from(cols))
}

// ---------------------------------------------------------------------------
// the event loop
// ---------------------------------------------------------------------------

/// What a key or a timer did to the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Nothing,
    Redraw,
    Exit(i32),
}

/// A TUI mode: draws frames, reacts to keys and timers.
pub trait App {
    fn frame(&self, rows: u16, cols: u16, now: Instant) -> Frame;
    fn key(&mut self, key: Key, now: Instant) -> Step;
    fn tick(&mut self, now: Instant) -> Step;
    /// The next instant [`App::tick`] has something to do.
    fn deadline(&self) -> Option<Instant>;
}

/// Terminal state a mode hands to [`run`].
pub struct Session {
    pub raw: RawMode,
    pub input: Input,
    pub alt: bool,
}

impl Session {
    /// Undo what the mode did to the terminal (alternate screen, raw mode).
    pub fn leave(&mut self) {
        if self.alt {
            out_str(ALT_LEAVE);
            self.alt = false;
        }
        self.raw.restore();
    }
}

/// Run `app` until it exits or a terminating signal arrives. `on_exit` runs
/// after the terminal is restored on both paths (the lead's `kill-pane`
/// cleanup). On a signal the process then dies of that signal.
pub fn run(app: &mut dyn App, session: &mut Session, on_exit: &mut dyn FnMut()) -> i32 {
    let (mut rows, mut cols) = window_size();
    let mut decoder = Decoder::default();
    let mut dirty = true;
    loop {
        if let Some(sig) = term_signal() {
            session.leave();
            on_exit();
            reraise(sig);
        }
        if take_winch() {
            (rows, cols) = window_size();
            dirty = true;
        }
        let now = Instant::now();
        match app.tick(now) {
            Step::Nothing => {}
            Step::Redraw => dirty = true,
            Step::Exit(code) => {
                session.leave();
                on_exit();
                return code;
            }
        }
        if dirty {
            out(&app.frame(rows, cols, now).render(rows, cols));
            dirty = false;
        }
        let wake = [app.deadline(), decoder.deadline()]
            .into_iter()
            .flatten()
            .min();
        let timeout = wake.map_or(MAX_WAIT, |w| w.saturating_duration_since(Instant::now()));
        let bytes = session.input.read_timeout(timeout);
        decoder.push(&bytes);
        let now = Instant::now();
        for key in decoder.drain(now) {
            match app.key(key, now) {
                Step::Nothing => {}
                Step::Redraw => dirty = true,
                Step::Exit(code) => {
                    session.leave();
                    on_exit();
                    return code;
                }
            }
        }
    }
}
