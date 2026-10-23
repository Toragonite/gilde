//! T21 — the shipped table and app features (contract §3.10 input 1, §1.3,
//! §15.5 data follow-up; §14.2 T-compat-* "shipped table lookup").
//!
//! The first two tests lock `resources/claude-compat.json` and
//! `resources/app-features.json` to the §1.3 Status column after the R6
//! fold-in: a data change that is not also a contract change fails here.

use std::collections::BTreeMap;

use gilde_code_lib::compat::features::{AppFeatures, APP_FLAGS, CLAUDE_FLAGS, TEAM_STORE_VERIFIED};
use gilde_code_lib::compat::table::ShippedTable;

fn flags(pairs: &[(&str, bool)]) -> BTreeMap<String, bool> {
    pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
}

#[test]
fn the_shipped_table_holds_exactly_the_r6_fold_in() {
    let table = ShippedTable::bundled();
    assert_eq!(
        table.entries().count(),
        2,
        "only the probed versions are in the table"
    );

    // 2.1.306: conversation verified (group 1 ran on it), pane hosting not.
    let v306 = table.get("2.1.306").expect("2.1.306");
    assert!(v306.conversation_verified);
    assert!(!v306.pane_hosting_verified);
    assert_eq!(
        v306.features,
        flags(&[
            ("handshakeShape", true),        // P1
            ("sessionIdHonoured", true),     // P2
            ("permissionPromptsHost", true), // P3
            ("allowSession", true),          // P4
            ("denyAndStop", true),           // P5
            ("resumeRearm", false),          // P6 fallback
            ("stdinEofCancel", false),       // P7 fallback
            ("idleStdinExit", true),         // P8
            ("nowPriority", true),           // P9
            ("interruptReceipt", true),      // P9
            ("sessionStateEvents", true),    // P9
            ("planAnswer", true),            // P10
            ("freeTextAnswer", true),        // P10
            ("parallelRequests", true),      // P11
            ("errorReply", true),            // P11
            ("sandboxAsk", true),            // P11
            ("getUsageLive", true),          // P12
            ("entrypointMarker", true),      // P14
            ("screenDigitNoCr", true),       // P20
            ("transcriptUsage", true),       // P32
        ])
    );

    // 2.1.307: pane hosting verified (group 3 ran on it), conversation not.
    let v307 = table.get("2.1.307").expect("2.1.307");
    assert!(!v307.conversation_verified);
    assert!(v307.pane_hosting_verified);
    assert_eq!(
        v307.features,
        flags(&[
            ("sessionIdHonoured", true),        // P2
            ("nowPriority", true),              // P9
            ("interruptReceipt", true),         // P9
            ("sessionStateEvents", true),       // P9
            ("paneHostingVerified", true),      // P18
            ("leadPromptsForTeammates", true),  // P19
            ("screenDigitNoCr", true),          // P20
            ("idlePerTurn", true),              // P22
            ("teammateQuestionLocation", true), // P22
            ("sendMessageMsgId", true),         // P22
            ("inFlightSignals", false),         // P23 fallback
            ("bgAttach", true),                 // P28
        ])
    );

    // Open / blocked / conditional clauses ship their fallback (absent).
    for open in [
        "ruleAllowMarker",
        "l2SpawnSeam",
        "continueOtherAccountResume",
    ] {
        assert!(
            !v306.features.contains_key(open) && !v307.features.contains_key(open),
            "{open}"
        );
    }
    // teamStoreVerified is derived from the canary, never shipped (§5.2.4).
    assert!(!v306.features.contains_key(TEAM_STORE_VERIFIED));
    assert!(!v307.features.contains_key(TEAM_STORE_VERIFIED));
    assert!(table.get("2.1.305").is_none());
    assert!(table.get("garbage").is_none());
}

#[test]
fn the_shipped_app_features_are_all_fallbacks() {
    // LP-24/28/29/36 were not in R6's brief: every app flag is pending.
    let app = AppFeatures::bundled();
    assert!(app.flags().is_empty());
    for flag in APP_FLAGS {
        assert!(!app.get(flag), "{flag}");
    }
    assert!(!app.get("unknownFlag"));
}

// ---------------------------------------------------------------------------
// the loader's lints (every value traceable, only §1.3 flags)
// ---------------------------------------------------------------------------

/// A minimal valid table: one version with one flag; every other flag pending.
fn table_json(versions: serde_json::Value, extra_pending: &[&str]) -> String {
    let mut pending = serde_json::Map::new();
    let set: Vec<String> = versions
        .as_object()
        .unwrap()
        .values()
        .flat_map(|e| {
            e["features"]
                .as_object()
                .map(|m| m.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default()
        })
        .collect();
    for flag in CLAUDE_FLAGS {
        if *flag != TEAM_STORE_VERIFIED && !set.iter().any(|s| s == flag) {
            pending.insert(flag.to_string(), serde_json::json!("open"));
        }
    }
    for flag in extra_pending {
        pending.insert(flag.to_string(), serde_json::json!("open"));
    }
    serde_json::json!({ "schema": 1, "provenance": "test", "pending": pending, "versions": versions }).to_string()
}

fn one_version(entry: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "2.1.400": entry })
}

