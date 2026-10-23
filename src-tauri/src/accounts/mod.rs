//! Accounts (contract §3.4.3, §4.2.7, §5.1; C6, C7, C8, F-35).
//!
//! Two layers live here:
//!
//!   * **The Gilde 2 registry** ([`AccountsService`]): per vendor the default
//!     login (`~/.claude` / `~/.codex` — run with the account variable
//!     **unset**, C8 — and the cursor / agy machine logins), the config dirs the
//!     user added (`User`), the orchestrator's worker accounts read from its
//!     registry (`Orchestrator`), and every `~/.claude-*` sibling holding a
//!     `.claude.json` (`Discovered`: read-only and not offered for New session
//!     until the user confirms it, PM-44). Everything it learns about a config
//!     dir is by `stat` — no file inside one is ever opened (C6). Quota follows
//!     the `quota_refresh` rate rules and the F-35 background rule; sign-in
//!     comes from the sidecar's `probe.authStatus` (never a token).
//!   * **The pre-Gilde-2 slots** ([`AccountSlot`], [`list`], [`import_cco`]):
//!     kept because the PTY manager and the registry's env composition still take
//!     them until their owners move to [`AccountsService::env_for`]. The dead
//!     `follow` mode of the orchestrator registry is gone (R1 §1a); the registry
//!     is read, read-only, when the account list is computed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::config::cco_import::{self, CcoSettings};
use crate::config::ConfigStore;
use crate::contract::account::{Account, AccountSource as Source, Quota, QuotaUnavailable, SignIn};
use crate::contract::common::{AbsPath, EpochMs, Vendor};
use crate::contract::error::IpcError;
use crate::contract::ids::{AccountId, ClientRequestId};
use crate::contract::ipc::{AccountAddVendor, AccountSignInReq, AccountSignInRes, Screen, ViewState};
use crate::contract::services::{SessionActorApi, SidecarApi, WorldApi};
use crate::contract::session::{Control, ControlMode};
use crate::contract::sidecar::{AccountEnv, ProbeParams};
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
/// | `cooldownMinutes`, `frontierWorkerDispatch`, `overageWorkerDispatch` | stored read-only |
///
/// (`permissionMode` / `workerTimeoutMinutes` fed the legacy `dispatch`
/// settings, which Gilde 2 no longer has, A22.3.)
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

/// The read-only cco settings last seen by an import, for the accounts pane.
pub async fn cco_read_only(db: &Db) -> Result<Option<CcoSettings>> {
    let value = db
        .read(|conn| dao::settings_kv::get(conn, CCO_READ_ONLY_KEY))
        .await?;
    Ok(value.and_then(|v| serde_json::from_value(v).ok()))
}

/// Fold the non-account half of `registry.json` into `config.json`.
fn apply_cco_settings_to_config(config: &ConfigStore, settings: &CcoSettings) -> Result<()> {
    use crate::config::model::AppConfig;

    let current = config.get();
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

    if patch.is_empty() {
        return Ok(());
    }

    let _: AppConfig = config.patch(serde_json::Value::Object(patch))?;
    Ok(())
}

// ===========================================================================
// Gilde 2 accounts (contract §3.4.3, §4.2.7, §5.1, C7, C8, F-35)
// ===========================================================================

/// `settings_kv` key: the chosen default account per vendor (`{vendor: accountId}`).
/// Not a `Settings` key (A7.8): it changes only through `account_set_default`.
pub const DEFAULTS_KEY: &str = "accounts.defaults";
/// `settings_kv` key: the user's account labels (`{accountId: label}`).
pub const LABELS_KEY: &str = "accounts.labels";
/// `settings_kv` key: the last quota per account (`{accountId: Quota}`) — no
/// secrets, so Home can show the last-known headroom without a probe (§3.4.3).
pub const QUOTA_KEY: &str = "accounts.quota";

/// `quota_refresh`: at most one probe per account per minute (§4.2.7)…
pub const QUOTA_MIN_INTERVAL_MS: EpochMs = 60_000;
/// …and agy at most every five minutes (it writes logs on each read, R2 E24).
pub const AGY_QUOTA_MIN_INTERVAL_MS: EpochMs = 5 * 60_000;
/// Background refresh (F-35): at most one per account per five minutes.
pub const BACKGROUND_QUOTA_INTERVAL_MS: EpochMs = 5 * 60_000;
/// A live Conversation session's own reading this recent is used instead of a probe.
pub const LIVE_QUOTA_FRESH_MS: EpochMs = 5 * 60_000;
/// `account_sign_in` idempotency window (§4.1 rule 4).
pub const SIGN_IN_REMEMBER_MS: EpochMs = 60_000;

/// The minimum interval between two explicit quota probes of an account.
pub const fn quota_min_interval(vendor: Vendor) -> EpochMs {
    match vendor {
        Vendor::Agy => AGY_QUOTA_MIN_INTERVAL_MS,
        _ => QUOTA_MIN_INTERVAL_MS,
    }
}

/// What `quota_refresh` does for one account (§4.2.7, PM-29).
#[derive(Debug, Clone, PartialEq)]
pub enum QuotaDecision {
    /// Start a probe.
    Probe,
    /// Answer with the cached value, marked stale (the rate limit).
    Cached,
    /// A live Conversation session already read it (`rate_limit_event` /
    /// `get_usage`): no probe process for this account.
    Live(Quota),
}

/// The `quota_refresh` rule. `live` is the reading of a live Conversation
/// session of this account (World's account quota while one runs).
pub fn decide_quota_refresh(
    now: EpochMs,
    vendor: Vendor,
    last_probe_at: Option<EpochMs>,
    live: Option<&Quota>,
) -> QuotaDecision {
    if let Some(live) = live.filter(|q| now.saturating_sub(q.fetched_at) <= LIVE_QUOTA_FRESH_MS) {
        return QuotaDecision::Live(live.clone());
    }
    match last_probe_at {
        Some(at) if now.saturating_sub(at) < quota_min_interval(vendor) => QuotaDecision::Cached,
        _ => QuotaDecision::Probe,
    }
}

/// The background refresh rule (F-35): only while the window is visible and
/// Home or Accounts & health is shown, at most once per five minutes per
/// account, and never for an account a live Conversation session already
/// reports on.
pub fn background_refresh_due(
    now: EpochMs,
    view: Option<&ViewState>,
    last_probe_at: Option<EpochMs>,
    live_conversation: bool,
) -> bool {
    let shown = view.is_some_and(|v| v.document_visible && matches!(v.screen, Screen::Home | Screen::Accounts));
    if !shown || live_conversation {
        return false;
    }
    last_probe_at.is_none_or(|at| now.saturating_sub(at) >= BACKGROUND_QUOTA_INTERVAL_MS)
}

/// A quota with no reading: the vendor reports none, or the probe failed.
pub fn quota_unavailable(now: EpochMs, why: QuotaUnavailable) -> Quota {
    Quota {
        windows: Vec::new(),
        fetched_at: now,
        stale: false,
        unavailable: Some(why),
    }
}

/// The range check at the core's side of the boundary (§3.4.3, A §13.3): the
/// sidecar already normalised each percentage; a value above 100 that still
/// arrives is absent, never clamped.
pub fn drop_out_of_range_pct(quota: &mut Quota) {
    for window in &mut quota.windows {
        if window.used_pct.is_some_and(|pct| pct > 100) {
            window.used_pct = None;
        }
    }
}

