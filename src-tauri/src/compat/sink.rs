//! Handshake and deviation evidence (`CompatSink`, §3.10 inputs 3–4) and the
//! `claude_compat_observed` rows that keep it and the canary cache (§3.9).
//!
//! Runtime evidence only ever **lowers** a version: a failed handshake or a
//! deviation flips a capability to `Failed` at once for new sessions of that
//! version; nothing raises it again (a later `ok` handshake does not undo a
//! failure). Running sessions keep the record they resolved at spawn [A1].
//!
//! Rows (`PRIMARY KEY (version, binary_fingerprint)`, F-22):
//! - one **canary row** per scanned binary: `binary_fingerprint` =
//!   `"<size>:<mtime_ms>:<inode>"`, `source = static_canary`, `static_result`
//!   = this module's JSON (`{setsHash, missing}`);
//! - one **version row** for the runtime evidence: `binary_fingerprint` =
//!   [`VERSION_ROW`] (`""`), `source = handshake | observed`.
//!
//! Notes are short protocol descriptions supplied by the transports (e.g.
//! "unknown shim subcommand: display-message"); they are capped and stripped
//! of control characters here, and never carry content.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::canary::{CanarySet, ScanOutcome};
use crate::contract::common::CompatState;
use crate::contract::compat::{CompatCapability, CompatSource};
use crate::contract::session::BinaryFingerprint;

/// `binary_fingerprint` of a version-level row (handshake, deviations, notes).
pub const VERSION_ROW: &str = "";

/// Notes are cut to this many bytes (on a char boundary).
pub const NOTE_MAX_BYTES: usize = 200;

/// A note fit for Details and diagnostics: control characters become spaces,
/// the length is capped.
pub fn clean_note(note: &str) -> String {
    let mut out: String = note
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_string();
    if out.len() > NOTE_MAX_BYTES {
        let mut cut = NOTE_MAX_BYTES;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
    }
    out
}

/// Runtime evidence for one version (inputs 3–4).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RuntimeEvidence {
    /// The `initialize` handshake (None = not yet run). `Some(false)` sticks.
    pub handshake_ok: Option<bool>,
    /// A deviation flipped `conversation` (§6.1.9).
    pub conversation_failed: bool,
    /// A deviation flipped `paneHosting` (§7.4.9).
    pub pane_hosting_failed: bool,
    /// Features a deviation turned off (e.g. `allowSession`, §6.1.4 guard).
    pub features_off: BTreeSet<String>,
    /// The first failure: a deviation, or the failed handshake's reason.
    pub failure_note: Option<String>,
    /// The first plain note (degraded card, `system/init` version race).
    pub plain_note: Option<String>,
}

impl RuntimeEvidence {
    /// Any runtime deviation recorded: the version then inherits no features
    /// from older table versions (§3.10 *Features*) and its source is `Observed`.
    pub fn deviated(&self) -> bool {
        self.conversation_failed || self.pane_hosting_failed || !self.features_off.is_empty()
    }

    /// The note shown for the version: the first failure, else the first note.
    pub fn note(&self) -> Option<&String> {
        self.failure_note.as_ref().or(self.plain_note.as_ref())
    }

    /// Returns whether anything changed.
    pub fn apply_handshake(&mut self, ok: bool, note: Option<&str>) -> bool {
        let before = self.clone();
        match (self.handshake_ok, ok) {
            (Some(false), _) => {} // a failure sticks
            (_, true) => self.handshake_ok = Some(true),
            (_, false) => {
                self.handshake_ok = Some(false);
                if self.failure_note.is_none() {
                    self.failure_note = note.map(clean_note).filter(|n| !n.is_empty());
                }
            }
        }
        *self != before
    }

    pub fn apply_deviation(&mut self, capability: &CompatCapability, note: &str) -> bool {
        let before = self.clone();
        match capability {
            CompatCapability::Conversation => self.conversation_failed = true,
            CompatCapability::PaneHosting => self.pane_hosting_failed = true,
            CompatCapability::Feature(flag) => {
                self.features_off.insert(flag.clone());
            }
        }
        if self.failure_note.is_none() {
            let n = clean_note(note);
            if !n.is_empty() {
                self.failure_note = Some(n);
            }
        }
        *self != before
    }

    pub fn apply_note(&mut self, note: &str) -> bool {
        if self.plain_note.is_some() {
            return false;
        }
        let n = clean_note(note);
        if n.is_empty() {
            return false;
        }
        self.plain_note = Some(n);
        true
    }
}

// ---------------------------------------------------------------------------
// rows
// ---------------------------------------------------------------------------

