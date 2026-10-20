//! ★ SINGLE OWNER: T04. Every `gilde://` event name and payload.
//!
//! DESIGN §4. These all go out through `app.emit(...)` as app-global events.
//! Only PTY bytes use a `Channel`. Every payload is idempotent and carries its
//! identifiers, so a lost event degrades to "briefly stale", never to
//! "permanently wrong".
//!
//! **There are no JS→Rust events.** The only path from the front end into Rust
//! is `invoke()`.
//!
//! Mirrored 1:1 by `src/lib/ipc.ts`. Add new events here first.

pub mod bus;
pub mod coalesce;

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tauri::{AppHandle, Emitter};

use crate::model::graph::{PressureLevel, SidecarState};
use crate::model::status::{SessionStatus, StatusEvidence};

// ---------------------------------------------------------------------------
// Event names
// ---------------------------------------------------------------------------

/// `GraphPatch` — coalesced, at most 10 Hz.
pub const GRAPH_PATCH: &str = "gilde://graph/patch";
/// `GraphSnapshot` — sent when the client detects a revision gap.
pub const GRAPH_RESYNC: &str = "gilde://graph/resync";
/// [`SessionStatusEvent`] — on transition only.
pub const SESSION_STATUS: &str = "gilde://session/status";
/// [`SessionExitEvent`] — exactly once per session.
pub const SESSION_EXIT: &str = "gilde://session/exit";
/// [`PtyResyncEvent`] — emitted when a chunk was dropped under backpressure.
pub const PTY_RESYNC: &str = "gilde://pty/resync";
/// [`DispatchEvent`] — streaming; `text` is coalesced at 100 ms.
pub const DISPATCH_EVENT: &str = "gilde://dispatch/event";
/// [`WorktreeChangedEvent`] — 2 s debounce after an fs event.
pub const WORKTREE_CHANGED: &str = "gilde://worktree/changed";
/// [`ProcScanEvent`] — scanner cadence (3 s active, paused on blur).
pub const PROC_SCAN: &str = "gilde://proc/scan";
/// [`ProcRegistryEvent`] — registry invalidation.
pub const PROC_REGISTRY: &str = "gilde://proc/registry";
/// [`BudgetAlertEvent`] — on threshold crossing.
pub const BUDGET_ALERT: &str = "gilde://budget/alert";
/// [`AgentsDetectedEvent`] — boot and manual rescan.
pub const AGENTS_DETECTED: &str = "gilde://agents/detected";
/// [`SidecarStateEvent`] — on transition.
pub const SIDECAR_STATE: &str = "gilde://sidecar/state";
/// [`RecoveryPendingEvent`] — after the boot sweep.
pub const RECOVERY_PENDING: &str = "gilde://recovery/pending";
/// [`ToastEvent`] — `{key, params}`, never prose.
pub const TOAST: &str = "gilde://toast";
/// [`LogEvent`] — development drawer.
pub const LOG: &str = "gilde://log";
/// [`TeamSnapshotEvent`] — one Claude Code Orchestrator team's whole view (the
/// read-only preview), latest wins; coalesced at 250 ms per team by the sidecar.
pub const TEAM_SNAPSHOT: &str = "gilde://team/snapshot";
/// `HealthSnapshot` (`supervise::health`) — "is everything running well",
/// every watchdog tick (5 s) and after a dismiss.
pub const HEALTH: &str = "gilde://health";

/// Every event name, for tests and for the developer event drawer.
pub const ALL_EVENTS: &[&str] = &[
    GRAPH_PATCH,
    GRAPH_RESYNC,
    SESSION_STATUS,
    SESSION_EXIT,
    PTY_RESYNC,
    DISPATCH_EVENT,
    WORKTREE_CHANGED,
    PROC_SCAN,
    PROC_REGISTRY,
    BUDGET_ALERT,
    AGENTS_DETECTED,
    SIDECAR_STATE,
    RECOVERY_PENDING,
    TOAST,
    LOG,
    TEAM_SNAPSHOT,
    HEALTH,
];

