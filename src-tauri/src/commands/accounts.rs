//! Settings and account commands (DESIGN §3 `app` / `accounts`).
//!
//! `app_settings_*` live here rather than in `app.rs` because they are the
//! command surface of [`crate::config::ConfigStore`], which this task owns;
//! `app.rs` is T01's file. Wiring (`app.manage(...)` + `generate_handler!`) is
//! T29's — see the state aliases below for the exact types to register.

use std::sync::Arc;

use tauri::State;

use crate::accounts::{self, AccountSlot, ImportReport};
use crate::commands::app::PathsState;
use crate::config::{AppConfig, ConfigStore};
use crate::error::CmdResult;
use crate::store::Db;

/// Managed state: the loaded `config.json`.
pub type ConfigState = Arc<ConfigStore>;

/// Managed state: the `state.db` handle (cheap to clone, one shared connection).
pub type DbState = Db;

/// Current settings (DESIGN §10-3).
#[tauri::command]
pub async fn app_settings_get(config: State<'_, ConfigState>) -> CmdResult<AppConfig> {
    Ok(config.get())
}

/// Deep-merge `patch` into the settings and return the result.
///
/// The frontend sends only the subtree it changed. A `null` value clears a key
/// back to its default. The write is debounced 500 ms and lands atomically; a
/// patch that would produce an unusable config is rejected and changes nothing.
#[tauri::command]
pub async fn app_settings_set(
    patch: serde_json::Value,
    config: State<'_, ConfigState>,
) -> CmdResult<AppConfig> {
    Ok(config.patch(patch)?)
}

/// Every account slot, with a freshly probed login badge.
#[tauri::command]
pub async fn accounts_list(
    db: State<'_, DbState>,
    paths: State<'_, PathsState>,
) -> CmdResult<Vec<AccountSlot>> {
    Ok(accounts::list(db.inner(), paths.inner()).await?)
}

/// Re-read `~/.claude-code-orchestrator/registry.json` (read-only) and refresh
/// the account table from it.
#[tauri::command]
pub async fn accounts_import_cco(
    db: State<'_, DbState>,
    config: State<'_, ConfigState>,
    paths: State<'_, PathsState>,
) -> CmdResult<ImportReport> {
    Ok(accounts::import_cco(db.inner(), config.inner(), paths.inner()).await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::AppPaths;

    /// The command bodies are one-liners over the services, which have their own
    /// tests; what is worth pinning here is that the four functions keep the
    /// signatures `src/lib/ipc.ts` declares.
    #[tokio::test]
    async fn the_services_behind_the_commands_answer_with_the_wire_shapes() {
        let home = tempfile::tempdir().expect("tempdir");
        let paths = AppPaths::for_home(home.path());
        paths.ensure_all().expect("dirs");

        let db = Db::open_in_memory().expect("db");
        let config = Arc::new(ConfigStore::load(&paths).expect("config"));

        let settings = serde_json::to_value(config.get()).expect("serialize");
        assert_eq!(settings["locale"], "system");

        let patched = config
            .patch(serde_json::json!({ "locale": "en" }))
            .expect("patch");
        assert_eq!(serde_json::to_value(patched).expect("serialize")["locale"], "en");
        // F1: "system" is a value `app_settings_set` accepts (back to following the OS).
        let patched = config
            .patch(serde_json::json!({ "locale": "system" }))
            .expect("patch");
        assert_eq!(serde_json::to_value(patched).expect("serialize")["locale"], "system");
        let patched = config
            .patch(serde_json::json!({ "locale": "en" }))
            .expect("patch");
        assert_eq!(serde_json::to_value(patched).expect("serialize")["locale"], "en");

        let listed = accounts::list(&db, &paths).await.expect("list");
        assert_eq!(serde_json::to_value(&listed).expect("serialize").as_array().map(Vec::len), Some(1));

        let report = accounts::import_cco(&db, &config, &paths)
            .await
            .expect("import");
        let value = serde_json::to_value(report).expect("serialize");
        assert_eq!(value["imported"], 0);
        assert!(value["accounts"].is_array());
    }
}
