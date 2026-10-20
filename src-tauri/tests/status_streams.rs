//! The status engine against **bytes a real agent CLI actually produced** —
//! DESIGN §6-4, M1-PLAN T16.6.
//!
//! The unit tests in `status::engine` prove the state machine. They cannot prove
//! the thing that actually breaks in the field: that `claude`'s and `codex`'s
//! real escape-sequence output, run through [`AnsiTail`], still leaves the lines
//! the signature patterns expect. So this file replays streams captured from
//! this machine on 2026-09-07 (`claude` 2.1.263, `codex` 0.145.0) and asserts
//! the `Idle` / `Working` / `WaitingForInput` sequence that comes out.
//!
//! ## The fixtures
//!
//! `tests/fixtures/streams/*.bin` are raw pty bytes — no framing, no timestamps,
//! exactly what the reader thread pushed into the ring. They were captured by
//! the `#[ignore]`d test at the bottom of this file, which opens a real pty
//! through `PtyManager` (120×32, because `codex` draws nothing into a 0×0 one)
//! and dumps the delivered bytes:
//!
//! ```text
//! cargo test --test status_streams -- --ignored --nocapture
//! ```
//!
//! Re-run it when a CLI's TUI changes and the patterns need to move with it.
//! The captures are small on purpose (a few KiB each): they are read as
//! *evidence about one screen*, not as a benchmark.
//!
//! ## Why replay and not a live run
//!
//! A test that launches `claude` needs a network, an authenticated config and a
//! model that is reachable. `codex` on this machine cannot reach its default
//! model at all (PINNED §9). A captured stream has none of those dependencies
//! and fails for exactly one reason: the inference stopped working.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use gilde_code_lib::agents::signatures::{SignatureFile, Signatures};
use gilde_code_lib::agents::{AgentKind, AgentRegistry};
use gilde_code_lib::paths::AppPaths;
use gilde_code_lib::config::ConfigStore;
use gilde_code_lib::pty::{CloseMode, OpenSpec, PtyChunk, PtyManager};
use gilde_code_lib::store::Db;
use gilde_code_lib::supervise::registry::{EnvBase, Supervisor};
use base64::Engine as _;
use tauri::ipc::{Channel, InvokeResponseBody};
use gilde_code_lib::model::status::{SessionStatus, StatusEvidence};
use gilde_code_lib::pty::AnsiTail;
use gilde_code_lib::status::engine::{StaticSignatures, StatusEngine};
use gilde_code_lib::store::now_ms;

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

fn streams_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/streams")
}

/// A wall-clock ceiling for a test, enforced from a watchdog thread that kills
/// the **process** when it expires.
///
/// The `#[ignore]`d tests in this file drive a real `claude` / `codex` through
/// a real pty. Every one of their waits is open-ended by nature: a CLI that
/// puts up a dialog nobody answers, a login prompt, a model that never
/// responds, a `manager.write` into a child that stopped reading. None of that
/// is a failure the test can detect — it is simply a wait that does not end.
///
/// Left alone, an agent-driven `cargo test … --ignored --nocapture` run of
/// these sits forever with the pty open. It happened twice in two days; the
/// second time two harnesses had been running for two and a half hours and the
/// agent that started them reported the machine clean.
///
/// The watchdog cannot be a panic: the test thread is the one that is stuck,
/// and a panic on the watchdog thread fails only that thread. It cannot be a
/// `tokio::time::timeout` either — that only covers the futures it wraps, not a
/// blocking `write(2)` into a stdout pipe nobody is draining, which is the
/// other way `--nocapture` hangs. So the watchdog calls [`std::process::exit`]
/// with cargo's own test-failure code (101). The run ends loudly, naming the
/// test and the limit.
///
/// The guard disarms on drop, so a normal finish never trips it.
#[must_use = "the deadline disarms as soon as the guard is dropped"]
fn deadline(name: &'static str, limit: std::time::Duration) -> DeadlineGuard {
    use std::sync::atomic::AtomicBool;

    let done = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&done);
    std::thread::spawn(move || {
        let started = std::time::Instant::now();
        while started.elapsed() < limit {
            if flag.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        if flag.load(Ordering::SeqCst) {
            return;
        }
        eprintln!(
            "\n\n*** {name}: still running after {}s — this test has a deadline because \
             it drives a real agent CLI and has hung before. Killing the process so the \
             run fails instead of holding a pty open.\n\n",
            limit.as_secs()
        );
        std::process::exit(101);
    });
    DeadlineGuard { done }
}

