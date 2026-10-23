//! The S5 status engine against **bytes a real agent CLI produced** (contract
//! §6.2.6; the kept `status_streams` suite).
//!
//! `tests/fixtures/streams/*.bin` are raw pty bytes captured on this machine
//! (`claude` 2.1.263, `codex` 0.145.0) at 120×32: no framing, exactly what the
//! reader thread pushed into the ring. `*.cap` are timed `GCAP1` recordings
//! (see the README there). They are replayed through the real grid and engine
//! on a **synthetic clock** — no sleeping, no wall-clock budget (A10.7) — and
//! the status each screen produces is asserted.
//!
//! No test here starts a vendor CLI (the old recapture harness was removed:
//! tests never spawn `claude`/`codex`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gilde_code_lib::agents::signatures::{SignatureFile, Signatures};
use gilde_code_lib::agents::AgentRegistry;
use gilde_code_lib::config::ConfigStore;
use gilde_code_lib::contract::activity::PtyStatus;
use gilde_code_lib::contract::interaction::{InteractionKind, OptionRole};
use gilde_code_lib::model::status::{SessionStatus, StatusEvidence};
use gilde_code_lib::paths::AppPaths;
use gilde_code_lib::pty::AnsiTail;
use gilde_code_lib::status::engine::{StaticSignatures, StatusEngine};
use gilde_code_lib::status::evidence::{screen_card, CONF_READY_SIGNATURE};
use gilde_code_lib::store::now_ms;

/// The capture size of the `.bin` fixtures.
const COLS: u16 = 120;
const ROWS: u16 = 32;
/// An arbitrary epoch for the synthetic clock.
const T0: i64 = 1_000_000;

fn streams_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/streams")
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

/// Evaluate every 100 ms of synthetic time from `from` to `to`, as the ticker
/// would.
fn tick_through(engine: &StatusEngine, id: &str, from: i64, to: i64) {
    let mut t = from;
    while t <= to {
        engine.tick_at(id, t);
        t += 100;
    }
}

/// Replay one stream into a fresh session and return the status it settles
/// on 2.5 s (synthetic) after the last byte.
fn settle(engine: &StatusEngine, id: &str, agent: &str, bytes: &[u8]) -> SessionStatus {
    engine.register_sized(id, agent, 200, COLS, ROWS, T0);
    let at = T0 + 2_000;
    engine.feed_at(id, bytes, at);
    tick_through(engine, id, at, at + 2_500);
    engine.last_report(id).status
}

fn joined(bytes: &[u8]) -> String {
    let mut tail = AnsiTail::with_size(COLS, ROWS);
    tail.feed(bytes);
    tail.visible_lines(12).join("\n")
}

// ---------------------------------------------------------------------------
// claude 2.1.263
// ---------------------------------------------------------------------------

/// A fresh folder opens on the folder-trust dialog: a setup prompt (§6.2.4),
/// recognised as the trust dialog (A6), WaitingForInput — never answered.
#[test]
fn a_fresh_folder_shows_the_trust_dialog_as_a_setup_prompt() {
    let bytes = stream("claude-waiting.bin");
    let engine = engine();
    let status = settle(&engine, "claude-waiting", "claude", &bytes);
    assert!(
        matches!(status, SessionStatus::WaitingForInput { .. }),
        "expected the trust dialog, got {status:?}\nscreen:\n{}",
        joined(&bytes)
    );
    let facts = engine.last_facts("claude-waiting").expect("facts");
    assert_eq!(facts.status, PtyStatus::WaitingForInput);
    let prompt = facts.prompt.expect("a setup prompt");
    assert!(prompt.setup, "an unnumbered menu is a setup prompt");
    assert!(prompt.trust_dialog, "the trust dialog is recognised: {prompt:?}");
    let labels: Vec<&str> = prompt.options.iter().map(|o| o.label.as_str()).collect();
    assert_eq!(labels, vec!["No, exit", "Yes, I trust this folder"]);
    assert!(prompt.options[0].selected, "it opens on `No, exit`");
    let card = screen_card(&prompt, Some(facts.confidence));
    assert_eq!(card.kind, InteractionKind::SetupPrompt);
    // A25.1: the one option is the trust action's marker (A6), never a key to send.
    let roles: Vec<_> = card.options.iter().map(|o| (o.key.as_str(), o.role)).collect();
    assert_eq!(roles, vec![("2", OptionRole::TrustFolder)], "the trust dialog offers only the trust action");
}

