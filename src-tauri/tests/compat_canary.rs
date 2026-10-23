//! T21 — the static literal canary (contract §3.10 input 2; §14.2
//! T-compat-canary, T-compat-canaryTimeout, T-compat-shapeRegex; F-08, N-07,
//! PM-43, PM-57). Reads synthesized fixture files only; never executes one.

#[path = "fixtures/compat/mod.rs"]
mod compat_fixtures;

use std::collections::BTreeMap;
use std::time::Duration;

use compat_fixtures::*;
use gilde_code_lib::compat::canary::{
    is_macho, lint_literal, lint_regex, minified_token, regex_skeleton, CanaryEntry,
    CanaryLiterals, CanarySet, ScanOutcome, Scanner, SetStatus, UnknownReason, CHUNK_BYTES,
    OVERLAP_BYTES,
};
use gilde_code_lib::contract::compat::CanaryResult;

const LONG: Duration = Duration::from_secs(60);

fn bundled_scanner() -> Scanner {
    Scanner::new(&CanaryLiterals::bundled()).expect("bundled scanner")
}

fn missing(outcome: &ScanOutcome) -> BTreeMap<CanarySet, Vec<String>> {
    match outcome {
        ScanOutcome::Scanned { missing } => missing.clone(),
        other => panic!("expected a finished scan, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// the shipped entry list and its lint [F-08] — T-compat-shapeRegex
// ---------------------------------------------------------------------------

#[test]
fn bundled_literals_cover_the_three_sets_and_pass_the_lint() {
    let lits = CanaryLiterals::bundled();
    for set in CanarySet::ALL {
        let entries = &lits.sets()[&set];
        assert!(!entries.is_empty(), "{set:?} empty");
        for e in entries {
            match e {
                CanaryEntry::Literal(l) => lint_literal(l).unwrap_or_else(|m| panic!("{m}")),
                CanaryEntry::Regex(r) => lint_regex(r).unwrap_or_else(|m| panic!("{m}")),
            }
        }
    }
    // The contract's sets, exactly (§3.10 input 2).
    let literals = |set| -> Vec<String> {
        lits.sets()[&set]
            .iter()
            .filter_map(|e| match e {
                CanaryEntry::Literal(l) => Some(l.clone()),
                CanaryEntry::Regex(_) => None,
            })
            .collect()
    };
    assert_eq!(
        literals(CanarySet::Conversation),
        [
            "can_use_tool",
            "control_request",
            "control_cancel_request",
            "--permission-prompt-tool",
            "permission_suggestions",
            "pending_permission_requests",
            "--include-partial-messages",
            "--replay-user-messages"
        ]
    );
    assert_eq!(
        literals(CanarySet::PaneHosting),
        [
            "claude-swarm",
            "swarm-view",
            "split-window",
            "respawn-pane",
            "remain-on-exit",
            "kill-pane",
            "has-session",
            "list-panes",
            "#{pane_id}",
            "--agent-id",
            "--parent-session-id",
            "in-process"
        ]
    );
    assert_eq!(
        lits.sets()[&CanarySet::TeamStore],
        vec![
            CanaryEntry::Regex(r"function [$\w]+\(\)\{if\(![$\w]+\(\)\)return;return\}".into()),
            CanaryEntry::Regex(
                r"async function [$\w]+\(e\)\{if\(![$\w]+\(\)\)return\[\];return\[\]\}".into()
            ),
            CanaryEntry::Literal("var g=5000".into()),
        ]
    );
}

#[test]
fn no_literal_set_contains_a_minified_identifier() {
    // The names that vanished one patch later (R6 §6 A-P5) and their forms.
    for bad in [
        "F$r",
        "_Lr",
        "y0o",
        "gs",
        "z",
        "function F$r(){if(!z())return;return}",
        "async function _Lr(e){if(!gs())return[];return[]}",
        "y0o=1000",
        "if(!z())",
        "x=Ab3.y",
    ] {
        assert!(lint_literal(bad).is_err(), "{bad:?} must be rejected");
    }
    for good in [
        "var g=5000",
        "\"split-window\",\"-d\",\"-t\"",
        "#{pane_id}",
        "in-process",
        "--agent-id",
        "CLAUDE_CODE_TEAMMATE_COMMAND",
        "tengu_amber_flint",
        "can_use_tool",
    ] {
        lint_literal(good).unwrap_or_else(|m| panic!("{good:?}: {m}"));
    }
    assert_eq!(
        minified_token("async function _Lr(e){}").as_deref(),
        Some("_Lr")
    );
    assert_eq!(minified_token("var g=5000"), None);

    // Regex entries: the skeleton is linted; a pinned name in a regex fails.
    let skeleton =
        regex_skeleton(r"async function [$\w]+\(e\)\{if\(![$\w]+\(\)\)return\[\];return\[\]\}");
    let words: Vec<&str> = skeleton.split_whitespace().collect();
    assert_eq!(
        words,
        ["async", "function", "(e){if(!", "())return[];return[]}"]
    );
    assert_eq!(
        minified_token(&skeleton),
        None,
        "a parameter `e` and `if(` are not pinned names"
    );
    assert!(lint_regex(r"function F\$r\(\)").is_err());
    assert!(lint_regex(r"async function _Lr\(e\)").is_err());
    assert!(
        lint_regex(r"(?:foo)?").is_err(),
        "a regex that matches the empty input passes everything"
    );
    assert!(lint_regex(r"unclosed(").is_err());
}

#[test]
fn shape_regexes_match_the_2_1_306_and_2_1_307_stand_ins() {
    let scanner = bundled_scanner();
    let (_d, dir) = temp_dir();
    let v306 = write_fake(
        &dir.join("306"),
        &Spec::full("2.1.306").team(Some(TEAM_STORE_306_JS)),
    );
    let v307 = write_fake(
        &dir.join("307"),
        &Spec::full("2.1.307").team(Some(TEAM_STORE_307_JS)),
    );
    for path in [&v306, &v307] {
        let out = scanner.scan_path(path, None, LONG);
        assert!(out.passed(), "{path:?}: {out:?}");
        assert_eq!(out.set_status(CanarySet::TeamStore), SetStatus::Present);
    }
    // Why shapes: the pinned names of 2.1.306 are gone in the 2.1.307 stand-in,
    // so a literal canary on them would have failed the next patch (PM-57).
    let has = |path: &std::path::Path, needle: &str| {
        let bytes = std::fs::read(path).unwrap();
        bytes.windows(needle.len()).any(|w| w == needle.as_bytes())
    };
    for pinned in [
        "function F$r(){if(!z())return;return}",
        "async function _Lr(e){if(!gs())return[];return[]}",
        "y0o=1000",
    ] {
        assert!(has(&v306, pinned) && !has(&v307, pinned), "{pinned}");
    }
}

// ---------------------------------------------------------------------------
// pass / missing / unknown — T-compat-canary
// ---------------------------------------------------------------------------

#[test]
fn every_set_present_passes() {
    let (_d, dir) = temp_dir();
    let p = write_fake(&dir.join("claude"), &Spec::full("2.1.310"));
    let out = bundled_scanner().scan_path(&p, None, LONG);
    assert_eq!(
        out,
        ScanOutcome::Scanned {
            missing: BTreeMap::new()
        }
    );
    assert_eq!(out.to_result(), CanaryResult::Pass);
    assert!(out.is_final());
}

#[test]
fn some_entries_missing_fail_only_their_set() {
    let scanner = bundled_scanner();
    let (_d, dir) = temp_dir();

    let p = write_fake(
        &dir.join("a"),
        &Spec::full("2.1.310").without("--replay-user-messages"),
    );
    let out = scanner.scan_path(&p, None, LONG);
    assert_eq!(
        missing(&out),
        BTreeMap::from([(
            CanarySet::Conversation,
            vec!["--replay-user-messages".to_string()]
        )])
    );
    assert_eq!(out.set_status(CanarySet::PaneHosting), SetStatus::Present);
    assert_eq!(
        out.to_result(),
        CanaryResult::Missing {
            literals: vec!["conversation: --replay-user-messages".into()]
        }
    );

    let p = write_fake(
        &dir.join("b"),
        &Spec::full("2.1.310")
            .without("respawn-pane")
            .without("in-process"),
    );
    assert_eq!(
        missing(&scanner.scan_path(&p, None, LONG)),
        BTreeMap::from([(
            CanarySet::PaneHosting,
            vec!["respawn-pane".to_string(), "in-process".to_string()]
        )])
    );

    // Team-store shapes gone (a refactor): only teamStore is missing.
    let p = write_fake(
        &dir.join("c"),
        &Spec::full("2.1.310").team(Some("var g=5000;")),
    );
    let out = scanner.scan_path(&p, None, LONG);
    let m = missing(&out);
    assert_eq!(
        m.keys().copied().collect::<Vec<_>>(),
        [CanarySet::TeamStore]
    );
    assert_eq!(m[&CanarySet::TeamStore].len(), 2);
    assert!(m[&CanarySet::TeamStore][0].starts_with("/function "));
}

#[test]
fn no_entries_at_all_fails_every_set() {
    let (_d, dir) = temp_dir();
    let p = write_fake(&dir.join("claude"), &Spec::empty("2.1.310"));
    let out = bundled_scanner().scan_path(&p, None, LONG);
    let m = missing(&out);
    assert_eq!(m.len(), 3);
    assert_eq!(m[&CanarySet::Conversation].len(), 8);
    assert_eq!(m[&CanarySet::PaneHosting].len(), 12);
    assert_eq!(m[&CanarySet::TeamStore].len(), 3);
}

#[test]
fn anything_but_a_readable_mach_o_is_unknown_never_missing() {
    let scanner = bundled_scanner();
    let (_d, dir) = temp_dir();

    // A launcher script carrying every literal is still not checked [N-07].
    let script = write_fake(
        &dir.join("cli-wrapper.cjs"),
        &Spec::full("2.1.310").not_macho(),
    );
    assert_eq!(
        scanner.scan_path(&script, None, LONG),
        ScanOutcome::Unknown(UnknownReason::NotMachO)
    );

    let tiny = dir.join("tiny");
    std::fs::write(&tiny, [0xcf, 0xfa, 0xed]).unwrap();
    assert_eq!(
        scanner.scan_path(&tiny, None, LONG),
        ScanOutcome::Unknown(UnknownReason::NotMachO)
    );

    let empty = dir.join("empty");
    std::fs::write(&empty, []).unwrap();
    assert_eq!(
        scanner.scan_path(&empty, None, LONG),
        ScanOutcome::Unknown(UnknownReason::NotMachO)
    );

    let locked = write_fake(&dir.join("locked"), &Spec::full("2.1.310"));
    {
        let _guard = Unreadable::make(&locked).unwrap();
        assert_eq!(
            scanner.scan_path(&locked, None, LONG),
            ScanOutcome::Unknown(UnknownReason::Unreadable)
        );
    }

    assert_eq!(
        scanner.scan_path(&dir, None, LONG),
        ScanOutcome::Unknown(UnknownReason::NotRegularFile)
    );
    assert_eq!(
        scanner.scan_path(&dir.join("absent"), None, LONG),
        ScanOutcome::Unknown(UnknownReason::Unreadable)
    );

    // A FIFO: opening it must not block waiting for a writer.
    let fifo = dir.join("fifo");
    let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    // SAFETY: a valid NUL-terminated path in a temp dir this test owns.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
    assert_eq!(
        scanner.scan_path(&fifo, None, LONG),
        ScanOutcome::Unknown(UnknownReason::NotRegularFile)
    );

    for r in [
        UnknownReason::NotMachO,
        UnknownReason::Unreadable,
        UnknownReason::NotRegularFile,
    ] {
        assert_eq!(ScanOutcome::Unknown(r).to_result(), CanaryResult::Unknown);
        assert_eq!(
            ScanOutcome::Unknown(r).set_status(CanarySet::Conversation),
            SetStatus::Unknown
        );
    }
}

#[test]
fn a_scan_past_its_deadline_is_unknown_never_failed() {
    // T-compat-canaryTimeout (PM-43): a cold or stalled disk times the scan out.
    let (_d, dir) = temp_dir();
    let p = write_fake(
        &dir.join("claude"),
        &Spec::empty("2.1.310").filler(3 * CHUNK_BYTES),
    );
    let out = bundled_scanner().scan_path(&p, None, Duration::ZERO);
    assert_eq!(out, ScanOutcome::Unknown(UnknownReason::Timeout));
    assert_eq!(out.to_result(), CanaryResult::Unknown);
    assert!(!out.is_final(), "a timeout is retried later");
}

#[test]
fn a_replaced_binary_is_not_scanned_under_the_old_fingerprint() {
    let (_d, dir) = temp_dir();
    let p = write_fake(&dir.join("claude"), &Spec::full("2.1.306"));
    let old = gilde_code_lib::compat::canary::fingerprint_of(&p).unwrap();
    replace_by_rename(&p, &Spec::full("2.1.307"));
    let out = bundled_scanner().scan_path(&p, Some(&old), LONG);
    assert_eq!(out, ScanOutcome::Unknown(UnknownReason::Changed));
    assert!(
        out.is_final(),
        "those bytes are gone; rescanning cannot help"
    );
}

// ---------------------------------------------------------------------------
// streaming: seams and early exit
// ---------------------------------------------------------------------------

#[test]
fn matches_straddling_a_step_seam_are_found() {
    let scanner = bundled_scanner();
    let (_d, dir) = temp_dir();
    // Steps run from the end: the last step covers [len - CHUNK, len); place
    // each entry so it straddles offset len - CHUNK.
    let mut bytes = MACHO_ARM64_HEADER.to_vec();
    bytes.extend(filler_bytes(3 * CHUNK_BYTES, 7));
    let len = bytes.len();
    let seam = len - CHUNK_BYTES;
    let all = format!("{CONVERSATION_JS}{PANE_HOSTING_JS}{TEAM_STORE_307_JS}");
    assert!(all.len() < OVERLAP_BYTES);
    // Put the whole script across the seam, half on each side.
    let at = seam - all.len() / 2;
    bytes[at..at + all.len()].copy_from_slice(all.as_bytes());
    let p = dir.join("seam");
    std::fs::write(&p, &bytes).unwrap();
    assert!(scanner.scan_path(&p, None, LONG).passed());

    // The same with the script at the very start and the very end.
    for at in [MACHO_ARM64_HEADER.len(), len - all.len()] {
        let mut b = MACHO_ARM64_HEADER.to_vec();
        b.extend(filler_bytes(3 * CHUNK_BYTES, 7));
        b[at..at + all.len()].copy_from_slice(all.as_bytes());
        std::fs::write(&p, &b).unwrap();
        assert!(scanner.scan_path(&p, None, LONG).passed(), "script at {at}");
    }
}

#[test]
fn counts_are_never_compared() {
    // One hit or a hundred: presence is all that matters [F-08].
    let scanner = bundled_scanner();
    let once = format!("{CONVERSATION_JS}{PANE_HOSTING_JS}{TEAM_STORE_307_JS}");
    let many = once.repeat(100);
    assert!(scanner.scan_bytes(once.as_bytes()).passed());
    assert!(scanner.scan_bytes(many.as_bytes()).passed());
}

// ---------------------------------------------------------------------------
// the Mach-O input check [N-07]
// ---------------------------------------------------------------------------

#[test]
fn mach_o_magics() {
    assert!(is_macho(&MACHO_ARM64_HEADER));
    assert!(is_macho(&[0xce, 0xfa, 0xed, 0xfe, 0, 0, 0, 0])); // MH_MAGIC (32-bit, LE)
    assert!(is_macho(&[0xfe, 0xed, 0xfa, 0xcf, 0, 0, 0, 0])); // MH_CIGAM_64 (big-endian image)
    assert!(is_macho(&[0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 2])); // universal, 2 architectures
    assert!(is_macho(&[0xca, 0xfe, 0xba, 0xbf, 0, 0, 0, 1])); // universal 64
    assert!(!is_macho(&[0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 0x37])); // another class-file format (version 55)
    assert!(!is_macho(&[0x7f, b'E', b'L', b'F', 2, 1, 1, 0])); // ELF
    assert!(!is_macho(b"#!/bin/sh\n"));
    assert!(!is_macho(&[0xcf, 0xfa, 0xed])); // too short
}

// ---------------------------------------------------------------------------
// the data file's own lint
// ---------------------------------------------------------------------------

fn literals_json(sets: serde_json::Value) -> String {
    serde_json::json!({
        "schema": 1,
        "provenance": "test",
        "evidence": { "conversation": "e", "paneHosting": "e", "teamStore": "e" },
        "sets": sets,
    })
    .to_string()
}

#[test]
fn the_literal_file_lint_rejects_bad_entries() {
    let ok_sets = serde_json::json!({
        "conversation": [{ "literal": "can_use_tool" }],
        "paneHosting": [{ "literal": "claude-swarm" }],
        "teamStore": [{ "literal": "var g=5000" }],
    });
    CanaryLiterals::parse(&literals_json(ok_sets.clone())).expect("minimal file parses");

    let mut cases: Vec<(String, &str)> = Vec::new();
    let mut with = |set: &str, entry: serde_json::Value, why: &'static str| {
        let mut s = ok_sets.clone();
        s[set] = serde_json::json!([entry]);
        cases.push((literals_json(s), why));
    };
    with(
        "teamStore",
        serde_json::json!({ "literal": "y0o=1000" }),
        "minified literal",
    );
    with(
        "teamStore",
        serde_json::json!({ "literal": "F$r" }),
        "identifier-like literal",
    );
    with(
        "teamStore",
        serde_json::json!({ "regex": "function F\\$r\\(\\)" }),
        "pinned name in a regex",
    );
    with(
        "teamStore",
        serde_json::json!({ "regex": "a*" }),
        "regex matching empty input",
    );
    with(
        "conversation",
        serde_json::json!({ "literal": "a", "regex": "b" }),
        "two kinds in one entry",
    );
    with(
        "conversation",
        serde_json::json!({ "count": 3 }),
        "unknown entry kind",
    );
    with(
        "conversation",
        serde_json::json!({ "literal": "" }),
        "empty literal",
    );
    for (json, why) in cases {
        assert!(CanaryLiterals::parse(&json).is_err(), "{why}");
    }

    let mut no_team = ok_sets.clone();
    no_team.as_object_mut().unwrap().remove("teamStore");
    assert!(
        CanaryLiterals::parse(&literals_json(no_team)).is_err(),
        "a set is missing"
    );

    let mut extra = ok_sets.clone();
    extra["tasks"] = serde_json::json!([{ "literal": "TaskCreate" }]);
    assert!(
        CanaryLiterals::parse(&literals_json(extra)).is_err(),
        "unknown set"
    );

    let bad_schema = literals_json(ok_sets).replace("\"schema\":1", "\"schema\":2");
    assert!(CanaryLiterals::parse(&bad_schema).is_err(), "schema 2");
}

#[test]
fn the_entry_list_hash_changes_with_the_file() {
    let a = CanaryLiterals::bundled();
    let b = CanaryLiterals::parse(&format!(
        "{}\n",
        gilde_code_lib::compat::canary::BUNDLED_CANARY_LITERALS
    ))
    .unwrap();
    assert_eq!(a.sets(), b.sets());
    assert_ne!(a.hash(), b.hash());
    assert_eq!(a.hash().len(), 16);
}
