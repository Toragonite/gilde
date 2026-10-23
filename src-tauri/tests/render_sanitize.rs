//! T-render-sanitize (property tests of `sanitize_display_text`, contract
//! §3.1 rule 6, §13.3) and T-render-truncate (grapheme-safe truncation).
//! Owner: T04.

use gilde_code_lib::contract::display::TextCap;
use gilde_code_lib::render::sanitize::{
    incomplete_escape_start, is_bidi_control, is_display_safe, sanitize_display_bytes, sanitize_display_text, ELLIPSIS,
};
use proptest::prelude::*;
use unicode_segmentation::UnicodeSegmentation;

const BIG: TextCap = TextCap::Bytes(1 << 22);

fn units(s: &str, cap: TextCap) -> usize {
    match cap {
        TextCap::Chars(_) => s.chars().count(),
        TextCap::Bytes(_) => s.len(),
    }
}

fn limit(cap: TextCap) -> usize {
    match cap {
        TextCap::Chars(n) | TextCap::Bytes(n) => n,
    }
}

/// Hostile text: ANSI/OSC/DCS fragments, C0/C1 controls, bidi overrides,
/// Hangul (precomposed and jamo), emoji ZWJ sequences, flags, combining marks.
fn hostile() -> impl Strategy<Value = String> {
    let piece = prop_oneof![
        Just("\u{1b}[31m".to_string()),
        Just("\u{1b}[0m".to_string()),
        Just("\u{1b}]0;title\u{07}".to_string()),
        Just("\u{1b}]8;;http://evil\u{1b}\\".to_string()),
        Just("\u{1b}P1;2q\u{1b}\\".to_string()),
        Just("\u{1b}".to_string()),
        Just("\u{1b}[".to_string()),
        Just("\u{9b}2J".to_string()),
        Just("\u{9d}x\u{9c}".to_string()),
        Just("\r".to_string()),
        Just("\u{0}".to_string()),
        Just("\u{7f}".to_string()),
        Just("\u{85}".to_string()),
        Just("\u{202e}".to_string()),
        Just("\u{2066}".to_string()),
        Just("\u{2069}".to_string()),
        Just("\n".to_string()),
        Just("\t".to_string()),
        Just("한국어".to_string()),
        Just("\u{1100}\u{1161}\u{11a8}".to_string()),
        Just("👨\u{200d}👩\u{200d}👧\u{200d}👦".to_string()),
        Just("🇰🇷".to_string()),
        Just("e\u{301}\u{302}".to_string()),
        Just("…".to_string()),
        "[a-zA-Z0-9 .,;:!?(){}<>\\[\\]]{0,12}",
        any::<char>().prop_map(|c| c.to_string()),
    ];
    proptest::collection::vec(piece, 0..40).prop_map(|v| v.concat())
}

