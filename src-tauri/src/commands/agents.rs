//! `agents_*` commands — DESIGN §3 `agents`, M1-PLAN T09.
//!
//! Thin over [`crate::agents::AgentRegistry`], like every command module: they
//! validate nothing (there is nothing to validate), call one service method and
//! return the shape `src/lib/ipc.ts` declares.
//!
//! Wiring — `app.manage(...)` plus the `generate_handler!` entries — is T29's,
//! in `lib.rs`. The state alias below names the exact type to register.

use std::sync::Arc;

use tauri::State;

use crate::agents::{AgentBinary, AgentRegistry, LaunchTemplate, PathReport, SignaturesReport};
use crate::error::CmdResult;

/// Managed state: the agent registry (detection caches + live signatures).
pub type AgentsState = Arc<AgentRegistry>;

/// Every agent kind, installed or not (DESIGN §3).
///
/// `refresh: true` bypasses both the 60 s in-memory cache and the
/// `agent_binary` rows, and re-runs the login-shell `PATH` probe. It is what
/// the "Rescan" button in Settings calls; the boot path passes `false`.
///
/// A missing CLI is a row with `installed: false`, never an error — the app has
/// to stay usable on a machine where only one of the five is installed.
#[tauri::command]
pub async fn agents_detect(
    refresh: bool,
    agents: State<'_, AgentsState>,
) -> CmdResult<Vec<AgentBinary>> {
    Ok(agents.detect(refresh).await?)
}

/// How `PATH` was resolved, for the diagnostics sheet (DESIGN §10-6).
///
/// Answers "why can't it find `codex`" without a shell: the rung that produced
/// the answer, the fingerprint that keyed the cache, and every warning collected
/// on the way down the ladder.
#[tauri::command]
pub async fn agents_path_report(agents: State<'_, AgentsState>) -> CmdResult<PathReport> {
    let agents = agents.inner().clone();
    Ok(tokio::task::spawn_blocking(move || agents.path_report(false))
        .await
        .map_err(|e| crate::error::GildeError::internal(format!("path probe panicked: {e}")))?)
}

/// One launch template per installed agent (DESIGN §6-6).
#[tauri::command]
pub async fn agents_launch_templates(
    agents: State<'_, AgentsState>,
) -> CmdResult<Vec<LaunchTemplate>> {
    Ok(agents.templates().await?)
}

/// Re-read `agent-signatures.json` on demand.
///
/// The file is also watched (2 s hot reload); this is the explicit path, used
/// by the "Reload signatures" affordance next to the evidence popover, and it
/// returns the per-pattern compile warnings so a typo is visible immediately
/// instead of silently costing one pattern.
#[tauri::command]
pub async fn agents_reload_signatures(
    agents: State<'_, AgentsState>,
) -> CmdResult<SignaturesReport> {
    Ok(agents.reload_signatures())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::AgentKind;
    use crate::config::ConfigStore;
    use crate::paths::AppPaths;

    /// The bodies are one-liners over services that have their own tests; what
    /// is worth pinning here is that the four functions answer with the wire
    /// shapes `src/lib/ipc.ts` declares.
    #[tokio::test]
    async fn the_services_behind_the_commands_answer_with_the_wire_shapes() {
        let home = tempfile::tempdir().expect("tempdir");
        let paths = AppPaths::for_home(home.path());
        paths.ensure_all().expect("dirs");
        let config = Arc::new(ConfigStore::load(&paths).expect("config"));
        let registry = Arc::new(AgentRegistry::without_db(paths, config));

        let detected = registry.detect(false).await.expect("detect");
        let value = serde_json::to_value(&detected).expect("json");
        let rows = value.as_array().expect("array");
        assert_eq!(rows.len(), AgentKind::ALL.len());
        for row in rows {
            assert!(row["kind"].is_string());
            assert!(row.get("installed").is_some());
            assert!(row.get("ok").is_some());
            assert!(row.get("verified").is_some());
        }

        let report = serde_json::to_value(registry.path_report(false)).expect("json");
        assert!(report["resolved"].is_array(), "ipc.ts calls it `resolved`");
        assert!(report["fingerprint"].is_string());
        assert!(report["probedAt"].is_number());

        let templates = serde_json::to_value(registry.templates().await.expect("templates"))
            .expect("json");
        assert!(templates.is_array());

        let signatures = serde_json::to_value(registry.reload_signatures()).expect("json");
        assert_eq!(signatures["agents"], serde_json::json!(6));
        assert!(signatures["warnings"].is_array());
    }
}
