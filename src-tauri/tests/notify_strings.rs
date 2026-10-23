//! The notification texts Rust composes are the WebView's message tables,
//! verbatim (contract §9.12, ux-ia §9.2): every `notify::STRINGS` entry must
//! equal the ko and en values of the same key in `src/i18n/areas/*.ts` (the
//! source; owner T13). A drift fails here instead of showing two wordings.

use std::path::{Path, PathBuf};

use gilde_code_lib::notify::STRINGS;

fn areas_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("src").join("i18n").join("areas")
}

/// The quoted value after `'key':` in a TS object block.
fn ts_value(block: &str, key: &str) -> Option<String> {
    let needle = format!("'{key}':");
    let at = block.find(&needle)? + needle.len();
    let rest = block[at..].trim_start();
    let quote = rest.chars().next()?;
    if quote != '\'' && quote != '"' {
        return None;
    }
    let mut out = String::new();
    let mut chars = rest[1..].chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            out.push(chars.next()?);
        } else if c == quote {
            return Some(out);
        } else {
            out.push(c);
        }
    }
    None
}

#[test]
fn every_rust_notification_string_equals_the_webview_table() {
    let mut files: Vec<(PathBuf, String)> = std::fs::read_dir(areas_dir())
        .expect("src/i18n/areas")
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "ts"))
        .map(|p| {
            let text = std::fs::read_to_string(&p).unwrap();
            (p, text)
        })
        .collect();
    files.sort();
    for (key, ko, en) in STRINGS {
        let needle = format!("'{key}':");
        let owners: Vec<&(PathBuf, String)> = files.iter().filter(|(_, t)| t.contains(&needle)).collect();
        assert_eq!(owners.len(), 1, "{key} lives in exactly one area file");
        let text = &owners[0].1;
        let ko_at = text.find("export const ko").expect("a ko block");
        let en_at = text.find("export const en").expect("an en block");
        let (ko_block, en_block) = if ko_at < en_at {
            (&text[ko_at..en_at], &text[en_at..])
        } else {
            (&text[ko_at..], &text[en_at..ko_at])
        };
        assert_eq!(ts_value(ko_block, key).as_deref(), Some(*ko), "ko {key}");
        assert_eq!(ts_value(en_block, key).as_deref(), Some(*en), "en {key}");
    }
}

#[test]
fn korean_texts_follow_the_style_rules() {
    // sentences end in -습니다, no -세요, no exclamation marks (ux-ia §9.1)
    for (key, ko, _) in STRINGS {
        assert!(!ko.contains("세요"), "{key}");
        assert!(!ko.contains('!'), "{key}");
        if ko.ends_with('.') {
            assert!(ko.ends_with("습니다."), "{key}: {ko}");
        }
    }
}