struct DeadlineGuard {
    done: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for DeadlineGuard {
    fn drop(&mut self) {
        self.done.store(true, Ordering::SeqCst);
    }
}

fn stream(name: &str) -> Vec<u8> {
    let path = streams_dir().join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("fixture {} is missing: {e}", path.display()))
}

fn engine() -> Arc<StatusEngine> {
    let engine = Arc::new(StatusEngine::new());
    engine.set_signatures(Arc::new(StaticSignatures::new(Signatures::compile(
        &SignatureFile::defaults(),
    ))));
    engine
}

/// Replay one stream into a fresh session and return the status it settles on.
///
/// The session is registered far enough in the past that the `Starting` grace
/// has expired, and the replay is followed by a wait longer than the burst
/// window, so what comes back is the *considered* answer rather than "bytes are
/// flowing" — which every stream would trivially produce.
fn settle(engine: &Arc<StatusEngine>, id: &str, agent: &str, bytes: &[u8]) -> SessionStatus {
    engine.register(id, agent, 200, now_ms() - 10_000);
    engine.on_output(id, bytes);
    std::thread::sleep(std::time::Duration::from_millis(620));
    engine.report(id).status
}

fn visible(bytes: &[u8]) -> Vec<String> {
    let mut tail = AnsiTail::new();
    tail.feed(bytes);
    tail.visible_lines(8)
}

fn joined(bytes: &[u8]) -> String {
    visible(bytes).join("\n")
}

// ---------------------------------------------------------------------------
// claude 2.1.263
// ---------------------------------------------------------------------------

#[test]
fn a_fresh_worktree_asks_claude_for_folder_trust_and_that_is_waiting_for_input() {
    // The very first thing a session in a new worktree shows: "Quick safety
    // check: Is this a project you created or one you trust?" over a two-option
    // menu. PINNED §9 records it as the reason the first status is
    // `WaitingForInput`, not `Idle` — and therefore the reason the UI needs
    // DESIGN §6-5's literal-key buttons rather than a text box.
    let bytes = stream("claude-waiting.bin");
    let engine = engine();
    let status = settle(&engine, "claude-waiting", "claude", &bytes);
    match &status {
        // The matched line is claude's confirm hint, "Enter to confirm · Esc to
        // cancel" — the newest waiting-class line on that screen, and exactly
        // the keys the literal-key buttons offer.
        SessionStatus::WaitingForInput { prompt } => assert!(
            prompt.contains("Enter to confirm"),
            "the prompt line is what the literal-key buttons are built from: {prompt:?}"
        ),
        other => panic!(
            "expected a folder-trust dialog, got {other:?}\ntail was:\n{}",
            joined(&bytes)
        ),
    }
    assert!(
        engine
            .report("claude-waiting")
            .evidence
            .iter()
            .any(|e| matches!(e, StatusEvidence::PatternMatch { .. })),
        "the Inspector has to be able to name the pattern that decided this"
    );
}

#[test]
fn claude_at_its_input_box_is_idle_with_a_matched_pattern() {
    let bytes = stream("claude-idle.bin");
    let engine = engine();
    let status = settle(&engine, "claude-idle", "claude", &bytes);
    assert!(
        matches!(status, SessionStatus::Idle),
        "expected idle, got {status:?}\ntail was:\n{}",
        joined(&bytes)
    );
    let report = engine.report("claude-idle");
    assert!(
        report.confidence >= 0.6,
        "a recognised prompt is a confident idle — this is what opens the \
         send_prompt gate (DESIGN §6-5), got {}",
        report.confidence
    );
    assert!(
        report
            .evidence
            .iter()
            .any(|e| matches!(e, StatusEvidence::PatternMatch { .. })),
        "the evidence names the pattern that matched: {:?}",
        report.evidence
    );
}

#[test]
fn claude_running_a_turn_is_working() {
    let bytes = stream("claude-working.bin");
    let status = settle(&engine(), "claude-working", "claude", &bytes);
    assert!(
        matches!(status, SessionStatus::Working { .. }),
        "expected working, got {status:?}\ntail was:\n{}",
        joined(&bytes)
    );
}

// ---------------------------------------------------------------------------
// codex 0.145.0
// ---------------------------------------------------------------------------

