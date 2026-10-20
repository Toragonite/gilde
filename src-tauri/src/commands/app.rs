//! `app` commands (DESIGN §3).
//!
//! `app_bootstrap` is the first call the frontend makes; it hands back the
//! resolved paths and the identity of this run. T01 filled in the path half
//! with placeholder identities; T29 replaced them with the real ones — the
//! `app_instance` row and `boot_id` from the supervisor, the schema version
//! from `state.db`, the locale from `config.json` and the count of rows the
//! boot sweep left for the user to decide on.
//!
//! `app_diagnostics` (T28) is the other half: one call that answers every
//! question a "it works in dev, not from Finder" report needs, from the resolved
//! `PATH` down to which sysctl actually answered for memory pressure.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use serde::Serialize;
use tauri::{Manager as _, State};
use tauri_plugin_opener::OpenerExt as _;

use crate::agents::AgentBinary;
use crate::error::{GildeError, GildeErrorPayload, CmdResult};
use crate::model::graph::SidecarState as SidecarLifecycle;
use crate::paths::AppPaths;

/// Managed state alias so later tasks can ask for the same thing.
pub type PathsState = Arc<AppPaths>;

/// Locale the UI starts in until `config.json` says otherwise: `system`, which
/// the frontend resolves from the OS language (ko* → ko, else en).
pub const DEFAULT_LOCALE: &str = "system";

/// Every path `AppPaths` resolves, as the frontend sees it (DESIGN §10-1).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PathsInfo {
    pub app_data_dir: PathBuf,
    pub config_file: PathBuf,
    pub state_db: PathBuf,
    pub signatures_file: PathBuf,
    pub path_cache: PathBuf,
    pub instance_lock: PathBuf,
    pub logs_dir: PathBuf,
    pub scrollback_dir: PathBuf,
    pub runs_dir: PathBuf,
    pub worktrees_root: PathBuf,
    pub archive_root: PathBuf,
    pub dispatch_root: PathBuf,
    pub aliases_file: PathBuf,
    pub cco_registry: PathBuf,
}

impl From<&AppPaths> for PathsInfo {
    fn from(p: &AppPaths) -> Self {
        PathsInfo {
            app_data_dir: p.app_data_dir().to_path_buf(),
            config_file: p.config_file(),
            state_db: p.state_db(),
            signatures_file: p.signatures_file(),
            path_cache: p.path_cache(),
            instance_lock: p.instance_lock(),
            logs_dir: p.logs_dir().to_path_buf(),
            scrollback_dir: p.scrollback_dir().to_path_buf(),
            runs_dir: p.runs_dir().to_path_buf(),
            worktrees_root: p.worktrees_root().to_path_buf(),
            archive_root: p.archive_root().to_path_buf(),
            dispatch_root: p.dispatch_root().to_path_buf(),
            aliases_file: p.aliases_file(),
            cco_registry: p.cco_registry(),
        }
    }
}

/// What the frontend needs before it can render anything (DESIGN §3, `app`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BootstrapInfo {
    /// `app_instance.id` for this run.
    pub app_instance_id: String,
    /// Machine boot session — rows from an earlier boot carry meaningless PIDs.
    pub boot_id: String,
    pub paths: PathsInfo,
    /// `state.db` schema version. `0` until the store is wired (T02/T29).
    pub schema_version: u32,
    pub locale: String,
    /// Rows the startup sweep left for the user to decide on (T11/T29).
    pub recovery_pending: u32,
}

/// Fallback identities for this process (T29).
///
/// Only reachable when `boot()` never managed a supervisor — which in a shipped
/// build means the boot failed before step 6 and there is no window to call
/// this command from. It survives for the tests and for the frontend's sake:
/// two stable, unique-per-run strings mean nothing downstream has to
/// special-case an empty id. The `provisional:` prefix is what makes the
/// difference visible in a bug report.
struct ProvisionalIds {
    app_instance_id: String,
    boot_id: String,
}

fn provisional_ids() -> &'static ProvisionalIds {
    static IDS: OnceLock<ProvisionalIds> = OnceLock::new();
    IDS.get_or_init(|| ProvisionalIds {
        app_instance_id: uuid::Uuid::now_v7().to_string(),
        boot_id: format!("provisional:{}", uuid::Uuid::now_v7()),
    })
}

