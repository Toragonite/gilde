//! Episodes and "since" (§3.5.8).
//!
//! Owner: T02. An episode starts when `derive` first returns a state kind (or
//! a different `ErrorSub`) and ends when it returns another; re-deriving the
//! same kind does not start a new one. A wait that flips approval ↔ answer
//! within the same wait keeps its episode (and so its notification key, §8.4)
//! [A §8.2]. `since` is, for waits, the earliest `received_at` / registry
//! `status_updated_at` / prompt first-seen time, otherwise the episode start.

use crate::contract::activity::{ActivityState, DidntStartReason, ErrorSub};
use crate::contract::common::EpochMs;
use crate::contract::ids::{EpisodeId, SessionUid};

/// The episode class of a state: the part of the state that, when it
/// changes, starts a new episode.
pub fn class_of(state: &ActivityState) -> String {
    match state {
        // Approval → answer within the same wait keeps the episode.
        ActivityState::NeedsApproval | ActivityState::NeedsAnswer => "wait".to_string(),
        ActivityState::Error { sub } => match sub {
            ErrorSub::UsageLimit { .. } => "error:usageLimit".to_string(),
            ErrorSub::SignInNeeded => "error:signInNeeded".to_string(),
            ErrorSub::AgentError => "error:agentError".to_string(),
            ErrorSub::DidntStart { reason } => format!(
                "error:didntStart:{}",
                match reason {
                    DidntStartReason::Timeout => "timeout",
                    DidntStartReason::Exited => "exited",
                    DidntStartReason::CompatFailed => "compatFailed",
                    DidntStartReason::MinVersion => "minVersion",
                }
            ),
        },
        other => other.kind_str().to_string(),
    }
}

/// Convert a wall instant to the uptime domain at observation time (facts
/// from the past keep their age; the uptime clock excludes sleep, §3.5.2).
pub fn up_of(wall: EpochMs, now_wall: EpochMs, now_up: u64) -> u64 {
    let age = now_wall.saturating_sub(wall).max(0);
    now_up.saturating_sub(u64::try_from(age).unwrap_or(0))
}

/// One session's current episode.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EpisodeTracker {
    class: Option<String>,
    entered_at: EpochMs,
    entered_up: u64,
    id: Option<EpisodeId>,
    since: EpochMs,
    since_up: u64,
}

/// What [`EpisodeTracker::observe`] decided.
#[derive(Debug, Clone, PartialEq)]
pub struct EpisodeView {
    pub id: EpisodeId,
    /// The episode started with this observation.
    pub entered: bool,
    /// Wall clock, for display.
    pub since: EpochMs,
    /// Uptime of `since` (for `waited_ms` and Q4's oldest-first).
    pub since_up: u64,
}

impl EpisodeTracker {
    /// Observe a derived state; returns the episode it belongs to.
    pub fn observe(
        &mut self,
        uid: &SessionUid,
        state: &ActivityState,
        since_hint: Option<EpochMs>,
        now_wall: EpochMs,
        now_up: u64,
    ) -> EpisodeView {
        let class = class_of(state);
        let entered = self.class.as_deref() != Some(class.as_str()) || self.id.is_none();
        if entered {
            self.class = Some(class.clone());
            self.entered_at = now_wall;
            self.entered_up = now_up;
            self.id = Some(EpisodeId::derive(uid, &class, now_wall));
            self.since = now_wall;
            self.since_up = now_up;
        }
        let is_wait = matches!(state, ActivityState::NeedsApproval | ActivityState::NeedsAnswer);
        if is_wait {
            // The earliest wait time seen in this episode; never later than
            // the episode start would make it, never in the future.
            let hint = since_hint.unwrap_or(self.entered_at).min(now_wall);
            if entered || hint < self.since {
                self.since = hint;
                self.since_up = up_of(hint, now_wall, now_up).min(self.entered_up);
            }
        }
        EpisodeView {
            id: self.id.clone().expect("episode id set above"),
            entered,
            since: self.since,
            since_up: self.since_up,
        }
    }

    pub fn current(&self) -> Option<&EpisodeId> {
        self.id.as_ref()
    }

    pub fn entered_up(&self) -> u64 {
        self.entered_up
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_kind_keeps_the_episode_and_approval_to_answer_too() {
        let uid = SessionUid::mint();
        let mut t = EpisodeTracker::default();
        let a = t.observe(&uid, &ActivityState::Working, None, 1_000, 10);
        assert!(a.entered);
        let b = t.observe(&uid, &ActivityState::Working, None, 2_000, 20);
        assert!(!b.entered);
        assert_eq!(a.id, b.id);
        let c = t.observe(&uid, &ActivityState::NeedsApproval, Some(2_500), 3_000, 30);
        assert!(c.entered);
        assert_eq!(c.since, 2_500);
        let d = t.observe(&uid, &ActivityState::NeedsAnswer, Some(2_900), 4_000, 40);
        assert!(!d.entered);
        assert_eq!(d.id, c.id);
        assert_eq!(d.since, 2_500);
    }

    #[test]
    fn a_different_error_sub_is_a_new_episode() {
        let uid = SessionUid::mint();
        let mut t = EpisodeTracker::default();
        let a = t.observe(&uid, &ActivityState::Error { sub: ErrorSub::AgentError }, None, 1_000, 1);
        let b = t.observe(&uid, &ActivityState::Error { sub: ErrorSub::SignInNeeded }, None, 2_000, 2);
        assert!(b.entered);
        assert_ne!(a.id, b.id);
        let c = t.observe(&uid, &ActivityState::NoProgress { for_secs: 300 }, None, 3_000, 3);
        let d = t.observe(&uid, &ActivityState::NoProgress { for_secs: 360 }, None, 4_000, 4);
        assert!(c.entered && !d.entered);
    }

    #[test]
    fn up_of_keeps_ages_and_never_underflows() {
        assert_eq!(up_of(9_000, 10_000, 50_000), 49_000);
        assert_eq!(up_of(0, 10_000, 5), 0);
        assert_eq!(up_of(11_000, 10_000, 50_000), 50_000);
    }
}
