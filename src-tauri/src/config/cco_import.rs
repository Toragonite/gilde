//! Read-only import of `~/.claude-code-orchestrator/registry.json` (DESIGN §10-5).
//!
//! **This module never writes to that directory — not one byte.** The directory
//! is actively owned by a VS Code extension and N dispatch-server processes that
//! coordinate through their own hand-rolled advisory file lock; joining that
//! protocol from Rust would mean a second, untested implementation of a lock.
//! M1 needs the account list, not write access.
//!
//! Two guards keep it that way:
//!
//! * `watch_is_read_only_over_a_long_window` copies a registry into an isolated
//!   home, imports it, follows it for a full minute and asserts mtime, size and
//!   content hash are all unchanged;
//! * `this_module_contains_no_write_calls` greps this file's own source for
//!   filesystem mutation APIs.
//!
//! Identity follows cco's `findWorkerByConfigDir`: the **lexically resolved**
//! `configDir` is the account's identity, not its name (a worker keeps its
//! directory across renames, so a name is not stable). Symlinks are deliberately
//! not resolved — that is what `path.resolve` does on their side, and an id that
//! changed when a directory disappeared would fork the account row in two.

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use notify::RecursiveMode;
use notify_debouncer_full::{new_debouncer, DebounceEventResult};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::accounts::{AccountSlot, ImportReport, VENDOR_CLAUDE};
use crate::error::{GildeError, Result};
use crate::paths::AppPaths;
use crate::store::models::AccountSource;

/// Debounce for `registryMode = "follow"` (DESIGN §10-5).
pub const WATCH_DEBOUNCE: Duration = Duration::from_millis(500);

/// Namespace for every derived account id.
///
/// DESIGN specifies `uuid_v5("cco:" + canonical(configDir))`; RFC 4122 v5 also
/// needs a namespace, and this is ours. **Changing this constant renames every
/// account**, so it is frozen.
pub const GILDE_ACCOUNT_NAMESPACE: Uuid = Uuid::from_u128(0x6f1e_0e6c_9d2a_5f7b_9a4d_3c0b_8e5a_1d42);

/// The synthetic "whatever `~/.claude` is logged into" slot (DESIGN §10-5).
/// Always present, always last, never carries a `configDir`.
pub const AMBIENT_ACCOUNT_NAME: &str = "ambient:claude";

/// Login probe filenames — cco's `dirHasLogin` heuristic, so our badge agrees
/// with theirs.
const LOGIN_MARKERS: [&str; 2] = [".credentials.json", ".claude.json"];

// ---------------------------------------------------------------------------
// the file
// ---------------------------------------------------------------------------

/// `registry.json` as far as we care about it. Unknown fields are ignored —
/// cco keeps adding mesh knobs and none of them are ours.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CcoRegistry {
    pub workers: Option<Vec<CcoWorker>>,
    #[serde(rename = "claudePath")]
    pub claude_path: Option<String>,
    #[serde(rename = "permissionMode")]
    pub permission_mode: Option<String>,
    #[serde(rename = "workerTimeoutMinutes")]
    pub worker_timeout_minutes: Option<u32>,
    #[serde(rename = "cooldownMinutes")]
    pub cooldown_minutes: Option<u32>,
    #[serde(rename = "frontierWorkerDispatch")]
    pub frontier: Option<String>,
    #[serde(rename = "overageWorkerDispatch")]
    pub overage: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CcoWorker {
    pub name: String,
    #[serde(rename = "configDir")]
    pub config_dir: String,
    pub model: Option<String>,
    /// Absent from the live file; harmless because it is optional.
    pub preferred: Option<bool>,
}

/// Everything from `registry.json` that is *not* an account.
///
/// `cooldown_minutes`, `frontier` and `overage` are stored and displayed
/// read-only ("billing guards, managed by claude-code-orchestrator"); M1 never
/// acts on them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CcoSettings {
    /// Already-resolved absolute path to `claude` — saves one login-shell probe.
    pub claude_path: Option<String>,
    pub permission_mode: Option<String>,
    pub worker_timeout_minutes: Option<u32>,
    pub cooldown_minutes: Option<u32>,
    pub frontier_worker_dispatch: Option<String>,
    pub overage_worker_dispatch: Option<String>,
}