/// The folder picker `account_add` opens (the dialog plugin's Rust API in the
/// app; a fake in tests).
#[async_trait]
pub trait FolderPicker: Send + Sync {
    /// `None` = cancelled.
    async fn pick_folder(&self, start: Option<PathBuf>) -> Option<PathBuf>;
}

/// The dialog plugin's native folder picker (its Rust API, §4.2.7).
pub struct DialogFolderPicker<R: tauri::Runtime> {
    app: tauri::AppHandle<R>,
}

impl<R: tauri::Runtime> DialogFolderPicker<R> {
    pub fn new(app: tauri::AppHandle<R>) -> DialogFolderPicker<R> {
        DialogFolderPicker { app }
    }
}

#[async_trait]
impl<R: tauri::Runtime> FolderPicker for DialogFolderPicker<R> {
    async fn pick_folder(&self, start: Option<PathBuf>) -> Option<PathBuf> {
        use tauri::Manager as _;
        use tauri_plugin_dialog::DialogExt as _;
        if self.app.try_state::<tauri_plugin_dialog::Dialog<R>>().is_none() {
            tracing::warn!(target: "accounts", "the dialog plugin is not registered; no folder picker");
            return None;
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut builder = self.app.dialog().file();
        if let Some(dir) = start {
            builder = builder.set_directory(dir);
        }
        builder.pick_folder(move |picked| {
            let _ = tx.send(picked.and_then(|p| p.into_path().ok()));
        });
        rx.await.ok().flatten()
    }
}

/// Where account changes go besides World: the sidecar's `mesh.configure`.
#[async_trait]
pub trait AccountEnvSink: Send + Sync {
    async fn accounts_changed(&self, envs: Vec<AccountEnv>);
}

#[async_trait]
impl AccountEnvSink for crate::sidecar::SidecarManager {
    async fn accounts_changed(&self, envs: Vec<AccountEnv>) {
        if let Err(err) = self.set_accounts(envs).await {
            tracing::warn!(target: "accounts", code = err.code.as_str(), "could not reconfigure the sidecar's accounts");
        }
    }
}

/// One account as the service holds it: the contract entity plus what is needed
/// to run under it.
#[derive(Debug, Clone, PartialEq)]
struct Entry {
    account: Account,
    /// The `envKey` of the account variable when this is NOT the default login.
    env_key: Option<&'static str>,
    /// The persisted `account` row (user-added accounts only).
    row_id: Option<String>,
}

/// Per-account quota bookkeeping.
#[derive(Debug, Clone, Default)]
struct QuotaBook {
    last_probe_at: HashMap<AccountId, EpochMs>,
    cached: HashMap<AccountId, Quota>,
    sign_in: HashMap<AccountId, SignIn>,
}

/// The Gilde 2 account registry (§3.4.3): the default logins, the user-added
/// config dirs, the orchestrator's worker accounts and the discovered
/// `~/.claude-*` siblings — with confirm, defaults, labels, sign-in and the
/// quota rules. Everything it reads under a config dir is `stat` only: no file
/// inside one is ever opened (C6, C7).
pub struct AccountsService {
    db: Db,
    paths: AppPaths,
    sidecar: Arc<dyn SidecarApi>,
    env_sink: Option<Arc<dyn AccountEnvSink>>,
    picker: Arc<dyn FolderPicker>,
    sessions: OnceLock<Arc<dyn SessionActorApi>>,
    world: OnceLock<Arc<dyn WorldApi>>,
    view: Mutex<Option<ViewState>>,
    quota: Mutex<QuotaBook>,
    sign_ins: Mutex<HashMap<ClientRequestId, (EpochMs, AccountSignInRes)>>,
    clock: Arc<dyn Fn() -> EpochMs + Send + Sync>,
}

impl std::fmt::Debug for AccountsService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountsService").finish_non_exhaustive()
    }
}

/// A `~/.claude-*` sibling that holds a `.claude.json` (by `stat`, never opened).
fn is_discovered_sibling(dir: &Path) -> bool {
    dir.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with(".claude-") && n.len() > ".claude-".len())
        && dir.is_dir()
        && std::fs::symlink_metadata(dir.join(".claude.json")).is_ok()
}

/// A config dir a user may add (§3.4.3): a directory owned by the user that
/// holds `sessions/` or `projects/` (claude) / `sessions/` or `config.toml`
/// (codex). `stat` only.
pub fn is_valid_config_dir(vendor: Vendor, dir: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    let Ok(meta) = std::fs::metadata(dir) else { return false };
    if !meta.is_dir() || meta.uid() != nix::unistd::getuid().as_raw() {
        return false;
    }
    let has = |name: &str| std::fs::symlink_metadata(dir.join(name)).is_ok();
    match vendor {
        Vendor::Claude => has("sessions") || has("projects"),
        Vendor::Codex => has("sessions") || has("config.toml"),
        Vendor::Cursor | Vendor::Agy => false,
    }
}

