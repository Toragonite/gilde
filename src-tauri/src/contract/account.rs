//! Account **[meta]** (contract §3.4.3).
//!
//! Invariants: agy has exactly one account (`AGY_HOME` is ignored); a Claude
//! `config_dir` given by the user is accepted only if it is a directory owned
//! by the user and contains `sessions/` or `projects/` (checked with `stat`);
//! a Discovered sibling is a `~/.claude-*` directory that contains
//! `.claude.json` (checked with `stat`, never opened) [C7]; the default Claude
//! account is `~/.claude`, launched with `CLAUDE_CONFIG_DIR` **unset** [C8];
//! `used_pct` outside 0..=100 → `None`, never clamped.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::contract::common::{AbsPath, EpochMs, Vendor};
use crate::contract::ids::AccountId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum AccountSource {
    Ambient,
    User,
    Orchestrator,
    /// A `~/.claude-*` sibling, read-only [C7, §5.1].
    Discovered,
}

/// From agent-mesh `authStatus`; never a token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum SignIn {
    SignedIn,
    SignedOut,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct Account {
    pub id: AccountId,
    pub vendor: Vendor,
    /// Claude `CLAUDE_CONFIG_DIR` / `~/.claude`; Codex `CODEX_HOME` / `~/.codex`;
    /// `None` for the cursor/agy default login.
    pub config_dir: Option<AbsPath>,
    /// User label or "Default"; ≤ 64 chars.
    pub label: String,
    /// Exactly one per vendor among usable accounts.
    pub is_default: bool,
    pub source: AccountSource,
    /// False for Discovered until the user confirms it (`account_confirm`).
    pub usable_for_new: bool,
    pub sign_in: SignIn,
    pub quota: Option<Quota>,
    pub live_sessions: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum QuotaWindowKind {
    FiveHour,
    Week,
    Other,
}

/// Why no quota could be read (no secrets, no prose).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum QuotaUnavailable {
    /// The vendor or adapter reports no quota.
    NotReported,
    /// Signed out.
    SignedOut,
    /// The helper (sidecar) is down.
    SidecarDown,
    /// The probe failed.
    ProbeFailed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct Quota {
    pub windows: Vec<QuotaWindow>,
    #[ts(type = "number")]
    pub fetched_at: EpochMs,
    pub stale: bool,
    pub unavailable: Option<QuotaUnavailable>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct QuotaWindow {
    pub kind: QuotaWindowKind,
    pub label: Option<String>,
    /// 0..=100; out of range → `None` (absent), never clamped.
    pub used_pct: Option<u8>,
    #[ts(type = "number | null")]
    pub resets_at: Option<EpochMs>,
}

impl QuotaWindow {
    /// Normalise a vendor percentage at the adapter boundary (§3.4.3, A §13.3):
    /// values in `0.0..=1.0` are fractions when `fraction` is true; anything
    /// outside `0..=100` after scaling is absent.
    pub fn normalise_pct(raw: f64, fraction: bool) -> Option<u8> {
        if !raw.is_finite() {
            return None;
        }
        let pct = if fraction {
            if !(0.0..=1.0).contains(&raw) {
                return None;
            }
            raw * 100.0
        } else {
            raw
        };
        if (0.0..=100.0).contains(&pct) {
            Some(pct.round() as u8)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_range_is_absent_not_clamped() {
        assert_eq!(QuotaWindow::normalise_pct(0.42, true), Some(42));
        assert_eq!(QuotaWindow::normalise_pct(42.0, false), Some(42));
        assert_eq!(QuotaWindow::normalise_pct(101.0, false), None);
        assert_eq!(QuotaWindow::normalise_pct(-1.0, false), None);
        assert_eq!(QuotaWindow::normalise_pct(1.5, true), None);
        assert_eq!(QuotaWindow::normalise_pct(f64::NAN, false), None);
    }
}
