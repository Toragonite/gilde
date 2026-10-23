//! `AppPaths` — every filesystem location the app touches, resolved once at boot.
//!
//! Layout (docs/DESIGN.md §10-1). Two roots, on purpose:
//!
//! ```text
//! <app-data>/                                   machine state, safe to delete wholesale
//!   config.json                                 user settings + projects + layout + account slots
//!   state.db  -wal  -shm                        rusqlite WAL
//!   instance.lock                               flock single-instance guard
//!   agent-signatures.json                       status / classification patterns (hot reload)
//!   cache/path.json                             resolved login-shell PATH + fingerprint
//!   sessions/<session_id>/raw.log               optional verbatim PTY bytes (default OFF)
//!   logs/gilde.log.YYYY-MM-DD                   tracing, daily rotation, 7 kept
//!   logs/sidecar.log.YYYY-MM-DD                 sidecar stderr
//!   runs/<run_id>.txt                           spill for run text over 4 MiB
//!   run/                                        sockets (PaneHost `ph.sock`, §4.6)      [Gilde 2]
//!   shim/<buildId>/tmux                         per-build gilde-tmux copy (§4.7)        [Gilde 2]
//!   tmp/vendorcopy/                             SQLite clones of vendor DBs (§5.7)      [Gilde 2]
//!   tmp/diff/<uuid>/                            IDE diff base files, 0400 (§9.4)        [Gilde 2]
//!   cache/canary/                               static canary results (§3.10)           [Gilde 2]
//!
//! ~/.gilde/                                     user-facing output, typed into a shell
//!   worktrees/<repo-slug>/<task-slug>/
//!   archive/<repo-slug>/<task-slug>-<ts>/
//!   dispatch/<task_id>/<run_id>/{prompt.md,result.json,stream.jsonl}
//!   aliases.json
//! ```
//!
//! On macOS `<app-data>` is
//! `~/Library/Application Support/com.toragonite.gildecode`.
//!
//! # `GILDE_HOME`
//!
//! [`AppPaths::resolve`] reads the `GILDE_HOME` environment variable **first**. When
//! set, it is treated as a stand-in `$HOME`: both roots (and the read-only
//! claude-code-orchestrator registry) are re-based under it, keeping the exact
//! same relative layout. That makes an isolated-HOME test one env var wide, and
//! it makes "which tree did this run touch?" answerable by looking at one path.

use std::path::{Path, PathBuf};

use crate::error::{GildeError, Result};

/// Environment variable that relocates every root. Must be an absolute path.
pub const GILDE_HOME_ENV: &str = "GILDE_HOME";

/// Bundle identifier — also the `<app-data>` directory name on macOS.
pub const APP_IDENTIFIER: &str = "com.toragonite.gildecode";

/// User-facing root under `$HOME` (DECISIONS: worktrees live outside the repo).
pub const USER_ROOT_DIR_NAME: &str = ".gilde";

/// Read-only compatibility source: `~/.claude-code-orchestrator/registry.json`.
pub const CCO_ROOT_DIR_NAME: &str = ".claude-code-orchestrator";

/// Directory mode for everything we create: owner-only.
#[cfg(unix)]
const DIR_MODE: u32 = 0o700;

/// All resolved roots. Cheap to clone, immutable after boot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    home_dir: PathBuf,
    app_data_dir: PathBuf,
    cache_dir: PathBuf,
    logs_dir: PathBuf,
    scrollback_dir: PathBuf,
    runs_dir: PathBuf,
    user_root: PathBuf,
    worktrees_root: PathBuf,
    archive_root: PathBuf,
    dispatch_root: PathBuf,
    run_dir: PathBuf,
    shim_root: PathBuf,
    tmp_vendorcopy_dir: PathBuf,
    tmp_diff_dir: PathBuf,
    canary_cache_dir: PathBuf,
}

impl AppPaths {
    /// Resolve every root. `GILDE_HOME` wins; otherwise Tauri's resolver provides
    /// `app_data_dir()` and `home_dir()`.
    pub fn resolve(app: &tauri::AppHandle) -> Result<Self> {
        if let Some(paths) = Self::from_env()? {
            return Ok(paths);
        }

        use tauri::Manager as _;
        let resolver = app.path();
        let app_data_dir = resolver
            .app_data_dir()
            .map_err(|e| GildeError::Config(format!("cannot resolve app data dir: {e}")))?;
        let home_dir = resolver
            .home_dir()
            .map_err(|e| GildeError::Config(format!("cannot resolve home dir: {e}")))?;

        Ok(Self::new(app_data_dir, home_dir))
    }

