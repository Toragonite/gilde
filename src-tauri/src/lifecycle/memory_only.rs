//! Memory-only mode (§2.4).
//!
//! Owner: T09.
//!
//! `GILDE_HOME` read-only or the disk full at boot (or a durable write failing
//! later) → the app keeps running without persisting `session_meta`, the
//! history index, quit records or marks; one notice says so
//! (`notice.memoryOnly`, §9.12); **spawns are refused with `DISK_FULL`**,
//! because a spawn needs a durable registry row [R1 §2]. Writes resume when a
//! check every [`RECHECK_INTERVAL`] succeeds [A §10.1, A §10.7].
//!
//! The probe is a real durable write: a small file in `GILDE_HOME`, `fsync`ed
//! and removed. Anything else (`access(2)`, free-space arithmetic) says what
//! *should* work, not what does.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;

use crate::contract::common::Notice;

/// How often a memory-only app re-checks that it can write again.
pub const RECHECK_INTERVAL: Duration = Duration::from_secs(60);

/// The probe file's name inside `GILDE_HOME`.
const PROBE_FILE: &str = ".gilde-write-probe";

/// The notice key (§9.12).
pub const NOTICE_KEY: &str = "notice.memoryOnly";

/// Why state cannot be saved. The string forms are the `{reason}` parameter of
/// `notice.memoryOnly` and `BootStatus.memoryOnly`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MemoryOnlyReason {
    /// `EROFS`, `EACCES`, `EPERM`: the folder cannot be written.
    ReadOnly,
    /// `ENOSPC`, `EDQUOT`: no space left.
    DiskFull,
    /// Any other write failure.
    Unwritable,
}

impl MemoryOnlyReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            MemoryOnlyReason::ReadOnly => "readOnly",
            MemoryOnlyReason::DiskFull => "diskFull",
            MemoryOnlyReason::Unwritable => "unwritable",
        }
    }

    /// Classify a failed write.
    pub fn of_io(err: &std::io::Error) -> MemoryOnlyReason {
        match err.raw_os_error() {
            Some(libc::ENOSPC) | Some(libc::EDQUOT) => MemoryOnlyReason::DiskFull,
            Some(libc::EROFS) | Some(libc::EACCES) | Some(libc::EPERM) => MemoryOnlyReason::ReadOnly,
            _ if err.kind() == std::io::ErrorKind::PermissionDenied => MemoryOnlyReason::ReadOnly,
            _ => MemoryOnlyReason::Unwritable,
        }
    }

    /// Classify a SQLite failure text (`database or disk is full`,
    /// `attempt to write a readonly database`).
    pub fn of_db_message(msg: &str) -> Option<MemoryOnlyReason> {
        let m = msg.to_ascii_lowercase();
        if m.contains("disk is full") || m.contains("disk full") || m.contains("no space") {
            Some(MemoryOnlyReason::DiskFull)
        } else if m.contains("readonly") || m.contains("read-only") || m.contains("permission denied") {
            Some(MemoryOnlyReason::ReadOnly)
        } else {
            None
        }
    }
}

/// Write, `fsync` and remove a probe file in `dir`.
pub fn probe_writable(dir: &Path) -> Result<(), MemoryOnlyReason> {
    if let Err(e) = std::fs::create_dir_all(dir) {
        return Err(MemoryOnlyReason::of_io(&e));
    }
    let path = dir.join(PROBE_FILE);
    let result = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        file.write_all(b"gilde\n")?;
        file.sync_all()?;
        Ok(())
    })();
    let _ = std::fs::remove_file(&path);
    result.map_err(|e| MemoryOnlyReason::of_io(&e))
}

/// Observer of mode changes (the store's `Boot`/`Notices` ops).
pub type ModeListener = Arc<dyn Fn(Option<MemoryOnlyReason>) + Send + Sync>;

/// The process-wide memory-only state.
#[derive(Default)]
pub struct MemoryOnly {
    reason: RwLock<Option<MemoryOnlyReason>>,
    listeners: RwLock<Vec<ModeListener>>,
}

impl std::fmt::Debug for MemoryOnly {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryOnly").field("reason", &*self.reason.read()).finish()
    }
}

impl MemoryOnly {
    pub fn new() -> Arc<MemoryOnly> {
        Arc::new(MemoryOnly::default())
    }

