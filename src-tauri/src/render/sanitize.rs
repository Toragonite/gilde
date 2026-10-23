//! `sanitize_display_text` (contract §3.1 rule 6, §9.1): the one display-text
//! sanitizer every agent- or vendor-originated string passes before it leaves
//! the core.
//!
//! Owner: T04. The signatures are fixed by T01 (`contract::display`).
//!
//! What it does, in one pass over the input:
//!
//! 1. **Escape sequences are removed whole**, the way a terminal consumes
//!    them: CSI (`ESC [` … final byte, or the 8-bit `U+009B`), the string
//!    sequences OSC/DCS/SOS/PM/APC (`ESC ]`, `ESC P`, `ESC X`, `ESC ^`,
//!    `ESC _` or their 8-bit forms) up to BEL or ST (`ESC \` / `U+009C`), and
//!    the short `ESC <intermediates> <final>` forms. A string sequence also
//!    ends at CAN/SUB and — unlike a terminal — at a line feed (which is
//!    kept), so one stray `ESC ]` never swallows the rest of a long message.
//! 2. **Controls are dropped**: C0 except `\n` and `\t` (so `\r`, NUL and
//!    stray ESCs), DEL, and C1 (`U+0080`–`U+009F`).
//! 3. **Bidi controls are dropped**: overrides and embeddings
//!    (`U+202A`–`U+202E`), isolates (`U+2066`–`U+2069`) and the implicit marks
//!    LRM, RLM and ALM (`U+200E`, `U+200F`, `U+061C`, A48.3).
//! 4. **Invalid UTF-8** (byte input) becomes `U+FFFD` first. A `&str` is valid
//!    by construction: lone surrogates cannot exist in a Rust string, and JSON
//!    `\uD800` escapes are repaired to `U+FFFD` before transcript rows are
//!    parsed (`render::transcript::repair_lone_surrogates`).
//! 5. **The cap** ([`TextCap`]) bounds the result *including* the `…` that
//!    marks a cut. The cut is at an extended grapheme-cluster boundary, so a
//!    Hangul syllable (precomposed or conjoining jamo), an emoji ZWJ sequence,
//!    a flag pair or a base with its combining marks is never split.
//!
//! The work is bounded by the cap, not by the input: once the cleaned text is
//! certainly longer than the cap the rest of the input is not looked at, so a
//! 1.36 MB tool-result line costs no more than its 8 KiB preview.
//!
//! Markdown is not touched (the WebView renders an AST, §9.1).
//!
//! **Labels** ([`sanitize_label_text`], A48.3): names and paths shown as
//! labels (a project, a worktree, a branch, a teammate, a notification's
//! session label) additionally lose the zero-width characters `U+200B`–`U+200D`
//! and `U+FEFF`, so an invisible character can neither hide inside a name nor
//! make two different names look the same. General text keeps them: `U+200D`
//! joins emoji sequences there.

use std::borrow::Cow;

use unicode_segmentation::UnicodeSegmentation;

use crate::contract::display::{Sanitized, TextCap};

/// The mark appended to a cut field (§3.1 rule 6, §3.7.2).
pub const ELLIPSIS: char = '…';

/// How far past the cap the cleaner keeps producing output before it stops:
/// enough that every grapheme boundary at or below the cap is decided by the
/// character that follows it.
const SLACK_CHARS: usize = 32;

/// Strip ANSI/CSI/OSC sequences, C0/C1 controls except `\n` and `\t`, bidi
/// controls (U+202A–U+202E, U+2066–U+2069, LRM/RLM/ALM); cap per field;
/// truncate at grapheme-cluster boundaries with `…`.
pub fn sanitize_display_text(input: &str, cap: TextCap) -> Sanitized {
    let (clean, complete) = clean(input, cap, Mode::Display);
    fit(&clean, complete, cap)
}

