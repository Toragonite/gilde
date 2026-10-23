//! The shipped table loader (§3.10 input 1).
//!
//! `resources/claude-compat.json` is written from the live-probe phase (§14.4):
//! per version, `conversation` / `paneHosting` = `verified` and the `features`
//! flags whose probe passed (§1.3 Status column). The table is the **only** way
//! a version reaches `Verified`.
//!
//! The loader lints the file so that every value stays traceable: each flag
//! and each verified capability carries an `evidence` string (the §1.3 row and
//! the R6 section), only §1.3 flags are accepted, `teamStoreVerified` (derived
//! from the canary) is never listed, and every flag is accounted for — set
//! for some version or listed under `pending` with its reason.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

use super::features::{is_claude_flag, CLAUDE_FLAGS, TEAM_STORE_VERIFIED};
use super::CompatDataError;
use crate::contract::compat::parse_version;

/// The bundled shipped table.
pub const BUNDLED_CLAUDE_COMPAT: &str = include_str!("../../resources/claude-compat.json");

/// A parsed `major.minor.patch`.
pub type Version = (u32, u32, u32);

/// One version of the shipped table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableEntry {
    /// As written in the file (`^\d+\.\d+\.\d+$`).
    pub version: String,
    pub conversation_verified: bool,
    pub pane_hosting_verified: bool,
    /// Provisional-clause flags; an explicit `false` = the probe contradicted it.
    pub features: BTreeMap<String, bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShippedTable {
    entries: BTreeMap<Version, TableEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTable {
    schema: u32,
    provenance: String,
    #[serde(default)]
    pending: BTreeMap<String, String>,
    versions: BTreeMap<String, RawEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawEntry {
    #[serde(default)]
    conversation: Option<Marked>,
    #[serde(default)]
    pane_hosting: Option<Marked>,
    #[serde(default)]
    features: BTreeMap<String, bool>,
    evidence: BTreeMap<String, String>,
}

/// The table can only mark a capability verified; `unverified` is the default
/// and `failed` only ever comes from observation.
#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "camelCase")]
enum Marked {
    Verified,
}

impl ShippedTable {
    pub fn parse(json: &str) -> Result<ShippedTable, CompatDataError> {
        let err = |m: String| CompatDataError::new(format!("claude-compat.json: {m}"));
        let raw: RawTable = serde_json::from_str(json).map_err(|e| err(e.to_string()))?;
        if raw.schema != 1 {
            return Err(err(format!("unsupported schema {}", raw.schema)));
        }
        if raw.provenance.trim().is_empty() {
            return Err(err("empty provenance".into()));
        }

        let mut entries = BTreeMap::new();
        let mut set_somewhere: BTreeSet<&str> = BTreeSet::new();
        for (version, e) in &raw.versions {
            let v = parse_version(version)
                .ok_or_else(|| err(format!("bad version key {version:?}")))?;
            let mut needs_evidence: BTreeSet<&str> = BTreeSet::new();
            if e.conversation.is_some() {
                needs_evidence.insert("conversation");
            }
            if e.pane_hosting.is_some() {
                needs_evidence.insert("paneHosting");
            }
            for flag in e.features.keys() {
                if !is_claude_flag(flag) {
                    return Err(err(format!("{version}: unknown Claude flag {flag:?}")));
                }
                if flag == TEAM_STORE_VERIFIED {
                    return Err(err(format!(
                        "{version}: {TEAM_STORE_VERIFIED} is derived from the canary's teamStore set and never shipped"
                    )));
                }
                needs_evidence.insert(flag.as_str());
                set_somewhere.insert(flag.as_str());
            }
            if needs_evidence.is_empty() {
                return Err(err(format!("{version}: an entry with nothing verified")));
            }
            for key in &needs_evidence {
                if e.evidence.get(*key).is_none_or(|s| s.trim().is_empty()) {
                    return Err(err(format!("{version}: {key:?} has no evidence")));
                }
            }
            if let Some(key) = e
                .evidence
                .keys()
                .find(|k| !needs_evidence.contains(k.as_str()))
            {
                return Err(err(format!(
                    "{version}: evidence for {key:?}, which is not set"
                )));
            }
            entries.insert(
                v,
                TableEntry {
                    version: version.clone(),
                    conversation_verified: matches!(e.conversation, Some(Marked::Verified)),
                    pane_hosting_verified: matches!(e.pane_hosting, Some(Marked::Verified)),
                    features: e.features.clone(),
                },
            );
        }

        for (flag, why) in &raw.pending {
            if !is_claude_flag(flag) || why.trim().is_empty() {
                return Err(err(format!("bad pending entry {flag:?}")));
            }
            if set_somewhere.contains(flag.as_str()) {
                return Err(err(format!("{flag:?} is both set and pending")));
            }
        }
        if let Some(flag) = CLAUDE_FLAGS.iter().find(|f| {
            **f != TEAM_STORE_VERIFIED
                && !set_somewhere.contains(**f)
                && !raw.pending.contains_key(**f)
        }) {
            return Err(err(format!(
                "{flag:?} is neither set for a version nor pending"
            )));
        }
        Ok(ShippedTable { entries })
    }

    /// The bundled table. The T-compat tests parse it, so a failure here is a
    /// build defect, never a user condition.
    pub fn bundled() -> ShippedTable {
        ShippedTable::parse(BUNDLED_CLAUDE_COMPAT)
            .expect("the bundled claude-compat.json must parse")
    }

    pub fn get(&self, version: &str) -> Option<&TableEntry> {
        self.get_exact(parse_version(version)?)
    }

    pub fn get_exact(&self, version: Version) -> Option<&TableEntry> {
        self.entries.get(&version)
    }

    /// Table entries strictly below `version`, oldest first.
    pub fn entries_below(&self, version: Version) -> impl Iterator<Item = &TableEntry> {
        self.entries.range(..version).map(|(_, e)| e)
    }

    /// Every entry, oldest first.
    pub fn entries(&self) -> impl Iterator<Item = (&Version, &TableEntry)> {
        self.entries.iter()
    }
}
