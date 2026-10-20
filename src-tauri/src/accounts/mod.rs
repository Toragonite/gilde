//! Account slots (DESIGN §10-5).
//!
//! **M1 does display and plumbing only.** There is no pool, no `pickWorker`, no
//! quota failover, no cooldown, no keepalive. What exists is the full path an
//! account id travels — `OpenSessionReq.accountId` → launch template
//! `account_env_key` → PTY env, and `DispatchTarget.accountId` → `RunTask.account`
//! → sidecar → adapter — plus the read-only list the settings pane renders. That
//! is the literal reading of DECISIONS' "설계상 슬롯만 확보", and it is what makes
//! M2's pool a pure addition instead of a refactor.
//!
//! The default is the *ambient* login: `account: undefined`, which agent-mesh
//! reads as "use whatever the CLI itself is logged into".

use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::config::cco_import::{self, CcoSettings, Watcher};
use crate::config::model::RegistryMode;
use crate::config::ConfigStore;
use crate::error::Result;
use crate::paths::AppPaths;
use crate::store::models::{AccountRow, AccountSource};
use crate::store::{dao, now_ms, Db};

/// The only vendor M1 imports accounts for. Codex/Cursor/Gemini accounts are an
/// M2 slot: the column and the plumbing exist, the importer does not.
pub const VENDOR_CLAUDE: &str = "claude";

/// `settings_kv` key holding the read-only half of `registry.json` — cooldown
/// and the two billing guards, shown as "managed by claude-code-orchestrator".
pub const CCO_READ_ONLY_KEY: &str = "accounts.ccoReadOnly";

/// One account slot as the UI sees it (`AccountSlot` in `src/lib/ipc.ts`).
///
/// `vendor` is a plain string rather than an enum: the vendor list is owned by
/// the sidecar/dispatch layer, and an account row must survive a vendor this
/// build has not heard of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountSlot {
    pub id: String,
    pub vendor: String,
    pub label: String,
    /// `CLAUDE_CONFIG_DIR` for this account; `None` for the ambient slot.
    pub config_dir: Option<String>,
    pub model_hint: Option<String>,
    pub source: AccountSource,
    pub enabled: bool,
    pub is_default: bool,
    /// Probed live at read time — a user can log in while the app is running.
    pub logged_in: bool,
    pub last_seen_at: Option<i64>,
    pub last_quota_at: Option<i64>,
}

impl AccountSlot {
    /// Imported from `~/.claude-code-orchestrator/registry.json`.
    pub fn is_cco(&self) -> bool {
        self.source == AccountSource::Cco
    }

    /// The synthetic "whatever `~/.claude` is logged into" slot.
    pub fn is_ambient(&self) -> bool {
        self.source == AccountSource::Ambient
    }

    /// Projection into a `state.db` row. `imported_at` is supplied by the caller
    /// so a whole import shares one timestamp.
    pub fn to_row(&self, imported_at: i64) -> AccountRow {
        AccountRow {
            id: self.id.clone(),
            vendor: self.vendor.clone(),
            label: self.label.clone(),
            config_dir: self.config_dir.clone(),
            model_hint: self.model_hint.clone(),
            source: self.source,
            enabled: self.enabled,
            is_default: self.is_default,
            imported_at,
            last_seen_at: self.last_seen_at,
            last_quota_json: None,
            last_quota_at: self.last_quota_at,
        }
    }

    /// Rehydrate from a row, re-probing the login badge against the filesystem.
    pub fn from_row(row: &AccountRow, home: &Path) -> Self {
        let logged_in = match (&row.config_dir, row.source) {
            (Some(dir), _) => cco_import::dir_has_login(Path::new(dir)),
            // The ambient slot has no directory of its own: `~/.claude` is it.
            (None, AccountSource::Ambient) => cco_import::dir_has_login(&home.join(".claude")),
            (None, _) => false,
        };

        Self {
            id: row.id.clone(),
            vendor: row.vendor.clone(),
            label: row.label.clone(),
            config_dir: row.config_dir.clone(),
            model_hint: row.model_hint.clone(),
            source: row.source,
            enabled: row.enabled,
            is_default: row.is_default,
            logged_in,
            last_seen_at: row.last_seen_at,
            last_quota_at: row.last_quota_at,
        }
    }
}

