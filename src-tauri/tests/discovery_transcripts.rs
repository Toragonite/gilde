//! Transcript head/tail (contract §5.2.5, §5.8): T-disc-transcriptGlob,
//! T-disc-lastActivity, the S3 facts of the tail scan, the caps and the
//! malformed-row rules.

#[path = "fixtures/vendor/support.rs"]
mod support;

use std::io::Write;

use gilde_code_lib::contract::activity::ErrorSub;
use gilde_code_lib::contract::common::PermissionMode;
use gilde_code_lib::discovery::claude::transcripts::{self, FirstPrompt, TailMode};
use gilde_code_lib::discovery::safe_read::{self, Root};
use support::*;

fn open(path: &std::path::Path) -> safe_read::Opened {
    let root = Root::new(path.parent().unwrap().parent().unwrap()).unwrap();
    safe_read::open_in(&root, path).unwrap()
}

/// Located by glob under any slug — incl. a `-w` worker's worktree slug —
/// never by building the slug (§5.2.5, R2 C3).
#[test]
fn t_disc_transcript_glob_finds_any_slug() {
    let h = Home::new();
    let cwd = h.workdir("repo");
    let sid = uuid(11);
    let p = h.write_transcript(&h.claude, "-Users-x-repo--claude-worktrees-feat-a-b", &sid, "claude/transcripts/normal.jsonl", &[("CWD", cwd.to_str().unwrap())]);
    h.write_transcript(&h.claude, "-other", &uuid(12), "claude/transcripts/normal.jsonl", &[("CWD", cwd.to_str().unwrap())]);
    let projects = Root::new(&h.claude.join("projects")).unwrap();
    assert_eq!(transcripts::find_transcript(&projects, &sid), Some(p));
    assert_eq!(transcripts::find_transcript(&projects, &uuid(99)), None);
    assert_eq!(transcripts::find_transcript(&projects, "../../etc"), None);
    assert_eq!(transcripts::session_id_of(std::path::Path::new("/a/b/x.jsonl")), None);
}

