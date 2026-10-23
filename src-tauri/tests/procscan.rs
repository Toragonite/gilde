//! Integration tests for the process scanner (T07 / DESIGN §8-7).
//!
//! These drive real processes through the `fake_agent` fixture (T06), because
//! the properties under test — "the descendant walk finds a grandchild the
//! process group does not cover", "the env marker is findable" — are only true
//! or false against the live kernel.

mod fixtures;

use std::time::{Duration, Instant};

use gilde_code_lib::supervise::procscan::{self, RowContext, Signatures, Snapshot};
use gilde_code_lib::supervise::{ProcGroupKind, ProcTag, RegistryIndex};

/// DESIGN §8-7 says the whole-machine snapshot costs about 15 ms; M1-PLAN T07
/// asks for a measurement with a 250 ms budget. The assertion is deliberately
/// looser than the budget so a loaded CI box does not produce a false failure —
/// the *number* is what matters and it is printed.
const PERF_BUDGET: Duration = Duration::from_millis(250);
const PERF_HARD_CEILING: Duration = Duration::from_millis(1_000);

#[test]
fn descendants_walk_finds_a_grandchild_that_outlives_its_parent() {
    let child = fixtures::start_spawn_grandchild(30);
    let parent_pid = child.parent.pid();
    let grandchild_pid = child.grandchild_pid;

    let snap = procscan::ps_snapshot().expect("ps snapshot");
    let descendants = snap.descendants_of(parent_pid);

    assert!(
        descendants.contains(&grandchild_pid),
        "descendants_of({parent_pid}) = {descendants:?} is missing the grandchild {grandchild_pid}"
    );
    assert!(
        snap.ancestors_of(grandchild_pid).contains(&parent_pid),
        "the walk must be symmetric"
    );
}

#[test]
fn a_setsid_escapee_leaves_the_process_group_but_not_the_parent_chain() {
    // This is the case DESIGN §8-6 unions three discovery paths for: `killpg`
    // alone cannot reach it, so the ppid walk has to.
    let escaped = fixtures::start_escape_group();
    let parent_pid = escaped.parent.pid();
    let escaped_pid = escaped.escaped_pid;

    let snap = procscan::ps_snapshot().expect("ps snapshot");

    let group = snap.pids_in_group(escaped.parent.pgid());
    assert!(
        !group.contains(&escaped_pid),
        "the escapee is supposed to have left the process group, but pids_in_group = {group:?}"
    );
    assert!(
        snap.descendants_of(parent_pid).contains(&escaped_pid),
        "the ppid walk is the only path left to the escapee"
    );
}

#[test]
fn pids_in_group_covers_a_fixture_and_its_own_children() {
    let child = fixtures::start_spawn_grandchild(30);
    let pgid = child.parent.pgid();
    assert_eq!(pgid, child.parent.pid(), "the fixture leads its own group");

    let snap = procscan::ps_snapshot().expect("ps snapshot");
    let group = snap.pids_in_group(pgid);
    assert!(group.contains(&child.parent.pid()));
    assert!(
        group.contains(&child.grandchild_pid),
        "an inherited group must include the child: {group:?}"
    );
}

#[test]
fn with_env_finds_our_marker_when_the_platform_allows_reading_environments() {
    // DESIGN §8-4 documents that reading another process's environment block is
    // restricted on macOS. Not finding the marker is therefore *reported*, not
    // failed — the identity fence never relies on this alone.
    let marker = format!("t07-{}", std::process::id());
    let running = MarkedChild::spawn(&["hang"], &[("GILDE_SPAWN_ID", marker.as_str())]);
    let pid = running.pid;

    let snap = procscan::ps_snapshot().expect("ps snapshot");
    let hits = snap.with_env("GILDE_SPAWN_ID", &marker);

    if hits.contains(&pid) {
        println!("with_env: env marker readable on this machine (pid {pid})");
        let env = procscan::environ_of(pid).expect("environ of our own child");
        assert!(env.iter().any(|e| e == &format!("GILDE_SPAWN_ID={marker}")));
    } else {
        println!(
            "with_env: SKIPPED — this machine did not expose the child's environment \
             (DESIGN §8-4 documented limitation). environ_of => {:?}",
            procscan::environ_of(pid).map(|e| e.len())
        );
    }

    // Whatever the platform allows, the call must not report processes that do
    // not carry the marker.
    assert!(
        !hits.contains(&(std::process::id() as i32)),
        "we do not carry the marker ourselves"
    );
    drop(running);
}

