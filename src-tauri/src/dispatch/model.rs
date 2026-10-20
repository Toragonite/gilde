//! Wire shapes and pure rules of the headless dispatch lane (DESIGN §3, §9-7,
//! §10-2; M1-PLAN T22).
//!
//! Everything in this file is data plus the decisions that can be made without
//! a database, a sidecar or a clock — which is exactly what makes them
//! testable. The engine next door owns every effect.
//!
//! Three shapes are load-bearing enough to state here:
//!
//!   * **[`RunPhase`] is a superset of agent-mesh's `RunStatus`.** The adapter
//!     contract knows `ok | error | stopped | incomplete`; a *run* also has the
//!     three states that belong to the host rather than to the agent —
//!     `queued`, `spawning`, `running` — plus the two the user must be able to
//!     tell apart from a failure: `cancelled` (they asked) and `interrupted`
//!     (**the app** broke, not the agent; DESIGN §5-10 rule 2).
//!   * **The artifact directory is the record** (DESIGN §10-2). [`RunArtifacts`]
//!     is the only place that spells those three filenames, so a row in
//!     `dispatch_run` can be lost without the run becoming irreproducible.
//!   * **A spilled result keeps a pointer, not a path.** Text over
//!     [`RESULT_SPILL_BYTES`] moves to `<app-data>/runs/<run_id>.txt` and the
//!     column holds [`SPILL_PREFIX`]`<run_id>.txt` — relative, so moving the
//!     app-data directory does not strand every historical result.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::sidecar::{RunResult, RunStatus, TokenUsage};
use crate::store::models::{DispatchRunRow, DispatchTaskRow};

/// The vendor token that means "ask `fleet.route`" (DESIGN §9-7).
///
/// This string is the **only** door to routing: `Auto` is something a user
/// picks, and every other path runs the vendor it was handed.
pub const AUTO_VENDOR: &str = "auto";

/// Results larger than this move to `<app-data>/runs/<run_id>.txt` (DESIGN §10-2).
pub const RESULT_SPILL_BYTES: usize = 4 * 1024 * 1024;

/// How much result text a command may hand back. A dispatch card renders the
/// tail of a transcript, not four megabytes of it; DESIGN §3 forbids commands
/// that return large state, and the artifact file is one `app_reveal` away.
pub const RUN_TEXT_WIRE_CAP: usize = 256 * 1024;

/// Merge window for `text` events on `gilde://dispatch/event` (DESIGN §4).
pub const TEXT_COALESCE_MS: u64 = 100;

/// Marks a `dispatch_run.text` column that holds a pointer rather than text.
pub const SPILL_PREFIX: &str = "@runs/";

// ---------------------------------------------------------------------------
// request
// ---------------------------------------------------------------------------

/// One agent a prompt is fanned out to (`ipc.ts::DispatchTarget`).
///
/// `vendor` is a plain `String` rather than an enum on purpose: the adapter
/// roster is whatever the sidecar built (DESIGN §5-3), and a vendor this build
/// has never heard of must fail as "no adapter for X" rather than as a
/// deserialization error the user cannot act on. [`AUTO_VENDOR`] is the one
/// value the host interprets itself.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DispatchTarget {
    pub vendor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// A routing note the composer already showed the user, persisted verbatim
    /// so the card explains itself after a restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_note: Option<String>,
}

impl DispatchTarget {
    pub fn new(vendor: impl Into<String>) -> DispatchTarget {
        DispatchTarget {
            vendor: vendor.into(),
            ..DispatchTarget::default()
        }
    }

    /// Whether this target asks the host to route it (DESIGN §9-7).
    pub fn is_auto(&self) -> bool {
        self.vendor.eq_ignore_ascii_case(AUTO_VENDOR)
    }
}

/// Where the runs of one task do their work (`ipc.ts::WorktreeStrategy`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "strategy", rename_all = "camelCase")]
pub enum WorktreeStrategy {
    /// One worktree per target — `<task-slug>-<vendor>`, branch `gilde/<same>`.
    PerTarget,
    /// One worktree named `<task-slug>`, shared by every target.
    Shared,
    /// An existing worktree, used by every target.
    #[serde(rename_all = "camelCase")]
    Existing { worktree_id: String },
}