#[test]
fn codex_opens_on_a_dialog_that_needs_a_human_too() {
    // On this machine the first `codex` screen is its "Update available!" notice
    // ("1. Update now … 2. Skip … Press enter to continue") rather than a folder
    // trust prompt. Different dialog, same thing the status engine has to say:
    // nothing is running and a person has to answer.
    let bytes = stream("codex-waiting.bin");
    let status = settle(&engine(), "codex-waiting", "codex", &bytes);
    assert!(
        matches!(status, SessionStatus::WaitingForInput { .. }),
        "expected a blocking dialog, got {status:?}\ntail was:\n{}",
        joined(&bytes)
    );
}

#[test]
fn codex_at_its_composer_is_idle() {
    let bytes = stream("codex-idle.bin");
    let engine = engine();
    let status = settle(&engine, "codex-idle", "codex", &bytes);
    assert!(
        matches!(status, SessionStatus::Idle),
        "expected idle, got {status:?}\ntail was:\n{}",
        joined(&bytes)
    );
    assert!(engine.report("codex-idle").confidence >= 0.6);
}

#[test]
fn codex_running_a_turn_is_working() {
    let bytes = stream("codex-working.bin");
    let status = settle(&engine(), "codex-working", "codex", &bytes);
    assert!(
        matches!(status, SessionStatus::Working { .. }),
        "expected working, got {status:?}\ntail was:\n{}",
        joined(&bytes)
    );
}

// ---------------------------------------------------------------------------
// the sequence
// ---------------------------------------------------------------------------

/// One session, replayed in the order a real one lives: trust dialog → idle
/// prompt → a turn running → back to idle.
///
/// This is the assertion M1-PLAN T16.6 actually asks for: not that each screen
/// classifies in isolation, but that the *transitions* between them come out in
/// the right order from one continuous engine state.
#[test]
fn one_session_walks_waiting_then_idle_then_working_then_idle() {
    for (agent, files) in [
        (
            "claude",
            [
                "claude-waiting.bin",
                "claude-idle.bin",
                "claude-working.bin",
                "claude-idle.bin",
            ],
        ),
        (
            "codex",
            [
                "codex-waiting.bin",
                "codex-idle.bin",
                "codex-working.bin",
                "codex-idle.bin",
            ],
        ),
    ] {
        let engine = engine();
        let id = format!("{agent}-sequence");
        engine.register(&id, agent, 200, now_ms() - 10_000);

        let mut seen: Vec<String> = Vec::new();
        for file in files {
            engine.on_output(&id, &stream(file));
            // Past the 500 ms burst window: every screen would otherwise report
            // `Working` simply because bytes had just arrived.
            std::thread::sleep(std::time::Duration::from_millis(620));
            let kind = engine.report(&id).status.kind().to_string();
            if seen.last() != Some(&kind) {
                seen.push(kind);
            }
        }

        assert_eq!(
            seen,
            vec![
                "waitingForInput".to_string(),
                "idle".to_string(),
                "working".to_string(),
                "idle".to_string()
            ],
            "{agent} did not walk the expected sequence"
        );
    }
}

/// Every fixture must leave the tail with something a human could have read.
///
/// The failure this catches is the quiet one: a change to the parser that turns
/// a TUI frame into blank lines still classifies as `Idle` at 0.4 and looks
/// almost fine, while the whole third signal source is silently dead.
#[test]
fn every_captured_stream_still_yields_readable_lines() {
    let mut checked = 0;
    for entry in std::fs::read_dir(streams_dir()).expect("streams dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("bin") {
            continue;
        }
        let bytes = std::fs::read(&path).expect("fixture");
        let lines = visible(&bytes);
        assert!(
            !lines.is_empty(),
            "{} parsed to nothing at all",
            path.display()
        );
        assert!(
            lines.iter().any(|l| l.chars().count() > 3),
            "{} parsed to only fragments: {lines:?}",
            path.display()
        );
        assert!(
            !lines.iter().any(|l| l.contains('\u{1b}')),
            "{} left an escape byte in the visible text: {lines:?}",
            path.display()
        );
        checked += 1;
    }
    assert!(checked >= 6, "expected the six captured screens, found {checked}");
}

// ---------------------------------------------------------------------------
// the signature file, on disk
// ---------------------------------------------------------------------------