/// A fixture process started with extra environment variables.
///
/// `fixtures::Running` does not take an environment, and `tests/fixtures/mod.rs`
/// belongs to T06, so the env-marker test owns this small spawner instead.
struct MarkedChild {
    child: std::process::Child,
    pid: i32,
}

impl MarkedChild {
    fn spawn(args: &[&str], env: &[(&str, &str)]) -> MarkedChild {
        use std::os::unix::process::CommandExt as _;
        let mut cmd = std::process::Command::new(fixtures::fake_agent_path());
        cmd.args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            // DESIGN §8-1: every spawn leads its own process group.
            .process_group(0);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let child = cmd.spawn().expect("spawn fake_agent");
        let pid = child.id() as i32;
        MarkedChild { child, pid }
    }
}

impl Drop for MarkedChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn ps_args_and_cwd_resolve_for_a_live_fixture() {
    let mut running = fixtures::Running::start(&["hang"]);
    let pid = running.pid();

    let args = procscan::ps_args_of(pid).expect("argv of a live fixture");
    assert!(args.contains("fake_agent"), "unexpected argv: {args:?}");
    assert!(args.ends_with("hang"), "unexpected argv: {args:?}");

    let cwd = procscan::cwd_of(pid).expect("cwd of a live fixture");
    let expected = std::env::current_dir().expect("current_dir");
    assert_eq!(
        cwd.canonicalize().unwrap_or(cwd.clone()),
        expected.canonicalize().unwrap_or(expected.clone()),
        "the fixture inherits our working directory"
    );

    assert_eq!(
        procscan::uid_of(pid),
        // SAFETY: getuid() takes no arguments and always succeeds.
        Some(unsafe { libc::getuid() })
    );
    let start = procscan::start_time_of(pid).expect("kernel start time");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64;
    assert!((now - start).abs() < 120, "start time {start} vs now {now}");

    running.kill_and_reap();

    // After the process is gone the lookups say so instead of guessing.
    assert!(
        fixtures::wait_until_gone(pid, Duration::from_secs(5)),
        "fixture did not exit"
    );
    assert!(procscan::start_time_of(pid).is_none() || procscan::uid_of(pid).is_none());
}

#[test]
fn an_unregistered_fixture_is_never_classified_as_ours() {
    let mut running = fixtures::Running::start(&["hang"]);
    let pid = running.pid();

    let snap = procscan::ps_snapshot().expect("ps snapshot");
    let row = snap.get(pid).expect("fixture row");
    let empty = RegistryIndex::new("inst-current");
    let sigs = Signatures::defaults();

    let (group, _tags) = procscan::classify(row, &empty, &sigs);
    assert_ne!(group, ProcGroupKind::OursLive);
    assert_ne!(group, ProcGroupKind::OursOrphan);

    running.kill_and_reap();
}

#[test]
fn build_rows_covers_the_whole_machine_without_resolving_cwd() {
    let snap = procscan::ps_snapshot().expect("ps snapshot");
    let registry = RegistryIndex::new("inst-current");
    let sigs = Signatures::defaults();
    let ctx = RowContext::new(&snap, &registry, &sigs);

    let started = Instant::now();
    let rows = snap.build_rows(&ctx);
    let elapsed = started.elapsed();
    println!(
        "T07 build_rows: {} rows in {:?} ({:.1} µs/row)",
        rows.len(),
        elapsed,
        elapsed.as_micros() as f64 / rows.len().max(1) as f64
    );

    assert_eq!(rows.len(), snap.len());
    assert!(rows.iter().all(|r| r.cwd.is_none()), "cwd must stay lazy");
    assert!(
        rows.iter().any(|r| r.group == ProcGroupKind::Other),
        "a real machine has unrelated processes"
    );
}

/// What a second look at one pid found, when a row built from a snapshot
/// disagreed with what the kernel says about that pid *now*.
enum Recheck {
    /// The pid exited after the snapshot: nothing left to protect.
    Gone,
    /// The pid now names a different process (kernel start time moved).
    Recycled,
    /// Same process, and a FRESH snapshot — the one the kill path takes — does
    /// protect it: the first snapshot caught it before an `exec` into the bundle
    /// main (launchd's spawn trampoline does exactly that) or before its args
    /// settled. The row was right about what the pid *was*.
    ProtectedNow(String),
    /// Same process, fresh snapshot, still not protected: a real safety bug.
    StillUnprotected(String),
}