impl CcoSettings {
    fn from_registry(reg: &CcoRegistry) -> Self {
        Self {
            claude_path: non_empty(reg.claude_path.as_deref()),
            permission_mode: non_empty(reg.permission_mode.as_deref()),
            worker_timeout_minutes: reg.worker_timeout_minutes,
            cooldown_minutes: reg.cooldown_minutes,
            frontier_worker_dispatch: non_empty(reg.frontier.as_deref()),
            overage_worker_dispatch: non_empty(reg.overage.as_deref()),
        }
    }
}

/// Result of reading the registry: candidate account slots plus the settings.
///
/// Nothing here has touched the database yet — [`crate::accounts::import_cco`]
/// does the persisting.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CcoScan {
    /// Imported workers, in file order, followed by the synthetic ambient slot.
    pub accounts: Vec<AccountSlot>,
    pub settings: CcoSettings,
    /// Whether a parseable `registry.json` was found at all.
    pub found: bool,
    /// Worker entries that were dropped (blank or duplicate `configDir`).
    pub skipped: u32,
}

// ---------------------------------------------------------------------------
// reading
// ---------------------------------------------------------------------------

/// Read and parse the registry. A missing or damaged file is **not an error**:
/// it yields `found: false` and only the ambient account.
pub fn scan_cco_registry(paths: &AppPaths) -> CcoScan {
    let path = paths.cco_registry();
    let home = paths.home_dir();

    let (registry, found) = match std::fs::read_to_string(&path) {
        Ok(raw) => match serde_json::from_str::<CcoRegistry>(&raw) {
            Ok(reg) => (reg, true),
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "claude-code-orchestrator registry.json did not parse; importing nothing"
                );
                (CcoRegistry::default(), false)
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => (CcoRegistry::default(), false),
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "claude-code-orchestrator registry.json is unreadable; importing nothing"
            );
            (CcoRegistry::default(), false)
        }
    };

    let mut accounts = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let mut skipped = 0u32;

    for worker in registry.workers.iter().flatten() {
        let dir = canonical_config_dir(home, &worker.config_dir);
        if dir.is_empty() || worker.name.trim().is_empty() {
            skipped += 1;
            continue;
        }
        if seen.iter().any(|d| d == &dir) {
            // cco itself refuses two workers on one directory; if the file has
            // them anyway, the first wins and the id stays stable.
            skipped += 1;
            continue;
        }
        seen.push(dir.clone());

        accounts.push(AccountSlot {
            id: account_id_for_config_dir(&dir),
            vendor: VENDOR_CLAUDE.to_string(),
            label: worker.name.clone(),
            config_dir: Some(dir.clone()),
            model_hint: non_empty(worker.model.as_deref()),
            source: AccountSource::Cco,
            enabled: true,
            is_default: false,
            logged_in: dir_has_login(Path::new(&dir)),
            last_seen_at: None,
            last_quota_at: None,
        });
    }

    accounts.push(ambient_account(home));

    CcoScan {
        accounts,
        settings: CcoSettings::from_registry(&registry),
        found,
        skipped,
    }
}

/// The DESIGN §T03 entry point: scan and describe, without persisting.
///
/// `imported` counts the cco workers (the ambient slot is synthetic and is not
/// "imported" from anywhere), `skipped` counts dropped worker entries.
pub fn import_cco_registry(paths: &AppPaths) -> ImportReport {
    let scan = scan_cco_registry(paths);
    ImportReport {
        imported: scan.accounts.iter().filter(|a| a.is_cco()).count() as u32,
        skipped: scan.skipped,
        accounts: scan.accounts,
        // T26: read-only billing guards, for display in the accounts panel.
        guards: Some(scan.settings),
    }
}

/// The always-present "use whatever `~/.claude` is logged into" slot.
pub fn ambient_account(home: &Path) -> AccountSlot {
    AccountSlot {
        id: uuid_v5(GILDE_ACCOUNT_NAMESPACE, AMBIENT_ACCOUNT_NAME).to_string(),
        vendor: VENDOR_CLAUDE.to_string(),
        label: "ambient".to_string(),
        // NULL on purpose: the ambient login has no dedicated `CLAUDE_CONFIG_DIR`,
        // and the DAO keys ambient rows by id for exactly this reason.
        config_dir: None,
        model_hint: None,
        source: AccountSource::Ambient,
        enabled: true,
        is_default: true,
        logged_in: dir_has_login(&home.join(".claude")),
        last_seen_at: None,
        last_quota_at: None,
    }
}

