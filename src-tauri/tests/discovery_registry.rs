//! The Claude registry and the liveness rule (contract §5.2.1, §5.8):
//! T-disc-liveness, T-disc-procStart, T-disc-torn, T-disc-unknownFormat,
//! T-disc-registryKind and the registry caps.

#[path = "fixtures/vendor/support.rs"]
mod support;

use std::sync::Arc;
use std::time::Duration;

use gilde_code_lib::contract::activity::{ProcessFacts, RegStatus};
use gilde_code_lib::contract::services::SourceFacts;
use gilde_code_lib::contract::session::Origin;
use gilde_code_lib::discovery::claude::registry::{self, FormatError, StaleWhy, Verdict};
use gilde_code_lib::discovery::safe_read::Root;
use gilde_code_lib::discovery::{Engine, HostedLead, OwnMatch, OwnProcesses};
use support::*;

const START: i64 = 1_792_558_000;

fn verdict_of(h: &Home, pid: i32, procs: &gilde_code_lib::discovery::procs::FakeProcs) -> Verdict {
    let root = Root::new(&h.claude.join("sessions")).unwrap();
    let path = h.claude.join(format!("sessions/{pid}.json"));
    let (parsed, _) = registry::read_registry(&root, &path, pid, Duration::from_millis(1)).unwrap();
    registry::verdict(parsed, pid, procs)
}

/// All four checks, each failing alone (§5.2.1, PM-20).
#[test]
fn t_disc_liveness_each_check_fails_alone() {
    let h = Home::new();
    let cwd = h.workdir("p");
    let p = procs();
    // good
    h.write_registry(&h.claude, 101, &uuid(1), START, &cwd, "interactive", "cli", "idle", "");
    p.insert(101, claude_proc(START, 1, &["claude"], &cwd));
    assert!(matches!(verdict_of(&h, 101, &p), Verdict::Live { weak: false, .. }));
    // dead pid
    h.write_registry(&h.claude, 102, &uuid(2), START, &cwd, "interactive", "cli", "busy", "");
    assert_eq!(verdict_of(&h, 102, &p), Verdict::Stale { why: StaleWhy::NotAlive });
    // reused pid: a compiler started an hour later under the same pid
    h.write_registry(&h.claude, 103, &uuid(3), START, &cwd, "interactive", "cli", "busy", "");
    let mut reused = claude_proc(START + 3600, 1, &["claude"], &cwd);
    reused.exec_path = Some("/usr/bin/clang".into());
    p.insert(103, reused);
    assert_eq!(verdict_of(&h, 103, &p), Verdict::Stale { why: StaleWhy::StartMismatch });
    // same start, not a claude executable
    let mut cc = claude_proc(START, 1, &["cc"], &cwd);
    cc.exec_path = Some("/usr/bin/cc".into());
    p.insert(103, cc);
    assert_eq!(verdict_of(&h, 103, &p), Verdict::Stale { why: StaleWhy::NotClaude });
    // another user's process (EPERM): excluded, never signalled
    h.write_registry(&h.claude, 104, &uuid(4), START, &cwd, "interactive", "cli", "idle", "");
    let mut other = claude_proc(START, 1, &["claude"], &cwd);
    other.other_user = true;
    p.insert(104, other);
    assert_eq!(verdict_of(&h, 104, &p), Verdict::Stale { why: StaleWhy::OtherUser });
    // pid in the file ≠ file-name pid: stale (hidden, counted) whatever runs
    // under either pid (A33.5(a)) — the `pid` field (101) names a live claude,
    // and then the file-name pid (105) is a live claude too.
    let body = std::fs::read_to_string(h.claude.join("sessions/101.json")).unwrap();
    std::fs::write(h.claude.join("sessions/105.json"), body).unwrap();
    assert_eq!(verdict_of(&h, 105, &p), Verdict::Stale { why: StaleWhy::PidMismatch });
    p.insert(105, claude_proc(START, 1, &["claude"], &cwd));
    assert_eq!(verdict_of(&h, 105, &p), Verdict::Stale { why: StaleWhy::PidMismatch });
    let lying = std::fs::read(h.claude.join("sessions/105.json")).unwrap();
    assert_eq!(registry::parse_registry(&lying, 105).unwrap_err(), FormatError::PidMismatch);
    // peerProtocol 2 → unknownFormat, the live process reported (Status unknown{UnknownPeerProtocol})
    h.write_registry(&h.claude, 106, &uuid(6), START, &cwd, "interactive", "cli", "idle", "");
    let body = std::fs::read_to_string(h.claude.join("sessions/106.json")).unwrap().replace("\"peerProtocol\":1", "\"peerProtocol\":2");
    std::fs::write(h.claude.join("sessions/106.json"), body).unwrap();
    p.insert(106, claude_proc(START, 1, &["claude"], &cwd));
    assert!(matches!(verdict_of(&h, 106, &p), Verdict::UnknownFormat { why: FormatError::PeerProtocol, live: Some(_) }));
    // non-UUID session id
    h.write_registry(&h.claude, 107, "not-a-uuid", START, &cwd, "interactive", "cli", "idle", "");
    p.insert(107, claude_proc(START, 1, &["claude"], &cwd));
    assert!(matches!(verdict_of(&h, 107, &p), Verdict::UnknownFormat { why: FormatError::SessionId, live: Some(_) }));
    // a pre-warmed spare
    h.write_registry(&h.claude, 108, &uuid(8), START, &cwd, "interactive", "cli", "idle", ",\"spare\":true");
    p.insert(108, claude_proc(START, 1, &["claude"], &cwd));
    assert_eq!(verdict_of(&h, 108, &p), Verdict::Spare);
}