fn describe(snap: &Snapshot, pid: i32, row: Option<&gilde_code_lib::supervise::ProcRow>) -> String {
    let ps = snap.get(pid);
    format!(
        "pid {pid}: ppid {:?}, elapsed {:?}s, snapshot args {:?}, exe now {:?}, row tags {:?}, row start {:?} ms, kernel start now {:?} s",
        ps.map(|r| r.ppid),
        ps.map(|r| r.elapsed_secs),
        ps.map(|r| r.args.as_str()),
        procscan::exe_path_of(pid),
        row.map(|r| &r.tags),
        row.map(|r| r.start_time),
        procscan::start_time_of(pid),
    )
}

/// Re-derive one pid's protection exactly as the kill path would: a fresh
/// `ps` snapshot, `Snapshot::is_app_bundle_main` (kill.rs `exclusion_for` and
/// the bulk gate's predicate) and a freshly built monitor row.
fn recheck(pid: i32, row: &gilde_code_lib::supervise::ProcRow, first: &Snapshot) -> Recheck {
    let Some(start_now) = procscan::start_time_of(pid) else {
        return Recheck::Gone;
    };
    // `row.start_time` is libproc's (ms) or, when that was unreadable, derived
    // from `ps`'s whole-second etime — allow two seconds either way.
    if (start_now - row.start_time / 1000).abs() > 2 {
        return Recheck::Recycled;
    }
    let fresh = procscan::ps_snapshot().expect("fresh ps snapshot");
    let Some(ps) = fresh.get(pid) else {
        return Recheck::Gone;
    };
    let registry = RegistryIndex::new("inst-current");
    let sigs = Signatures::defaults();
    let ctx = RowContext::new(&fresh, &registry, &sigs);
    let fresh_row = fresh.build_row(ps, &ctx);
    let kill_path_refuses = fresh.is_app_bundle_main(pid);
    let detail = format!(
        "first look: {} | fresh look: {} | kill path refuses as appBundleMain: {kill_path_refuses}",
        describe(first, pid, Some(row)),
        describe(&fresh, pid, Some(&fresh_row)),
    );
    if fresh_row.is_protected() && kill_path_refuses {
        Recheck::ProtectedNow(detail)
    } else {
        Recheck::StillUnprotected(detail)
    }
}