    /// Enter memory-only mode. `true` when this call changed the mode.
    pub fn enter(&self, reason: MemoryOnlyReason) -> bool {
        let changed = {
            let mut cur = self.reason.write();
            let changed = cur.is_none();
            *cur = Some(reason);
            changed
        };
        if changed {
            tracing::warn!(target: "app", reason = reason.as_str(), "memory-only mode: state is not saved");
            self.notify(Some(reason));
        }
        changed
    }

    /// Leave memory-only mode (a re-check succeeded). `true` when it changed.
    pub fn leave(&self) -> bool {
        let changed = self.reason.write().take().is_some();
        if changed {
            tracing::info!(target: "app", "memory-only mode ended: writes resume");
            self.notify(None);
        }
        changed
    }

    pub fn reason(&self) -> Option<MemoryOnlyReason> {
        *self.reason.read()
    }

    pub fn is_active(&self) -> bool {
        self.reason.read().is_some()
    }

    /// The reason as the string `SupervisorApi::memory_only` and
    /// `BootStatus.memoryOnly` carry.
    pub fn reason_str(&self) -> Option<String> {
        self.reason().map(|r| r.as_str().to_string())
    }

    /// `notice.memoryOnly{reason}` while active.
    pub fn notice(&self) -> Option<Notice> {
        self.reason().map(|r| {
            let mut n = Notice::new(NOTICE_KEY);
            n.params
                .insert("reason".into(), serde_json::Value::String(r.as_str().into()));
            n
        })
    }

    pub fn subscribe(&self, listener: ModeListener) {
        self.listeners.write().push(listener);
    }

    fn notify(&self, reason: Option<MemoryOnlyReason>) {
        let listeners = self.listeners.read().clone();
        for l in listeners {
            l(reason);
        }
    }

    /// One check: probe `dir`; leave the mode on success, enter it on failure.
    /// Returns the mode after the check.
    pub fn check(&self, dir: &Path) -> Option<MemoryOnlyReason> {
        match probe_writable(dir) {
            Ok(()) => {
                self.leave();
                None
            }
            Err(reason) => {
                self.enter(reason);
                Some(reason)
            }
        }
    }
}

/// The 60 s re-check (§2.4). `on_resume` runs once each time writes resume
/// (the boot code re-opens `state.db` on disk there).
pub fn spawn_recheck(
    mode: Arc<MemoryOnly>,
    dir: PathBuf,
    interval: Duration,
    on_resume: Arc<dyn Fn() + Send + Sync>,
) -> tauri::async_runtime::JoinHandle<()> {
    tauri::async_runtime::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            if !mode.is_active() {
                continue;
            }
            let d = dir.clone();
            let ok = tokio::task::spawn_blocking(move || probe_writable(&d).is_ok())
                .await
                .unwrap_or(false);
            if ok {
                on_resume();
                mode.leave();
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn a_writable_dir_passes_and_leaves_no_probe_behind() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(probe_writable(dir.path()), Ok(()));
        assert!(!dir.path().join(PROBE_FILE).exists());
    }

    #[test]
    fn a_read_only_dir_is_read_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let r = probe_writable(dir.path());
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(r, Err(MemoryOnlyReason::ReadOnly));
    }

    #[test]
    fn errno_and_sqlite_messages_classify() {
        let full = std::io::Error::from_raw_os_error(libc::ENOSPC);
        assert_eq!(MemoryOnlyReason::of_io(&full), MemoryOnlyReason::DiskFull);
        let ro = std::io::Error::from_raw_os_error(libc::EROFS);
        assert_eq!(MemoryOnlyReason::of_io(&ro), MemoryOnlyReason::ReadOnly);
        assert_eq!(
            MemoryOnlyReason::of_db_message("database or disk is full"),
            Some(MemoryOnlyReason::DiskFull)
        );
        assert_eq!(
            MemoryOnlyReason::of_db_message("attempt to write a readonly database"),
            Some(MemoryOnlyReason::ReadOnly)
        );
        assert_eq!(MemoryOnlyReason::of_db_message("no such table"), None);
    }

    #[test]
    fn enter_and_leave_notify_once_and_carry_the_notice() {
        let mode = MemoryOnly::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        mode.subscribe(Arc::new(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        }));
        assert!(mode.enter(MemoryOnlyReason::DiskFull));
        assert!(!mode.enter(MemoryOnlyReason::DiskFull));
        assert_eq!(mode.reason_str().as_deref(), Some("diskFull"));
        let notice = mode.notice().unwrap();
        assert_eq!(notice.key, NOTICE_KEY);
        assert_eq!(notice.params["reason"], "diskFull");
        assert!(mode.leave());
        assert!(!mode.leave());
        assert!(mode.notice().is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
