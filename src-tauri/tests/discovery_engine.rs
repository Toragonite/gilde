//! The discovery engine end to end (contract §5.1, §5.8, §5.9, §5.10):
//! evidence of a live registry session, T-disc-permissionDenied, the 10 s
//! unregistered grace, T-disc-fsevents, T-disc-orchestrator, sibling accounts
//! and the threaded service.

#[path = "fixtures/vendor/support.rs"]
mod support;

use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gilde_code_lib::contract::activity::ProcessFacts;
use gilde_code_lib::contract::services::SourceFacts;
use gilde_code_lib::contract::session::Origin;
use gilde_code_lib::contract::store::{SourceKind, SourceState};
use gilde_code_lib::discovery::fsevents::{FsSignal, FsWatch, RootTag, WatchSpec};
use gilde_code_lib::discovery::procs::{FakeBundles, FakeProc};
use gilde_code_lib::discovery::{now_up_ms, now_wall_ms, Discovery, Engine};
use support::*;

const START: i64 = 1_792_558_000;

/// A live registry session yields Alive + Registry (S2) + Meta + Transcript (S3)
/// with a resolvable hint, and its cwd's location facts.
#[test]
fn live_registry_session_evidence() {
    let h = Home::new();
    let cwd = h.workdir("proj");
    std::fs::create_dir_all(cwd.join(".git")).unwrap();
    std::fs::write(cwd.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    let sid = uuid(1);
    h.write_registry(&h.claude, 4000, &sid, START, &cwd, "interactive", "cli", "busy", "");
    h.write_transcript(&h.claude, "-proj", &sid, "claude/transcripts/open_tool.jsonl", &[("CWD", cwd.to_str().unwrap())]);
    let p = procs();
    // claude ← -zsh ← login ← iTermServer inside iTerm.app
    p.insert(3997, FakeProc { ppid: 1, uid: UID, start_sec: 10, comm: "iTermServer-3.5".into(), exec_path: Some("/Applications/iTerm.app/Contents/MacOS/iTermServer-3.5".into()), ..FakeProc::default() });
    p.insert(3998, FakeProc { ppid: 3997, uid: 0, start_sec: 11, comm: "login".into(), exec_path: Some("/usr/bin/login".into()), ..FakeProc::default() });
    p.insert(3999, FakeProc { ppid: 3998, uid: UID, start_sec: 12, comm: "zsh".into(), exec_path: Some("/bin/zsh".into()), ..FakeProc::default() });
    p.insert(4000, claude_proc(START, 3999, &["claude", "--model", "opus"], &cwd));
    p.insert(4001, FakeProc { ppid: 4000, uid: UID, start_sec: START + 5, comm: "node".into(), exec_path: Some("/opt/node".into()), rss_bytes: 25_000_000, ..FakeProc::default() });
    let mut bundles = FakeBundles::default();
    bundles.0.insert("/Applications/iTerm.app".into(), ("com.googlecode.iterm2".into(), "iTerm".into()));
    let rec = Arc::new(Rec::default());
    let mut cfg = h.config(p, Arc::new(TeamsRec::default()), None);
    cfg.bundles = Arc::new(bundles);
    let mut e = Engine::new(cfg, rec.clone());
    e.reconcile();
    let updates: Vec<_> = rec.all().into_iter().filter(|u| u.hint.vendor_session_id.as_deref() == Some(sid.as_str())).collect();
    let hint = &updates[0].hint;
    assert_eq!(hint.account_id.as_ref(), Some(&h.claude_account()));
    assert_eq!(hint.process.as_ref().unwrap().as_str(), format!("x_4000_{START}"));
    assert!(updates.iter().any(|u| matches!(u.facts, SourceFacts::Process(ProcessFacts::Alive { pid: 4000, .. }))));
    let meta = updates.iter().find_map(|u| match &u.facts {
        SourceFacts::Meta(m) => Some(m.clone()),
        _ => None,
    }).unwrap();
    assert_eq!(meta.origin, Some(Origin::Terminal), "entrypoint cli + login/iTermServer ancestor");
    assert_eq!(meta.owner.as_ref().unwrap().app_name, "iTerm2");
    assert_eq!(meta.model.as_deref(), Some("opus"));
    assert_eq!(meta.claude_version.as_deref(), Some("2.1.306"));
    assert_eq!(meta.memory_bytes, Some(70_000_000), "subtree RSS, quantized to 10 MB");
    assert!(meta.transcript_path.is_some());
    assert!(meta.cwd.is_some());
    let s3 = updates.iter().find_map(|u| match &u.facts {
        SourceFacts::Transcript(t) => Some(t.clone()),
        _ => None,
    }).unwrap();
    assert!(s3.open_tool_use.unwrap().is_question);
    let locs = rec.locations.lock().unwrap().clone();
    assert!(locs.iter().any(|l| l.git && l.branch.as_deref() == Some("main")));
    // A second identical pass sends nothing new (change-driven).
    let n = rec.all().len();
    e.reconcile();
    assert_eq!(rec.all().len(), n, "no duplicate evidence without a change");
}

/// T-disc-permissionDenied (PM-19): a source that becomes unreadable is
/// `PermissionDenied`, not empty: last facts kept, `SourceDenied(true)`, no
/// session turns Ended because a read failed.
#[test]
fn t_disc_permission_denied_keeps_facts() {
    let h = Home::new();
    let cwd = h.workdir("proj");
    let sid = uuid(2);
    h.write_registry(&h.claude, 4100, &sid, START, &cwd, "interactive", "cli", "idle", "");
    let p = procs();
    p.insert(4100, claude_proc(START, 1, &["claude"], &cwd));
    let rec = Arc::new(Rec::default());
    let mut e = Engine::new(h.config(p, Arc::new(TeamsRec::default()), None), rec.clone());
    e.reconcile();
    rec.clear();
    let sessions = h.claude.join("sessions");
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o000)).unwrap();
    e.reconcile();
    e.tick();
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o755)).unwrap();
    let facts = rec.for_sid(&sid);
    assert!(facts.contains(&SourceFacts::SourceDenied(true)), "{facts:?}");
    assert!(!facts.iter().any(|f| matches!(f, SourceFacts::Process(ProcessFacts::Gone { .. }))), "never Ended because a read failed");
    let statuses = rec.statuses.lock().unwrap().clone();
    assert!(statuses.iter().any(|s| s.source == SourceKind::Registry && s.state == SourceState::PermissionDenied));
    rec.clear();
    e.reconcile();
    assert!(rec.for_sid(&sid).contains(&SourceFacts::SourceDenied(false)), "cleared once readable again");
}

