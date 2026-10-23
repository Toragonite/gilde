//! Provisional flags with forward inheritance (§3.10 *Features*) and the
//! per-build app features (`resources/app-features.json`).
//!
//! A provisional clause of §1.3 is switched by data, never by a code branch
//! added later (§15.1): the Claude flags live per installed version in
//! `claude-compat.json`, the app flags per build in `app-features.json`.
//! An absent flag means "fallback".

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

use super::table::{ShippedTable, Version};
use super::CompatDataError;

/// Every "Claude flag" of the §1.3 table, in clause order. A table entry naming
/// anything else is rejected (typo protection).
pub const CLAUDE_FLAGS: &[&str] = &[
    "handshakeShape",             // P1
    "sessionIdHonoured",          // P2
    "permissionPromptsHost",      // P3
    "allowSession",               // P4
    "denyAndStop",                // P5
    "resumeRearm",                // P6
    "stdinEofCancel",             // P7
    "idleStdinExit",              // P8
    "nowPriority",                // P9
    "interruptReceipt",           // P9
    "sessionStateEvents",         // P9
    "planAnswer",                 // P10
    "freeTextAnswer",             // P10
    "parallelRequests",           // P11
    "errorReply",                 // P11
    "sandboxAsk",                 // P11
    "getUsageLive",               // P12
    "ruleAllowMarker",            // P13
    "entrypointMarker",           // P14
    "l2SpawnSeam",                // P15
    "teamStoreVerified",          // P17 (derived from the canary, never shipped)
    "paneHostingVerified",        // P18
    "leadPromptsForTeammates",    // P19
    "screenDigitNoCr",            // P20
    "idlePerTurn",                // P22
    "teammateQuestionLocation",   // P22
    "sendMessageMsgId",           // P22
    "inFlightSignals",            // P23
    "bgAttach",                   // P28
    "continueOtherAccountResume", // P31
    "transcriptUsage",            // P32
];

/// The flag the static canary's `teamStore` set decides per binary (§3.10
/// input 2, §5.2.4): present → true, missing → false, not scanned → absent.
pub const TEAM_STORE_VERIFIED: &str = "teamStoreVerified";

/// Every "app flag" of the §1.3 table (§3.10 *Per-build app features*).
pub const APP_FLAGS: &[&str] = &[
    "vscodeWindowFocus",  // P30, LP-24
    "notificationCenter", // P34, LP-28
    "dockMenu",           // P35, LP-29
    "ideGotoWindow",      // P37, LP-36
    "worktreesInRepo",    // P41, LP-40 (A6, A17.2)
];

pub fn is_claude_flag(flag: &str) -> bool {
    CLAUDE_FLAGS.contains(&flag)
}

/// The flags of one installed version (§3.10 *Features*).
///
/// - the version's own table entry always applies;
/// - **forward inheritance**: when `inherit` holds (the installed binary's
///   static canary passed and no runtime deviation was recorded for the
///   version), a flag the version's own entry does not name takes the value
///   of the newest table version *below* it that names the flag — an explicit
///   `false` there stops an older `true`;
/// - `team_store` (the canary's `teamStore` set) sets [`TEAM_STORE_VERIFIED`];
/// - `runtime_off` (deviations, e.g. the AllowSession settings-file guard)
///   lowers a flag to `false` last, so nothing re-raises it.
pub fn effective(
    table: &ShippedTable,
    version: Version,
    inherit: bool,
    team_store: Option<bool>,
    runtime_off: &BTreeSet<String>,
) -> BTreeMap<String, bool> {
    let mut out = BTreeMap::new();
    if inherit {
        // Oldest first, so the newest version below that names a flag wins.
        for entry in table.entries_below(version) {
            for (flag, on) in &entry.features {
                out.insert(flag.clone(), *on);
            }
        }
    }
    if let Some(own) = table.get_exact(version) {
        for (flag, on) in &own.features {
            out.insert(flag.clone(), *on);
        }
    }
    if let Some(present) = team_store {
        out.insert(TEAM_STORE_VERIFIED.to_string(), present);
    }
    for flag in runtime_off {
        out.insert(flag.clone(), false);
    }
    out
}

// ---------------------------------------------------------------------------
// app-features.json
// ---------------------------------------------------------------------------

/// The bundled per-build app features.
pub const BUNDLED_APP_FEATURES: &str = include_str!("../../resources/app-features.json");

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppFeatures {
    features: BTreeMap<String, bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAppFeatures {
    schema: u32,
    /// Documentation inside the data file; required so every file says where
    /// its values come from.
    provenance: String,
    features: BTreeMap<String, bool>,
    evidence: BTreeMap<String, String>,
    #[serde(default)]
    pending: BTreeMap<String, String>,
}

impl AppFeatures {
    /// Parse and lint `app-features.json`: known flags only, every set flag
    /// carries evidence, every known flag is either set or pending.
    pub fn parse(json: &str) -> Result<AppFeatures, CompatDataError> {
        let err = |m: String| CompatDataError::new(format!("app-features.json: {m}"));
        let raw: RawAppFeatures = serde_json::from_str(json).map_err(|e| err(e.to_string()))?;
        if raw.schema != 1 {
            return Err(err(format!("unsupported schema {}", raw.schema)));
        }
        if raw.provenance.trim().is_empty() {
            return Err(err("empty provenance".into()));
        }
        for flag in raw.features.keys() {
            if !APP_FLAGS.contains(&flag.as_str()) {
                return Err(err(format!("unknown app flag {flag:?}")));
            }
            if raw.evidence.get(flag).is_none_or(|e| e.trim().is_empty()) {
                return Err(err(format!("{flag:?} has no evidence")));
            }
        }
        if let Some(key) = raw.evidence.keys().find(|k| !raw.features.contains_key(*k)) {
            return Err(err(format!("evidence for {key:?}, which is not set")));
        }
        for (flag, why) in &raw.pending {
            if !APP_FLAGS.contains(&flag.as_str()) || why.trim().is_empty() {
                return Err(err(format!("bad pending entry {flag:?}")));
            }
            if raw.features.contains_key(flag) {
                return Err(err(format!("{flag:?} is both set and pending")));
            }
        }
        if let Some(flag) = APP_FLAGS
            .iter()
            .find(|f| !raw.features.contains_key(**f) && !raw.pending.contains_key(**f))
        {
            return Err(err(format!("{flag:?} is neither set nor pending")));
        }
        Ok(AppFeatures {
            features: raw.features,
        })
    }

    /// The bundled file. The T-compat tests parse it, so a failure here is a
    /// build defect, never a user condition.
    pub fn bundled() -> AppFeatures {
        AppFeatures::parse(BUNDLED_APP_FEATURES).expect("the bundled app-features.json must parse")
    }

    /// On only when this build's file sets it true; absent = fallback.
    pub fn get(&self, flag: &str) -> bool {
        self.features.get(flag).copied().unwrap_or(false)
    }

    pub fn flags(&self) -> &BTreeMap<String, bool> {
        &self.features
    }
}