/// Outcome of `accounts_import_cco` (`ImportReport` in `src/lib/ipc.ts`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportReport {
    /// cco worker accounts written (the ambient slot is synthetic, not imported).
    pub imported: u32,
    /// Worker entries the file described but we could not use.
    pub skipped: u32,
    /// Every slot the app now knows about, ambient included.
    pub accounts: Vec<AccountSlot>,
    // T26: the billing guards the panel shows read-only. They were already
    // persisted under `accounts.ccoReadOnly` (see `cco_read_only`), but nothing
    // carried them to the frontend; the accounts panel reads them from here.
    /// The non-account half of `registry.json`, for read-only display only.
    /// `None` when the registry could not be read.
    pub guards: Option<CcoSettings>,
}

/// Every persisted account, with live login badges.
///
/// A database that has never been imported into still answers with the ambient
/// slot, so the UI always has one selectable account. **Reads never write.**
pub async fn list(db: &Db, paths: &AppPaths) -> Result<Vec<AccountSlot>> {
    let home = paths.home_dir().to_path_buf();
    let rows = db.read(dao::account::list).await?;

    if rows.is_empty() {
        return Ok(vec![cco_import::ambient_account(&home)]);
    }
    Ok(rows
        .iter()
        .map(|row| AccountSlot::from_row(row, &home))
        .collect())
}

/// Import `~/.claude-code-orchestrator/registry.json` into `state.db`
/// (DESIGN §10-5). Read-only with respect to that file.
///
/// Config mapping, in the order DESIGN's table gives it:
///
/// | cco | ours |
/// |---|---|
/// | `claudePath` | `agents.binaries.claude`, **only when it is null** |
/// | `permissionMode` | `dispatch.permissionMode` default |
/// | `workerTimeoutMinutes` | `dispatch.defaultTimeoutMinutes` default |
/// | `cooldownMinutes`, `frontierWorkerDispatch`, `overageWorkerDispatch` | stored read-only |
///
/// The two dispatch defaults are only filled in while they still hold the
/// built-in value: they are *defaults*, and a re-import must not silently undo a
/// choice the user made in Settings.
pub async fn import_cco(db: &Db, config: &ConfigStore, paths: &AppPaths) -> Result<ImportReport> {
    let scan = cco_import::scan_cco_registry(paths);
    let home = paths.home_dir().to_path_buf();
    let imported_at = now_ms();

    let rows: Vec<AccountRow> = scan
        .accounts
        .iter()
        .map(|slot| slot.to_row(imported_at))
        .collect();
    let imported = scan.accounts.iter().filter(|a| a.is_cco()).count() as u32;

    let settings = scan.settings.clone();
    let stored = db
        .write(move |conn| {
            let tx = conn.unchecked_transaction()?;
            for row in &rows {
                dao::account::upsert_by_config_dir(&tx, row)?;
            }
            dao::settings_kv::set(
                &tx,
                CCO_READ_ONLY_KEY,
                &serde_json::to_value(&settings).unwrap_or(serde_json::Value::Null),
            )?;
            tx.commit()?;
            dao::account::list(conn)
        })
        .await?;

    apply_cco_settings_to_config(config, &scan.settings)?;

    Ok(ImportReport {
        imported,
        skipped: scan.skipped,
        accounts: stored
            .iter()
            .map(|row| AccountSlot::from_row(row, &home))
            .collect(),
        // T26: same value that just went into `settings_kv`, handed to the UI.
        guards: Some(scan.settings),
    })
}