impl Default for WorktreeStrategy {
    fn default() -> Self {
        WorktreeStrategy::PerTarget
    }
}

impl WorktreeStrategy {
    /// The `dispatch_task.worktree_strategy` column value.
    pub fn to_column(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| r#"{"strategy":"perTarget"}"#.to_string())
    }

    /// Read the column back. An unreadable value degrades to `perTarget`: the
    /// strategy is spent by the time a row is read, and refusing to list a task
    /// because its strategy no longer parses would hide the task's results.
    pub fn from_column(raw: &str) -> WorktreeStrategy {
        serde_json::from_str(raw).unwrap_or(WorktreeStrategy::PerTarget)
    }
}

/// `dispatch_create` parameters (`ipc.ts::DispatchSpec`).
///
/// `timeout_ms` and `permission_mode` are optional here and filled from
/// `config.dispatch` when absent — the sidecar rejects a task with no wall
/// clock (DESIGN §5-4), so the value is *always* set by the time it leaves.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DispatchSpec {
    pub prompt: String,
    pub project_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_ref: Option<String>,
    pub task_slug: String,
    pub targets: Vec<DispatchTarget>,
    #[serde(default)]
    pub worktree_strategy: WorktreeStrategy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    /// Optional card title; defaults to the task slug.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// Which tasks `dispatch_list` returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DispatchScope {
    /// Unfinished tasks only.
    Active,
    /// Newest first, finished or not.
    #[default]
    Recent,
    /// Same as `Recent` in M1; kept distinct because retention will diverge.
    All,
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

/// Aggregate state of a fan-out (`ipc.ts::DispatchTaskStatus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TaskStatus {
    Queued,
    Running,
    /// Every run finished and every one of them succeeded.
    Done,
    /// Some runs succeeded and some did not — the honest answer for a fan-out,
    /// and the reason this is not just `done | error`.
    Partial,
    Cancelled,
    /// Every run failed.
    Error,
}

impl TaskStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Queued => "queued",
            TaskStatus::Running => "running",
            TaskStatus::Done => "done",
            TaskStatus::Partial => "partial",
            TaskStatus::Cancelled => "cancelled",
            TaskStatus::Error => "error",
        }
    }

    pub fn from_str(raw: &str) -> TaskStatus {
        match raw {
            "queued" => TaskStatus::Queued,
            "running" => TaskStatus::Running,
            "done" => TaskStatus::Done,
            "partial" => TaskStatus::Partial,
            "cancelled" => TaskStatus::Cancelled,
            _ => TaskStatus::Error,
        }
    }

    pub fn is_terminal(self) -> bool {
        !matches!(self, TaskStatus::Queued | TaskStatus::Running)
    }
}

/// State of one run (`ipc.ts::RunStatus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RunPhase {
    Queued,
    Spawning,
    Running,
    Done,
    Error,
    Stopped,
    Incomplete,
    /// The app failed, not the agent: the sidecar generation that owned this
    /// run died under it (DESIGN §5-10). Never rendered as an error.
    Interrupted,
    Cancelled,
}