#[test]
fn a_minimal_table_parses() {
    let json = table_json(
        one_version(serde_json::json!({
            "conversation": "verified",
            "features": { "allowSession": true },
            "evidence": { "conversation": "probe", "allowSession": "probe" }
        })),
        &[],
    );
    let t = ShippedTable::parse(&json).expect("parses");
    let e = t.get("2.1.400").unwrap();
    assert!(e.conversation_verified && !e.pane_hosting_verified);
    assert_eq!(e.features, flags(&[("allowSession", true)]));
}

#[test]
fn the_table_lint_rejects_untraceable_or_unknown_values() {
    let cases: Vec<(String, &str)> = vec![
        (
            table_json(
                one_version(
                    serde_json::json!({ "features": { "allowSesion": true }, "evidence": { "allowSesion": "x" } }),
                ),
                &[],
            ),
            "a misspelt flag",
        ),
        (
            table_json(
                one_version(
                    serde_json::json!({ "features": { "allowSession": true }, "evidence": {} }),
                ),
                &[],
            ),
            "a flag without evidence",
        ),
        (
            table_json(
                one_version(
                    serde_json::json!({ "features": { "allowSession": true }, "evidence": { "allowSession": "  " } }),
                ),
                &[],
            ),
            "blank evidence",
        ),
        (
            table_json(
                one_version(serde_json::json!({
                    "features": { "allowSession": true },
                    "evidence": { "allowSession": "x", "denyAndStop": "x" }
                })),
                &[],
            ),
            "evidence for a flag that is not set",
        ),
        (
            table_json(
                one_version(serde_json::json!({ "conversation": "verified", "evidence": {} })),
                &[],
            ),
            "a verified capability without evidence",
        ),
        (
            table_json(
                one_version(
                    serde_json::json!({ "conversation": "failed", "evidence": { "conversation": "x" } }),
                ),
                &[],
            ),
            "the table cannot fail a capability",
        ),
        (
            table_json(
                one_version(
                    serde_json::json!({ "paneHosting": "unverified", "evidence": { "paneHosting": "x" } }),
                ),
                &[],
            ),
            "the table only verifies",
        ),
        (
            table_json(
                one_version(
                    serde_json::json!({ "features": { "teamStoreVerified": true }, "evidence": { "teamStoreVerified": "x" } }),
                ),
                &[],
            ),
            "teamStoreVerified is canary-derived",
        ),
        (
            table_json(
                serde_json::json!({ "2.1": { "conversation": "verified", "evidence": { "conversation": "x" } } }),
                &[],
            ),
            "a bad version key",
        ),
        (
            table_json(one_version(serde_json::json!({ "evidence": {} })), &[]),
            "an empty entry",
        ),
        (
            table_json(
                one_version(
                    serde_json::json!({ "features": { "allowSession": true }, "evidence": { "allowSession": "x" } }),
                ),
                &["allowSession"],
            ),
            "a flag both set and pending",
        ),
        (
            table_json(
                one_version(
                    serde_json::json!({ "features": { "allowSession": true }, "evidence": { "allowSession": "x" }, "notes": "x" }),
                ),
                &[],
            ),
            "an unknown entry field",
        ),
    ];
    for (json, why) in cases {
        assert!(ShippedTable::parse(&json).is_err(), "must reject: {why}");
    }

    // Every flag must be accounted for: drop one from `pending`.
    let full = table_json(
        one_version(
            serde_json::json!({ "features": { "allowSession": true }, "evidence": { "allowSession": "x" } }),
        ),
        &[],
    );
    let mut v: serde_json::Value = serde_json::from_str(&full).unwrap();
    v["pending"].as_object_mut().unwrap().remove("denyAndStop");
    assert!(
        ShippedTable::parse(&v.to_string()).is_err(),
        "an unaccounted flag"
    );
    v["schema"] = serde_json::json!(2);
    assert!(ShippedTable::parse(&v.to_string()).is_err(), "schema 2");
}

#[test]
fn the_app_feature_lint() {
    let all_pending = |features: serde_json::Value, evidence: serde_json::Value, skip: &str| {
        let mut pending = serde_json::Map::new();
        for f in APP_FLAGS {
            if *f != skip && !features.as_object().unwrap().contains_key(*f) {
                pending.insert(f.to_string(), serde_json::json!("open"));
            }
        }
        serde_json::json!({ "schema": 1, "provenance": "t", "features": features, "evidence": evidence, "pending": pending })
            .to_string()
    };
    let ok = AppFeatures::parse(&all_pending(
        serde_json::json!({ "dockMenu": true }),
        serde_json::json!({ "dockMenu": "LP-29 passed" }),
        "",
    ))
    .expect("parses");
    assert!(ok.get("dockMenu"));
    assert!(!ok.get("notificationCenter"));

    for (json, why) in [
        (
            all_pending(
                serde_json::json!({ "dockMenu": true }),
                serde_json::json!({}),
                "",
            ),
            "no evidence",
        ),
        (
            all_pending(
                serde_json::json!({ "dockMenus": true }),
                serde_json::json!({ "dockMenus": "x" }),
                "",
            ),
            "unknown app flag",
        ),
        (
            all_pending(serde_json::json!({}), serde_json::json!({}), "dockMenu"),
            "neither set nor pending",
        ),
        (
            all_pending(
                serde_json::json!({}),
                serde_json::json!({ "dockMenu": "x" }),
                "",
            ),
            "dangling evidence",
        ),
    ] {
        assert!(AppFeatures::parse(&json).is_err(), "must reject: {why}");
    }
}
