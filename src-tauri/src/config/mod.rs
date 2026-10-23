//! `ConfigStore` — the in-memory `config.json` plus a debounced, atomic writer.
//!
//! DESIGN §10-3 pins the behaviour:
//!
//! * reads are lock-free-ish (`RwLock` read, clone out) so any thread can ask
//!   for settings without waiting on disk;
//! * writes are coalesced for 500 ms and then land atomically —
//!   `config.json.tmp` → `File::sync_all` → `rename` → directory `sync_all`;
//! * a `config.json` that will not parse is moved aside to
//!   `config.json.bad-<ts>` and replaced by the defaults. **Boot never fails
//!   because of this file**; the caller reports [`ConfigWarning`] as a toast.
//!
//! The debounce lives on a dedicated OS thread rather than a tokio task: the
//! store is constructed during `setup()` (before the async runtime is doing
//! anything useful), it must survive shutdown long enough to flush, and it must
//! be usable from blocking code such as the reaper's startup sweep.

pub mod cco_import;
pub mod model;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex, RwLock};
use serde_json::Value;

use crate::error::{GildeError, Result};
use crate::paths::AppPaths;

pub use cco_import::{
    dir_has_login, import_cco_registry, scan_cco_registry, watch_cco_registry, CcoScan,
    CcoSettings, Watcher,
};
pub use model::{deep_merge, AppConfig};

/// How long a burst of `patch()` calls is coalesced before hitting the disk.
pub const DEBOUNCE: Duration = Duration::from_millis(500);

/// Why the config on disk was not usable. Not an error: the app boots with
/// defaults and the UI shows where the old file went (DESIGN §10-3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigWarning {
    /// i18n key for the toast (`src/i18n/ko.ts`).
    pub key: &'static str,
    /// Where the unparseable file was moved.
    pub backup: PathBuf,
    /// Parser message, shown verbatim in a monospace block.
    pub detail: String,
}

/// Called after every applied change with the new settings (the composition
/// root forwards attention/notification settings to World and notifications).
/// Runs on the caller's thread with no store lock held; must not block.
pub type ConfigListener = Arc<dyn Fn(&AppConfig) + Send + Sync>;

/// Shared, mutable application settings.
pub struct ConfigStore {
    inner: Arc<Inner>,
    writer: Mutex<Option<JoinHandle<()>>>,
    listeners: Mutex<Vec<ConfigListener>>,
}

struct Inner {
    path: PathBuf,
    current: RwLock<AppConfig>,
    /// Debounce state + the writer thread's wakeup.
    pending: Mutex<Pending>,
    wake: Condvar,
    /// Serializes actual file writes between the writer thread and `flush()`.
    io: Mutex<()>,
    warning: Option<ConfigWarning>,
}

#[derive(Debug, Default)]
struct Pending {
    /// When the coalesced write is due. `None` ⇒ nothing to write.
    due: Option<Instant>,
    stop: bool,
    /// Bumped on every completed write; tests wait on it.
    writes: u64,
}

impl ConfigStore {
    /// Load `<app-data>/config.json`, starting the debounced writer.
    pub fn load(paths: &AppPaths) -> Result<Self> {
        Self::open_at(&paths.config_file())
    }

    /// Same as [`ConfigStore::load`] against an explicit path (tests, and any
    /// caller that already resolved the file).
    pub fn open_at(path: &Path) -> Result<Self> {
        let (config, warning) = read_or_recover(path)?;

        let inner = Arc::new(Inner {
            path: path.to_path_buf(),
            current: RwLock::new(config),
            pending: Mutex::new(Pending::default()),
            wake: Condvar::new(),
            io: Mutex::new(()),
            warning,
        });

        let store = Self {
            inner: inner.clone(),
            writer: Mutex::new(None),
            listeners: Mutex::new(Vec::new()),
        };

        let handle = std::thread::Builder::new()
            .name("gilde-config-writer".to_string())
            .spawn(move || writer_loop(inner))
            .map_err(GildeError::Io)?;
        *store.writer.lock() = Some(handle);

        Ok(store)
    }

    /// The file this store is backed by.
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// `Some` when the previous `config.json` had to be quarantined.
    pub fn warning(&self) -> Option<&ConfigWarning> {
        self.inner.warning.as_ref()
    }

    /// A snapshot of the current settings.
    pub fn get(&self) -> AppConfig {
        self.inner.current.read().clone()
    }