impl RunPhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            RunPhase::Queued => "queued",
            RunPhase::Spawning => "spawning",
            RunPhase::Running => "running",
            RunPhase::Done => "done",
            RunPhase::Error => "error",
            RunPhase::Stopped => "stopped",
            RunPhase::Incomplete => "incomplete",
            RunPhase::Interrupted => "interrupted",
            RunPhase::Cancelled => "cancelled",
        }
    }

    pub fn from_str(raw: &str) -> RunPhase {
        match raw {
            "queued" => RunPhase::Queued,
            "spawning" => RunPhase::Spawning,
            "running" => RunPhase::Running,
            "done" => RunPhase::Done,
            "stopped" => RunPhase::Stopped,
            "incomplete" => RunPhase::Incomplete,
            "interrupted" => RunPhase::Interrupted,
            "cancelled" => RunPhase::Cancelled,
            _ => RunPhase::Error,
        }
    }

    /// A phase nothing will move out of on its own.
    pub fn is_terminal(self) -> bool {
        !matches!(
            self,
            RunPhase::Queued | RunPhase::Spawning | RunPhase::Running
        )
    }

    /// Whether the run produced the answer it was asked for. `incomplete` did
    /// not (timeout, permission wall) and neither did anything below it.
    pub fn is_success(self) -> bool {
        matches!(self, RunPhase::Done)
    }

    /// The phase an adapter's terminal [`RunStatus`] maps to.
    pub fn from_run_status(status: RunStatus) -> RunPhase {
        match status {
            RunStatus::Ok => RunPhase::Done,
            RunStatus::Error => RunPhase::Error,
            RunStatus::Stopped => RunPhase::Stopped,
            RunStatus::Incomplete => RunPhase::Incomplete,
        }
    }
}

/// Fold the runs of a task into the task's own status.
///
/// The rule the fan-out needs and a single run does not: a task with a mix of
/// outcomes is `partial`, never "done" and never "error". Anything unfinished
/// keeps the task `running` (or `queued`, before the first run starts).
pub fn fold_task_status(runs: &[RunPhase]) -> TaskStatus {
    if runs.is_empty() {
        return TaskStatus::Queued;
    }
    if runs.iter().any(|p| !p.is_terminal()) {
        return if runs.iter().all(|p| *p == RunPhase::Queued) {
            TaskStatus::Queued
        } else {
            TaskStatus::Running
        };
    }
    if runs.iter().all(|p| *p == RunPhase::Cancelled) {
        return TaskStatus::Cancelled;
    }
    let ok = runs.iter().filter(|p| p.is_success()).count();
    match ok {
        0 => TaskStatus::Error,
        n if n == runs.len() => TaskStatus::Done,
        _ => TaskStatus::Partial,
    }
}

// ---------------------------------------------------------------------------
// responses
// ---------------------------------------------------------------------------

/// `ipc.ts::DispatchTask`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DispatchTask {
    pub id: String,
    pub project_id: String,
    pub title: String,
    pub prompt: String,
    pub task_slug: String,
    pub base_ref: Option<String>,
    pub worktree_strategy: WorktreeStrategy,
    pub permission_mode: String,
    pub timeout_ms: i64,
    pub created_at: i64,
    pub finished_at: Option<i64>,
    pub status: TaskStatus,
}

impl From<DispatchTaskRow> for DispatchTask {
    fn from(row: DispatchTaskRow) -> Self {
        DispatchTask {
            id: row.id,
            project_id: row.project_id.unwrap_or_default(),
            title: row.title,
            prompt: row.prompt,
            task_slug: row.task_slug,
            base_ref: row.base_ref,
            worktree_strategy: WorktreeStrategy::from_column(&row.worktree_strategy),
            permission_mode: row.permission_mode.unwrap_or_default(),
            timeout_ms: row.timeout_ms.unwrap_or_default(),
            created_at: row.created_at,
            finished_at: row.finished_at,
            status: TaskStatus::from_str(&row.status),
        }
    }
}

/// `ipc.ts::DispatchRun`.
///
/// `text` is capped at [`RUN_TEXT_WIRE_CAP`] and `text_path` names the artifact
/// whenever the full result lives on disk — the two together let a card show
/// the tail and still offer "reveal the file".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DispatchRun {
    pub id: String,
    pub task_id: String,
    pub vendor: String,
    pub model_requested: Option<String>,
    pub model_actual: Option<String>,
    pub account_id: Option<String>,
    pub worktree_id: Option<String>,
    pub spawn_id: Option<String>,
    pub conversation_id: Option<String>,
    pub status: RunPhase,
    pub note: Option<String>,
    pub route_note: Option<String>,
    pub text: String,
    /// Absolute path of the spilled result, when there is one.
    pub text_path: Option<String>,
    pub usage: Option<Value>,
    pub cost_usd: Option<f64>,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub error: Option<String>,
}

