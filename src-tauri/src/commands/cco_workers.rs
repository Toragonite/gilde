//! `cco_workers` — the worker accounts the Claude Code Orchestrator ENGINE knows,
//! read-only (Gilde plan #3c).
//!
//! The sidecar (`sidecar/src/methods/cco-workers.ts`, `cco.workers`) asks a
//! dispatch host's `list_workers` on an observer connection and composes a
//! `CcoWorkersResult`: the engine's `WorkersSnapshot` (`cco.workers/1`), checked
//! by value, plus its poll-floor bookkeeping. Rust does not interpret any of it —
//! Gilde DISPLAYS what the engine knows — and passes the object through with two
//! stamps, exactly like `cco_team_list`:
//!
//!   * `available: true` — the sidecar answered;
//!   * `generation` — the sidecar generation that composed the answer.
//!
//! The scope (`roots`, `panePids`, `allWorkspaces`) is the same one `cco_team_list` states, so a
//! Gilde that never opens either view never connects to a host.
//!
//! A sidecar that is down, or that died under the call, is not an error: the
//! answer says `available: false`, holds no snapshot (never an empty stand-in)
//! and asks for the next poll now, so the panel retries as soon as the sidecar is
//! back. Every other failure (a sidecar-side error, a timeout, a malformed
//! answer) is an error, as for `cco_team_list`.

use std::time::Duration;

use serde_json::{json, Map, Value};
use tauri::State;

use crate::commands::cco_team::scope;
use crate::commands::mesh::SidecarState;
use crate::commands::pty::PtyState;
use crate::error::{CmdResult, GildeError, GildeErrorPayload};
use crate::sidecar::{method, SidecarCallError};
use crate::store::Db;

/// The sidecar may wait `WORKERS_REQUEST_TIMEOUT_MS` (30 s, the engine's usage
/// probes) plus `WORKERS_SETTLE_MS` (3 s, a host still being connected to)
/// before it answers; this has to outlast both.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(40);

/// The engine's poll floor (`WORKERS_SNAPSHOT_MIN_POLL_MS`), stated in the
/// down answer so the panel schedules by the same number as when it is up.
pub const MIN_POLL_MS: i64 = 60_000;

/// The engine's worker accounts, as the sidecar holds them.
///
/// `refresh: true` is the user's explicit refresh; the sidecar rate-limits it
/// (and every automatic poll is floored at 60 s there, for all consumers).
#[tauri::command]
pub async fn cco_workers(
    refresh: bool,
    sidecar: State<'_, SidecarState>,
    db: State<'_, Db>,
    pty: State<'_, PtyState>,
) -> CmdResult<Value> {
    let scope = scope(db.inner(), pty.inner()).await;
    let params = json!({
        "refresh": refresh,
        "roots": scope.roots,
        "panePids": scope.pane_pids,
        "allWorkspaces": scope.all_workspaces,
    });
    let outcome = sidecar
        .call_detailed::<_, Value>(method::WORKERS, &params, Some(CALL_TIMEOUT))
        .await;
    workers_answer(outcome, sidecar.generation(), now_ms())
}

/// Turn the sidecar's outcome into the command's answer (pure, so the branches
/// are testable without a sidecar).
pub fn workers_answer(
    outcome: std::result::Result<Value, SidecarCallError>,
    generation: i64,
    now_ms: i64,
) -> CmdResult<Value> {
    match outcome {
        Ok(Value::Object(mut answer)) => {
            answer.insert("available".to_string(), Value::Bool(true));
            answer.insert("generation".to_string(), Value::from(generation));
            Ok(Value::Object(answer))
        }
        Ok(other) => Err(GildeErrorPayload::from(GildeError::from(SidecarCallError::Decode {
            method: method::WORKERS.to_string(),
            detail: format!("expected an object, got {}", json_kind(&other)),
        }))),
        Err(SidecarCallError::Down) | Err(SidecarCallError::Gone { .. }) => {
            Ok(unavailable_answer(generation, now_ms))
        }
        Err(err) => Err(GildeErrorPayload::from(GildeError::from(err))),
    }
}

