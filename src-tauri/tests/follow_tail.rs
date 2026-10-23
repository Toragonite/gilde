//! T-follow-* (contract §6.4, §14.2): offset advance, partial lines
//! (T-follow-partial), invalid UTF-8 rows (T-follow-utf8), truncation and
//! inode change (T-follow-reset), load older, `RowTooLarge` on an 83 MB file
//! with a 4.5 MiB line (T-follow-large), usage once per `message.id`
//! (T-follow-usageDedupe), and the structure sink (S3 facts, tool-set
//! attachments, the interrupt marker). Owner: T04.

mod fixtures;
#[path = "fixtures/transcripts/support.rs"]
mod support;

use std::io::Write;
use std::sync::Arc;

use gilde_code_lib::contract::activity::ErrorSub;
use gilde_code_lib::contract::content::ResetReason;
use gilde_code_lib::contract::services::{IdentityHint, SourceFacts};
use gilde_code_lib::follow::tail::{
    apply_line, open_followed, read_back, refuse_path, Follower, Line, LineListener, OlderCursor, StructureState,
    TranscriptFormat, TranscriptTail, INITIAL_BUDGET, LINE_CAP, OLDER_BUDGET,
};
use gilde_code_lib::render::transcript::{parse_rfc3339_ms, TokenUsage};
use support::*;

fn ms(s: &str) -> i64 {
    parse_rfc3339_ms(s).unwrap()
}

fn structure_of(rel: &str, format: TranscriptFormat) -> StructureState {
    let file = std::fs::File::open(fixture(rel)).unwrap();
    let size = file.metadata().unwrap().len();
    let back = read_back(&file, OlderCursor::at(size), u64::MAX, LINE_CAP, &mut |_: &Line| false).unwrap();
    let mut state = StructureState::default();
    for line in &back.lines {
        apply_line(&mut state, format, line);
    }
    state
}

fn complete(lines: &[Line]) -> Vec<String> {
    lines
        .iter()
        .map(|l| match l {
            Line::Complete { bytes, .. } => String::from_utf8_lossy(bytes).into_owned(),
            Line::TooLarge { start } => format!("<too large @{start}>"),
        })
        .collect()
}

#[test]
fn t_follow_offset_advance() {
    let env = fixtures::Hermetic::new();
    let path = env.tmp_dir().join("s.jsonl");
    std::fs::write(&path, "a\nbb\nccc\n").unwrap();
    let mut tail = TranscriptTail::open(&path).unwrap();
    assert_eq!(tail.offset(), 9, "positioned after the last complete line");
    assert!(tail.advance().unwrap().lines.is_empty());
    append(&path, b"dd\neee\n");
    let adv = tail.advance().unwrap();
    assert_eq!(adv.reset, None);
    assert_eq!(adv.lines, vec![Line::Complete { start: 9, bytes: b"dd".to_vec() }, Line::Complete { start: 12, bytes: b"eee".to_vec() }]);
    assert_eq!(tail.offset(), 16);
    assert!(tail.advance().unwrap().lines.is_empty(), "no new bytes, no lines");
}

#[test]
fn t_follow_partial() {
    let env = fixtures::Hermetic::new();
    let path = env.tmp_dir().join("p.jsonl");
    // The file ends in a partial line: it is not part of the initial offset.
    std::fs::write(&path, "one\ntw").unwrap();
    let mut tail = TranscriptTail::open(&path).unwrap();
    assert_eq!(tail.offset(), 4);
    let adv = tail.advance().unwrap();
    assert!(adv.lines.is_empty(), "a partial trailing line is never parsed half");
    append(&path, b"o");
    assert!(tail.advance().unwrap().lines.is_empty());
    append(&path, b"\nthr");
    let adv = tail.advance().unwrap();
    assert_eq!(complete(&adv.lines), vec!["two"]);
    assert_eq!(adv.lines[0].start(), 4);
    append(&path, b"ee\n");
    assert_eq!(complete(&tail.advance().unwrap().lines), vec!["three"]);
}

