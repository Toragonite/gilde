//! Tracing setup and the panic hook.
//!
//! * JSON lines to `<app-data>/logs/gilde.log.YYYY-MM-DD`, daily rotation, 7 files kept.
//! * Level from `GILDE_LOG`, else `RUST_LOG`, else `info`.
//! * A human-readable mirror on stderr in debug builds (or with `GILDE_LOG_STDERR=1`),
//!   because a desktop app started from a terminal during development should say
//!   something.
//!
//! **Content never reaches the log** (contract §9.7): fields are an allowlist of
//! ids, counts, kinds, durations, error codes, row types and byte counts. The
//! [`ContentGuard`] layer counts every event that carries a content-named
//! field ([`CONTENT_FIELDS`]); tests assert the count stays zero, and debug
//! builds say which field it was. An argv is logged only as
//! [`redacted_argv`] (the one helper of `supervise::redact`); an `IpcError`
//! only as its code, key and detail length (§4.4).
//!
//! `panic = "abort"` is deliberately absent from `[profile.release]` (DESIGN §2-2):
//! this app runs PTY reader threads, a watchdog, and user-editable regexes, and one
//! thread panicking must not take the process — and every open PTY — down with it.
//! [`install_panic_hook`] makes those panics loud in the log instead of silent.

use std::panic::PanicHookInfo;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{Builder as RollingBuilder, Rotation};
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::EnvFilter;

use crate::error::{GildeError, Result};
use crate::paths::AppPaths;

/// Log file prefix — files land as `gilde.log.YYYY-MM-DD`.
pub const LOG_FILE_PREFIX: &str = "gilde.log";

/// How many rotated files to keep.
pub const LOG_FILES_KEPT: usize = 7;

/// Preferred level env var; falls back to `RUST_LOG`, then `info`.
pub const LOG_ENV: &str = "GILDE_LOG";

/// Set to `1` to mirror logs on stderr in a release build.
pub const LOG_STDERR_ENV: &str = "GILDE_LOG_STDERR";

static INITIALIZED: OnceLock<()> = OnceLock::new();

/// Install the global subscriber.
///
/// The returned [`WorkerGuard`] must be kept alive for the lifetime of the
/// process — dropping it flushes and then stops the writer thread. `lib.rs`
/// parks it in Tauri managed state.
///
/// Calling this twice in one process is not an error: the second call logs a
/// warning and returns a guard for a writer nothing is subscribed to, which
/// keeps tests and hot-restart paths from panicking.
pub fn init_tracing(paths: &AppPaths) -> Result<WorkerGuard> {
    crate::paths::ensure_private_dir(paths.logs_dir())?;

    let appender = RollingBuilder::new()
        .rotation(Rotation::DAILY)
        .filename_prefix(LOG_FILE_PREFIX)
        .max_log_files(LOG_FILES_KEPT)
        .build(paths.logs_dir())
        .map_err(|e| GildeError::Internal(format!("cannot open log directory: {e}")))?;

    let (writer, guard) = tracing_appender::non_blocking(appender);

    if INITIALIZED.set(()).is_err() {
        tracing::warn!("init_tracing called more than once; keeping the first subscriber");
        return Ok(guard);
    }

    let file_layer = tracing_subscriber::fmt::layer()
        .json()
        .flatten_event(true)
        .with_current_span(true)
        .with_span_list(false)
        .with_target(true)
        .with_thread_names(true)
        .with_ansi(false)
        .with_writer(writer);

    let stderr_layer = if stderr_mirror_enabled() {
        Some(
            tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_ansi(true)
                .with_writer(std::io::stderr),
        )
    } else {
        None
    };

    tracing_subscriber::registry()
        .with(env_filter())
        .with(ContentGuard)
        .with(file_layer)
        .with(stderr_layer)
        .init();

    Ok(guard)
}

/// Field names that are content or secrets (contract §3.9, §9.7): never a
/// log field.
pub const CONTENT_FIELDS: &[&str] = &[
    "text", "prompt", "content", "title", "body", "preview", "summary", "subject", "blocks",
    "raw", "token", "secret", "argv", "command", "env", "detail", "stderr", "stdout",
];

static CONTENT_VIOLATIONS: AtomicU64 = AtomicU64::new(0);

/// How many events carried a content-named field since start.
pub fn content_violations() -> u64 {
    CONTENT_VIOLATIONS.load(Ordering::SeqCst)
}

/// The tracing layer that watches the field allowlist (see the module docs).
pub struct ContentGuard;