/// A display label: one line, no control or bidi characters, ≤ 64 chars.
pub fn clean_label(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|c| !c.is_control() && !matches!(*c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'))
        .collect();
    let trimmed = cleaned.trim();
    trimmed.chars().take(64).collect()
}

fn label_of_dir(dir: &Path) -> String {
    let name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("account")
        .trim_start_matches('.');
    clean_label(name)
}

impl AccountsService {
    pub fn new(
        db: Db,
        paths: AppPaths,
        sidecar: Arc<dyn SidecarApi>,
        env_sink: Option<Arc<dyn AccountEnvSink>>,
        picker: Arc<dyn FolderPicker>,
    ) -> Arc<AccountsService> {
        Arc::new(AccountsService {
            db,
            paths,
            sidecar,
            env_sink,
            picker,
            sessions: OnceLock::new(),
            world: OnceLock::new(),
            view: Mutex::new(None),
            quota: Mutex::new(QuotaBook::default()),
            sign_ins: Mutex::new(HashMap::new()),
            clock: Arc::new(now_ms),
        })
    }

    /// Pin the clock (tests).
    pub fn with_clock(self: Arc<Self>, clock: Arc<dyn Fn() -> EpochMs + Send + Sync>) -> Arc<Self> {
        match Arc::try_unwrap(self) {
            Ok(mut me) => {
                me.clock = clock;
                Arc::new(me)
            }
            Err(me) => me,
        }
    }

    /// Bind the sessions manager (T10): `account_sign_in` starts its session there.
    pub fn bind_sessions(&self, sessions: Arc<dyn SessionActorApi>) -> bool {
        self.sessions.set(sessions).is_ok()
    }

    /// Bind World (T02): accounts are published to it; live sessions are read from it.
    pub fn bind_world(&self, world: Arc<dyn WorldApi>) -> bool {
        self.world.set(world).is_ok()
    }

    /// `app_view_state` (forwarded by its handler): the background refresh rule
    /// needs the screen and the document visibility (F-35).
    pub fn observe_view(&self, view: ViewState) {
        *self.view.lock() = Some(view);
    }

    fn now(&self) -> EpochMs {
        (self.clock)()
    }

    fn home(&self) -> &Path {
        self.paths.home_dir()
    }

    async fn kv<T: serde::de::DeserializeOwned + Default>(&self, key: &'static str) -> T {
        match self.db.read(move |conn| dao::settings_kv::get(conn, key)).await {
            Ok(Some(value)) => serde_json::from_value(value).unwrap_or_default(),
            _ => T::default(),
        }
    }

    async fn set_kv<T: Serialize>(&self, key: &'static str, value: &T) -> std::result::Result<(), IpcError> {
        let value = serde_json::to_value(value).map_err(|_| IpcError::internal("accounts-kv"))?;
        self.db
            .write(move |conn| dao::settings_kv::set(conn, key, &value))
            .await
            .map_err(|e| IpcError::from(&e))
    }

    /// The default login of a vendor (C8): `~/.claude`, `~/.codex`, or the
    /// machine login of cursor / agy.
    fn ambient(&self, vendor: Vendor) -> Entry {
        let config_dir = match vendor {
            Vendor::Claude => crate::sidecar::canonical_abs(&self.home().join(".claude")),
            Vendor::Codex => crate::sidecar::canonical_abs(&self.home().join(".codex")),
            Vendor::Cursor | Vendor::Agy => None,
        };
        let id = match &config_dir {
            Some(dir) => AccountId::derive(vendor, dir.as_str()),
            None => AccountId::derive_default_login(vendor),
        };
        Entry {
            account: Account {
                id,
                vendor,
                config_dir,
                label: "Default".to_string(),
                is_default: false,
                source: Source::Ambient,
                usable_for_new: true,
                sign_in: SignIn::Unknown,
                quota: None,
                live_sessions: 0,
            },
            env_key: None,
            row_id: None,
        }
    }

    fn entry_for_dir(&self, vendor: Vendor, dir: &Path, source: Source, label: String) -> Option<Entry> {
        let abs = crate::sidecar::canonical_abs(dir)?;
        Some(Entry {
            account: Account {
                id: AccountId::derive(vendor, abs.as_str()),
                vendor,
                config_dir: Some(abs),
                label,
                is_default: false,
                usable_for_new: source != Source::Discovered,
                source,
                sign_in: SignIn::Unknown,
                quota: None,
                live_sessions: 0,
            },
            env_key: crate::sidecar::account_env_key(vendor),
            row_id: None,
        })
    }

    /// Every account, in roster order: per vendor the default login, then
    /// user-added, orchestrator and discovered accounts.
    async fn entries(&self) -> Vec<Entry> {
        let mut out: Vec<Entry> = Vec::new();
        let push = |out: &mut Vec<Entry>, entry: Entry| {
            if !out.iter().any(|e| e.account.id == entry.account.id) {
                out.push(entry);
            }
        };
        for vendor in Vendor::ALL {
            push(&mut out, self.ambient(vendor));
        }
        // User-added config dirs (persisted, enabled).
        let rows = self.db.read(dao::account::list).await.unwrap_or_default();
        for row in rows.iter().filter(|r| r.source == AccountSource::Manual && r.enabled) {
            let Some(vendor) = Vendor::parse(&row.vendor) else { continue };
            let Some(dir) = row.config_dir.as_deref() else { continue };
            if let Some(mut entry) =
                self.entry_for_dir(vendor, Path::new(dir), Source::User, clean_label(&row.label))
            {
                entry.row_id = Some(row.id.clone());
                push(&mut out, entry);
            }
        }
        // The orchestrator's worker accounts (read-only, from its registry).
        let scan = cco_import::scan_cco_registry(&self.paths);
        for slot in scan.accounts.iter().filter(|a| a.is_cco()) {
            if let Some(dir) = slot.config_dir.as_deref() {
                if let Some(entry) = self.entry_for_dir(
                    Vendor::Claude,
                    Path::new(dir),
                    Source::Orchestrator,
                    clean_label(&slot.label),
                ) {
                    push(&mut out, entry);
                }
            }
        }
        // Discovered `~/.claude-*` siblings (C7): read-only until confirmed.
        for dir in discover_siblings(self.home()) {
            let label = label_of_dir(&dir);
            if let Some(entry) = self.entry_for_dir(Vendor::Claude, &dir, Source::Discovered, label) {
                push(&mut out, entry);
            }
        }

        // Labels, defaults, quota, sign-in, live sessions.
        let labels: HashMap<String, String> = self.kv(LABELS_KEY).await;
        let defaults: HashMap<String, String> = self.kv(DEFAULTS_KEY).await;
        let stored_quota: HashMap<String, Quota> = self.kv(QUOTA_KEY).await;
        let live = self.live_counts();
        {
            let book = self.quota.lock();
            for entry in &mut out {
                let a = &mut entry.account;
                if let Some(label) = labels.get(a.id.as_str()) {
                    a.label = label.clone();
                }
                a.quota = book
                    .cached
                    .get(&a.id)
                    .cloned()
                    .or_else(|| stored_quota.get(a.id.as_str()).cloned());
                a.sign_in = book.sign_in.get(&a.id).copied().unwrap_or(SignIn::Unknown);
                a.live_sessions = live.get(&a.id).copied().unwrap_or(0);
            }
        }
        for vendor in Vendor::ALL {
            let chosen = defaults
                .get(vendor.as_str())
                .and_then(|id| out.iter().position(|e| e.account.vendor == vendor && e.account.id.as_str() == id && e.account.usable_for_new));
            let index = chosen.or_else(|| {
                out.iter()
                    .position(|e| e.account.vendor == vendor && e.account.source == Source::Ambient)
            });
            if let Some(i) = index {
                out[i].account.is_default = true;
            }
        }
        out
    }

    fn live_counts(&self) -> HashMap<AccountId, u32> {
        let mut out = HashMap::new();
        if let Some(world) = self.world.get() {
            for session in world.snapshot().sessions {
                let live = !matches!(session.control, Control::NotRunning { .. });
                if let (true, Some(id)) = (live, session.account_id.clone()) {
                    *out.entry(id).or_insert(0) += 1;
                }
            }
        }
        out
    }

    /// The reading of a live Conversation session of this account, if World has one.
    fn live_conversation_quota(&self, id: &AccountId) -> (bool, Option<Quota>) {
        let Some(world) = self.world.get() else { return (false, None) };
        let snapshot = world.snapshot();
        let live = snapshot.sessions.iter().any(|s| {
            s.account_id.as_ref() == Some(id)
                && matches!(s.control, Control::InGilde { mode: ControlMode::Conversation, .. })
        });
        let quota = live
            .then(|| snapshot.accounts.iter().find(|a| &a.id == id).and_then(|a| a.quota.clone()))
            .flatten();
        (live, quota)
    }

    /// Every account (the store's `accounts`, the Accounts & health screen).
    pub async fn accounts(&self) -> Vec<Account> {
        self.entries().await.into_iter().map(|e| e.account).collect()
    }

    /// One account.
    pub async fn account(&self, id: &AccountId) -> Option<Account> {
        self.entries().await.into_iter().find(|e| &e.account.id == id).map(|e| e.account)
    }

    /// The accounts New session may offer (`usable_for_new`, §4.2.2).
    pub async fn usable_for_new(&self) -> Vec<Account> {
        self.accounts().await.into_iter().filter(|a| a.usable_for_new).collect()
    }

    /// `session_start` on an unconfirmed Discovered account is refused (PM-44).
    pub async fn require_usable(&self, id: &AccountId) -> std::result::Result<Account, IpcError> {
        let account = self.account(id).await.ok_or_else(|| IpcError::not_found("account"))?;
        if !account.usable_for_new {
            return Err(IpcError::refused("accountNotConfirmed"));
        }
        Ok(account)
    }

    /// The env rule of an account (C8): `None` key = the default login, the
    /// variable stays UNSET; otherwise the variable and the realpath.
    pub async fn env_for(&self, id: &AccountId) -> Option<AccountEnv> {
        self.entries().await.into_iter().find(|e| &e.account.id == id).map(|e| env_of(&e))
    }

    /// Every account's env (the sidecar's `mesh.configure`).
    pub async fn envs(&self) -> Vec<AccountEnv> {
        self.entries().await.iter().map(env_of).collect()
    }

    /// Tell World and the sidecar the current accounts.
    pub async fn publish(&self) {
        let entries = self.entries().await;
        if let Some(world) = self.world.get() {
            let sink = world.sink();
            for e in &entries {
                sink.account(e.account.clone());
            }
        }
        if let Some(sink) = &self.env_sink {
            sink.accounts_changed(entries.iter().map(env_of).collect()).await;
        }
    }

    // -- the §4.2.7 commands -----------------------------------------------------

    /// `account_add`: a native folder picker, pre-selecting a discovered sibling;
    /// validated by `stat`; never runs `claude agents --json`.
    pub async fn add(&self, vendor: AccountAddVendor) -> std::result::Result<Option<Account>, IpcError> {
        let vendor = match vendor {
            AccountAddVendor::Claude => Vendor::Claude,
            AccountAddVendor::Codex => Vendor::Codex,
        };
        let start = match vendor {
            Vendor::Claude => discover_siblings(self.home()).into_iter().next().or_else(|| Some(self.home().to_path_buf())),
            _ => Some(self.home().to_path_buf()),
        };
        let Some(picked) = self.picker.pick_folder(start).await else {
            return Ok(None);
        };
        self.add_dir(vendor, &picked).await.map(Some)
    }

    /// The validation and persistence half of `account_add`.
    pub async fn add_dir(&self, vendor: Vendor, dir: &Path) -> std::result::Result<Account, IpcError> {
        if !is_valid_config_dir(vendor, dir) {
            return Err(IpcError::refused("notAConfigDir"));
        }
        let abs = crate::sidecar::canonical_abs(dir).ok_or_else(|| IpcError::refused("notAConfigDir"))?;
        let id = AccountId::derive(vendor, abs.as_str());
        if let Some(existing) = self.entries().await.into_iter().find(|e| e.account.id == id) {
            if existing.account.source != Source::Discovered {
                return Err(IpcError::refused("alreadyAdded"));
            }
        }
        self.persist_user(vendor, &abs, label_of_dir(Path::new(abs.as_str()))).await?;
        self.publish().await;
        self.account(&id).await.ok_or_else(|| IpcError::internal("accounts-add"))
    }

    async fn persist_user(&self, vendor: Vendor, dir: &AbsPath, label: String) -> std::result::Result<(), IpcError> {
        let row = AccountRow {
            id: uuid::Uuid::now_v7().to_string(),
            vendor: vendor.as_str().to_string(),
            label,
            config_dir: Some(dir.as_str().to_string()),
            model_hint: None,
            source: AccountSource::Manual,
            enabled: true,
            is_default: false,
            imported_at: self.now(),
            last_seen_at: None,
            last_quota_json: None,
            last_quota_at: None,
        };
        self.db
            .write(move |conn| {
                let stored = dao::account::upsert_by_config_dir(conn, &row)?;
                // A removed (disabled) row comes back on re-add.
                dao::account::set_enabled(conn, &stored.id, true)?;
                Ok(())
            })
            .await
            .map_err(|e| IpcError::from(&e))
    }

    /// `account_confirm`: a Discovered sibling becomes `User` and usable (C7).
    pub async fn confirm(&self, id: &AccountId) -> std::result::Result<Account, IpcError> {
        let entry = self
            .entries()
            .await
            .into_iter()
            .find(|e| &e.account.id == id)
            .ok_or_else(|| IpcError::not_found("account"))?;
        if entry.account.source != Source::Discovered {
            return Err(IpcError::refused("notDiscovered"));
        }
        let dir = entry.account.config_dir.clone().ok_or_else(|| IpcError::refused("notDiscovered"))?;
        self.persist_user(entry.account.vendor, &dir, entry.account.label.clone()).await?;
        self.publish().await;
        self.account(id).await.ok_or_else(|| IpcError::internal("accounts-confirm"))
    }

    /// `account_remove`: user-added accounts only; the folder is never touched
    /// (a removed sibling returns as Discovered on the next scan).
    pub async fn remove(&self, id: &AccountId) -> std::result::Result<(), IpcError> {
        let entry = self
            .entries()
            .await
            .into_iter()
            .find(|e| &e.account.id == id)
            .ok_or_else(|| IpcError::not_found("account"))?;
        let (Source::User, Some(row_id)) = (entry.account.source, entry.row_id.clone()) else {
            return Err(IpcError::refused("notUserAdded"));
        };
        if entry.account.live_sessions > 0 {
            return Err(IpcError::refused("inUse"));
        }
        self.db
            .write(move |conn| dao::account::set_enabled(conn, &row_id, false))
            .await
            .map_err(|e| IpcError::from(&e))?;
        let mut defaults: HashMap<String, String> = self.kv(DEFAULTS_KEY).await;
        if defaults.get(entry.account.vendor.as_str()).map(String::as_str) == Some(id.as_str()) {
            defaults.remove(entry.account.vendor.as_str());
            self.set_kv(DEFAULTS_KEY, &defaults).await?;
        }
        self.publish().await;
        Ok(())
    }

    /// `account_set_default`: one default per vendor, usable accounts only.
    pub async fn set_default(&self, id: &AccountId) -> std::result::Result<(), IpcError> {
        let account = self.account(id).await.ok_or_else(|| IpcError::not_found("account"))?;
        if !account.usable_for_new {
            return Err(IpcError::refused("notUsable"));
        }
        let mut defaults: HashMap<String, String> = self.kv(DEFAULTS_KEY).await;
        defaults.insert(account.vendor.as_str().to_string(), id.as_str().to_string());
        self.set_kv(DEFAULTS_KEY, &defaults).await?;
        self.publish().await;
        Ok(())
    }

    /// `account_set_label`: 1..=64 chars, sanitized.
    pub async fn set_label(&self, id: &AccountId, label: &str) -> std::result::Result<(), IpcError> {
        let cleaned = clean_label(label);
        if cleaned.is_empty() {
            return Err(IpcError::invalid_argument("label", "minLength"));
        }
        if self.account(id).await.is_none() {
            return Err(IpcError::not_found("account"));
        }
        let mut labels: HashMap<String, String> = self.kv(LABELS_KEY).await;
        labels.insert(id.as_str().to_string(), cleaned);
        self.set_kv(LABELS_KEY, &labels).await?;
        self.publish().await;
        Ok(())
    }

    /// `account_sign_in`: a Terminal-mode session running the CLI's own sign-in
    /// with this account's env, started by the sessions manager. Gilde handles no
    /// credentials. Idempotent by `clientRequestId` for 60 s.
    pub async fn sign_in(&self, req: &AccountSignInReq) -> std::result::Result<AccountSignInRes, IpcError> {
        let now = self.now();
        {
            let mut seen = self.sign_ins.lock();
            seen.retain(|_, (at, _)| now.saturating_sub(*at) < SIGN_IN_REMEMBER_MS);
            if let Some((_, res)) = seen.get(&req.client_request_id) {
                return Ok(res.clone());
            }
        }
        if self.account(&req.account_id).await.is_none() {
            return Err(IpcError::not_found("account"));
        }
        let sessions = self.sessions.get().cloned().ok_or_else(IpcError::not_built)?;
        let res = sessions.start_sign_in(&req.account_id).await?;
        self.sign_ins
            .lock()
            .insert(req.client_request_id.clone(), (now, res.clone()));
        Ok(res)
    }

    /// `quota_refresh` (§4.2.7): ≤ 1 probe per account per 60 s (agy 5 min),
    /// else the cached value with `stale: true`; a live Conversation session's
    /// own reading first, so no probe process starts for that account.
    pub async fn refresh_quota(&self, id: &AccountId) -> std::result::Result<Quota, IpcError> {
        let account = self.account(id).await.ok_or_else(|| IpcError::not_found("account"))?;
        let now = self.now();
        let (_, live) = self.live_conversation_quota(id);
        let last = self.quota.lock().last_probe_at.get(id).copied();
        match decide_quota_refresh(now, account.vendor, last, live.as_ref()) {
            QuotaDecision::Live(quota) => {
                self.store_quota(id, quota.clone()).await;
                Ok(quota)
            }
            QuotaDecision::Cached => Ok(self.cached_stale(id, &account, now)),
            QuotaDecision::Probe => self.probe(&account, now).await,
        }
    }

    fn cached_stale(&self, id: &AccountId, account: &Account, now: EpochMs) -> Quota {
        let cached = self.quota.lock().cached.get(id).cloned().or_else(|| account.quota.clone());
        let mut quota = cached.unwrap_or_else(|| quota_unavailable(now, QuotaUnavailable::ProbeFailed));
        quota.stale = true;
        quota
    }

    async fn probe(&self, account: &Account, now: EpochMs) -> std::result::Result<Quota, IpcError> {
        self.quota.lock().last_probe_at.insert(account.id.clone(), now);
        let params = ProbeParams {
            vendor: account.vendor,
            account_id: account.id.clone(),
        };
        let quota = match self.sidecar.quota(params.clone()).await {
            Ok(Some(mut quota)) => {
                quota.stale = false;
                drop_out_of_range_pct(&mut quota);
                quota
            }
            Ok(None) => quota_unavailable(now, QuotaUnavailable::NotReported),
            Err(err) => {
                // §10.4: a cached value with its age, else the error.
                let cached = self.quota.lock().cached.get(&account.id).cloned().or_else(|| account.quota.clone());
                return match cached {
                    Some(mut quota) => {
                        quota.stale = true;
                        Ok(quota)
                    }
                    None => Err(err),
                };
            }
        };
        if let Ok(status) = self.sidecar.auth_status(params).await {
            self.quota.lock().sign_in.insert(account.id.clone(), status.sign_in);
        }
        self.store_quota(&account.id, quota.clone()).await;
        Ok(quota)
    }

    async fn store_quota(&self, id: &AccountId, quota: Quota) {
        self.quota.lock().cached.insert(id.clone(), quota.clone());
        let mut stored: HashMap<String, Quota> = self.kv(QUOTA_KEY).await;
        stored.insert(id.as_str().to_string(), quota);
        if let Err(err) = self.set_kv(QUOTA_KEY, &stored).await {
            tracing::warn!(target: "accounts", code = err.code.as_str(), "could not persist a quota reading");
        }
        if let Some(world) = self.world.get() {
            if let Some(account) = self.account(id).await {
                world.sink().account(account);
            }
        }
    }

    /// A quota reading a live session's driver observed (A24.1): kept as the
    /// service's own reading (memory and `settings_kv`, like a probe's) and
    /// published, so the service's next publish shows it instead of
    /// reverting it. No probe process starts (§4.2.7).
    pub async fn observe_quota(&self, id: &AccountId, quota: Quota) {
        self.store_quota(id, quota).await;
    }

    /// A driver saw the account signed out (A24.1): the service's own
    /// sign-in state, then a publish of that account.
    pub async fn observe_signed_out(&self, id: &AccountId) {
        self.quota.lock().sign_in.insert(id.clone(), SignIn::SignedOut);
        if let Some(world) = self.world.get() {
            if let Some(account) = self.account(id).await {
                world.sink().account(account);
            }
        }
    }

    /// One background pass (F-35): every account whose refresh is due.
    pub async fn background_tick(&self) -> usize {
        let now = self.now();
        let view = self.view.lock().clone();
        let mut probed = 0;
        for account in self.accounts().await {
            if account.source == Source::Discovered {
                continue; // read-only until confirmed: no probe of a worker's account
            }
            let (live, _) = self.live_conversation_quota(&account.id);
            let last = self.quota.lock().last_probe_at.get(&account.id).copied();
            if background_refresh_due(now, view.as_ref(), last, live) && self.probe(&account, now).await.is_ok() {
                probed += 1;
            }
        }
        probed
    }

    /// Run [`AccountsService::background_tick`] every minute (the rule decides
    /// what is due). The returned task ends with the service.
    pub fn spawn_background_refresh(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let Some(me) = weak.upgrade() else { return };
                me.background_tick().await;
            }
        })
    }
}

