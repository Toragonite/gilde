//! A33.4 / §5.8 / PM-19 (T-disc-permissionDenied, HP-7 variants): an
//! `EPERM`/`EACCES` on the config root, a project dir or a transcript file
//! makes that source `PermissionDenied` — never "missing": the last facts are
//! kept, the affected sessions (and only those) get `SourceDenied(true)`, no
//! session or team ends because a read failed, and access coming back clears
//! it. Every chmod is undone by a drop guard (also on panic); temp dirs only.

#[path = "fixtures/vendor/support.rs"]
mod support;

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gilde_code_lib::contract::activity::ProcessFacts;
use gilde_code_lib::contract::services::SourceFacts;
use gilde_code_lib::contract::store::{SourceKind, SourceState};
use gilde_code_lib::discovery::Engine;
use support::*;

const START: i64 = 1_792_558_000;

/// Paths made mode 000, restored on drop.
#[derive(Default)]
struct Denied(Vec<(PathBuf, u32)>);

impl Denied {
    fn deny(&mut self, p: &Path, back: u32) {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o000)).unwrap();
        self.0.push((p.to_path_buf(), back));
    }
    fn restore(&mut self) {
        while let Some((p, m)) = self.0.pop() {
            let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(m));
        }
    }
}

impl Drop for Denied {
    fn drop(&mut self) {
        self.restore();
    }
}

/// The current state of `kind` (the last status the engine sent).
fn state(rec: &Rec, kind: SourceKind) -> Option<SourceState> {
    rec.statuses.lock().unwrap().iter().rev().find(|s| s.source == kind).map(|s| s.state.clone())
}

fn denied_flag(rec: &Rec, sid: &str) -> Option<bool> {
    rec.for_sid(sid).iter().rev().find_map(|f| match f {
        SourceFacts::SourceDenied(b) => Some(*b),
        _ => None,
    })
}

fn gone(rec: &Rec, sid: &str) -> bool {
    rec.for_sid(sid).iter().any(|f| matches!(f, SourceFacts::Process(ProcessFacts::Gone { .. }) | SourceFacts::Process(ProcessFacts::Exited { .. })))
}