#[test]
fn t_follow_utf8() {
    let env = fixtures::Hermetic::new();
    let path = env.tmp_dir().join("u.jsonl");
    let mut bytes = Vec::new();
    bytes.extend_from_slice(user_row("u1", "first", "2026-10-22T01:00:00.000Z").as_bytes());
    bytes.extend_from_slice(b"{\"type\":\"user\",\"uuid\":\"bad\",\"message\":{\"role\":\"user\",\"content\":\"\xff\xfe\"}}\n");
    bytes.extend_from_slice(assistant_row("r1", "msg_1", "second", "2026-10-22T01:00:01.000Z").as_bytes());
    std::fs::write(&path, &bytes).unwrap();
    let file = open_followed(&path).unwrap();
    let back = read_back(&file, OlderCursor::at(bytes.len() as u64), INITIAL_BUDGET, LINE_CAP, &mut |_: &Line| false).unwrap();
    assert_eq!(back.lines.len(), 3);
    let mut state = StructureState::default();
    for l in &back.lines {
        apply_line(&mut state, TranscriptFormat::Claude, l);
    }
    assert_eq!(state.counters.invalid_utf8, 1, "the invalid row is skipped and counted");
    assert_eq!(state.counters.rows, 2);
    assert_eq!(state.facts.last_row_at, Some(ms("2026-10-22T01:00:01.000Z")));

    // A lone surrogate escape is repaired, not dropped (basic.jsonl's last row).
    let basic = structure_of("claude/basic.jsonl", TranscriptFormat::Claude);
    assert_eq!(basic.counters.unparseable, 0);
    assert_eq!(basic.counters.invalid_utf8, 0);
}

#[test]
fn t_follow_reset() {
    let env = fixtures::Hermetic::new();
    let path = env.tmp_dir().join("r.jsonl");
    std::fs::write(&path, "a\nb\nc\n").unwrap();
    let mut tail = TranscriptTail::open(&path).unwrap();
    // Truncation: size < offset.
    std::fs::OpenOptions::new().write(true).open(&path).unwrap().set_len(2).unwrap();
    let adv = tail.advance().unwrap();
    assert_eq!(adv.reset, Some(ResetReason::Truncated));
    assert_eq!(tail.offset(), 2, "restarted after the last complete line");
    append(&path, b"z\n");
    assert_eq!(complete(&tail.advance().unwrap().lines), vec!["z"]);
    // Replacement: a new inode at the path.
    let other = env.tmp_dir().join("r2.jsonl");
    std::fs::write(&other, "x\ny\n").unwrap();
    std::fs::rename(&other, &path).unwrap();
    let adv = tail.advance().unwrap();
    assert_eq!(adv.reset, Some(ResetReason::Replaced));
    assert_eq!(tail.offset(), 4);
    append(&path, b"w\n");
    assert_eq!(complete(&tail.advance().unwrap().lines), vec!["w"]);
    // A vanished file reads nothing and does not fail.
    std::fs::remove_file(&path).unwrap();
    assert_eq!(tail.advance().unwrap(), Default::default());
}