    /// `Some` when `GILDE_HOME` is set to a non-empty absolute path, `None` when it
    /// is unset or empty, `Err` when it is set to a relative path (a relative
    /// root would silently follow the process CWD around).
    pub fn from_env() -> Result<Option<Self>> {
        let raw = match std::env::var_os(GILDE_HOME_ENV) {
            Some(v) => v,
            None => return Ok(None),
        };
        if raw.is_empty() {
            return Ok(None);
        }
        let home = PathBuf::from(raw);
        if !home.is_absolute() {
            return Err(GildeError::Config(format!(
                "{GILDE_HOME_ENV} must be an absolute path, got {}",
                home.display()
            )));
        }
        Ok(Some(Self::for_home(&home)))
    }

    /// Derive both roots from a home directory, mirroring the production layout.
    /// This is what `GILDE_HOME` uses, and what tests use with a temp directory.
    pub fn for_home(home: &Path) -> Self {
        let app_data_dir = home
            .join("Library")
            .join("Application Support")
            .join(APP_IDENTIFIER);
        Self::new(app_data_dir, home.to_path_buf())
    }

    /// Build from an explicit `<app-data>` directory and home directory.
    pub fn new(app_data_dir: PathBuf, home_dir: PathBuf) -> Self {
        let user_root = home_dir.join(USER_ROOT_DIR_NAME);
        Self {
            run_dir: app_data_dir.join("run"),
            shim_root: app_data_dir.join("shim"),
            tmp_vendorcopy_dir: app_data_dir.join("tmp").join("vendorcopy"),
            tmp_diff_dir: app_data_dir.join("tmp").join("diff"),
            canary_cache_dir: app_data_dir.join("cache").join("canary"),
            cache_dir: app_data_dir.join("cache"),
            logs_dir: app_data_dir.join("logs"),
            scrollback_dir: app_data_dir.join("sessions"),
            runs_dir: app_data_dir.join("runs"),
            worktrees_root: user_root.join("worktrees"),
            archive_root: user_root.join("archive"),
            dispatch_root: user_root.join("dispatch"),
            user_root,
            app_data_dir,
            home_dir,
        }
    }

    // -- roots ----------------------------------------------------------------

    /// `~/Library/Application Support/com.toragonite.gildecode`
    pub fn app_data_dir(&self) -> &Path {
        &self.app_data_dir
    }

    /// The home directory every user-facing path hangs off.
    pub fn home_dir(&self) -> &Path {
        &self.home_dir
    }

    /// `~/.gilde`
    pub fn user_root(&self) -> &Path {
        &self.user_root
    }

    /// `<app-data>/cache`
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// `<app-data>/logs`
    pub fn logs_dir(&self) -> &Path {
        &self.logs_dir
    }

    /// `<app-data>/sessions` — per-session optional raw PTY logs (default OFF).
    pub fn scrollback_dir(&self) -> &Path {
        &self.scrollback_dir
    }

    /// `<app-data>/runs` — spill files for run text over 4 MiB.
    pub fn runs_dir(&self) -> &Path {
        &self.runs_dir
    }

    /// `~/.gilde/worktrees`
    pub fn worktrees_root(&self) -> &Path {
        &self.worktrees_root
    }

    /// `~/.gilde/archive`
    pub fn archive_root(&self) -> &Path {
        &self.archive_root
    }

    /// `~/.gilde/dispatch` — legacy (contract §3.9): never read or written by
    /// Gilde 2, no longer created at boot.
    pub fn dispatch_root(&self) -> &Path {
        &self.dispatch_root
    }

    /// `<app-data>/run` — sockets (the PaneHost socket, §4.6).
    pub fn run_dir(&self) -> &Path {
        &self.run_dir
    }

    /// `<app-data>/shim` — the per-build `gilde-tmux` copies live below it.
    pub fn shim_root(&self) -> &Path {
        &self.shim_root
    }

    /// `<app-data>/shim/<build_id>` (§4.7: a copy per build, not a symlink).
    pub fn shim_dir(&self, build_id: &str) -> PathBuf {
        self.shim_root.join(build_id)
    }

    /// `<app-data>/tmp/vendorcopy` — SQLite clones of vendor databases (§5.7).
    pub fn tmp_vendorcopy_dir(&self) -> &Path {
        &self.tmp_vendorcopy_dir
    }

    /// `<app-data>/tmp/diff` — IDE diff base files (§9.4).
    pub fn tmp_diff_dir(&self) -> &Path {
        &self.tmp_diff_dir
    }