/// `procStart` grammar: space-padded day, all months, UTC (§5.2.1, R2 E7).
#[test]
fn t_disc_proc_start_grammar_round_trips_every_month() {
    for month in 0..12i64 {
        for day in [1i64, 9, 10, 28] {
            let secs = 1_767_225_600 + (month * 30 + day) * 86_400 + 3_723; // 2026, varied
            let s = ctime_utc(secs);
            assert_eq!(registry::parse_proc_start(&s), Some(secs), "{s}");
        }
    }
    assert_eq!(ctime_utc(0), "Thu Jan  1 00:00:00 1970");
}

/// A torn registry (in-place rewrite) is retried once after 75 ms (§5.8, R2 E32).
#[test]
fn t_disc_torn_registry_retried_once() {
    let h = Home::new();
    let cwd = h.workdir("p");
    let full = h.write_registry(&h.claude, 201, &uuid(21), START, &cwd, "interactive", "cli", "busy", "");
    let body = std::fs::read_to_string(&full).unwrap();
    std::fs::write(&full, &body[..body.len() / 2]).unwrap();
    let root = Root::new(&h.claude.join("sessions")).unwrap();
    let writer = {
        let full = full.clone();
        let body = body.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            std::fs::write(&full, body).unwrap();
        })
    };
    let (parsed, _) = registry::read_registry(&root, &full, 201, registry::TORN_RETRY).unwrap();
    writer.join().unwrap();
    assert!(parsed.is_ok(), "the retry after 75 ms read the completed file");
    std::fs::write(&full, &body[..10]).unwrap();
    let (parsed, _) = registry::read_registry(&root, &full, 201, Duration::from_millis(1)).unwrap();
    assert_eq!(parsed.unwrap_err(), FormatError::Parse, "still torn after one retry: unknownFormat");
}

