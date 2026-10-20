//! The PTY lane against **real ptys and real children** — DESIGN §6, M1-PLAN T15.
//!
//! The unit tests inside `pty::ring` and `pty::session` cover the arithmetic and
//! the state machine. These cover the part that cannot be faked:
//!
//!   * that `pair.slave` is really dropped (a retained slave fd means the reader
//!     never sees EOF and the session can never be closed) — acceptance 1;
//!   * that every pty child leads its own process group — acceptance 2;
//!   * that a wedged view cannot apply back-pressure to a 50 MiB flood, and that
//!     the drop is reported as `gilde://pty/resync` — acceptance 3;
//!   * that a byte which entered the pty leaves the ring **identical**, markers
//!     straddling the 64 KiB read boundary included — acceptance 7;
//!   * that the three close modes do what DESIGN §6-5 says, without ever calling
//!     `Child::kill()` — acceptance 9;
//!   * what four simultaneous streams actually cost — acceptance 12 (R1 gate).
//!
//! Two tests are `#[ignore]`d because they drive a real agent CLI / editor: they
//! are the manual verification of acceptance 14 and are run explicitly with
//! `cargo test --test pty_fidelity -- --ignored`.

mod fixtures;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gilde_code_lib::agents::AgentKind;
use gilde_code_lib::config::ConfigStore;
use gilde_code_lib::events::bus::{EventBus, PatchSink};
use gilde_code_lib::model::graph::{GraphPatch, GraphState};
use gilde_code_lib::model::status::SessionStatus;
use gilde_code_lib::pty::ring::DEFAULT_RING_BYTES;
use gilde_code_lib::pty::session::{FLUSH_BYTES, FLUSH_INTERVAL, QUEUE_FRAMES, READ_CHUNK};
use gilde_code_lib::pty::{
    parse_snapshot_header, CloseMode, OpenSpec, PtyChunk, PtyManager, PtySession, PtySignal,
    SNAPSHOT_HEADER_LEN,
};
use gilde_code_lib::store::models::ProcState;
use gilde_code_lib::store::{dao, Db};
use gilde_code_lib::supervise::registry::Supervisor;

use base64::Engine as _;
use parking_lot::RwLock;
use serde_json::Value;
use tauri::ipc::{Channel, InvokeResponseBody};

use fixtures::*;

/// How long a test waits for the kernel and three threads to agree.
const SETTLE: Duration = Duration::from_secs(10);

// `#[serial_test::serial(pty_load)]` — the serial group two kinds of test share.
//
// One kind saturates every core on the machine (the 50 MiB flood, the four
// simultaneous streams of the R1 gate, the 5 MB `yes` flood); the other asks
// "is this session *still* working" / "has this row been written yet", which is
// a question about wall-clock scheduling. Running them at the same time is what
// made `send_prompt_waits_for_idle…` and `the_exit_watcher_reports_the_code…`
// fail under `cargo test --no-fail-fast` while passing alone (PINNED §23-4).
// `serial_test` is intra-binary, which is exactly the scope of the problem:
// `cargo test` runs one test binary at a time.
//
// Everything inside the group still asserts on **events**, never on a sleep —
// the group only removes the CPU competition that made the budgets too tight.

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

/// A [`PatchSink`] that keeps every emitted event so a test can assert on it.
#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<(String, Value)>>,
    patches: Mutex<Vec<Value>>,
}

impl Recorder {
    fn named(&self, name: &str) -> Vec<Value> {
        self.events
            .lock()
            .expect("recorder")
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
            .collect()
    }

    fn patch_count(&self) -> usize {
        self.patches.lock().expect("recorder").len()
    }
}

impl PatchSink for Recorder {
    fn emit_patch(&self, patch: &GraphPatch) {
        if let Ok(value) = serde_json::to_value(patch) {
            self.patches.lock().expect("recorder").push(value);
        }
    }

    fn emit_named(&self, name: &str, payload: &Value) {
        self.events
            .lock()
            .expect("recorder")
            .push((name.to_string(), payload.clone()));
    }
}

struct Harness {
    manager: Arc<PtyManager>,
    db: Db,
    recorder: Arc<Recorder>,
    dir: tempfile::TempDir,
}

async fn harness() -> Harness {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::open_in_memory().expect("db");
    let config =
        Arc::new(ConfigStore::open_at(&dir.path().join("config.json")).expect("config store"));
    let supervisor = Arc::new(
        Supervisor::open_instance(&db, config.clone(), "0.0.0-test")
            .await
            .expect("open_instance"),
    );
    let recorder = Arc::new(Recorder::default());
    let bus = EventBus::with_sink(
        recorder.clone(),
        Arc::new(RwLock::new(GraphState::new())),
    );
    let manager = Arc::new(PtyManager::new(supervisor, config).with_events(bus));
    Harness {
        manager,
        db,
        recorder,
        dir,
    }
}

impl Harness {
    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// An [`OpenSpec`] that runs the `fake_agent` fixture in the harness's temp
    /// directory. `idle_after_ms` is small so the prompt gate can be exercised
    /// without a four-second sleep.
    fn fixture_spec(&self, args: &[&str]) -> OpenSpec {
        let mut argv = vec![fake_agent_path().to_string_lossy().into_owned()];
        argv.extend(args.iter().map(|s| s.to_string()));
        self.spec(argv)
    }

    fn spec(&self, argv: Vec<String>) -> OpenSpec {
        let mut spec = OpenSpec::new(AgentKind::Shell, self.path(), argv);
        spec.idle_after_ms = 250;
        spec
    }

    /// Write `body` to `<tmp>/<name>` and return an [`OpenSpec`] that runs it
    /// under `/bin/sh`.
    ///
    /// A script file rather than `sh -c`: DESIGN's ban on `.arg("-c")` is about
    /// argv never travelling through a shell's parser, and the tests hold
    /// themselves to it even though `scripts/lint-forbidden.sh` only scans
    /// `src-tauri/src`.
    fn script_spec(&self, name: &str, body: &str) -> OpenSpec {
        let path = self.path().join(name);
        std::fs::write(&path, body).expect("write script");
        self.spec(vec![
            "/bin/sh".to_string(),
            path.to_string_lossy().into_owned(),
        ])
    }
}