/// Apply `accounts.registryMode` at boot (DESIGN §10-5).
///
/// * `"import-once"` (default) — read the registry on the **first run only**
///   (an empty `account` table). After that our own table is authoritative and
///   the file is read again only when the user asks for a re-import.
/// * `"follow"` — import now, then keep watching with a 500 ms debounce and
///   re-import on every change. Still never writes to the registry.
///
/// The returned [`Watcher`] must be kept alive for follow mode to stay live;
/// dropping it stops the watch.
pub async fn apply_registry_mode(
    db: &Db,
    config: &Arc<ConfigStore>,
    paths: &AppPaths,
) -> Result<Option<Watcher>> {
    let mode = config.get().accounts.registry_mode;

    match mode {
        RegistryMode::ImportOnce => {
            let first_run = db.read(|conn| dao::account::list(conn)).await?.is_empty();
            if first_run {
                import_cco(db, config, paths).await?;
            }
            Ok(None)
        }
        RegistryMode::Follow => {
            import_cco(db, config, paths).await?;
            Ok(Some(follow_registry(
                db.clone(),
                config.clone(),
                paths.clone(),
            )?))
        }
    }
}

/// Watch `registry.json` and re-import on every change (follow mode).
///
/// Must be called from inside a tokio runtime: the watcher callback is
/// synchronous and hands the re-import to the runtime rather than blocking the
/// notify thread on a database write.
pub fn follow_registry(db: Db, config: Arc<ConfigStore>, paths: AppPaths) -> Result<Watcher> {
    let handle = tokio::runtime::Handle::try_current().map_err(|e| {
        crate::error::GildeError::internal(format!("follow mode needs a tokio runtime: {e}"))
    })?;

    let watch_paths = paths.clone();
    cco_import::watch_cco_registry(&watch_paths, move || {
        let db = db.clone();
        let config = config.clone();
        let paths = paths.clone();
        handle.spawn(async move {
            match import_cco(&db, &config, &paths).await {
                Ok(report) => tracing::info!(
                    imported = report.imported,
                    skipped = report.skipped,
                    "re-imported the claude-code-orchestrator registry"
                ),
                Err(err) => {
                    tracing::warn!(error = %err, "re-import of the cco registry failed")
                }
            }
        });
    })
}

/// The read-only cco settings last seen by an import, for the accounts pane.
pub async fn cco_read_only(db: &Db) -> Result<Option<CcoSettings>> {
    let value = db
        .read(|conn| dao::settings_kv::get(conn, CCO_READ_ONLY_KEY))
        .await?;
    Ok(value.and_then(|v| serde_json::from_value(v).ok()))
}

