//! PTY fidelity (contract §6.2.3, §6.2.6, §6.2.7; the kept `pty_fidelity`
//! suite): bytes in, bytes out, nothing lost, nothing reordered, nothing
//! decoded; one view; the child in its own group; spawns only through
//! `SupervisorApi` (A12.8); exits reported; titles memory only (A12.4).
//!
//! Every PTY here is opened through the real `PtyManager` over the real
//! supervisor with `fake_agent` or `/bin/sh` panes — never a vendor CLI. Waits
//! are event-polled with generous ceilings; the throughput and latency budgets
//! live in the `#[ignore]`d benchmarks at the bottom (G5, A10.7), which the
//! perf harness runs, not the functional gate.
//!
//! A52 (T49): a quickly exiting child's last output is never lost. The
//! `TestHold` hooks recreate both orders in which it used to be: the core
//! closing its spawn-time slave after the child had exited, and a reader
//! starved past the child's exit.

mod fixtures;

#[path = "fixtures/fake_agent/pty_harness.rs"]
mod harness;

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use gilde_code_lib::contract::common::AbsPath;
use gilde_code_lib::contract::content::{DetachReason, PtyChunk};
use gilde_code_lib::contract::ids::ViewId;
use gilde_code_lib::contract::process::ProcKind;
use gilde_code_lib::contract::services::{PtyApi, SignalPlan};
use gilde_code_lib::contract::ErrorCode;
use gilde_code_lib::pty::manager::TestHold;
use gilde_code_lib::pty::ring::DEFAULT_RING_BYTES;
use gilde_code_lib::pty::session::{ReaderHold, FLUSH_BYTES, FLUSH_INTERVAL, QUEUE_FRAMES, READ_CHUNK};
use gilde_code_lib::pty::{parse_snapshot_header, PtyOptions, SNAPSHOT_HEADER_LEN};
use gilde_code_lib::store::dao;
use rusty_fork::rusty_fork_test;

use harness::{alive, wait_for, Collect, Harness, CEILING};

fn view(name: &str) -> ViewId {
    ViewId::parse(name).expect("view id")
}

fn raw() -> PtyOptions {
    PtyOptions {
        raw_output: true,
        ..PtyOptions::default()
    }
}

// ---------------------------------------------------------------------------
// EOF comes when the child is gone; the child leads its group
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn killing_the_child_makes_the_reader_see_eof() {
    let h = Harness::new().await;
    let agent = h.install("agent");
    let (handle, exits) = h.open(h.spec(&agent, &["hang"])).await;
    let session = h.manager.get(&handle.pty_id).expect("session");
    assert!(alive(handle.pid));
    assert!(!session.saw_eof(), "no EOF while the child holds the slave");

    // The test started this fixture; it may end it.
    unsafe { libc::kill(handle.pid, libc::SIGKILL) };

    assert!(
        wait_for(CEILING, || session.saw_eof()).await,
        "the reader must see EOF once the child dies — else a slave was left open"
    );
    assert!(wait_for(CEILING, || exits.first().is_some()).await, "the exit sink fires");
    assert_eq!(exits.first().and_then(|e| e.signal), Some(libc::SIGKILL));
}

#[tokio::test(flavor = "multi_thread")]
async fn every_pty_child_leads_its_own_process_group() {
    let h = Harness::new().await;
    let agent = h.install("agent");
    let (handle, _exits) = h.open(h.spec(&agent, &["hang"])).await;
    let session = h.manager.get(&handle.pty_id).expect("session");
    assert_eq!(session.pgid(), handle.pid, "the child is its own group leader");
    assert_eq!(session.foreground_pgid(), Some(handle.pid), "tcgetpgrp names the child's group");
    let outcome = h
        .manager
        .close(&handle.pty_id, SignalPlan::HupThenTerm { grace_ms: 2_000 })
        .await
        .expect("close");
    assert!(outcome.exited && !outcome.survivor, "{outcome:?}");
    assert!(wait_for(CEILING, || !alive(handle.pid)).await);
}