fn append_row(path: &Path, sid: &str) {
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(f, r#"{{"type":"attachment","attachment":{{"type":"queued_command"}},"timestamp":"2026-10-23T00:00:00.000Z","sessionId":"{sid}"}}"#).unwrap();
}

struct Two {
    h: Home,
    a: String,
    b: String,
    a_path: PathBuf,
}

/// Two live sessions with transcripts in two project dirs.
fn two(p: &Arc<gilde_code_lib::discovery::procs::FakeProcs>) -> Two {
    let h = Home::new();
    let (a, b) = (uuid(31), uuid(32));
    let (cwd_a, cwd_b) = (h.workdir("a"), h.workdir("b"));
    h.write_registry(&h.claude, 4310, &a, START, &cwd_a, "interactive", "cli", "idle", "");
    h.write_registry(&h.claude, 4320, &b, START, &cwd_b, "interactive", "cli", "idle", "");
    let a_path = h.write_transcript(&h.claude, "-a", &a, "claude/transcripts/normal.jsonl", &[("CWD", cwd_a.to_str().unwrap())]);
    h.write_transcript(&h.claude, "-b", &b, "claude/transcripts/normal.jsonl", &[("CWD", cwd_b.to_str().unwrap())]);
    p.insert(4310, claude_proc(START, 1, &["claude"], &cwd_a));
    p.insert(4320, claude_proc(START, 1, &["claude"], &cwd_b));
    Two { h, a, b, a_path }
}

/// The whole config root unreadable: registry, teams and transcripts are
/// `PermissionDenied`; sessions keep their facts and are marked; the team
/// does not end; restoring clears every mark.
#[test]
fn t_disc_denied_config_root_keeps_sessions_and_teams() {
    let p = procs();
    let h = Home::new();
    let cwd = h.workdir("proj");
    let team = "session-0a33d004".to_string();
    let lead = uuid(41);
    h.write_registry(&h.claude, 4400, &lead, START, &cwd, "interactive", "cli", "busy", "");
    h.write_transcript(&h.claude, "-proj", &lead, "claude/transcripts/normal.jsonl", &[("CWD", cwd.to_str().unwrap())]);
    p.insert(4400, claude_proc(START, 1, &["claude", "--teammate-mode", "tmux"], &cwd));
    p.insert(4401, claude_proc(START + 10, 1, &["claude.exe", "--agent-id", &format!("alpha@{team}"), "--agent-name", "alpha", "--team-name", &team, "--agent-color", "blue", "--parent-session-id", &lead], &cwd));
    let tdir = h.claude.join("teams").join(&team);
    std::fs::create_dir_all(tdir.join("inboxes")).unwrap();
    std::fs::write(
        tdir.join("config.json"),
        template("claude/teams/config.json", &[("TEAM", &team), ("CREATED_AT", &((START - 1) * 1000).to_string()), ("JOINED_AT", &((START + 10) * 1000).to_string()), ("BOOT_ID", "0a33d004"), ("CWD", cwd.to_str().unwrap())]),
    )
    .unwrap();
    let teams = Arc::new(TeamsRec::default());
    let rec = Arc::new(Rec::default());
    let mut e = Engine::new(h.config(p, teams.clone(), None), rec.clone());
    e.reconcile();
    assert_eq!(state(&rec, SourceKind::Registry), Some(SourceState::Ok));
    assert!(!teams.obs.lock().unwrap().is_empty(), "the team was observed");
    let teammate_hint = rec.for_pid(4401).first().map(|u| u.hint.clone()).expect("the teammate has evidence");
    rec.clear();

    let mut guard = Denied::default();
    guard.deny(&h.claude, 0o755);
    e.reconcile();
    e.tick();
    for k in [SourceKind::Registry, SourceKind::Teams, SourceKind::Tasks, SourceKind::Transcripts] {
        assert_eq!(state(&rec, k), Some(SourceState::PermissionDenied), "{k:?} while the config root is denied");
    }
    assert_eq!(denied_flag(&rec, &lead), Some(true), "the lead is marked");
    assert!(!gone(&rec, &lead), "never Ended because a read failed");
    assert!(teams.gone.lock().unwrap().is_empty(), "no team ends because a read failed");
    let mate: Vec<_> = rec.for_pid(4401);
    assert!(mate.iter().any(|u| u.facts == SourceFacts::SourceDenied(true)), "the teammate is marked: {mate:?}");
    assert!(mate.iter().all(|u| u.hint == teammate_hint), "the teammate keeps its identity (account) while the dir is hidden");
    assert!(!rec.for_sid(&lead).iter().any(|f| matches!(f, SourceFacts::Transcript(t) if t.last_row_at.is_none())), "no empty transcript facts");

    guard.restore();
    rec.clear();
    e.reconcile();
    for k in [SourceKind::Registry, SourceKind::Teams, SourceKind::Transcripts] {
        assert_eq!(state(&rec, k), Some(SourceState::Ok), "{k:?} after access came back");
    }
    assert_eq!(denied_flag(&rec, &lead), Some(false), "cleared once readable again");
}

/// A project dir and then a transcript file unreadable: `Transcripts` is
/// denied, only the session whose transcript it is gets the mark (per path,
/// not per account), the registry stays `Ok`; each is restored and clears.
#[test]
fn t_disc_denied_project_dir_and_transcript_file_mark_only_their_session() {
    let p = procs();
    let s = two(&p);
    let rec = Arc::new(Rec::default());
    let mut e = Engine::new(s.h.config(p, Arc::new(TeamsRec::default()), None), rec.clone());
    e.reconcile();
    rec.clear();
    let mut guard = Denied::default();

    // (1) The project dir `<cfg>/projects/-a`.
    guard.deny(&s.h.claude.join("projects/-a"), 0o755);
    e.reconcile();
    assert_eq!(state(&rec, SourceKind::Transcripts), Some(SourceState::PermissionDenied));
    assert_eq!(state(&rec, SourceKind::Registry), Some(SourceState::Ok), "the registry is readable");
    assert_eq!(denied_flag(&rec, &s.a), Some(true));
    assert_ne!(denied_flag(&rec, &s.b), Some(true), "the other session is not marked");
    assert!(!gone(&rec, &s.a));
    guard.restore();
    rec.clear();
    e.reconcile();
    assert_eq!(state(&rec, SourceKind::Transcripts), Some(SourceState::Ok));
    assert_eq!(denied_flag(&rec, &s.a), Some(false));

    // (2) The transcript file itself (the vendor wrote a row first, so a
    // re-read is due).
    rec.clear();
    append_row(&s.a_path, &s.a);
    guard.deny(&s.a_path, 0o644);
    e.reconcile();
    assert_eq!(state(&rec, SourceKind::Transcripts), Some(SourceState::PermissionDenied));
    assert_eq!(denied_flag(&rec, &s.a), Some(true));
    assert_ne!(denied_flag(&rec, &s.b), Some(true));
    guard.restore();
    rec.clear();
    e.reconcile();
    assert_eq!(state(&rec, SourceKind::Transcripts), Some(SourceState::Ok));
    assert_eq!(denied_flag(&rec, &s.a), Some(false));
}

/// First sight while denied: a session whose project dir is unreadable before
/// discovery ever found its transcript is marked (the search could not look
/// everywhere), and found once readable.
#[test]
fn t_disc_denied_project_dir_before_first_sight() {
    let p = procs();
    let s = two(&p);
    let mut guard = Denied::default();
    guard.deny(&s.h.claude.join("projects/-a"), 0o755);
    let rec = Arc::new(Rec::default());
    let mut e = Engine::new(s.h.config(p, Arc::new(TeamsRec::default()), None), rec.clone());
    e.reconcile();
    assert_eq!(state(&rec, SourceKind::Transcripts), Some(SourceState::PermissionDenied));
    assert_eq!(denied_flag(&rec, &s.a), Some(true));
    assert!(rec.for_sid(&s.a).iter().any(|f| matches!(f, SourceFacts::Process(ProcessFacts::Alive { .. }))), "still live");
    guard.restore();
    rec.clear();
    e.reconcile();
    assert_eq!(denied_flag(&rec, &s.a), Some(false));
    assert!(rec.for_sid(&s.a).iter().any(|f| matches!(f, SourceFacts::Transcript(t) if t.last_row_at.is_some())), "found and read once readable");
}