/// M1-PLAN T16.5: editing `agent-signatures.json` changes the classification
/// **within two seconds, with no restart**, and a pattern that does not compile
/// is one warning rather than a disabled agent.
///
/// The unit test in `status::engine` proves the engine re-reads its source on
/// every evaluation. This proves the other half: that a real editor writing a
/// real file reaches that source in time. `notify` watches the *directory*, and
/// the write below is the atomic temp-file-plus-rename an editor performs, which
/// is precisely the case an inode-bound watch would miss after the first edit.
#[test]
fn editing_the_signature_file_reclassifies_a_live_session_within_two_seconds() {
    let dir = tempfile::tempdir().expect("tempdir");
    let paths = AppPaths::for_home(dir.path());
    paths.ensure_all().expect("app dirs");
    let config =
        Arc::new(ConfigStore::open_at(&dir.path().join("config.json")).expect("config store"));
    let registry = Arc::new(AgentRegistry::without_db((paths).clone(), config));
    let _watch = registry.watch_signatures().expect("watch the signature file");

    let engine = Arc::new(StatusEngine::new());
    engine.set_signatures(registry.clone());
    engine.register("s1", "claude", 100, now_ms() - 10_000);
    // A line no bundled pattern knows about.
    engine.on_output("s1", b"AWAITING ORDERS, CAPTAIN\n");
    std::thread::sleep(std::time::Duration::from_millis(650));
    assert_eq!(
        engine.report("s1").confidence,
        0.4,
        "nothing matches it yet, and the engine says so rather than guessing"
    );

    // The edit, exactly as an editor performs it — and with one pattern that
    // cannot compile, which must cost only itself.
    let file = paths.signatures_file();
    let tmp = file.with_extension("json.editing");
    std::fs::write(
        &tmp,
        r#"{"version":1,"agents":{"claude":{"idle":["AWAITING ORDERS","(unclosed"]}}}"#,
    )
    .expect("write");
    std::fs::rename(&tmp, &file).expect("rename over the original");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut reclassified = false;
    while std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
        if engine.report("s1").confidence > 0.8 {
            reclassified = true;
            break;
        }
    }
    assert!(
        reclassified,
        "the edit did not reach the engine within two seconds; status is {:?}",
        engine.report("s1")
    );
    assert!(matches!(engine.report("s1").status, SessionStatus::Idle));
    assert_eq!(
        registry
            .signatures()
            .warnings()
            .iter()
            .filter(|w| w.contains("unclosed"))
            .count(),
        1,
        "the pattern that did not compile is one warning"
    );
    assert!(
        registry.signatures().for_agent("claude").is_some(),
        "and it did not disable the agent"
    );
}

// ---------------------------------------------------------------------------
// what the session row records
// ---------------------------------------------------------------------------