    /// `<app-data>/cache/canary` — static canary results (§3.10).
    pub fn canary_cache_dir(&self) -> &Path {
        &self.canary_cache_dir
    }

    // -- files ----------------------------------------------------------------

    /// `<app-data>/state.db`
    pub fn state_db(&self) -> PathBuf {
        self.app_data_dir.join("state.db")
    }

    /// `<app-data>/config.json`
    pub fn config_file(&self) -> PathBuf {
        self.app_data_dir.join("config.json")
    }

    /// `<app-data>/agent-signatures.json`
    pub fn signatures_file(&self) -> PathBuf {
        self.app_data_dir.join("agent-signatures.json")
    }

    /// `<app-data>/cache/path.json`
    pub fn path_cache(&self) -> PathBuf {
        self.cache_dir.join("path.json")
    }

    /// `<app-data>/instance.lock`
    pub fn instance_lock(&self) -> PathBuf {
        self.app_data_dir.join("instance.lock")
    }

    /// `~/.gilde/aliases.json`
    pub fn aliases_file(&self) -> PathBuf {
        self.user_root.join("aliases.json")
    }

    /// `~/.claude-code-orchestrator/registry.json` — read-only compatibility source.
    pub fn cco_registry(&self) -> PathBuf {
        self.home_dir.join(CCO_ROOT_DIR_NAME).join("registry.json")
    }

    // -- derived, per-entity --------------------------------------------------

    /// `<app-data>/sessions/<session_id>`
    pub fn session_dir(&self, session_id: &str) -> PathBuf {
        self.scrollback_dir.join(session_id)
    }

    /// `<app-data>/sessions/<session_id>/raw.log`
    pub fn session_raw_log(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join("raw.log")
    }

    /// `<app-data>/runs/<run_id>.txt` — spill target for oversized run text.
    pub fn run_spill_file(&self, run_id: &str) -> PathBuf {
        self.runs_dir.join(format!("{run_id}.txt"))
    }

    /// `~/.gilde/dispatch/<task_id>`
    pub fn dispatch_task_dir(&self, task_id: &str) -> PathBuf {
        self.dispatch_root.join(task_id)
    }

    /// `~/.gilde/dispatch/<task_id>/<run_id>`
    pub fn dispatch_run_dir(&self, task_id: &str, run_id: &str) -> PathBuf {
        self.dispatch_task_dir(task_id).join(run_id)
    }

    // -- creation -------------------------------------------------------------

    /// Every directory this struct hands out, `create_dir_all`-ed and (on unix)
    /// narrowed to mode 0700. Idempotent; safe to call on every boot.
    pub fn ensure_all(&self) -> Result<()> {
        for dir in self.managed_dirs() {
            ensure_private_dir(dir)?;
        }
        Ok(())
    }

    /// The directories `ensure_all` creates, in creation order (parents first).
    pub fn managed_dirs(&self) -> Vec<&Path> {
        vec![
            self.app_data_dir.as_path(),
            self.cache_dir.as_path(),
            self.logs_dir.as_path(),
            self.scrollback_dir.as_path(),
            self.runs_dir.as_path(),
            self.user_root.as_path(),
            self.worktrees_root.as_path(),
            self.archive_root.as_path(),
            self.run_dir.as_path(),
            self.shim_root.as_path(),
            self.tmp_vendorcopy_dir.as_path(),
            self.tmp_diff_dir.as_path(),
            self.canary_cache_dir.as_path(),
        ]
    }
}

