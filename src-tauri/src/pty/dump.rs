//! `GILDE_PTY_DUMP` — the app's own pty traffic, in the replay harness's format
//! (PINNED §36).
//!
//! ## Why this exists
//!
//! PINNED §35 fixed `/model` and `/help` against a capture taken by a *harness*
//! — a python `pty`, and later `tests/status_streams.rs`, standing in for the
//! WebView. The fix was real and the recording was faithful to what that harness
//! saw, and the packaged app was still wrong, which leaves exactly one
//! possibility: **the stream the app reads is not the stream the harness
//! recorded**. There is no way to reason that difference out from the outside.
//! So the app records its own, through the same reader thread the status engine
//! reads, in the same `GCAP1` framing `tests/fixtures/streams/*.cap` already
//! uses — and a dump taken from the running app replays byte-for-byte through
//! the same test harness as a fixture.
//!
//! What the first dump showed was not a terminal-query dialogue at all: the app
//! was resizing the pty every two seconds and reading the child's `SIGWINCH`
//! repaint as work. That is why the `resize` marker is in here — the difference
//! turned out to be an **app action**, not a byte the harness failed to send.
//!
//! ## The format (see `tests/fixtures/streams/README.md`)
//!
//! ```text
//! "GCAP1\n", then records of
//!     u8   kind    0 = pty output, 1 = marker, 2 = pty input
//!     u32  ms      milliseconds since this session's first record
//!     u32  len
//!     len  bytes
//! ```
//!
//! Kind 2 is what §36 added: **what the frontend wrote back**. A recording of
//! one direction cannot show a dialogue, and "xterm.js keeps answering queries
//! and claude keeps re-rendering" was the leading hypothesis. It was wrong —
//! 140 bytes in 830 seconds — and it took kind-2 records to say so instead of
//! arguing about it. Older captures have none and replay exactly as before.
//!
//! Markers are plain text. `open <cols>x<rows>` and `resize <cols>x<rows>` say
//! what the child was told about the window; `cpu <pct>` is the `ps -o %cpu` of
//! the session's foreground group at the tick the status engine sampled it,
//! because half of §35's bug was a decaying CPU average and a byte-only replay
//! cannot reproduce it.
//!
//! ## Cost when it is off
//!
//! One [`OnceLock`] read and a null check. The environment is read once, the
//! marker helpers take [`std::fmt::Arguments`] so a disabled marker formats
//! nothing, and no file is opened until a session actually writes.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

use dashmap::DashMap;
use parking_lot::Mutex;

use crate::store::now_ms;

/// Set to a directory to record every session's pty traffic into it.
pub const DUMP_ENV: &str = "GILDE_PTY_DUMP";

/// `GCAP1` record kinds.
pub const KIND_OUTPUT: u8 = 0;
pub const KIND_MARKER: u8 = 1;
pub const KIND_INPUT: u8 = 2;

/// The magic the fixtures and the replay harness agree on.
pub const MAGIC: &[u8] = b"GCAP1\n";

/// The dump directory, resolved once.
///
/// A directory that cannot be created disables the feature with one line on
/// stderr rather than failing the boot: this is a diagnostic, and a diagnostic
/// that stops the app from starting is worse than no diagnostic.
fn dump_dir() -> Option<&'static PathBuf> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        let value = std::env::var(DUMP_ENV).ok()?;
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return None;
        }
        let path = PathBuf::from(trimmed);
        if let Err(err) = std::fs::create_dir_all(&path) {
            eprintln!(
                "gilde-code: {DUMP_ENV}={} is unusable ({err}); pty dumping is off",
                path.display()
            );
            return None;
        }
        eprintln!("gilde-code: dumping pty streams to {}", path.display());
        Some(path)
    })
    .as_ref()
}

/// True when `GILDE_PTY_DUMP` named a usable directory.
#[inline]
pub fn enabled() -> bool {
    dump_dir().is_some()
}

/// One open `.cap` file and the instant its clock starts from.
struct SessionDump {
    file: Option<File>,
    started_at: i64,
}