#[test]
fn claude_at_its_input_box_is_idle_by_its_ready_signature() {
    let bytes = stream("claude-idle.bin");
    let engine = engine();
    let status = settle(&engine, "claude-idle", "claude", &bytes);
    assert!(
        matches!(status, SessionStatus::Idle),
        "expected idle, got {status:?}\nscreen:\n{}",
        joined(&bytes)
    );
    let report = engine.last_report("claude-idle");
    assert_eq!(report.confidence, CONF_READY_SIGNATURE);
    assert!(report.evidence.contains(&StatusEvidence::ReadySignature));
}

#[test]
fn claude_running_a_turn_is_working() {
    let bytes = stream("claude-working.bin");
    let status = settle(&engine(), "claude-working", "claude", &bytes);
    assert!(
        matches!(status, SessionStatus::Working { .. }),
        "expected working, got {status:?}\nscreen:\n{}",
        joined(&bytes)
    );
}

// ---------------------------------------------------------------------------
// codex 0.145.0
// ---------------------------------------------------------------------------

/// codex's "Update available!" notice: `1. Update now / 2. Skip / Press enter
/// to continue` — a numbered, question-shaped screen prompt.
#[test]
fn codex_opens_on_a_numbered_dialog_that_is_a_screen_prompt() {
    let bytes = stream("codex-waiting.bin");
    let engine = engine();
    let status = settle(&engine, "codex-waiting", "codex", &bytes);
    assert!(
        matches!(status, SessionStatus::WaitingForInput { .. }),
        "expected a blocking dialog, got {status:?}\nscreen:\n{}",
        joined(&bytes)
    );
    let prompt = engine.prompt("codex-waiting").expect("prompt");
    assert!(!prompt.setup);
    assert!(!prompt.permission_shaped, "update/skip is not yes/no");
    assert!(prompt.options.len() >= 2);
    assert_eq!(prompt.options[0].key, "1");
}

#[test]
fn codex_at_its_composer_is_idle() {
    let bytes = stream("codex-idle.bin");
    let engine = engine();
    let status = settle(&engine, "codex-idle", "codex", &bytes);
    assert!(
        matches!(status, SessionStatus::Idle),
        "expected idle, got {status:?}\nscreen:\n{}",
        joined(&bytes)
    );
    assert!(engine.last_report("codex-idle").confidence >= 0.6);
}

#[test]
fn codex_running_a_turn_is_working() {
    let bytes = stream("codex-working.bin");
    let status = settle(&engine(), "codex-working", "codex", &bytes);
    assert!(
        matches!(status, SessionStatus::Working { .. }),
        "expected working, got {status:?}\nscreen:\n{}",
        joined(&bytes)
    );
}

// ---------------------------------------------------------------------------
// the sequence
// ---------------------------------------------------------------------------

/// One session, in the order a real one lives: dialog → idle → a turn → idle.
#[test]
fn one_session_walks_waiting_then_idle_then_working_then_idle() {
    for (agent, files) in [
        ("claude", ["claude-waiting.bin", "claude-idle.bin", "claude-working.bin", "claude-idle.bin"]),
        ("codex", ["codex-waiting.bin", "codex-idle.bin", "codex-working.bin", "codex-idle.bin"]),
    ] {
        let engine = engine();
        let id = format!("{agent}-sequence");
        engine.register_sized(&id, agent, 200, COLS, ROWS, T0);
        let mut seen: Vec<String> = Vec::new();
        let mut at = T0 + 2_000;
        for file in files {
            engine.feed_at(&id, &stream(file), at);
            tick_through(&engine, &id, at, at + 2_500);
            let kind = engine.last_report(&id).status.kind().to_string();
            if seen.last() != Some(&kind) {
                seen.push(kind);
            }
            at += 3_000;
        }
        assert_eq!(
            seen,
            vec!["waitingForInput", "idle", "working", "idle"],
            "{agent} did not walk the expected sequence"
        );
    }
}