/// M1-PLAN T16.9: `pty_session.last_io_at` is written from real activity, so
/// T12's idle timeout measures a session that has gone quiet rather than a wall
/// clock that has gone forward.
#[tokio::test(flavor = "multi_thread")]
async fn the_session_row_records_the_last_real_io() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::open_in_memory().expect("db");
    let config =
        Arc::new(ConfigStore::open_at(&dir.path().join("config.json")).expect("config store"));
    let supervisor = Arc::new(
        Supervisor::open_instance(&db, config.clone(), "0.0.0-test")
            .await
            .expect("open_instance"),
    );
    let manager = Arc::new(PtyManager::new(supervisor, config));

    // A script that prints once and then waits, so the row is written by output
    // and not by the child exiting.
    let script = dir.path().join("talk.sh");
    std::fs::write(&script, "printf 'hello\\n'\nexec cat\n").expect("script");
    let mut spec = OpenSpec::new(
        AgentKind::Shell,
        dir.path(),
        vec!["/bin/sh".to_string(), script.to_string_lossy().into_owned()],
    );
    spec.idle_after_ms = 200;
    // Count what the child actually said. The wait below is then keyed on two
    // events in order — "the child spoke" and "the row was written" — instead of
    // on one flat 5 s budget that could not tell those two apart. That budget is
    // what failed 1 run in 3 under `cargo test --no-fail-fast` (PINNED §23-4):
    // `StatusEngine::persist_io` hands the write to a spawned task, so on a
    // saturated machine the column is written late, not never.
    let spoke = Arc::new(AtomicUsize::new(0));
    let heard = spoke.clone();
    let channel = Channel::new(move |_body| {
        heard.fetch_add(1, Ordering::Relaxed);
        Ok(())
    });
    let info = manager.open(spec, channel).await.expect("open");

    let opened_at = now_ms();
    let id = info.id.clone();
    // 30 s, not 5 s: this is a "has the write landed yet" wait, and the only
    // thing a tight budget buys is a flake. A real regression (the column is
    // never written) still fails, it just takes 30 s to say so.
    let budget = std::time::Duration::from_secs(30);
    let deadline = std::time::Instant::now() + budget;
    let mut recorded: Option<i64> = None;
    while std::time::Instant::now() < deadline {
        let id = id.clone();
        let row = db
            .read(move |conn| {
                gilde_code_lib::store::dao::pty_session::get(conn, &id)
            })
            .await
            .expect("read")
            .expect("the session row exists");
        if let Some(at) = row.last_io_at {
            recorded = Some(at);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let recorded = recorded.unwrap_or_else(|| {
        panic!(
            "last_io_at must be written once the child speaks; waited {budget:?} \
             and the view had received {} chunk(s) by then",
            spoke.load(Ordering::Relaxed)
        )
    });
    assert!(
        (recorded - opened_at).abs() < 30_000,
        "last_io_at is a real timestamp from this session, got {recorded} against {opened_at}"
    );

    let _ = manager.close(&info.id, CloseMode::Standard).await;
}

// ---------------------------------------------------------------------------
// the capture
// ---------------------------------------------------------------------------

/// Re-capture the fixtures from the real CLIs.
///
/// `#[ignore]`d: it drives two agent binaries, needs their login, and takes
/// about a minute. It is the *only* place the captures come from, so that a
/// fixture can always be regenerated rather than hand-edited.
///
/// Deadlined at 180 s. The scripted waits add up to about 80 s (10 s + four
/// 6 s answer rounds, twice), so three minutes is "a CLI stopped answering",
/// not "a slow machine". See [`deadline`].
///
/// ```text
/// cargo test --test status_streams -- --ignored --nocapture recapture
/// ```
#[tokio::test(flavor = "multi_thread")]
#[ignore = "drives the real claude and codex binaries"]
async fn recapture_the_streams_from_the_real_clis() {
    let _deadline = deadline(
        "recapture_the_streams_from_the_real_clis",
        std::time::Duration::from_secs(180),
    );
    for (agent, kind) in [("claude", AgentKind::Claude), ("codex", AgentKind::Codex)] {
        let Some(binary) = which_agent(agent) else {
            eprintln!("[T16] {agent} is not installed; skipping its capture");
            continue;
        };
        capture_one(agent, kind, &binary).await;
    }
}

/// What to type at a dialog, chosen by **reading the screen**.
///
/// Never by ordinal: the first version of this capture answered `1` at what it
/// assumed was a folder-trust prompt and hit `codex`'s "1. Update now (runs
/// `npm install -g @openai/codex`)" instead, which upgraded the developer's
/// global install mid-test. Matching on the option text costs three lines and
/// makes that class of accident impossible.
fn answer_for(lines: &[String]) -> Option<&'static str> {
    let screen = lines.join("\n");
    if screen.contains("Update available") || screen.contains("Update now") {
        // Never the update. "Skip" is option 2 in codex 0.145.0.
        return Some("2\r");
    }
    if screen.contains("Yes, I trust this folder") {
        // claude's menu is arrow-driven and defaults to "No, exit".
        return Some("\x1b[B\r");
    }
    if screen.contains("Press enter to continue") {
        return Some("\r");
    }
    if screen.contains("1. Yes") || screen.contains("Yes, allow") {
        return Some("1\r");
    }
    None
}

/// Does this screen carry the agent's own "a turn is running" marker?
fn shows_working(agent: &str, lines: &[String]) -> bool {
    let signatures = Signatures::compile(&SignatureFile::defaults());
    signatures
        .for_agent(agent)
        .and_then(|a| a.match_tail(lines))
        .is_some_and(|(pattern, _)| {
            matches!(
                pattern.class,
                gilde_code_lib::model::status::PatternClass::Working
            )
        })
}