// ---------------------------------------------------------------------------
// channels
// ---------------------------------------------------------------------------

/// Everything a test wants to know about the delivered chunk stream.
#[derive(Default)]
struct Collected {
    chunks: usize,
    bytes: Vec<u8>,
    /// Every chunk with the sequence number it declared, so a test can check
    /// each one against the source at its own offset even if an earlier one was
    /// dropped.
    pieces: Vec<(u64, Vec<u8>)>,
    /// Sequence number the next chunk must carry if nothing was dropped.
    expect_seq: Option<u64>,
    /// Chunks that did not continue where the previous one ended.
    gaps: usize,
}

impl Collected {
    fn record(&mut self, seq: u64, bytes: &[u8]) {
        if let Some(expected) = self.expect_seq {
            if expected != seq {
                self.gaps += 1;
            }
        }
        self.expect_seq = Some(seq + bytes.len() as u64);
        self.chunks += 1;
        self.bytes.extend_from_slice(bytes);
        self.pieces.push((seq, bytes.to_vec()));
    }
}

type Sink = Arc<Mutex<Collected>>;

fn decode(body: InvokeResponseBody) -> (u64, Vec<u8>) {
    let json = match body {
        InvokeResponseBody::Json(json) => json,
        InvokeResponseBody::Raw(_) => panic!("a PtyChunk is a json payload, not a raw one"),
    };
    let chunk: PtyChunk = serde_json::from_str(&json).expect("chunk is PtyChunk-shaped");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(chunk.b64.as_bytes())
        .expect("chunk b64 decodes");
    (chunk.seq, bytes)
}

/// A channel that keeps every byte it is handed.
fn collecting_channel() -> (Channel<PtyChunk>, Sink) {
    let state: Sink = Arc::new(Mutex::new(Collected::default()));
    let sink = state.clone();
    let channel = Channel::new(move |body| {
        let (seq, bytes) = decode(body);
        sink.lock().expect("sink").record(seq, &bytes);
        Ok(())
    });
    (channel, state)
}

/// A channel that counts bytes and nothing else — for throughput measurement.
fn counting_channel() -> (Channel<PtyChunk>, Arc<AtomicUsize>) {
    let count = Arc::new(AtomicUsize::new(0));
    let sink = count.clone();
    let channel = Channel::new(move |body| {
        let (_seq, bytes) = decode(body);
        sink.fetch_add(bytes.len(), Ordering::Relaxed);
        Ok(())
    });
    (channel, count)
}

/// A channel that models a wedged webview: the first send blocks until `gate`
/// is opened, which is exactly the situation the coalescer queue exists for.
fn wedged_channel(gate: Arc<AtomicBool>) -> (Channel<PtyChunk>, Arc<AtomicUsize>) {
    let count = Arc::new(AtomicUsize::new(0));
    let sink = count.clone();
    let channel = Channel::new(move |body| {
        while !gate.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(5));
        }
        let (_seq, bytes) = decode(body);
        sink.fetch_add(bytes.len(), Ordering::Relaxed);
        Ok(())
    });
    (channel, count)
}

// ---------------------------------------------------------------------------
// small helpers
// ---------------------------------------------------------------------------

