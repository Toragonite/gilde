//! Watch scope (contract §5.9, PM-07): never a Claude config dir root
//! recursively (25,700 `session-env` dirs [R2 Q6]), never `<cfg>/ide`, never
//! agy `log/`; a change under `session-env/` wakes nothing.
//!
//! A50.3: the test synchronizes on events, not time (A10.7). FSEvents
//! delivers in event-id order, so a marker file written into a watched root
//! (`<cfg>/sessions/`) and seen by the stream proves that every event of
//! what was done before it has been delivered: one marker after the setup
//! (the creation of the dirs below may arrive after the stream started, under
//! load), one after the writes the test asserts on.

#[path = "fixtures/vendor/support.rs"]
mod support;

use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use gilde_code_lib::discovery::fsevents::{FsSignal, FsWatch};
use gilde_code_lib::discovery::Engine;
use support::*;

type Signals = Arc<(Mutex<Vec<FsSignal>>, Condvar)>;

fn names(s: &FsSignal) -> Option<&str> {
    match s {
        FsSignal::Changed { path, .. } => path.file_name().and_then(|n| n.to_str()),
        FsSignal::Rescan { .. } => None,
    }
}

/// Write the marker `name` into the watched `sessions/` root and wait until
/// the stream reports it. The deadline only keeps a broken stream from
/// hanging the test; it is never a timing budget.
fn barrier(sessions: &Path, got: &Signals, name: &str) {
    std::fs::write(sessions.join(name), "{}").unwrap();
    let (lock, cv) = &**got;
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut g = lock.lock().unwrap();
    while !g.iter().any(|s| names(s) == Some(name)) {
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "the stream never reported the marker {name}: {:?}", *g);
        g = cv.wait_timeout(g, left).unwrap().0;
    }
}

#[test]
fn session_env_and_ide_are_never_watched() {
    let h = Home::new();
    let env_root = h.claude.join("session-env");
    for i in 0..25_000 {
        std::fs::create_dir_all(env_root.join(format!("{i:08x}-0000-4000-8000-000000000000"))).unwrap();
    }
    std::fs::create_dir_all(h.claude.join("ide")).unwrap();
    std::fs::create_dir_all(h.home.join(".gemini/antigravity-cli/log")).unwrap();
    let e = Engine::new(h.config(procs(), Arc::new(TeamsRec::default()), None), Arc::new(Rec::default()));
    let specs = e.watch_specs();
    assert!(!specs.is_empty());
    let claude = std::fs::canonicalize(&h.claude).unwrap();
    for s in &specs {
        assert_ne!(s.path, claude, "the config root itself is never watched");
        assert!(!env_root.starts_with(&s.path) || !s.tag.recursive(), "{s:?} would cover session-env");
        assert!(!s.path.ends_with("ide") && !s.path.ends_with("log"), "{s:?}");
    }
    let sessions = claude.join("sessions");
    assert!(specs.iter().any(|s| s.path == sessions), "the markers' root is watched");
    let got: Signals = Arc::default();
    let sink = got.clone();
    let _w = FsWatch::start(
        specs,
        Arc::new(move |s| {
            let (lock, cv) = &*sink;
            lock.lock().unwrap().push(s);
            cv.notify_all();
        }),
    );
    // Everything the setup did has been delivered (or never will be).
    barrier(&sessions, &got, "watchscope-setup.json");
    got.0.lock().unwrap().clear();

    std::fs::write(env_root.join("00000001-0000-4000-8000-000000000000/x"), "1").unwrap();
    std::fs::write(h.claude.join("ide/5555.lock"), "1").unwrap();
    std::fs::write(h.home.join(".gemini/antigravity-cli/log/cli-1.log"), "1").unwrap();
    // Every event of the three writes is delivered before this marker's.
    barrier(&sessions, &got, "watchscope-after.json");

    let woke: Vec<FsSignal> = got
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|s| !matches!(names(s), Some("watchscope-setup.json" | "watchscope-after.json")))
        .cloned()
        .collect();
    assert!(woke.is_empty(), "a write under session-env/, ide/ or agy log/ woke discovery: {woke:?}");
}