fn env_of(entry: &Entry) -> AccountEnv {
    let (env_key, config_dir) = match (entry.env_key, entry.account.source) {
        (Some(key), source) if source != Source::Ambient => {
            (Some(key.to_string()), entry.account.config_dir.clone())
        }
        _ => (None, None),
    };
    AccountEnv {
        account_id: entry.account.id.clone(),
        vendor: entry.account.vendor,
        env_key,
        config_dir,
    }
}

/// Every `~/.claude-*` sibling holding a `.claude.json` (non-recursive listing of
/// `$HOME`; `stat` only), sorted.
pub fn discover_siblings(home: &Path) -> Vec<PathBuf> {
    let Ok(read) = std::fs::read_dir(home) else { return Vec::new() };
    let mut out: Vec<PathBuf> = read
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| is_discovered_sibling(p))
        .collect();
    out.sort();
    out
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

        // The user pins their own values...
        f.config
            .patch(serde_json::json!({
                "agents": { "binaries": { "claude": "/usr/local/bin/claude" } }
            }))
            .expect("patch");

        // ...and a re-import leaves every one of them alone.
        import_cco(&f.db, &f.config, &f.paths).await.expect("import");
        let cfg = f.config.get();
        assert_eq!(cfg.agent_binary("claude"), Some("/usr/local/bin/claude"));
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