/// Last activity = the last row carrying `timestamp`, never mtime (R2 E30).
#[test]
fn t_disc_last_activity_from_rows_not_mtime() {
    let h = Home::new();
    let cwd = h.workdir("p");
    let sid = uuid(13);
    let p = h.write_transcript(&h.claude, "-p", &sid, "claude/transcripts/normal.jsonl", &[("CWD", cwd.to_str().unwrap())]);
    // The CLI appends metadata rows without timestamps; mtime moves to "now".
    let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
    writeln!(f, r#"{{"type":"ai-title","aiTitle":"x","sessionId":"{sid}"}}"#).unwrap();
    drop(f);
    let tail = transcripts::scan_tail(&open(&p), TailMode::History).unwrap();
    let expected = transcripts::parse_iso8601_ms("2026-10-21T04:25:12.500Z");
    assert_eq!(tail.last_row_at, expected);
    let mtime = std::fs::metadata(&p).unwrap().modified().unwrap();
    let mtime_ms = mtime.duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    assert!(mtime_ms > expected.unwrap());
}

#[test]
fn head_and_tail_of_a_normal_transcript() {
    let h = Home::new();
    let cwd = h.workdir("p");
    let sid = uuid(14);
    let p = h.write_transcript(&h.claude, "-p", &sid, "claude/transcripts/normal.jsonl", &[("CWD", cwd.to_str().unwrap())]);
    let f = open(&p);
    let head = transcripts::scan_head(&f).unwrap();
    assert_eq!(head.session_id.as_deref(), Some(sid.as_str()));
    assert_eq!(head.cwd.as_deref(), cwd.to_str());
    assert_eq!(head.git_branch.as_deref(), Some("main"));
    assert_eq!(head.entrypoint.as_deref(), Some("cli"));
    assert!(matches!(head.first_prompt, FirstPrompt::Found(ref s) if s.contains("first prompt")));
    assert_eq!(head.permission_mode, Some(PermissionMode::Default));
    let tail = transcripts::scan_tail(&f, TailMode::Live).unwrap();
    assert!(tail.ai_title.as_deref().unwrap().contains("ai title"));
    assert!(tail.last_reply.as_deref().unwrap().contains("final reply"));
    assert!(tail.facts.open_tool_use.is_none(), "the tool call was closed by its result");
    assert!(tail.facts.last_turn_end_at.is_some());
    assert!(tail.facts.last_human_prompt_at.is_some());
    assert!(tail.facts.last_api_error.is_none());
    assert!(!tail.whole_file, "the live window stops at the current turn's prompt");
    let full = transcripts::scan_tail(&f, TailMode::Full).unwrap();
    assert!(full.whole_file);
    assert_eq!(full.turns, Some(1));
    assert_eq!(full.permission_mode, Some(PermissionMode::Default));
    // agent-name (user-set name) and custom-title near the metadata tail.
    let mut w = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
    writeln!(w, r#"{{"type":"custom-title","customTitle":"my custom","sessionId":"{sid}"}}"#).unwrap();
    writeln!(w, r#"{{"type":"agent-name","agentName":"renamed by user","sessionId":"{sid}"}}"#).unwrap();
    writeln!(w, r#"{{"type":"continued-in","continuedInSessionId":"{}","sessionId":"{sid}"}}"#, uuid(15)).unwrap();
    drop(w);
    let tail = transcripts::scan_tail(&open(&p), TailMode::History).unwrap();
    assert_eq!(tail.user_name.as_deref(), Some("renamed by user"));
    assert_eq!(tail.custom_title.as_deref(), Some("my custom"));
    assert_eq!(tail.continued_in.as_deref(), Some(uuid(15).as_str()));
}

#[test]
fn s3_open_tool_question_and_api_errors() {
    let h = Home::new();
    let cwd = h.workdir("p");
    let open_tool = h.write_transcript(&h.claude, "-p", &uuid(21), "claude/transcripts/open_tool.jsonl", &[("CWD", cwd.to_str().unwrap())]);
    let tail = transcripts::scan_tail(&open(&open_tool), TailMode::Live).unwrap();
    let t = tail.facts.open_tool_use.unwrap();
    assert_eq!(t.tool_use_id, "toolu_11");
    assert_eq!(t.tool_name, "AskUserQuestion");
    assert!(t.is_question);

    let final_err = h.write_transcript(&h.claude, "-p", &uuid(22), "claude/transcripts/api_error_final.jsonl", &[("CWD", cwd.to_str().unwrap())]);
    let tail = transcripts::scan_tail(&open(&final_err), TailMode::Live).unwrap();
    assert_eq!(tail.facts.last_api_error.map(|e| e.1), Some(ErrorSub::SignInNeeded));

    let recovered = h.write_transcript(&h.claude, "-p", &uuid(23), "claude/transcripts/api_error_recovered.jsonl", &[("CWD", cwd.to_str().unwrap())]);
    let tail = transcripts::scan_tail(&open(&recovered), TailMode::Live).unwrap();
    assert!(tail.facts.last_api_error.is_none(), "a retried request that recovered is not an error state");
    assert!(tail.facts.last_turn_end_at.is_some());
}

/// Malformed input (§5.8, §13.1): unknown row types ignored, non-JSON and
/// wrong-shape rows skipped and counted, a partial trailing line never parsed,
/// invalid UTF-8 rows skipped.
#[test]
fn malformed_rows_partial_lines_and_invalid_utf8() {
    let h = Home::new();
    let cwd = h.workdir("p");
    let p = h.write_transcript(&h.claude, "-p", &uuid(31), "claude/transcripts/unknown_rows.jsonl", &[("CWD", cwd.to_str().unwrap())]);
    let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
    f.write_all(b"{\"type\":\"user\",\"message\":{\"content\":\"\xff\xfe broken\"},\"timestamp\":\"2026-10-21T07:00:05.000Z\"}\n").unwrap();
    f.write_all(br#"{"type":"assistant","timestamp":"2026-10-21T07:09:09.000Z","message":{"content":[{"type":"text","te"#).unwrap();
    drop(f);
    let o = open(&p);
    let head = transcripts::scan_head(&o).unwrap();
    assert_eq!(head.entrypoint.as_deref(), Some("sdk-cli"));
    let tail = transcripts::scan_tail(&o, TailMode::History).unwrap();
    assert_eq!(tail.last_row_at, transcripts::parse_iso8601_ms("2026-10-21T07:00:03.000Z"), "neither the invalid-UTF-8 row nor the partial line counts");
    assert!(tail.rows_bad >= 2);
}

/// A 4.5 MiB line: the head stops at 64 KiB ("first prompt: unavailable"),
/// the tail never buffers past 4 MiB and keeps reading what fits.
#[test]
fn caps_head_64k_tail_4m() {
    let h = Home::new();
    let dir = h.claude.join("projects/-big");
    std::fs::create_dir_all(&dir).unwrap();
    let sid = uuid(41);
    let p = dir.join(format!("{sid}.jsonl"));
    let mut f = std::fs::File::create(&p).unwrap();
    writeln!(f, r#"{{"type":"attachment","sessionId":"{sid}","cwd":"/w","version":"2.1.306","entrypoint":"cli","timestamp":"2026-10-21T08:00:00.000Z"}}"#).unwrap();
    writeln!(f, r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"t","content":"{}"}}]}},"timestamp":"2026-10-21T08:00:01.000Z"}}"#, "x".repeat(4_700_000)).unwrap();
    writeln!(f, r#"{{"type":"user","message":{{"content":"late prompt"}},"timestamp":"2026-10-21T08:00:02.000Z"}}"#).unwrap();
    drop(f);
    let o = open(&p);
    let head = transcripts::scan_head(&o).unwrap();
    assert_eq!(head.first_prompt, FirstPrompt::Unavailable);
    assert_eq!(head.session_id.as_deref(), Some(sid.as_str()));
    let tail = transcripts::scan_tail(&o, TailMode::Live).unwrap();
    assert_eq!(tail.last_row_at, transcripts::parse_iso8601_ms("2026-10-21T08:00:02.000Z"));
    assert!(!tail.whole_file);
}

/// NFD-named directories (APFS keeps the bytes given) are found like any other.
#[test]
fn nfd_slug_dirs_are_listed() {
    use unicode_normalization::UnicodeNormalization;
    let h = Home::new();
    let cwd = h.workdir("p");
    let slug: String = "-Users-u-한글".nfd().collect();
    let sid = uuid(51);
    h.write_transcript(&h.claude, &slug, &sid, "claude/transcripts/normal.jsonl", &[("CWD", cwd.to_str().unwrap())]);
    let projects = Root::new(&h.claude.join("projects")).unwrap();
    assert!(transcripts::find_transcript(&projects, &sid).is_some());
}