struct FieldNames(Option<&'static str>);

impl tracing::field::Visit for FieldNames {
    fn record_debug(&mut self, field: &tracing::field::Field, _value: &dyn std::fmt::Debug) {
        if self.0.is_none() && is_content_field(field.name()) {
            self.0 = Some(field.name());
        }
    }
}

/// Whether a field name is a content name: the name itself or its last
/// `.`/`_` part (`user.prompt`, `first_prompt`); `detail_len` (a byte count)
/// is not.
pub fn is_content_field(name: &str) -> bool {
    let last = name.rsplit(['.', '_']).next().unwrap_or(name);
    CONTENT_FIELDS.contains(&name) || CONTENT_FIELDS.contains(&last)
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for ContentGuard {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        let mut names = FieldNames(None);
        event.record(&mut names);
        if let Some(field) = names.0 {
            CONTENT_VIOLATIONS.fetch_add(1, Ordering::SeqCst);
            if cfg!(debug_assertions) {
                eprintln!("gilde-code: a log event carried the content field `{field}` (contract §9.7)");
            }
        }
    }
}

/// An argv in the only form a log line may carry (§9.7).
pub fn redacted_argv(argv: &[String]) -> String {
    crate::supervise::redact::redact_display(argv)
}

/// `GILDE_LOG` → `RUST_LOG` → `info`. An unparsable value falls back to `info`
/// rather than failing the boot.
fn env_filter() -> EnvFilter {
    for var in [LOG_ENV, "RUST_LOG"] {
        if let Ok(value) = std::env::var(var) {
            if value.trim().is_empty() {
                continue;
            }
            match EnvFilter::try_new(&value) {
                Ok(filter) => return filter,
                Err(e) => {
                    eprintln!("gilde-code: ignoring invalid {var}={value:?} ({e})");
                }
            }
        }
    }
    EnvFilter::new("info")
}

fn stderr_mirror_enabled() -> bool {
    match std::env::var(LOG_STDERR_ENV) {
        Ok(v) => v != "0" && !v.is_empty(),
        Err(_) => cfg!(debug_assertions),
    }
}

/// Route panics into the log (payload, thread, location, backtrace) and then let
/// the previous hook run so a terminal-attached dev build still sees it.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info: &PanicHookInfo<'_>| {
        let payload = panic_message(info);
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".to_string());
        let thread = std::thread::current();
        let thread_name = thread.name().unwrap_or("<unnamed>").to_string();
        let backtrace = std::backtrace::Backtrace::force_capture().to_string();

        tracing::error!(
            panic.payload = %payload,
            panic.location = %location,
            panic.thread = %thread_name,
            panic.backtrace = %backtrace,
            "thread panicked"
        );

        previous(info);
    }));
}

