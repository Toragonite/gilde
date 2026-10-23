//! The core wakeup counter file (contract §11.1, A9.1, F-34).
//!
//! Owner: T09 (T09b). Only in builds with feature `wakeup-count` (never a
//! release: the release-profile check asserts its absence like `probe-log`)
//! and only when `GILDE_WAKEUP_COUNT_FILE` holds an **absolute** path —
//! relative, empty or unset means no file and one warn line. The core counts
//! every worker-thread unpark of its async runtime (a tokio runtime built
//! with an `on_thread_unpark` hook and installed as Tauri's runtime) and, at
//! most every [`WRITE_EVERY`] and once at a clean exit, writes
//! `{"v":1,"wakeups":<u64 since process start>,"monotonicMs":<u64 ms since
//! process start>}` + `\n` to a temporary file in the same directory, then
//! renames it over the target, so a reader never sees a partial file. A write
//! failure is counted and logged once; it is never retried in a loop and
//! never fatal. T19's perf harness reads exactly this format.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// The environment variable naming the counter file.
pub const ENV_FILE: &str = "GILDE_WAKEUP_COUNT_FILE";

/// The file is rewritten at most this often (and once at exit).
pub const WRITE_EVERY: Duration = Duration::from_secs(5);

/// Counts runtime wakeups and writes the counter file.
#[derive(Debug)]
pub struct WakeupCounter {
    wakeups: AtomicU64,
    started: Instant,
    target: PathBuf,
    failures: AtomicU64,
    warned: AtomicBool,
}

impl WakeupCounter {
    /// A counter writing to `target` (an absolute path, see [`target_from`]).
    pub fn new(target: PathBuf) -> Arc<WakeupCounter> {
        Arc::new(WakeupCounter {
            wakeups: AtomicU64::new(0),
            started: Instant::now(),
            target,
            failures: AtomicU64::new(0),
            warned: AtomicBool::new(false),
        })
    }

    /// One worker-thread unpark (the runtime hook; must stay this cheap).
    pub fn note_unpark(&self) {
        self.wakeups.fetch_add(1, Ordering::Relaxed);
    }

    pub fn wakeups(&self) -> u64 {
        self.wakeups.load(Ordering::Relaxed)
    }

    /// Writes that failed so far.
    pub fn failures(&self) -> u64 {
        self.failures.load(Ordering::Relaxed)
    }

    pub fn target(&self) -> &Path {
        &self.target
    }

    /// The file's one line, `\n`-terminated.
    pub fn line(&self) -> String {
        let ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        format!("{{\"v\":1,\"wakeups\":{},\"monotonicMs\":{}}}\n", self.wakeups(), ms)
    }

    /// Write the file now: a temporary file beside the target, then a rename
    /// over it. `false` on failure (counted; the first one is logged).
    pub fn write_now(&self) -> bool {
        match write_atomic(&self.target, self.line().as_bytes()) {
            Ok(()) => true,
            Err(err) => {
                self.failures.fetch_add(1, Ordering::Relaxed);
                if !self.warned.swap(true, Ordering::Relaxed) {
                    tracing::warn!(target: "perf", error = %err, "the wakeup counter file could not be written (logged once)");
                }
                false
            }
        }
    }
}

fn write_atomic(target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let dir = target.parent().ok_or_else(|| std::io::Error::other("no parent directory"))?;
    let name = target.file_name().ok_or_else(|| std::io::Error::other("no file name"))?;
    let tmp = dir.join(format!(".{}.{}.tmp", name.to_string_lossy(), std::process::id()));
    let result = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, target)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// The counter file named by the environment value, or why there is none
/// (`unset`, `empty`, `relative`): only an absolute path is accepted.
pub fn target_from(value: Option<&std::ffi::OsStr>) -> Result<PathBuf, &'static str> {
    let Some(value) = value else { return Err("unset") };
    if value.is_empty() {
        return Err("empty");
    }
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err("relative");
    }
    Ok(path)
}

/// A multi-thread tokio runtime whose every worker unpark is counted.
pub fn counting_runtime(counter: Arc<WakeupCounter>) -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("gilde-async")
        .on_thread_unpark(move || counter.note_unpark())
        .build()
}

static INSTALLED: OnceLock<Arc<WakeupCounter>> = OnceLock::new();