/// The Gilde 2 account tests (§15.2 T08): T-accounts-quotaRate,
/// T-accounts-quotaRange, T-accounts-agy, T-accounts-confirm,
/// T-env-defaultAccount. Every service test runs against the synthetic home of
/// `tests/fixtures/cco/home-basic` with every credential file planted as a FIFO
/// (C6): an `open` of one would block and fail the test. The sidecar is a fake;
/// no vendor process and no vendor endpoint is ever involved.
#[cfg(test)]
mod gilde2_tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

    use crate::config::cco_import::fixture_home;
    use crate::contract::account::{QuotaWindow, QuotaWindowKind};
    use crate::contract::error::ErrorCode;
    use crate::contract::services::TurnEventSink;
    use crate::contract::sidecar::{AuthStatus, ModelInfo, TurnStartParams};

    const T0: EpochMs = 1_800_000_000_000;

    type Reply = std::result::Result<Option<Quota>, IpcError>;

    struct FakeSidecar {
        calls: AtomicUsize,
        probed: Mutex<Vec<AccountId>>,
        reply: Mutex<Reply>,
    }

    impl FakeSidecar {
        fn new(reply: Reply) -> Arc<FakeSidecar> {
            Arc::new(FakeSidecar {
                calls: AtomicUsize::new(0),
                probed: Mutex::new(Vec::new()),
                reply: Mutex::new(reply),
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl SidecarApi for FakeSidecar {
        async fn ensure_started(&self) -> std::result::Result<(), IpcError> {
            Ok(())
        }
        async fn auth_status(&self, _p: ProbeParams) -> std::result::Result<AuthStatus, IpcError> {
            Ok(AuthStatus { sign_in: SignIn::SignedIn })
        }
        async fn models(&self, _p: ProbeParams) -> std::result::Result<Vec<ModelInfo>, IpcError> {
            Ok(Vec::new())
        }
        async fn quota(&self, p: ProbeParams) -> std::result::Result<Option<Quota>, IpcError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.probed.lock().push(p.account_id);
            self.reply.lock().clone()
        }
        async fn turn_start(
            &self,
            _p: TurnStartParams,
            _sink: Arc<dyn TurnEventSink>,
        ) -> std::result::Result<(), IpcError> {
            Err(IpcError::unsupported("turns"))
        }
        async fn turn_cancel(&self, _turn_id: &str) -> std::result::Result<bool, IpcError> {
            Ok(false)
        }
    }

    struct Picker(Option<PathBuf>);

    #[async_trait]
    impl FolderPicker for Picker {
        async fn pick_folder(&self, _start: Option<PathBuf>) -> Option<PathBuf> {
            self.0.clone()
        }
    }

    struct Rig {
        home: PathBuf,
        service: Arc<AccountsService>,
        sidecar: Arc<FakeSidecar>,
        clock: Arc<AtomicI64>,
    }

    impl Rig {
        fn advance(&self, ms: EpochMs) {
            self.clock.fetch_add(ms, Ordering::SeqCst);
        }

        async fn by_dir(&self, name: &str) -> Account {
            let dir = std::fs::canonicalize(self.home.join(name)).expect("fixture dir");
            self.service
                .accounts()
                .await
                .into_iter()
                .find(|a| a.config_dir.as_ref().map(|d| d.as_str()) == dir.to_str())
                .unwrap_or_else(|| panic!("no account for {name}"))
        }

        async fn default_of(&self, vendor: Vendor) -> Account {
            let all = self.service.accounts().await;
            let defaults: Vec<&Account> = all.iter().filter(|a| a.vendor == vendor && a.is_default).collect();
            assert_eq!(defaults.len(), 1, "exactly one default per vendor");
            defaults[0].clone()
        }
    }

    /// Sends when dropped — also when the test body panics.
    struct Done(std::sync::mpsc::Sender<()>);
    impl Drop for Done {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }

    /// Run `body` against a fresh copy of the fixture home with every credential
    /// file planted as a FIFO, on its own thread and runtime: a body that opened
    /// one would never finish, and fails here (C6).
    fn guarded<F, Fut>(what: &'static str, reply: Reply, picked: Option<&str>, body: F)
    where
        F: FnOnce(Rig) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()>,
    {
        let (dir, home) = fixture_home::copy("home-basic");
        fixture_home::plant_credential_fifos(&home);
        let picked = picked.map(|name| home.join(name));
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let _done = Done(tx);
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            runtime.block_on(async move {
                let sidecar = FakeSidecar::new(reply);
                let clock = Arc::new(AtomicI64::new(T0));
                let read = clock.clone();
                let service = AccountsService::new(
                    Db::open_in_memory().expect("db"),
                    AppPaths::for_home(&home),
                    sidecar.clone(),
                    None,
                    Arc::new(Picker(picked)),
                )
                .with_clock(Arc::new(move || read.load(Ordering::SeqCst)));
                body(Rig { home, service, sidecar, clock }).await;
            });
        });
        match rx.recv_timeout(std::time::Duration::from_secs(20)) {
            Ok(()) => {
                if let Err(panic) = handle.join() {
                    std::panic::resume_unwind(panic);
                }
            }
            Err(_) => panic!("{what} did not finish: it opened a credential FIFO (C6)"),
        }
        drop(dir);
    }

    fn window(pct: Option<u8>) -> QuotaWindow {
        QuotaWindow {
            kind: QuotaWindowKind::FiveHour,
            label: None,
            used_pct: pct,
            resets_at: Some(T0 + 3_600_000),
        }
    }

    fn quota_with(pct: u8, at: EpochMs) -> Quota {
        Quota {
            windows: vec![window(Some(pct))],
            fetched_at: at,
            stale: false,
            unavailable: None,
        }
    }

    fn view(screen: Screen, visible: bool) -> ViewState {
        ViewState {
            screen,
            visible_uids: Vec::new(),
            focused_uid: None,
            zoomed_uid: None,
            team_key: None,
            document_visible: visible,
        }
    }

    fn reason(err: &IpcError) -> Option<&str> {
        err.params.get("reason").and_then(|v| v.as_str())
    }

    // -- T-accounts-quotaRate ---------------------------------------------------

    #[test]
    fn t_accounts_quota_rate_explicit_rule() {
        assert_eq!(decide_quota_refresh(T0, Vendor::Claude, None, None), QuotaDecision::Probe);
        assert_eq!(decide_quota_refresh(T0 + 59_999, Vendor::Claude, Some(T0), None), QuotaDecision::Cached);
        assert_eq!(decide_quota_refresh(T0 + 60_000, Vendor::Claude, Some(T0), None), QuotaDecision::Probe);
        assert_eq!(decide_quota_refresh(T0 + 60_000, Vendor::Codex, Some(T0), None), QuotaDecision::Probe);
        // agy writes logs on each read: five minutes.
        assert_eq!(decide_quota_refresh(T0 + 299_999, Vendor::Agy, Some(T0), None), QuotaDecision::Cached);
        assert_eq!(decide_quota_refresh(T0 + 300_000, Vendor::Agy, Some(T0), None), QuotaDecision::Probe);
        // A live Conversation session's own recent reading: no probe at all.
        let live = quota_with(40, T0);
        assert_eq!(
            decide_quota_refresh(T0 + 1_000, Vendor::Claude, None, Some(&live)),
            QuotaDecision::Live(live.clone())
        );
        assert_eq!(
            decide_quota_refresh(T0 + 1_000, Vendor::Claude, Some(T0), Some(&live)),
            QuotaDecision::Live(live)
        );
        // …but an old one is not a reading.
        let old = quota_with(40, T0 - LIVE_QUOTA_FRESH_MS - 1);
        assert_eq!(decide_quota_refresh(T0, Vendor::Claude, None, Some(&old)), QuotaDecision::Probe);
    }

    #[test]
    fn t_accounts_quota_rate_background_rule() {
        let home = view(Screen::Home, true);
        assert!(background_refresh_due(T0, Some(&home), None, false));
        assert!(background_refresh_due(T0, Some(&view(Screen::Accounts, true)), None, false));
        assert!(!background_refresh_due(T0, None, None, false), "no view state yet");
        assert!(!background_refresh_due(T0, Some(&view(Screen::Home, false)), None, false), "hidden window");
        for screen in [Screen::Session, Screen::Split, Screen::Team, Screen::Floor, Screen::History, Screen::Settings] {
            assert!(!background_refresh_due(T0, Some(&view(screen, true)), None, false), "{screen:?}");
        }
        assert!(!background_refresh_due(T0 + 299_999, Some(&home), Some(T0), false));
        assert!(background_refresh_due(T0 + 300_000, Some(&home), Some(T0), false));
        assert!(!background_refresh_due(T0, Some(&home), None, true), "a live Conversation session reports it");
    }

    #[test]
    fn t_accounts_quota_rate_explicit_refresh_probes_at_most_once_a_minute() {
        guarded("quota rate", Ok(Some(quota_with(30, T0))), None, |r| async move {
            let claude = r.default_of(Vendor::Claude).await;
            let first = r.service.refresh_quota(&claude.id).await.expect("probe");
            assert!(!first.stale);
            assert_eq!(r.sidecar.calls(), 1);

            r.advance(10_000);
            let second = r.service.refresh_quota(&claude.id).await.expect("cached");
            assert!(second.stale, "inside the minute: the cached value, stale");
            assert_eq!(second.windows, first.windows);
            assert_eq!(r.sidecar.calls(), 1, "no second probe");

            r.advance(50_000);
            let third = r.service.refresh_quota(&claude.id).await.expect("probe again");
            assert!(!third.stale);
            assert_eq!(r.sidecar.calls(), 2);

            // agy: once per five minutes.
            let agy = r.default_of(Vendor::Agy).await;
            r.service.refresh_quota(&agy.id).await.expect("agy probe");
            assert_eq!(r.sidecar.calls(), 3);
            r.advance(120_000);
            assert!(r.service.refresh_quota(&agy.id).await.expect("agy cached").stale);
            assert_eq!(r.sidecar.calls(), 3);
            r.advance(180_000);
            assert!(!r.service.refresh_quota(&agy.id).await.expect("agy probe").stale);
            assert_eq!(r.sidecar.calls(), 4);

            // The reading is kept per account and shown on it, with the sign-in.
            let shown = r.service.account(&claude.id).await.expect("account");
            assert_eq!(shown.quota.map(|q| q.windows), Some(first.windows));
            assert_eq!(shown.sign_in, SignIn::SignedIn);
        });
    }

    #[test]
    fn t_accounts_quota_rate_a_failed_probe_answers_the_cached_value_or_the_error() {
        guarded("quota failure", Err(IpcError::sidecar_down()), None, |r| async move {
            let claude = r.default_of(Vendor::Claude).await;
            let err = r.service.refresh_quota(&claude.id).await.expect_err("nothing cached");
            assert_eq!(err.code, ErrorCode::SidecarDown);

            *r.sidecar.reply.lock() = Ok(Some(quota_with(10, T0)));
            r.advance(60_000);
            r.service.refresh_quota(&claude.id).await.expect("a reading");

            *r.sidecar.reply.lock() = Err(IpcError::sidecar_down());
            r.advance(60_000);
            let cached = r.service.refresh_quota(&claude.id).await.expect("the cached value");
            assert!(cached.stale);
            assert_eq!(cached.windows[0].used_pct, Some(10));

            // No reading reported: unavailable, not an error.
            *r.sidecar.reply.lock() = Ok(None);
            r.advance(60_000);
            let none = r.service.refresh_quota(&claude.id).await.expect("unavailable");
            assert_eq!(none.unavailable, Some(QuotaUnavailable::NotReported));
        });
    }

    #[test]
    fn t_accounts_quota_rate_background_tick_follows_the_view_and_skips_discovered() {
        guarded("background refresh", Ok(Some(quota_with(5, T0))), None, |r| async move {
            // Nothing before the WebView reports a view, nor on another screen.
            assert_eq!(r.service.background_tick().await, 0);
            r.service.observe_view(view(Screen::Session, true));
            assert_eq!(r.service.background_tick().await, 0);
            r.service.observe_view(view(Screen::Home, false));
            assert_eq!(r.service.background_tick().await, 0);
            assert_eq!(r.sidecar.calls(), 0);

            r.service.observe_view(view(Screen::Home, true));
            let accounts = r.service.accounts().await;
            let discovered: Vec<AccountId> = accounts
                .iter()
                .filter(|a| a.source == Source::Discovered)
                .map(|a| a.id.clone())
                .collect();
            assert!(!discovered.is_empty(), "the fixture has a discovered sibling");
            let due = accounts.len() - discovered.len();
            assert_eq!(r.service.background_tick().await, due);
            assert!(
                r.sidecar.probed.lock().iter().all(|id| !discovered.contains(id)),
                "a Discovered account is never probed"
            );

            // Not again inside five minutes, again after.
            r.advance(60_000);
            assert_eq!(r.service.background_tick().await, 0);
            r.advance(240_000);
            assert_eq!(r.service.background_tick().await, due);
            assert_eq!(r.sidecar.calls(), 2 * due);
        });
    }

    // -- T-accounts-quotaRange --------------------------------------------------

    #[test]
    fn t_accounts_quota_range_is_absent_never_clamped() {
        let mut quota = Quota {
            windows: vec![window(Some(0)), window(Some(100)), window(Some(101)), window(Some(255)), window(None)],
            fetched_at: T0,
            stale: false,
            unavailable: None,
        };
        drop_out_of_range_pct(&mut quota);
        let pcts: Vec<Option<u8>> = quota.windows.iter().map(|w| w.used_pct).collect();
        assert_eq!(pcts, vec![Some(0), Some(100), None, None, None]);

        // The adapter-boundary rule the sidecar applies (fractions vs percent).
        assert_eq!(QuotaWindow::normalise_pct(0.42, true), Some(42));
        assert_eq!(QuotaWindow::normalise_pct(42.0, false), Some(42));
        assert_eq!(QuotaWindow::normalise_pct(1.5, true), None);
        assert_eq!(QuotaWindow::normalise_pct(140.0, false), None);
        assert_eq!(QuotaWindow::normalise_pct(-1.0, false), None);
        assert_eq!(QuotaWindow::normalise_pct(f64::NAN, false), None);

        // Through the service: an out-of-range reading arrives absent.
        guarded("quota range", Ok(Some(quota_with(150, T0))), None, |r| async move {
            let claude = r.default_of(Vendor::Claude).await;
            let quota = r.service.refresh_quota(&claude.id).await.expect("probe");
            assert_eq!(quota.windows[0].used_pct, None);
        });
    }

    // -- T-accounts-agy -----------------------------------------------------------

    #[test]
    fn t_accounts_agy_is_one_account_the_default_login() {
        guarded("agy", Ok(None), None, |r| async move {
            let accounts = r.service.accounts().await;
            let agy: Vec<&Account> = accounts.iter().filter(|a| a.vendor == Vendor::Agy).collect();
            assert_eq!(agy.len(), 1, "one agy account");
            assert_eq!(agy[0].source, Source::Ambient);
            assert!(agy[0].config_dir.is_none());
            assert!(agy[0].is_default);
            assert_eq!(agy[0].id, AccountId::derive_default_login(Vendor::Agy));
            let env = r.service.env_for(&agy[0].id).await.expect("env");
            assert_eq!(env.env_key, None, "no home variable is ever set for agy");
            assert_eq!(env.config_dir, None);
            assert_eq!(crate::sidecar::account_env_key(Vendor::Agy), None);

            // No config dir is ever an agy account.
            assert!(!is_valid_config_dir(Vendor::Agy, &r.home.join(".claude")));
            let err = r.service.add_dir(Vendor::Agy, &r.home.join(".claude-stray")).await.expect_err("agy");
            assert_eq!((err.code, reason(&err)), (ErrorCode::Refused, Some("notAConfigDir")));
        });
    }

    // -- T-accounts-confirm -------------------------------------------------------

    #[test]
    fn t_accounts_confirm_discovered_is_read_only_until_confirmed() {
        guarded("confirm", Ok(None), None, |r| async move {
            let extra = r.by_dir(".claude-extra").await;
            assert_eq!(extra.source, Source::Discovered);
            assert!(!extra.usable_for_new);
            assert!(!extra.is_default);
            assert!(!r.service.usable_for_new().await.iter().any(|a| a.id == extra.id));

            // A sibling without `.claude.json` is never discovered.
            let stray = std::fs::canonicalize(r.home.join(".claude-stray")).expect("stray");
            assert!(!r
                .service
                .accounts()
                .await
                .iter()
                .any(|a| a.config_dir.as_ref().map(|d| d.as_str()) == stray.to_str()));

            // session_start's check, the default and remove refuse it.
            let err = r.service.require_usable(&extra.id).await.expect_err("unconfirmed");
            assert_eq!((err.code, reason(&err)), (ErrorCode::Refused, Some("accountNotConfirmed")));
            let err = r.service.set_default(&extra.id).await.expect_err("not usable");
            assert_eq!(reason(&err), Some("notUsable"));
            let err = r.service.remove(&extra.id).await.expect_err("not user-added");
            assert_eq!(reason(&err), Some("notUserAdded"));

            // Confirm: User, usable, same id.
            let confirmed = r.service.confirm(&extra.id).await.expect("confirm");
            assert_eq!(confirmed.id, extra.id);
            assert_eq!(confirmed.source, Source::User);
            assert!(confirmed.usable_for_new);
            let err = r.service.confirm(&extra.id).await.expect_err("twice");
            assert_eq!(reason(&err), Some("notDiscovered"));
            r.service.require_usable(&extra.id).await.expect("usable now");

            // It may become the default, replacing the default login.
            r.service.set_default(&extra.id).await.expect("default");
            assert_eq!(r.default_of(Vendor::Claude).await.id, extra.id);

            // Removing it never touches the folder: it is Discovered again, and
            // the default falls back to the default login.
            r.service.remove(&extra.id).await.expect("remove");
            assert!(r.home.join(".claude-extra").is_dir());
            let again = r.by_dir(".claude-extra").await;
            assert_eq!(again.source, Source::Discovered);
            assert_eq!(r.default_of(Vendor::Claude).await.source, Source::Ambient);

            // The orchestrator's workers come from its registry, read-only.
            let work = r.by_dir(".claude-work").await;
            assert_eq!(work.source, Source::Orchestrator);
            assert_eq!(work.label, "work");
            let err = r.service.confirm(&work.id).await.expect_err("not discovered");
            assert_eq!(reason(&err), Some("notDiscovered"));
            let err = r.service.remove(&work.id).await.expect_err("not user-added");
            assert_eq!(reason(&err), Some("notUserAdded"));
        });
    }

    // -- T-env-defaultAccount -----------------------------------------------------

    #[test]
    fn t_env_default_account_leaves_the_variable_unset() {
        guarded("default env", Ok(None), Some(".codex-work"), |r| async move {
            let claude = r.default_of(Vendor::Claude).await;
            assert_eq!(claude.source, Source::Ambient);
            let env = r.service.env_for(&claude.id).await.expect("env");
            assert_eq!(env.env_key, None, "CLAUDE_CONFIG_DIR stays unset for ~/.claude (C8)");
            assert_eq!(env.config_dir, None);
            let codex = r.default_of(Vendor::Codex).await;
            let env = r.service.env_for(&codex.id).await.expect("env");
            assert_eq!((env.env_key, env.config_dir), (None, None));

            // Any other account carries the variable and its realpath.
            let work = r.by_dir(".claude-work").await;
            let env = r.service.env_for(&work.id).await.expect("env");
            assert_eq!(env.env_key.as_deref(), Some("CLAUDE_CONFIG_DIR"));
            let real = std::fs::canonicalize(r.home.join(".claude-work")).expect("realpath");
            assert_eq!(env.config_dir.as_ref().map(|d| d.as_str()), real.to_str());

            // A user-added codex home (through the picker).
            let added = r
                .service
                .add(AccountAddVendor::Codex)
                .await
                .expect("add")
                .expect("not cancelled");
            assert_eq!(added.source, Source::User);
            let env = r.service.env_for(&added.id).await.expect("env");
            assert_eq!(env.env_key.as_deref(), Some("CODEX_HOME"));
            let real = std::fs::canonicalize(r.home.join(".codex-work")).expect("realpath");
            assert_eq!(env.config_dir.as_ref().map(|d| d.as_str()), real.to_str());
            let err = r.service.add_dir(Vendor::Codex, &r.home.join(".codex-work")).await.expect_err("twice");
            assert_eq!(reason(&err), Some("alreadyAdded"));

            // The default login itself is never added as an account with the variable.
            let err = r.service.add_dir(Vendor::Claude, &r.home.join(".claude")).await.expect_err("default");
            assert_eq!(reason(&err), Some("alreadyAdded"));
            let err = r.service.add_dir(Vendor::Claude, &r.home.join("missing")).await.expect_err("missing");
            assert_eq!(reason(&err), Some("notAConfigDir"));

            // The sidecar's view: every account, and no variable points at a default dir.
            let envs = r.service.envs().await;
            assert_eq!(envs.len(), r.service.accounts().await.len());
            let default_dirs: Vec<PathBuf> = [".claude", ".codex"]
                .iter()
                .map(|d| std::fs::canonicalize(r.home.join(d)).expect("realpath"))
                .collect();
            for env in &envs {
                if let Some(dir) = &env.config_dir {
                    assert!(env.env_key.is_some());
                    assert!(!default_dirs.iter().any(|d| d.to_str() == Some(dir.as_str())));
                }
            }
        });
    }

    // -- labels, sign-in --------------------------------------------------------

    #[test]
    fn labels_are_sanitized_and_sign_in_needs_the_sessions_manager() {
        guarded("labels", Ok(None), None, |r| async move {
            let claude = r.default_of(Vendor::Claude).await;
            r.service
                .set_label(&claude.id, "  \u{202E}Personal\nmain  ")
                .await
                .expect("label");
            assert_eq!(r.service.account(&claude.id).await.expect("account").label, "Personalmain");
            let err = r.service.set_label(&claude.id, " \n ").await.expect_err("empty");
            assert_eq!(err.code, ErrorCode::InvalidArgument);
            let long = "x".repeat(80);
            r.service.set_label(&claude.id, &long).await.expect("long");
            assert_eq!(r.service.account(&claude.id).await.expect("account").label.chars().count(), 64);

            let req = AccountSignInReq {
                account_id: claude.id.clone(),
                client_request_id: ClientRequestId::parse("1b4e28ba-2fa1-41d2-883f-0016d3cca427".to_string())
                    .expect("uuid"),
            };
            let err = r.service.sign_in(&req).await.expect_err("not bound");
            assert_eq!(err.code, ErrorCode::Unsupported);
        });
    }
}