fn pgid_via_ps(pid: i32) -> Option<i32> {
    let out = Command::new("/bin/ps")
        .args(["-o", "pgid=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

fn wait_until_exited(session: &Arc<PtySession>, budget: Duration) -> bool {
    wait_for(budget, || session.has_exited() && session.saw_eof())
}

/// How long the delivered byte count must stay unchanged to count as "quiet".
///
/// One unchanged 60 ms sample used to be enough. On a loaded machine (a full
/// parallel `cargo test`, a machine deep in swap) the reader and coalescer
/// threads can be descheduled for longer than that between two chunks, and the
/// test then measured a burst that had not finished arriving (387 of 400 bytes
/// during a full parallel run). Quiet now means no growth for a
/// sustained window; a stream that really lost bytes still settles short and
/// still fails the exact-count assertions.
const QUIET_WINDOW: Duration = Duration::from_millis(600);

/// Poll until the delivered byte count stops growing — the coalescer has an
/// 8 ms window, so "the child exited" is not yet "the last chunk arrived".
fn wait_for_quiet_sink(sink: &Sink, budget: Duration) -> usize {
    let deadline = Instant::now() + budget;
    let mut last = usize::MAX;
    let mut unchanged_since = Instant::now();
    loop {
        let now = sink.lock().expect("sink").bytes.len();
        if now != last {
            last = now;
            unchanged_since = Instant::now();
        } else if unchanged_since.elapsed() >= QUIET_WINDOW {
            return now;
        }
        if Instant::now() >= deadline {
            return now;
        }
        std::thread::sleep(Duration::from_millis(60));
    }
}

/// Poll an async read until it answers `Some`, or the budget runs out.
///
/// The exit watcher writes the registry row and the session row from its own
/// task, *after* the session has flipped to exited, so a one-shot read races
/// the watcher — the read is a question about a database row, and the answer
/// arrives as an event, not at a fixed time. Waiting on the row's *content* is
/// what makes the assertion independent of how busy the machine is.
async fn wait_for_row<T, F, Fut>(budget: Duration, mut read: F) -> Option<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = Instant::now() + budget;
    loop {
        if let Some(value) = read().await {
            return Some(value);
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn text(sink: &Sink) -> String {
    String::from_utf8_lossy(&sink.lock().expect("sink").bytes).into_owned()
}

fn source_of(relative: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn pty_sources() -> Vec<(String, String)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/pty");
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("read src/pty") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            out.push((name, std::fs::read_to_string(&path).expect("read")));
        }
    }
    assert!(out.len() >= 4, "src/pty should hold mod/manager/session/ring");
    out
}

// ---------------------------------------------------------------------------
// acceptance 1 — the slave is dropped
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn killing_the_child_makes_the_reader_see_eof_within_a_second() {
    let h = harness().await;
    let (channel, _sink) = collecting_channel();
    let info = h
        .manager
        .open(h.fixture_spec(&["hang"]), channel)
        .await
        .expect("open");
    let session = h.manager.get(&info.id).expect("session");

    assert!(pid_alive(info.pid), "the child should be running");
    assert!(!session.saw_eof(), "no EOF while the child holds the slave");

    kill_pid(info.pid, libc::SIGKILL);

    assert!(
        wait_for(Duration::from_secs(1), || session.saw_eof()),
        "the reader must see EOF within one second of the child dying — if it \
         does not, `pair.slave` was still open in this process and no session \
         could ever be closed"
    );
    assert!(wait_for(SETTLE, || session.has_exited()));
}

// ---------------------------------------------------------------------------
// acceptance 2 / 13 — process group discipline and the foreground group
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn every_pty_child_leads_its_own_process_group() {
    let h = harness().await;
    let (channel, _sink) = collecting_channel();
    let info = h
        .manager
        .open(h.fixture_spec(&["hang"]), channel)
        .await
        .expect("open");
    let session = h.manager.get(&info.id).expect("session");

    assert_eq!(
        pgid_via_ps(info.pid),
        Some(info.pid),
        "`ps -o pgid=` must report the child as its own group leader"
    );
    assert_eq!(info.pgid, info.pid, "the recorded pgid agrees with the kernel");
    assert_eq!(
        session.foreground_pgid(),
        Some(info.pid),
        "tcgetpgrp on the master must name the child's group; PINNED §2 says \
         portable-pty 0.9 exposes it as process_group_leader()"
    );

    h.manager
        .close(&info.id, CloseMode::Gentle)
        .await
        .expect("close");
    assert!(wait_until_gone(info.pid, SETTLE));
}

// ---------------------------------------------------------------------------
// acceptance 3 — a wedged view never back-pressures the child
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(pty_load)]
async fn a_fifty_mib_flood_finishes_while_the_view_is_wedged_and_asks_for_a_resync() {
    const TOTAL: u64 = 52_428_800; // 50 MiB, M1-PLAN T15 acceptance 3.

    let h = harness().await;
    let gate = Arc::new(AtomicBool::new(false));
    let (channel, delivered) = wedged_channel(gate.clone());

    let mut spec = h.fixture_spec(&["binary-flood", &TOTAL.to_string()]);
    spec.raw_output = true; // byte accounting, not terminal cooking
    let started = Instant::now();
    let info = h.manager.open(spec, channel).await.expect("open");
    let session = h.manager.get(&info.id).expect("session");

    assert!(
        wait_until_exited(&session, Duration::from_secs(120)),
        "the flood must complete even though the view never took a chunk"
    );
    let elapsed = started.elapsed();
    assert_eq!(
        session.exit_info().and_then(|e| e.code),
        Some(0),
        "the fixture exited cleanly"
    );
    assert_eq!(
        session.next_seq(),
        TOTAL,
        "every byte the fixture wrote reached the ring"
    );
    assert_eq!(
        session.info().first_retained_seq,
        TOTAL - DEFAULT_RING_BYTES as u64,
        "the ring retains its last 2 MiB and nothing more"
    );
    assert!(
        delivered.load(Ordering::Relaxed) < TOTAL as usize,
        "a wedged view is supposed to miss frames"
    );

    // Let the view come back. The dropped frames become a resync, not a corrupt
    // screen: the coalescer refuses to send a chunk that is not contiguous.
    gate.store(true, Ordering::Release);
    assert!(
        wait_for(Duration::from_secs(30), || !h
            .recorder
            .named("gilde://pty/resync")
            .is_empty()),
        "dropping frames must publish gilde://pty/resync"
    );
    let resync = h.recorder.named("gilde://pty/resync");
    assert_eq!(resync[0]["sessionId"], Value::String(info.id.clone()));
    assert_eq!(
        resync[0]["nextSeq"],
        Value::from(TOTAL),
        "the resync carries the ring head the view must catch up to: {:?}",
        resync[0]
    );

    eprintln!(
        "[T15] 50 MiB through a wedged view in {:?} ({:.1} MB/s produced)",
        elapsed,
        TOTAL as f64 / 1_000_000.0 / elapsed.as_secs_f64()
    );
}

// ---------------------------------------------------------------------------
// acceptance 4 — no byte becomes a String
// ---------------------------------------------------------------------------

#[test]
fn nothing_under_src_pty_turns_pty_bytes_into_a_string() {
    for (name, body) in pty_sources() {
        for banned in ["from_utf8_lossy", "String::from_utf8"] {
            assert!(
                !body.contains(banned),
                "src/pty/{name} contains `{banned}`; PTY bytes stay bytes until \
                 xterm.js decodes them exactly once (DESIGN §0 rule 4)"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// acceptance 5 — ring and coalescer sizes
// ---------------------------------------------------------------------------

#[test]
fn the_ring_and_the_coalescer_use_the_sizes_design_specifies() {
    assert_eq!(DEFAULT_RING_BYTES, 2 * 1024 * 1024, "DESIGN §6-7: 2 MiB ring");
    assert_eq!(FLUSH_INTERVAL, Duration::from_millis(8), "DESIGN §6-2: 8 ms");
    assert_eq!(FLUSH_BYTES, 32 * 1024, "DESIGN §6-2: 32 KiB");
    assert_eq!(READ_CHUNK, 64 * 1024, "DESIGN §7: 64 KiB reads");
    assert_eq!(QUEUE_FRAMES, 64);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_burst_of_tiny_writes_arrives_coalesced() {
    let h = harness().await;
    let (channel, sink) = collecting_channel();
    // 400 one-byte writes as fast as a shell can make them.
    let mut spec = h.script_spec(
        "tiny.sh",
        "i=0\nwhile [ $i -lt 400 ]; do printf x; i=$((i+1)); done\n",
    );
    spec.raw_output = true;
    let info = h.manager.open(spec, channel).await.expect("open");
    let session = h.manager.get(&info.id).expect("session");

    assert!(wait_until_exited(&session, SETTLE));
    let total = wait_for_quiet_sink(&sink, SETTLE);

    let state = sink.lock().expect("sink");
    assert_eq!(total, 400, "every byte arrived");
    assert!(state.bytes.iter().all(|b| *b == b'x'));
    assert_eq!(state.gaps, 0, "a healthy view sees a contiguous stream");
    assert!(
        state.chunks <= 40,
        "400 one-byte writes must not become 400 ipc messages; the 8 ms window \
         coalesced them into {} chunks",
        state.chunks
    );
}

// ---------------------------------------------------------------------------
// acceptance 6 — the snapshot wire format
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_snapshot_is_a_sixteen_byte_header_followed_by_raw_ring_bytes() {
    let h = harness().await;
    let (channel, sink) = collecting_channel();
    let mut spec = h.fixture_spec(&["binary-flood", "40000"]);
    spec.raw_output = true;
    let info = h.manager.open(spec, channel).await.expect("open");
    let session = h.manager.get(&info.id).expect("session");

    assert!(wait_until_exited(&session, SETTLE));
    wait_for_quiet_sink(&sink, SETTLE);

    let snapshot = h.manager.snapshot(&info.id).expect("snapshot");
    assert_eq!(&snapshot[0..4], b"GLDP", "magic");
    assert_eq!(snapshot[4], 1, "version");
    assert_eq!(&snapshot[5..8], &[0, 0, 0], "reserved");
    assert_eq!(
        parse_snapshot_header(&snapshot),
        Some((1, 40_000)),
        "offset 8 carries next_seq as u64 LE"
    );
    assert_eq!(
        &snapshot[SNAPSHOT_HEADER_LEN..],
        &expected_flood(40_000)[..],
        "the payload is the raw ring, not an encoding of it"
    );
    assert_eq!(session.next_seq(), 40_000);
}

// ---------------------------------------------------------------------------
// acceptance 7 — byte fidelity
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn flood_bytes_survive_the_pty_and_the_ring_byte_for_byte() {
    const TOTAL: usize = 1_048_576; // 16 read boundaries, well inside the ring.

    let h = harness().await;
    let (channel, sink) = collecting_channel();
    let mut spec = h.fixture_spec(&["binary-flood", &TOTAL.to_string()]);
    spec.raw_output = true;
    let info = h.manager.open(spec, channel).await.expect("open");
    let session = h.manager.get(&info.id).expect("session");

    assert!(wait_until_exited(&session, SETTLE));
    wait_for_quiet_sink(&sink, SETTLE);

    let expected = expected_flood(TOTAL);
    let snapshot = h.manager.snapshot(&info.id).expect("snapshot");
    let ring = &snapshot[SNAPSHOT_HEADER_LEN..];

    assert_eq!(ring.len(), TOTAL, "the whole flood fits in a 2 MiB ring");
    assert_eq!(ring, &expected[..], "the ring is byte-identical to the source");

    let state = sink.lock().expect("sink");
    assert!(state.chunks > 0, "the view was fed something");
    for (seq, bytes) in &state.pieces {
        let at = *seq as usize;
        assert_eq!(
            &expected[at..at + bytes.len()],
            &bytes[..],
            "the chunk stamped seq {seq} is not the run of bytes the fixture \
             wrote at that offset; a chunk that lies about its offset is what \
             makes a remount paint garbage"
        );
    }
    if state.gaps == 0 {
        assert_eq!(
            state.bytes, expected,
            "the delivered chunk stream is byte-identical to the source"
        );
    } else {
        // A loaded machine can outrun the ipc channel even here. That is a
        // documented outcome, not a corruption: the view is told to re-snapshot
        // instead of being handed bytes that do not join up.
        assert!(
            !h.recorder.named("gilde://pty/resync").is_empty(),
            "{} frames were dropped without publishing gilde://pty/resync",
            state.gaps
        );
    }
    drop(state);

    // The specific hostility this fixture was built for: a 4-byte U+1F600 that
    // straddles a 64 KiB read boundary, invalid UTF-8 on both sides of it.
    for boundary in [65_536usize, 131_072, 983_040] {
        let straddle = &ring[boundary - 1..boundary + 3];
        assert_eq!(
            straddle,
            &fixtures::flood::STRADDLING_CHAR[..],
            "the emoji spanning read boundary {boundary} came through intact"
        );
        assert_eq!(
            &ring[boundary - 8..boundary + 8],
            &fixtures::flood::MARKER[..],
            "the whole marker, invalid UTF-8 included, survived boundary {boundary}"
        );
    }
    assert!(
        std::str::from_utf8(ring).is_err(),
        "the fixture emits invalid UTF-8 on purpose; a path that decoded would \
         have replaced it and this assertion would fail"
    );
}

// ---------------------------------------------------------------------------
// acceptance 8 — winsize
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_child_gets_the_opening_winsize_and_a_resize_reaches_it_as_sigwinch() {
    let h = harness().await;
    let (channel, sink) = collecting_channel();
    // `stty size` reads the tty on stdin; the trap re-reads it on SIGWINCH,
    // which is exactly what a TUI redraw is driven by.
    let spec = h
        .script_spec(
            "winsize.sh",
            "stty size\ntrap 'stty size' WINCH\nwhile :; do sleep 0.1; done\n",
        )
        .size(100, 30);
    let info = h.manager.open(spec, channel).await.expect("open");
    let session = h.manager.get(&info.id).expect("session");

    assert_eq!((info.cols, info.rows), (100, 30));
    assert_eq!(session.tty_size(), Some((100, 30)), "TIOCGWINSZ at open");
    assert!(
        wait_for(SETTLE, || text(&sink).contains("30 100")),
        "the child must read the opening winsize; it saw {:?}",
        text(&sink)
    );

    h.manager.resize(&info.id, 120, 40).expect("resize");
    assert_eq!(
        session.tty_size(),
        Some((120, 40)),
        "session_resize must call master.resize, not just bookkeep"
    );
    assert!(
        wait_for(SETTLE, || text(&sink).contains("40 120")),
        "SIGWINCH must reach the foreground group; the child printed {:?}",
        text(&sink)
    );
    assert_eq!(session.info().cols, 120);
    assert_eq!(session.info().rows, 40);

    // A zero from a view that has not laid out yet never reaches the kernel:
    // codex draws nothing into a 0×0 pty (PINNED §9-4).
    h.manager.resize(&info.id, 0, 0).expect("resize");
    assert_eq!(session.tty_size(), Some((80, 24)));

    let report = h
        .manager
        .close(&info.id, CloseMode::Standard)
        .await
        .expect("close");
    assert!(report.survivors.is_empty(), "{report:?}");
    assert!(wait_until_gone(info.pid, SETTLE));
}

// ---------------------------------------------------------------------------
// acceptance 8 — session_write is bytes, not text
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn session_write_delivers_the_exact_bytes_it_was_given() {
    let h = harness().await;
    let (channel, sink) = collecting_channel();
    // `cat` echoes its stdin back to the tty; ECHO would too, so the assertion
    // below only checks that the bytes arrived, not how many times.
    let spec = h.spec(vec!["/bin/cat".to_string()]);
    let info = h.manager.open(spec, channel).await.expect("open");
    let session = h.manager.get(&info.id).expect("session");

    // A multi-byte character split across two writes: if anything on the path
    // decoded, the replacement character would show up instead.
    let hangul = "한글".as_bytes().to_vec();
    h.manager.write(&info.id, &hangul[..3]).expect("write");
    h.manager.write(&info.id, &hangul[3..]).expect("write");
    h.manager.write(&info.id, b"\r").expect("write");

    assert!(
        wait_for(SETTLE, || {
            let seen = sink.lock().expect("sink").bytes.clone();
            seen.windows(hangul.len()).any(|w| w == &hangul[..])
        }),
        "the child echoed {:?}",
        text(&sink)
    );

    h.manager.write(&info.id, &[0x04]).expect("eof"); // ^D ends cat
    assert!(wait_until_exited(&session, SETTLE));
    assert_eq!(session.exit_info().and_then(|e| e.code), Some(0));
}

// ---------------------------------------------------------------------------
// acceptance 9 — the three close modes
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn gentle_close_hups_the_foreground_group() {
    let h = harness().await;
    let (channel, _sink) = collecting_channel();
    let info = h
        .manager
        .open(h.fixture_spec(&["hang"]), channel)
        .await
        .expect("open");

    let report = h
        .manager
        .close(&info.id, CloseMode::Gentle)
        .await
        .expect("close");

    assert_eq!(report.fence, "foregroundGroup");
    assert_eq!(report.requested, vec![info.pid], "SIGHUP went to tcgetpgrp");
    assert!(report.survivors.is_empty(), "{report:?}");
    assert!(wait_until_gone(info.pid, SETTLE));
    assert!(h.manager.get(&info.id).is_err(), "a closed session is retired");
}

#[tokio::test(flavor = "multi_thread")]
async fn signalling_a_session_reaches_the_foreground_group() {
    let h = harness().await;
    let (channel, _sink) = collecting_channel();
    let info = h
        .manager
        .open(h.fixture_spec(&["hang"]), channel)
        .await
        .expect("open");
    let session = h.manager.get(&info.id).expect("session");

    h.manager.signal(&info.id, PtySignal::Int).expect("signal");
    assert!(wait_until_exited(&session, SETTLE));
    assert_eq!(session.exit_info().and_then(|e| e.signal), Some(libc::SIGINT));
}

#[tokio::test(flavor = "multi_thread")]
async fn standard_close_types_control_c_before_it_ever_kills() {
    let h = harness().await;
    let (channel, _sink) = collecting_channel();
    let info = h
        .manager
        .open(h.fixture_spec(&["hang"]), channel)
        .await
        .expect("open");
    let session = h.manager.get(&info.id).expect("session");

    let report = h
        .manager
        .close(&info.id, CloseMode::Standard)
        .await
        .expect("close");

    assert_eq!(
        report.fence, "exitedOnInterrupt",
        "^C through the line discipline is enough for a child that honours it"
    );
    assert!(report.requested.is_empty(), "no signal was sent by us");
    assert!(report.survivors.is_empty());
    assert_eq!(
        session.exit_info().and_then(|e| e.signal),
        Some(libc::SIGINT),
        "the kernel, not us, delivered the SIGINT"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn standard_close_escalates_to_kill_tree_when_the_tui_ignores_the_keys() {
    let h = harness().await;
    let (channel, sink) = collecting_channel();
    // Ignores ^C and never reads stdin, so ^D means nothing to it either.
    let spec = h.script_spec(
        "stubborn.sh",
        "trap '' INT\ntrap '' QUIT\necho armed\nwhile :; do sleep 0.2; done\n",
    );
    let info = h.manager.open(spec, channel).await.expect("open");
    // The script announces itself *after* installing the traps, so the close
    // below cannot win a race against a shell that is still starting up.
    assert!(
        wait_for(SETTLE, || text(&sink).contains("armed")),
        "the stubborn script never armed its traps"
    );

    let started = Instant::now();
    let report = h
        .manager
        .close(&info.id, CloseMode::Standard)
        .await
        .expect("close");
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "the two-second grace of DESIGN §6-5 is real patience, not a formality"
    );
    assert_eq!(report.fence, "verified", "kill_tree ran its identity fence");
    assert!(
        report.requested.contains(&info.pid),
        "the escalation targeted the session's own child: {report:?}"
    );
    assert!(report.survivors.is_empty(), "{report:?}");
    assert!(wait_until_gone(info.pid, SETTLE));
}

#[tokio::test(flavor = "multi_thread")]
async fn detach_keeps_the_child_alive_and_says_so_in_the_registry() {
    let h = harness().await;
    let (channel, sink) = collecting_channel();
    let info = h
        .manager
        .open(h.fixture_spec(&["hang"]), channel)
        .await
        .expect("open");
    let session = h.manager.get(&info.id).expect("session");

    let report = h
        .manager
        .close(&info.id, CloseMode::Detach)
        .await
        .expect("close");

    assert_eq!(report.refused_reason.as_deref(), Some("detached"));
    assert_eq!(report.survivors, vec![info.pid]);
    assert!(pid_alive(info.pid), "a detached child keeps running");
    assert!(session.is_detached());
    assert!(matches!(
        session.status_report().status,
        SessionStatus::Detached
    ));

    let spawn_id = info.spawn_id.clone();
    let row = h
        .db
        .read(move |conn| dao::proc::get(conn, &spawn_id))
        .await
        .expect("read")
        .expect("row");
    assert!(row.keep_alive, "keep_alive = 1, or the reaper would take it");
    assert_eq!(row.state, ProcState::Running);

    let session_id = info.id.clone();
    let session_row = h
        .db
        .read(move |conn| dao::pty_session::get(conn, &session_id))
        .await
        .expect("read")
        .expect("row");
    assert!(session_row.detached);

    // Delivery stopped; the reader has not.
    let before = sink.lock().expect("sink").bytes.len();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(sink.lock().expect("sink").bytes.len(), before);

    kill_pid(info.pid, libc::SIGKILL);
    assert!(wait_until_gone(info.pid, SETTLE));
}

#[test]
fn the_pty_lane_never_calls_child_kill() {
    let mut sources = pty_sources();
    sources.push((
        "commands/pty.rs".to_string(),
        source_of("src/commands/pty.rs"),
    ));
    for (name, body) in sources {
        for line in body.lines() {
            let code = line.split("//").next().unwrap_or("");
            assert!(
                !code.contains("child.kill()") && !code.contains(".kill()"),
                "{name} calls a Child::kill()-shaped method; DESIGN §6-5 routes \
                 every termination through kill_tree so a child's own children \
                 are not left behind: {line}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// acceptance 10 — the prompt gate
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(pty_load)]
async fn send_prompt_waits_for_idle_and_wraps_multiple_lines_in_bracketed_paste() {
    let h = harness().await;
    let (channel, _sink) = collecting_channel();
    let captured = h.path().join("typed.bin");
    let gate = h.path().join("prompt.gate");
    // `cat` writes what the tty handed it into a file, so the assertion is
    // about the bytes the child received, not about what the terminal echoed.
    //
    // The lines around it are what T16's gate needs. `$ ` is a prompt the
    // `shell` signatures recognise, so the session reaches `Idle` at 0.85 —
    // DESIGN §6-5 refuses to type into a terminal whose prompt we do *not*
    // recognise, and a bare `cat` is exactly that. `stty -echo` keeps the
    // recognised prompt as the newest visible line: without it the echo of
    // what we just typed would be, and the second prompt would be refused.
    //
    // The dot loop is what makes the "too early" half of this test a statement
    // about the session rather than about the scheduler. The old script printed
    // its prompt immediately, which left roughly 750 ms (one output burst plus
    // `idle_after_ms`) in which the probe had to run; on a loaded machine the
    // session had already settled by the time the test thread was scheduled,
    // and the probe was answered `sent: true`. Now the child stays *provably*
    // busy — one byte every 50 ms against an `OUTPUT_BURST_MS` of 500 — until
    // this test creates the gate file, so the not-yet-idle window is opened and
    // closed by the test, not by a timer.
    let spec = h.script_spec(
        "capture.sh",
        &format!(
            "stty -echo\nwhile [ ! -f {gate} ]; do printf '.'; sleep 0.05; done\n\
             printf '\\n$ '\nexec cat > {captured}\n",
            gate = gate.to_string_lossy(),
            captured = captured.to_string_lossy()
        ),
    );
    let info = h.manager.open(spec, channel).await.expect("open");
    let session = h.manager.get(&info.id).expect("session");

    assert!(
        wait_for(SETTLE, || matches!(
            session.status_report().status,
            SessionStatus::Working { .. }
        )),
        "the dot loop must show up as a working session"
    );
    let refused = h.manager.send_prompt(&info.id, "too early").expect("prompt");
    assert!(
        !refused.sent,
        "a session that has not settled is not an idle one: {refused:?}"
    );
    // The recognised prompt has not been printed yet, so a *confident* idle is
    // impossible here whatever the scheduler does: the three reasons below are
    // the same refusal — the gate held and nothing was typed into a terminal
    // whose prompt we have not recognised (DESIGN §6-5).
    assert!(
        matches!(
            refused.refused_reason.as_deref(),
            Some("status:starting" | "status:working" | "lowConfidence")
        ),
        "refused because of the status, not something else: {:?}",
        refused.refused_reason
    );

    // Let the child print the prompt it will be judged idle on.
    std::fs::write(&gate, b"go").expect("open the gate");
    assert!(
        wait_for(SETTLE, || matches!(
            session.status_report().status,
            SessionStatus::Idle
        )),
        "the engine should settle to idle after the configured quiet window"
    );
    assert!(
        session.status_report().confidence >= 0.6,
        "a recognised shell prompt is a confident idle, which is what opens the \
         gate (DESIGN §6-5)"
    );
    // A refusal writes nothing, so the session is still idle afterwards.
    let empty = h.manager.send_prompt(&info.id, "   \n\n").expect("prompt");
    assert!(!empty.sent, "whitespace is not a prompt");
    assert_eq!(empty.refused_reason.as_deref(), Some("empty"));

    let single = h.manager.send_prompt(&info.id, "hello").expect("prompt");
    assert!(single.sent);
    assert!(!single.bracketed_paste, "one line needs no paste guard");

    // The echo of what we just typed counts as output, so wait for idle again
    // before the second prompt — the gate is doing its job.
    assert!(wait_for(SETTLE, || matches!(
        session.status_report().status,
        SessionStatus::Idle
    )));
    let multi = h
        .manager
        .send_prompt(&info.id, "line one\nline two")
        .expect("prompt");
    assert!(multi.sent);
    assert!(
        multi.bracketed_paste,
        "multiple lines are wrapped so the TUI submits once, at the end"
    );

    h.manager.write(&info.id, &[0x04]).expect("eof");
    assert!(wait_until_exited(&session, SETTLE));

    let typed = std::fs::read(&captured).expect("read captured");
    assert_eq!(
        typed,
        b"hello\n\x1b[200~line one\nline two\x1b[201~\n".to_vec(),
        "got {:?}",
        String::from_utf8_lossy(&typed)
    );
}

// ---------------------------------------------------------------------------
// acceptance 11 — the exit watcher
// ---------------------------------------------------------------------------

// Every wait in this test is keyed on the event it is waiting for — the
// session's own exit flag, the published event, the graph patch, and finally
// the two rows' contents. The version that failed under `--no-fail-fast` read
// the two rows **once**, immediately, which raced the exit watcher's own
// database task: on a busy machine the read landed between "the session says it
// exited" and "the watcher finished writing" (PINNED §23-4). Nothing here
// sleeps for a fixed time, and the serial group keeps the floods out of the way.
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(pty_load)]
async fn the_exit_watcher_reports_the_code_closes_the_rows_and_leaves_no_zombie() {
    let h = harness().await;
    let (channel, _sink) = collecting_channel();
    let info = h
        .manager
        .open(h.fixture_spec(&["normal", "3"]), channel)
        .await
        .expect("open");
    let session = h.manager.get(&info.id).expect("session");

    assert!(wait_until_exited(&session, SETTLE));
    let exit = session.exit_info().expect("exit info");
    assert_eq!(exit.code, Some(3));
    assert_eq!(exit.signal, None);
    assert!(
        wait_for(SETTLE, || !pid_alive(info.pid)),
        "waitpid ran: an unreaped zombie would still answer kill(pid, 0)"
    );

    assert!(
        wait_for(SETTLE, || !h.recorder.named("gilde://session/exit").is_empty()),
        "gilde://session/exit must be published"
    );
    let event = h.recorder.named("gilde://session/exit")[0].clone();
    assert_eq!(event["sessionId"], Value::String(info.id.clone()));
    assert_eq!(event["exitCode"], Value::from(3));
    assert_eq!(event["pid"], Value::from(info.pid));
    assert!(
        wait_for(SETTLE, || h.recorder.patch_count() > 0),
        "the session node was patched; the bus coalesces before it publishes, \
         so this is a wait rather than an immediate read"
    );

    let spawn_id = info.spawn_id.clone();
    let row = wait_for_row(SETTLE, || {
        let db = h.db.clone();
        let spawn_id = spawn_id.clone();
        async move {
            db.read(move |conn| dao::proc::get(conn, &spawn_id))
                .await
                .ok()
                .flatten()
                .filter(|row| row.state == ProcState::Exited)
        }
    })
    .await
    .expect("the registry row must reach `exited`");
    assert_eq!(row.exit_code, Some(3));

    let session_id = info.id.clone();
    let session_row = wait_for_row(SETTLE, || {
        let db = h.db.clone();
        let session_id = session_id.clone();
        async move {
            db.read(move |conn| dao::pty_session::get(conn, &session_id))
                .await
                .ok()
                .flatten()
                .filter(|row| row.closed_at.is_some())
        }
    })
    .await
    .expect("the pty_session row must be closed");
    assert_eq!(session_row.exit_code, Some(3));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_exit_watcher_reports_a_signal_as_a_number_not_as_prose() {
    let h = harness().await;
    let (channel, _sink) = collecting_channel();
    let info = h
        .manager
        .open(h.fixture_spec(&["hang"]), channel)
        .await
        .expect("open");
    let session = h.manager.get(&info.id).expect("session");

    kill_pid(info.pid, libc::SIGKILL);
    assert!(wait_until_exited(&session, SETTLE));

    let exit = session.exit_info().expect("exit info");
    assert_eq!(exit.signal, Some(libc::SIGKILL));
    assert_eq!(exit.code, None);
    assert!(matches!(
        session.status_report().status,
        SessionStatus::Exited {
            signal: Some(9),
            ..
        }
    ));
}

// ---------------------------------------------------------------------------
// acceptance 12 — the R1 throughput gate
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(pty_load)]
async fn four_simultaneous_streams_report_their_sustained_throughput() {
    const PER_SESSION: u64 = 12 * 1024 * 1024; // 48 MiB in total
    const PANELS: usize = 4;

    let h = harness().await;
    let mut sessions = Vec::new();
    let mut counters = Vec::new();

    let started = Instant::now();
    for _ in 0..PANELS {
        let (channel, count) = counting_channel();
        let mut spec = h.fixture_spec(&["binary-flood", &PER_SESSION.to_string()]);
        spec.raw_output = true;
        let info = h.manager.open(spec, channel).await.expect("open");
        sessions.push(h.manager.get(&info.id).expect("session"));
        counters.push(count);
    }

    assert!(
        wait_for(Duration::from_secs(120), || sessions
            .iter()
            .all(|s| s.has_exited() && s.saw_eof())),
        "all four panels must finish"
    );
    let elapsed = started.elapsed();

    let produced: u64 = sessions.iter().map(|s| s.next_seq()).sum();
    assert_eq!(
        produced,
        PER_SESSION * PANELS as u64,
        "no byte was lost between the four ptys and their rings"
    );

    // Delivery lags the ring by up to one flush window; give it a moment before
    // reading the counters, then report both numbers.
    std::thread::sleep(Duration::from_millis(500));
    let delivered: usize = counters.iter().map(|c| c.load(Ordering::Relaxed)).sum();
    let mb_s = produced as f64 / 1_000_000.0 / elapsed.as_secs_f64();

    eprintln!(
        "[T15 R1] {PANELS} panels × {} MiB = {:.0} MB in {:?} → {mb_s:.1} MB/s aggregate \
         ({:.1}% delivered to the views)",
        PER_SESSION / 1024 / 1024,
        produced as f64 / 1_000_000.0,
        elapsed,
        delivered as f64 * 100.0 / produced as f64
    );

    assert!(
        mb_s >= 1.0,
        "four streams managed only {mb_s:.2} MB/s, which is below even the \
         floor this assertion guards; the measured number belongs in \
         docs/PINNED-VERSIONS.md"
    );
}

// ---------------------------------------------------------------------------
// acceptance 14 — a flood never wedges the session
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(pty_load)]
async fn a_five_megabyte_flood_never_makes_the_command_surface_wait() {
    let h = harness().await;
    let (channel, _count) = counting_channel();
    // The acceptance criterion's own example, as a script so no argv reaches a
    // shell parser: `yes | head -c 5000000`.
    let spec = h.script_spec("yes.sh", "yes | head -c 5000000\n");
    let info = h.manager.open(spec, channel).await.expect("open");
    let session = h.manager.get(&info.id).expect("session");

    // While the flood is in flight, every command a user could issue must
    // return promptly — that is what "the UI does not freeze" means here.
    let mut worst = Duration::ZERO;
    for i in 0..40 {
        let at = Instant::now();
        h.manager.resize(&info.id, 100 + (i % 3), 30).expect("resize");
        let _ = h.manager.status(&info.id).expect("status");
        let _ = h.manager.list();
        let _ = h.manager.snapshot(&info.id).expect("snapshot");
        worst = worst.max(at.elapsed());
        std::thread::sleep(Duration::from_millis(5));
    }

    assert!(
        worst < Duration::from_millis(250),
        "the slowest command round-trip during the flood was {worst:?}"
    );
    assert!(wait_until_exited(&session, SETTLE));
    assert!(session.next_seq() >= 5_000_000);
    eprintln!("[T15] worst command latency during a 5 MB flood: {worst:?}");
}

// ---------------------------------------------------------------------------
// acceptance 14 — manual, against the real tools (`-- --ignored`)
// ---------------------------------------------------------------------------

fn resolve(binary: &str) -> Option<PathBuf> {
    let path = std::env::var("PATH").unwrap_or_default();
    let extra = std::env::var("HOME").map(|h| format!("{h}/.nvm/versions/node")).ok();
    let mut roots: Vec<PathBuf> = std::env::split_paths(&path).collect();
    if let Some(nvm) = extra {
        if let Ok(entries) = std::fs::read_dir(nvm) {
            for entry in entries.flatten() {
                roots.push(entry.path().join("bin"));
            }
        }
    }
    roots.into_iter().map(|d| d.join(binary)).find(|p| p.is_file())
}

/// Manual: `claude` paints its TUI in a fresh worktree, takes input, resizes
/// and closes cleanly (M1-PLAN T15 acceptance 14).
///
/// Run with `cargo test --test pty_fidelity -- --ignored manual_claude`.
/// It stops at the folder-trust gate on purpose — PINNED §9-3 — so it costs no
/// tokens and starts no conversation.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "drives the real claude CLI"]
async fn manual_claude_draws_a_tui_and_closes_cleanly() {
    let h = harness().await;
    let Some(claude) = resolve("claude") else {
        panic!("claude is not on this machine's PATH");
    };
    let (channel, sink) = collecting_channel();
    let spec = h
        .spec(vec![claude.to_string_lossy().into_owned()])
        .size(120, 40);
    let info = h.manager.open(spec, channel).await.expect("open");
    let session = h.manager.get(&info.id).expect("session");

    assert!(
        // The words are separated by cursor-positioning escapes, which is why
        // this looks for them one at a time rather than as a phrase.
        wait_for(Duration::from_secs(30), || {
            let painted = text(&sink);
            painted.contains("trust") && painted.contains("Yes,")
        }),
        "claude should paint its folder-trust screen; it painted {:?}",
        text(&sink)
    );
    let first_screen = sink.lock().expect("sink").bytes.len();
    eprintln!("[T15 manual] claude painted {first_screen} bytes at {claude:?}");
    eprintln!("[T15 manual] status: {:?}", session.status_report().status);

    // It takes input: a cursor-down repaints the selection.
    h.manager.write(&info.id, b"\x1b[B").expect("write");
    assert!(
        wait_for(Duration::from_secs(10), || sink
            .lock()
            .expect("sink")
            .bytes
            .len()
            > first_screen + 20),
        "claude should repaint after a key"
    );
    let after_key = sink.lock().expect("sink").bytes.len();

    // It resizes: the kernel gets the new winsize and the TUI redraws into it.
    h.manager.resize(&info.id, 100, 30).expect("resize");
    assert_eq!(session.tty_size(), Some((100, 30)));
    assert!(
        wait_for(Duration::from_secs(10), || sink
            .lock()
            .expect("sink")
            .bytes
            .len()
            > after_key + 50),
        "claude should redraw on SIGWINCH"
    );

    let report = h
        .manager
        .close(&info.id, CloseMode::Standard)
        .await
        .expect("close");
    eprintln!("[T15 manual] close report: {report:?}");
    assert!(report.survivors.is_empty(), "{report:?}");
    assert!(wait_until_gone(info.pid, SETTLE));
}

/// Manual: `vim` — the classic "does resize actually redraw" check.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "drives the real vim"]
async fn manual_vim_resizes_and_quits() {
    let h = harness().await;
    let Some(vim) = resolve("vim") else {
        panic!("vim is not on this machine's PATH");
    };
    let (channel, sink) = collecting_channel();
    let spec = h
        .spec(vec![vim.to_string_lossy().into_owned()])
        .size(80, 24);
    let info = h.manager.open(spec, channel).await.expect("open");
    let session = h.manager.get(&info.id).expect("session");

    assert!(
        wait_for(Duration::from_secs(15), || sink
            .lock()
            .expect("sink")
            .bytes
            .len()
            > 200),
        "vim should paint"
    );
    let before = sink.lock().expect("sink").bytes.len();

    h.manager.resize(&info.id, 132, 43).expect("resize");
    assert!(
        wait_for(Duration::from_secs(5), || sink
            .lock()
            .expect("sink")
            .bytes
            .len()
            > before + 200),
        "vim must redraw on SIGWINCH"
    );
    assert_eq!(session.tty_size(), Some((132, 43)));
    eprintln!("[T15 manual] vim redrew after resize");

    h.manager.write(&info.id, b"\x1b:q!\r").expect("write");
    assert!(wait_until_exited(&session, SETTLE));
    assert_eq!(session.exit_info().and_then(|e| e.code), Some(0));
}