/// Build the counting runtime, install it as Tauri's async runtime and start
/// the writer (feature `wakeup-count` only; call first thing in `run()`,
/// before anything uses the async runtime). `None` = no counter: the feature
/// is off (nothing is read, nothing is written), or the variable is not an
/// absolute path (one warn line).
pub fn install() -> Option<Arc<WakeupCounter>> {
    if !cfg!(feature = "wakeup-count") {
        return None;
    }
    let target = match target_from(std::env::var_os(ENV_FILE).as_deref()) {
        Ok(t) => t,
        Err(why) => {
            tracing::warn!(target: "perf", reason = why, "{ENV_FILE} is not an absolute path; no wakeup counter file");
            return None;
        }
    };
    let counter = WakeupCounter::new(target);
    let runtime = match counting_runtime(counter.clone()) {
        Ok(rt) => rt,
        Err(err) => {
            tracing::warn!(target: "perf", error = %err, "the counting runtime could not be built; no wakeup counter");
            return None;
        }
    };
    tauri::async_runtime::set(runtime.handle().clone());
    // The runtime lives as long as the process (Tauri holds only a handle).
    std::mem::forget(runtime);
    let writer = Arc::downgrade(&counter);
    let spawned = std::thread::Builder::new().name("gilde-wakeups".into()).spawn(move || loop {
        std::thread::sleep(WRITE_EVERY);
        let Some(c) = writer.upgrade() else { return };
        c.write_now();
    });
    if let Err(err) = spawned {
        tracing::warn!(target: "perf", error = %err, "the wakeup writer did not start; written at exit only");
    }
    let _ = INSTALLED.set(counter.clone());
    Some(counter)
}

/// The clean-exit write (no-op without an installed counter).
pub fn write_at_exit() {
    if let Some(c) = INSTALLED.get() {
        c.write_now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_json(path: &Path) -> serde_json::Value {
        let text = std::fs::read_to_string(path).expect("read");
        assert!(text.ends_with('\n'), "one line, newline-terminated: {text:?}");
        assert_eq!(text.matches('\n').count(), 1);
        serde_json::from_str(text.trim_end()).expect("json")
    }

    /// T-perf-wakeupFile: the format.
    #[test]
    fn t_perf_wakeup_file_format() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wakeups.json");
        let c = WakeupCounter::new(target.clone());
        for _ in 0..3 {
            c.note_unpark();
        }
        assert!(c.write_now());
        let v = read_json(&target);
        assert_eq!(v["v"], 1);
        assert_eq!(v["wakeups"], 3);
        assert!(v["monotonicMs"].is_u64());
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(keys.len(), 3, "exactly v, wakeups, monotonicMs: {keys:?}");
    }

    /// T-perf-wakeupFile: the rename is atomic — the target is always a
    /// whole line, and no temporary file is left beside it.
    #[test]
    fn t_perf_wakeup_file_is_replaced_by_rename() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wakeups.json");
        let c = WakeupCounter::new(target.clone());
        assert!(c.write_now());
        let ino_before = std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(&target).unwrap());
        c.note_unpark();
        assert!(c.write_now());
        let ino_after = std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(&target).unwrap());
        assert_ne!(ino_before, ino_after, "a new file renamed over the old one, not rewritten in place");
        assert_eq!(read_json(&target)["wakeups"], 1);
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["wakeups.json".to_string()], "no temporary file left");
    }

    /// T-perf-wakeupFile: only an absolute path names a file.
    #[test]
    fn t_perf_wakeup_file_needs_an_absolute_path() {
        use std::ffi::OsStr;
        assert_eq!(target_from(None), Err("unset"));
        assert_eq!(target_from(Some(OsStr::new(""))), Err("empty"));
        assert_eq!(target_from(Some(OsStr::new("rel/wakeups.json"))), Err("relative"));
        assert_eq!(target_from(Some(OsStr::new("./wakeups.json"))), Err("relative"));
        assert_eq!(
            target_from(Some(OsStr::new("/tmp/wakeups.json"))),
            Ok(PathBuf::from("/tmp/wakeups.json"))
        );
    }

    /// T-perf-wakeupFile: without the feature nothing is read or written,
    /// even with the variable set to an absolute path.
    #[cfg(not(feature = "wakeup-count"))]
    #[test]
    fn t_perf_wakeup_file_absent_without_the_feature() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wakeups.json");
        std::env::set_var(ENV_FILE, &target);
        assert!(install().is_none());
        std::env::remove_var(ENV_FILE);
        write_at_exit();
        assert!(!target.exists(), "no counter file without the feature");
    }

    /// A failed write is counted and never panics or loops.
    #[test]
    fn a_failed_write_is_counted_not_fatal() {
        let c = WakeupCounter::new(PathBuf::from("/nonexistent-gilde-dir/wakeups.json"));
        assert!(!c.write_now());
        assert!(!c.write_now());
        assert_eq!(c.failures(), 2);
    }

    /// The runtime hook counts worker unparks.
    #[test]
    fn the_counting_runtime_counts_unparks() {
        let dir = tempfile::tempdir().unwrap();
        let c = WakeupCounter::new(dir.path().join("w.json"));
        let rt = counting_runtime(c.clone()).expect("runtime");
        rt.block_on(async {
            for _ in 0..8 {
                tokio::spawn(async { tokio::time::sleep(Duration::from_millis(2)).await })
                    .await
                    .unwrap();
            }
        });
        drop(rt);
        assert!(c.wakeups() > 0, "worker threads were unparked");
    }
}