/// `uuid_v5("cco:" + canonical(configDir))` — DESIGN §10-5.
pub fn account_id_for_config_dir(canonical_dir: &str) -> String {
    uuid_v5(GILDE_ACCOUNT_NAMESPACE, &format!("cco:{canonical_dir}")).to_string()
}

/// Whether `config_dir` holds a Claude Code login.
///
/// Same heuristic as cco's `dirHasLogin` (`.credentials.json` or `.claude.json`),
/// so our badge and theirs always agree. Any I/O error reads as "no login".
pub fn dir_has_login(config_dir: &Path) -> bool {
    LOGIN_MARKERS
        .iter()
        .any(|marker| config_dir.join(marker).exists())
}

/// Absolute, lexically normalized form of a `configDir`.
///
/// `~` expands against `home`; a relative path is joined onto `home` (the cco
/// extension resolves against its own cwd, which we have no equivalent of);
/// `.`/`..`/duplicate and trailing separators are folded. Symlinks are **not**
/// resolved, matching `path.resolve` on the cco side, and the result does not
/// depend on whether the directory exists — an account id must not change when
/// its directory is temporarily gone.
pub fn canonical_config_dir(home: &Path, raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let expanded: PathBuf = if trimmed == "~" {
        home.to_path_buf()
    } else if let Some(rest) = trimmed.strip_prefix("~/") {
        home.join(rest)
    } else {
        let p = Path::new(trimmed);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            home.join(p)
        }
    };

    let mut out = PathBuf::new();
    for component in expanded.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out.to_string_lossy().into_owned()
}

fn non_empty(s: Option<&str>) -> Option<String> {
    s.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// follow mode
// ---------------------------------------------------------------------------

/// Live handle for `registryMode = "follow"`. Dropping it stops the watch.
pub struct Watcher {
    _debouncer: notify_debouncer_full::Debouncer<
        notify::RecommendedWatcher,
        notify_debouncer_full::RecommendedCache,
    >,
    watched_dir: PathBuf,
}

impl Watcher {
    /// The directory being watched (the registry's parent).
    pub fn watched_dir(&self) -> &Path {
        &self.watched_dir
    }
}

impl std::fmt::Debug for Watcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watcher")
            .field("watched_dir", &self.watched_dir)
            .finish()
    }
}

/// Watch `registry.json` and call `on_change` after a 500 ms quiet period.
///
/// The **parent directory** is watched non-recursively rather than the file:
/// cco replaces the registry by writing a temp file and renaming it over the
/// old one, which detaches any watch bound to the original inode. Events for
/// other files in that directory (`tasks.jsonl`, `usage.json`, …) are filtered
/// out here.
///
/// Purely observational — no handle returned by `notify` is ever used to write.
pub fn watch_cco_registry(
    paths: &AppPaths,
    on_change: impl Fn() + Send + 'static,
) -> Result<Watcher> {
    let registry = paths.cco_registry();
    let dir = registry
        .parent()
        .ok_or_else(|| GildeError::config("cco registry path has no parent directory"))?
        .to_path_buf();

    if !dir.is_dir() {
        return Err(GildeError::not_found(dir.display().to_string()));
    }

    // Match on the file name, not the full path: the watch is non-recursive on
    // one directory, and macOS FSEvents reports paths through the resolved
    // `/private/...` prefix, which never compares equal to the `/var/...` form
    // the caller handed us.
    let target = registry
        .file_name()
        .ok_or_else(|| GildeError::config("cco registry path has no file name"))?
        .to_os_string();

    let mut debouncer = new_debouncer(
        WATCH_DEBOUNCE,
        None,
        move |result: DebounceEventResult| match result {
            Ok(events) => {
                let touched = events.iter().any(|event| {
                    event
                        .paths
                        .iter()
                        .any(|p| p.file_name().is_some_and(|name| name == target))
                });
                if touched {
                    on_change();
                }
            }
            Err(errors) => {
                for error in errors {
                    tracing::warn!(error = %error, "cco registry watch error");
                }
            }
        },
    )
    .map_err(|e| GildeError::config(format!("cannot watch the cco registry: {e}")))?;

    debouncer
        .watch(&dir, RecursiveMode::NonRecursive)
        .map_err(|e| GildeError::config(format!("cannot watch {}: {e}", dir.display())))?;

    Ok(Watcher {
        _debouncer: debouncer,
        watched_dir: dir,
    })
}