// ---------------------------------------------------------------------------
// Payloads
// ---------------------------------------------------------------------------

/// `gilde://session/status`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStatusEvent {
    pub session_id: String,
    pub status: SessionStatus,
    pub confidence: f32,
    pub evidence: Vec<StatusEvidence>,
    /// Unix millis at which the current status was entered.
    pub since: i64,
    pub at: i64,
}

/// `gilde://session/exit`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionExitEvent {
    pub session_id: String,
    pub pid: i32,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub signal: Option<i32>,
    pub at: i64,
}

/// `gilde://pty/resync` — the ring kept reading, the channel dropped frames.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PtyResyncEvent {
    pub session_id: String,
    pub next_seq: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DispatchEventKind {
    Routed,
    Spawned,
    Text,
    Tool,
    Usage,
    Done,
    Error,
    Interrupted,
    Cancelled,
}

/// `gilde://dispatch/event` — the headless lane's streaming surface.
///
/// `payload` is the adapter's own `RunEvent` shape passed through verbatim; the
/// host does not reinterpret vendor data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DispatchEvent {
    pub task_id: String,
    pub run_id: String,
    pub vendor: String,
    pub kind: DispatchEventKind,
    #[serde(default)]
    pub payload: Value,
}

/// `gilde://worktree/changed`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeChangedEvent {
    pub worktree_id: String,
    pub dirty: u32,
    pub added: u64,
    pub removed: u64,
    pub ahead: u32,
    pub behind: u32,
    pub at: i64,
}

/// `gilde://proc/scan`
///
/// Generic over the row and totals types so that this file — the single owner
/// of the event catalog — does not depend on `supervise::procscan` (T07).
/// Producers instantiate it as `ProcScanEvent<ProcRow, ProcTotals>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcScanEvent<R, T> {
    pub at: i64,
    pub rows: Vec<R>,
    pub totals: T,
}

/// `gilde://proc/registry`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcRegistryEvent {
    pub changed_spawn_ids: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BudgetLevel {
    Soft,
    Hard,
    System,
}

/// `gilde://budget/alert` — warnings only. Pressure never triggers an auto-kill.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetAlertEvent {
    pub level: BudgetLevel,
    #[serde(default)]
    pub session_id: Option<String>,
    pub rss_bytes: u64,
    pub budget_percent: f32,
    pub pressure_level: PressureLevel,
    /// Present when the alert opened a 10 s undo window.
    #[serde(default)]
    pub undo_token: Option<String>,
}

/// `gilde://agents/detected`
///
/// Generic over the binary description for the same reason as
/// [`ProcScanEvent`]; T09 instantiates it as `AgentsDetectedEvent<AgentBinary>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentsDetectedEvent<A> {
    pub agents: Vec<A>,
}

/// `gilde://sidecar/state`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SidecarStateEvent {
    pub state: SidecarState,
    pub generation: u64,
    pub restarts: u32,
    #[serde(default)]
    pub last_error: Option<String>,
}

/// `gilde://recovery/pending`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryPendingEvent {
    pub count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ToastLevel {
    Info,
    Success,
    Warn,
    Error,
}

/// `gilde://toast`
///
/// Rust never returns prose. `key` indexes `src/i18n/ko.ts`, `params` fills the
/// `{placeholders}`. Changing a message does not require a Rust rebuild.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToastEvent {
    pub level: ToastLevel,
    pub key: String,
    #[serde(default)]
    pub params: Map<String, Value>,
    #[serde(default)]
    pub action_id: Option<String>,
    #[serde(default)]
    pub undo_token: Option<String>,
}

impl ToastEvent {
    pub fn new(level: ToastLevel, key: impl Into<String>) -> Self {
        Self {
            level,
            key: key.into(),
            params: Map::new(),
            action_id: None,
            undo_token: None,
        }
    }

    pub fn param(mut self, name: impl Into<String>, value: impl Into<Value>) -> Self {
        self.params.insert(name.into(), value.into());
        self
    }
}