/// An unregistered Claude process is not shown for its first 10 s (§5.2.2).
#[test]
fn unregistered_claude_after_ten_seconds() {
    let h = Home::new();
    let cwd = h.workdir("p");
    let p = procs();
    p.insert(4200, claude_proc(START, 1, &["claude"], &cwd));
    let rec = Arc::new(Rec::default());
    let mut e = Engine::new(h.config(p, Arc::new(TeamsRec::default()), None), rec.clone());
    let up = now_up_ms();
    e.tick_at(now_wall_ms(), up);
    assert!(rec.for_pid(4200).is_empty(), "hidden while it may be starting");
    e.tick_at(now_wall_ms(), up + 9_000);
    assert!(rec.for_pid(4200).is_empty());
    e.tick_at(now_wall_ms(), up + 10_500);
    let ev = rec.for_pid(4200);
    assert!(ev.iter().any(|u| matches!(u.facts, SourceFacts::Process(ProcessFacts::Alive { .. }))));
    assert!(ev.iter().all(|u| u.hint.account_id.is_none() && u.hint.vendor_session_id.is_none()), "account unknown, env never read");
}

/// T-disc-fsevents: real FSEvents on temp roots — events are routed to their
/// root; a drop flag becomes a rescan (unit-tested in `fsevents.rs`); the
/// engine debounces per path and re-stats on handling.
#[test]
fn t_disc_fsevents_routes_real_events() {
    let h = Home::new();
    let sessions = std::fs::canonicalize(h.claude.join("sessions")).unwrap();
    let teams = std::fs::canonicalize(h.claude.join("teams")).unwrap();
    let got: Arc<Mutex<Vec<FsSignal>>> = Arc::default();
    let sink = got.clone();
    let _w = FsWatch::start(
        vec![WatchSpec { tag: RootTag::ClaudeSessions(0), path: sessions.clone() }, WatchSpec { tag: RootTag::ClaudeTeams(0), path: teams.clone() }],
        Arc::new(move |s| sink.lock().unwrap().push(s)),
    );
    std::thread::sleep(Duration::from_millis(300));
    std::fs::write(sessions.join("77.json"), "{}").unwrap();
    std::fs::create_dir_all(teams.join("session-x/inboxes")).unwrap();
    std::fs::write(teams.join("session-x/inboxes/a.json"), "[]").unwrap();
    std::fs::create_dir_all(sessions.join("nested")).unwrap();
    std::fs::write(sessions.join("nested/9.json"), "{}").unwrap();
    let t0 = Instant::now();
    let want_s = FsSignal::Changed { tag: RootTag::ClaudeSessions(0), path: sessions.join("77.json") };
    let want_t = FsSignal::Changed { tag: RootTag::ClaudeTeams(0), path: teams.join("session-x/inboxes/a.json") };
    while t0.elapsed() < Duration::from_secs(10) {
        let g = got.lock().unwrap();
        if g.contains(&want_s) && g.contains(&want_t) {
            break;
        }
        drop(g);
        std::thread::sleep(Duration::from_millis(50));
    }
    let g = got.lock().unwrap().clone();
    assert!(g.contains(&want_s), "{g:?}");
    assert!(g.contains(&want_t), "{g:?}");
    assert!(!g.iter().any(|s| matches!(s, FsSignal::Changed { path, .. } if path.ends_with("nested/9.json"))), "sessions/ is non-recursive");

    // Engine side: duplicate/out-of-order signals collapse; a rescan reconciles.
    let rec = Arc::new(Rec::default());
    let mut e = Engine::new(h.config(procs(), Arc::new(TeamsRec::default()), None), rec);
    for _ in 0..5 {
        e.on_fs(want_s.clone());
    }
    e.on_fs(FsSignal::Rescan { tag: RootTag::ClaudeTeams(0) });
    assert!(e.next_pending().is_some());
    e.flush_fs(true);
    assert!(e.next_pending().is_none());
    assert_eq!(e.diagnostics().fs_rescans, 1);
}