async fn capture_one(name: &str, kind: AgentKind, binary: &Path) {
    use std::time::Duration;

    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::open_in_memory().expect("db");
    let config =
        Arc::new(ConfigStore::open_at(&dir.path().join("config.json")).expect("config store"));
    let supervisor = Arc::new(
        Supervisor::open_instance(&db, config.clone(), "0.0.0-capture")
            .await
            .expect("open_instance"),
    );
    let manager = Arc::new(PtyManager::new(supervisor, config));

    let collected: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = collected.clone();
    let channel = Channel::new(move |body| {
        let json = match body {
            InvokeResponseBody::Json(json) => json,
            InvokeResponseBody::Raw(_) => unreachable!("chunks are json"),
        };
        let chunk: PtyChunk = serde_json::from_str(&json).expect("chunk");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(chunk.b64.as_bytes())
            .expect("b64");
        sink.lock().expect("sink").extend_from_slice(&bytes);
        Ok(())
    });

    // 120×32, never 0×0: `codex` renders nothing into a zero-sized pty
    // (PINNED §9-4).
    let mut spec = OpenSpec::new(kind, dir.path(), vec![binary.to_string_lossy().into_owned()]);
    spec = spec.size(120, 32);
    // The same base `session_open` composes in the real app. `OpenSpec::new`'s
    // default carries only `PATH` and `HOME`, and `claude` needs `USER` to find
    // its credentials in the macOS keychain — without it the capture is a
    // permanent "Not logged in" screen (found the hard way).
    spec.env_base = EnvBase::from_process_env(std::env::var("PATH").unwrap_or_default());
    let info = manager.open(spec, channel).await.expect("open");

    // Each phase is written **cumulatively** — every byte since the session
    // opened. A phase's own delta would replay from a blank screen into half a
    // frame, because a TUI only repaints what changed; the whole stream replays
    // into exactly the screen the user was looking at.
    let so_far = || collected.lock().expect("sink").clone();
    let write_phase = |phase: &str, bytes: &[u8]| {
        let path = streams_dir().join(format!("{name}-{phase}.bin"));
        std::fs::create_dir_all(streams_dir()).expect("streams dir");
        std::fs::write(&path, bytes).expect("write fixture");
        eprintln!(
            "[T16] wrote {} ({} bytes)\n  tail: {:#?}",
            path.display(),
            bytes.len(),
            visible(bytes)
        );
    };

    // 1. A fresh directory: both CLIs open with a dialog that needs a human.
    //    Which dialog is not up to us — `claude` asks for folder trust,
    //    `codex` 0.145.0 leads with its "Update available!" notice — so the
    //    phase is named for the *status* it produces, not for the dialog.
    tokio::time::sleep(Duration::from_secs(10)).await;
    write_phase("waiting", &so_far());

    // 2. Answer whatever is on screen, by name, until nothing is asking.
    for _ in 0..4 {
        let bytes = so_far();
        let Some(answer) = answer_for(&visible(&bytes)) else {
            break;
        };
        eprintln!("[T16] {name}: answering {answer:?}");
        if let Err(err) = manager.write(&info.id, answer.as_bytes()) {
            eprintln!("[T16] {name}: could not answer: {err}");
            break;
        }
        tokio::time::sleep(Duration::from_secs(6)).await;
    }
    write_phase("idle", &so_far());

    // 3. One turn, sampled until the spinner shows.
    //
    //    Both CLIs take the "esc to interrupt" affordance down the moment a
    //    turn ends — and on this machine both turns end almost immediately
    //    (claude cannot read its keychain credentials from a spawned pty;
    //    codex's default model is unreachable, PINNED §9). So the capture polls
    //    rather than sleeping, and keeps the first frame that actually shows
    //    the marker.
    if let Err(err) = manager.write(&info.id, b"reply with the single word ok and stop") {
        eprintln!("[T16] {name}: could not type the prompt: {err}");
    }
    // The newline goes separately: `codex` treats one fast burst as a paste and
    // leaves it sitting in the composer unsent.
    tokio::time::sleep(Duration::from_millis(400)).await;
    if let Err(err) = manager.write(&info.id, b"\r") {
        eprintln!("[T16] {name}: could not submit: {err}");
    }
    let mut working: Option<Vec<u8>> = None;
    for _ in 0..120 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let bytes = so_far();
        if shows_working(name, &visible(&bytes)) {
            working = Some(bytes);
            break;
        }
    }
    match working {
        Some(bytes) => write_phase("working", &bytes),
        None => {
            eprintln!(
                "[T16] {name}: never showed a working marker; writing the last frame anyway \
                 so the failure is visible in the fixture rather than hidden"
            );
            write_phase("working", &so_far());
        }
    }

    let _ = manager.close(&info.id, CloseMode::Standard).await;
}