#[test]
fn every_live_app_bundle_main_is_protected() {
    // DESIGN §8-6: protecting the foreground GUI apps is one string match, and
    // it has to hold for the machine as it actually is — including every bundle
    // whose path contains a space ("Google Chrome", "Visual Studio Code", every
    // "* Helper.app"), which is where the whitespace-split argv[0] failed.
    //
    // A4 (load verdict: TEST race, not product). The rows are built from ONE
    // snapshot, but the checks below ask the kernel about each pid LATER
    // (`exe_path_of`). On a loaded machine with heavy process churn that gap
    // contains exits (a short-lived `codesign … X.app/Contents/MacOS/X` or
    // `login … ShellLauncher` whose exe is then unreadable, which the old code
    // read as "it is a bundle main"), pid reuse, and processes caught before
    // the `exec` into their bundle main. The kill path never acts on the old
    // snapshot: it takes a fresh one and re-runs `is_app_bundle_main` per pid
    // immediately before any signal (kill.rs `SelfGuard::exclusion_for`,
    // commands/monitor.rs `bulk_refusals`). So a disagreement is re-derived the
    // same way before it may fail the test — and it still fails, naming the pid,
    // both command lines and the exe, when the fresh look does not protect it.
    let snap = procscan::ps_snapshot().expect("ps snapshot");
    let registry = RegistryIndex::new("inst-current");
    let sigs = Signatures::defaults();
    let ctx = RowContext::new(&snap, &registry, &sigs);
    let rows = snap.build_rows(&ctx);

    let mut checked = 0usize;
    let mut with_spaces = 0usize;
    let mut argument_only = 0usize;

    struct Tally {
        raced: Vec<String>,
        vanished: usize,
        failures: Vec<String>,
    }
    let mut tally = Tally {
        raced: Vec::new(),
        vanished: 0,
        failures: Vec::new(),
    };
    fn judge(
        tally: &mut Tally,
        snap: &Snapshot,
        pid: i32,
        row: &gilde_code_lib::supervise::ProcRow,
        what: &str,
    ) {
        match recheck(pid, row, snap) {
            Recheck::Gone | Recheck::Recycled => tally.vanished += 1,
            Recheck::ProtectedNow(detail) => tally.raced.push(detail),
            Recheck::StillUnprotected(detail) => tally.failures.push(format!("{what}: {detail}")),
        }
    }

    for ps in snap.rows() {
        // `\.app/Contents/MacOS/[^/]+$` against the whole argv selects the
        // command lines that end in a bundle path — but that is not the same as
        // "the command line is exactly the executable path": the regex also
        // matches a tool that merely *takes* a bundle path as an argument with a
        // slash-free tail. iTerm2 starts every shell as
        // `/usr/bin/login -fpl <user> /Applications/iTerm.app/Contents/MacOS/ShellLauncher --launch_shell`,
        // which is `login`, not a GUI app, and is deliberately not protected by
        // the bundle rule (see `a_bundle_path_that_is_only_an_argument_…`). So
        // the kernel decides which executable a process runs; the string form
        // is only the fallback when it will not say.
        if !procscan::is_app_bundle_path(&ps.args) {
            continue;
        }
        let row = rows
            .iter()
            .find(|r| r.pid == ps.pid)
            .unwrap_or_else(|| panic!("pid {} has no monitor row", ps.pid));
        match procscan::exe_path_of(ps.pid) {
            Some(exe) if !procscan::is_app_bundle_path(&exe) => {
                argument_only += 1;
                continue;
            }
            Some(_) => {}
            // Unreadable: another uid (protected by the uid rule — the row must
            // say so) or already gone (nothing left to protect).
            None => {
                if procscan::start_time_of(ps.pid).is_none() {
                    tally.vanished += 1;
                    continue;
                }
            }
        }
        // …except our own vendored `gilde-node`, which `externalBin` puts beside
        // the app's main executable. Protecting it left an orphaned sidecar
        // with no UI path to termination in the packaged build (PINNED §23-1);
        // it is ours, and the row model has to say so.
        if procscan::is_our_sidecar_path(&ps.args) {
            assert!(
                !row.has_tag(ProcTag::AppBundleMain),
                "our own sidecar is not a foreground app: {}",
                describe(&snap, ps.pid, Some(row))
            );
            continue;
        }
        checked += 1;
        if ps.args.contains(' ') {
            with_spaces += 1;
        }
        if !(row.has_tag(ProcTag::AppBundleMain) && row.is_protected()) {
            judge(&mut tally, &snap, ps.pid, row, "a bundle main by its command line is not protected");
        }
    }

    // The wider set: bundle mains that were started *with arguments*, so the
    // argv is not the bare path (`… /MacOS/Google Chrome Helper --type=renderer`).
    // The kernel's own answer is the authority here.
    let mut by_exe = 0usize;
    for ps in snap.rows() {
        let Some(exe) = procscan::exe_path_of(ps.pid) else {
            continue; // another uid, or already gone — protected either way
        };
        if !procscan::is_app_bundle_path(&exe) || procscan::is_our_sidecar_path(&exe) {
            continue;
        }
        by_exe += 1;
        let Some(row) = rows.iter().find(|r| r.pid == ps.pid) else {
            continue;
        };
        if !row.is_protected() {
            judge(&mut tally, &snap, ps.pid, row, &format!("runs {exe:?} and the monitor offers to kill it"));
        }
    }

    println!(
        "T07 bundle mains on this machine: {checked} rows ({with_spaces} with spaces in the path; \
         {argument_only} command lines only passed a bundle path as an argument)"
    );
    println!("T07 bundle mains by proc_pidpath: {by_exe} rows");
    println!(
        "T07 races resolved by a fresh look: {} protected now, {} exited or recycled",
        tally.raced.len(),
        tally.vanished
    );
    for detail in &tally.raced {
        println!("T07 race: {detail}");
    }
    if checked == 0 {
        // A headless box can genuinely have no bundle main running; say so
        // rather than failing, the same way the env-marker test does.
        println!("T07 bundle mains: SKIPPED — no .app bundle main was running");
    }
    assert!(
        tally.failures.is_empty(),
        "live .app bundle mains the kill path would NOT refuse:\n{}",
        tally.failures.join("\n")
    );
}