    /// Deep-merge `patch` into the current settings and schedule a write.
    ///
    /// The merge happens on the JSON projection so the frontend can send exactly
    /// the subtree it changed (`{"terminal":{"fontSize":13}}`) without knowing
    /// the rest of the shape. Unknown keys survive; `null` removes a key and
    /// lets the serde default take over. A patch that cannot be re-read as an
    /// `AppConfig` is rejected **and the in-memory config is left untouched**.
    pub fn patch(&self, patch: Value) -> Result<AppConfig> {
        if !patch.is_object() {
            return Err(GildeError::config("settings patch must be a JSON object"));
        }

        let updated = {
            let mut guard = self.inner.current.write();
            let mut merged = serde_json::to_value(&*guard).map_err(GildeError::config)?;
            deep_merge(&mut merged, patch);

            let next: AppConfig = serde_json::from_value(merged).map_err(|e| {
                GildeError::config(format!("settings patch produced an unusable config: {e}"))
            })?;
            *guard = next.clone();
            next
        };

        self.mark_dirty();
        self.changed(&updated);
        Ok(updated)
    }

    /// Register `listener` for every later change ([`ConfigListener`]).
    pub fn on_change(&self, listener: ConfigListener) {
        self.listeners.lock().push(listener);
    }

    fn changed(&self, config: &AppConfig) {
        let listeners = self.listeners.lock().clone();
        for listener in listeners {
            listener(config);
        }
    }

    /// Replace the whole config (used by the importer, which computes a new
    /// value from several sources at once). Schedules a write.
    pub fn replace(&self, config: AppConfig) -> AppConfig {
        {
            let mut guard = self.inner.current.write();
            *guard = config.clone();
        }
        self.mark_dirty();
        self.changed(&config);
        config
    }

    /// Write the current settings now, atomically, on the calling thread.
    pub fn flush(&self) -> Result<()> {
        self.inner.pending.lock().due = None;
        self.inner.write_now()
    }

    /// Number of completed disk writes. Tests only.
    #[cfg(test)]
    fn writes(&self) -> u64 {
        self.inner.pending.lock().writes
    }

    fn mark_dirty(&self) {
        let mut pending = self.inner.pending.lock();
        pending.due = Some(Instant::now() + DEBOUNCE);
        self.inner.wake.notify_all();
    }
}

impl std::fmt::Debug for ConfigStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigStore")
            .field("path", &self.inner.path)
            .field("warning", &self.inner.warning)
            .finish()
    }
}

impl Drop for ConfigStore {
    fn drop(&mut self) {
        {
            let mut pending = self.inner.pending.lock();
            pending.stop = true;
            self.inner.wake.notify_all();
        }
        if let Some(handle) = self.writer.lock().take() {
            let _ = handle.join();
        }
    }
}

impl Inner {
    /// `config.json.tmp` → `sync_all` → `rename` → directory `sync_all`.
    ///
    /// The temp file is opened with `create(true).truncate(true)`, so a torn
    /// leftover from a previous crash is overwritten rather than appended to,
    /// and `config.json` itself is never touched until a complete, fsynced file
    /// exists next to it.
    fn write_now(&self) -> Result<()> {
        // The io lock is taken *before* the snapshot: two writers (the debounce
        // thread and an explicit `flush`) must not serialize their file writes
        // in the opposite order from the snapshots they took, which would leave
        // the older config on disk.
        let _io = self.io.lock();
        let snapshot = self.current.read().clone();

        let json = serde_json::to_string_pretty(&snapshot).map_err(GildeError::config)?;
        let dir = self
            .path
            .parent()
            .ok_or_else(|| GildeError::config("config path has no parent directory"))?;
        std::fs::create_dir_all(dir)?;

        let tmp = tmp_path(&self.path);
        {
            use std::io::Write as _;
            let mut file = std::fs::File::create(&tmp)?;
            file.write_all(json.as_bytes())?;
            file.write_all(b"\n")?;
            file.sync_all()?;
        }
        std::fs::rename(&tmp, &self.path)?;

        // Durably record the rename itself; on APFS the directory entry swap is
        // atomic, but without this fsync it can still be lost on power failure.
        if let Ok(dir_handle) = std::fs::File::open(dir) {
            let _ = dir_handle.sync_all();
        }

        self.pending.lock().writes += 1;
        Ok(())
    }
}