fn which_agent(name: &str) -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let candidates = [
        format!("{home}/.nvm/versions/node/v20.19.2/bin/{name}"),
        format!("{home}/.local/bin/{name}"),
        format!("/opt/homebrew/bin/{name}"),
        format!("/usr/local/bin/{name}"),
    ];
    candidates.into_iter().map(PathBuf::from).find(|p| p.is_file())
}

// ---------------------------------------------------------------------------
// PINNED §35: the full-screen views, through the app's own path
// ---------------------------------------------------------------------------

/// Drive `/model` and `/help` in a real `claude`, through `PtyManager` and the
/// real [`StatusEngine`], and record both the bytes and the verdicts.
///
/// This is the closest a test can get to the packaged app without opening a
/// window: the same env composition, the same termios, the same reader thread,
/// the same 500 ms tick with a real `ps` snapshot behind it — and, crucially,
/// the same **answers to the child's terminal queries** that xterm.js sends
/// back through `session_write`. A plain `script`-style capture answers none of
/// them, and claude Code behaves differently when nobody is listening
/// (PINNED §35).
///
/// It writes `tests/fixtures/streams/claude-model-help.cap` — the timed capture
/// the unit tests in `status::engine` replay.
///
/// Deadlined at 120 s. The `pump()` calls script about 38 s of real time, so
/// two minutes is "claude stopped drawing" — a `pump` whose inner
/// `manager.write` blocks, or a login screen that never becomes a composer —
/// not a slow machine. See [`deadline`].
///
/// ```text
/// cargo test --test status_streams -- --ignored --nocapture model_and_help
/// ```
#[tokio::test(flavor = "multi_thread")]
#[ignore = "drives the real claude binary"]
async fn recapture_model_and_help_and_print_the_verdicts() {
    use std::time::{Duration, Instant};

    use gilde_code_lib::supervise::procscan::ps_snapshot;

    let _deadline = deadline(
        "recapture_model_and_help_and_print_the_verdicts",
        Duration::from_secs(120),
    );

    let Some(binary) = which_agent("claude") else {
        eprintln!("[§35] claude is not installed; skipping");
        return;
    };
    // A worktree whose folder trust has already been granted, so the capture is
    // the composer and not the trust dialog.
    let cwd = PathBuf::from(std::env::var("GILDE_CAPTURE_CWD").unwrap_or_else(|_| {
        format!(
            "{}/.gilde/worktrees/m15-proj-737c3278/m15-check-685f8c",
            std::env::var("HOME").unwrap_or_default()
        )
    }));
    if !cwd.is_dir() {
        eprintln!("[§35] {} is missing; skipping", cwd.display());
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::open_in_memory().expect("db");
    let config =
        Arc::new(ConfigStore::open_at(&dir.path().join("config.json")).expect("config store"));
    let supervisor = Arc::new(
        Supervisor::open_instance(&db, config.clone(), "0.0.0-capture")
            .await
            .expect("open_instance"),
    );
    let manager = Arc::new(PtyManager::new(supervisor, config));
    manager
        .status_engine()
        .set_signatures(Arc::new(StaticSignatures::new(Signatures::compile(
            &SignatureFile::defaults(),
        ))));

    let started = Instant::now();
    // (kind, ms, payload) — the GCAP1 records, in arrival order.
    let log: Arc<Mutex<Vec<(u8, u32, Vec<u8>)>>> = Arc::new(Mutex::new(Vec::new()));
    let pending_replies: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = log.clone();
    let replies = pending_replies.clone();
    let channel = Channel::new(move |body| {
        let json = match body {
            InvokeResponseBody::Json(json) => json,
            InvokeResponseBody::Raw(_) => unreachable!("chunks are json"),
        };
        let chunk: PtyChunk = serde_json::from_str(&json).expect("chunk");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(chunk.b64.as_bytes())
            .expect("b64");
        // What xterm.js would answer. `CSI ? 6 n` (DECXCPR) is the one that
        // matters: claude Code polls it five times a second **for as long as
        // something answers**, and stops after one unanswered try.
        let mut queue = Vec::new();
        let mut i = 0;
        while i + 1 < bytes.len() {
            if bytes[i] == 0x1b && bytes[i + 1] == b'[' {
                let rest = &bytes[i + 2..];
                if rest.starts_with(b"?6n") {
                    queue.push(b"\x1b[?43;3R".to_vec());
                } else if rest.starts_with(b"6n") {
                    queue.push(b"\x1b[43;3R".to_vec());
                } else if rest.starts_with(b"c") {
                    queue.push(b"\x1b[?1;2c".to_vec());
                } else if rest.starts_with(b"5n") {
                    queue.push(b"\x1b[0n".to_vec());
                }
            }
            i += 1;
        }
        if !queue.is_empty() {
            replies.lock().expect("replies").extend(queue);
        }
        sink.lock().expect("sink").push((
            0,
            started.elapsed().as_millis() as u32,
            bytes,
        ));
        Ok(())
    });

    let mut spec = OpenSpec::new(
        AgentKind::Claude,
        &cwd,
        vec![binary.to_string_lossy().into_owned()],
    );
    spec = spec.size(160, 45);
    spec.env_base = EnvBase::from_process_env(std::env::var("PATH").unwrap_or_default());
    let info = manager.open(spec, channel).await.expect("open");
    let engine = manager.status_engine().clone();

    let mark = |text: &str| {
        log.lock().expect("log").push((
            1,
            started.elapsed().as_millis() as u32,
            text.as_bytes().to_vec(),
        ));
        eprintln!("[§35] {:6}ms MARK {text}", started.elapsed().as_millis());
    };

    // The app's 500 ms tick with a real `ps` snapshot behind it, plus the
    // frontend's job of answering the child's queries.
    let pump = |seconds: f32| {
        let manager = manager.clone();
        let engine = engine.clone();
        let id = info.id.clone();
        let replies = pending_replies.clone();
        let log = log.clone();
        async move {
            let until = Instant::now() + Duration::from_secs_f32(seconds);
            while Instant::now() < until {
                for reply in replies.lock().expect("replies").drain(..) {
                    let _ = manager.write(&id, &reply);
                }
                if let Ok(snap) = ps_snapshot() {
                    engine.tick(&snap);
                }
                let report = engine.report(&id);
                // The CPU half of the recording. `ps -o %cpu` is the other
                // input the engine decides on, and a replay that only has the
                // bytes cannot reproduce the reported bug (PINNED §35).
                if let Some(StatusEvidence::ForegroundGroup { cpu_pct, .. }) = report
                    .evidence
                    .iter()
                    .find(|e| matches!(e, StatusEvidence::ForegroundGroup { .. }))
                {
                    log.lock().expect("log").push((
                        1,
                        started.elapsed().as_millis() as u32,
                        format!("cpu {cpu_pct:.1}").into_bytes(),
                    ));
                }
                eprintln!(
                    "[§35] {:6}ms {:?} conf={:.2} {:?}",
                    started.elapsed().as_millis(),
                    report.status.kind(),
                    report.confidence,
                    report.evidence,
                );
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    };

    mark("boot");
    pump(12.0).await;
    mark("type /model");
    manager.write(&info.id, b"/model").expect("write");
    pump(1.5).await;
    manager.write(&info.id, b"\r").expect("write");
    mark("RECORD model 8s");
    pump(2.5).await;
    // The reported scenario is a person *navigating* the menu: every arrow key
    // is another full repaint, and every repaint is another CPU blip.
    mark("arrow down");
    manager.write(&info.id, b"\x1b[B").expect("write");
    pump(2.0).await;
    mark("arrow down");
    manager.write(&info.id, b"\x1b[B").expect("write");
    pump(2.0).await;
    mark("arrow up");
    manager.write(&info.id, b"\x1b[A").expect("write");
    pump(3.5).await;
    mark("END model");

    manager.write(&info.id, b"\x1b").expect("write");
    pump(2.0).await;
    mark("type /help");
    manager.write(&info.id, b"/help").expect("write");
    pump(1.5).await;
    manager.write(&info.id, b"\r").expect("write");
    mark("RECORD help 8s");
    pump(8.0).await;
    mark("END help");

    manager.write(&info.id, b"\x1b").expect("write");
    pump(1.5).await;
    mark("close");
    let _ = manager.close(&info.id, CloseMode::Standard).await;

    // GCAP1: `u8 kind · u32 ms · u32 len · payload`, little endian.
    let mut out = b"GCAP1\n".to_vec();
    for (kind, ms, payload) in log.lock().expect("log").iter() {
        out.push(*kind);
        out.extend_from_slice(&ms.to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(payload);
    }
    let path = streams_dir().join("claude-model-help.cap");
    std::fs::write(&path, &out).expect("write capture");
    eprintln!("[§35] wrote {} ({} bytes)", path.display(), out.len());
}