/// Caps: a registry file over 64 KiB is `unknownFormat`; `.key` files beside
/// the entries are never listed; more than 4,096 entries keep the newest.
#[test]
fn t_disc_unknown_format_and_caps() {
    let h = Home::new();
    let cwd = h.workdir("p");
    let big = h.write_registry(&h.claude, 301, &uuid(31), START, &cwd, "interactive", "cli", "idle", &format!(",\"pad\":\"{}\"", "x".repeat(70 * 1024)));
    let root = Root::new(&h.claude.join("sessions")).unwrap();
    let (parsed, _) = registry::read_registry(&root, &big, 301, Duration::from_millis(1)).unwrap();
    assert_eq!(parsed.unwrap_err(), FormatError::TooLarge);
    std::fs::write(h.claude.join("sessions/301.0123456789abcdef.key"), CANARY).unwrap();
    let (files, truncated) = registry::list_registry(&root).unwrap();
    assert_eq!(files.len(), 1, "only ^\\d+\\.json$ names");
    assert!(!truncated);
    for pid in 0..4_100 {
        std::fs::write(h.claude.join(format!("sessions/{}.json", 10_000 + pid)), "{}").unwrap();
    }
    let (files, truncated) = registry::list_registry(&root).unwrap();
    assert_eq!(files.len(), registry::SESSIONS_ENTRIES_CAP);
    assert!(truncated);
}

struct Own {
    pid: i32,
    start: i64,
}

impl OwnProcesses for Own {
    fn lookup(&self, pid: i32, start_sec: i64) -> Option<OwnMatch> {
        (pid == self.pid && start_sec == self.start).then(|| OwnMatch { spawn_id: "spawn-1".into(), this_instance: true })
    }
    fn match_planned(&self, _argv: &[String], _cwd: Option<&str>) -> Option<String> {
        None
    }
    fn hosted_leads(&self) -> Vec<HostedLead> {
        Vec::new()
    }
}

/// T-disc-registryKind (R6 §6, §8 item 22): registry `kind:"interactive"` is
/// written for `-p` stream-json sessions too, so it never classifies Gilde's
/// own Conversation process: the pid match marks it Gilde's (`gildeSpawnId`),
/// and `entrypoint:"gilde"` with an own row is never `OtherGilde` [N-05];
/// without an own row it is `OtherGilde` [F-42].
#[test]
fn t_disc_registry_kind_interactive_never_misclassifies_gildes_own() {
    let h = Home::new();
    let cwd = h.workdir("p");
    let p = procs();
    h.write_registry(&h.claude, 401, &uuid(41), START, &cwd, "interactive", "gilde", "busy", "");
    p.insert(401, claude_proc(START, 1, &["claude", "-p", "--output-format", "stream-json", "--input-format", "stream-json"], &cwd));
    h.write_registry(&h.claude, 402, &uuid(42), START, &cwd, "interactive", "gilde", "busy", "");
    p.insert(402, claude_proc(START, 1, &["claude", "-p", "--output-format", "stream-json"], &cwd));
    let rec = Arc::new(Rec::default());
    let mut cfg = h.config(p, Arc::new(TeamsRec::default()), None);
    cfg.own = Arc::new(Own { pid: 401, start: START });
    let mut e = Engine::new(cfg, rec.clone());
    e.reconcile();
    let own: Vec<_> = rec.for_pid(401);
    assert!(!own.is_empty());
    assert!(own.iter().all(|u| u.hint.gilde_spawn_id.as_deref() == Some("spawn-1")), "every hint of the own process carries its spawn id");
    let origin_of = |pid: i32| {
        rec.for_pid(pid).into_iter().find_map(|u| match u.facts {
            SourceFacts::Meta(m) => m.origin,
            _ => None,
        })
    };
    assert_ne!(origin_of(401), Some(Origin::OtherGilde), "own row wins over the gilde marker [N-05]");
    assert_eq!(origin_of(402), Some(Origin::OtherGilde), "a foreign entrypoint:\"gilde\" is Another Gilde [F-42]");
    // Registry status reaches World as S2.
    let reg = rec.for_sid(&uuid(42)).into_iter().find_map(|f| match f {
        SourceFacts::Registry(r) => Some(r),
        _ => None,
    });
    assert_eq!(reg.unwrap().status, Some(RegStatus::Busy));
}