/// T-disc-orchestrator (PM-22): presence by `registry.json` or a socket, via
/// the reconcile (and the `$HOME` kqueue watch); absent → nothing renders.
#[test]
fn t_disc_orchestrator_presence() {
    let h = Home::new();
    let seen: Arc<Mutex<Vec<bool>>> = Arc::default();
    let s = seen.clone();
    let mut cfg = h.config(procs(), Arc::new(TeamsRec::default()), None);
    cfg.on_orchestrator = Some(Arc::new(move |b| s.lock().unwrap().push(b)));
    let mut e = Engine::new(cfg, Arc::new(Rec::default()));
    e.reconcile();
    std::fs::create_dir_all(h.home.join(".claude-code-orchestrator")).unwrap();
    std::fs::write(h.home.join(".claude-code-orchestrator/registry.json"), "{}").unwrap();
    e.reconcile();
    std::fs::remove_file(h.home.join(".claude-code-orchestrator/registry.json")).unwrap();
    e.reconcile();
    assert_eq!(*seen.lock().unwrap(), vec![false, true, false]);
}

/// Siblings are discovered by `stat`, reported once, and their sessions are
/// watched with the sibling's own account id.
#[test]
fn sibling_accounts_are_watched_read_only() {
    let h = Home::new();
    let cwd = h.workdir("p");
    let sib = h.home.join(".claude-work");
    std::fs::create_dir_all(sib.join("sessions")).unwrap();
    std::fs::write(sib.join(".claude.json"), CANARY).unwrap();
    std::fs::set_permissions(sib.join(".claude.json"), std::fs::Permissions::from_mode(0o000)).unwrap();
    h.write_registry(&sib, 4300, &uuid(3), START, &cwd, "interactive", "sdk-cli", "idle", "");
    let p = procs();
    let mut proc = claude_proc(START, 1, &["claude"], &cwd);
    proc.env_names = Some(vec!["PATH".into(), "CCO_TEAM_MEMBER".into()]);
    p.insert(4300, proc);
    let rec = Arc::new(Rec::default());
    let mut e = Engine::new(h.config(p, Arc::new(TeamsRec::default()), None), rec.clone());
    e.reconcile();
    e.reconcile();
    assert_eq!(rec.accounts.lock().unwrap().len(), 1);
    let acct = rec.accounts.lock().unwrap()[0].clone();
    let ev = rec.for_sid(&uuid(3));
    let hint_acct = rec.all().into_iter().find(|u| u.hint.vendor_session_id.as_deref() == Some(uuid(3).as_str())).unwrap().hint.account_id;
    assert_eq!(hint_acct, Some(acct.id));
    let origin = ev.iter().find_map(|f| match f {
        SourceFacts::Meta(m) => m.origin,
        _ => None,
    });
    assert_eq!(origin, Some(Origin::Orchestrator), "CCO_TEAM_MEMBER beats sdk-cli");
}

/// The threaded service: starts (pass 1), answers `sync`, publishes the
/// snapshot, stops cleanly with its watchers.
#[test]
fn discovery_thread_start_sync_stop() {
    let h = Home::new();
    let cwd = h.workdir("p");
    let p = procs();
    p.insert(4400, claude_proc(START, 1, &["claude"], &cwd));
    h.write_registry(&h.claude, 4400, &uuid(4), START, &cwd, "interactive", "cli", "idle", "");
    let rec = Arc::new(Rec::default());
    let snaps: Arc<Mutex<u32>> = Arc::default();
    let s = snaps.clone();
    let mut cfg = h.config(p, Arc::new(TeamsRec::default()), None);
    cfg.watch = true;
    cfg.on_snapshot = Some(Arc::new(move |_| *s.lock().unwrap() += 1));
    let d = Discovery::start(cfg, rec.clone());
    let diag = d.sync(Duration::from_secs(20)).expect("the engine answers");
    assert!(diag.scans >= 1 && diag.reconciles >= 1);
    assert!(d.last_snapshot().is_some());
    assert!(*snaps.lock().unwrap() >= 1);
    assert!(rec.sids().contains(&uuid(4)));
    d.set_frontmost(false);
    d.wake();
    assert!(d.sync(Duration::from_secs(20)).is_some());
    d.stop();
}