// ---------------------------------------------------------------------------
// UUID v5
// ---------------------------------------------------------------------------

/// RFC 4122 name-based UUID (version 5, SHA-1), from the `uuid` crate.
///
/// This used to be a hand-rolled SHA-1 plus the RFC bit-twiddling, because the
/// frozen dependency set did not enable the `v5` feature. INT-A added the
/// feature; the tests below still pin the published RFC 4122 vector and the
/// account ids this machine already has, so the swap is proven equivalent.
fn uuid_v5(namespace: Uuid, name: &str) -> Uuid {
    Uuid::new_v5(&namespace, name.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Verbatim copy of this machine's registry (DESIGN §10-5), used so the
    /// expectations hold even on a machine that has never run cco.
    const FIXTURE: &str = r#"{
      "workers": [
        { "name": "work-claude", "configDir": "~/.claude-work-claude", "model": "claude-opus-4-8" },
        { "name": "work-gpt",    "configDir": "~/.claude-work-gpt",    "model": "claude-opus-4-8" },
        { "name": "personal",     "configDir": "~/.claude-personal",     "model": "claude-opus-4-8" }
      ],
      "permissionMode": "acceptEdits",
      "claudePath": "/Users/tester/.nvm/versions/node/v20.19.2/bin/claude",
      "cooldownMinutes": 30,
      "workerTimeoutMinutes": 45,
      "frontierWorkerDispatch": "block",
      "overageWorkerDispatch": "block",
      "meshMode": "off",
      "meshWorkerIdleTimeoutMin": 15
    }"#;

    /// An isolated home with a registry file in it. Never the user's real one.
    fn home_with_registry(contents: &str) -> (tempfile::TempDir, AppPaths) {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = AppPaths::for_home(dir.path());
        let registry = paths.cco_registry();
        std::fs::create_dir_all(registry.parent().expect("parent")).expect("mkdir");
        std::fs::write(&registry, contents).expect("write fixture");
        (dir, paths)
    }

    fn sha256_of(path: &Path) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(std::fs::read(path).expect("read"));
        hex::encode(hasher.finalize())
    }

    #[test]
    fn uuid_v5_matches_rfc_4122() {
        // The canonical example: v5 of the DNS namespace over "python.org".
        let dns = Uuid::from_u128(0x6ba7b810_9dad_11d1_80b4_00c04fd430c8);
        assert_eq!(
            uuid_v5(dns, "python.org").to_string(),
            "886313e1-3b8a-5372-9b90-0c9aee199e5d"
        );
        let id = uuid_v5(GILDE_ACCOUNT_NAMESPACE, "cco:/Users/x/.claude-a");
        assert_eq!(id.get_version_num(), 5);
        assert_eq!(id.get_variant(), uuid::Variant::RFC4122);
    }

    #[test]
    fn account_ids_are_stable_and_keyed_on_the_directory_not_the_name() {
        let a = account_id_for_config_dir("/Users/x/.claude-personal");
        let b = account_id_for_config_dir("/Users/x/.claude-personal");
        let c = account_id_for_config_dir("/Users/x/.claude-other");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn config_dirs_normalize_the_way_path_resolve_does() {
        let home = Path::new("/Users/tester");
        assert_eq!(
            canonical_config_dir(home, "~/.claude-a"),
            "/Users/tester/.claude-a"
        );
        assert_eq!(
            canonical_config_dir(home, "/Users/tester/.claude-a/"),
            "/Users/tester/.claude-a"
        );
        assert_eq!(
            canonical_config_dir(home, "/Users/tester/./x/../.claude-a"),
            "/Users/tester/.claude-a"
        );
        assert_eq!(
            canonical_config_dir(home, ".claude-a"),
            "/Users/tester/.claude-a"
        );
        assert_eq!(canonical_config_dir(home, "   "), "");
        assert_eq!(canonical_config_dir(home, "~"), "/Users/tester");
    }

    #[test]
    fn the_fixture_registry_yields_three_cco_accounts_plus_ambient() {
        let (_dir, paths) = home_with_registry(FIXTURE);
        let report = import_cco_registry(&paths);

        assert_eq!(report.imported, 3);
        assert_eq!(report.skipped, 0);
        assert_eq!(report.accounts.len(), 4);

        let labels: Vec<&str> = report.accounts.iter().map(|a| a.label.as_str()).collect();
        assert_eq!(
            labels,
            vec!["work-claude", "work-gpt", "personal", "ambient"]
        );

        for account in report.accounts.iter().take(3) {
            assert_eq!(account.vendor, "claude");
            assert_eq!(account.source, AccountSource::Cco);
            assert_eq!(account.model_hint.as_deref(), Some("claude-opus-4-8"));
            let dir = account.config_dir.as_deref().expect("configDir");
            assert!(
                dir.starts_with(paths.home_dir().to_str().expect("utf-8")),
                "{dir} should sit under the isolated home"
            );
            assert!(dir.contains("/.claude-"));
            assert_eq!(account.id, account_id_for_config_dir(dir));
            assert!(!account.logged_in, "the fixture dirs hold no credentials");
        }

        let ambient = report.accounts.last().expect("ambient");
        assert_eq!(ambient.source, AccountSource::Ambient);
        assert_eq!(ambient.config_dir, None);
        assert!(ambient.is_default);

        let scan = scan_cco_registry(&paths);
        assert!(scan.found);
        assert_eq!(scan.settings.permission_mode.as_deref(), Some("acceptEdits"));
        assert_eq!(scan.settings.worker_timeout_minutes, Some(45));
        assert_eq!(scan.settings.cooldown_minutes, Some(30));
        assert_eq!(scan.settings.frontier_worker_dispatch.as_deref(), Some("block"));
        assert_eq!(scan.settings.overage_worker_dispatch.as_deref(), Some("block"));
        assert!(scan
            .settings
            .claude_path
            .as_deref()
            .is_some_and(|p| p.ends_with("/bin/claude")));
    }

    /// A registry in the shape the live cco 2.x writes today: absolute and
    /// `~` configDirs side by side, short worker names, a bare model alias, and
    /// mesh/dispatch knobs Gilde does not read.
    const LIVE_SHAPED_FIXTURE: &str = r#"{
      "workers": [
        { "name": "c",  "configDir": "/Users/tester/.claude-c",  "model": "opus" },
        { "name": "c2", "configDir": "/Users/tester/.claude-c2", "model": "opus" },
        { "name": "g",  "configDir": "~/.claude-g",              "model": "opus", "preferred": true },
        { "name": "t",  "configDir": "~/.claude-t/",             "model": "opus" },
        { "name": "mg", "configDir": "~/./.claude-mg",           "model": "opus" }
      ],
      "permissionMode": "bypassPermissions",
      "claudePath": "/Users/tester/.local/bin/claude",
      "cooldownMinutes": 30,
      "workerTimeoutMinutes": 45,
      "frontierWorkerDispatch": "allow",
      "overageWorkerDispatch": "block",
      "meshMode": "on",
      "meshWorkerIdleTimeoutMin": 15,
      "dispatchHost": { "socket": "/tmp/cco.sock" }
    }"#;

    /// What a registry must import as, computed **independently** of the
    /// importer: every worker with a non-blank name and configDir, the first
    /// occurrence of each directory winning, in file order, then `ambient`.
    fn expected_labels(contents: &str, home: &Path) -> Vec<String> {
        let value: serde_json::Value = serde_json::from_str(contents).expect("registry is JSON");
        let mut seen = std::collections::HashSet::new();
        let mut labels = Vec::new();
        for worker in value["workers"].as_array().into_iter().flatten() {
            let name = worker["name"].as_str().unwrap_or("");
            let dir = canonical_config_dir(home, worker["configDir"].as_str().unwrap_or(""));
            if name.trim().is_empty() || dir.is_empty() || !seen.insert(dir) {
                continue;
            }
            labels.push(name.to_string());
        }
        labels.push("ambient".to_string());
        labels
    }

    fn assert_imports_every_worker(contents: &str, origin: &str) {
        let (_dir, paths) = home_with_registry(contents);
        let report = import_cco_registry(&paths);
        let expected = expected_labels(contents, paths.home_dir());

        assert!(scan_cco_registry(&paths).found, "{origin}: registry must parse");
        assert_eq!(
            report.imported as usize,
            expected.len() - 1,
            "{origin}: accounts: {:?}",
            report.accounts
        );
        assert_eq!(report.accounts.len(), expected.len());
        let labels: Vec<&str> = report.accounts.iter().map(|a| a.label.as_str()).collect();
        assert_eq!(labels, expected, "{origin}: labels in file order, ambient last");
        for account in report.accounts.iter().filter(|a| a.is_cco()) {
            let dir = account.config_dir.as_deref().expect("configDir");
            assert!(Path::new(dir).is_absolute(), "{origin}: {dir} must be absolute");
            assert!(!dir.ends_with('/'), "{origin}: {dir} must be normalized");
            assert_eq!(account.id, account_id_for_config_dir(dir));
        }
        let ambient = report.accounts.last().expect("ambient");
        assert_eq!(ambient.source, AccountSource::Ambient);
        assert!(ambient.is_default);
    }

    /// A registry of the live shape imports every worker it lists (DESIGN §10-5).
    ///
    /// This used to read `$HOME/.claude-code-orchestrator/registry.json` and
    /// assert the three worker names one development machine happened to have,
    /// so it failed on every other machine (and read the user's real cco state
    /// from a unit test). The expectations are now derived from the file itself,
    /// and the inputs are checked-in fixtures. To check a real registry too,
    /// point `GILDE_CCO_REGISTRY_SAMPLE` at it; it is only ever read.
    #[test]
    fn a_live_shaped_registry_yields_every_listed_worker() {
        assert_imports_every_worker(LIVE_SHAPED_FIXTURE, "live-shaped fixture");
        assert_imports_every_worker(FIXTURE, "design fixture");

        match std::env::var_os("GILDE_CCO_REGISTRY_SAMPLE") {
            Some(sample) => {
                let contents = std::fs::read_to_string(&sample).unwrap_or_else(|e| {
                    panic!("GILDE_CCO_REGISTRY_SAMPLE={sample:?} is unreadable: {e}")
                });
                assert_imports_every_worker(&contents, "GILDE_CCO_REGISTRY_SAMPLE");
            }
            None => println!(
                "note: GILDE_CCO_REGISTRY_SAMPLE is unset; only the checked-in fixtures were imported"
            ),
        }
    }

    #[test]
    fn a_missing_or_damaged_registry_is_empty_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = AppPaths::for_home(dir.path());

        let report = import_cco_registry(&paths);
        assert_eq!(report.imported, 0);
        assert_eq!(report.accounts.len(), 1, "only the ambient slot");
        assert!(!scan_cco_registry(&paths).found);

        let (_dir2, paths2) = home_with_registry("{ not json at all");
        let report2 = import_cco_registry(&paths2);
        assert_eq!(report2.imported, 0);
        assert_eq!(report2.accounts.len(), 1);
        assert!(!scan_cco_registry(&paths2).found);
    }

    #[test]
    fn unknown_fields_and_blank_workers_are_ignored() {
        let (_dir, paths) = home_with_registry(
            r#"{
              "workers": [
                { "name": "a", "configDir": "~/.claude-a", "preferred": true, "future": 1 },
                { "name": "b", "configDir": "  " },
                { "name": "dup", "configDir": "~/.claude-a" }
              ],
              "somethingBrandNew": { "nested": true }
            }"#,
        );

        let report = import_cco_registry(&paths);
        assert_eq!(report.imported, 1);
        assert_eq!(report.skipped, 2);
        assert_eq!(report.accounts.len(), 2);
    }

    #[test]
    fn dir_has_login_follows_the_cco_heuristic() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(!dir_has_login(dir.path()));

        std::fs::write(dir.path().join(".claude.json"), b"{}").expect("write");
        assert!(dir_has_login(dir.path()));

        let other = tempfile::tempdir().expect("tempdir");
        std::fs::write(other.path().join(".credentials.json"), b"{}").expect("write");
        assert!(dir_has_login(other.path()));

        assert!(!dir_has_login(Path::new("/nonexistent/definitely")));
    }

    #[test]
    fn ambient_login_is_probed_against_the_home_claude_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(!ambient_account(dir.path()).logged_in);

        std::fs::create_dir_all(dir.path().join(".claude")).expect("mkdir");
        std::fs::write(dir.path().join(".claude/.credentials.json"), b"{}").expect("write");
        assert!(ambient_account(dir.path()).logged_in);
    }

    /// DESIGN §10-5 / T03 acceptance 8: import, then follow the file for a full
    /// minute, and assert its mtime, size and content hash never move.
    ///
    /// `GILDE_CCO_WATCH_SECS` shortens the window for a quick local loop; the
    /// default is the specified 60 seconds.
    #[test]
    fn watch_is_read_only_over_a_long_window() {
        let (_dir, paths) = home_with_registry(FIXTURE);
        let registry = paths.cco_registry();

        assert_eq!(import_cco_registry(&paths).imported, 3);

        let before = std::fs::metadata(&registry).expect("stat");
        let before_mtime = before.modified().expect("mtime");
        let before_len = before.len();
        let before_hash = sha256_of(&registry);

        // Drain the fixture's own creation before the watch under test starts.
        // macOS FSEvents delivers events that happened shortly *before* a stream
        // was created (the kernel has not handed them to fseventsd yet), so a
        // watch started milliseconds after `home_with_registry` reports the
        // fixture's Create(File) as if it were a change. A throwaway watch
        // absorbs that replay; once it has fired (or a short window passed on a
        // backend that never replays, e.g. inotify) the history is flushed and
        // the quiet window below measures only what happens from here on.
        {
            let replayed = Arc::new(AtomicUsize::new(0));
            let seen = replayed.clone();
            let drain = watch_cco_registry(&paths, move || {
                seen.fetch_add(1, Ordering::SeqCst);
            })
            .expect("drain watch");
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            while replayed.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            drop(drain);
        }

        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let watcher = watch_cco_registry(&paths, move || {
            counter.fetch_add(1, Ordering::SeqCst);
        })
        .expect("watch");
        assert_eq!(watcher.watched_dir(), registry.parent().expect("parent"));

        let seconds: u64 = std::env::var("GILDE_CCO_WATCH_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60);
        std::thread::sleep(Duration::from_secs(seconds));

        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "nothing changed the file, so nothing should have fired"
        );

        let after = std::fs::metadata(&registry).expect("stat");
        assert_eq!(after.modified().expect("mtime"), before_mtime, "mtime moved");
        assert_eq!(after.len(), before_len, "size changed");
        assert_eq!(sha256_of(&registry), before_hash, "contents changed");

        // …and the watch really is live: an external edit reaches the callback.
        std::fs::write(&registry, FIXTURE.replace("personal", "renamed")).expect("write");
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while hits.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            hits.load(Ordering::SeqCst) >= 1,
            "follow mode must notice an external write"
        );

        // The re-read picks the change up, and still writes nothing back.
        let refreshed = import_cco_registry(&paths);
        assert!(refreshed.accounts.iter().any(|a| a.label == "renamed"));
        let settled = sha256_of(&registry);
        std::thread::sleep(Duration::from_secs(2));
        assert_eq!(sha256_of(&registry), settled);
    }

    /// Static guard for "not one byte": this module's own source must not
    /// contain a filesystem mutation call. Tests are excluded — the fixture
    /// writing above happens in isolated temp homes.
    #[test]
    fn this_module_contains_no_write_calls() {
        let source = include_str!("cco_import.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("source before the test module");

        for forbidden in [
            "fs::write",
            "fs::remove",
            "fs::rename",
            "fs::copy",
            "fs::create_dir",
            "fs::set_permissions",
            "File::create",
            "OpenOptions",
            "write_all",
            "append(true)",
        ] {
            assert!(
                !production.contains(forbidden),
                "cco_import.rs must never mutate the filesystem, found `{forbidden}`"
            );
        }
    }
}