/// One `claude_compat_observed` row (§3.9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedRow {
    pub version: String,
    /// `"<size>:<mtime_ms>:<inode>"`, or [`VERSION_ROW`].
    pub binary_fingerprint: String,
    pub conversation: Option<CompatState>,
    pub pane_hosting: Option<CompatState>,
    /// `StaticCanary`, `Handshake` or `Observed` (never `Shipped`).
    pub source: CompatSource,
    pub static_result: Option<String>,
    pub handshake_ok: Option<bool>,
    pub features_json: Option<String>,
    pub note: Option<String>,
    pub observed_at: i64,
}

/// `static_result` of a canary row: the scanned entry list's hash and the
/// labels that were missing, per set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StoredScan {
    pub sets_hash: String,
    pub missing: BTreeMap<CanarySet, Vec<String>>,
}

pub fn fingerprint_key(fp: &BinaryFingerprint) -> String {
    fp.key()
}

pub fn parse_fingerprint_key(key: &str) -> Option<BinaryFingerprint> {
    let mut it = key.split(':');
    let fp = BinaryFingerprint {
        size: it.next()?.parse().ok()?,
        mtime_ms: it.next()?.parse().ok()?,
        inode: it.next()?.parse().ok()?,
    };
    it.next().is_none().then_some(fp)
}

fn state_text(s: CompatState) -> &'static str {
    match s {
        CompatState::Verified => "verified",
        CompatState::Unverified => "unverified",
        CompatState::Failed => "failed",
    }
}

fn state_from(s: &str) -> Option<CompatState> {
    match s {
        "verified" => Some(CompatState::Verified),
        "unverified" => Some(CompatState::Unverified),
        "failed" => Some(CompatState::Failed),
        _ => None,
    }
}

fn source_text(s: CompatSource) -> &'static str {
    match s {
        CompatSource::Shipped => "shipped",
        CompatSource::StaticCanary => "static_canary",
        CompatSource::Handshake => "handshake",
        CompatSource::Observed => "observed",
    }
}

fn source_from(s: &str) -> Option<CompatSource> {
    match s {
        "static_canary" => Some(CompatSource::StaticCanary),
        "handshake" => Some(CompatSource::Handshake),
        "observed" => Some(CompatSource::Observed),
        _ => None,
    }
}

/// The canary row for one finished scan (`None` for an `Unknown` outcome:
/// those are never persisted, so a later boot scans again).
pub fn canary_row(
    version: &str,
    fp: &BinaryFingerprint,
    outcome: &ScanOutcome,
    sets_hash: &str,
    now_ms: i64,
) -> Option<ObservedRow> {
    let ScanOutcome::Scanned { missing } = outcome else {
        return None;
    };
    let state = |set: CanarySet| {
        Some(if missing.contains_key(&set) {
            CompatState::Failed
        } else {
            CompatState::Unverified
        })
    };
    let stored = StoredScan {
        sets_hash: sets_hash.to_string(),
        missing: missing.clone(),
    };
    let team: BTreeMap<&str, bool> = BTreeMap::from([(
        super::features::TEAM_STORE_VERIFIED,
        !missing.contains_key(&CanarySet::TeamStore),
    )]);
    Some(ObservedRow {
        version: version.to_string(),
        binary_fingerprint: fingerprint_key(fp),
        conversation: state(CanarySet::Conversation),
        pane_hosting: state(CanarySet::PaneHosting),
        source: CompatSource::StaticCanary,
        static_result: serde_json::to_string(&stored).ok(),
        handshake_ok: None,
        features_json: serde_json::to_string(&team).ok(),
        note: None,
        observed_at: now_ms,
    })
}

/// The version row for one version's runtime evidence.
pub fn version_row(version: &str, rt: &RuntimeEvidence, now_ms: i64) -> ObservedRow {
    let conversation_failed = rt.conversation_failed || rt.handshake_ok == Some(false);
    let features: BTreeMap<&str, bool> = rt
        .features_off
        .iter()
        .map(|f| (f.as_str(), false))
        .collect();
    ObservedRow {
        version: version.to_string(),
        binary_fingerprint: VERSION_ROW.to_string(),
        conversation: conversation_failed.then_some(CompatState::Failed),
        pane_hosting: rt.pane_hosting_failed.then_some(CompatState::Failed),
        source: if rt.deviated() {
            CompatSource::Observed
        } else {
            CompatSource::Handshake
        },
        static_result: None,
        handshake_ok: rt.handshake_ok,
        features_json: (!features.is_empty())
            .then(|| serde_json::to_string(&features).unwrap_or_default()),
        note: rt.note().cloned(),
        observed_at: now_ms,
    }
}