/// `create_dir_all` + owner-only permissions on the leaf.
///
/// The mode is applied after creation rather than through a umask dance: the
/// process umask belongs to whoever launched us, and these trees hold session
/// transcripts and dispatch prompts.
pub fn ensure_private_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(|e| {
        GildeError::Io(std::io::Error::new(
            e.kind(),
            format!("create_dir_all {}: {e}", dir.display()),
        ))
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let perms = std::fs::Permissions::from_mode(DIR_MODE);
        // A pre-existing directory the user deliberately opened up is not worth
        // failing a boot over; log-free best effort, then verify below.
        if let Err(e) = std::fs::set_permissions(dir, perms) {
            tracing::warn!(dir = %dir.display(), error = %e, "could not tighten directory permissions");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn temp_home() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn layout_matches_design_10_1() {
        let paths = AppPaths::for_home(Path::new("/home/tester"));

        assert_eq!(
            paths.app_data_dir(),
            Path::new("/home/tester/Library/Application Support/com.toragonite.gildecode")
        );
        assert_eq!(
            paths.state_db(),
            paths.app_data_dir().join("state.db"),
            "state.db lives in app data"
        );
        assert_eq!(paths.config_file(), paths.app_data_dir().join("config.json"));
        assert_eq!(
            paths.signatures_file(),
            paths.app_data_dir().join("agent-signatures.json")
        );
        assert_eq!(
            paths.path_cache(),
            paths.app_data_dir().join("cache").join("path.json")
        );
        assert_eq!(
            paths.instance_lock(),
            paths.app_data_dir().join("instance.lock")
        );
        assert_eq!(paths.logs_dir(), paths.app_data_dir().join("logs"));
        assert_eq!(paths.scrollback_dir(), paths.app_data_dir().join("sessions"));
        assert_eq!(paths.runs_dir(), paths.app_data_dir().join("runs"));

        assert_eq!(
            paths.worktrees_root(),
            Path::new("/home/tester/.gilde/worktrees")
        );
        assert_eq!(
            paths.archive_root(),
            Path::new("/home/tester/.gilde/archive")
        );
        assert_eq!(
            paths.dispatch_root(),
            Path::new("/home/tester/.gilde/dispatch")
        );
        assert_eq!(
            paths.aliases_file(),
            Path::new("/home/tester/.gilde/aliases.json")
        );
        assert_eq!(
            paths.cco_registry(),
            Path::new("/home/tester/.claude-code-orchestrator/registry.json")
        );
    }

    #[test]
    fn derived_paths_compose() {
        let paths = AppPaths::for_home(Path::new("/home/tester"));
        assert_eq!(
            paths.session_raw_log("s1"),
            paths.scrollback_dir().join("s1").join("raw.log")
        );
        assert_eq!(
            paths.run_spill_file("r1"),
            paths.runs_dir().join("r1.txt")
        );
        assert_eq!(
            paths.dispatch_run_dir("t1", "r1"),
            paths.dispatch_root().join("t1").join("r1")
        );
    }

    #[test]
    fn ensure_all_creates_every_directory_privately() {
        let home = temp_home();
        let paths = AppPaths::for_home(home.path());

        for dir in paths.managed_dirs() {
            assert!(!dir.exists(), "{} should not exist yet", dir.display());
        }

        paths.ensure_all().expect("ensure_all");

        for dir in paths.managed_dirs() {
            assert!(dir.is_dir(), "{} should be a directory", dir.display());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let mode = std::fs::metadata(dir).expect("metadata").permissions().mode();
                assert_eq!(mode & 0o777, DIR_MODE, "{} mode", dir.display());
            }
        }

        // Idempotent.
        paths.ensure_all().expect("ensure_all twice");
    }

    #[test]
    #[serial(gilde_home_env)]
    fn gilde_home_wins_and_relocates_both_roots() {
        let home = temp_home();
        // SAFETY: guarded by `serial(gilde_home_env)`, no other thread reads the env here.
        unsafe { std::env::set_var(GILDE_HOME_ENV, home.path()) };

        let paths = AppPaths::from_env()
            .expect("from_env")
            .expect("GILDE_HOME should produce paths");

        assert!(paths.app_data_dir().starts_with(home.path()));
        assert!(paths.worktrees_root().starts_with(home.path()));
        assert!(paths.cco_registry().starts_with(home.path()));
        assert_eq!(paths, AppPaths::for_home(home.path()));

        unsafe { std::env::remove_var(GILDE_HOME_ENV) };
    }

    #[test]
    #[serial(gilde_home_env)]
    fn unset_or_empty_gilde_home_defers_to_the_platform() {
        // SAFETY: guarded by `serial(gilde_home_env)`.
        unsafe { std::env::remove_var(GILDE_HOME_ENV) };
        assert!(AppPaths::from_env().expect("from_env").is_none());

        unsafe { std::env::set_var(GILDE_HOME_ENV, "") };
        assert!(AppPaths::from_env().expect("from_env").is_none());

        unsafe { std::env::remove_var(GILDE_HOME_ENV) };
    }

    #[test]
    #[serial(gilde_home_env)]
    fn relative_gilde_home_is_a_typed_config_error() {
        // SAFETY: guarded by `serial(gilde_home_env)`.
        unsafe { std::env::set_var(GILDE_HOME_ENV, "relative/path") };

        let err = AppPaths::from_env().expect_err("relative GILDE_HOME must fail");
        assert_eq!(err.code(), "CONFIG");

        unsafe { std::env::remove_var(GILDE_HOME_ENV) };
    }
}
