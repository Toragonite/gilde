//! Smoke tests for the `fake_agent` fixture (T06).
//!
//! These prove that every mode does the observable thing the rest of M1 relies
//! on. Two of them are load-bearing beyond the fixture itself:
//!
//!   * `escape_group_survives_killpg` demonstrates *why* T10 needs a descendant
//!     walk — `killpg` alone provably misses a `setsid()` escapee;
//!   * `binary_flood_is_byte_exact` pins the byte stream that T15/T16 use to
//!     prove the PTY path never mangles a multi-byte character on a 64 KiB
//!     read boundary.

mod fixtures;

use std::io::Write;
use std::time::Duration;

use fixtures::*;

const SHORT: Duration = Duration::from_millis(600);

// ---------------------------------------------------------------------------
// normal / usage
// ---------------------------------------------------------------------------

#[test]
fn normal_prints_and_exits_with_the_requested_code() {
    for code in [0, 1, 3, 42] {
        let out = run_normal(code);
        assert_eq!(out.status.code(), Some(code), "exit code for `normal {code}`");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            format!("fake_agent normal {code}")
        );
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("fake_agent stderr"),
            "stderr should carry a line too"
        );
    }
}

#[test]
fn unknown_mode_is_a_usage_error() {
    let out = run(&["not-a-mode"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown mode"));

    let out = run(&[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no mode given"));
}

// ---------------------------------------------------------------------------
// hang
// ---------------------------------------------------------------------------

#[test]
fn hang_lives_until_killed() {
    let mut agent = start_hang();
    assert!(agent.wait_timeout(SHORT).is_none(), "`hang` exited on its own");
    assert!(pid_alive(agent.pid()));

    agent.signal(libc::SIGKILL);
    let status = agent.expect_exit(EXIT_TIMEOUT);
    assert_eq!(exit_signal(&status), Some(libc::SIGKILL));
}

#[test]
fn spawned_fixtures_lead_their_own_process_group() {
    let agent = start_hang();
    assert_eq!(
        agent.pgid(),
        agent.pid(),
        "the fixture must be its own process-group leader, or killpg tests \
         would signal the test runner"
    );
}

// ---------------------------------------------------------------------------
// escape-group  (acceptance 2)
// ---------------------------------------------------------------------------

/// `killpg` on the fixture's process group kills the fixture and misses the
/// `setsid()` escapee. This is the empirical justification for T10's recursive
/// descendant walk: process-group signalling alone is not sufficient.
#[test]
fn escape_group_survives_killpg() {
    let mut escape = start_escape_group();
    let escaped = escape.escaped_pid;
    let pgid = escape.parent.pgid();
    assert_eq!(pgid, escape.parent.pid());

    // The escapee left our group: it leads a session of its own.
    assert_eq!(
        getpgid(escaped),
        Some(escaped),
        "setsid() should have made the escapee its own group leader"
    );
    assert_ne!(getpgid(escaped), Some(pgid));

    killpg(pgid, libc::SIGKILL);

    let status = escape.parent.expect_exit(EXIT_TIMEOUT);
    assert_eq!(exit_signal(&status), Some(libc::SIGKILL));

    // The whole point: it is still there.
    std::thread::sleep(SHORT);
    assert!(
        pid_alive(escaped),
        "killpg reached a process outside the group — the escape fixture is broken"
    );

    // ...and it dies when someone walks the tree and signals it directly.
    kill_pid(escaped, libc::SIGKILL);
    assert!(wait_until_gone(escaped, EXIT_TIMEOUT));
}

// ---------------------------------------------------------------------------
// spawn-grandchild  (acceptance 3)
// ---------------------------------------------------------------------------

#[test]
fn grandchild_outlives_its_parent() {
    let mut spawned = start_spawn_grandchild(30);
    let grandchild = spawned.grandchild_pid;
    assert!(pid_alive(grandchild));

    spawned.parent.signal(libc::SIGKILL);
    let status = spawned.parent.expect_exit(EXIT_TIMEOUT);
    assert_eq!(exit_signal(&status), Some(libc::SIGKILL));

    std::thread::sleep(SHORT);
    assert!(
        pid_alive(grandchild),
        "the grandchild should have been orphaned, not killed, by the parent's death"
    );

    kill_pid(grandchild, libc::SIGKILL);
    assert!(wait_until_gone(grandchild, EXIT_TIMEOUT));
}

// ---------------------------------------------------------------------------
// binary-flood  (acceptance 4)
// ---------------------------------------------------------------------------

#[test]
fn binary_flood_is_byte_exact() {
    const TOTAL: usize = 5_000_000;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("flood.bin");
    let status = run_binary_flood_to_file(TOTAL as u64, &path);
    assert!(status.success(), "binary-flood exited with {status:?}");

    let actual = std::fs::read(&path).expect("read flood file");
    let expected = expected_flood(TOTAL);

    assert_eq!(actual.len(), TOTAL, "flood length");
    if actual != expected {
        let at = actual
            .iter()
            .zip(expected.iter())
            .position(|(a, b)| a != b)
            .unwrap_or(0);
        panic!(
            "flood bytes diverge at offset {at}: got {:#04x}, expected {:#04x}",
            actual[at], expected[at]
        );
    }
}

/// The stream has to actually be hostile, or the fidelity test above proves
/// nothing. Assert the two properties DESIGN §7 cares about.
#[test]
fn binary_flood_contains_invalid_utf8_and_a_straddling_char() {
    let bytes = expected_flood(200_000);

    assert!(
        std::str::from_utf8(&bytes).is_err(),
        "flood must contain invalid UTF-8"
    );

    let boundary = flood::CHUNK_BOUNDARY as usize;
    for k in 1..=(bytes.len() / boundary) {
        let b = k * boundary;
        // The 4-byte U+1F600 starts one byte before the boundary, so exactly
        // one of its bytes lands in the previous 64 KiB chunk.
        assert_eq!(
            &bytes[b - 1..b + 3],
            &flood::STRADDLING_CHAR,
            "no straddling multi-byte character at 64 KiB boundary {b}"
        );
        assert!(
            std::str::from_utf8(&bytes[b - 1..b + 3]).is_ok(),
            "the straddling bytes must be a valid character when kept together"
        );
        assert!(
            std::str::from_utf8(&bytes[b..b + 3]).is_err(),
            "splitting at the boundary must break the character"
        );
    }
}

/// Chunked emission must not change a single byte — the producer streams in
/// 40 000-byte writes while the predictor materialises the whole buffer.
#[test]
fn flood_generation_is_chunk_independent() {
    let whole = flood::flood_bytes(300_000);
    let mut pieced = vec![0u8; whole.len()];
    let mut off = 0usize;
    for step in [1usize, 7, 4096, 65_536, 40_000, 99_991] {
        let n = step.min(whole.len() - off);
        flood::fill(off as u64, &mut pieced[off..off + n]);
        off += n;
        if off == whole.len() {
            break;
        }
    }
    flood::fill(off as u64, &mut pieced[off..]);
    assert_eq!(pieced, whole);
}

// ---------------------------------------------------------------------------
// ignore-term / slow-exit
// ---------------------------------------------------------------------------

#[test]
fn ignore_term_survives_sigterm_and_dies_on_sigkill() {
    let mut agent = start_ignore_term();

    agent.signal(libc::SIGTERM);
    assert!(
        agent.wait_timeout(SHORT).is_none(),
        "`ignore-term` exited on SIGTERM"
    );
    agent.signal(libc::SIGTERM);
    assert!(agent.wait_timeout(SHORT).is_none());

    agent.signal(libc::SIGKILL);
    let status = agent.expect_exit(EXIT_TIMEOUT);
    assert_eq!(exit_signal(&status), Some(libc::SIGKILL));
}

#[test]
fn slow_exit_waits_the_requested_delay_after_sigterm() {
    let mut agent = start_slow_exit(2);
    let started = std::time::Instant::now();

    agent.signal(libc::SIGTERM);
    agent.expect_stdout_line("term-received", READY_TIMEOUT);

    assert!(
        agent.wait_timeout(Duration::from_millis(800)).is_none(),
        "`slow-exit 2` exited immediately on SIGTERM"
    );
    let status = agent.expect_exit(Duration::from_secs(10));
    assert_eq!(status.code(), Some(0), "slow-exit should exit cleanly");
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "slow-exit returned too early: {:?}",
        started.elapsed()
    );
}

// ---------------------------------------------------------------------------
// print-env  (acceptance 5)
// ---------------------------------------------------------------------------

#[test]
fn print_env_echoes_the_marker_the_parent_planted() {
    let marker = "gilde-spawn-01JD5K8Q0R7C6M2ZP9V4XW3TYB";
    let out = run_print_env("GILDE_SPAWN_ID", marker);
    assert!(out.status.success(), "print-env exited with {:?}", out.status);
    assert_eq!(String::from_utf8_lossy(&out.stdout), format!("{marker}\n"));
}

#[test]
fn print_env_reports_a_missing_variable() {
    let mut cmd = command(&["print-env", "GILDE_DEFINITELY_NOT_SET_12345"]);
    cmd.env_remove("GILDE_DEFINITELY_NOT_SET_12345");
    let out = cmd.output().expect("run print-env");
    assert_eq!(out.status.code(), Some(3));
    assert!(out.stdout.is_empty());
}

// ---------------------------------------------------------------------------
// emit-jsonl
// ---------------------------------------------------------------------------

#[test]
fn emit_jsonl_replays_the_capture_byte_for_byte() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("capture.jsonl");

    // A capture that is deliberately awkward: no trailing newline on the last
    // line, an empty line in the middle, and a non-ASCII payload.
    let capture = concat!(
        "{\"type\":\"system\",\"subtype\":\"init\"}\n",
        "\n",
        "{\"type\":\"assistant\",\"text\":\"안녕 😀\"}\n",
        "{\"type\":\"result\",\"is_error\":false}"
    );
    let mut f = std::fs::File::create(&path).expect("create capture");
    f.write_all(capture.as_bytes()).expect("write capture");
    drop(f);

    let out = run_emit_jsonl(&path);
    assert!(out.status.success(), "emit-jsonl exited with {:?}", out.status);
    assert_eq!(out.stdout, capture.as_bytes());
}

// ---------------------------------------------------------------------------
// alloc
// ---------------------------------------------------------------------------

/// The kernel's own accounting of a process's memory: `ri_phys_footprint`
/// counts resident AND compressed pages, which is what "the process holds this
/// memory" means on macOS; `ps` RSS counts only the resident part.
fn phys_footprint_kib(pid: i32) -> Option<u64> {
    use libproc::libproc::pid_rusage::{pidrusage, RUsageInfoV2};
    pidrusage::<RUsageInfoV2>(pid).ok().map(|u| u.ri_phys_footprint / 1024)
}

/// A4 (load verdict: ENVIRONMENT, not product). This test used to read `ps`
/// RSS exactly once, right after `allocated`. On a machine deep in memory
/// pressure (measured: load 82, ~40 MB free) the compressor takes freshly
/// written pages before the first sample and `ps` said 38 496 KiB of 96 MiB —
/// while the fixture was holding every byte (its footprint said so) and its
/// 100 ms re-touch loop brought the pages back within ~1.5 s. One sample of a
/// quantity the kernel is actively moving is a race, not a measurement.
///
/// What it proves now, neither of it weaker than before:
///   * the allocation is HELD: the kernel's footprint (resident + compressed)
///     is at least the requested size, sampled within the deadline;
///   * the pages are made RESIDENT: `ps` RSS reaches the same half-size bar
///     as before at some sample within the deadline — the fixture's re-touch
///     is what un-compresses them, so this also proves the re-touch works;
///   * it stays alive for the hold and dies on SIGKILL.
#[test]
fn alloc_makes_the_pages_resident_and_holds_them() {
    const MIB: u64 = 96;
    const DEADLINE: Duration = Duration::from_secs(20);
    let mut agent = start_alloc(MIB, 60);
    let pid = agent.pid();

    let started = std::time::Instant::now();
    let mut held = false;
    let mut resident = false;
    let mut last = (0u64, 0u64);
    let mut samples = 0u32;
    while started.elapsed() < DEADLINE && !(held && resident) {
        let rss = rss_kib(pid).unwrap_or(0);
        let footprint = phys_footprint_kib(pid).unwrap_or(0);
        last = (rss, footprint);
        samples += 1;
        // 95 %: the footprint is the allocation plus the binary's own pages,
        // so anything materially below the request means it was not held.
        held |= footprint >= MIB * 1024 * 95 / 100;
        resident |= rss >= MIB * 1024 / 2;
        if !(held && resident) {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    assert!(
        held,
        "pid {pid} never held {MIB} MiB: footprint {} KiB, ps RSS {} KiB after {samples} samples",
        last.1, last.0
    );
    assert!(
        resident,
        "pid {pid} held {MIB} MiB (footprint {} KiB) but ps never showed half of it resident \
         within {DEADLINE:?} — the re-touch loop is not bringing pages back (last RSS {} KiB, \
         {samples} samples)",
        last.1, last.0
    );

    assert!(agent.wait_timeout(SHORT).is_none(), "`alloc` exited early");
    agent.signal(libc::SIGKILL);
    let status = agent.expect_exit(EXIT_TIMEOUT);
    assert_eq!(exit_signal(&status), Some(libc::SIGKILL));
}

#[test]
fn alloc_exits_on_its_own_after_the_hold() {
    let mut agent = start_alloc(8, 1);
    let status = agent.expect_exit(Duration::from_secs(15));
    assert_eq!(status.code(), Some(0));
}

// ---------------------------------------------------------------------------
// acceptance 6 — nothing survives SIGKILL, nothing is left behind
// ---------------------------------------------------------------------------

#[test]
fn every_mode_dies_on_sigkill_and_leaves_nothing_behind() {
    let mut leaked: Vec<(String, i32)> = Vec::new();

    for args in ALL_MODES {
        let mut agent = Running::start(args);
        let pid = agent.pid();

        // Modes that fork announce the pid they forked; register it so the
        // drop guard reaps it and so this test can prove it really went away.
        match args[0] {
            "escape-group" => {
                let escaped = agent.expect_pid_line("escaped-pid ", READY_TIMEOUT);
                agent.track(escaped);
            }
            "spawn-grandchild" => {
                let grandchild = agent.expect_pid_line("grandchild-pid ", READY_TIMEOUT);
                agent.track(grandchild);
            }
            _ => {}
        }

        // Give short-lived modes (`normal`, `print-env`) a moment to finish on
        // their own; the long-lived ones are still running here.
        std::thread::sleep(Duration::from_millis(120));

        agent.signal(libc::SIGKILL);
        let status = agent.expect_exit(EXIT_TIMEOUT);
        assert!(
            status.code().is_some() || exit_signal(&status) == Some(libc::SIGKILL),
            "mode {args:?} ended in an unexpected way: {status:?}"
        );
        // Reaped by expect_exit: the pid is no longer a zombie of ours.
        if !wait_until_gone(pid, EXIT_TIMEOUT) {
            leaked.push((format!("{args:?}"), pid));
        }

        // Descendants the fixture told us about must be reapable too. Drop
        // handles the kill; verify it actually took effect.
        let descendants: Vec<i32> = agent.tracked().to_vec();
        drop(agent);
        for pid in descendants {
            if !wait_until_gone(pid, EXIT_TIMEOUT) {
                leaked.push((format!("{args:?} descendant"), pid));
            }
        }
    }

    assert!(leaked.is_empty(), "processes survived the test: {leaked:?}");
}