#[test]
fn t_follow_large() {
    let env = fixtures::Hermetic::new();
    let path = env.tmp_dir().join("big.jsonl");
    let row = assistant_row("r", "msg", &"x".repeat(900), "2026-10-22T01:00:00.000Z");
    let big_line = format!("{{\"type\":\"user\",\"pad\":\"{}\"}}\n", "y".repeat(4 * 1024 * 1024 + 512 * 1024));
    let mut f = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
    let rows_each = (39 * 1024 * 1024) / row.len();
    for _ in 0..rows_each {
        f.write_all(row.as_bytes()).unwrap();
    }
    let big_at = rows_each as u64 * row.len() as u64;
    f.write_all(big_line.as_bytes()).unwrap();
    for _ in 0..rows_each {
        f.write_all(row.as_bytes()).unwrap();
    }
    f.flush().unwrap();
    drop(f);
    let size = std::fs::metadata(&path).unwrap().len();
    assert!(size > 82 * 1024 * 1024, "an 83 MB fixture ({size} bytes)");

    // The initial load reads at most 4 MiB from the end, whatever the file size.
    let tail = TranscriptTail::open(&path).unwrap();
    let file = open_followed(&path).unwrap();
    let back = read_back(&file, OlderCursor::at(tail.offset()), INITIAL_BUDGET, LINE_CAP, &mut |_: &Line| false).unwrap();
    assert!(back.scanned <= INITIAL_BUDGET);
    assert!(back.has_older);
    assert!(!back.lines.is_empty());

    // Backwards through the 4.5 MiB line: TooLarge, never buffered whole,
    // and reading continues before it.
    let cursor = OlderCursor::at(big_at + big_line.len() as u64 + row.len() as u64);
    let mut seen = Vec::new();
    let mut c = cursor;
    for _ in 0..4 {
        let back = read_back(&file, c, OLDER_BUDGET, LINE_CAP, &mut |l: &Line| matches!(l, Line::TooLarge { .. })).unwrap();
        assert!(back.scanned <= OLDER_BUDGET);
        seen.extend(back.lines);
        c = back.cursor;
        if seen.iter().any(|l| matches!(l, Line::TooLarge { .. })) {
            break;
        }
    }
    assert!(seen.contains(&Line::TooLarge { start: big_at }), "the oversize line is reported at its start");
    let back = read_back(&file, c, OLDER_BUDGET, LINE_CAP, &mut |_: &Line| true).unwrap();
    assert!(matches!(&back.lines[0], Line::Complete { bytes, .. } if bytes.len() + 1 == row.len()), "reading continues before it");

    // Forward: an appended oversize line is skipped to its newline.
    let mut tail = TranscriptTail::open(&path).unwrap();
    append(&path, big_line.as_bytes());
    append(&path, row.as_bytes());
    let start = size;
    let adv = tail.advance().unwrap();
    assert_eq!(adv.lines.len(), 2);
    assert_eq!(adv.lines[0], Line::TooLarge { start });
    assert!(matches!(&adv.lines[1], Line::Complete { .. }));
    // A partial oversize line in flight is never buffered beyond the cap.
    append(&path, "z".repeat(LINE_CAP + 10).as_bytes());
    assert!(tail.advance().unwrap().lines.is_empty());
    append(&path, b"\n");
    assert!(matches!(tail.advance().unwrap().lines.as_slice(), [Line::TooLarge { .. }]));
}

#[test]
fn t_follow_load_older_pages_cover_the_file() {
    let path = fixture("claude/basic.jsonl");
    let text = std::fs::read_to_string(&path).unwrap();
    let want: Vec<&str> = text.lines().collect();
    let file = open_followed(&path).unwrap();
    let mut cursor = OlderCursor::at(text.len() as u64);
    let mut pages: Vec<Vec<Line>> = Vec::new();
    loop {
        let mut n = 0;
        let back = read_back(&file, cursor, OLDER_BUDGET, LINE_CAP, &mut |_: &Line| {
            n += 1;
            n >= 4
        })
        .unwrap();
        pages.push(back.lines);
        cursor = back.cursor;
        if !back.has_older {
            break;
        }
    }
    let got: Vec<String> = pages.into_iter().rev().flat_map(|p| complete(&p)).collect();
    assert_eq!(got, want);
}

#[test]
fn t_follow_usage_dedupe() {
    // Eleven rows share one message.id and repeat its usage (R6 §6 LP-26).
    let s = structure_of("claude/eleven-rows.jsonl", TranscriptFormat::Claude);
    assert_eq!(s.usage, TokenUsage { input: 6, output: 100, cache_creation: 80, cache_read: 18_000 });
    let s = structure_of("claude/basic.jsonl", TranscriptFormat::Claude);
    assert_eq!(s.usage.output, 20 + 7 + 9 + 30 + 11, "msg_A counted once over its three rows");
    assert_eq!(s.usage.input, 10 + 5 + 4 + 3 + 2);
}