impl DispatchRun {
    /// Project a row for the wire. `runs_dir` resolves a [`SPILL_PREFIX`]
    /// pointer; the file is read lazily and capped, and an unreadable spill
    /// degrades to empty text plus the path rather than to an error — the run
    /// happened either way and the card must still render.
    pub fn from_row(row: DispatchRunRow, runs_dir: &Path) -> DispatchRun {
        let (text, text_path) = match row.text.as_deref() {
            Some(raw) if raw.starts_with(SPILL_PREFIX) => {
                let path = runs_dir.join(raw.trim_start_matches(SPILL_PREFIX));
                let text = std::fs::read_to_string(&path)
                    .map(|s| cap_text(&s))
                    .unwrap_or_default();
                (text, Some(path.display().to_string()))
            }
            Some(raw) => (cap_text(raw), None),
            None => (String::new(), None),
        };
        DispatchRun {
            id: row.id,
            task_id: row.task_id,
            vendor: row.vendor,
            model_requested: row.model_requested,
            model_actual: row.model_actual,
            account_id: row.account_id,
            worktree_id: row.worktree_id,
            spawn_id: row.spawn_id,
            conversation_id: row.conversation_id,
            status: RunPhase::from_str(&row.status),
            note: row.note,
            route_note: row.route_note,
            text,
            text_path,
            usage: row
                .usage_json
                .as_deref()
                .and_then(|raw| serde_json::from_str(raw).ok()),
            cost_usd: row.cost_usd,
            started_at: row.started_at,
            finished_at: row.finished_at,
            error: row.error,
        }
    }
}

/// The tail of `text`, at most [`RUN_TEXT_WIRE_CAP`] bytes, cut on a character
/// boundary. The tail rather than the head: the end of a transcript is the
/// answer, the start is the preamble.
pub fn cap_text(text: &str) -> String {
    if text.len() <= RUN_TEXT_WIRE_CAP {
        return text.to_string();
    }
    let mut start = text.len() - RUN_TEXT_WIRE_CAP;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_string()
}

/// Store a result: inline, or spilled to `<app-data>/runs/<run_id>.txt` with a
/// relative pointer in the column (DESIGN §10-2, T22 acceptance 9).
///
/// The pointer is relative to `runs_dir` so that moving the app-data directory
/// does not strand every historical result. A spill that cannot be written
/// degrades to the capped tail inline: losing the head of one transcript is
/// recoverable, losing the row is not.
pub fn store_result_text(runs_dir: &Path, run_id: &str, text: &str) -> String {
    if text.len() <= RESULT_SPILL_BYTES {
        return text.to_string();
    }
    let name = format!("{run_id}.txt");
    let path = runs_dir.join(&name);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::write(&path, text) {
        Ok(()) => format!("{SPILL_PREFIX}{name}"),
        Err(err) => {
            tracing::warn!(target: "dispatch", error = %err, path = %path.display(),
                "could not spill the result; storing the tail inline");
            cap_text(text)
        }
    }
}

/// `ipc.ts::DispatchTaskDetail`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DispatchTaskDetail {
    pub task: DispatchTask,
    pub runs: Vec<DispatchRun>,
}

/// `ipc.ts::RunOutcome` — the small ack `dispatch_resume` answers with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunOutcome {
    pub run_id: String,
    pub status: RunPhase,
    pub conversation_id: Option<String>,
    pub note: Option<String>,
}

/// What `fleet.route` decided (`ipc.ts::RouteDecision`).
///
/// `note` is set when every candidate was gated and the fleet fell back
/// best-effort; dropping it would turn a guess into a confident answer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteDecision {
    pub vendor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

// ---------------------------------------------------------------------------
// artifacts (DESIGN §10-2)
// ---------------------------------------------------------------------------

