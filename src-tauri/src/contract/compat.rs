//! ClaudeCompat **[meta]** (contract §3.4.11, rules §3.10) [C2, A1].
//!
//! One record per installed Claude version, keyed on the binary actually
//! spawned. Inputs in order: the shipped table (the only way to `Verified`),
//! the static literal canary, the runtime `initialize` handshake, runtime
//! deviations — a later input may only lower a state.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::contract::common::CompatState;

/// The minimum Claude version for Conversation mode [C3].
pub const MIN_CONVERSATION_VERSION: (u32, u32, u32) = (2, 1, 268);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct ClaudeCompat {
    /// "2.1.306"; `^\d+\.\d+\.\d+$`.
    pub version: String,
    pub conversation: CompatState,
    pub pane_hosting: CompatState,
    /// The input that set the current values (§3.10).
    pub source: CompatSource,
    pub static_canary: CanaryResult,
    /// Runtime `initialize` handshake result (None = not yet run).
    pub handshake_ok: Option<bool>,
    /// version < 2.1.268 → Conversation not offered [C3].
    pub below_minimum: bool,
    /// First deviation, e.g. "unknown shim subcommand: display-message".
    pub note: Option<String>,
    /// Provisional-clause flags (§1.3 "Claude flag" column); absent = fallback.
    pub features: BTreeMap<String, bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum CompatSource {
    Shipped,
    StaticCanary,
    Handshake,
    Observed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum CanaryResult {
    NotRun,
    Pass,
    Missing { literals: Vec<String> },
    /// The scan could not run (unreadable binary, timeout, not Mach-O [N-07])
    /// = stays `Unverified` with the Details note.
    Unknown,
}

/// The capabilities a deviation or handshake can lower (§3.10 input 3–4).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CompatCapability {
    Conversation,
    PaneHosting,
    /// A provisional-clause flag of §1.3, e.g. `allowSession`.
    Feature(String),
}

/// Parse `^\d+\.\d+\.\d+$` into a comparable triple.
pub fn parse_version(v: &str) -> Option<(u32, u32, u32)> {
    let mut it = v.split('.');
    let parse = |s: Option<&str>| -> Option<u32> {
        let s = s?;
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) || s.len() > 9 {
            return None;
        }
        s.parse().ok()
    };
    let t = (parse(it.next())?, parse(it.next())?, parse(it.next())?);
    if it.next().is_some() {
        return None;
    }
    Some(t)
}

/// Whether `version` is below the Conversation minimum (§3.10 *Minimum version*).
/// An unparseable version is treated as below the minimum (stricter reading).
pub fn below_minimum(version: &str) -> bool {
    parse_version(version).is_none_or(|v| v < MIN_CONVERSATION_VERSION)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse_version("2.1.306"), Some((2, 1, 306)));
        assert_eq!(parse_version("2.1"), None);
        assert_eq!(parse_version("2.1.3.4"), None);
        assert_eq!(parse_version("2.1.x"), None);
        assert!(below_minimum("2.1.267"));
        assert!(!below_minimum("2.1.268"));
        assert!(!below_minimum("2.2.0"));
        assert!(below_minimum("garbage"));
    }
}