/// Every fixture leaves the grid with something a human could have read.
#[test]
fn every_captured_stream_still_yields_readable_lines() {
    let mut checked = 0;
    for entry in std::fs::read_dir(streams_dir()).expect("streams dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("bin") {
            continue;
        }
        let bytes = std::fs::read(&path).expect("fixture");
        let mut tail = AnsiTail::with_size(COLS, ROWS);
        tail.feed(&bytes);
        let lines = tail.visible_lines(8);
        assert!(!lines.is_empty(), "{} parsed to nothing", path.display());
        assert!(lines.iter().any(|l| l.chars().count() > 3), "{}: {lines:?}", path.display());
        assert!(!lines.iter().any(|l| l.contains('\u{1b}')), "{}: {lines:?}", path.display());
        checked += 1;
    }
    assert!(checked >= 6, "expected the six captured screens, found {checked}");
}

// ---------------------------------------------------------------------------
// the timed recordings (GCAP1)
// ---------------------------------------------------------------------------

struct Record {
    kind: u8,
    ms: u32,
    payload: Vec<u8>,
}

fn records(name: &str) -> Vec<Record> {
    let bytes = stream(name);
    assert!(bytes.starts_with(b"GCAP1\n"), "{name} is not a GCAP1 file");
    let mut out = Vec::new();
    let mut i = 6;
    while i + 9 <= bytes.len() {
        let kind = bytes[i];
        let ms = u32::from_le_bytes(bytes[i + 1..i + 5].try_into().unwrap());
        let len = u32::from_le_bytes(bytes[i + 5..i + 9].try_into().unwrap()) as usize;
        let payload = bytes[i + 9..(i + 9 + len).min(bytes.len())].to_vec();
        out.push(Record { kind, ms, payload });
        i += 9 + len;
    }
    out
}

fn size_of(marker: &str, word: &str) -> Option<(u16, u16)> {
    let rest = marker.strip_prefix(word)?.trim();
    let dims = rest.split_whitespace().next()?;
    let (c, r) = dims.split_once('x')?;
    Some((c.parse().ok()?, r.parse().ok()?))
}

/// Replay a recording on the synthetic clock (ticking every 100 ms between
/// records) and collect, for every phase marker, the status at the **end** of
/// that phase — just before the next marker.
fn replay(name: &str) -> Vec<(String, String, Vec<String>)> {
    let engine = engine();
    let id = name.to_string();
    let mut phases: Vec<(String, String, Vec<String>)> = Vec::new();
    let mut current: Option<String> = None;
    let mut registered = false;
    let mut last = T0;
    for record in records(name) {
        let at = T0 + i64::from(record.ms);
        if registered && at > last {
            tick_through(&engine, &id, last + 100, at);
        }
        last = last.max(at);
        match record.kind {
            0 => {
                if !registered {
                    engine.register_sized(&id, "claude", 1_200, 160, 45, at);
                    registered = true;
                }
                engine.feed_at(&id, &record.payload, at);
            }
            1 => {
                let marker: String = String::from_utf8(record.payload.clone()).unwrap_or_default();
                if let Some((c, r)) = size_of(&marker, "open") {
                    engine.register_sized(&id, "claude", 1_200, c, r, at);
                    registered = true;
                } else if let Some((c, r)) = size_of(&marker, "resize") {
                    engine.resize_at(&id, c, r, at);
                } else if !marker.starts_with("cpu") && registered {
                    if let Some(prev) = current.take() {
                        let screen = engine.screen(&id).map(|s| s.lines).unwrap_or_default();
                        phases.push((prev, engine.last_report(&id).status.kind().to_string(), screen));
                    }
                    current = Some(marker);
                }
            }
            _ => {}
        }
    }
    phases
}