/// `~/.gilde/dispatch/<task_id>/<run_id>/`.
///
/// The one place those three filenames are spelled. Nothing here fails a run:
/// an artifact we cannot write is logged by the caller and the run continues —
/// the DB row and the live stream are still there — but the *design* is that
/// this directory outlives both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunArtifacts {
    dir: PathBuf,
}

impl RunArtifacts {
    pub fn new(dispatch_root: &Path, task_id: &str, run_id: &str) -> RunArtifacts {
        RunArtifacts {
            dir: dispatch_root.join(task_id).join(run_id),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The prompt exactly as it was sent — the source of a reproduction.
    pub fn prompt_md(&self) -> PathBuf {
        self.dir.join("prompt.md")
    }

    /// The verbatim `RunResult`.
    pub fn result_json(&self) -> PathBuf {
        self.dir.join("result.json")
    }

    /// Every `RunEvent`, one per line.
    pub fn stream_jsonl(&self) -> PathBuf {
        self.dir.join("stream.jsonl")
    }

    pub fn ensure(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)
    }
}

// ---------------------------------------------------------------------------
// pure rules
// ---------------------------------------------------------------------------

/// The worktree task name for one target (M1-PLAN T22 acceptance 1).
///
/// `PerTarget` gets `<task-slug>-<vendor>`; `Shared` gets the slug alone. The
/// six-character uniqueness suffix that `worktree::naming::task_slug` appends
/// is T13's contract and is *not* reproduced here — two dispatches of the same
/// slug must not collide on one directory.
pub fn worktree_task_name(task_slug: &str, vendor: &str, per_target: bool) -> String {
    if per_target {
        format!("{task_slug}-{vendor}")
    } else {
        task_slug.to_string()
    }
}

/// The cost of a run, in USD, when the vendor reported one.
///
/// agent-mesh's `TokenUsage` has no cost field — every vendor that reports one
/// does it through the `extra` passthrough or through `RunResult.raw`, under a
/// name of its own (`costUsd`, `cost_usd`, `total_cost_usd`). Reading all of
/// them here keeps the vendor knowledge in exactly one function.
pub fn cost_usd(result: &RunResult) -> Option<f64> {
    const KEYS: [&str; 3] = ["costUsd", "cost_usd", "total_cost_usd"];

    let from_map = |map: &Map<String, Value>| -> Option<f64> {
        KEYS.iter()
            .find_map(|k| map.get(*k))
            .and_then(Value::as_f64)
    };

    if let Some(usage) = result.usage.as_ref() {
        if let Some(cost) = KEYS
            .iter()
            .find_map(|k| usage.extra.get(*k))
            .and_then(Value::as_f64)
        {
            return Some(cost);
        }
    }
    if let Some(cost) = KEYS
        .iter()
        .find_map(|k| result.extra.get(*k))
        .and_then(Value::as_f64)
    {
        return Some(cost);
    }
    result
        .raw
        .as_ref()
        .and_then(Value::as_object)
        .and_then(from_map)
}

/// The `usage_json` column: the token counts plus the cost, so a card can be
/// rendered from one column.
pub fn usage_json(usage: Option<&TokenUsage>, cost: Option<f64>) -> Option<String> {
    let usage = usage?;
    let mut value = serde_json::to_value(usage).ok()?;
    if let (Some(map), Some(cost)) = (value.as_object_mut(), cost) {
        map.entry("costUsd".to_string())
            .or_insert_with(|| Value::from(cost));
    }
    serde_json::to_string(&value).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mixed_fan_out_is_partial_never_done_and_never_error() {
        assert_eq!(
            fold_task_status(&[RunPhase::Done, RunPhase::Error]),
            TaskStatus::Partial
        );
        assert_eq!(
            fold_task_status(&[RunPhase::Done, RunPhase::Done]),
            TaskStatus::Done
        );
        assert_eq!(
            fold_task_status(&[RunPhase::Error, RunPhase::Interrupted]),
            TaskStatus::Error
        );
        assert_eq!(
            fold_task_status(&[RunPhase::Cancelled, RunPhase::Cancelled]),
            TaskStatus::Cancelled
        );
        // One still running keeps the whole task running.
        assert_eq!(
            fold_task_status(&[RunPhase::Done, RunPhase::Running]),
            TaskStatus::Running
        );
        assert_eq!(
            fold_task_status(&[RunPhase::Queued, RunPhase::Queued]),
            TaskStatus::Queued
        );
        assert_eq!(fold_task_status(&[]), TaskStatus::Queued);
    }

    #[test]
    fn incomplete_is_not_a_success_so_it_cannot_read_as_done() {
        assert!(RunPhase::Done.is_success());
        assert!(!RunPhase::Incomplete.is_success());
        assert!(RunPhase::Incomplete.is_terminal());
        assert!(!RunPhase::Running.is_terminal());
        assert_eq!(
            fold_task_status(&[RunPhase::Done, RunPhase::Incomplete]),
            TaskStatus::Partial
        );
    }

    #[test]
    fn every_phase_survives_the_database_column_round_trip() {
        for phase in [
            RunPhase::Queued,
            RunPhase::Spawning,
            RunPhase::Running,
            RunPhase::Done,
            RunPhase::Error,
            RunPhase::Stopped,
            RunPhase::Incomplete,
            RunPhase::Interrupted,
            RunPhase::Cancelled,
        ] {
            assert_eq!(RunPhase::from_str(phase.as_str()), phase);
        }
        for status in [
            TaskStatus::Queued,
            TaskStatus::Running,
            TaskStatus::Done,
            TaskStatus::Partial,
            TaskStatus::Cancelled,
            TaskStatus::Error,
        ] {
            assert_eq!(TaskStatus::from_str(status.as_str()), status);
        }
    }

    #[test]
    fn interrupted_never_decodes_as_error() {
        // The distinction DESIGN §5-10 rule 2 insists the user must see.
        assert_eq!(RunPhase::from_str("interrupted"), RunPhase::Interrupted);
        assert_ne!(RunPhase::from_str("interrupted"), RunPhase::Error);
        // An unknown column value is the only thing that degrades to `error`.
        assert_eq!(RunPhase::from_str("nonsense"), RunPhase::Error);
    }

    #[test]
    fn the_strategy_column_round_trips_and_degrades_instead_of_failing() {
        for strategy in [
            WorktreeStrategy::PerTarget,
            WorktreeStrategy::Shared,
            WorktreeStrategy::Existing {
                worktree_id: "w1".into(),
            },
        ] {
            assert_eq!(
                WorktreeStrategy::from_column(&strategy.to_column()),
                strategy
            );
        }
        assert_eq!(
            WorktreeStrategy::from_column("not json"),
            WorktreeStrategy::PerTarget
        );
        // The wire form ipc.ts declares.
        assert_eq!(
            serde_json::to_string(&WorktreeStrategy::Existing {
                worktree_id: "w1".into()
            })
            .expect("encode"),
            r#"{"strategy":"existing","worktreeId":"w1"}"#
        );
    }

    #[test]
    fn per_target_names_the_worktree_after_the_vendor() {
        assert_eq!(
            worktree_task_name("fix-pty-resize", "claude", true),
            "fix-pty-resize-claude"
        );
        assert_eq!(
            worktree_task_name("fix-pty-resize", "codex", false),
            "fix-pty-resize"
        );
    }

    #[test]
    fn the_artifact_layout_is_the_one_design_specifies() {
        let artifacts = RunArtifacts::new(Path::new("/root/dispatch"), "t1", "r1");
        assert_eq!(
            artifacts.prompt_md(),
            PathBuf::from("/root/dispatch/t1/r1/prompt.md")
        );
        assert_eq!(
            artifacts.result_json(),
            PathBuf::from("/root/dispatch/t1/r1/result.json")
        );
        assert_eq!(
            artifacts.stream_jsonl(),
            PathBuf::from("/root/dispatch/t1/r1/stream.jsonl")
        );
    }

    #[test]
    fn cost_is_read_from_usage_extra_from_the_result_and_from_raw() {
        let base = |value: Value| -> RunResult {
            serde_json::from_value(value).expect("decode")
        };

        let from_usage = base(serde_json::json!({
            "vendor": "claude", "conversationId": "c", "text": "t", "status": "ok",
            "usage": {"inputTokens": 10, "costUsd": 0.25}
        }));
        assert_eq!(cost_usd(&from_usage), Some(0.25));

        let from_result = base(serde_json::json!({
            "vendor": "claude", "conversationId": "c", "text": "t", "status": "ok",
            "costUsd": 1.5
        }));
        assert_eq!(cost_usd(&from_result), Some(1.5));

        let from_raw = base(serde_json::json!({
            "vendor": "claude", "conversationId": "c", "text": "t", "status": "ok",
            "raw": {"total_cost_usd": 0.0125}
        }));
        assert_eq!(cost_usd(&from_raw), Some(0.0125));

        let none = base(serde_json::json!({
            "vendor": "codex", "conversationId": "", "text": "", "status": "error"
        }));
        assert_eq!(cost_usd(&none), None);
    }

    #[test]
    fn the_usage_column_carries_the_cost_beside_the_tokens() {
        let usage: TokenUsage = serde_json::from_value(serde_json::json!({
            "inputTokens": 10, "outputTokens": 20
        }))
        .expect("decode");
        let json = usage_json(Some(&usage), Some(0.5)).expect("some");
        let value: Value = serde_json::from_str(&json).expect("json");
        assert_eq!(value["inputTokens"], 10);
        assert_eq!(value["costUsd"], 0.5);
        assert_eq!(usage_json(None, Some(0.5)), None);
    }

    #[test]
    fn a_capped_result_keeps_its_tail_on_a_character_boundary() {
        let short = "hello";
        assert_eq!(cap_text(short), short);

        // Multi-byte characters right at the cut.
        let long = "가".repeat(RUN_TEXT_WIRE_CAP);
        let capped = cap_text(&long);
        assert!(capped.len() <= RUN_TEXT_WIRE_CAP);
        assert!(long.ends_with(&capped), "the tail is what survives");
    }

    #[test]
    fn a_result_over_four_mebibytes_spills_and_the_column_points_at_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let runs = dir.path();

        // Under the threshold: the text is the column.
        let small = "small result";
        assert_eq!(store_result_text(runs, "r1", small), small);
        assert!(!runs.join("r1.txt").exists());

        // Over it: the file holds the result verbatim and the column holds a
        // relative pointer.
        let big = "x".repeat(RESULT_SPILL_BYTES + 1);
        let column = store_result_text(runs, "r2", &big);
        assert_eq!(column, "@runs/r2.txt");
        assert_eq!(
            std::fs::read_to_string(runs.join("r2.txt")).expect("spill file"),
            big,
            "the spilled file is the verbatim result, not a truncation"
        );

        // And a row carrying that pointer reads back as capped text plus the path.
        let row = DispatchRunRow {
            id: "r2".into(),
            task_id: "t1".into(),
            vendor: "claude".into(),
            model_requested: None,
            model_actual: None,
            account_id: None,
            worktree_id: None,
            spawn_id: None,
            run_id: Some("r2".into()),
            conversation_id: None,
            status: "done".into(),
            note: None,
            route_note: None,
            text: Some(column),
            usage_json: None,
            cost_usd: None,
            started_at: None,
            finished_at: None,
            error: None,
        };
        let run = DispatchRun::from_row(row, runs);
        assert_eq!(run.text.len(), RUN_TEXT_WIRE_CAP);
        assert_eq!(
            run.text_path.as_deref(),
            Some(runs.join("r2.txt").display().to_string().as_str())
        );
    }

    #[test]
    fn auto_is_the_only_vendor_token_the_host_interprets() {
        assert!(DispatchTarget::new("auto").is_auto());
        assert!(DispatchTarget::new("AUTO").is_auto());
        assert!(!DispatchTarget::new("claude").is_auto());
    }
}