/// A4: the race `every_live_app_bundle_main_is_protected` must tolerate, made
/// deterministic with a process we start ourselves — and proof that tolerating
/// it is safe. A shell is scanned BEFORE it `exec`s into a `.app` main binary
/// (a copy of the fixture inside a temp bundle): the row built from that scan
/// is honestly unprotected (it was `/bin/sh`), the pid's live exe is a bundle
/// main a moment later, and the fresh look the kill path takes protects it.
#[test]
fn a_process_scanned_before_its_exec_into_a_bundle_main_is_protected_on_the_fresh_look() {
    use std::os::unix::process::CommandExt as _;

    let dir = tempfile::tempdir().expect("tempdir");
    let macos = dir.path().join("Fixture Probe.app/Contents/MacOS");
    std::fs::create_dir_all(&macos).expect("bundle dir");
    let main = macos.join("Fixture Probe");
    std::fs::copy(fixtures::fake_agent_path(), &main).expect("copy the fixture into the bundle");

    // The shell execs only once the test has taken its first look (an event,
    // not a 2 s head start a loaded machine can eat — A10.7).
    let go = dir.path().join("go");
    let script = format!(
        "while [ ! -e \"{}\" ]; do sleep 0.05; done; exec \"{}\" hang",
        go.display(),
        main.display()
    );
    let child = std::process::Command::new("/bin/sh")
        .args(["-c", &script])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .expect("spawn sh");
    let pid = child.id() as i32;
    struct Reap(std::process::Child);
    impl Drop for Reap {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    // First look: still the shell.
    let first = procscan::ps_snapshot().expect("ps snapshot");
    let registry = RegistryIndex::new("inst-current");
    let sigs = Signatures::defaults();
    let ctx = RowContext::new(&first, &registry, &sigs);
    let row = first.build_row(first.get(pid).expect("the shell is in the scan"), &ctx);
    let exe_then = procscan::exe_path_of(pid);
    let guard = Reap(child);
    assert!(
        exe_then.as_deref().is_some_and(|e| !procscan::is_app_bundle_path(e)),
        "the first look must catch the shell, not the bundle main: {exe_then:?}"
    );
    assert!(
        !row.has_tag(ProcTag::AppBundleMain),
        "a shell that only names a bundle path in its script is not a bundle main: {row:?}"
    );

    // Let it exec, and wait for that.
    std::fs::write(&go, b"").expect("release the shell");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !procscan::exe_path_of(pid).is_some_and(|e| procscan::is_app_bundle_path(&e)) {
        assert!(Instant::now() < deadline, "pid {pid} never exec'd into the bundle main");
        std::thread::sleep(Duration::from_millis(50));
    }

    match recheck(pid, &row, &first) {
        Recheck::ProtectedNow(detail) => println!("T07 deterministic race: {detail}"),
        Recheck::StillUnprotected(detail) => {
            panic!("after its exec the kill path would signal a bundle main: {detail}")
        }
        Recheck::Gone => panic!("pid {pid} vanished"),
        Recheck::Recycled => panic!("pid {pid}: exec must keep the kernel start time"),
    }
    drop(guard);
}

#[test]
fn ps_snapshot_round_trip_is_within_the_polling_budget() {
    // Warm up: the very first run pays for loading `ps` itself.
    let _ = procscan::ps_snapshot().expect("warm-up snapshot");

    let mut samples = Vec::new();
    let mut processes = 0usize;
    for _ in 0..5 {
        let started = Instant::now();
        let snap = procscan::ps_snapshot().expect("ps snapshot");
        samples.push(started.elapsed());
        processes = snap.len();
    }
    samples.sort();
    let median = samples[samples.len() / 2];
    let worst = *samples.last().expect("samples");

    println!(
        "T07 ps_snapshot: {processes} processes · median {median:?} · worst {worst:?} \
         · budget {PERF_BUDGET:?} (cwd excluded)"
    );
    if median > PERF_BUDGET {
        println!(
            "T07 ps_snapshot: OVER BUDGET — raise the Process Monitor poll interval \
             above the DESIGN §8-7 default of 3 s"
        );
    }
    assert!(
        median < PERF_HARD_CEILING,
        "ps snapshot median {median:?} exceeds the hard ceiling {PERF_HARD_CEILING:?}"
    );
}

#[test]
fn a_synthetic_snapshot_never_loops_on_a_cycle() {
    // Same property as the unit test, asserted here too because the tree walk is
    // what every kill path depends on.
    let rows: Vec<_> = (0..3)
        .map(|i| {
            let mut row = procscan::PsRow {
                pid: 900 + i,
                ppid: 900 + ((i + 2) % 3),
                pgid: 900,
                rss_kb: 0,
                cpu_pct: 0.0,
                elapsed_secs: 0,
                user: "tester".into(),
                args: "/bin/synthetic".into(),
            };
            row.args = format!("/bin/synthetic-{i}");
            row
        })
        .collect();
    let snap = Snapshot::from_rows(rows);

    let mut kids = snap.descendants_of(900);
    kids.sort_unstable();
    assert_eq!(kids, vec![901, 902]);
    assert_eq!(snap.ancestors_of(900).len(), 2);
}