/// The answer while there is no sidecar to ask.
pub fn unavailable_answer(generation: i64, now_ms: i64) -> Value {
    let mut answer = Map::new();
    answer.insert("available".into(), Value::Bool(false));
    answer.insert("supported".into(), Value::Bool(true));
    answer.insert("snapshot".into(), Value::Null);
    answer.insert("hostKey".into(), Value::Null);
    answer.insert("receivedAt".into(), Value::Null);
    answer.insert("stale".into(), Value::Bool(false));
    answer.insert("fromCache".into(), Value::Bool(false));
    answer.insert("unavailable".into(), Value::Null);
    answer.insert("minPollMs".into(), Value::from(MIN_POLL_MS));
    answer.insert("nextPollAt".into(), Value::from(now_ms));
    answer.insert("generation".into(), Value::from(generation));
    Value::Object(answer)
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every field the frontend's `CcoWorkersAnswer` declares.
    const ANSWER_KEYS: [&str; 11] = [
        "available",
        "supported",
        "snapshot",
        "hostKey",
        "receivedAt",
        "stale",
        "fromCache",
        "unavailable",
        "nextPollAt",
        "minPollMs",
        "generation",
    ];

    fn assert_down_shape(answer: &Value, generation: i64, now: i64) {
        let object = answer.as_object().expect("an object");
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        let mut want = ANSWER_KEYS.to_vec();
        want.sort_unstable();
        assert_eq!(keys, want, "exactly the CcoWorkersAnswer fields");
        assert_eq!(
            answer,
            &json!({
                "available": false,
                "supported": true,
                "snapshot": null,
                "hostKey": null,
                "receivedAt": null,
                "stale": false,
                "fromCache": false,
                "unavailable": null,
                "minPollMs": 60_000,
                "nextPollAt": now,
                "generation": generation,
            })
        );
    }

    #[test]
    fn a_down_sidecar_answers_unavailable_with_no_snapshot() {
        let answer = workers_answer(Err(SidecarCallError::Down), 0, 1_760_000_000_000)
            .expect("down is not an error");
        assert_down_shape(&answer, 0, 1_760_000_000_000);
    }

    #[test]
    fn a_sidecar_gone_under_the_call_answers_unavailable_too() {
        let answer = workers_answer(
            Err(SidecarCallError::Gone { generation: 3 }),
            4,
            1_760_000_000_123,
        )
        .expect("gone is not an error");
        // The generation is the one current when the answer is composed.
        assert_down_shape(&answer, 4, 1_760_000_000_123);
    }

    #[test]
    fn an_answer_passes_through_with_available_and_generation_stamped() {
        let from_sidecar = json!({
            "supported": true,
            "snapshot": { "schema": "cco.workers/1", "generatedAt": 1, "accounts": [] },
            "hostKey": "s123",
            "receivedAt": 2,
            "stale": false,
            "fromCache": true,
            "unavailable": null,
            "nextPollAt": 3,
            "minPollMs": 60_000,
            // A sidecar cannot claim availability or a generation for itself.
            "available": false,
            "generation": 99,
        });
        let answer = workers_answer(Ok(from_sidecar.clone()), 7, 0).expect("ok");
        let mut want = from_sidecar;
        want["available"] = json!(true);
        want["generation"] = json!(7);
        assert_eq!(answer, want, "everything else is passed through untouched");
    }

    #[test]
    fn a_non_object_answer_is_an_error_not_an_empty_list() {
        for bad in [Value::Null, json!([]), json!("x"), json!(1)] {
            assert!(workers_answer(Ok(bad.clone()), 1, 0).is_err(), "{bad} must be refused");
        }
    }

    #[test]
    fn other_failures_stay_errors() {
        let timeout = SidecarCallError::Timeout {
            method: method::WORKERS.to_string(),
            ms: 40_000,
        };
        assert!(workers_answer(Err(timeout), 1, 0).is_err());
    }

    #[test]
    fn the_call_outlasts_the_sidecars_own_wait() {
        // WORKERS_REQUEST_TIMEOUT_MS (30 s) + WORKERS_SETTLE_MS (3 s).
        assert!(CALL_TIMEOUT >= Duration::from_secs(40));
        assert!(CALL_TIMEOUT > Duration::from_millis(30_000 + 3_000));
    }
}
