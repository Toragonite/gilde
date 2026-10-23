//! Session status, the evidence behind it, node badges and node metrics.
//!
//! DESIGN §6-4 and §9-5. Status is never reported without evidence: the UI has
//! to be able to say *why* it thinks a session is idle, because TUI prompt
//! matching is structurally version-fragile and the design refuses to hide it.

use serde::{Deserialize, Serialize};

/// Where a session is, as inferred from three independent signal sources
/// (output dynamics, OS foreground-group CPU, visible-text tail patterns).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SessionStatus {
    /// Spawned; no output observed yet.
    Starting,
    /// A prompt is up and nothing is running.
    Idle,
    /// Actively producing output or burning CPU.
    #[serde(rename_all = "camelCase")]
    Working {
        since: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hint: Option<String>,
    },
    /// A permission/confirmation dialog is up. The only status that needs a human.
    #[serde(rename_all = "camelCase")]
    WaitingForInput { prompt: String },
    /// Quiet, no CPU, and no recognised prompt.
    #[serde(rename_all = "camelCase")]
    Stalled { quiet_ms: u64 },
    #[serde(rename_all = "camelCase")]
    Exited {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signal: Option<i32>,
    },
    Killed,
    /// Deliberately outlives the app (`keep_alive = 1`).
    Detached,
    #[serde(rename_all = "camelCase")]
    Failed { reason: String },
}

impl SessionStatus {
    /// Stable discriminant string, useful for persistence.
    pub fn kind(&self) -> &'static str {
        match self {
            SessionStatus::Starting => "starting",
            SessionStatus::Idle => "idle",
            SessionStatus::Working { .. } => "working",
            SessionStatus::WaitingForInput { .. } => "waitingForInput",
            SessionStatus::Stalled { .. } => "stalled",
            SessionStatus::Exited { .. } => "exited",
            SessionStatus::Killed => "killed",
            SessionStatus::Detached => "detached",
            SessionStatus::Failed { .. } => "failed",
        }
    }

    /// True once no further transition is possible.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            SessionStatus::Exited { .. } | SessionStatus::Killed | SessionStatus::Failed { .. }
        )
    }
}

/// Which bucket of the hot-reloadable signature file a pattern came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PatternClass {
    Working,
    Waiting,
    Idle,
}

/// One observation that fed the status decision. Rendered verbatim by the
/// evidence popover, so every variant has to be explainable in one sentence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum StatusEvidence {
    #[serde(rename_all = "camelCase")]
    OutputBurst { bytes_per_s: u64 },
    #[serde(rename_all = "camelCase")]
    OutputQuiet { ms: u64 },
    #[serde(rename_all = "camelCase")]
    PatternMatch {
        signature_id: String,
        class: PatternClass,
        line: String,
    },
    #[serde(rename_all = "camelCase")]
    ForegroundGroup {
        pgid: i32,
        cpu_pct: f32,
        rss_bytes: u64,
    },
    #[serde(rename_all = "camelCase")]
    AltScreen { on: bool },
    /// The vte grid has held the same cells for `ms` (DESIGN §6-4).
    ///
    /// The observation the byte counter cannot make: a TUI that repaints an
    /// unchanged frame is loud and idle at the same time, and this is the half
    /// of the truth that says so.
    #[serde(rename_all = "camelCase")]
    ScreenStable { ms: u64 },
    Bell,
    /// A screen prompt (or an unnumbered setup menu) was read off the grid
    /// (contract §6.2.4). No label text: the evidence carries shape only.
    #[serde(rename_all = "camelCase")]
    ScreenPrompt { options: u8, setup: bool },
    /// The vendor's ready signature is on a settled grid (§6.2.6).
    ReadySignature,
    /// The grid has been settled this long with no prompt and no menu: the
    /// fallback for a vendor without a ready signature (§6.2.6, N-03).
    #[serde(rename_all = "camelCase")]
    SettledGrid { ms: u64 },
    #[serde(rename_all = "camelCase")]
    ChildExited {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<i32>,
    },
}

/// The full answer to "what is this session doing, and how sure are we".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStatusReport {
    pub status: SessionStatus,
    /// 0.0..1.0. Below 0.5 the UI draws a hollow, dashed pill.
    pub confidence: f32,
    pub evidence: Vec<StatusEvidence>,
    /// Unix millis at which the current status was entered.
    pub since: i64,
}