/// A live entry that disappears (the CLI deletes the file at exit) and whose
/// process is gone yields exactly one `Gone`.
#[test]
fn registry_exit_yields_one_gone() {
    let h = Home::new();
    let cwd = h.workdir("p");
    let p = procs();
    let f = h.write_registry(&h.claude, 501, &uuid(51), START, &cwd, "interactive", "cli", "idle", "");
    p.insert(501, claude_proc(START, 1, &["claude"], &cwd));
    let rec = Arc::new(Rec::default());
    let mut e = Engine::new(h.config(p.clone(), Arc::new(TeamsRec::default()), None), rec.clone());
    e.reconcile();
    assert!(rec.for_sid(&uuid(51)).iter().any(|f| matches!(f, SourceFacts::Process(ProcessFacts::Alive { .. }))));
    p.remove(501);
    std::fs::remove_file(f).unwrap();
    e.tick();
    e.reconcile();
    let gone = rec.for_sid(&uuid(51)).iter().filter(|f| matches!(f, SourceFacts::Process(ProcessFacts::Gone { .. }))).count();
    assert_eq!(gone, 1);
}

/// A33.5(a) through the engine: a registry file whose `pid` field differs
/// from its name is stale — hidden, counted in `registryStale` and the
/// Registry source's `hiddenStale` — whether the `pid` field names another
/// live claude or the file-name pid is a live claude itself; its session id
/// is never attributed; the live process under the file-name pid is an
/// ordinary unregistered claude (no session id, after the 10 s grace).
#[test]
fn t_disc_pid_mismatch_is_stale_whatever_runs() {
    use gilde_code_lib::contract::store::SourceKind;
    use gilde_code_lib::discovery::{now_up_ms, now_wall_ms};
    let h = Home::new();
    let cwd = h.workdir("p");
    let p = procs();
    // The honest lead 4400 (live), and two liars.
    h.write_registry(&h.claude, 4400, &uuid(60), START, &cwd, "interactive", "cli", "idle", "");
    p.insert(4400, claude_proc(START, 1, &["claude"], &cwd));
    let lie = |file_pid: i32, field_pid: i32, sid: &str| {
        h.write_registry(&h.claude, field_pid, sid, START, &cwd, "interactive", "cli", "busy", "");
        let body = std::fs::read_to_string(h.claude.join(format!("sessions/{field_pid}.json"))).unwrap();
        std::fs::remove_file(h.claude.join(format!("sessions/{field_pid}.json"))).unwrap();
        std::fs::write(h.claude.join(format!("sessions/{file_pid}.json")), body).unwrap();
    };
    // (1) names the live lead's pid; the file-name pid is dead.
    lie(4401, 4400, &uuid(61));
    // (2) the file-name pid is a live claude; the field names a dead pid.
    lie(4402, 4998, &uuid(62));
    p.insert(4402, claude_proc(START, 1, &["claude"], &cwd));
    // `lie` rewrote 4400.json for (1): write the honest one again.
    h.write_registry(&h.claude, 4400, &uuid(60), START, &cwd, "interactive", "cli", "idle", "");
    let rec = Arc::new(Rec::default());
    let mut e = Engine::new(h.config(p, Arc::new(TeamsRec::default()), None), rec.clone());
    e.reconcile();
    let d = e.diagnostics();
    assert_eq!((d.registry_live, d.registry_stale, d.registry_unknown_format), (1, 2, 0), "{d:?}");
    let reg = rec.statuses.lock().unwrap().iter().rev().find(|s| s.source == SourceKind::Registry).cloned().unwrap();
    assert_eq!(reg.hidden_stale, 2, "{reg:?}");
    let sids = rec.sids();
    assert!(sids.contains(&uuid(60)), "the honest lead keeps its own id");
    assert!(!sids.contains(&uuid(61)) && !sids.contains(&uuid(62)), "a lying file's id is never shown: {sids:?}");
    // The live process behind (2): unregistered, never with the lying id.
    let up = now_up_ms();
    e.tick_at(now_wall_ms(), up + 10_500);
    let ev = rec.for_pid(4402);
    assert!(ev.iter().any(|u| matches!(u.facts, SourceFacts::Process(ProcessFacts::Alive { .. }))), "shown as an unregistered claude");
    assert!(ev.iter().all(|u| u.hint.vendor_session_id.is_none()), "no session id: {ev:?}");
}

