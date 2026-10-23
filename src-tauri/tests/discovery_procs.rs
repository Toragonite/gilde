//! The libproc scan, argv/env parsing and kqueue (contract §5.2.2, §5.9):
//! T-disc-procs, T-disc-bgHelpers, T-disc-teammateArgv, T-disc-kqueue and the
//! ≤ 5 ms-per-scan budget. Real processes are only ones this test starts (a
//! copy of `/bin/sh` named `claude`, `/bin/sleep`); every scan of the real
//! process table is scoped so no other process's argv is ever read.

#[path = "fixtures/vendor/support.rs"]
mod support;

use std::collections::HashSet;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gilde_code_lib::contract::common::Vendor;
use gilde_code_lib::contract::process::ProcRole;
use gilde_code_lib::contract::services::SourceFacts;
use gilde_code_lib::contract::session::Origin;
use gilde_code_lib::discovery::kqueue::{KqEvent, KqWatcher, Track};
use gilde_code_lib::discovery::procs::{self, FakeBundles, FakeProc, Libproc, ProcSource, Scanner};
use gilde_code_lib::discovery::{fail_closed_redactor, Engine, NoOwnProcesses};
use support::*;

struct Fake {
    child: Child,
    _dir: tempfile::TempDir,
}

impl Drop for Fake {
    fn drop(&mut self) {
        // Our own process group (the copy of sh and its sleep).
        // SAFETY: killpg on a group this test created.
        unsafe {
            libc::killpg(self.child.id() as i32, libc::SIGTERM);
        }
        let _ = self.child.wait();
    }
}

/// A copy of `/bin/sh` named `claude` running `sleep` with `args` after it.
fn spawn_fake_claude(args: &[&str], env: &[(&str, &str)]) -> Fake {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("claude");
    std::fs::copy("/bin/sh", &exe).unwrap();
    let mut cmd = Command::new(&exe);
    cmd.arg("-c").arg("sleep 30; :");
    cmd.args(args);
    cmd.env_clear();
    cmd.env("PATH", "/usr/bin:/bin");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    cmd.process_group(0);
    let child = cmd.spawn().unwrap();
    // An event, not a fixed sleep (A10.7): a fresh copy of a binary can take
    // a while to exec (its first code-signing assessment), so wait until the
    // kernel reports the whole argv — the program, `-c`, the script, `args`.
    let pid = child.id() as i32;
    let want = 3 + args.len();
    let lp = Libproc::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let argc = lp.args(pid).and_then(|r| r.argv.as_ref().map(Vec::len));
        if argc == Some(want) {
            break;
        }
        assert!(Instant::now() < deadline, "the fake never showed its {want} argv strings (last: {argc:?})");
        std::thread::sleep(Duration::from_millis(10));
    }
    Fake { child, _dir: dir }
}