#[test]
fn t_follow_structure_facts() {
    let s = structure_of("claude/basic.jsonl", TranscriptFormat::Claude);
    let f = &s.facts;
    assert_eq!(f.last_row_at, Some(ms("2026-10-22T03:08:10.000Z")));
    assert_eq!(f.last_human_prompt_at, Some(ms("2026-10-22T03:07:10.000Z")), "the peer message opened the last turn");
    assert_eq!(f.last_turn_end_at, Some(ms("2026-10-22T03:08:00.000Z")));
    assert_eq!(f.open_tool_use, None);
    assert_eq!(f.last_api_error, Some((ms("2026-10-22T03:07:20.000Z"), ErrorSub::UsageLimit { account_id: None, resets_at: Some(1_792_700_000_000) })));
    assert_eq!(s.last_interrupt_at, Some(ms("2026-10-22T03:05:30.000Z")), "the interrupt marker row is a turn end");
    assert!(s.tool_set.contains("TaskCreate") && s.tool_set.contains("SendMessage"));
    assert!(s.task_tools_seen);
    assert_eq!(s.counters.unknown_types.get("future-row-kind"), Some(&1));
    assert_eq!(s.counters.meta, 5);

    // An open question tool call (no result yet) is S3's open tool use.
    let env = fixtures::Hermetic::new();
    let path = env.tmp_dir().join("q.jsonl");
    let ask = serde_json::json!({"type": "assistant", "uuid": "r1", "timestamp": "2026-10-22T02:00:00.000Z",
        "message": {"id": "m1", "role": "assistant", "stop_reason": "tool_use",
                    "content": [{"type": "tool_use", "id": "toolu_q", "name": "AskUserQuestion", "input": {"questions": []}}]}});
    std::fs::write(&path, format!("{}{}\n", user_row("u1", "go", "2026-10-22T01:59:00.000Z"), ask)).unwrap();
    let file = open_followed(&path).unwrap();
    let size = std::fs::metadata(&path).unwrap().len();
    let back = read_back(&file, OlderCursor::at(size), INITIAL_BUDGET, LINE_CAP, &mut |_: &Line| false).unwrap();
    let mut st = StructureState::default();
    for l in &back.lines {
        apply_line(&mut st, TranscriptFormat::Claude, l);
    }
    let open = st.facts.open_tool_use.expect("open");
    assert_eq!(open.tool_name, "AskUserQuestion");
    assert!(open.is_question);
}

#[test]
fn t_follow_codex_structure() {
    let s = structure_of("codex/rollout-basic.jsonl", TranscriptFormat::CodexRollout);
    assert_eq!(s.facts.last_human_prompt_at, Some(ms("2026-10-22T04:00:03.000Z")));
    assert_eq!(s.facts.last_turn_end_at, Some(ms("2026-10-22T04:01:02.000Z")));
    assert_eq!(s.last_interrupt_at, Some(ms("2026-10-22T04:01:02.000Z")));
    assert_eq!(s.counters.unknown_types.get("brand_new_type"), Some(&1));
}

#[test]
fn t_follow_never_open_filter() {
    use std::path::Path;
    for p in ["/x/history.jsonl", "/x/a.key", "/x/token.jsonl", "/x/.credentials.json", "/x/store.db", "/x/oauth_creds.jsonl", "/x/s.json"] {
        assert!(refuse_path(Path::new(p)), "{p}");
    }
    assert!(!refuse_path(Path::new("/x/0d4c1f6e-6b8a-4c7e-9a51-3f2b8c9d0e11.jsonl")));
    assert!(!refuse_path(Path::new("/x/rollout-2026-10-22T04-00-00-5c7c0b9e.jsonl")));
    // Symlinks are not followed.
    let env = fixtures::Hermetic::new();
    let target = env.tmp_dir().join("t.jsonl");
    std::fs::write(&target, "a\n").unwrap();
    let link = env.tmp_dir().join("l.jsonl");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(open_followed(&link).is_err());
}

#[test]
fn t_follow_follower_structure_and_content() {
    let env = fixtures::Hermetic::new();
    let path = copy_fixture("claude/eleven-rows.jsonl", &env.tmp_dir());
    let follower = Follower::manual(manual_config());
    let evidence = Arc::new(Evidence::default());
    let rows = Arc::new(Rows::default());
    follower.set_evidence_sink(evidence.clone());
    follower.set_row_sink(rows.clone());
    let hint = IdentityHint { vendor_session_id: Some("0d4c1f6e-6b8a-4c7e-9a51-3f2b8c9d0e11".into()), ..IdentityHint::default() };
    follower.follow_structure(&path, TranscriptFormat::Claude, hint.clone());
    assert_eq!(evidence.got.lock().unwrap().len(), 1, "initial facts sent once");
    assert!(matches!(&evidence.got.lock().unwrap()[0].facts, SourceFacts::Transcript(f) if f.last_turn_end_at == Some(ms("2026-10-22T03:12:00.000Z"))));

    struct L(std::sync::Mutex<Vec<Line>>);
    impl LineListener for L {
        fn lines(&self, _p: &std::path::Path, lines: &[Line]) {
            self.0.lock().unwrap().extend_from_slice(lines);
        }
        fn reset(&self, _p: &std::path::Path, _r: ResetReason, _o: u64) {}
    }
    let listener = Arc::new(L(Default::default()));
    let attach = follower.attach_content(&path, TranscriptFormat::Claude, listener.clone()).unwrap();
    assert_eq!(attach.offset, std::fs::metadata(&path).unwrap().len());

    // Nothing new: no evidence repeat, no lines.
    follower.run_once();
    assert_eq!(evidence.got.lock().unwrap().len(), 1);
    assert!(listener.0.lock().unwrap().is_empty());

    append(&path, user_row("u-new", "next please", "2026-10-22T03:20:00.000Z").as_bytes());
    follower.poke(&path);
    follower.run_once();
    assert_eq!(listener.0.lock().unwrap().len(), 1);
    assert_eq!(listener.0.lock().unwrap()[0].start(), attach.offset);
    let got = evidence.got.lock().unwrap();
    assert_eq!(got.len(), 2);
    assert!(matches!(&got[1].facts, SourceFacts::Transcript(f) if f.last_human_prompt_at == Some(ms("2026-10-22T03:20:00.000Z"))));
    assert_eq!(got[1].hint, hint);
    drop(got);
    assert_eq!(rows.got.lock().unwrap().len(), 1);
    assert_eq!(rows.got.lock().unwrap()[0].uuid.as_deref(), Some("u-new"));

    follower.detach_content(&path, attach.id);
    follower.unfollow_structure(&path);
    assert!(follower.followed().is_empty(), "the tail is dropped with its last user");
}