fn files() -> &'static DashMap<String, Mutex<SessionDump>> {
    static FILES: OnceLock<DashMap<String, Mutex<SessionDump>>> = OnceLock::new();
    FILES.get_or_init(DashMap::new)
}

/// Session ids are ULIDs, but a file name is not the place to trust that.
fn safe_name(session_id: &str) -> String {
    session_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Append one record. Silent on any I/O failure after the first — a diagnostic
/// never becomes the reason a session stops working.
fn record(session_id: &str, kind: u8, payload: &[u8]) {
    let Some(dir) = dump_dir() else { return };
    let now = now_ms();

    let entry = files().entry(session_id.to_string()).or_insert_with(|| {
        let path = dir.join(format!("{}.cap", safe_name(session_id)));
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| {
                // A fresh file gets the magic; an appended-to one already has it.
                if f.metadata().map(|m| m.len()).unwrap_or(0) == 0 {
                    f.write_all(MAGIC)?;
                }
                Ok(f)
            });
        let file = match file {
            Ok(file) => Some(file),
            Err(err) => {
                eprintln!("gilde-code: cannot dump to {}: {err}", path.display());
                None
            }
        };
        Mutex::new(SessionDump {
            file,
            started_at: now,
        })
    });

    let mut dump = entry.lock();
    let ms = (now - dump.started_at).clamp(0, u32::MAX as i64) as u32;
    let Some(file) = dump.file.as_mut() else {
        return;
    };
    // One `write_all` of the whole record: a SIGTERM between the header and the
    // payload would leave a file the parser cannot read past.
    let mut framed = Vec::with_capacity(9 + payload.len());
    framed.push(kind);
    framed.extend_from_slice(&ms.to_le_bytes());
    framed.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    framed.extend_from_slice(payload);
    if file.write_all(&framed).is_err() {
        dump.file = None;
    }
}

/// Bytes the child wrote, exactly as the reader thread received them.
#[inline]
pub fn output(session_id: &str, bytes: &[u8]) {
    if !enabled() {
        return;
    }
    record(session_id, KIND_OUTPUT, bytes);
}

/// Bytes the frontend wrote to the child — keystrokes, pastes, and the terminal
/// query replies xterm.js sends on its own.
#[inline]
pub fn input(session_id: &str, bytes: &[u8]) {
    if !enabled() {
        return;
    }
    record(session_id, KIND_INPUT, bytes);
}

/// A labelled moment. Takes [`std::fmt::Arguments`] so that a disabled dump
/// formats nothing at all.
#[inline]
pub fn marker(session_id: &str, args: std::fmt::Arguments<'_>) {
    if !enabled() {
        return;
    }
    record(session_id, KIND_MARKER, args.to_string().as_bytes());
}

/// Close a session's file so a `SIGTERM` right after does not lose the tail.
pub fn close(session_id: &str) {
    if !enabled() {
        return;
    }
    if let Some((_, entry)) = files().remove(session_id) {
        if let Some(file) = entry.lock().file.as_mut() {
            let _ = file.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_id_becomes_a_file_name_that_cannot_escape_the_directory() {
        assert_eq!(safe_name("01JQ-ab_CD"), "01JQ-ab_CD");
        assert_eq!(safe_name("../../etc/passwd"), "______etc_passwd");
        assert_eq!(safe_name("a b"), "a_b");
    }

    #[test]
    fn dumping_is_off_unless_the_environment_names_a_directory() {
        // The process running the test suite has no `GILDE_PTY_DUMP`, and the
        // resolution is a `OnceLock` — so this also pins that every helper is a
        // null check in that case.
        assert!(!enabled(), "GILDE_PTY_DUMP must be opt-in");
        output("s1", b"x");
        input("s1", b"y");
        marker("s1", format_args!("cpu {:.1}", 1.0));
        close("s1");
        assert_eq!(files().len(), 0, "nothing is opened when it is off");
    }
}