/// Resolved paths + identity of this run (DESIGN §3, `app`).
///
/// Pure lookup — every value is already in memory or one `PRAGMA` away — so the
/// settings sheet re-calling it to find `signaturesFile` costs nothing.
///
/// The `recoveryPending` count comes from the boot sweep's own pending set, not
/// from a query: rows whose process has since died on its own are dropped from
/// it, so the number matches what the Recovered Sessions sheet will actually
/// show (PINNED-VERSIONS §17-2).
#[tauri::command]
pub async fn app_bootstrap(
    app: tauri::AppHandle,
    paths: State<'_, PathsState>,
) -> CmdResult<BootstrapInfo> {
    let supervisor = app.try_state::<super::monitor::SupervisorState>();
    let (app_instance_id, boot_id) = match supervisor.as_ref() {
        Some(sup) => (
            sup.app_instance_id().to_string(),
            sup.current_boot_id().to_string(),
        ),
        None => {
            let ids = provisional_ids();
            (ids.app_instance_id.clone(), ids.boot_id.clone())
        }
    };

    let schema_version = app
        .try_state::<super::accounts::DbState>()
        .and_then(|db| db.schema_version().ok())
        .unwrap_or(0);

    let locale = app
        .try_state::<super::accounts::ConfigState>()
        .map(|config| config.get().locale.as_str().to_string())
        .unwrap_or_else(|| DEFAULT_LOCALE.to_string());

    Ok(BootstrapInfo {
        app_instance_id,
        boot_id,
        paths: PathsInfo::from(paths.inner().as_ref()),
        schema_version,
        locale,
        recovery_pending: crate::supervise::reaper::pending().len() as u32,
    })
}

/// Show `path` in Finder (the enclosing directory, with the item selected).
#[tauri::command]
pub async fn app_reveal(app: tauri::AppHandle, path: String) -> CmdResult<()> {
    let target = Path::new(&path);

    if path.trim().is_empty() {
        return Err(GildeError::refused("emptyPath").into());
    }
    if !target.exists() {
        return Err(GildeError::not_found(path.clone()).into());
    }

    app.opener()
        .reveal_item_in_dir(target)
        .map_err(|e| GildeErrorPayload::from(GildeError::Internal(e.to_string())))
}

// ---------------------------------------------------------------------------
// diagnostics (T28)
// ---------------------------------------------------------------------------

/// The sidecar half of [`Diagnostics`] (DESIGN §10-6).
///
/// `node_source` is the field this whole struct exists for. "The bundle works
/// from the terminal but not from Finder" is, on this machine, always one of two
/// things: a `PATH` that no longer contains nvm, or a Node runtime that came
/// from somewhere other than the one we vendored. Both are visible here.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SidecarDiagnostics {
    pub node_path: Option<String>,
    /// `"vendored"` (the pinned `gilde-node` inside the bundle), `"configured"`
    /// (`config.sidecar.nodePath`) or `"system"` (a `node` off the resolved
    /// `PATH` — a fallback, reported rather than hidden).
    pub node_source: &'static str,
    pub script_path: Option<String>,
    pub state: SidecarLifecycle,
    pub generation: i64,
    pub restarts: u32,
    pub last_error: Option<String>,
    /// Why the runtime resolution landed where it did, when it was not the
    /// happy path. Empty on a vendored runtime.
    pub warnings: Vec<String>,
}

/// The `state.db` half of [`Diagnostics`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DbDiagnostics {
    pub path: String,
    pub schema_version: u32,
    pub integrity_ok: bool,
}

/// Everything a bug report needs, in one call (DESIGN §10-6).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Diagnostics {
    pub resolved_path: Vec<PathBuf>,
    pub path_source: &'static str,
    pub path_fingerprint: String,
    pub agents: Vec<AgentBinary>,
    pub sidecar: SidecarDiagnostics,
    pub db: DbDiagnostics,
    pub memory_pressure_source: &'static str,
    pub app_instance_id: String,
    pub boot_id: String,
    pub app_data_dir: PathBuf,
    pub worktree_root: PathBuf,
    pub dispatch_root: PathBuf,
}