/// As [`sanitize_display_text`], for a name or path shown as a label: also
/// strips the zero-width characters U+200B–U+200D and U+FEFF (A48.3).
pub fn sanitize_label_text(input: &str, cap: TextCap) -> Sanitized {
    let (clean, complete) = clean(input, cap, Mode::Label);
    fit(&clean, complete, cap)
}

/// As [`sanitize_display_text`] for raw bytes: invalid UTF-8 (and lone
/// surrogates, which are invalid UTF-8 as bytes) become U+FFFD first.
pub fn sanitize_display_bytes(input: &[u8], cap: TextCap) -> Sanitized {
    let text = String::from_utf8_lossy(input);
    sanitize_display_text(&text, cap)
}

/// The signatures match the contract's function types.
const _: crate::contract::display::SanitizeText = sanitize_display_text;
const _: crate::contract::display::SanitizeBytes = sanitize_display_bytes;

/// The sanitized text only (for callers that track truncation elsewhere).
pub fn display(input: &str, cap: TextCap) -> String {
    sanitize_display_text(input, cap).text
}

/// `U+202A`–`U+202E` (embeddings and overrides), `U+2066`–`U+2069`
/// (isolates) and the marks LRM `U+200E`, RLM `U+200F` and ALM `U+061C`
/// (A48.3).
pub fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}' | '\u{061C}')
}

/// The zero-width characters a label drops (A48.3): ZWSP `U+200B`, ZWNJ
/// `U+200C`, ZWJ `U+200D` and the BOM / ZWNBSP `U+FEFF`.
pub fn is_zero_width(c: char) -> bool {
    matches!(c, '\u{200B}'..='\u{200D}' | '\u{FEFF}')
}

/// Whether `c` may appear in a sanitized label ([`sanitize_label_text`]).
pub fn is_label_safe(c: char) -> bool {
    is_display_safe(c) && !is_zero_width(c)
}

/// Display text or a label.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Display,
    Label,
}

#[inline]
fn is_safe(c: char, mode: Mode) -> bool {
    match mode {
        Mode::Display => is_display_safe(c),
        Mode::Label => is_label_safe(c),
    }
}

/// Whether `c` may appear in sanitized output.
pub fn is_display_safe(c: char) -> bool {
    match c {
        '\n' | '\t' => true,
        '\u{0}'..='\u{1F}' | '\u{7F}'..='\u{9F}' => false,
        c => !is_bidi_control(c),
    }
}

/// The cap's limit and the size of the ellipsis in the cap's unit.
fn limit_of(cap: TextCap) -> (usize, usize) {
    match cap {
        TextCap::Chars(n) => (n, 1),
        TextCap::Bytes(n) => (n, ELLIPSIS.len_utf8()),
    }
}

#[inline]
fn unit_len(c: char, cap: TextCap) -> usize {
    match cap {
        TextCap::Chars(_) => 1,
        TextCap::Bytes(_) => c.len_utf8(),
    }
}

fn units_of(s: &str, cap: TextCap) -> usize {
    match cap {
        TextCap::Chars(_) => s.chars().count(),
        TextCap::Bytes(_) => s.len(),
    }
}

/// Where the cleaner may stop: past the cap by [`SLACK_CHARS`] characters.
fn stop_units(cap: TextCap) -> usize {
    match cap {
        TextCap::Chars(n) => n.saturating_add(SLACK_CHARS),
        TextCap::Bytes(n) => n.saturating_add(SLACK_CHARS * 4),
    }
}

/// The terminal-style parser state of the cleaner.
#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    /// After `ESC`.
    Esc,
    /// `ESC` + intermediate bytes (0x20–0x2F), waiting for a final byte.
    EscIntermediate,
    /// Inside a CSI, waiting for a final byte (0x40–0x7E).
    Csi,
    /// Inside OSC/DCS/SOS/PM/APC, waiting for BEL or ST.
    Str,
    /// `ESC` inside a string sequence (`ESC \` = ST).
    StrEsc,
}