/// T-disc-procs: `KERN_PROCARGS2` gives exactly argc strings; the env block is
/// scanned in place for the marker *names* only — values never leave the
/// buffer — and the scanner keeps only the redacted argv.
#[test]
fn t_disc_procs_real_process_argv_and_env_markers() {
    let secret_env = "SECRET-ENV-VALUE-91c2";
    let fake = spawn_fake_claude(
        &["--agent-id", "alpha@session-9", "--agent-name", "alpha", "--team-name", "session-9", "--settings", "{\"apiKeyHelper\":\"x\"}"],
        &[("CCO_TEAM_MEMBER", secret_env), ("GILDE_APP_INSTANCE", secret_env), ("AWS_SECRET_ACCESS_KEY", secret_env)],
    );
    let pid = fake.child.id() as i32;
    let lp = Libproc::new();
    let raw = lp.args(pid).expect("our own child's argv is readable");
    let argv = raw.argv.clone().unwrap();
    assert_eq!(argv.len(), 11, "argc strings exactly: {}", argv.len());
    assert_eq!(&argv[1..3], &["-c".to_string(), "sleep 30; :".to_string()]);
    assert_eq!(raw.cco_team_member, Some(true));
    assert_eq!(raw.gilde_app_instance, Some(true));
    assert!(!format!("{raw:?}").contains(secret_env));
    drop(raw);

    let info = lp.bsd_info(pid).unwrap();
    assert_eq!(info.ppid, std::process::id() as i32);
    assert!(lp.exec_path(pid).unwrap().ends_with("/claude"));
    assert!(lp.cwd(pid).is_some());
    assert!(lp.task_info(pid).unwrap().rss_bytes > 0);
    assert_eq!(lp.alive(pid), procs::Alive::Yes);
    assert_eq!(lp.alive(1), procs::Alive::OtherUser, "launchd: EPERM, never signalled");

    // A scanner caches a process's classification for its lifetime, so a
    // scan that met the argv mid-read is retried with a fresh scanner — only
    // while the scan could not read the argv at all (`argv_redacted` empty).
    // With the argv read, the role must be Teammate: anything else is a
    // classifier bug, never retried away.
    let deadline = Instant::now() + Duration::from_secs(10);
    let out = loop {
        let mut s = Scanner::new(Arc::new(Libproc::new()), fail_closed_redactor(), Arc::new(NoOwnProcesses), Arc::new(FakeBundles::default()));
        s.set_scope(Some(HashSet::from([pid])));
        let out = s.scan(1, 1);
        let read = out.snapshot.procs.iter().any(|p| p.pid == pid && !p.argv_redacted.is_empty());
        if read || Instant::now() >= deadline {
            break out;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let mine = out.snapshot.procs.iter().find(|p| p.pid == pid).expect("the fake is in the snapshot");
    assert!(!mine.argv_redacted.is_empty(), "the scan never read the fake's argv: {mine:?}");
    assert_eq!(mine.vendor, Some(Vendor::Claude));
    assert_eq!(procs::role_of(mine), ProcRole::Teammate, "{mine:?}");
    assert_eq!(mine.labels.agent_name.as_deref(), Some("alpha"));
    assert_eq!(mine.cco_team_member, Some(true));
    assert_eq!(mine.argv_redacted, vec!["claude".to_string(), "<arguments hidden>".to_string()]);
    assert!(!format!("{:?}", out.snapshot).contains(secret_env));
    assert!(!format!("{:?}", out.snapshot).contains("apiKeyHelper"));
    for other in out.snapshot.procs.iter().filter(|p| p.pid != pid) {
        assert!(other.argv_redacted.is_empty(), "no other process's argv was read");
    }
}

/// T-disc-bgHelpers (R6 §6 LP-22, F-47): `claude daemon run --origin
/// transient` (ppid 1) and `claude bg-pty-host …` (incl. a spare) are
/// `VendorHelper`, never sessions — no evidence, no Status unknown row — while
/// the `--bg` session itself (registry `kind:"bg"`) is Background.
#[test]
fn t_disc_bg_helpers_are_never_sessions() {
    let h = Home::new();
    let cwd = h.workdir("p");
    let p = procs();
    let start = 1_792_558_000;
    p.insert(600, claude_proc(start, 1, &["claude.exe", "daemon", "run", "--origin", "transient", "--spawned-by", "{}"], &cwd));
    p.insert(601, claude_proc(start, 600, &["claude", "bg-pty-host", "--bg-pty-host", "/tmp/cc-daemon-501/h/spare/x.pty.sock", "200", "50", "--"], &cwd));
    p.insert(602, claude_proc(start, 600, &["claude", "bg-pty-host", "--bg-pty-host", "/tmp/cc-daemon-501/h/pty/04b2b152.sock", "200", "50", "--"], &cwd));
    let sid = "04b2b152-0000-4000-8000-000000000001";
    p.insert(603, claude_proc(start, 602, &["claude.exe", "--session-id", sid, "--model", "haiku"], &cwd));
    h.write_registry(&h.claude, 603, sid, start, &cwd, "bg", "cli", "idle", ",\"jobId\":\"04b2b152\"");
    std::fs::create_dir_all(h.claude.join("jobs/04b2b152")).unwrap();
    std::fs::write(h.claude.join("jobs/04b2b152/state.json"), template("claude/jobs/state.json", &[("SID", sid)])).unwrap();
    let rec = Arc::new(Rec::default());
    let mut e = Engine::new(h.config(p, Arc::new(TeamsRec::default()), None), rec.clone());
    e.reconcile();
    e.tick_at(gilde_code_lib::discovery::now_wall_ms(), gilde_code_lib::discovery::now_up_ms() + 60_000);
    for helper in [600, 601, 602] {
        assert!(rec.for_pid(helper).is_empty(), "helper {helper} produced evidence");
    }
    let origin = rec.for_sid(sid).into_iter().find_map(|f| match f {
        SourceFacts::Meta(m) => m.origin,
        _ => None,
    });
    assert_eq!(origin, Some(Origin::Background));
    let snap = e.last_snapshot().unwrap();
    for helper in [600, 601, 602] {
        let info = snap.procs.iter().find(|x| x.pid == helper).unwrap();
        assert_eq!(procs::role_of(info), ProcRole::VendorHelper);
    }
}

/// T-disc-teammateArgv: the three flags anywhere, in any order, `--k v` or
/// `--k=v`; any one missing → not a teammate.
#[test]
fn t_disc_teammate_argv_orders_and_forms() {
    let p = procs();
    let cwd = std::env::temp_dir();
    p.insert(700, claude_proc(1, 1, &["claude", "--model", "haiku", "--team-name=session-1", "--agent-color", "blue", "--agent-name=beta", "--permission-mode", "default", "--agent-id", "beta@session-1"], &cwd));
    p.insert(701, claude_proc(1, 1, &["claude", "--agent-id", "beta@session-1", "--team-name", "session-1"], &cwd));
    p.insert(702, claude_proc(1, 1, &["claude", "--agent-id", "--agent-name", "x", "--team-name", "t"], &cwd));
    let mut s = Scanner::new(p, fail_closed_redactor(), Arc::new(NoOwnProcesses), Arc::new(FakeBundles::default()));
    let out = s.scan(1, 1);
    let get = |pid| out.snapshot.procs.iter().find(|x| x.pid == pid).unwrap().clone();
    assert_eq!(procs::role_of(&get(700)), ProcRole::Teammate);
    assert_eq!(get(700).labels.team_name.as_deref(), Some("session-1"));
    assert_eq!(procs::role_of(&get(701)), ProcRole::Session, "--agent-name missing");
    assert_eq!(procs::role_of(&get(702)), ProcRole::Session, "a flag value that is itself a flag");
}

/// T-disc-kqueue: `NOTE_EXIT` per tracked pid, keyed by `(pid, start)`; a pid
/// whose start time differs (reused) is reported gone at once.
#[test]
fn t_disc_kqueue_exit_events_keyed_by_pid_and_start() {
    let events: Arc<Mutex<Vec<KqEvent>>> = Arc::default();
    let sink = events.clone();
    let kq = KqWatcher::start(Arc::new(move |e| sink.lock().unwrap().push(e))).unwrap();
    let lp = Libproc::new();
    let mut child = Command::new("/bin/sleep").arg("0.4").spawn().unwrap();
    let pid = child.id() as i32;
    let start = lp.bsd_info(pid).unwrap().start_sec;
    assert_eq!(kq.track(pid, start, |p| lp.bsd_info(p).map(|b| b.start_sec)), Track::Tracked);
    assert_eq!(kq.track(pid, start, |p| lp.bsd_info(p).map(|b| b.start_sec)), Track::AlreadyTracked);
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(5) && !events.lock().unwrap().contains(&KqEvent::Exit { pid, start_sec: start }) {
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.wait();
    assert!(events.lock().unwrap().contains(&KqEvent::Exit { pid, start_sec: start }), "NOTE_EXIT delivered");
    // A reused pid (the kernel start differs from the tracked one).
    let mut c2 = Command::new("/bin/sleep").arg("2").spawn().unwrap();
    let pid2 = c2.id() as i32;
    let real = lp.bsd_info(pid2).unwrap().start_sec;
    assert_eq!(kq.track(pid2, real - 100, |p| lp.bsd_info(p).map(|b| b.start_sec)), Track::AlreadyGone);
    assert!(events.lock().unwrap().contains(&KqEvent::Exit { pid: pid2, start_sec: real - 100 }));
    // Re-registration under the real key works.
    assert_eq!(kq.track(pid2, real, |p| lp.bsd_info(p).map(|b| b.start_sec)), Track::Tracked);
    kq.untrack(pid2);
    let _ = c2.kill();
    let _ = c2.wait();
    // An exited pid: gone at once.
    assert_eq!(kq.track(pid, start, |p| lp.bsd_info(p).map(|b| b.start_sec)), Track::AlreadyGone);
    drop(kq);
}

fn thread_cpu_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: valid out-pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// The steady-state scan cost (best of 5) and the process count.
fn steady_scan() -> (u64, usize) {
    let mut s = Scanner::new(Arc::new(Libproc::new()), fail_closed_redactor(), Arc::new(NoOwnProcesses), Arc::new(FakeBundles::default()));
    s.set_scope(Some(HashSet::new()));
    s.scan(1, 1); // the first pass resolves every exec path once
    let mut best = u64::MAX;
    let mut n = 0usize;
    for i in 0..5 {
        let t = thread_cpu_ns();
        let out = s.scan(2 + i, 2 + i as u64);
        best = best.min(thread_cpu_ns() - t);
        n = out.snapshot.procs.len();
    }
    eprintln!("scan: {n} processes of this user, best {:.2} ms CPU", best as f64 / 1e6);
    (best, n)
}

/// Functional (A10.7): the scan works on the real table, within 10× the budget.
#[test]
fn scan_steady_state_functional() {
    let (best, n) = steady_scan();
    assert!(n > 0);
    let ceiling = 50_000_000u64 * (n.max(600) as u64) / 600;
    assert!(best <= ceiling, "scan {best} ns > 10× budget {ceiling} ns");
}

/// G5 (§5.2.2): ≤ 5 ms CPU per scan with 600 pids (scaled; scoped so no argv
/// is read).
#[test]
#[ignore = "G5 benchmark (A10.7): cargo test --test discovery_procs -- --ignored"]
fn bench_scan_budget_steady_state() {
    let (best, n) = steady_scan();
    let budget = 5_000_000u64 * (n.max(600) as u64) / 600;
    assert!(best <= budget, "scan {best} ns > budget {budget} ns");
}

/// The cpu-active flag (≥ 1 % of a core between scans) feeds R8.
#[test]
fn cpu_activity_between_scans() {
    let p = procs();
    p.insert(800, FakeProc { ppid: 1, uid: UID, start_sec: 5, exec_path: Some("/x/claude".into()), argv: Some(vec!["claude".into()]), cpu_ns: 0, ..FakeProc::default() });
    let mut s = Scanner::new(p.clone(), fail_closed_redactor(), Arc::new(NoOwnProcesses), Arc::new(FakeBundles::default()));
    s.scan(1_000, 1);
    p.table.lock().unwrap().get_mut(&800).unwrap().cpu_ns = 100_000_000; // 100 ms in 3 s
    s.scan(4_000, 2);
    assert_eq!(s.by_pid(800).unwrap().cpu_active_at, Some(4_000));
    s.scan(7_000, 3);
    assert_eq!(s.by_pid(800).unwrap().cpu_active_at, Some(4_000), "idle since");
}

// ---------------------------------------------------------------------------
// A39.1(a) (G7c-01): discovery's classifier is the one shared classifier and
// knows the install layouts Gilde itself spawns (no real process runs here).
// ---------------------------------------------------------------------------

const NATIVE_CLAUDE: &str = "/Users/u/.local/share/claude/versions/2.1.307";
const CURSOR_NODE: &str = "/Users/u/.local/share/cursor-agent/versions/2026.10.19-6bbf2c4/node";
const CURSOR_INDEX: &str = "/Users/u/.local/share/cursor-agent/versions/2026.10.19-6bbf2c4/index.js";

fn strings(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|p| (*p).to_string()).collect()
}

/// The reviewer's G7c-01c, verbatim.
#[test]
fn a39_discovery_classifier_knows_both_layouts() {
    use gilde_code_lib::discovery::procs::classify;
    let native = classify(NATIVE_CLAUDE, Some(&[NATIVE_CLAUDE.to_string(), "--resume".into()]));
    assert!(native.vendor.is_some(), "discovery misses a native-installer claude started by realpath: {native:?}");
    let argv: Vec<String> = vec!["/Users/u/.local/bin/cursor-agent".into(), "--use-system-ca".into(), CURSOR_INDEX.into()];
    let cursor = classify(CURSOR_NODE, Some(&argv));
    assert!(cursor.vendor.is_some(), "discovery misses every cursor-agent process: {cursor:?}");
}

/// Roles and label flags are read after the script and its leading flags.
#[test]
fn a39_discovery_reads_roles_and_labels_after_the_script() {
    use gilde_code_lib::discovery::procs::classify;
    // cursor's launcher argv: `--resume <chatId>` after index.js.
    let c = classify(
        CURSOR_NODE,
        Some(&strings(&["/Users/u/.local/bin/cursor-agent", "--use-system-ca", CURSOR_INDEX, "--resume", "0123abcd-4567"])),
    );
    assert_eq!(c.vendor, Some(Vendor::Cursor));
    assert_eq!(c.role, Some(ProcRole::Session));
    assert_eq!(c.labels.resume.as_deref(), Some("0123abcd-4567"));
    // An unreadable argv: the vendored node is still cursor's.
    assert_eq!(classify(CURSOR_NODE, None).vendor, Some(Vendor::Cursor));
    // Native installer, other semvers: sessions, helpers and teammates.
    for path in [NATIVE_CLAUDE, "/Users/u/.local/share/claude/versions/2.1.400", "/x/claude/versions/3.0.0-beta.1"] {
        let s = classify(path, Some(&strings(&[path, "--resume", &uuid(7)])));
        assert_eq!((s.vendor, s.role), (Some(Vendor::Claude), Some(ProcRole::Session)), "{path}");
        assert_eq!(s.labels.resume, Some(uuid(7)), "{path}");
        let helper = classify(path, Some(&strings(&[path, "daemon", "run"])));
        assert_eq!(helper.role, Some(ProcRole::VendorHelper), "{path}");
        let mate = classify(
            path,
            Some(&strings(&[path, "--agent-id", "a@t", "--agent-name", "a", "--team-name", "t"])),
        );
        assert_eq!(mate.role, Some(ProcRole::Teammate), "{path}");
    }
    // node flags before the script; the subcommand after it decides the role.
    let cli = "/u/lib/node_modules/@anthropic-ai/claude-code/cli.js";
    let n = classify("/opt/homebrew/bin/node", Some(&strings(&["node", "--max-old-space-size=8192", cli, "daemon"])));
    assert_eq!((n.vendor, n.role), (Some(Vendor::Claude), Some(ProcRole::VendorHelper)));
    let b = classify("/opt/homebrew/bin/bun", Some(&strings(&["bun", "run", cli, "--resume", &uuid(8)])));
    assert_eq!((b.vendor, b.role), (Some(Vendor::Claude), Some(ProcRole::Session)));
    assert_eq!(b.labels.resume, Some(uuid(8)));
    // A plain node running anything else is not a candidate.
    let plain = classify("/opt/homebrew/bin/node", Some(&strings(&["node", "/srv/server.js", "--resume", &uuid(9)])));
    assert_eq!((plain.vendor, plain.candidate), (None, false));
    let flagged = classify("/opt/homebrew/bin/node", Some(&strings(&["node", "--inspect", "/srv/server.js"])));
    assert_eq!((flagged.vendor, flagged.candidate), (None, false));
}

/// The scan itself sees both layouts (G7c-01: "discovery never sees those
/// processes"), with the argv readable and without it.
#[test]
fn a39_the_scan_sees_native_installer_and_cursor_processes() {
    let p = procs();
    let fake = |exec: &str, argv: Option<Vec<String>>| FakeProc {
        ppid: 1,
        uid: UID,
        start_sec: 5,
        exec_path: Some(exec.to_string()),
        argv,
        ..FakeProc::default()
    };
    p.insert(900, fake(NATIVE_CLAUDE, Some(strings(&[NATIVE_CLAUDE]))));
    p.insert(
        901,
        fake(CURSOR_NODE, Some(strings(&["/Users/u/.local/bin/cursor-agent", "--use-system-ca", CURSOR_INDEX]))),
    );
    p.insert(902, fake(CURSOR_NODE, None));
    p.insert(903, fake("/opt/homebrew/bin/node", Some(strings(&["node", "--no-warnings", "/srv/server.js"]))));
    p.insert(
        904,
        fake("/opt/homebrew/bin/bun", Some(strings(&["bun", "/u/node_modules/@anthropic-ai/claude-code/cli.js"]))),
    );
    let mut s = Scanner::new(p, fail_closed_redactor(), Arc::new(NoOwnProcesses), Arc::new(FakeBundles::default()));
    s.scan(1_000, 1);
    for (pid, vendor) in [(900, Vendor::Claude), (901, Vendor::Cursor), (902, Vendor::Cursor), (904, Vendor::Claude)] {
        let seen = s.by_pid(pid).unwrap_or_else(|| panic!("pid {pid} not scanned"));
        assert!(seen.candidate, "pid {pid} is a vendor CLI: {seen:?}");
        assert_eq!(seen.vendor, Some(vendor), "pid {pid}");
        assert_eq!(seen.role, Some(ProcRole::Session), "pid {pid}");
    }
    assert!(!s.by_pid(903).is_some_and(|p| p.candidate), "a plain node is not a candidate");
}

/// A49.6: a process whose first argv read fails (mid-exec) is not cached with
/// its exec-path-only classification: the next scan reads the argv again and
/// the teammate is a Teammate, not a solo session for its whole life.
#[test]
fn a49_an_unreadable_argv_is_read_again_on_the_next_scan() {
    let p = procs();
    let teammate = strings(&["claude", "--agent-id", "alpha@t", "--agent-name", "alpha", "--team-name", "t"]);
    p.insert(950, FakeProc { ppid: 1, uid: UID, start_sec: 5, exec_path: Some("/x/claude".into()), argv: None, ..FakeProc::default() });
    let mut s = Scanner::new(p.clone(), fail_closed_redactor(), Arc::new(NoOwnProcesses), Arc::new(FakeBundles::default()));
    let first = s.scan(1_000, 1);
    let seen = first.snapshot.procs.iter().find(|x| x.pid == 950).expect("scanned").clone();
    assert_eq!(procs::role_of(&seen), ProcRole::Session, "the exec path alone, this scan");
    assert!(first.appeared.contains(&(950, 5)));
    p.table.lock().unwrap().get_mut(&950).unwrap().argv = Some(teammate);
    let second = s.scan(4_000, 2);
    let seen = second.snapshot.procs.iter().find(|x| x.pid == 950).expect("scanned").clone();
    assert_eq!(procs::role_of(&seen), ProcRole::Teammate, "the argv was read again");
    assert_eq!(seen.labels.agent_name.as_deref(), Some("alpha"));
    assert!(!second.appeared.contains(&(950, 5)), "a re-read is not a new process");
    // Read once, the classification is cached as before.
    p.table.lock().unwrap().get_mut(&950).unwrap().argv = None;
    let third = s.scan(7_000, 3);
    let seen = third.snapshot.procs.iter().find(|x| x.pid == 950).expect("scanned").clone();
    assert_eq!(procs::role_of(&seen), ProcRole::Teammate, "a read argv stays cached");
}