// ---------------------------------------------------------------------------
// A47.1 (G7c-17): the liveness verdict classifies the resolved exec path
// with the one shared classifier
// ---------------------------------------------------------------------------

mod fixtures;

/// The reviewer's G7c-17: a claude started through the native installer's
/// `~/.local/bin/claude` link (`proc_pidpath` = `…/versions/<semver>`) is a
/// live registry entry. The subject is the `fake_agent` fixture copied to that
/// layout, never a real CLI; it is killed at the end.
#[test]
fn g7c_17_a_native_installer_claude_registry_entry_is_live() {
    use gilde_code_lib::discovery::claude::registry::RegistryRecord;
    use gilde_code_lib::discovery::procs::{Libproc, ProcSource};
    struct Kid(std::process::Child);
    impl Drop for Kid {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let home = tempfile::tempdir().unwrap();
    let home = std::fs::canonicalize(home.path()).unwrap();
    let versions = home.join(".local/share/claude/versions");
    std::fs::create_dir_all(&versions).unwrap();
    let real = versions.join("2.1.307");
    std::fs::copy(fixtures::fake_agent_path(), &real).unwrap();
    let bin = home.join(".local/bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(&real, bin.join("claude")).unwrap();
    // What a terminal does for `claude`: exec the PATH entry (the link).
    let kid = Kid(
        std::process::Command::new(bin.join("claude"))
            .arg("hang")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let pid = kid.0.id() as i32;
    std::thread::sleep(Duration::from_millis(300));
    let procs = Libproc::new();
    let exec = procs.exec_path(pid).expect("exec path");
    let start = procs.bsd_info(pid).expect("bsd info").start_sec;
    let rec = RegistryRecord {
        pid,
        session_id: "6a1e2b0c-1111-4222-8333-444455556666".into(),
        cwd: Some(home.to_string_lossy().into_owned()),
        started_at: Some(start * 1000),
        proc_start: Some(start),
        version: Some("2.1.307".into()),
        peer_protocol: Some(1),
        kind: None,
        entrypoint: Some("cli".into()),
        status: None,
        status_updated_at: None,
        waiting_for: None,
        user_name: None,
        job_id: None,
        spare: false,
    };
    let v = registry::verdict(Ok(rec), pid, &procs);
    assert!(
        matches!(v, Verdict::Live { .. }),
        "§5.2.1: a live claude (launched as ~/.local/bin/claude; proc_pidpath = {exec:?}) must be Live, got {v:?}"
    );
}

/// The verdict's exec check, by layout: another semver, an npm claude under
/// node (argv names claude's script, flags first), a plain node, a codex.
#[test]
fn a47_the_verdict_classifies_the_exec_path_like_the_scan() {
    let h = Home::new();
    let cwd = h.workdir("p");
    let p = procs();
    let with = |exec: &str, argv: &[&str]| {
        let mut f = claude_proc(START, 1, argv, &cwd);
        f.exec_path = Some(exec.to_string());
        f
    };
    let cases: [(i32, &str, &[&str], bool); 5] = [
        (201, "/Users/u/.local/share/claude/versions/2.1.400", &["claude"], true),
        (202, "/Users/u/.local/share/claude/versions/3.0.0-beta.1", &["claude", "--resume"], true),
        (
            203,
            "/opt/homebrew/bin/node",
            &["node", "--no-warnings", "/u/lib/node_modules/@anthropic-ai/claude-code/cli.js"],
            true,
        ),
        (204, "/opt/homebrew/bin/node", &["node", "/srv/server.js"], false),
        (205, "/opt/homebrew/bin/codex", &["codex"], false),
    ];
    for (pid, exec, argv, live) in cases {
        h.write_registry(&h.claude, pid, &uuid(pid as u32), START, &cwd, "interactive", "cli", "idle", "");
        p.insert(pid, with(exec, argv));
        let v = verdict_of(&h, pid, &p);
        if live {
            assert!(matches!(v, Verdict::Live { .. }), "{exec} {argv:?}: {v:?}");
        } else {
            assert_eq!(v, Verdict::Stale { why: StaleWhy::NotClaude }, "{exec} {argv:?}");
        }
    }
}