/// The cleaned text and whether the whole input was consumed. When it was
/// not, the cleaned text is already longer than the cap.
fn clean(input: &str, cap: TextCap, mode: Mode) -> (Cow<'_, str>, bool) {
    let stop = stop_units(cap);
    let mut units = 0usize;
    for (i, c) in input.char_indices() {
        if !is_safe(c, mode) {
            return clean_slow(input, i, units, stop, cap, mode);
        }
        units += unit_len(c, cap);
        if units > stop {
            return (Cow::Borrowed(&input[..i + c.len_utf8()]), false);
        }
    }
    (Cow::Borrowed(input), true)
}

fn clean_slow(input: &str, start: usize, mut units: usize, stop: usize, cap: TextCap, mode: Mode) -> (Cow<'_, str>, bool) {
    let mut out = String::with_capacity(input.len().min(stop.saturating_mul(4)).max(start));
    out.push_str(&input[..start]);
    let mut state = State::Ground;
    let mut chars = input[start..].chars();
    let mut pending: Option<char> = None;
    while let Some(c) = pending.take().or_else(|| chars.next()) {
        if state == State::Ground && is_safe(c, mode) {
            out.push(c);
            units += unit_len(c, cap);
            if units > stop {
                return (Cow::Owned(out), false);
            }
            continue;
        }
        let (next, again) = transition(state, c);
        state = next;
        if again {
            pending = Some(c);
        }
    }
    (Cow::Owned(out), true)
}

/// One step of the escape-sequence parser: the next state and whether `c`
/// must be processed again in it (it ended a sequence without belonging to
/// it). In `Ground`, a character that is not safe and does not start a
/// sequence (a control, a bidi control, a label's zero-width character) is
/// simply dropped.
fn transition(state: State, c: char) -> (State, bool) {
    match state {
        State::Ground => match c {
            '\u{1B}' => (State::Esc, false),
            '\u{9B}' => (State::Csi, false),
            '\u{90}' | '\u{98}' | '\u{9D}' | '\u{9E}' | '\u{9F}' => (State::Str, false),
            _ => (State::Ground, false),
        },
        State::Esc => match c {
            '[' => (State::Csi, false),
            ']' | 'P' | 'X' | '^' | '_' => (State::Str, false),
            '\u{20}'..='\u{2F}' => (State::EscIntermediate, false),
            '\u{30}'..='\u{7E}' => (State::Ground, false),
            // A second ESC restarts the escape.
            '\u{1B}' => (State::Esc, false),
            _ => (State::Ground, true),
        },
        State::EscIntermediate => match c {
            '\u{20}'..='\u{2F}' => (State::EscIntermediate, false),
            '\u{30}'..='\u{7E}' => (State::Ground, false),
            _ => (State::Ground, true),
        },
        State::Csi => match c {
            '\u{20}'..='\u{3F}' => (State::Csi, false),
            '\u{40}'..='\u{7E}' => (State::Ground, false),
            // A malformed CSI is cancelled; the character is text again.
            _ => (State::Ground, true),
        },
        State::Str => match c {
            '\u{07}' | '\u{9C}' | '\u{18}' | '\u{1A}' => (State::Ground, false),
            '\u{1B}' => (State::StrEsc, false),
            '\n' => (State::Ground, true),
            _ => (State::Str, false),
        },
        // ESC + anything but `\\` ends the string and starts a new escape.
        State::StrEsc => match c {
            '\\' => (State::Ground, false),
            _ => (State::Esc, true),
        },
    }
}

/// The byte index where a trailing, unterminated escape sequence starts, if
/// `s` ends inside one. Streaming callers (text deltas) keep that suffix and
/// prepend it to the next delta, so a sequence split across two deltas is
/// still removed whole instead of leaking its tail (`[31m`) as text.
pub fn incomplete_escape_start(s: &str) -> Option<usize> {
    let mut state = State::Ground;
    let mut start = 0usize;
    for (i, c) in s.char_indices() {
        let mut cur = Some(c);
        while let Some(ch) = cur.take() {
            let before = state;
            let (next, again) = transition(state, ch);
            state = next;
            if before == State::Ground && state != State::Ground {
                start = i;
            }
            if again {
                cur = Some(ch);
            }
        }
    }
    (state != State::Ground).then_some(start)
}

