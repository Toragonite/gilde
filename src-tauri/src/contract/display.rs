//! Display text rule (contract §3.1 rule 6) — the signature T01 fixes; the
//! implementation is T04's `render::sanitize`.
//!
//! Every string that originates from an agent or a vendor file passes
//! `sanitize_display_text` in Rust before it leaves the core: strip ANSI/CSI/OSC
//! sequences, strip C0/C1 controls except `\n` and `\t`, strip bidi overrides
//! and isolates (U+202A–U+202E, U+2066–U+2069), replace lone surrogates /
//! invalid UTF-8 with U+FFFD, cap per field, truncate at grapheme-cluster
//! boundaries with `…`. Markdown is *not* rendered in Rust.

/// The cap of one display field, as the contract states it per field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TextCap {
    /// "≤ n chars": grapheme-safe, counted in Unicode scalar values.
    Chars(usize),
    /// "≤ n KiB": UTF-8 bytes, cut at a grapheme boundary.
    Bytes(usize),
}

/// A sanitized display string.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Sanitized {
    pub text: String,
    /// The input was cut at its cap (`…` appended; blocks set `truncated: true`).
    pub truncated: bool,
}

/// The signature of the one sanitizer (`render::sanitize::sanitize_display_text`).
pub type SanitizeText = fn(&str, TextCap) -> Sanitized;

/// The signature of its byte-input form (PTY grids, transcript bytes with
/// invalid UTF-8): `render::sanitize::sanitize_display_bytes`.
pub type SanitizeBytes = fn(&[u8], TextCap) -> Sanitized;