fn panic_message(info: &PanicHookInfo<'_>) -> String {
    let payload = info.payload();
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusty_fork::rusty_fork_test;

    #[test]
    fn content_field_names() {
        assert!(is_content_field("prompt"));
        assert!(is_content_field("first_prompt"));
        assert!(is_content_field("argv"));
        assert!(!is_content_field("spawn_id"));
        assert!(!is_content_field("count"));
        assert!(!is_content_field("detail_len"));
        let line = redacted_argv(&["/x/claude".into(), "-p".into(), "secret plan".into()]);
        assert!(!line.contains("secret plan"));
    }

    rusty_fork_test! {
        #[test]
        fn the_content_guard_counts_content_fields() {
            let home = tempfile::tempdir().expect("tempdir");
            let paths = AppPaths::for_home(home.path());
            paths.ensure_all().expect("ensure_all");
            unsafe { std::env::set_var(LOG_STDERR_ENV, "0") };
            let guard = init_tracing(&paths).expect("init_tracing");
            let before = content_violations();
            tracing::info!(spawn_id = "x", count = 3, "fine");
            assert_eq!(content_violations(), before);
            tracing::info!(prompt = "do not log me", "bad");
            assert_eq!(content_violations(), before + 1);
            drop(guard);
        }
    }

    rusty_fork_test! {
        #[test]
        fn writes_json_lines_to_a_dated_file_and_honours_gilde_log() {
            let home = tempfile::tempdir().expect("tempdir");
            let paths = AppPaths::for_home(home.path());
            paths.ensure_all().expect("ensure_all");

            // SAFETY: rusty_fork gives this test its own process.
            unsafe { std::env::set_var(LOG_ENV, "warn") };
            unsafe { std::env::set_var(LOG_STDERR_ENV, "0") };

            let guard = init_tracing(&paths).expect("init_tracing");
            tracing::info!(marker = "should_be_filtered", "info line");
            tracing::warn!(marker = "gilde_test_marker", count = 3, "warn line");
            drop(guard);

            let mut files: Vec<_> = std::fs::read_dir(paths.logs_dir())
                .expect("read logs dir")
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .collect();
            files.sort();
            assert_eq!(files.len(), 1, "expected exactly one log file: {files:?}");

            let name = files[0].file_name().unwrap().to_string_lossy().to_string();
            assert!(
                name.starts_with("gilde.log."),
                "log file must be gilde.log.YYYY-MM-DD, got {name}"
            );
            let date = name.trim_start_matches("gilde.log.");
            assert_eq!(date.len(), 10, "date suffix should be YYYY-MM-DD, got {date}");
            assert_eq!(date.matches('-').count(), 2, "got {date}");

            let body = std::fs::read_to_string(&files[0]).expect("read log");
            let lines: Vec<&str> = body.lines().filter(|l| !l.trim().is_empty()).collect();
            assert_eq!(lines.len(), 1, "GILDE_LOG=warn must filter the info line: {body}");

            let value: serde_json::Value =
                serde_json::from_str(lines[0]).expect("each line is JSON");
            assert_eq!(value["level"], "WARN");
            assert_eq!(value["marker"], "gilde_test_marker");
            assert_eq!(value["count"], 3);
            assert_eq!(value["message"], "warn line");
        }
    }

    rusty_fork_test! {
        #[test]
        fn level_comes_from_gilde_log_then_rust_log_then_info() {
            // SAFETY: rusty_fork gives this test its own process.
            unsafe { std::env::remove_var(LOG_ENV) };
            unsafe { std::env::remove_var("RUST_LOG") };
            assert_eq!(env_filter().to_string(), "info", "default");

            unsafe { std::env::set_var("RUST_LOG", "debug") };
            assert_eq!(env_filter().to_string(), "debug", "RUST_LOG is honoured");

            unsafe { std::env::set_var(LOG_ENV, "trace") };
            assert_eq!(env_filter().to_string(), "trace", "GILDE_LOG wins over RUST_LOG");

            // An unparsable value must not fail the boot; it falls through.
            unsafe { std::env::set_var(LOG_ENV, "=!not a filter=") };
            assert_eq!(env_filter().to_string(), "debug", "falls back to RUST_LOG");

            unsafe { std::env::remove_var("RUST_LOG") };
            assert_eq!(env_filter().to_string(), "info", "falls back to info");
        }
    }

    rusty_fork_test! {
        #[test]
        fn daily_rotation_keeps_seven_files() {
            let home = tempfile::tempdir().expect("tempdir");
            let paths = AppPaths::for_home(home.path());
            paths.ensure_all().expect("ensure_all");

            // SAFETY: rusty_fork gives this test its own process.
            unsafe { std::env::set_var(LOG_ENV, "info") };
            unsafe { std::env::set_var(LOG_STDERR_ENV, "0") };

            for day in 1..=9 {
                let stale = paths.logs_dir().join(format!("{LOG_FILE_PREFIX}.2020-01-0{day}"));
                std::fs::write(&stale, b"stale\n").expect("seed stale log");
            }
            assert_eq!(count_logs(&paths), 9);

            let guard = init_tracing(&paths).expect("init_tracing");
            tracing::info!("today");
            drop(guard);

            assert_eq!(
                count_logs(&paths),
                LOG_FILES_KEPT,
                "the appender must prune down to {LOG_FILES_KEPT} files"
            );
        }
    }

    fn count_logs(paths: &AppPaths) -> usize {
        std::fs::read_dir(paths.logs_dir())
            .expect("read logs dir")
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(LOG_FILE_PREFIX)
            })
            .count()
    }

    rusty_fork_test! {
        #[test]
        fn a_panicking_thread_is_logged_and_does_not_kill_the_process() {
            let home = tempfile::tempdir().expect("tempdir");
            let paths = AppPaths::for_home(home.path());
            paths.ensure_all().expect("ensure_all");

            // SAFETY: rusty_fork gives this test its own process.
            unsafe { std::env::set_var(LOG_ENV, "error") };
            unsafe { std::env::set_var(LOG_STDERR_ENV, "0") };

            let guard = init_tracing(&paths).expect("init_tracing");
            install_panic_hook();

            let handle = std::thread::Builder::new()
                .name("gilde-test-panicker".into())
                .spawn(|| panic!("gilde_panic_marker"))
                .expect("spawn");
            assert!(handle.join().is_err(), "the thread must have panicked");

            // The whole point: the process is still here and still works.
            let sum: u32 = (1..=10).sum();
            assert_eq!(sum, 55);
            drop(guard);

            let file = std::fs::read_dir(paths.logs_dir())
                .expect("read logs dir")
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .next()
                .expect("a log file");
            let body = std::fs::read_to_string(file).expect("read log");

            assert!(body.contains("gilde_panic_marker"), "payload missing: {body}");
            assert!(body.contains("gilde-test-panicker"), "thread name missing: {body}");
            assert!(
                body.contains("panic.backtrace"),
                "backtrace field missing: {body}"
            );
            assert!(body.contains("log.rs"), "panic location missing: {body}");
        }
    }
}