/// Apply the cap to cleaned text.
fn fit(clean: &str, complete: bool, cap: TextCap) -> Sanitized {
    let (limit, ellipsis) = limit_of(cap);
    if complete && units_of(clean, cap) <= limit {
        return Sanitized { text: clean.to_owned(), truncated: false };
    }
    if limit < ellipsis {
        return Sanitized { text: String::new(), truncated: true };
    }
    let budget = limit - ellipsis;
    let mut cut = 0usize;
    let mut used = 0usize;
    for (i, g) in clean.grapheme_indices(true) {
        let n = units_of(g, cap);
        if used + n > budget {
            break;
        }
        used += n;
        cut = i + g.len();
    }
    let mut text = String::with_capacity(cut + ELLIPSIS.len_utf8());
    text.push_str(&clean[..cut]);
    text.push(ELLIPSIS);
    Sanitized { text, truncated: true }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(input: &str, cap: TextCap) -> (String, bool) {
        let r = sanitize_display_text(input, cap);
        (r.text, r.truncated)
    }

    const BIG: TextCap = TextCap::Bytes(1 << 20);

    #[test]
    fn plain_text_passes_unchanged() {
        assert_eq!(s("hello\tworld\n안녕하세요", BIG), ("hello\tworld\n안녕하세요".into(), false));
        assert_eq!(s("", TextCap::Chars(0)), (String::new(), false));
    }

    #[test]
    fn ansi_sequences_are_removed_whole() {
        assert_eq!(s("\u{1b}[31mred\u{1b}[0m", BIG).0, "red");
        assert_eq!(s("a\u{1b}[38;2;255;0;0mb", BIG).0, "ab");
        assert_eq!(s("\u{1b}[?1049h\u{1b}[Hx", BIG).0, "x");
        assert_eq!(s("\u{1b}]0;title\u{07}text", BIG).0, "text");
        assert_eq!(s("\u{1b}]8;;http://x\u{1b}\\link\u{1b}]8;;\u{1b}\\", BIG).0, "link");
        assert_eq!(s("\u{1b}Pqdata\u{1b}\\after", BIG).0, "after");
        assert_eq!(s("\u{1b}(Bplain", BIG).0, "plain");
        assert_eq!(s("\u{1b}=k", BIG).0, "k");
        assert_eq!(s("\u{9b}31mx", BIG).0, "x");
        assert_eq!(s("\u{9d}0;t\u{9c}y", BIG).0, "y");
        assert_eq!(s("end\u{1b}", BIG).0, "end");
        assert_eq!(s("\u{1b}]0;t\u{1b}[1mz", BIG).0, "z");
    }

    #[test]
    fn stray_osc_stops_at_line_feed() {
        assert_eq!(s("a\u{1b}]junk\nrest", BIG).0, "a\nrest");
    }

    #[test]
    fn malformed_csi_keeps_text() {
        assert_eq!(s("\u{1b}[12\u{e9}t", BIG).0, "\u{e9}t");
    }

    #[test]
    fn controls_and_bidi_are_dropped() {
        assert_eq!(s("a\rb\u{0}c\u{7f}d\u{85}e", BIG).0, "abcde");
        assert_eq!(s("x\u{202E}gnp.exe\u{202C}", BIG).0, "xgnp.exe");
        assert_eq!(s("\u{2066}i\u{2069}", BIG).0, "i");
        assert_eq!(s("crlf\r\nline", BIG).0, "crlf\nline");
    }

    // A48.3: the implicit bidi marks go everywhere; zero-width characters go
    // in labels only (U+200D still joins an emoji sequence in text).
    #[test]
    fn bidi_marks_are_dropped_everywhere() {
        assert_eq!(s("a\u{200E}b\u{200F}c\u{061C}d", BIG).0, "abcd");
        assert_eq!(s("\u{200F}\u{202E}cod.exe", BIG).0, "cod.exe");
        let l = sanitize_label_text("pay\u{200E}ments\u{061C}", TextCap::Chars(80));
        assert_eq!(l.text, "payments");
        for c in ['\u{200E}', '\u{200F}', '\u{061C}'] {
            assert!(is_bidi_control(c) && !is_display_safe(c) && !is_label_safe(c), "{c:?}");
        }
    }

    #[test]
    fn zero_width_characters_are_dropped_in_labels_only() {
        let name = "pay\u{200B}ments\u{200C}\u{200D}\u{FEFF}";
        assert_eq!(sanitize_label_text(name, TextCap::Chars(80)).text, "payments");
        assert_eq!(s(name, BIG).0, name, "display text keeps them");
        let family = "👨\u{200d}👩\u{200d}👧";
        assert_eq!(s(family, BIG).0, family, "a ZWJ sequence survives in text");
        // only zero-width characters: an empty label
        assert_eq!(sanitize_label_text("\u{200B}\u{FEFF}", TextCap::Chars(8)).text, "");
        // mixed with escapes, controls and bidi controls
        assert_eq!(sanitize_label_text("\u{1b}[31mw\u{200B}t\u{202E}\u{200F}-1\r", TextCap::Chars(64)).text, "wt-1");
        // the cap counts what is kept, not the zero-width characters
        let padded = format!("{}abcdef", "\u{200B}".repeat(10));
        assert_eq!(sanitize_label_text(&padded, TextCap::Chars(4)).text, "abc…");
        assert_eq!(sanitize_label_text(&padded, TextCap::Chars(6)), Sanitized { text: "abcdef".into(), truncated: false });
    }

    #[test]
    fn bytes_become_replacement_chars() {
        let r = sanitize_display_bytes(b"ok\xff\xfe!", BIG);
        assert!(r.text.starts_with("ok"));
        assert!(r.text.contains('\u{FFFD}'));
        assert!(r.text.ends_with('!'));
        // A CESU-8 encoded lone surrogate.
        let r = sanitize_display_bytes(b"a\xed\xa0\x80b", BIG);
        assert!(r.text.contains('\u{FFFD}') && r.text.starts_with('a') && r.text.ends_with('b'));
    }

    #[test]
    fn caps_include_the_ellipsis() {
        assert_eq!(s("abcdef", TextCap::Chars(4)), ("abc…".into(), true));
        assert_eq!(s("abcd", TextCap::Chars(4)), ("abcd".into(), false));
        assert_eq!(s("abcdef", TextCap::Bytes(5)), ("ab…".into(), true));
        assert_eq!(s("abcdef", TextCap::Bytes(2)), (String::new(), true));
        assert_eq!(s("abc", TextCap::Chars(0)), (String::new(), true));
    }

    #[test]
    fn graphemes_are_never_split() {
        // Conjoining jamo: one syllable of three code points.
        let jamo = "\u{1100}\u{1161}\u{11A8}";
        let text = format!("{jamo}{jamo}{jamo}");
        let (t, cut) = s(&text, TextCap::Chars(5));
        assert!(cut);
        assert_eq!(t, format!("{jamo}…"));
        let family = "👨\u{200d}👩\u{200d}👧";
        let (t, _) = s(&format!("a{family}b"), TextCap::Chars(4));
        assert_eq!(t, "a…");
    }

    #[test]
    fn long_inputs_stop_early_but_cut_exactly() {
        let long = "x".repeat(5_000_000);
        let r = sanitize_display_text(&long, TextCap::Bytes(8 * 1024));
        assert!(r.truncated);
        assert_eq!(r.text.len(), 8 * 1024);
        assert!(r.text.ends_with('…'));
    }
}