// ---------------------------------------------------------------------------
// spawning (A12.8, §9.3)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_binary_outside_the_allowlist_is_refused_and_nothing_runs() {
    let h = Harness::new().await;
    // On PATH, but never allowlisted: a copy, so its realpath is its own (the
    // allowlist is keyed on realpaths, §9.3).
    let path = h.hermetic.bin_dir().join("stranger");
    std::fs::copy(fixtures::fake_agent_path(), &path).expect("copy");
    let argv0 = gilde_code_lib::contract::common::AbsPath::parse(path.to_string_lossy().into_owned())
        .expect("abs");
    let spec = h.spec(&argv0, &["normal", "0"]);
    let err = h
        .manager
        .open(spec, Arc::new(harness::ExitRecorder::default()))
        .await
        .expect_err("refused");
    assert_eq!(err.code, ErrorCode::Refused, "{err:?}");
    assert!(h.manager.is_empty(), "no PTY was opened");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_child_gets_the_composed_env_and_its_spawn_marker_only() {
    let h = Harness::new().await;
    let agent = h.install("agent");
    let mut spec = h.spec(&agent, &["print-env", "GILDE_SPAWN_ID"]);
    // A caller-supplied marker never wins over the plan's ticket.
    spec.env.push(("GILDE_SPAWN_ID".to_string(), "forged".to_string()));
    let (handle, exits) = h.open(spec).await;
    let collect = Arc::new(Collect::default());
    h.manager.attach(&handle.pty_id, view("v1"), collect.clone()).expect("attach");
    assert!(wait_for(CEILING, || exits.first().is_some()).await);
    let session = h.manager.get(&handle.pty_id).expect("session");
    assert!(session.saw_eof(), "an exit is reported once the last bytes are in the ring");
    let snapshot = h.manager.snapshot(&handle.pty_id).expect("snapshot");
    let text: String = snapshot[SNAPSHOT_HEADER_LEN..].iter().map(|b| *b as char).collect();
    assert!(text.contains(&handle.ticket.env_marker), "the child saw its marker: {text:?}");
    assert!(!text.contains("forged"));
}

// ---------------------------------------------------------------------------
// a wedged view never back-pressures the child
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(pty_load)]
async fn a_flood_finishes_while_the_view_is_wedged_and_the_view_is_told_to_resync() {
    const TOTAL: u64 = 16 * 1024 * 1024;
    let h = Harness::new().await;
    let agent = h.install("agent");
    let (handle, exits) = h
        .open_with(h.spec(&agent, &["binary-flood", &TOTAL.to_string()]), raw())
        .await;
    let collect = Arc::new(Collect::default());
    collect.wedged.store(true, Ordering::Release);
    h.manager.attach(&handle.pty_id, view("v1"), collect.clone()).expect("attach");
    let session = h.manager.get(&handle.pty_id).expect("session");

    assert!(
        wait_for(Duration::from_secs(300), || exits.first().is_some()).await,
        "the flood must complete although the view takes nothing in time"
    );
    assert_eq!(exits.first().and_then(|e| e.code), Some(0));
    assert!(wait_for(CEILING, || session.saw_eof()).await);
    assert_eq!(session.next_offset(), TOTAL, "every byte reached the ring");

    collect.wedged.store(false, Ordering::Release);
    assert!(
        wait_for(CEILING, || collect.resyncs() > 0).await,
        "dropped frames become a Resync, never a corrupt screen"
    );
    // Every chunk still tells the truth about its offset.
    let expected = fixtures::expected_flood(TOTAL as usize);
    for (offset, bytes) in collect.data() {
        let at = offset as usize;
        assert_eq!(&expected[at..at + bytes.len()], &bytes[..], "chunk at {offset}");
    }
}

// ---------------------------------------------------------------------------
// bytes stay bytes
// ---------------------------------------------------------------------------

fn pty_sources() -> Vec<(String, String)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/pty");
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("src/pty") {
        let path = entry.expect("entry").path();
        if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            out.push((name, std::fs::read_to_string(&path).expect("source")));
        }
    }
    out
}

#[test]
fn nothing_under_src_pty_turns_pty_bytes_into_a_string() {
    for (name, body) in pty_sources() {
        for banned in ["from_utf8_lossy", "String::from_utf8"] {
            assert!(!body.contains(banned), "src/pty/{name} contains `{banned}`");
        }
    }
}

#[test]
fn the_pty_lane_never_kills_a_child_directly() {
    for (name, body) in pty_sources() {
        // Code, not prose: the doc comments say "never SIGKILL" on purpose.
        let code: String = body
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for banned in [".kill()", "SIGKILL", "killpg(", "nix::sys::signal::kill("] {
            let body = &code;
            assert!(
                !body.contains(banned),
                "src/pty/{name} contains `{banned}`: every signal goes through the \
                 supervisor's identity fence (§9.8, C5)"
            );
        }
    }
}