fn caps() -> impl Strategy<Value = TextCap> {
    prop_oneof![(0usize..80).prop_map(TextCap::Chars), (0usize..240).prop_map(TextCap::Bytes)]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 2000, .. ProptestConfig::default() })]

    /// No escape, control or bidi character ever survives.
    #[test]
    fn t_render_sanitize_output_is_display_safe(input in hostile(), cap in caps()) {
        let out = sanitize_display_text(&input, cap);
        prop_assert!(out.text.chars().all(is_display_safe), "{:?} -> {:?}", input, out.text);
        prop_assert!(!out.text.chars().any(is_bidi_control));
        prop_assert!(!out.text.contains('\u{1b}'), "an ESC survived");
    }

    /// The cap holds, including the ellipsis.
    #[test]
    fn t_render_sanitize_cap_holds(input in hostile(), cap in caps()) {
        let out = sanitize_display_text(&input, cap);
        prop_assert!(units(&out.text, cap) <= limit(cap), "{:?} over {:?}", out.text, cap);
    }

    /// A cut keeps a grapheme-boundary prefix of the uncut result and marks it.
    #[test]
    fn t_render_truncate_prefix_at_grapheme_boundary(input in hostile(), cap in caps()) {
        let full = sanitize_display_text(&input, BIG);
        prop_assert!(!full.truncated);
        let out = sanitize_display_text(&input, cap);
        if out.truncated {
            let ell = match cap { TextCap::Chars(_) => 1, TextCap::Bytes(_) => ELLIPSIS.len_utf8() };
            if limit(cap) >= ell {
                prop_assert!(out.text.ends_with(ELLIPSIS));
                let prefix = &out.text[..out.text.len() - ELLIPSIS.len_utf8()];
                prop_assert!(full.text.starts_with(prefix));
                let boundaries: Vec<usize> = full.text.grapheme_indices(true).map(|(i, _)| i).chain([full.text.len()]).collect();
                prop_assert!(boundaries.contains(&prefix.len()), "cut inside a grapheme: {:?}", prefix);
            } else {
                prop_assert!(out.text.is_empty());
            }
        } else {
            prop_assert_eq!(&out.text, &full.text);
        }
    }

    /// Sanitizing is idempotent on its own (uncut) output.
    #[test]
    fn t_render_sanitize_idempotent(input in hostile()) {
        let once = sanitize_display_text(&input, BIG).text;
        let twice = sanitize_display_text(&once, BIG).text;
        prop_assert_eq!(once, twice);
    }

    /// Plain text passes unchanged.
    #[test]
    fn t_render_sanitize_plain_unchanged(input in "[a-zA-Z0-9 가-힣\\n\\t.,]{0,200}") {
        let out = sanitize_display_text(&input, BIG);
        prop_assert_eq!(out.text, input);
        prop_assert!(!out.truncated);
    }

    /// Bytes: valid UTF-8 behaves as text; any bytes give display-safe text.
    #[test]
    fn t_render_sanitize_bytes(bytes in proptest::collection::vec(any::<u8>(), 0..300), cap in caps()) {
        let out = sanitize_display_bytes(&bytes, cap);
        prop_assert!(out.text.chars().all(is_display_safe));
        prop_assert!(units(&out.text, cap) <= limit(cap));
        if let Ok(s) = std::str::from_utf8(&bytes) {
            prop_assert_eq!(out, sanitize_display_text(s, cap));
        }
    }

    /// A sequence split across two deltas is removed whole when the
    /// incomplete suffix is carried over.
    #[test]
    fn t_render_sanitize_split_deltas(input in hostile(), split in 0usize..400) {
        let at = (0..=split.min(input.len())).rev().find(|i| input.is_char_boundary(*i)).unwrap_or(0);
        let (a, b) = input.split_at(at);
        let mut first = a.to_string();
        let carry = match incomplete_escape_start(&first) { Some(i) => first.split_off(i), None => String::new() };
        let joined = format!(
            "{}{}",
            sanitize_display_text(&first, BIG).text,
            sanitize_display_text(&format!("{carry}{b}"), BIG).text
        );
        prop_assert_eq!(joined, sanitize_display_text(&input, BIG).text);
    }
}

#[test]
fn t_render_truncate_hangul_and_emoji() {
    // Precomposed syllables: chars cap.
    let r = sanitize_display_text("가나다라마바사", TextCap::Chars(4));
    assert_eq!(r.text, "가나다…");
    assert!(r.truncated);
    // Bytes cap: a syllable is 3 bytes, never split.
    let r = sanitize_display_text("가나다라", TextCap::Bytes(8));
    assert_eq!(r.text, "가…");
    // Conjoining jamo stay one cluster.
    let jamo = "\u{1100}\u{1161}\u{11a8}";
    let r = sanitize_display_text(&jamo.repeat(4), TextCap::Chars(7));
    assert_eq!(r.text, format!("{jamo}{jamo}…"));
    // An emoji family is one cluster of 7 chars.
    let fam = "👨\u{200d}👩\u{200d}👧\u{200d}👦";
    let r = sanitize_display_text(&format!("{fam}{fam}"), TextCap::Chars(10));
    assert_eq!(r.text, format!("{fam}…"));
    // Flags pair regional indicators.
    let r = sanitize_display_text("🇰🇷🇺🇸🇯🇵", TextCap::Chars(5));
    assert_eq!(r.text, "🇰🇷🇺🇸…");
    // Combining marks stay with their base.
    let r = sanitize_display_text("ae\u{301}\u{302}x", TextCap::Chars(3));
    assert_eq!(r.text, "a…");
}

#[test]
fn t_render_sanitize_known_attacks() {
    let cases: &[(&str, &str)] = &[
        ("\u{1b}]52;c;ZXZpbA==\u{07}ok", "ok"),                   // OSC 52 clipboard write
        ("file\u{202e}gnp.js", "filegnp.js"),                     // RLO spoofing
        ("\u{1b}[2J\u{1b}[Hcleared", "cleared"),                  // screen clear
        ("a\u{1b}]8;;javascript:alert(1)\u{1b}\\b\u{1b}]8;;\u{1b}\\", "ab"), // OSC 8 link
        ("x\u{90}dcs\u{9c}y", "xy"),                               // 8-bit DCS
        ("bell\u{07}", "bell"),
    ];
    for (input, want) in cases {
        assert_eq!(sanitize_display_text(input, BIG).text, *want, "{input:?}");
    }
}