/// `gilde://log`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogEvent {
    pub level: String,
    pub target: String,
    pub msg: String,
    pub at: i64,
}

/// `gilde://team/snapshot` — a team view composed by the sidecar's read-only
/// observer (Claude Code Orchestrator team contract v2 §8.2). Idempotent: a
/// receiver keeps the view with the greatest `(generation, rev)` and drops the
/// rest; `view: None` means the team left sight.
///
/// Generic over the view for the same reason as [`ProcScanEvent`]: this file
/// does not depend on the sidecar's types. The producer instantiates it as
/// `TeamSnapshotEvent<CcoTeamView>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamSnapshotEvent<V> {
    pub generation: i64,
    pub rev: u64,
    pub team_id: String,
    pub view: Option<V>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Unix milliseconds. One definition, so every `at` field means the same thing.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Emit an app-global event, logging (never propagating) a transport failure.
///
/// A failed emit is a lost UI update, not a lost fact: the authoritative state
/// lives in `GraphState` and in SQLite, and the client can always resync.
pub fn emit<P>(app: &AppHandle, name: &str, payload: P)
where
    P: Serialize + Clone,
{
    if let Err(err) = app.emit(name, payload) {
        tracing::warn!(target: "events", event = name, error = %err, "failed to emit event");
    }
}

/// Convenience wrapper for `gilde://toast`.
pub fn emit_toast(app: &AppHandle, toast: ToastEvent) {
    emit(app, TOAST, toast);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_event_name_is_namespaced_and_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for name in ALL_EVENTS {
            assert!(name.starts_with("gilde://"), "{name} must be gilde:// namespaced");
            assert!(seen.insert(*name), "duplicate event name {name}");
        }
        assert_eq!(seen.len(), 17);
    }

    #[test]
    fn payloads_are_camel_case() {
        let json = serde_json::to_value(SessionExitEvent {
            session_id: "s".into(),
            pid: 42,
            exit_code: Some(0),
            signal: None,
            at: 1,
        })
        .unwrap();
        assert_eq!(json["sessionId"], "s");
        assert_eq!(json["exitCode"], 0);
        assert!(json.get("signal").is_some(), "explicit null is kept");

        let json = serde_json::to_value(PtyResyncEvent {
            session_id: "s".into(),
            next_seq: 9,
        })
        .unwrap();
        assert_eq!(json["nextSeq"], 9);

        let json = serde_json::to_value(ProcRegistryEvent {
            changed_spawn_ids: vec!["a".into()],
        })
        .unwrap();
        assert_eq!(json["changedSpawnIds"][0], "a");
    }

    #[test]
    fn dispatch_event_kinds_are_lower_camel() {
        let json = serde_json::to_value(DispatchEvent {
            task_id: "t".into(),
            run_id: "r".into(),
            vendor: "claude".into(),
            kind: DispatchEventKind::Interrupted,
            payload: Value::Null,
        })
        .unwrap();
        assert_eq!(json["kind"], "interrupted");
    }

    #[test]
    fn toast_carries_keys_not_prose() {
        let toast = ToastEvent::new(ToastLevel::Warn, "worktree.remove.dirty").param("n", 3);
        let json = serde_json::to_value(&toast).unwrap();
        assert_eq!(json["key"], "worktree.remove.dirty");
        assert_eq!(json["params"]["n"], 3);
        assert_eq!(json["level"], "warn");
    }

    #[test]
    fn generic_events_instantiate() {
        #[derive(Clone, Serialize)]
        struct Row {
            pid: i32,
        }
        let json = serde_json::to_value(ProcScanEvent {
            at: 1,
            rows: vec![Row { pid: 7 }],
            totals: 1u32,
        })
        .unwrap();
        assert_eq!(json["rows"][0]["pid"], 7);
    }

    #[test]
    fn now_ms_is_plausible() {
        // 2020-01-01T00:00:00Z
        assert!(now_ms() > 1_577_836_800_000);
    }
}