#[test]
fn the_ring_and_the_coalescer_use_the_specified_sizes() {
    assert_eq!(DEFAULT_RING_BYTES, 2 * 1024 * 1024);
    assert_eq!(FLUSH_INTERVAL, Duration::from_millis(8));
    assert_eq!(FLUSH_BYTES, 32 * 1024);
    assert_eq!(READ_CHUNK, 64 * 1024);
    assert_eq!(QUEUE_FRAMES, 64);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_burst_of_tiny_writes_arrives_coalesced_and_in_order() {
    let h = Harness::new().await;
    let (handle, exits) = h
        .open_with(
            h.shell("i=0; while [ $i -lt 400 ]; do printf '%03d,' $((i % 1000)); i=$((i+1)); done"),
            raw(),
        )
        .await;
    let collect = Arc::new(Collect::default());
    h.manager.attach(&handle.pty_id, view("v1"), collect.clone()).expect("attach");
    assert!(wait_for(CEILING, || exits.first().is_some()).await);
    let session = h.manager.get(&handle.pty_id).expect("session");
    assert!(wait_for(CEILING, || session.saw_eof()).await);
    // The view attached after the child started: snapshot + live chunks.
    let snapshot = h.manager.snapshot(&handle.pty_id).expect("snapshot");
    let text: String = snapshot[SNAPSHOT_HEADER_LEN..].iter().map(|b| *b as char).collect();
    let expected: String = (0..400).map(|i| format!("{:03},", i % 1000)).collect();
    assert_eq!(text, expected, "no byte lost, none reordered");
    let data = collect.data();
    assert!(
        data.len() < 400,
        "400 writes are coalesced into fewer chunks ({} chunks)",
        data.len()
    );
    for (offset, bytes) in &data {
        let at = *offset as usize;
        assert_eq!(&expected.as_bytes()[at..at + bytes.len()], &bytes[..], "chunk at {offset}");
    }
    // A52.2: consecutive chunks join up unless the view was told to resync
    // between them. Under load the reader may drop frames while the
    // coalescer waits for a CPU (a slow view never back-pressures the
    // child, §6.2.6); the hole is announced as a Resync before the far side
    // of it, and the ring above still holds every byte.
    let mut end: Option<u64> = None;
    for chunk in collect.chunks.lock().iter() {
        match chunk {
            PtyChunk::Data { offset, b64 } => {
                if let Some(end) = end {
                    assert_eq!(end, *offset, "chunks join up unless a Resync came between them");
                }
                let len = base64::engine::general_purpose::STANDARD.decode(b64).expect("b64").len();
                end = Some(offset + len as u64);
            }
            PtyChunk::Resync { .. } => end = None,
            _ => {}
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_snapshot_is_a_sixteen_byte_header_followed_by_raw_ring_bytes() {
    let h = Harness::new().await;
    let (handle, exits) = h.open_with(h.shell("printf 'hello\\001\\377world'"), raw()).await;
    assert!(wait_for(CEILING, || exits.first().is_some()).await);
    let session = h.manager.get(&handle.pty_id).expect("session");
    assert!(wait_for(CEILING, || session.saw_eof()).await);
    let snapshot = h.manager.snapshot(&handle.pty_id).expect("snapshot");
    let (version, next) = parse_snapshot_header(&snapshot).expect("GLDP header");
    assert_eq!(version, 1);
    assert_eq!(next, (snapshot.len() - SNAPSHOT_HEADER_LEN) as u64);
    assert_eq!(&snapshot[SNAPSHOT_HEADER_LEN..], b"hello\x01\xffworld");
}

#[tokio::test(flavor = "multi_thread")]
async fn flood_bytes_survive_the_pty_and_the_ring_byte_for_byte() {
    const TOTAL: usize = 1_048_576;
    let h = Harness::new().await;
    let agent = h.install("agent");
    let (handle, exits) = h
        .open_with(h.spec(&agent, &["binary-flood", &TOTAL.to_string()]), raw())
        .await;
    let collect = Arc::new(Collect::default());
    h.manager.attach(&handle.pty_id, view("v1"), collect.clone()).expect("attach");
    assert!(wait_for(CEILING, || exits.first().is_some()).await);
    let session = h.manager.get(&handle.pty_id).expect("session");
    assert!(wait_for(CEILING, || session.saw_eof()).await);

    let expected = fixtures::expected_flood(TOTAL);
    let snapshot = h.manager.snapshot(&handle.pty_id).expect("snapshot");
    let ring = &snapshot[SNAPSHOT_HEADER_LEN..];
    assert_eq!(ring.len(), TOTAL);
    assert_eq!(ring, &expected[..], "the ring is byte-identical to the source");
    for (offset, bytes) in collect.data() {
        let at = offset as usize;
        assert_eq!(&expected[at..at + bytes.len()], &bytes[..], "chunk at {offset}");
    }
    for boundary in [65_536usize, 131_072, 983_040] {
        assert_eq!(&ring[boundary - 1..boundary + 3], &fixtures::flood::STRADDLING_CHAR[..]);
        assert_eq!(&ring[boundary - 8..boundary + 8], &fixtures::flood::MARKER[..]);
    }
    assert!(std::str::from_utf8(ring).is_err(), "invalid UTF-8 survived undecoded");
}

// ---------------------------------------------------------------------------
// winsize, writes, the view
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_child_gets_the_opening_winsize_and_a_resize_reaches_it_as_sigwinch() {
    let h = Harness::new().await;
    // A52.2: the trap goes in before the opening size is printed, so "30 100"
    // on screen is the event that SIGWINCH now has a handler. With the trap
    // after the first `stty`, a resize under load could land in between and
    // be ignored (SIGWINCH's default action).
    let mut spec = h.shell("trap '/bin/stty size' WINCH; /bin/stty size; while :; do /bin/sleep 0.1; done");
    spec.cols = 100;
    spec.rows = 30;
    let (handle, _exits) = h.open(spec).await;
    let session = h.manager.get(&handle.pty_id).expect("session");
    let collect = Arc::new(Collect::default());
    h.manager.attach(&handle.pty_id, view("v1"), collect.clone()).expect("attach");
    assert_eq!(session.tty_size(), Some((100, 30)), "TIOCGWINSZ at open");
    let ring_text = |h: &Harness| -> String {
        let s = h.manager.snapshot(&handle.pty_id).unwrap_or_default();
        s.get(SNAPSHOT_HEADER_LEN..).unwrap_or_default().iter().map(|b| *b as char).collect()
    };
    assert!(wait_for(CEILING, || ring_text(&h).contains("30 100")).await, "{:?}", ring_text(&h));

    h.manager.resize(&handle.pty_id, 120, 40).await.expect("resize");
    assert_eq!(session.tty_size(), Some((120, 40)));
    assert!(wait_for(CEILING, || ring_text(&h).contains("40 120")).await, "{:?}", ring_text(&h));
    assert_eq!(h.manager.status_engine().screen(handle.pty_id.as_str()).map(|s| (s.cols, s.rows)), Some((120, 40)), "the grid follows the size");

    // Clamped, never rejected (§4.2.4): 0 → the 120×40 default, huge → the max.
    h.manager.resize(&handle.pty_id, 0, 0).await.expect("resize");
    assert_eq!(session.tty_size(), Some((120, 40)));
    h.manager.resize(&handle.pty_id, 5_000, 5_000).await.expect("resize");
    assert_eq!(session.tty_size(), Some((1_000, 500)));

    let outcome = h
        .manager
        .close(&handle.pty_id, SignalPlan::HupThenTerm { grace_ms: 2_000 })
        .await
        .expect("close");
    assert!(outcome.exited, "{outcome:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn write_delivers_the_exact_bytes_it_was_given() {
    let h = Harness::new().await;
    let (handle, exits) = h.open(h.shell("exec /bin/cat")).await;
    let collect = Arc::new(Collect::default());
    h.manager.attach(&handle.pty_id, view("v1"), collect.clone()).expect("attach");
    let hangul = "한글".as_bytes().to_vec();
    // A multi-byte character split across two writes.
    h.manager.write(&handle.pty_id, &hangul[..3]).await.expect("write");
    h.manager.write(&handle.pty_id, &hangul[3..]).await.expect("write");
    h.manager.write(&handle.pty_id, b"\r").await.expect("write");
    assert!(
        wait_for(CEILING, || harness::contains(&collect.bytes(), &hangul)).await,
        "{:?}",
        collect.text()
    );
    h.manager.write(&handle.pty_id, &[0x04]).await.expect("eof");
    assert!(wait_for(CEILING, || exits.first().is_some()).await);
    assert_eq!(exits.first().and_then(|e| e.code), Some(0));
    // Writing to a PTY whose child is gone fails cleanly.
    let session = h.manager.get(&handle.pty_id).expect("still listed until closed");
    assert!(wait_for(CEILING, || session.has_exited()).await);
}

/// T-pty-singleView: one attached view per PTY; another view takes it over
/// and the previous one hears `Detached{otherView}`.
#[tokio::test(flavor = "multi_thread")]
async fn a_second_view_takes_the_pty_and_the_first_is_told() {
    let h = Harness::new().await;
    let (handle, _exits) = h.open(h.shell("exec /bin/cat")).await;
    let a = Arc::new(Collect::default());
    let b = Arc::new(Collect::default());
    h.manager.attach(&handle.pty_id, view("tile-a"), a.clone()).expect("attach a");
    h.manager.write(&handle.pty_id, b"one\r").await.expect("write");
    assert!(wait_for(CEILING, || a.text().contains("one")).await);

    let info = h.manager.attach(&handle.pty_id, view("tile-b"), b.clone()).expect("attach b");
    assert_eq!(a.detached(), 1, "the first view hears that another took over");
    assert!(matches!(
        a.chunks.lock().last(),
        Some(PtyChunk::Detached { reason: DetachReason::OtherView })
    ));
    assert!(info.offset > 0, "attach answers the ring offset to snapshot from");

    h.manager.write(&handle.pty_id, b"two\r").await.expect("write");
    assert!(wait_for(CEILING, || b.text().contains("two")).await);
    assert!(!a.text().contains("two"), "nothing reaches a detached view");

    // Re-attaching the same view is a remount, not a takeover.
    h.manager.attach(&handle.pty_id, view("tile-b"), b.clone()).expect("remount");
    assert_eq!(b.detached(), 0);
    // Detach is idempotent and only the attached view can detach.
    h.manager.detach(&handle.pty_id, &view("tile-a"));
    h.manager.write(&handle.pty_id, b"three\r").await.expect("write");
    assert!(wait_for(CEILING, || b.text().contains("three")).await);
    h.manager.detach(&handle.pty_id, &view("tile-b"));
    h.manager.detach(&handle.pty_id, &view("tile-b"));
    let _ = h
        .manager
        .close(&handle.pty_id, SignalPlan::HupThenTerm { grace_ms: 2_000 })
        .await;
}

// ---------------------------------------------------------------------------
// exits and closing
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_exit_sink_reports_the_code_and_the_registry_row_closes() {
    let h = Harness::new().await;
    let agent = h.install("agent");
    let (handle, exits) = h.open(h.spec(&agent, &["normal", "3"])).await;
    assert!(wait_for(CEILING, || exits.first().is_some()).await);
    assert_eq!(exits.first(), Some(gilde_code_lib::contract::activity::ExitInfo { code: Some(3), signal: None }));
    let spawn_id = handle.ticket.spawn_id.clone();
    let row_closed = || {
        let db = h.db.clone();
        let id = spawn_id.clone();
        async move {
            db.read(move |conn| dao::proc::get(conn, &id))
                .await
                .ok()
                .flatten()
                .is_some_and(|row| !row.state.is_live())
        }
    };
    let deadline = Instant::now() + CEILING;
    let mut closed = false;
    while Instant::now() < deadline {
        if row_closed().await {
            closed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(closed, "the registry row is closed by mark_exited");
    // A finished PTY stays readable until it is closed (a pane keeps its tile).
    assert!(h.manager.snapshot(&handle.pty_id).is_ok());
    let outcome = h
        .manager
        .close(&handle.pty_id, SignalPlan::Term { grace_ms: 1_000 })
        .await
        .expect("close");
    assert!(outcome.exited);
    assert!(h.manager.is_empty(), "closing a finished PTY retires it");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_exit_sink_reports_a_signal_as_a_number() {
    let h = Harness::new().await;
    let agent = h.install("agent");
    let (handle, exits) = h.open(h.spec(&agent, &["hang"])).await;
    unsafe { libc::kill(handle.pid, libc::SIGTERM) };
    assert!(wait_for(CEILING, || exits.first().is_some()).await);
    assert_eq!(exits.first().and_then(|e| e.signal), Some(libc::SIGTERM));
    assert_eq!(exits.first().and_then(|e| e.code), None);
}

/// A12.4: titles live only in memory — exposed on attach, never in `state.db`.
#[tokio::test(flavor = "multi_thread")]
async fn titles_are_exposed_on_attach_and_never_stored() {
    let h = Harness::new().await;
    let (handle, _exits) = h
        .open(h.shell("printf '\\033]0;TITLE-CANARY-T06\\007ready'; exec /bin/cat"))
        .await;
    assert!(
        wait_for(CEILING, || h.manager.title(&handle.pty_id).is_some()).await,
        "the OSC title is captured"
    );
    let info = h
        .manager
        .attach(&handle.pty_id, view("v1"), Arc::new(Collect::default()))
        .expect("attach");
    assert_eq!(info.title.as_deref(), Some("TITLE-CANARY-T06"));
    let rows = h
        .db
        .read(dao::pty_session::list_open)
        .await
        .expect("read");
    assert!(rows.is_empty(), "the PTY lane writes no pty_session rows");
    let _ = h
        .manager
        .close(&handle.pty_id, SignalPlan::HupThenTerm { grace_ms: 2_000 })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_pty_is_unsupported_no_pty() {
    let h = Harness::new().await;
    let ghost = gilde_code_lib::contract::ids::PtyId::mint();
    let err = h.manager.write(&ghost, b"x").await.expect_err("no pty");
    assert_eq!(err.code, ErrorCode::Unsupported);
    assert_eq!(err.params["cap"], "noPty");
    assert!(h.manager.attach(&ghost, view("v1"), Arc::new(Collect::default())).is_err());
    h.manager.detach(&ghost, &view("v1")); // idempotent, no error
    let spec = h.shell("true");
    assert_eq!(spec.kind, ProcKind::Pane);
}

// ---------------------------------------------------------------------------
// A52.1: a quickly exiting child's last output is never lost (T49)
// ---------------------------------------------------------------------------
//
// On macOS the last close of a pty slave waits at most ~0.6 s for the master
// to read what is queued, then discards it. Before A52.1 the core dropped its
// slave right after the fork, so that last close was either the core's own
// (when the child printed and exited before the core got to the drop; no
// reader existed yet) or the child's exit (when the reader was starved past
// it). The core now holds a slave until the child is reaped and the master
// drained. Each hook below recreates one of those orders on purpose, with
// `fake_agent`: a child that opened `/dev/tty` (bash does, so a `/bin/sh`
// pane) is spared either way, because its session keeps the terminal open
// until the drain is over.

/// Runs per hook: each must keep its marker every time.
const A52_RUNS: usize = 50;
/// The `ReaderUntilReaped` ceiling. With the held slave, a child that leads
/// its session cannot be reaped while its output is unread, so this is
/// what releases the reader; before A52.1 the reap came first (~0.6 s).
const A52_READER_CEILING: Duration = Duration::from_secs(2);

fn held(hold: TestHold) -> PtyOptions {
    PtyOptions {
        test_hold: Some(hold),
        ..PtyOptions::default()
    }
}

fn ring_bytes(h: &Harness, pty: &gilde_code_lib::contract::ids::PtyId) -> Vec<u8> {
    h.manager
        .snapshot(pty)
        .map(|s| s[SNAPSHOT_HEADER_LEN..].to_vec())
        .unwrap_or_default()
}

/// One opened child that prints its `GILDE_SPAWN_ID`: wait for its exit
/// report, say whether the marker reached the ring, and retire the PTY.
async fn marker_outcome(
    h: &Harness,
    handle: &gilde_code_lib::contract::services::PtyHandle,
    exits: &harness::ExitRecorder,
) -> Result<Option<ReaderHold>, String> {
    if !wait_for(CEILING, || exits.first().is_some()).await {
        return Err("no exit reported".into());
    }
    let session = h.manager.get(&handle.pty_id).map_err(|e| format!("{e:?}"))?;
    let ring = ring_bytes(h, &handle.pty_id);
    let outcome = (
        exits.first().and_then(|e| e.code),
        session.saw_eof(),
        harness::contains(&ring, handle.ticket.env_marker.as_bytes()),
        session.held_slave_fd(),
    );
    let hold = session.reader_hold();
    drop(session);
    let _ = h.manager.close(&handle.pty_id, SignalPlan::Term { grace_ms: 1_000 }).await;
    match outcome {
        (Some(0), true, true, None) => Ok(hold),
        (code, eof, kept, fd) => Err(format!(
            "code {code:?}, eof {eof}, marker kept {kept}, held slave {fd:?}, ring {:?}",
            ring.iter().map(|b| *b as char).collect::<String>()
        )),
    }
}

/// T-pty-lastOutputExited (A52.1, the child exits before the core lets go
/// of its spawn-time slave): the core holds its drop (and so the reader's
/// start) until the child is exiting and its session has let go of the
/// terminal. Before A52.1 the core's close was then the last one and threw
/// the marker away (0 of 50 kept, T49).
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(pty_load)]
async fn a52_a_child_that_exits_before_the_core_lets_go_of_its_spawn_slave_keeps_its_marker() {
    let h = Harness::new().await;
    let agent = h.install("agent");
    let mut failures = Vec::new();
    for run in 0..A52_RUNS {
        // The hold fails the open if the child does not get there.
        let (handle, exits) = h
            .open_with(
                h.spec(&agent, &["print-env", "GILDE_SPAWN_ID"]),
                held(TestHold::SpawnSlaveUntilChildExited),
            )
            .await;
        if let Err(why) = marker_outcome(&h, &handle, &exits).await {
            failures.push(format!("run {run}: {why}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {A52_RUNS} runs kept the marker: {failures:#?}",
        A52_RUNS - failures.len()
    );
}

/// T-pty-lastOutputStarved (A52.1, the reader starved past the exit): the
/// reader does not read until the child is reaped (or a ceiling). Before
/// A52.1 the child's own last close threw the marker away and the reap
/// released the reader onto an EOF (0 of 50 kept, T49). Now the child cannot
/// be reaped before its output is read, so the ceiling releases the reader.
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(pty_load)]
async fn a52_a_child_whose_reader_is_held_back_past_its_exit_keeps_its_marker() {
    let h = Harness::new().await;
    let agent = h.install("agent");
    // All held at once: the ceilings run side by side.
    let mut opened = Vec::new();
    for _ in 0..A52_RUNS {
        opened.push(
            h.open_with(
                h.spec(&agent, &["print-env", "GILDE_SPAWN_ID"]),
                held(TestHold::ReaderUntilReaped { ceiling: A52_READER_CEILING }),
            )
            .await,
        );
    }
    let mut failures = Vec::new();
    let mut by_ceiling = 0;
    for (run, (handle, exits)) in opened.iter().enumerate() {
        match marker_outcome(&h, handle, exits).await {
            Ok(Some(ReaderHold::Ceiling)) => by_ceiling += 1,
            Ok(_) => {}
            Err(why) => failures.push(format!("run {run}: {why}")),
        }
    }
    eprintln!("[A52] reader released by the ceiling, the child still unreaped: {by_ceiling} of {A52_RUNS}");
    assert!(
        failures.is_empty(),
        "{} of {A52_RUNS} runs kept the marker: {failures:#?}",
        A52_RUNS - failures.len()
    );
}

/// A52.1: the held slave is close-on-exec, so no other spawn inherits it,
/// and it is closed once the child is gone.
#[tokio::test(flavor = "multi_thread")]
async fn a52_the_held_slave_is_close_on_exec_and_closed_after_the_exit() {
    let h = Harness::new().await;
    let agent = h.install("agent");
    let (handle, exits) = h.open(h.spec(&agent, &["hang"])).await;
    let session = h.manager.get(&handle.pty_id).expect("session");
    let fd = session.held_slave_fd().expect("the core holds a slave while the child runs");
    // SAFETY: F_GETFD only reads the flags of a descriptor the session owns.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    assert!(flags >= 0 && flags & libc::FD_CLOEXEC != 0, "FD_CLOEXEC is set: {flags}");
    // SAFETY: isatty only inspects the descriptor.
    assert_eq!(unsafe { libc::isatty(fd) }, 1, "it is the terminal's slave side");
    // A plain spawn from this process does not see it.
    let probe = std::process::Command::new("/bin/test")
        .args(["-e", &format!("/dev/fd/{fd}")])
        .status()
        .expect("/bin/test");
    assert_eq!(probe.code(), Some(1), "a spawned process does not inherit descriptor {fd}");

    let outcome = h
        .manager
        .close(&handle.pty_id, SignalPlan::HupThenTerm { grace_ms: 2_000 })
        .await
        .expect("close");
    assert!(outcome.exited, "{outcome:?}");
    assert!(wait_for(CEILING, || exits.first().is_some()).await);
    assert_eq!(session.held_slave_fd(), None, "closed once the child is reaped");
}

/// The descriptors this process has open, and how many of them are ptys.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Census {
    total: usize,
    ptys: usize,
}

fn census() -> Census {
    let mut out = Census { total: 0, ptys: 0 };
    for entry in std::fs::read_dir("/dev/fd").expect("/dev/fd") {
        let name = entry.expect("entry").file_name();
        let Ok(fd) = name.to_string_lossy().parse::<i32>() else {
            continue;
        };
        out.total += 1;
        let mut path = [0 as libc::c_char; libc::PATH_MAX as usize];
        // SAFETY: F_GETPATH writes at most PATH_MAX bytes into `path`.
        if unsafe { libc::fcntl(fd, libc::F_GETPATH, path.as_mut_ptr()) } == 0 {
            // SAFETY: F_GETPATH wrote a NUL-terminated path.
            let path = unsafe { std::ffi::CStr::from_ptr(path.as_ptr()) }.to_string_lossy();
            if path.starts_with("/dev/ttys") || path.starts_with("/dev/ptmx") || path.starts_with("/dev/pty") {
                out.ptys += 1;
            }
        }
    }
    out
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(f)
}

/// Open one PTY and end it: `run` even = a child that exits by itself, odd
/// = one that is closed while it runs. Waits until it is retired.
async fn open_and_close(h: &Harness, agent: &AbsPath, run: usize) {
    let exits_by_itself = run.is_multiple_of(2);
    let args: &[&str] = if exits_by_itself { &["print-env", "GILDE_SPAWN_ID"] } else { &["hang"] };
    let (handle, exits) = h.open(h.spec(agent, args)).await;
    if !exits_by_itself {
        let outcome = h
            .manager
            .close(&handle.pty_id, SignalPlan::HupThenTerm { grace_ms: 2_000 })
            .await
            .expect("close");
        assert!(outcome.exited, "run {run}: {outcome:?}");
    }
    assert!(wait_for(CEILING, || exits.first().is_some()).await, "run {run}: no exit");
    let _ = h.manager.close(&handle.pty_id, SignalPlan::Term { grace_ms: 1_000 }).await;
    assert!(h.manager.get(&handle.pty_id).is_err(), "run {run}: retired");
}

rusty_fork_test! {
    /// T-pty-noFdLeak (A52.1): open and close 20 sessions (and one spawn
    /// that fails after the pty exists); the process's descriptor count
    /// returns to its baseline. Its own process (`rusty-fork`), so no other
    /// test's descriptors move the count.
    #[test]
    fn a52_twenty_sessions_opened_and_closed_leave_the_descriptor_count_at_its_baseline() {
        block_on(async {
            let h = Harness::new().await;
            let agent = h.install("agent");
            // Warm-up: whatever the runtime creates lazily exists before the
            // baseline. Every descriptor a session owns is a pty one, so "no
            // pty descriptor open" means the warm-up session is all gone.
            open_and_close(&h, &agent, 0).await;
            assert!(wait_for(CEILING, || census().ptys == 0).await, "warm-up: {:?}", census());
            let baseline = census();

            for run in 0..20 {
                open_and_close(&h, &agent, run).await;
            }
            // A spawn that fails after the pty and the held slave exist: an
            // allowlisted file without its execute bit (`execvp` answers
            // EACCES; a non-binary with the bit would run as a script).
            let junk = h.hermetic.bin_dir().join("not-executable");
            std::fs::write(&junk, b"#!/bin/sh\nexit 0\n").expect("write");
            let mut perms = std::fs::metadata(&junk).expect("meta").permissions();
            std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o644);
            std::fs::set_permissions(&junk, perms).expect("chmod");
            gilde_code_lib::supervise::spawn::allow_executable(&junk, &[gilde_code_lib::store::models::ProcKind::Pty]);
            let argv0 = AbsPath::parse(junk.to_string_lossy().into_owned()).expect("abs");
            let err = h
                .manager
                .open(h.spec(&argv0, &[]), Arc::new(harness::ExitRecorder::default()))
                .await
                .expect_err("the exec fails");
            assert_eq!(err.code, ErrorCode::SpawnFailed, "{err:?}");

            assert!(wait_for(CEILING, || census().ptys == 0).await, "pty descriptors left: {:?}", census());
            assert!(
                wait_for(CEILING, || census() == baseline).await,
                "baseline {baseline:?}, now {:?}",
                census()
            );
        });
    }
}

/// T-pty-grandchildBound (A52.1): a grandchild that keeps the slave open
/// (it inherits the leader's ignored SIGHUP, so the leader's exit does not
/// end it) does not hold the exit report back. On macOS the leader's exit
/// revokes the terminal, so EOF still comes; were it not, the report would
/// come at the grace bound (`EXIT_EOF_GRACE`).
#[tokio::test(flavor = "multi_thread")]
async fn a52_a_grandchild_holding_the_slave_does_not_hold_back_the_exit_report() {
    let h = Harness::new().await;
    let (handle, exits) = h
        .open(h.shell("trap '' HUP; /bin/sleep 60 & echo \"gc=$!\"; echo leader-done"))
        .await;
    let reported = wait_for(CEILING, || exits.first().is_some()).await;
    let text: String = ring_bytes(&h, &handle.pty_id).iter().map(|b| *b as char).collect();
    let grandchild: Option<i32> = text
        .split("gc=")
        .nth(1)
        .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|pid| pid.parse().ok());
    let still_there = grandchild.is_some_and(alive);
    // The test's own pane started it; end it whatever the outcome.
    if let Some(pid) = grandchild {
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    assert!(reported, "the exit is reported although a grandchild holds the slave: {text:?}");
    assert!(still_there, "the grandchild was still running at the report: {text:?}");
    assert!(text.contains("leader-done"), "the leader's last line is kept: {text:?}");
    let session = h.manager.get(&handle.pty_id).expect("session");
    assert_eq!(session.held_slave_fd(), None, "the held slave is closed at the report");
    eprintln!("[A52] grandchild case: EOF seen at the report: {}", session.saw_eof());
}

// ---------------------------------------------------------------------------
// G5 benchmarks (A10.7): run by the perf harness, never by the gate
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
#[ignore = "G5 benchmark (A10.7): throughput budget"]
async fn bench_four_simultaneous_floods_report_their_throughput() {
    const TOTAL: u64 = 32 * 1024 * 1024;
    let h = Harness::new().await;
    let agent = h.install("agent");
    let started = Instant::now();
    let mut all = Vec::new();
    for _ in 0..4 {
        let (handle, exits) = h
            .open_with(h.spec(&agent, &["binary-flood", &TOTAL.to_string()]), raw())
            .await;
        h.manager
            .attach(&handle.pty_id, view("v1"), Arc::new(Collect::default()))
            .expect("attach");
        all.push(exits);
    }
    for exits in &all {
        assert!(wait_for(Duration::from_secs(600), || exits.first().is_some()).await);
    }
    let secs = started.elapsed().as_secs_f64();
    eprintln!("[G5] 4 × {} MiB in {secs:.2}s = {:.1} MB/s", TOTAL >> 20, 4.0 * TOTAL as f64 / 1e6 / secs);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "G5 benchmark (A10.7): command-surface latency under a flood"]
async fn bench_a_flood_never_makes_the_command_surface_wait() {
    let h = Harness::new().await;
    let (handle, _exits) = h.open(h.shell("exec /usr/bin/yes")).await;
    h.manager
        .attach(&handle.pty_id, view("v1"), Arc::new(Collect::default()))
        .expect("attach");
    let mut worst = Duration::ZERO;
    for i in 0..50u16 {
        let t = Instant::now();
        h.manager.resize(&handle.pty_id, 100 + (i % 2), 30).await.expect("resize");
        let _ = h.manager.snapshot(&handle.pty_id).expect("snapshot");
        worst = worst.max(t.elapsed());
    }
    eprintln!("[G5] worst resize+snapshot under a flood: {worst:?}");
    let _ = h
        .manager
        .close(&handle.pty_id, SignalPlan::HupThenTerm { grace_ms: 2_000 })
        .await;
}

#[test]
#[ignore = "G5 benchmark (A10.7): grid throughput"]
fn bench_five_megabytes_of_newlines_through_the_grid() {
    let mut tail = gilde_code_lib::pty::AnsiTail::with_size(120, 40);
    let chunk = b"y\r\n".repeat(32 * 1024);
    let started = Instant::now();
    for _ in 0..54 {
        tail.feed(&chunk);
    }
    eprintln!("[G5] 5 MB of `yes` through the grid: {:?}", started.elapsed());
}