/// Badges are orthogonal to status: a node can be `working` *and* `dirty`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NodeBadge {
    Dirty,
    Unmerged,
    Orphan,
    Attention,
    OverBudget,
    QuotaLow,
    Stale,
    Unverified,
    Detached,
    Locked,
}

/// Numeric side-channel for a graph node. Every field is optional and patches
/// merge field-wise, so a 1 Hz RSS sample never clobbers a token counter.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeMetrics {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rss_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_pct: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub insertions: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deletions: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dirty_files: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ahead: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behind: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_percent: Option<f32>,
    /// Last 60 output-rate samples; drives the session node sparkline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_rate: Option<Vec<f32>>,
}

macro_rules! merge_fields {
    ($self:ident, $other:ident, $($field:ident),+ $(,)?) => {
        $(
            if $other.$field.is_some() {
                $self.$field = $other.$field.clone();
            }
        )+
    };
}

impl NodeMetrics {
    /// Field-wise overwrite: only the fields present in `other` are taken.
    ///
    /// This is what makes `patchNode { metrics }` safe to send at 1 Hz from the
    /// watchdog without erasing token counters written by the dispatch engine.
    pub fn merge_from(&mut self, other: &NodeMetrics) {
        merge_fields!(
            self,
            other,
            rss_bytes,
            cpu_pct,
            insertions,
            deletions,
            dirty_files,
            ahead,
            behind,
            input_tokens,
            output_tokens,
            cached_tokens,
            cost_usd,
            elapsed_ms,
            last_activity_at,
            quota_percent,
            output_rate,
        );
    }

    pub fn is_empty(&self) -> bool {
        *self == NodeMetrics::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_status_is_internally_tagged_camel_case() {
        let json = serde_json::to_value(SessionStatus::Working {
            since: 42,
            hint: Some("Bash".into()),
        })
        .unwrap();
        assert_eq!(json["kind"], "working");
        assert_eq!(json["since"], 42);
        assert_eq!(json["hint"], "Bash");

        let json = serde_json::to_value(SessionStatus::WaitingForInput {
            prompt: "[y/N]".into(),
        })
        .unwrap();
        assert_eq!(json["kind"], "waitingForInput");

        let json = serde_json::to_value(SessionStatus::Stalled { quiet_ms: 120_000 }).unwrap();
        assert_eq!(json["kind"], "stalled");
        assert_eq!(json["quietMs"], 120_000);

        let json = serde_json::to_value(SessionStatus::Idle).unwrap();
        assert_eq!(json, serde_json::json!({ "kind": "idle" }));
    }

    #[test]
    fn evidence_round_trips() {
        let evidence = vec![
            StatusEvidence::OutputQuiet { ms: 4200 },
            StatusEvidence::ForegroundGroup {
                pgid: 41822,
                cpu_pct: 0.3,
                rss_bytes: 2_100_000_000,
            },
            StatusEvidence::PatternMatch {
                signature_id: "claude.idle.0".into(),
                class: PatternClass::Idle,
                line: "│ > │".into(),
            },
            StatusEvidence::ScreenStable { ms: 2_100 },
            StatusEvidence::Bell,
        ];
        let json = serde_json::to_string(&evidence).unwrap();
        assert!(json.contains("\"foregroundGroup\""));
        assert!(json.contains("\"screenStable\""));
        assert!(json.contains("\"bytesPerS\"") || json.contains("\"cpuPct\""));
        let back: Vec<StatusEvidence> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, evidence);
    }

    #[test]
    fn badges_are_kebab_case() {
        assert_eq!(
            serde_json::to_string(&NodeBadge::OverBudget).unwrap(),
            "\"over-budget\""
        );
        assert_eq!(
            serde_json::to_string(&NodeBadge::QuotaLow).unwrap(),
            "\"quota-low\""
        );
    }

    #[test]
    fn metrics_merge_is_field_wise() {
        let mut base = NodeMetrics {
            rss_bytes: Some(100),
            input_tokens: Some(10),
            ..Default::default()
        };
        base.merge_from(&NodeMetrics {
            rss_bytes: Some(200),
            ..Default::default()
        });
        assert_eq!(base.rss_bytes, Some(200));
        assert_eq!(base.input_tokens, Some(10), "unrelated metric must survive");
    }

    #[test]
    fn empty_metrics_serialize_to_an_empty_object() {
        assert_eq!(
            serde_json::to_string(&NodeMetrics::default()).unwrap(),
            "{}"
        );
    }
}
