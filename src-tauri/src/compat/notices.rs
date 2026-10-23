//! The compat notices (§3.10 *Notices*, strings in §9.12 / ux-ia).
//!
//! - `below_minimum` → `notice.conversationMinVersion`; otherwise
//!   `conversation: Failed` → `notice.conversationUnavailable` (the New-session
//!   sheet shows one line and preselects Terminal);
//! - `paneHosting: Failed` → `team.inProcess` (ux-ia #103, on the Team toggle
//!   and the team header);
//! - `Unverified` → no notice; Details says `details.unverifiedVersion`;
//! - team files not verified for the version → Details says
//!   `details.teamFormatUnverified` (§5.2.4).
//!
//! `OutsideGilde` (ux-ia #104) is a per-team observation, not a version fact,
//! so it is not produced here.

use crate::contract::common::{CompatState, Notice};
use crate::contract::compat::ClaudeCompat;

use super::features::TEAM_STORE_VERIFIED;

pub const CONVERSATION_MIN_VERSION: &str = "notice.conversationMinVersion";
pub const CONVERSATION_UNAVAILABLE: &str = "notice.conversationUnavailable";
pub const TEAM_IN_PROCESS: &str = "team.inProcess";
pub const DETAILS_UNVERIFIED_VERSION: &str = "details.unverifiedVersion";
pub const DETAILS_TEAM_FORMAT_UNVERIFIED: &str = "details.teamFormatUnverified";

/// The New-session / team notices for one version record.
pub fn notices_for(c: &ClaudeCompat) -> Vec<Notice> {
    let mut out = Vec::new();
    if c.below_minimum {
        out.push(Notice::new(CONVERSATION_MIN_VERSION));
    } else if c.conversation == CompatState::Failed {
        out.push(Notice::new(CONVERSATION_UNAVAILABLE));
    }
    if c.pane_hosting == CompatState::Failed {
        out.push(Notice::new(TEAM_IN_PROCESS));
    }
    out
}

/// The compat lines of a session's ⋯ Details (no notice is shown for them).
pub fn details_lines(c: &ClaudeCompat) -> Vec<Notice> {
    let mut out = Vec::new();
    if c.conversation == CompatState::Unverified || c.pane_hosting == CompatState::Unverified {
        out.push(Notice::new(DETAILS_UNVERIFIED_VERSION));
    }
    if c.features.get(TEAM_STORE_VERIFIED) != Some(&true) {
        out.push(Notice::new(DETAILS_TEAM_FORMAT_UNVERIFIED));
    }
    out
}