#[test]
fn t_follow_missing_file_appears_later() {
    let env = fixtures::Hermetic::new();
    let path = env.tmp_dir().join("later.jsonl");
    let follower = Follower::manual(manual_config());
    let evidence = Arc::new(Evidence::default());
    follower.set_evidence_sink(evidence.clone());
    follower.follow_structure(&path, TranscriptFormat::Claude, IdentityHint::default());
    assert_eq!(follower.followed().len(), 1);
    std::fs::write(&path, user_row("u1", "hello", "2026-10-22T01:00:00.000Z")).unwrap();
    follower.run_once();
    let s = follower.structure(&path).unwrap();
    assert_eq!(s.facts.last_human_prompt_at, Some(ms("2026-10-22T01:00:00.000Z")));
}

#[test]
fn t_follow_usage_flag_p32() {
    use gilde_code_lib::follow::tail::TRANSCRIPT_USAGE_FLAG;
    assert_eq!(TRANSCRIPT_USAGE_FLAG, "transcriptUsage");
    let s = structure_of("claude/eleven-rows.jsonl", TranscriptFormat::Claude).snapshot();
    assert_eq!(s.transcript_usage(true).map(|u| u.output), Some(100));
    assert_eq!(s.transcript_usage(false), None, "flag off: unknown, never 0");
    let empty = StructureState::default().snapshot();
    assert_eq!(empty.transcript_usage(true), None, "no usage rows: unknown");
}

// ---------------------------------------------------------------------------
// spend (A10.4): running totals per local day, once per message.id, ≤ 1/s,
// nothing when the P32 flag is off or no row carried usage
// ---------------------------------------------------------------------------

mod spend {
    use super::*;
    use gilde_code_lib::contract::services::{UsageFacts, UsageSource};
    use gilde_code_lib::follow::tail::{utc_day, FollowConfig};
    use std::cell::Cell;

    thread_local! {
        static NOW: Cell<i64> = const { Cell::new(0) };
    }

    fn clock() -> i64 {
        NOW.with(Cell::get)
    }

    fn set_now(s: &str) {
        NOW.with(|n| n.set(ms(s)));
    }

    fn advance(by_ms: i64) {
        NOW.with(|n| n.set(n.get() + by_ms));
    }

    fn config() -> FollowConfig {
        FollowConfig { clock, local_day: utc_day, ..manual_config() }
    }

    /// A follower over a copy of `rel`, with the P32 gate on or off.
    fn follow(env: &fixtures::Hermetic, rel: &str, format: TranscriptFormat, gate: Option<bool>) -> (Follower, Arc<Evidence>, std::path::PathBuf) {
        let path = copy_fixture(rel, &env.tmp_dir());
        let follower = Follower::manual(config());
        let evidence = Arc::new(Evidence::default());
        follower.set_evidence_sink(evidence.clone());
        if let Some(on) = gate {
            follower.set_usage_gate(Arc::new(move |_: &std::path::Path, _: &IdentityHint| on));
        }
        follower.follow_structure(&path, format, IdentityHint::default());
        follower.run_once(); // runs the day scan
        (follower, evidence, path)
    }