/// Fold the non-account half of `registry.json` into `config.json`.
fn apply_cco_settings_to_config(config: &ConfigStore, settings: &CcoSettings) -> Result<()> {
    use crate::config::model::{AppConfig, DispatchConfig};

    let current = config.get();
    let defaults = DispatchConfig::default();
    let mut patch = serde_json::Map::new();

    if let Some(path) = settings.claude_path.as_deref() {
        // Only when the user has not pinned one. cco already resolved the nvm
        // path, which saves us a login-shell round trip; `agents::path` still
        // re-verifies it before anything is launched.
        if current.agent_binary("claude").is_none() {
            patch.insert(
                "agents".to_string(),
                serde_json::json!({ "binaries": { "claude": path } }),
            );
        }
    }

    let mut dispatch = serde_json::Map::new();
    if let Some(mode) = settings.permission_mode.as_deref() {
        if current.dispatch.permission_mode == defaults.permission_mode {
            dispatch.insert("permissionMode".to_string(), mode.into());
        }
    }
    if let Some(minutes) = settings.worker_timeout_minutes {
        if current.dispatch.default_timeout_minutes == defaults.default_timeout_minutes {
            dispatch.insert("defaultTimeoutMinutes".to_string(), minutes.into());
        }
    }
    if !dispatch.is_empty() {
        patch.insert("dispatch".to_string(), dispatch.into());
    }

    if patch.is_empty() {
        return Ok(());
    }

    let _: AppConfig = config.patch(serde_json::Value::Object(patch))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::model::AppConfig;

    const FIXTURE: &str = r#"{
      "workers": [
        { "name": "work-claude", "configDir": "~/.claude-work-claude", "model": "claude-opus-4-8" },
        { "name": "work-gpt",    "configDir": "~/.claude-work-gpt",    "model": "claude-opus-4-8" },
        { "name": "personal",     "configDir": "~/.claude-personal",     "model": "claude-opus-4-8" }
      ],
      "permissionMode": "bypassPermissions",
      "claudePath": "/opt/nvm/bin/claude",
      "cooldownMinutes": 30,
      "workerTimeoutMinutes": 90,
      "frontierWorkerDispatch": "block",
      "overageWorkerDispatch": "allow"
    }"#;

    struct Fixture {
        _home: tempfile::TempDir,
        paths: AppPaths,
        db: Db,
        config: ConfigStore,
    }

    fn fixture(registry: Option<&str>) -> Fixture {
        let home = tempfile::tempdir().expect("tempdir");
        let paths = AppPaths::for_home(home.path());
        paths.ensure_all().expect("ensure dirs");

        if let Some(contents) = registry {
            let file = paths.cco_registry();
            std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
            std::fs::write(&file, contents).expect("write registry");
        }

        Fixture {
            db: Db::open_in_memory().expect("db"),
            config: ConfigStore::load(&paths).expect("config"),
            paths,
            _home: home,
        }
    }

    #[tokio::test]
    async fn an_empty_database_still_offers_the_ambient_account() {
        let f = fixture(None);
        let accounts = list(&f.db, &f.paths).await.expect("list");
        assert_eq!(accounts.len(), 1);
        assert!(accounts[0].is_ambient());
        assert_eq!(accounts[0].config_dir, None);
    }

    #[tokio::test]
    async fn import_persists_three_workers_plus_ambient_and_is_idempotent() {
        let f = fixture(Some(FIXTURE));

        let report = import_cco(&f.db, &f.config, &f.paths).await.expect("import");
        assert_eq!(report.imported, 3);
        assert_eq!(report.skipped, 0);
        assert_eq!(report.accounts.len(), 4);
        assert_eq!(report.accounts.iter().filter(|a| a.is_cco()).count(), 3);
        assert_eq!(report.accounts.iter().filter(|a| a.is_ambient()).count(), 1);

        let ids: Vec<String> = report.accounts.iter().map(|a| a.id.clone()).collect();

        // A second import writes the same rows, not four more.
        let again = import_cco(&f.db, &f.config, &f.paths).await.expect("import");
        assert_eq!(again.accounts.len(), 4);
        let ids_again: Vec<String> = again.accounts.iter().map(|a| a.id.clone()).collect();
        assert_eq!(ids, ids_again);

        let listed = list(&f.db, &f.paths).await.expect("list");
        assert_eq!(listed.len(), 4);
    }

    #[tokio::test]
    async fn a_rename_in_cco_keeps_the_account_because_identity_is_the_directory() {
        let f = fixture(Some(FIXTURE));
        let first = import_cco(&f.db, &f.config, &f.paths).await.expect("import");
        let personal = first
            .accounts
            .iter()
            .find(|a| a.label == "personal")
            .expect("account")
            .clone();

        std::fs::write(
            f.paths.cco_registry(),
            FIXTURE.replace("\"personal\",", "\"personal-renamed\","),
        )
        .expect("rewrite the isolated fixture");

        let second = import_cco(&f.db, &f.config, &f.paths).await.expect("import");
        assert_eq!(second.accounts.len(), 4, "no duplicate row appeared");
        let same = second
            .accounts
            .iter()
            .find(|a| a.id == personal.id)
            .expect("same id survived the rename");
        assert_eq!(same.label, "personal-renamed");
        assert_eq!(same.config_dir, personal.config_dir);
    }

    #[tokio::test]
    async fn import_maps_settings_into_config_without_stomping_user_choices() {
        let f = fixture(Some(FIXTURE));

        import_cco(&f.db, &f.config, &f.paths).await.expect("import");
        let cfg: AppConfig = f.config.get();
        assert_eq!(cfg.agent_binary("claude"), Some("/opt/nvm/bin/claude"));
        assert_eq!(cfg.dispatch.permission_mode, "bypassPermissions");
        assert_eq!(cfg.dispatch.default_timeout_minutes, 90);

        // The user pins their own values...
        f.config
            .patch(serde_json::json!({
                "agents": { "binaries": { "claude": "/usr/local/bin/claude" } },
                "dispatch": { "permissionMode": "plan", "defaultTimeoutMinutes": 5 }
            }))
            .expect("patch");

        // ...and a re-import leaves every one of them alone.
        import_cco(&f.db, &f.config, &f.paths).await.expect("import");
        let cfg = f.config.get();
        assert_eq!(cfg.agent_binary("claude"), Some("/usr/local/bin/claude"));
        assert_eq!(cfg.dispatch.permission_mode, "plan");
        assert_eq!(cfg.dispatch.default_timeout_minutes, 5);
    }

    #[tokio::test]
    async fn billing_guards_are_stored_read_only() {
        let f = fixture(Some(FIXTURE));
        let report = import_cco(&f.db, &f.config, &f.paths).await.expect("import");

        // T26: the same values reach the accounts panel on the report — storing
        // them without a way to show them was the whole gap.
        let guards = report.guards.as_ref().expect("the report carries the guards");
        assert_eq!(guards.cooldown_minutes, Some(30));
        assert_eq!(guards.frontier_worker_dispatch.as_deref(), Some("block"));
        assert_eq!(guards.overage_worker_dispatch.as_deref(), Some("allow"));

        let stored = cco_read_only(&f.db).await.expect("read").expect("present");
        assert_eq!(stored.cooldown_minutes, Some(30));
        assert_eq!(stored.frontier_worker_dispatch.as_deref(), Some("block"));
        assert_eq!(stored.overage_worker_dispatch.as_deref(), Some("allow"));
        assert_eq!(stored.claude_path.as_deref(), Some("/opt/nvm/bin/claude"));
    }

    #[tokio::test]
    async fn a_missing_registry_imports_only_the_ambient_slot() {
        let f = fixture(None);
        let report = import_cco(&f.db, &f.config, &f.paths).await.expect("import");
        assert_eq!(report.imported, 0);
        assert_eq!(report.skipped, 0);
        assert_eq!(report.accounts.len(), 1);
        assert!(report.accounts[0].is_ambient());
        // Nothing was mapped into the config either.
        assert_eq!(f.config.get(), AppConfig::default());
    }

    #[tokio::test]
    async fn login_badges_are_probed_live() {
        let f = fixture(Some(FIXTURE));
        let before = import_cco(&f.db, &f.config, &f.paths).await.expect("import");
        assert!(before.accounts.iter().all(|a| !a.logged_in));

        let dir = f.paths.home_dir().join(".claude-personal");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join(".credentials.json"), b"{}").expect("write");

        let listed = list(&f.db, &f.paths).await.expect("list");
        let personal = listed
            .iter()
            .find(|a| a.label == "personal")
            .expect("account");
        assert!(personal.logged_in);
        assert_eq!(listed.iter().filter(|a| a.logged_in).count(), 1);
    }

    #[tokio::test]
    async fn import_once_reads_the_registry_only_on_a_first_run() {
        let f = fixture(Some(FIXTURE));
        let config = Arc::new(ConfigStore::load(&f.paths).expect("config"));
        assert_eq!(config.get().accounts.registry_mode, RegistryMode::ImportOnce);

        let watcher = apply_registry_mode(&f.db, &config, &f.paths)
            .await
            .expect("first run");
        assert!(watcher.is_none(), "import-once must not watch");
        assert_eq!(list(&f.db, &f.paths).await.expect("list").len(), 4);

        // A change to the file is ignored from now on: our table is authoritative.
        std::fs::write(
            f.paths.cco_registry(),
            FIXTURE.replace("\"personal\",", "\"personal-renamed\","),
        )
        .expect("rewrite the isolated fixture");

        apply_registry_mode(&f.db, &config, &f.paths)
            .await
            .expect("second run");
        let labels: Vec<String> = list(&f.db, &f.paths)
            .await
            .expect("list")
            .into_iter()
            .map(|a| a.label)
            .collect();
        assert!(labels.contains(&"personal".to_string()));
        assert!(!labels.contains(&"personal-renamed".to_string()));
    }

    #[tokio::test]
    async fn follow_mode_reimports_when_the_registry_changes() {
        let f = fixture(Some(FIXTURE));
        let config = Arc::new(ConfigStore::load(&f.paths).expect("config"));
        config
            .patch(serde_json::json!({ "accounts": { "registryMode": "follow" } }))
            .expect("patch");

        let watcher = apply_registry_mode(&f.db, &config, &f.paths)
            .await
            .expect("follow")
            .expect("follow mode returns a watcher");
        assert_eq!(
            watcher.watched_dir(),
            f.paths.cco_registry().parent().expect("parent")
        );

        std::fs::write(
            f.paths.cco_registry(),
            FIXTURE.replace("\"personal\",", "\"personal-renamed\","),
        )
        .expect("rewrite the isolated fixture");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let labels: Vec<String> = list(&f.db, &f.paths)
                .await
                .expect("list")
                .into_iter()
                .map(|a| a.label)
                .collect();
            if labels.contains(&"personal-renamed".to_string()) {
                // The directory is the identity, so the row was renamed in
                // place rather than duplicated.
                assert_eq!(labels.len(), 4, "{labels:?}");
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "follow mode never re-imported: {labels:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    #[test]
    fn slots_serialize_with_the_ipc_field_names() {
        let slot = AccountSlot {
            id: "id".into(),
            vendor: VENDOR_CLAUDE.into(),
            label: "personal".into(),
            config_dir: Some("/Users/x/.claude-personal".into()),
            model_hint: Some("claude-opus-4-8".into()),
            source: AccountSource::Cco,
            enabled: true,
            is_default: false,
            logged_in: true,
            last_seen_at: None,
            last_quota_at: None,
        };

        let value = serde_json::to_value(&slot).expect("serialize");
        let obj = value.as_object().expect("object");
        for key in [
            "id",
            "vendor",
            "label",
            "configDir",
            "modelHint",
            "source",
            "enabled",
            "isDefault",
            "loggedIn",
            "lastSeenAt",
            "lastQuotaAt",
        ] {
            assert!(obj.contains_key(key), "missing {key}");
        }
        assert_eq!(obj.len(), 11);
        assert_eq!(value["source"], "cco");

        let report = serde_json::to_value(ImportReport {
            imported: 3,
            skipped: 0,
            accounts: vec![slot],
            guards: Some(CcoSettings {
                cooldown_minutes: Some(30),
                frontier_worker_dispatch: Some("block".to_string()),
                overage_worker_dispatch: Some("block".to_string()),
                ..CcoSettings::default()
            }),
        })
        .expect("serialize");
        assert_eq!(report["imported"], 3);
        assert_eq!(report["skipped"], 0);
        assert!(report["accounts"].is_array());
        // T26: the guards travel to the frontend on the report, camelCased
        // exactly as `ImportReport` in `src/lib/ipc.ts` declares them.
        assert_eq!(report["guards"]["cooldownMinutes"], 30);
        assert_eq!(report["guards"]["frontierWorkerDispatch"], "block");
        assert_eq!(report["guards"]["overageWorkerDispatch"], "block");
    }
}