/// Rebuild the runtime evidence of a version row.
pub fn runtime_from_row(row: &ObservedRow) -> RuntimeEvidence {
    let observed = row.source == CompatSource::Observed;
    let features_off = row
        .features_json
        .as_deref()
        .and_then(|j| serde_json::from_str::<BTreeMap<String, bool>>(j).ok())
        .map(|m| {
            m.into_iter()
                .filter(|(_, on)| !on)
                .map(|(f, _)| f)
                .collect()
        })
        .unwrap_or_default();
    let failed = observed || row.handshake_ok == Some(false);
    RuntimeEvidence {
        handshake_ok: row.handshake_ok,
        conversation_failed: observed && row.conversation == Some(CompatState::Failed),
        pane_hosting_failed: row.pane_hosting == Some(CompatState::Failed),
        features_off,
        failure_note: if failed { row.note.clone() } else { None },
        plain_note: if failed { None } else { row.note.clone() },
    }
}

/// The scan a canary row records, if it was made with `sets_hash`.
pub fn scan_from_row(row: &ObservedRow, sets_hash: &str) -> Option<ScanOutcome> {
    let stored: StoredScan = serde_json::from_str(row.static_result.as_deref()?).ok()?;
    (stored.sets_hash == sets_hash).then_some(ScanOutcome::Scanned {
        missing: stored.missing,
    })
}

// ---------------------------------------------------------------------------
// persistence
// ---------------------------------------------------------------------------

/// Where observed rows live. The production implementation is `state.db`
/// ([`crate::store::Db`]); tests may use memory. Calls come from the canary
/// worker thread or from boot code, never from the async runtime.
pub trait ObservedStore: Send + Sync {
    fn load(&self) -> Result<Vec<ObservedRow>, String>;
    fn save(&self, row: &ObservedRow) -> Result<(), String>;
}

/// Every row of `claude_compat_observed` (unknown `source` values skipped).
pub fn load_rows(conn: &Connection) -> rusqlite::Result<Vec<ObservedRow>> {
    let mut stmt = conn.prepare(
        "SELECT version, binary_fingerprint, conversation, pane_hosting, source, static_result, \
         handshake_ok, features_json, note, observed_at FROM claude_compat_observed",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, Option<String>>(5)?,
            r.get::<_, Option<i64>>(6)?,
            r.get::<_, Option<String>>(7)?,
            r.get::<_, Option<String>>(8)?,
            r.get::<_, i64>(9)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (version, fp, conv, pane, source, static_result, hs, features_json, note, at) = row?;
        let Some(source) = source_from(&source) else {
            continue;
        };
        out.push(ObservedRow {
            version,
            binary_fingerprint: fp.unwrap_or_default(),
            conversation: conv.as_deref().and_then(state_from),
            pane_hosting: pane.as_deref().and_then(state_from),
            source,
            static_result,
            handshake_ok: hs.map(|v| v != 0),
            features_json,
            note,
            observed_at: at,
        });
    }
    Ok(out)
}

/// Insert or replace the row for `(version, binary_fingerprint)`.
pub fn upsert_row(conn: &Connection, row: &ObservedRow) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO claude_compat_observed (version, conversation, pane_hosting, source, \
         binary_fingerprint, static_result, handshake_ok, features_json, note, observed_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
         ON CONFLICT (version, binary_fingerprint) DO UPDATE SET \
         conversation = excluded.conversation, pane_hosting = excluded.pane_hosting, \
         source = excluded.source, static_result = excluded.static_result, \
         handshake_ok = excluded.handshake_ok, features_json = excluded.features_json, \
         note = excluded.note, observed_at = excluded.observed_at",
        params![
            row.version,
            row.conversation.map(state_text),
            row.pane_hosting.map(state_text),
            source_text(row.source),
            row.binary_fingerprint,
            row.static_result,
            row.handshake_ok.map(i64::from),
            row.features_json,
            row.note,
            row.observed_at,
        ],
    )?;
    Ok(())
}

/// Whether `claude_compat_observed` exists (state.db migration v2, T09).
pub fn table_exists(conn: &Connection) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'claude_compat_observed'",
        [],
        |_| Ok(()),
    )
    .optional()
    .map(|r| r.is_some())
}

/// `state.db` as the observed-row store. Before migration v2 (T09) creates the
/// table, loads return nothing and saves are skipped: compat then lives in
/// memory for the run, which only costs a rescan at the next boot.
impl ObservedStore for crate::store::Db {
    fn load(&self) -> Result<Vec<ObservedRow>, String> {
        self.blocking(|conn| {
            if !table_exists(conn)? {
                return Ok(Vec::new());
            }
            Ok(load_rows(conn)?)
        })
        .map_err(|e| e.to_string())
    }

    fn save(&self, row: &ObservedRow) -> Result<(), String> {
        self.blocking(|conn| {
            if table_exists(conn)? {
                upsert_row(conn, row)?;
            }
            Ok(())
        })
        .map_err(|e| e.to_string())
    }
}