    fn reports(ev: &Evidence) -> Vec<UsageFacts> {
        ev.got
            .lock()
            .unwrap()
            .iter()
            .filter_map(|u| match &u.facts {
                SourceFacts::Usage(f) => Some(f.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn t_follow_spend_dedupe_running_total() {
        set_now("2026-10-22T05:00:00.000Z");
        let env = fixtures::Hermetic::new();
        let (_f, ev, _p) = follow(&env, "claude/eleven-rows.jsonl", TranscriptFormat::Claude, Some(true));
        let r = reports(&ev);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].source, UsageSource::Transcript);
        let t = &r[0].today;
        assert_eq!(t.day, "2026-10-22");
        // Eleven rows of msg_ELEVEN carry the same usage: counted once (+ msg_DONE).
        assert_eq!(t.output_tokens, Some(100));
        assert_eq!(t.input_tokens, Some((3 + 40) * 2), "uncached + cache-creation input");
        assert_eq!(t.cached_tokens, Some(18_000));
        assert_eq!(t.cost_micros, None, "transcripts carry no cost");
    }

    #[test]
    fn t_follow_spend_throttle_and_total_not_delta() {
        set_now("2026-10-22T05:00:00.000Z");
        let env = fixtures::Hermetic::new();
        let (f, ev, path) = follow(&env, "claude/eleven-rows.jsonl", TranscriptFormat::Claude, Some(true));
        assert_eq!(reports(&ev).len(), 1);
        append(&path, assistant_row("r-n1", "msg_N1", "more", "2026-10-22T05:00:00.100Z").as_bytes());
        f.poke(&path);
        f.run_once();
        assert_eq!(reports(&ev).len(), 1, "within a second of the last report");
        advance(999);
        f.run_once();
        assert_eq!(reports(&ev).len(), 1);
        advance(1);
        f.run_once();
        let r = reports(&ev);
        assert_eq!(r.len(), 2, "the pending total goes out once the second has passed");
        assert_eq!(r[1].today.output_tokens, Some(102), "a running total, never a delta");
        // A repeated row of a counted message changes nothing: no report.
        append(&path, assistant_row("r-n1b", "msg_N1", "more", "2026-10-22T05:00:01.200Z").as_bytes());
        advance(5_000);
        f.poke(&path);
        f.run_once();
        assert_eq!(reports(&ev).len(), 2);
    }

    #[test]
    fn t_follow_spend_day_rollover() {
        set_now("2026-10-22T23:59:00.000Z");
        let env = fixtures::Hermetic::new();
        let (f, ev, path) = follow(&env, "claude/eleven-rows.jsonl", TranscriptFormat::Claude, Some(true));
        assert_eq!(reports(&ev)[0].today.day, "2026-10-22");
        set_now("2026-10-23T00:00:30.000Z");
        append(&path, assistant_row("r-late", "msg_LATE", "x", "2026-10-22T23:59:58.000Z").as_bytes());
        append(&path, assistant_row("r-new", "msg_NEW", "y", "2026-10-23T00:00:10.000Z").as_bytes());
        f.poke(&path);
        f.run_once();
        let r = reports(&ev);
        assert_eq!(r.len(), 2);
        assert_eq!(r[1].today.day, "2026-10-23");
        assert_eq!(r[1].today.output_tokens, Some(2), "a new total for the new day; yesterday's late row is not in it");
        assert_eq!(f.day_usage(&path).unwrap().day, "2026-10-23");
    }

    #[test]
    fn t_follow_spend_flag_off_or_unknown_sends_nothing() {
        set_now("2026-10-22T05:00:00.000Z");
        let env = fixtures::Hermetic::new();
        // P32 flag off.
        let (f, ev, path) = follow(&env, "claude/eleven-rows.jsonl", TranscriptFormat::Claude, Some(false));
        append(&path, assistant_row("r-x", "msg_X", "z", "2026-10-22T05:00:00.500Z").as_bytes());
        advance(2_000);
        f.poke(&path);
        f.run_once();
        assert!(reports(&ev).is_empty(), "flag off: nothing, never 0");
        // No gate wired = flag off.
        let env2 = fixtures::Hermetic::new();
        let (_f2, ev2, _) = follow(&env2, "claude/eleven-rows.jsonl", TranscriptFormat::Claude, None);
        assert!(reports(&ev2).is_empty());
        // No usage row today: nothing.
        set_now("2026-10-25T05:00:00.000Z");
        let env3 = fixtures::Hermetic::new();
        let (f3, ev3, p3) = follow(&env3, "claude/eleven-rows.jsonl", TranscriptFormat::Claude, Some(true));
        assert!(reports(&ev3).is_empty(), "rows of other days are not today's spend");
        append(&p3, user_row("u-z", "just a prompt", "2026-10-25T05:00:01.000Z").as_bytes());
        advance(2_000);
        f3.poke(&p3);
        f3.run_once();
        assert!(reports(&ev3).is_empty(), "a user row carries no usage");
        // Codex rollouts are not a transcript spend source.
        set_now("2026-10-22T05:00:00.000Z");
        let env4 = fixtures::Hermetic::new();
        let (_f4, ev4, _) = follow(&env4, "codex/rollout-basic.jsonl", TranscriptFormat::CodexRollout, Some(true));
        assert!(reports(&ev4).is_empty());
    }

    #[test]
    fn t_follow_spend_day_strings() {
        assert_eq!(utc_day(ms("2026-10-22T23:59:59.999Z")), "2026-10-22");
        assert_eq!(utc_day(ms("2026-10-23T00:00:00.000Z")), "2026-10-23");
        assert_eq!(utc_day(ms("2028-02-29T12:00:00.000Z")), "2028-02-29");
        // The local day is a well-formed date near the UTC one.
        let local = gilde_code_lib::follow::tail::local_day(ms("2026-10-22T12:00:00.000Z"));
        assert!(["2026-10-21", "2026-10-22", "2026-10-23"].contains(&local.as_str()), "{local}");
    }
}

// ---------------------------------------------------------------------------
// A50.1 — a followed file stays inside its vendor root (LV-1)
// ---------------------------------------------------------------------------

mod a50 {
    use std::path::{Path, PathBuf};

    use gilde_code_lib::follow::tail::{expected_real_path, followed_root, open_followed, TranscriptTail};

    const CANARY: &str = "A50-CANARY-never-followed";
    const SID: &str = "0d4c1f6e-6b8a-4c7e-9a51-3f2b8c9d0e11";

    fn row(text: &str) -> String {
        format!("{{\"type\":\"user\",\"uuid\":\"u-1\",\"timestamp\":\"2026-10-23T10:00:00.000Z\",\"message\":{{\"role\":\"user\",\"content\":\"{text}\"}}}}\n")
    }

    /// `<home>/.claude/projects/<slug>/` with one transcript, and the paths
    /// around it.
    struct Tree {
        _env: crate::fixtures::Hermetic,
        cfg: PathBuf,
        projects: PathBuf,
        outside: PathBuf,
    }

    fn tree() -> Tree {
        let env = crate::fixtures::Hermetic::new();
        let home = std::fs::canonicalize(env.home()).unwrap();
        let cfg = home.join(".claude");
        let projects = cfg.join("projects");
        std::fs::create_dir_all(projects.join("-w-proj")).unwrap();
        std::fs::write(projects.join("-w-proj").join(format!("{SID}.jsonl")), row("inside")).unwrap();
        let outside = home.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join(format!("{SID}.jsonl")), row(CANARY)).unwrap();
        Tree { _env: env, cfg, projects, outside }
    }

    fn refused(path: &Path) -> bool {
        open_followed(path).is_err() && TranscriptTail::open(path).is_err()
    }

    #[test]
    fn the_vendor_root_comes_from_the_layout() {
        let t = Path::new("/u/.claude/projects/-w-proj/a.jsonl");
        assert_eq!(followed_root(t), Some(Path::new("/u/.claude/projects")));
        let named_projects = Path::new("/u/.claude/projects/projects/a.jsonl");
        assert_eq!(followed_root(named_projects), Some(Path::new("/u/.claude/projects")), "a slug named projects is not the root");
        let r = Path::new("/u/.codex/sessions/2026/10/21/rollout-x.jsonl");
        assert_eq!(followed_root(r), Some(Path::new("/u/.codex/sessions")));
        assert_eq!(followed_root(Path::new("/tmp/fixture/a.jsonl")), Some(Path::new("/tmp/fixture")), "no layout: its own dir");
        assert_eq!(followed_root(Path::new("a.jsonl")), None);
        assert!(expected_real_path(Path::new("/u/.claude/projects/-w-proj/../../x/a.jsonl")).is_err(), "no `..` below the root");
    }

    #[test]
    fn an_ordinary_transcript_opens_also_through_a_symlinked_root() {
        let t = tree();
        let path = t.projects.join("-w-proj").join(format!("{SID}.jsonl"));
        assert!(open_followed(&path).is_ok());
        // The config dir itself kept elsewhere (a symlinked root) is fine.
        let alias = t.cfg.parent().unwrap().join("claude-alias");
        std::os::unix::fs::symlink(&t.cfg, &alias).unwrap();
        assert!(open_followed(&alias.join("projects/-w-proj").join(format!("{SID}.jsonl"))).is_ok());
    }

    #[test]
    fn a_hard_link_is_refused_whatever_it_names() {
        let t = tree();
        let creds = t.cfg.join(".credentials.json");
        std::fs::write(&creds, row(CANARY)).unwrap();
        let to_creds = t.projects.join("-w-proj/11111111-1111-4111-8111-111111111111.jsonl");
        std::fs::hard_link(&creds, &to_creds).unwrap();
        assert!(refused(&to_creds), "a hard link to a never-open file");
        // A second name of an ordinary transcript is refused too (the filter
        // never saw the other name).
        let twin = t.projects.join("-w-proj/22222222-2222-4222-8222-222222222222.jsonl");
        std::fs::hard_link(t.projects.join("-w-proj").join(format!("{SID}.jsonl")), &twin).unwrap();
        assert!(refused(&twin), "a transcript with two names");
    }

    #[test]
    fn a_symlinked_directory_below_the_root_is_refused() {
        let t = tree();
        // The project dir → a dir outside the vendor root.
        std::os::unix::fs::symlink(&t.outside, t.projects.join("-w-link")).unwrap();
        assert!(refused(&t.projects.join("-w-link").join(format!("{SID}.jsonl"))), "out of the root");
        // … and → another project dir inside the root: still a symlinked
        // directory component.
        std::os::unix::fs::symlink(t.projects.join("-w-proj"), t.projects.join("-w-alias")).unwrap();
        assert!(refused(&t.projects.join("-w-alias").join(format!("{SID}.jsonl"))), "inside the root, through a link");
        // A codex rollout whose day dir leads out of `sessions`.
        let sessions = t.cfg.parent().unwrap().join(".codex/sessions/2026/10");
        std::fs::create_dir_all(&sessions).unwrap();
        let day_out = t.outside.join("day");
        std::fs::create_dir_all(&day_out).unwrap();
        std::fs::write(day_out.join("rollout-2026-10-21T00-00-00-x.jsonl"), row(CANARY)).unwrap();
        std::os::unix::fs::symlink(&day_out, sessions.join("21")).unwrap();
        assert!(refused(&sessions.join("21/rollout-2026-10-21T00-00-00-x.jsonl")), "a rollout's day dir out of sessions/");
        // The file itself as a symlink (O_NOFOLLOW, unchanged).
        let link = t.projects.join("-w-proj/33333333-3333-4333-8333-333333333333.jsonl");
        std::os::unix::fs::symlink(t.outside.join(format!("{SID}.jsonl")), &link).unwrap();
        assert!(refused(&link));
    }

    #[test]
    fn a_fifo_named_like_a_transcript_never_blocks_the_follower() {
        use std::os::unix::ffi::OsStrExt as _;
        let t = tree();
        let fifo = t.projects.join("-w-proj/44444444-4444-4444-8444-444444444444.jsonl");
        let c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path for the call.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        // No writer: a blocking open would hang here.
        assert!(refused(&fifo));
    }

    #[test]
    fn a_followed_file_replaced_by_a_hard_link_reads_nothing_of_it() {
        let t = tree();
        let path = t.projects.join("-w-proj").join(format!("{SID}.jsonl"));
        let mut tail = TranscriptTail::open(&path).unwrap();
        let creds = t.cfg.join(".credentials.json");
        std::fs::write(&creds, row(CANARY).repeat(3)).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::hard_link(&creds, &path).unwrap();
        for _ in 0..3 {
            if let Ok(adv) = tail.advance() {
                let lines = format!("{:?}", adv.lines);
                assert!(!lines.contains(CANARY), "the replacement's bytes were read: {lines}");
            }
        }
    }
}