/// What the writer thread should do next.
enum Step {
    /// The debounce window closed: persist and keep going.
    Write,
    /// Shutting down with a write still owed: persist, then stop.
    WriteThenStop,
    /// Shutting down with nothing owed.
    Stop,
}

fn writer_loop(inner: Arc<Inner>) {
    loop {
        let step = {
            let mut pending = inner.pending.lock();
            loop {
                if pending.stop {
                    break if pending.due.take().is_some() {
                        Step::WriteThenStop
                    } else {
                        Step::Stop
                    };
                }
                match pending.due {
                    None => inner.wake.wait(&mut pending),
                    Some(due) if Instant::now() >= due => {
                        pending.due = None;
                        break Step::Write;
                    }
                    // A later patch pushes `due` out; the wait re-runs this loop.
                    Some(due) => {
                        inner.wake.wait_until(&mut pending, due);
                    }
                }
            }
        };

        match step {
            Step::Stop => break,
            Step::Write | Step::WriteThenStop => {
                if let Err(err) = inner.write_now() {
                    tracing::error!(
                        path = %inner.path.display(),
                        error = %err,
                        "could not persist config.json"
                    );
                }
                if matches!(step, Step::WriteThenStop) {
                    break;
                }
            }
        }
    }
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".tmp");
    PathBuf::from(name)
}

/// Read the config, quarantining a file that will not parse.
fn read_or_recover(path: &Path) -> Result<(AppConfig, Option<ConfigWarning>)> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok((AppConfig::default(), None));
        }
        Err(err) => return Err(GildeError::Io(err)),
    };

    match serde_json::from_str::<AppConfig>(&raw) {
        Ok(config) => Ok((config, None)),
        Err(err) => {
            let backup = quarantine(path)?;
            tracing::warn!(
                path = %path.display(),
                backup = %backup.display(),
                error = %err,
                "config.json did not parse; starting from defaults"
            );
            Ok((
                AppConfig::default(),
                Some(ConfigWarning {
                    key: "toast.config.quarantined",
                    backup,
                    detail: err.to_string(),
                }),
            ))
        }
    }
}

/// Move `config.json` to `config.json.bad-<epoch-ms>`, returning the new path.
fn quarantine(path: &Path) -> Result<PathBuf> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);

    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".bad-{stamp}"));
    let backup = PathBuf::from(name);

    std::fs::rename(path, &backup)?;
    Ok(backup)
}

#[cfg(test)]
mod tests {
    use super::model::{Locale, ThemePreference};
    use super::*;
    use serde_json::json;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn a_missing_file_loads_the_defaults_without_writing_anything() {
        let dir = temp_dir();
        let path = dir.path().join("config.json");

        let store = ConfigStore::open_at(&path).expect("load");

        assert_eq!(store.get(), AppConfig::default());
        assert!(store.warning().is_none());
        assert!(!path.exists(), "load must not create the file");
    }

    #[test]
    fn patch_deep_merges_and_persists_atomically() {
        let dir = temp_dir();
        let path = dir.path().join("config.json");
        let store = ConfigStore::open_at(&path).expect("load");

        let updated = store
            .patch(json!({ "terminal": { "fontSize": 15.0 }, "locale": "en" }))
            .expect("patch");

        assert_eq!(updated.terminal.font_size, 15.0);
        assert_eq!(updated.locale, Locale::En);
        // Untouched siblings survive the merge.
        assert_eq!(updated.terminal.font_family, "SF Mono, Menlo, monospace");

        store.flush().expect("flush");

        let on_disk: AppConfig =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("parse");
        assert_eq!(on_disk, updated);
        assert!(!tmp_path(&path).exists(), "tmp file must be renamed away");
    }

    #[test]
    fn writes_are_debounced_into_one_disk_write() {
        let dir = temp_dir();
        let path = dir.path().join("config.json");
        let store = ConfigStore::open_at(&path).expect("load");

        for size in 10..30 {
            store
                .patch(json!({ "terminal": { "fontSize": size } }))
                .expect("patch");
        }

        // Nothing has hit the disk yet — the debounce window is still open.
        assert_eq!(store.writes(), 0);
        assert!(!path.exists());

        let deadline = Instant::now() + Duration::from_secs(5);
        while store.writes() == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }

        assert_eq!(store.writes(), 1, "20 patches must coalesce into one write");
        let on_disk: AppConfig =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("parse");
        assert_eq!(on_disk.terminal.font_size, 29.0);
    }

    #[test]
    fn a_truncated_tmp_file_never_damages_the_live_config() {
        let dir = temp_dir();
        let path = dir.path().join("config.json");

        // A good config on disk...
        {
            let store = ConfigStore::open_at(&path).expect("load");
            store.patch(json!({ "locale": "en" })).expect("patch");
            store.flush().expect("flush");
        }
        let good = std::fs::read_to_string(&path).expect("read");

        // ...and a crash in the middle of the next write: a half-written temp
        // file is left behind.
        std::fs::write(tmp_path(&path), b"{\"locale\": \"e").expect("write tmp");

        assert_eq!(std::fs::read_to_string(&path).expect("read"), good);
        let store = ConfigStore::open_at(&path).expect("reload");
        assert_eq!(store.get().locale, Locale::En);
        assert!(store.warning().is_none());

        // The next real write overwrites the stale temp file instead of
        // appending to it.
        store.patch(json!({ "theme": "dark" })).expect("patch");
        store.flush().expect("flush");
        let reread: AppConfig =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("parse");
        assert_eq!(reread.theme, ThemePreference::Dark);
        assert_eq!(reread.locale, Locale::En);
    }

    #[test]
    fn an_unparseable_file_is_quarantined_and_reported_as_a_warning() {
        let dir = temp_dir();
        let path = dir.path().join("config.json");
        std::fs::write(&path, b"{ this is not json").expect("write");

        let store = ConfigStore::open_at(&path).expect("load must not fail");

        assert_eq!(store.get(), AppConfig::default());
        let warning = store.warning().expect("warning");
        assert_eq!(warning.key, "toast.config.quarantined");
        assert!(warning.backup.exists(), "the bad file is kept");
        assert!(warning
            .backup
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("config.json.bad-")));
        assert!(!path.exists(), "the bad file was moved, not copied");
    }

    #[test]
    fn unknown_keys_survive_a_load_save_round_trip() {
        let dir = temp_dir();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&json!({
                "locale": "en",
                "futureSection": { "keep": "me" },
                "terminal": { "fontSize": 9.0, "futureKnob": 7 }
            }))
            .expect("encode"),
        )
        .expect("write");

        let store = ConfigStore::open_at(&path).expect("load");
        store.patch(json!({ "theme": "light" })).expect("patch");
        store.flush().expect("flush");

        let raw: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("parse");
        assert_eq!(raw["futureSection"], json!({ "keep": "me" }));
        assert_eq!(raw["terminal"]["futureKnob"], json!(7));
        assert_eq!(raw["theme"], "light");
        assert_eq!(raw["locale"], "en");
    }

    #[test]
    fn a_patch_that_would_break_the_shape_is_rejected_and_changes_nothing() {
        let dir = temp_dir();
        let path = dir.path().join("config.json");
        let store = ConfigStore::open_at(&path).expect("load");

        let before = store.get();
        let err = store
            .patch(json!({ "terminal": { "fontSize": "huge" } }))
            .expect_err("must reject");

        assert!(matches!(err, GildeError::Config(_)));
        assert_eq!(store.get(), before);

        assert!(matches!(
            store.patch(json!([1, 2, 3])).expect_err("must reject"),
            GildeError::Config(_)
        ));
    }

    #[test]
    fn a_null_patch_resets_a_field_to_its_default() {
        let dir = temp_dir();
        let path = dir.path().join("config.json");
        let store = ConfigStore::open_at(&path).expect("load");

        store
            .patch(json!({ "agents": { "pathOverride": "/opt/bin" } }))
            .expect("patch");
        assert_eq!(store.get().agents.path_override.as_deref(), Some("/opt/bin"));

        store
            .patch(json!({ "agents": { "pathOverride": null } }))
            .expect("patch");
        assert_eq!(store.get().agents.path_override, None);
    }

    #[test]
    fn dropping_the_store_flushes_a_pending_write() {
        let dir = temp_dir();
        let path = dir.path().join("config.json");

        {
            let store = ConfigStore::open_at(&path).expect("load");
            store.patch(json!({ "theme": "dark" })).expect("patch");
            // Dropped inside the debounce window.
        }

        let on_disk: AppConfig =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("parse");
        assert_eq!(on_disk.theme, ThemePreference::Dark);
    }
}