/// One click, everything diagnosable (DESIGN §10-6).
///
/// Deliberately cheap and non-destructive: the `PATH` and the agent roster come
/// from the memoised reports (`refresh = false`), so opening the sheet never
/// re-runs a login shell or re-probes five binaries, and the integrity check is
/// `quick_check(1)` — enough to answer "is the file readable and structurally
/// sane", not the full page-by-page audit `Db::open` already ran at boot.
///
/// Nothing here may fail the call. A watchdog that is not managed yet reports
/// `"unavailable"`, which is a legal `MemoryPressureSource`; a database that
/// cannot answer reports `integrityOk: false` rather than an error, because a
/// diagnostics command that errors is the one command that must not.
#[tauri::command]
pub async fn app_diagnostics(
    app: tauri::AppHandle,
    paths: State<'_, PathsState>,
    agents: State<'_, super::agents::AgentsState>,
    sidecar: State<'_, super::mesh::SidecarState>,
    db: State<'_, super::accounts::DbState>,
    supervisor: State<'_, super::monitor::SupervisorState>,
) -> CmdResult<Diagnostics> {
    let registry = agents.inner().clone();
    // `path_report` may run a login shell on a cold cache; it is memoised, but
    // "may block" is enough to keep it off the async runtime's thread.
    let path = tokio::task::spawn_blocking(move || registry.path_report(false))
        .await
        .map_err(|e| GildeError::internal(format!("path probe panicked: {e}")))?;

    // A failed probe is a diagnostic, not an error: report the rows we have.
    let agent_rows = agents.detect(false).await.unwrap_or_default();

    let status = sidecar.status();
    let runtime = sidecar.runtime();

    let db_path = db
        .path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<in-memory>".to_string());
    let schema_version = db.schema_version().unwrap_or(0);
    let integrity_ok = db
        .read(|conn| {
            let verdict: String = conn.query_row("PRAGMA quick_check(1)", [], |row| row.get(0))?;
            Ok(verdict.eq_ignore_ascii_case("ok"))
        })
        .await
        .unwrap_or(false);

    let memory_pressure_source = app
        .try_state::<super::monitor::WatchdogState>()
        .map(|w| super::monitor::pressure_source(w.inner()).1)
        .unwrap_or("unavailable");

    Ok(Diagnostics {
        resolved_path: path.entries.clone(),
        path_source: path.source.as_str(),
        path_fingerprint: path.fingerprint.clone(),
        agents: agent_rows,
        sidecar: SidecarDiagnostics {
            node_path: status.node_path,
            node_source: runtime.node_source.as_str(),
            script_path: status.script_path,
            state: status.state,
            generation: status.generation,
            restarts: status.restarts,
            last_error: status.last_error,
            warnings: runtime.warnings.clone(),
        },
        db: DbDiagnostics {
            path: db_path,
            schema_version,
            integrity_ok,
        },
        memory_pressure_source,
        app_instance_id: supervisor.app_instance_id().to_string(),
        boot_id: supervisor.current_boot_id().to_string(),
        app_data_dir: paths.app_data_dir().to_path_buf(),
        worktree_root: paths.worktrees_root().to_path_buf(),
        dispatch_root: paths.dispatch_root().to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_info_serializes_every_design_10_1_path_in_camel_case() {
        let paths = AppPaths::for_home(Path::new("/home/tester"));
        let value = serde_json::to_value(PathsInfo::from(&paths)).expect("serialize");
        let obj = value.as_object().expect("object");

        for key in [
            "appDataDir",
            "configFile",
            "stateDb",
            "signaturesFile",
            "pathCache",
            "instanceLock",
            "logsDir",
            "scrollbackDir",
            "runsDir",
            "worktreesRoot",
            "archiveRoot",
            "dispatchRoot",
            "aliasesFile",
            "ccoRegistry",
        ] {
            let v = obj.get(key).unwrap_or_else(|| panic!("missing {key}"));
            assert!(
                v.as_str().is_some_and(|s| s.starts_with('/')),
                "{key} should be an absolute path, got {v}"
            );
        }
        assert_eq!(obj.len(), 14, "unexpected extra fields: {obj:?}");
    }

    #[test]
    fn bootstrap_info_uses_the_documented_wire_names() {
        let paths = AppPaths::for_home(Path::new("/home/tester"));
        let info = BootstrapInfo {
            app_instance_id: "a".into(),
            boot_id: "b".into(),
            paths: PathsInfo::from(&paths),
            schema_version: 0,
            locale: DEFAULT_LOCALE.into(),
            recovery_pending: 0,
        };
        let value = serde_json::to_value(&info).expect("serialize");

        assert_eq!(value["appInstanceId"], "a");
        assert_eq!(value["bootId"], "b");
        assert_eq!(value["schemaVersion"], 0);
        assert_eq!(value["locale"], "system");
        assert_eq!(value["recoveryPending"], 0);
        assert!(value["paths"].is_object());
    }

    #[test]
    fn provisional_ids_are_stable_within_a_process() {
        let a = provisional_ids();
        let b = provisional_ids();
        assert_eq!(a.app_instance_id, b.app_instance_id);
        assert!(!a.app_instance_id.is_empty());
        assert!(a.boot_id.starts_with("provisional:"));
    }

    // -- diagnostics (T28) ---------------------------------------------------

    fn sample_diagnostics() -> Diagnostics {
        let paths = AppPaths::for_home(Path::new("/home/tester"));
        Diagnostics {
            resolved_path: vec![PathBuf::from("/usr/bin"), PathBuf::from("/bin")],
            path_source: crate::agents::path::PathSource::LoginShell.as_str(),
            path_fingerprint: "fp".into(),
            agents: vec![crate::agents::AgentBinary::absent(
                crate::agents::AgentKind::Claude,
                None,
            )],
            sidecar: SidecarDiagnostics {
                node_path: Some("/Applications/x.app/Contents/MacOS/gilde-node".into()),
                node_source: crate::sidecar::NodeSource::Vendored.as_str(),
                script_path: Some("/Applications/x.app/Contents/Resources/sidecar.cjs".into()),
                state: SidecarLifecycle::Ready,
                generation: 3,
                restarts: 1,
                last_error: None,
                warnings: vec![],
            },
            db: DbDiagnostics {
                path: paths.state_db().display().to_string(),
                schema_version: 7,
                integrity_ok: true,
            },
            memory_pressure_source: "kern.memorystatus_vm_pressure_level",
            app_instance_id: "ai".into(),
            boot_id: "bi".into(),
            app_data_dir: paths.app_data_dir().to_path_buf(),
            worktree_root: paths.worktrees_root().to_path_buf(),
            dispatch_root: paths.dispatch_root().to_path_buf(),
        }
    }

    /// DESIGN §10-6 lists the fields by name; `src/lib/ipc.ts::Diagnostics`
    /// declares them for the frontend. A rename on either side is a silent
    /// `undefined` in the settings sheet, so the wire names are asserted.
    #[test]
    fn diagnostics_carries_every_design_10_6_field() {
        let value = serde_json::to_value(sample_diagnostics()).expect("serialize");
        let obj = value.as_object().expect("object");

        for key in [
            "resolvedPath",
            "pathSource",
            "pathFingerprint",
            "agents",
            "sidecar",
            "db",
            "memoryPressureSource",
            "appInstanceId",
            "bootId",
            "appDataDir",
            "worktreeRoot",
            "dispatchRoot",
        ] {
            assert!(obj.contains_key(key), "missing {key}");
        }
        assert_eq!(obj.len(), 12, "unexpected extra fields: {obj:?}");

        let sidecar = value["sidecar"].as_object().expect("sidecar object");
        for key in [
            "nodePath",
            "nodeSource",
            "scriptPath",
            "state",
            "generation",
            "restarts",
            "lastError",
        ] {
            assert!(sidecar.contains_key(key), "sidecar missing {key}");
        }
        assert_eq!(value["sidecar"]["nodeSource"], "vendored");
        assert_eq!(value["sidecar"]["state"], "ready");

        let db = value["db"].as_object().expect("db object");
        assert_eq!(db.len(), 3);
        assert_eq!(value["db"]["schemaVersion"], 7);
        assert_eq!(value["db"]["integrityOk"], true);

        assert_eq!(value["resolvedPath"][0], "/usr/bin");
        assert_eq!(value["pathSource"], "loginShell");
    }

    /// The three `nodeSource` strings are the contract with
    /// `ipc.ts::Diagnostics['sidecar']['nodeSource']`; "system" in particular is
    /// what a build made without the vendored `gilde-node` must report
    /// (M1-PLAN T28.1).
    #[test]
    fn node_source_reports_the_fallback_rather_than_hiding_it() {
        assert_eq!(crate::sidecar::NodeSource::Vendored.as_str(), "vendored");
        assert_eq!(crate::sidecar::NodeSource::Configured.as_str(), "configured");
        assert_eq!(crate::sidecar::NodeSource::System.as_str(), "system");

        let mut diag = sample_diagnostics();
        diag.sidecar.node_source = crate::sidecar::NodeSource::System.as_str();
        diag.sidecar.warnings =
            vec!["the vendored gilde-node binary is missing; falling back to the system node".into()];
        let value = serde_json::to_value(diag).expect("serialize");
        assert_eq!(value["sidecar"]["nodeSource"], "system");
        assert_eq!(value["sidecar"]["warnings"].as_array().map(Vec::len), Some(1));
    }
}