/// `/model` is a numbered menu with a selection marker and a footer: a screen
/// prompt, so WaitingForInput — including while the user arrows through it.
#[test]
fn the_model_menu_recording_is_waiting_for_input_while_it_is_up() {
    let phases = replay("claude-model-help.cap");
    let during: Vec<&(String, String, Vec<String>)> = phases
        .iter()
        .filter(|(m, _, _)| m.starts_with("RECORD model") || m.starts_with("arrow"))
        .collect();
    assert!(!during.is_empty(), "the recording has its markers");
    for (marker, kind, screen) in during {
        assert_eq!(kind, "waitingForInput", "at the end of `{marker}`:\n{}", screen.join("\n"));
    }
}

/// `/help` has no option run: it is never WaitingForInput from the alternate
/// screen alone (F-48), and it is not Working once it stopped drawing.
#[test]
fn the_help_view_recording_is_neither_waiting_nor_working_once_settled() {
    let phases = replay("claude-model-help.cap");
    let help = phases
        .iter()
        .find(|(m, _, _)| m.starts_with("RECORD help"))
        .expect("a RECORD help phase");
    assert_ne!(help.1, "waitingForInput", "{}", help.2.join("\n"));
    assert_ne!(help.1, "working", "{}", help.2.join("\n"));
}

// ---------------------------------------------------------------------------
// the signature file, on disk
// ---------------------------------------------------------------------------

/// Editing `agent-signatures.json` reclassifies a live session with no
/// restart, and a pattern that does not compile costs only itself. The wait
/// is event-polled with a generous ceiling (A10.7); the 2 s hot-reload budget
/// is a G5 benchmark.
#[test]
fn editing_the_signature_file_reclassifies_a_live_session() {
    let dir = tempfile::tempdir().expect("tempdir");
    let paths = AppPaths::for_home(dir.path());
    paths.ensure_all().expect("app dirs");
    let config = Arc::new(ConfigStore::open_at(&dir.path().join("config.json")).expect("config store"));
    let registry = Arc::new(AgentRegistry::without_db(paths.clone(), config));
    let _watch = registry.watch_signatures().expect("watch the signature file");

    let engine = Arc::new(StatusEngine::new());
    engine.set_signatures(registry.clone());
    // A shell on the main screen: its idle patterns decide.
    engine.register_sized("s1", "shell", 100, 80, 24, now_ms() - 10_000);
    engine.on_output("s1", b"AWAITING ORDERS, CAPTAIN\r\n");
    std::thread::sleep(Duration::from_millis(300));
    engine.tick_session("s1");

    let file = paths.signatures_file();
    let tmp = file.with_extension("json.editing");
    std::fs::write(
        &tmp,
        r#"{"version":4,"agents":{"shell":{"idle":["AWAITING ORDERS","(unclosed"]}}}"#,
    )
    .expect("write");
    std::fs::rename(&tmp, &file).expect("rename over the original");

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut reclassified = false;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        if registry.signatures().warnings().iter().any(|w| w.contains("unclosed")) {
            let report = engine.report("s1");
            if matches!(report.status, SessionStatus::Idle)
                && report
                    .evidence
                    .iter()
                    .any(|e| matches!(e, StatusEvidence::PatternMatch { signature_id, .. } if signature_id == "shell.idle.0"))
            {
                reclassified = true;
                break;
            }
        }
    }
    assert!(reclassified, "the edit never reached the engine: {:?}", engine.report("s1"));
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
    assert!(registry.signatures().ready_signature("claude").is_some(), "the defaults still merge in");
}
