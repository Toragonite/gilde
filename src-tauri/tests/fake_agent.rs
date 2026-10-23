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
//!
//! The `tui` module below drives the screen and team modes (`claude-tui`,
//! `tui-idle-ready`, `term-query`, `teammate`, `teammate-lead`) over a real
//! PTY and pins their screens row by row: those screens are the interface
//! Gilde's grid emulator, prompt grammar and ready signatures are built on.

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

// ---------------------------------------------------------------------------
// screen and team modes over a real PTY
// ---------------------------------------------------------------------------

mod tui {
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::unix::process::CommandExt;
    use std::path::PathBuf;
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
    use std::time::{Duration, Instant};

    use nix::sys::termios::{cfmakeraw, tcgetattr, tcsetattr, SetArg};
    use serde_json::Value;

    use super::fixtures::{exit_signal, fake_agent_path, Hermetic};

    /// Ceiling for "the screen reacted to that key" — far above the tens of
    /// milliseconds it takes; nothing here asserts speed (A10.7).
    const UI: Duration = Duration::from_secs(30);
    /// Ceiling for "the process exited".
    const EXIT: Duration = Duration::from_secs(30);
    const ROWS: u16 = 40;
    const COLS: u16 = 120;
    /// Row numbers (1-based) of the 40-row screens.
    const R: usize = ROWS as usize;

    // -----------------------------------------------------------------------
    // a small terminal grid (vte), with xterm's pending-wrap rule or without
    // -----------------------------------------------------------------------

    /// Just enough of a VT to place the fixture's frames: CUP/CUU/CUD/CUF/
    /// CUB/CHA, ED, EL, CR/LF with scrolling, `?1049` h/l. `xterm_wrap`
    /// selects xterm's pending-wrap rule (printing in the last column leaves
    /// the cursor there until the next printable); without it the cursor
    /// wraps eagerly, which is how a naive emulator misplaces rows.
    struct Grid {
        rows: usize,
        cols: usize,
        main: Vec<Vec<char>>,
        alt: Vec<Vec<char>>,
        alt_on: bool,
        row: usize,
        col: usize,
        wrap_pending: bool,
        xterm_wrap: bool,
        saved: (usize, usize),
    }

    impl Grid {
        fn new(rows: u16, cols: u16, xterm_wrap: bool) -> Grid {
            let (rows, cols) = (usize::from(rows), usize::from(cols));
            Grid {
                rows,
                cols,
                main: vec![vec![' '; cols]; rows],
                alt: vec![vec![' '; cols]; rows],
                alt_on: false,
                row: 0,
                col: 0,
                wrap_pending: false,
                xterm_wrap,
                saved: (0, 0),
            }
        }

        fn buf(&mut self) -> &mut Vec<Vec<char>> {
            if self.alt_on {
                &mut self.alt
            } else {
                &mut self.main
            }
        }

        fn line_feed(&mut self) {
            if self.row + 1 >= self.rows {
                let cols = self.cols;
                let buf = self.buf();
                buf.remove(0);
                buf.push(vec![' '; cols]);
            } else {
                self.row += 1;
            }
        }

        /// Row `r` (1-based), trailing blanks trimmed.
        fn row_text(&self, r: usize) -> String {
            let buf = if self.alt_on { &self.alt } else { &self.main };
            buf[r - 1].iter().collect::<String>().trim_end().to_string()
        }
    }

    impl vte::Perform for Grid {
        fn print(&mut self, c: char) {
            if self.xterm_wrap && self.wrap_pending {
                self.col = 0;
                self.line_feed();
                self.wrap_pending = false;
            }
            let (r, col) = (self.row, self.col);
            self.buf()[r][col] = c;
            if self.col + 1 < self.cols {
                self.col += 1;
            } else if self.xterm_wrap {
                self.wrap_pending = true;
            } else {
                self.col = 0;
                self.line_feed();
            }
        }

        fn execute(&mut self, byte: u8) {
            match byte {
                b'\r' => self.col = 0,
                b'\n' | 0x0b | 0x0c => self.line_feed(),
                0x08 => self.col = self.col.saturating_sub(1),
                _ => return,
            }
            self.wrap_pending = false;
        }

        fn csi_dispatch(&mut self, params: &vte::Params, inter: &[u8], _ignore: bool, action: char) {
            let p: Vec<usize> =
                params.iter().map(|s| usize::from(s.first().copied().unwrap_or(0))).collect();
            let arg = |i: usize| p.get(i).copied().filter(|v| *v != 0).unwrap_or(1);
            let (rows, cols) = (self.rows, self.cols);
            if inter.first() == Some(&b'?') {
                if p.contains(&1049) {
                    match action {
                        'h' => {
                            self.saved = (self.row, self.col);
                            self.alt = vec![vec![' '; cols]; rows];
                            self.alt_on = true;
                            (self.row, self.col) = (0, 0);
                        }
                        'l' => {
                            self.alt_on = false;
                            (self.row, self.col) = self.saved;
                        }
                        _ => {}
                    }
                }
                return;
            }
            match action {
                'H' | 'f' => {
                    self.row = (arg(0) - 1).min(rows - 1);
                    self.col = (arg(1) - 1).min(cols - 1);
                }
                'A' => self.row = self.row.saturating_sub(arg(0)),
                'B' => self.row = (self.row + arg(0)).min(rows - 1),
                'C' => self.col = (self.col + arg(0)).min(cols - 1),
                'D' => self.col = self.col.saturating_sub(arg(0)),
                'G' => self.col = (arg(0) - 1).min(cols - 1),
                'J' => {
                    let (r, c) = (self.row, self.col);
                    let mode = p.first().copied().unwrap_or(0);
                    let buf = self.buf();
                    for (i, line) in buf.iter_mut().enumerate() {
                        for (j, cell) in line.iter_mut().enumerate() {
                            if mode >= 2 || (mode == 0 && (i > r || (i == r && j >= c))) {
                                *cell = ' ';
                            }
                        }
                    }
                    return;
                }
                'K' => {
                    let (r, c) = (self.row, self.col);
                    let mode = p.first().copied().unwrap_or(0);
                    for (j, cell) in self.buf()[r].iter_mut().enumerate() {
                        if mode == 2 || (mode == 0 && j >= c) || (mode == 1 && j <= c) {
                            *cell = ' ';
                        }
                    }
                    return;
                }
                _ => return,
            }
            self.wrap_pending = false;
        }
    }

    // -----------------------------------------------------------------------
    // the PTY harness
    // -----------------------------------------------------------------------

    /// `fake_agent` on the slave side of a fresh PTY (its own session, the
    /// PTY as controlling terminal, so resizes deliver `SIGWINCH`). Output is
    /// read on a thread and fed into two grids (xterm wrap and eager wrap).
    /// SIGKILL + reap on drop.
    struct Pty {
        child: Child,
        master: File,
        rx: Receiver<Vec<u8>>,
        out: Vec<u8>,
        parser: vte::Parser,
        grid: Grid,
        eager_parser: vte::Parser,
        eager: Grid,
        closed: bool,
        status: Option<ExitStatus>,
    }

    impl Pty {
        /// Spawn `cmd` on a `ROWS`×`COLS` PTY. `early` is written into the
        /// PTY **before** the child exists, so it is provably queued before
        /// the child's first read.
        fn spawn(mut cmd: Command, early: &[u8]) -> Pty {
            let ws = libc::winsize { ws_row: ROWS, ws_col: COLS, ws_xpixel: 0, ws_ypixel: 0 };
            let pty = nix::pty::openpty(&ws, None).expect("openpty");
            // A raw line discipline from the start: early bytes are neither
            // echoed nor line-edited before the child takes over.
            let mut t = tcgetattr(&pty.slave).expect("tcgetattr");
            cfmakeraw(&mut t);
            tcsetattr(&pty.slave, SetArg::TCSANOW, &t).expect("tcsetattr");
            let mut master = File::from(pty.master);
            if !early.is_empty() {
                master.write_all(early).expect("write early input");
            }
            let slave = pty.slave;
            cmd.stdin(Stdio::from(slave.try_clone().expect("dup slave")))
                .stdout(Stdio::from(slave.try_clone().expect("dup slave")))
                .stderr(Stdio::from(slave));
            // SAFETY: between fork and exec; setsid and ioctl are
            // async-signal-safe and nothing is allocated.
            unsafe {
                cmd.pre_exec(|| {
                    if libc::setsid() == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    libc::ioctl(0, libc::TIOCSCTTY as _, 0);
                    Ok(())
                });
            }
            let child = cmd.spawn().expect("spawn fake_agent on a pty");
            // Our copies of the slave close with `cmd`, so the reader sees EOF
            // once the child is gone.
            drop(cmd);
            let mut reader = master.try_clone().expect("clone master");
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let mut buf = [0u8; 65536];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            if tx.send(buf[..n].to_vec()).is_err() {
                                return;
                            }
                        }
                    }
                }
            });
            Pty {
                child,
                master,
                rx,
                out: Vec::new(),
                parser: vte::Parser::new(),
                grid: Grid::new(ROWS, COLS, true),
                eager_parser: vte::Parser::new(),
                eager: Grid::new(ROWS, COLS, false),
                closed: false,
                status: None,
            }
        }

        fn pid(&self) -> i32 {
            self.child.id() as i32
        }

        /// Take one chunk of output (if any arrives within `timeout`).
        fn pump(&mut self, timeout: Duration) -> bool {
            match self.rx.recv_timeout(timeout) {
                Ok(chunk) => {
                    self.parser.advance(&mut self.grid, &chunk);
                    self.eager_parser.advance(&mut self.eager, &chunk);
                    self.out.extend_from_slice(&chunk);
                    true
                }
                Err(RecvTimeoutError::Timeout) => false,
                Err(RecvTimeoutError::Disconnected) => {
                    self.closed = true;
                    false
                }
            }
        }

        /// Pump output until `cond` holds; panic with the screen after `ceiling`.
        fn wait(&mut self, what: &str, ceiling: Duration, cond: impl Fn(&Pty) -> bool) {
            let deadline = Instant::now() + ceiling;
            loop {
                if cond(self) {
                    return;
                }
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() || (self.closed && !self.pump(Duration::ZERO)) {
                    if cond(self) {
                        return;
                    }
                    panic!(
                        "{what}: not seen within {ceiling:?} (output closed: {})\n{}",
                        self.closed,
                        self.dump()
                    );
                }
                self.pump(left.min(Duration::from_millis(200)));
            }
        }

        /// Wait until the output contains `needle`.
        fn wait_text(&mut self, needle: &str) {
            let n = needle.as_bytes().to_vec();
            self.wait(&format!("output {needle:?}"), UI, move |p| contains(&p.out, &n));
        }

        /// Wait until row `r` of the (xterm) screen reads exactly `text`.
        fn wait_row(&mut self, r: usize, text: &str) {
            let t = text.to_string();
            self.wait(&format!("row {r} == {text:?}"), UI, move |p| p.grid.row_text(r) == t);
        }

        /// Wait until every `(row, text)` pair holds **in the same state** of
        /// the screen. Frames start with `ESC[2J`, so including the frame's
        /// last row means the whole frame has arrived.
        fn wait_rows(&mut self, rows: &[(usize, &str)]) {
            let want: Vec<(usize, String)> = rows.iter().map(|(r, t)| (*r, t.to_string())).collect();
            self.wait(&format!("rows {want:?}"), UI, move |p| {
                want.iter().all(|(r, t)| p.grid.row_text(*r) == *t)
            });
        }

        /// Wait until some row of the screen reads exactly `text`.
        fn wait_any_row(&mut self, text: &str) {
            let t = text.to_string();
            self.wait(&format!("a row == {text:?}"), UI, move |p| p.find_row(&t).is_some());
        }

        fn find_row(&self, text: &str) -> Option<usize> {
            (1..=R).find(|r| self.grid.row_text(*r) == text)
        }

        fn row(&self, r: usize) -> String {
            self.grid.row_text(r)
        }

        fn send(&mut self, bytes: &[u8]) {
            self.master.write_all(bytes).expect("write to pty");
            self.master.flush().expect("flush pty");
        }

        fn mark(&self) -> usize {
            self.out.len()
        }

        /// Collect whatever arrives during `d` and return it (for "nothing
        /// was drawn" checks).
        fn quiet_for(&mut self, d: Duration) -> Vec<u8> {
            let from = self.out.len();
            let end = Instant::now() + d;
            while Instant::now() < end {
                self.pump(end.saturating_duration_since(Instant::now()));
            }
            self.out[from..].to_vec()
        }

        fn resize(&mut self, rows: u16, cols: u16) {
            let ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
            // SAFETY: TIOCSWINSZ on our own master fd with a winsize we own.
            let rc = unsafe { libc::ioctl(std::os::fd::AsRawFd::as_raw_fd(&self.master), libc::TIOCSWINSZ, &ws) };
            assert_eq!(rc, 0, "TIOCSWINSZ");
            self.parser = vte::Parser::new();
            self.grid = Grid::new(rows, cols, true);
            self.eager_parser = vte::Parser::new();
            self.eager = Grid::new(rows, cols, false);
        }

        fn wait_exit(&mut self, ceiling: Duration) -> ExitStatus {
            let deadline = Instant::now() + ceiling;
            loop {
                if let Some(s) = self.child.try_wait().expect("try_wait") {
                    self.status = Some(s);
                    // Drain what is left so callers can inspect the tail.
                    while self.pump(Duration::from_millis(50)) {}
                    return s;
                }
                assert!(
                    Instant::now() < deadline,
                    "fake_agent did not exit within {ceiling:?}\n{}",
                    self.dump()
                );
                self.pump(Duration::from_millis(20));
            }
        }

        fn screen(&self) -> String {
            (1..=self.grid.rows)
                .map(|r| format!("{r:02}|{}", self.grid.row_text(r)))
                .collect::<Vec<_>>()
                .join("\n")
        }

        fn dump(&self) -> String {
            let tail = &self.out[self.out.len().saturating_sub(600)..];
            format!(
                "--- screen ---\n{}\n--- raw tail ---\n{}",
                self.screen(),
                String::from_utf8_lossy(tail).escape_debug()
            )
        }
    }

    impl Drop for Pty {
        fn drop(&mut self) {
            // The child leads its own session and process group (setsid), so
            // the group holds only what it started — e.g. the teammate a
            // `/bin/sh -c` pane command forked.
            let pid = self.child.id() as i32;
            // SAFETY: signals to the group of the child this harness spawned.
            unsafe {
                libc::killpg(pid, libc::SIGKILL);
            }
            if self.status.is_none() {
                // SAFETY: as above, the child itself.
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
                let _ = self.child.wait();
            }
        }
    }

    fn contains(hay: &[u8], needle: &[u8]) -> bool {
        !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
    }

    fn count(hay: &[u8], needle: &[u8]) -> usize {
        hay.windows(needle.len()).filter(|w| *w == needle).count()
    }

    /// The hermetic world of one test: `HOME`, a Claude config dir, a project
    /// dir (the cwd), the input log, and a `PATH` holding only fixtures.
    struct World {
        h: Hermetic,
    }

    impl World {
        fn new() -> World {
            let h = Hermetic::new();
            for d in ["cfg", "proj"] {
                std::fs::create_dir_all(h.root().join(d)).expect("world dirs");
            }
            World { h }
        }

        fn cfg(&self) -> PathBuf {
            self.h.root().join("cfg")
        }

        /// The project dir, canonical (macOS temp dirs live under /private).
        fn proj(&self) -> PathBuf {
            std::fs::canonicalize(self.h.root().join("proj")).expect("canonical proj")
        }

        fn input_log(&self) -> PathBuf {
            self.h.root().join("input.log")
        }

        fn input(&self) -> Vec<u8> {
            std::fs::read(self.input_log()).unwrap_or_default()
        }

        fn cmd(&self, args: &[&str]) -> Command {
            let mut cmd = Command::new(fake_agent_path());
            cmd.args(args);
            self.h.apply(&mut cmd);
            cmd.env("CLAUDE_CONFIG_DIR", self.cfg())
                .env("TERM", "xterm-256color")
                .env("FAKE_AGENT_INPUT_LOG", self.input_log())
                .current_dir(self.proj());
            cmd
        }

        fn spawn(&self, args: &[&str]) -> Pty {
            Pty::spawn(self.cmd(args), b"")
        }
    }

    fn rule() -> String {
        "─".repeat(usize::from(COLS))
    }

    /// The idle main screen of the claude style, rows 1–3 and 36–40.
    fn assert_claude_idle(p: &Pty, composer: &str) {
        assert_eq!(p.row(1), " ▐▛███▜▌   Claude Code (fake)", "{}", p.dump());
        assert_eq!(p.row(2), "▝▜█████▛▘  Fake model · Test plan");
        assert_eq!(p.row(3), "  ▘▘ ▝▝    /tmp/fake-project");
        assert_eq!(p.row(R - 4), rule());
        assert_eq!(p.row(R - 3), format!("❯ {composer}"), "{}", p.dump());
        assert_eq!(p.row(R - 2), rule());
        assert_eq!(p.row(R - 1), "  ⏸ manual mode on · ? for shortcuts");
        assert_eq!(p.row(R), "");
    }

    const PLACEHOLDER: &str = "Try \"how does <filepath> work?\"";
    const FOOTER: &str = "  ⏸ manual mode on · ? for shortcuts";

    /// Wait for a complete idle frame showing `composer`, then check it.
    fn wait_idle(p: &mut Pty, composer: &str) {
        p.wait_rows(&[(R - 3, &format!("❯ {composer}")), (R - 1, FOOTER)]);
        assert_claude_idle(p, composer);
    }
    const INTERRUPTED: &str = "  ⎿  Interrupted · What should Claude do instead?";

    // -----------------------------------------------------------------------
    // claude-tui: the idle screen and the grid features it exercises
    // -----------------------------------------------------------------------

    #[test]
    fn claude_idle_screen_is_drawn_exactly_and_needs_pending_wrap() {
        let w = World::new();
        let mut p = w.spawn(&["claude-tui", "idle"]);
        p.wait_row(R - 1, "  ⏸ manual mode on · ? for shortcuts");
        assert_claude_idle(&p, PLACEHOLDER);

        // Alternate screen with every mode R6 §4 lists, and the title.
        assert!(p.out.starts_with(
            b"\x1b[?1049h\x1b[?2004h\x1b[?1004h\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h\x1b[?2031h"
        ));
        assert!(contains(&p.out, "\x1b]0;✳ Claude Code\x07".as_bytes()));
        // The rule rows: C columns, then `\r\n`, then the next row with no
        // cursor positioning (pending wrap), never `ESC[K` after a rule.
        let rule_then_composer = format!("\x1b[{};1H{}\r\n❯ {PLACEHOLDER}\x1b[K", R - 4, rule());
        assert!(contains(&p.out, rule_then_composer.as_bytes()), "{}", p.dump());
        let rule_then_footer =
            format!("\x1b[{};1H{}\r\n\x1b[2C⏸ manual mode on · ? for shortcuts\x1b[K", R - 2, rule());
        assert!(contains(&p.out, rule_then_footer.as_bytes()), "{}", p.dump());
        // Indentation by cursor-forward, a relative move to row 2.
        assert!(contains(&p.out, "\x1b[1;1H\x1b[1C▐▛███▜▌".as_bytes()));
        assert!(contains(&p.out, "\x1b[1B\x1b[1G▝▜█████▛▘".as_bytes()));

        // The proof that the frame needs xterm's pending-wrap rule: an
        // emulator that wraps eagerly puts each row after a rule one row too
        // low — the composer lands on R-2 (then the second rule overwrites
        // it) and the footer on R.
        assert_eq!(p.eager.row_text(R - 3), "");
        assert_eq!(p.eager.row_text(R - 2), rule());
        assert_eq!(p.eager.row_text(R - 1), "");
        assert_eq!(p.eager.row_text(R), "  ⏸ manual mode on · ? for shortcuts");
    }

    #[test]
    fn claude_submit_works_with_a_spinner_then_settles() {
        let w = World::new();
        let mut p = w.spawn(&["claude-tui", "idle", "--work-ms", "600"]);
        p.wait_row(R - 1, "  ⏸ manual mode on · ? for shortcuts");
        p.send(b"hello");
        p.wait_row(R - 3, "❯ hello");
        p.send(b"\r");
        p.wait_row(5, "⏺ got: hello");
        p.wait("the spinner row", UI, |p| {
            let s = p.row(R - 6);
            s.ends_with("s · esc to interrupt)") && s.contains(" Working… (")
        });
        let spin = p.row(R - 6).chars().next().expect("spinner char");
        assert!("✢✳✶✻✽".contains(spin), "spinner {spin:?}");
        p.wait("the spinner row to go", UI, |p| p.row(R - 6).is_empty() && p.row(R - 1) == FOOTER);
        assert_claude_idle(&p, PLACEHOLDER);
        assert_eq!(p.row(5), "⏺ got: hello");
        // Settled: no further output until input.
        let after = p.quiet_for(Duration::from_millis(500));
        assert!(after.is_empty(), "idle screen kept drawing: {:?}", String::from_utf8_lossy(&after));
    }

    #[test]
    fn claude_tui_redraws_on_resize() {
        let w = World::new();
        let mut p = w.spawn(&["claude-tui", "idle"]);
        p.wait_row(R - 1, "  ⏸ manual mode on · ? for shortcuts");
        p.resize(30, 100);
        p.wait("the 30-row footer", UI, |p| p.grid.row_text(29) == "  ⏸ manual mode on · ? for shortcuts");
        assert_eq!(p.grid.row_text(26), "─".repeat(100));
        assert_eq!(p.grid.row_text(27), format!("❯ {PLACEHOLDER}"));
    }

    #[test]
    fn input_log_holds_every_raw_byte_and_paste_is_not_a_submit() {
        let w = World::new();
        let mut p = w.spawn(&["claude-tui", "idle"]);
        p.wait_row(R - 1, "  ⏸ manual mode on · ? for shortcuts");
        p.send(b"ab");
        p.wait_row(R - 3, "❯ ab");
        p.send(b"\x1b[A"); // Up: ignored on the idle screen
        p.send(b"\x1b[200~XY\r\nZ\x1b[201~");
        // `\r\n` inside a paste is one line break → one space; no submit.
        p.wait_row(R - 3, "❯ abXY Z");
        assert!(p.find_row("⏺ got: abXY Z").is_none(), "a paste submitted by itself");
        p.send(b"\x7f\r");
        p.wait_row(5, "⏺ got: abXY");
        let expected: &[u8] = b"ab\x1b[A\x1b[200~XY\r\nZ\x1b[201~\x7f\r";
        assert_eq!(w.input(), expected, "input log");
    }

    // -----------------------------------------------------------------------
    // claude-tui: the permission prompt
    // -----------------------------------------------------------------------

    /// The permission block at 120×40: rows 26–39.
    fn assert_permission(p: &Pty, hl: usize) {
        let m = |n: usize| if n == hl { " ❯ " } else { "   " };
        assert_eq!(p.row(26), rule(), "{}", p.dump());
        assert_eq!(p.row(27), " Bash command");
        assert_eq!(p.row(28), " Create empty file demo.txt");
        assert_eq!(p.row(29), "╌".repeat(120));
        assert_eq!(p.row(30), " touch demo.txt");
        assert_eq!(p.row(31), "╌".repeat(120));
        assert_eq!(p.row(32), " Do you want to proceed?");
        assert_eq!(p.row(33), format!("{}1. Yes", m(1)));
        let opt2 = p.row(34);
        assert!(opt2.starts_with(&format!("{}2. Yes, and always allow access to /tmp/fake-project/nested/", m(2))));
        assert!(opt2.ends_with(" from"), "{opt2:?}");
        let len = opt2.chars().count();
        assert!(len > 120 - 6 && len <= 120, "option 2 must end in the last 6 columns: {len}");
        assert!(len + " this".len() > 120, "\"this\" must not fit on the first row: {len}");
        assert_eq!(p.row(35), "      this project");
        assert_eq!(
            p.row(36),
            format!("{}3. Yes, and switch to auto mode · auto mode handles these prompts for you", m(3))
        );
        assert_eq!(p.row(37), format!("{}4. No", m(4)));
        assert_eq!(p.row(38), "");
        assert_eq!(p.row(39), " Esc to cancel · Tab to amend");
        assert_eq!(p.row(40), "");
    }

    #[test]
    fn permission_prompt_is_drawn_exactly_and_a_digit_selects_without_enter() {
        let w = World::new();
        let mut p = w.spawn(&["claude-tui", "permission"]);
        p.wait_row(39, " Esc to cancel · Tab to amend");
        assert_permission(&p, 1);
        // The header stays, the composer and footer are hidden.
        assert_eq!(p.row(1), " ▐▛███▜▌   Claude Code (fake)");
        assert!(p.find_row(&format!("❯ {PLACEHOLDER}")).is_none());

        p.send(b"1");
        p.wait_any_row("⏺ selected 1");
        p.wait_row(R - 3, &format!("❯ {PLACEHOLDER}"));
        assert_eq!(w.input(), b"1", "the digit alone, no CR");
    }

    #[test]
    fn permission_esc_and_no_both_interrupt() {
        let w = World::new();
        let mut p = w.spawn(&["claude-tui", "permission"]);
        p.wait_row(39, " Esc to cancel · Tab to amend");
        p.send(b"\x1b");
        p.wait_any_row(INTERRUPTED);
        wait_idle(&mut p, PLACEHOLDER);

        // Bring it back from the composer, answer "No" this time.
        p.send(b"needs permission\r");
        p.wait_row(39, " Esc to cancel · Tab to amend");
        assert_permission(&p, 1);
        p.send(b"4");
        p.wait("a second Interrupted line", UI, |p| {
            (1..=R).filter(|r| p.row(*r) == INTERRUPTED).count() == 2
        });
        assert!(p.find_row("⏺ selected 4").is_none());
    }

    #[test]
    fn permission_arrows_move_the_highlight_and_enter_selects_it() {
        let w = World::new();
        let mut p = w.spawn(&["claude-tui", "permission"]);
        p.wait_row(39, " Esc to cancel · Tab to amend");
        p.send(b"\x1b[B");
        p.wait("highlight on 2", UI, |p| {
            p.row(34).starts_with(" ❯ 2.") && p.row(39) == " Esc to cancel · Tab to amend"
        });
        assert_permission(&p, 2);
        p.send(b"\x1bOB"); // SS3 form
        p.wait_row(36, " ❯ 3. Yes, and switch to auto mode · auto mode handles these prompts for you");
        p.send(b"\x1b[B\x1b[B"); // clamped at 4
        p.wait_row(37, " ❯ 4. No");
        p.send(b"\x1b[A");
        p.wait_row(36, " ❯ 3. Yes, and switch to auto mode · auto mode handles these prompts for you");
        p.send(b"\r");
        p.wait_any_row("⏺ selected 3");
    }

    #[test]
    fn permission_tab_amends_and_enter_approves_with_the_note() {
        let w = World::new();
        let mut p = w.spawn(&["claude-tui", "permission"]);
        p.wait_row(39, " Esc to cancel · Tab to amend");
        p.send(b"\t");
        p.wait_row(39, " Esc to cancel");
        assert_eq!(p.row(33), " ❯ 1. Yes, and tell Claude what to do next");
        p.send(b"name it b.txt");
        p.wait_row(33, " ❯ 1. Yes, name it b.txt");
        p.send(b"\x1b[200~ please\x1b[201~");
        p.wait_row(33, " ❯ 1. Yes, name it b.txt please");
        p.send(b"\r");
        p.wait_any_row("⏺ approved; note: name it b.txt please");
        p.wait_row(R - 3, &format!("❯ {PLACEHOLDER}"));
    }

    #[test]
    fn permission_esc_while_amending_interrupts() {
        let w = World::new();
        let mut p = w.spawn(&["claude-tui", "permission"]);
        p.wait_row(39, " Esc to cancel · Tab to amend");
        p.send(b"\t");
        p.wait_row(39, " Esc to cancel");
        p.send(b"x");
        p.wait_row(33, " ❯ 1. Yes, x");
        p.send(b"\x1b");
        p.wait_any_row(INTERRUPTED);
        assert!(p.find_row("⏺ approved; note: x").is_none());
    }

    // -----------------------------------------------------------------------
    // claude-tui: choice and worker prompts
    // -----------------------------------------------------------------------

    #[test]
    fn choice_prompt_is_drawn_exactly_and_answers() {
        let w = World::new();
        let mut p = w.spawn(&["claude-tui", "choice"]);
        p.wait_row(39, "Enter to select · ↑/↓ to navigate · Esc to cancel");
        let expected = [
            rule(),
            " ☐ Colour".into(),
            "Which colour do you prefer?".into(),
            "❯ 1. Red".into(),
            "     Choose red".into(),
            "  2. Green".into(),
            "     Choose green".into(),
            "  3. Blue".into(),
            "     Choose blue".into(),
            "  4. Type something.".into(),
            rule(),
            "  5. Chat about this".into(),
            "Enter to select · ↑/↓ to navigate · Esc to cancel".into(),
        ];
        for (i, want) in expected.iter().enumerate() {
            assert_eq!(&p.row(27 + i), want, "row {}\n{}", 27 + i, p.dump());
        }
        // The divider before "Chat about this" is a pending-wrap row too.
        assert!(contains(&p.out, format!("{}\r\n\x1b[2C5. Chat about this\x1b[K", rule()).as_bytes()));

        p.send(b"\x1b[B\x1b[B\x1b[B\x1b[B\x1b[B");
        p.wait_row(38, "❯ 5. Chat about this");
        p.send(b"\x1b[A\x1b[A");
        p.wait_row(34, "❯ 3. Blue");
        p.send(b"3");
        p.wait_any_row("⏺ answered Blue");
        p.wait_row(R - 3, &format!("❯ {PLACEHOLDER}"));

        // 4 → typing (no work), 5 → chat, Esc → interrupted.
        for (keys, line) in [(&b"4"[..], "⏺ typing"), (b"5", "⏺ chat"), (b"\x1b", INTERRUPTED)] {
            p.send(b"choice again\r");
            p.wait_row(39, "Enter to select · ↑/↓ to navigate · Esc to cancel");
            p.send(keys);
            p.wait_any_row(line);
            p.wait_row(R - 3, &format!("❯ {PLACEHOLDER}"));
        }
    }

    #[test]
    fn worker_prompt_is_drawn_exactly_and_answers() {
        let w = World::new();
        let mut p = w.spawn(&["claude-tui", "worker"]);
        p.wait_row(39, " Esc to cancel");
        assert_eq!(p.row(34), " Do you want to proceed?");
        assert_eq!(p.row(35), " ❯ 1. Yes");
        assert_eq!(p.row(36), "   2. Yes, and switch to auto mode");
        assert_eq!(p.row(37), "   3. No");
        assert_eq!(p.row(38), "");
        p.send(b"\x1b[B\r");
        p.wait_any_row("⏺ selected 2");
        p.wait_row(R - 3, &format!("❯ {PLACEHOLDER}"));
        p.send(b"worker\r");
        p.wait_row(35, " ❯ 1. Yes");
        p.send(b"3");
        p.wait_any_row(INTERRUPTED);
    }

    // -----------------------------------------------------------------------
    // claude-tui: setup screens (unnumbered)
    // -----------------------------------------------------------------------

    fn assert_trust(p: &Pty, yes: bool) {
        assert_eq!(p.row(29), rule(), "{}", p.dump());
        assert_eq!(p.row(30), " Accessing workspace:");
        assert_eq!(p.row(31), "");
        assert_eq!(p.row(32), " /tmp/fake-project");
        assert_eq!(p.row(34), " Quick safety check: Is this a project you created or one you trust?");
        let (no, yes_row) = if yes {
            ("   No, exit", " ❯ Yes, I trust this folder")
        } else {
            (" ❯ No, exit", "   Yes, I trust this folder")
        };
        assert_eq!(p.row(36), no);
        assert_eq!(p.row(37), yes_row);
        assert_eq!(p.row(39), " Enter to confirm · Esc to cancel");
    }

    #[test]
    fn trust_ignores_digits_moves_on_arrows_and_exits_1_on_no() {
        let w = World::new();
        let mut p = w.spawn(&["claude-tui", "trust"]);
        p.wait_row(39, " Enter to confirm · Esc to cancel");
        assert_trust(&p, false);

        // A digit is read (the log has it) but draws nothing: the next frame
        // is the one the arrow causes.
        let mark = p.mark();
        p.send(b"2");
        p.wait("the digit in the input log", UI, |_| w.input() == b"2");
        p.send(b"\x1b[B");
        p.wait_rows(&[(37, " ❯ Yes, I trust this folder"), (39, " Enter to confirm · Esc to cancel")]);
        assert_eq!(count(&p.out[mark..], b"\x1b[2J"), 1, "the digit redrew the dialog");
        assert_trust(&p, true);
        p.send(b"\x1b[B"); // clamped, no wrap
        p.send(b"\x1b[A");
        p.wait_rows(&[(36, " ❯ No, exit"), (39, " Enter to confirm · Esc to cancel")]);
        assert_trust(&p, false);

        p.send(b"\r");
        let status = p.wait_exit(EXIT);
        assert_eq!(status.code(), Some(1), "Enter on No exits 1");
        assert!(contains(&p.out, b"\x1b[?1049l"), "left the alternate screen");
    }

    #[test]
    fn trust_esc_exits_1_and_yes_goes_idle() {
        let w = World::new();
        let mut p = w.spawn(&["claude-tui", "trust"]);
        p.wait_row(39, " Enter to confirm · Esc to cancel");
        p.send(b"\x1b");
        assert_eq!(p.wait_exit(EXIT).code(), Some(1));

        let mut p = w.spawn(&["claude-tui", "trust", "--trust-default", "yes"]);
        p.wait_row(39, " Enter to confirm · Esc to cancel");
        assert_trust(&p, true);
        p.send(b"\r");
        p.wait_row(5, "⏺ trusted");
        wait_idle(&mut p, PLACEHOLDER);
    }

    #[test]
    fn trust_flaky_replaces_the_dialog_after_one_arrow() {
        let w = World::new();
        let mut p = w.spawn(&["claude-tui", "trust", "--trust-flaky"]);
        p.wait_row(39, " Enter to confirm · Esc to cancel");
        p.send(b"\x1b[A");
        p.wait_any_row(" Something else happened");
        for r in 1..=R {
            let row = p.row(r);
            assert!(
                row.is_empty() || row == " Something else happened",
                "row {r} still shows {row:?}\n{}",
                p.dump()
            );
        }
        // Kept: Enter changes nothing and does not exit.
        let mark = p.mark();
        p.send(b"\r");
        p.wait("Enter in the input log", UI, |_| w.input().ends_with(b"\r"));
        assert!(p.quiet_for(Duration::from_millis(300)).is_empty());
        assert_eq!(p.mark(), mark);
        assert!(p.child.try_wait().expect("try_wait").is_none());
    }

    #[test]
    fn onboarding_runs_theme_security_trust_then_idle() {
        let w = World::new();
        let mut p = w.spawn(&["claude-tui", "onboarding"]);
        p.wait_row(39, "     Dark mode (colorblind-friendly)");
        assert_eq!(p.row(33), " Choose the text style that looks best with your terminal");
        assert_eq!(p.row(34), " To change this later, run /theme");
        assert_eq!(p.row(35), "");
        assert_eq!(p.row(36), "     Auto (match terminal)");
        assert_eq!(p.row(37), " ❯ Dark mode");
        assert_eq!(p.row(38), "     Light mode");
        p.send(b"\x1b[A");
        p.wait_rows(&[(36, " ❯ Auto (match terminal)"), (39, "     Dark mode (colorblind-friendly)")]);
        assert_eq!(p.row(37), "     Dark mode");
        p.send(b"\r");
        p.wait_row(39, " Press Enter to continue…");
        assert_eq!(p.row(35), " Security notes:");
        assert_eq!(p.row(36), " 1. Claude can make mistakes.");
        assert_eq!(p.row(37), " 2. Due to prompt injection risks, only use it with code you trust");
        p.send(b"\r");
        p.wait_row(39, " Enter to confirm · Esc to cancel");
        assert_trust(&p, false);
        p.send(b"\x1b[B");
        p.wait_row(37, " ❯ Yes, I trust this folder");
        p.send(b"\r");
        p.wait_row(5, "⏺ trusted");
        wait_idle(&mut p, PLACEHOLDER);
    }

    #[test]
    fn chrome_dialog_then_idle() {
        let w = World::new();
        let mut p = w.spawn(&["claude-tui", "chrome"]);
        p.wait_row(39, "  Enter to confirm · Esc to keep browser tools off");
        assert_eq!(p.row(34), "  Claude in Chrome extension detected");
        assert_eq!(p.row(36), "  ❯ No, keep browser tools off");
        assert_eq!(p.row(37), "    Yes, use my browser");
        p.send(b"\x1b");
        wait_idle(&mut p, PLACEHOLDER);
    }

    #[test]
    fn exit_command_leaves_the_alternate_screen_with_0() {
        let w = World::new();
        let mut p = w.spawn(&["claude-tui", "idle"]);
        p.wait_row(R - 1, "  ⏸ manual mode on · ? for shortcuts");
        p.send(b"/exit\r");
        assert_eq!(p.wait_exit(EXIT).code(), Some(0));
        assert!(contains(&p.out, b"\x1b[?1049l"));
    }

    #[test]
    fn sigterm_restores_the_screen_and_dies_of_sigterm() {
        let w = World::new();
        let mut p = w.spawn(&["claude-tui", "idle"]);
        p.wait_row(R - 1, "  ⏸ manual mode on · ? for shortcuts");
        // SAFETY: the child this test spawned.
        unsafe {
            libc::kill(p.pid(), libc::SIGTERM);
        }
        let status = p.wait_exit(EXIT);
        assert_eq!(exit_signal(&status), Some(libc::SIGTERM));
        assert!(contains(&p.out, b"\x1b[?1049l"));
    }

    #[test]
    fn claude_tui_usage_errors() {
        for args in [&["claude-tui"][..], &["claude-tui", "nope"], &["claude-tui", "idle", "--bogus"]] {
            let out = super::run(args);
            assert_eq!(out.status.code(), Some(2), "{args:?}");
        }
    }

    // -----------------------------------------------------------------------
    // tui-idle-ready
    // -----------------------------------------------------------------------

    #[test]
    fn idle_ready_screens_match_the_ready_signatures() {
        let w = World::new();

        let mut p = w.spawn(&["tui-idle-ready", "0", "claude"]);
        p.wait_row(R - 1, "  ⏸ manual mode on · ? for shortcuts");
        assert_claude_idle(&p, PLACEHOLDER);

        let mut p = w.spawn(&["tui-idle-ready", "0", "codex"]);
        p.wait_row(R - 1, "  fake-model low · /tmp/fake-project");
        assert_eq!(p.row(1), ">_ Fake Codex (v0.0.0)");
        assert_eq!(p.row(R - 3), "› Find and fix a bug in @filename");
        assert_eq!(p.row(R - 2), "");
        assert!(!contains(&p.out, b"\x1b[?1049h"), "codex draws on the main screen");

        let mut p = w.spawn(&["tui-idle-ready", "0", "cursor"]);
        p.wait_row(R - 1, "  Fake Cursor Agent · / for commands · @ for files");
        let inner = usize::from(COLS) - 2;
        assert_eq!(p.row(R - 4), format!("╭{}╮", "─".repeat(inner)));
        let body = " → Plan, search, build anything";
        assert_eq!(p.row(R - 3), format!("│{body}{}│", " ".repeat(inner - body.chars().count())));
        assert_eq!(p.row(R - 2), format!("╰{}╯", "─".repeat(inner)));
        assert!(contains(&p.out, b"\x1b[?1049h"));
        // Three full-width rows in a row: only pending wrap lands them.
        assert_ne!(p.eager.row_text(R - 1), p.row(R - 1));

        let mut p = w.spawn(&["tui-idle-ready", "0", "agy"]);
        p.wait_row(R - 1, "  ? for shortcuts · Fake Antigravity");
        assert_eq!(p.row(R - 3), ">");
        assert_eq!(p.row(R - 2), "");
        assert!(contains(&p.out, b"\x1b[?1049h"));
    }

    #[test]
    fn idle_ready_echoes_submitted_lines_per_style() {
        let w = World::new();
        for (style, footer, input_row) in [
            ("claude", "  ⏸ manual mode on · ? for shortcuts", "❯ hi there"),
            ("codex", "  fake-model low · /tmp/fake-project", "› hi there"),
            ("agy", "  ? for shortcuts · Fake Antigravity", "> hi there"),
        ] {
            let mut p = w.spawn(&["tui-idle-ready", "0", style]);
            p.wait_row(R - 1, footer);
            p.send(b"hi there");
            p.wait_row(R - 3, input_row);
            p.send(b"\r");
            p.wait_any_row("got: hi there");
            p.wait_row(R - 1, footer);
            assert!(contains(&p.out, "working… (esc to interrupt)".as_bytes()), "{style}");
        }
    }

    #[test]
    fn idle_ready_drops_a_line_typed_before_ready_and_echoes_one_after() {
        let w = World::new();
        // Queued in the PTY before the process even exists: certainly early.
        let mut p = Pty::spawn(w.cmd(&["tui-idle-ready", "300", "cursor"]), b"early line\r");
        p.wait_text("Starting…");
        p.wait_row(R - 1, "  Fake Cursor Agent · / for commands · @ for files");
        p.send(b"later line\r");
        p.wait_any_row("got: later line");
        assert!(p.find_row("got: early line").is_none(), "{}", p.dump());
        assert!(!contains(&p.out, b"early line"), "the early line was echoed somewhere");
        assert_eq!(w.input(), b"early line\rlater line\r", "both are in the input log");
    }

    // -----------------------------------------------------------------------
    // term-query
    // -----------------------------------------------------------------------

    #[test]
    fn term_query_reports_none_when_nobody_answers() {
        let w = World::new();
        let mut p = w.spawn(&["term-query"]);
        p.wait_text("query-at\r\n\x1b[3;5H\x1b[c\x1b[6n\x1b[>0q");
        // The fixture gives up after its own 3 s; the ceiling is far above.
        p.wait("done", Duration::from_secs(60), |p| contains(&p.out, b"done\r\n"));
        assert!(contains(&p.out, b"da1=none\r\ndsr=none\r\ndone\r\n"), "{}", p.dump());
        assert!(p.child.try_wait().expect("try_wait").is_none(), "term-query hangs after done");
    }

    #[test]
    fn term_query_reports_the_replies() {
        let w = World::new();
        let mut p = w.spawn(&["term-query"]);
        p.wait_text("\x1b[>0q");
        // DSR as a terminal would answer it: the cursor really is at 3;5.
        assert_eq!((p.grid.row, p.grid.col), (2, 4));
        p.send(b"\x1bP>|fake-term 1.0\x1b\\\x1b[?62;22c\x1b[3;5R");
        p.wait_text("done\r\n");
        assert!(
            contains(&p.out, b"da1=\\e[?62;22c\r\ndsr=\\e[3;5R\r\ndone\r\n"),
            "{}",
            p.dump()
        );
    }

    // -----------------------------------------------------------------------
    // teammate
    // -----------------------------------------------------------------------

    #[test]
    fn teammate_requires_the_three_flags_together() {
        for args in [
            &["teammate", "--agent-id", "alpha@team-x"][..],
            &["teammate", "--agent-name", "alpha", "--team-name", "team-x"],
            &["teammate", "--agent-id", "alpha@team-x", "--team-name", "team-x"],
        ] {
            let out = super::run(args);
            assert_eq!(out.status.code(), Some(1), "{args:?}");
            assert!(
                String::from_utf8_lossy(&out.stderr).contains(
                    "Error: --agent-id, --agent-name, and --team-name must all be provided together"
                ),
                "{args:?}"
            );
        }
        assert_eq!(super::run(&["teammate"]).status.code(), Some(2));
    }

    /// The single transcript the teammate wrote, as parsed rows.
    fn transcript_rows(w: &World) -> Vec<Value> {
        let slug: String = w
            .proj()
            .to_string_lossy()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let dir = w.cfg().join("projects").join(slug);
        let files: Vec<PathBuf> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
            .map(|e| e.expect("dir entry").path())
            .collect();
        assert_eq!(files.len(), 1, "{files:?}");
        let name = files[0].file_stem().expect("stem").to_string_lossy().into_owned();
        assert!(uuid::Uuid::parse_str(&name).is_ok(), "transcript name {name}");
        std::fs::read_to_string(&files[0])
            .expect("transcript")
            .lines()
            .map(|l| serde_json::from_str(l).expect("a JSON row"))
            .collect()
    }

    const TEAMMATE_ARGS: [&str; 15] = [
        "teammate",
        "--agent-id",
        "alpha@team-x",
        "--agent-name",
        "alpha",
        "--team-name",
        "team-x",
        "--agent-color",
        "blue",
        "--parent-session-id",
        "11111111-2222-4333-8444-555555555555",
        "--permission-mode",
        "default",
        "--settings",
        "{\"permissions\":{\"defaultMode\":\"default\"}}",
    ];

    #[test]
    fn teammate_writes_a_transcript_head_and_echoes_its_argv() {
        let w = World::new();
        let mut p = w.spawn(&TEAMMATE_ARGS);
        p.wait_text("teammate alpha@team-x ready\r\n");
        for a in &TEAMMATE_ARGS[1..] {
            p.wait_text(&format!("arg: {a}\r\n"));
        }
        p.wait_row(R - 1, "  ⏸ manual mode on · ? for shortcuts");

        let rows = transcript_rows(&w);
        let head = &rows[0];
        let sid = head["sessionId"].as_str().expect("sessionId");
        assert!(uuid::Uuid::parse_str(sid).is_ok());
        assert_eq!(head["cwd"], w.proj().to_string_lossy().as_ref());
        assert_eq!(head["teamName"], "team-x");
        assert_eq!(head["agentName"], "alpha");
        assert_eq!(head["type"], "user");
        let ts = head["timestamp"].as_str().expect("timestamp");
        let shape_ok = ts.len() == 24
            && ts.ends_with('Z')
            && ts.as_bytes()[10] == b'T'
            && ts[..4].parse::<u32>().is_ok_and(|y| (2020..2200).contains(&y));
        assert!(shape_ok, "timestamp {ts:?}");

        // A finished turn: user + assistant rows and one idle_notification.
        p.send(b"do it\r");
        p.wait_any_row("⏺ got: do it");
        let inbox = w.cfg().join("teams/team-x/inboxes/team-lead.json");
        p.wait("the idle_notification", UI, |_| inbox.exists());
        let entries: Vec<Value> =
            serde_json::from_slice(&std::fs::read(&inbox).expect("inbox")).expect("inbox JSON");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["from"], "alpha");
        let frame: Value = serde_json::from_str(entries[0]["text"].as_str().expect("text")).expect("frame");
        assert_eq!(frame["type"], "idle_notification");
        assert_eq!(frame["idleReason"], "available");
        let rows = transcript_rows(&w);
        assert_eq!(rows.len(), 3, "{rows:?}");
        assert_eq!(rows[1]["message"]["content"], "do it");
        assert_eq!(rows[2]["type"], "assistant");
        assert!(rows.iter().all(|r| r["sessionId"] == sid && r["agentName"] == "alpha"));
    }

    #[test]
    fn teammate_esc_turn_leaves_the_interrupt_row_and_no_idle_notification() {
        let w = World::new();
        let mut args = TEAMMATE_ARGS.to_vec();
        args.push("--fake-esc-turn");
        let mut p = w.spawn(&args);
        p.wait_text("teammate alpha@team-x ready\r\n");
        p.wait_row(R - 1, "  ⏸ manual mode on · ? for shortcuts");
        let rows = transcript_rows(&w);
        assert_eq!(rows.len(), 3, "{rows:?}");
        assert_eq!(rows[1]["type"], "assistant");
        assert_eq!(rows[2]["type"], "user");
        assert_eq!(rows[2]["message"]["content"][0]["text"], "[Request interrupted by user]");

        // An interactive Esc mid-turn does the same.
        let mut args = TEAMMATE_ARGS.to_vec();
        args.extend(["--work-ms", "600000"]);
        let w = World::new();
        let mut p = w.spawn(&args);
        p.wait_row(R - 1, "  ⏸ manual mode on · ? for shortcuts");
        p.send(b"long one\r");
        p.wait("the spinner", UI, |p| p.row(R - 6).contains("Working…"));
        p.send(b"\x1b");
        p.wait_any_row(INTERRUPTED);
        let rows = transcript_rows(&w);
        let last = rows.last().expect("rows");
        assert_eq!(last["message"]["content"][0]["text"], "[Request interrupted by user]");
        assert!(!w.cfg().join("teams/team-x/inboxes/team-lead.json").exists());
    }

    // -----------------------------------------------------------------------
    // teammate-lead against a test-only fake tmux
    // -----------------------------------------------------------------------

    /// A POSIX-sh fake tmux (shell builtins only: the hermetic `PATH` holds
    /// nothing else). Logs each argv as `[a][b]…`, keeps `has-session` false
    /// until `new-session` ran, answers `%0`, `%1`, … for `-P` calls, saves
    /// each respawn-pane command verbatim, and fails the subcommand named in
    /// `$FAKE_TMUX_FAIL`.
    const FAKE_TMUX: &str = r#"#!/bin/sh
st="$FAKE_TMUX_STATE"
line=""
for a in "$@"; do line="$line[$a]"; done
printf '%s\n' "$line" >> "$st/log"
sub="$3"
if [ "$sub" = "$FAKE_TMUX_FAIL" ]; then echo "fake tmux: $sub failed" >&2; exit 1; fi
case "$sub" in
  has-session)
    if [ -f "$st/session" ]; then exit 0; fi
    echo "can't find session: claude-swarm" >&2; exit 1 ;;
  new-session) : > "$st/session" ;;
  list-windows) echo swarm-view ;;
  respawn-pane) printf '%s' "$8" > "$st/cmd.$6" ;;
esac
for a in "$@"; do
  if [ "$a" = "-P" ]; then
    n=0
    if [ -f "$st/next" ]; then read n < "$st/next"; fi
    echo "%$n"
    echo $((n + 1)) > "$st/next"
    break
  fi
done
exit 0
"#;

    struct LeadWorld {
        w: World,
        state: PathBuf,
    }

    impl LeadWorld {
        fn new() -> LeadWorld {
            let w = World::new();
            let tmux = w.h.bin_dir().join("tmux");
            std::fs::write(&tmux, FAKE_TMUX).expect("write fake tmux");
            let mut perms = std::fs::metadata(&tmux).expect("meta").permissions();
            std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
            std::fs::set_permissions(&tmux, perms).expect("chmod fake tmux");
            let state = w.h.root().join("tmux-state");
            std::fs::create_dir_all(&state).expect("state dir");
            LeadWorld { w, state }
        }

        fn spawn(&self, args: &[&str], extra_env: &[(&str, &str)]) -> Pty {
            let mut cmd = self.w.cmd(args);
            cmd.env("FAKE_TMUX_STATE", &self.state);
            for (k, v) in extra_env {
                cmd.env(k, v);
            }
            Pty::spawn(cmd, b"")
        }

        /// The logged calls, `-L claude-swarm-<pid>` stripped (and checked).
        fn calls(&self, pid: i32) -> Vec<String> {
            let log = std::fs::read_to_string(self.state.join("log")).unwrap_or_default();
            let prefix = format!("[-L][claude-swarm-{pid}]");
            log.lines()
                .map(|l| {
                    l.strip_prefix(&prefix)
                        .unwrap_or_else(|| panic!("tmux call without {prefix}: {l}"))
                        .to_string()
                })
                .collect()
        }

        fn command(&self, pane: &str) -> String {
            std::fs::read_to_string(self.state.join(format!("cmd.{pane}"))).expect("saved command")
        }

        fn team(&self) -> PathBuf {
            self.w.cfg().join("teams/team-x")
        }
    }

    /// `procStart` computed independently: the kernel's start time of `pid`
    /// through `gmtime_r` + `strftime("%a %b %e %H:%M:%S %Y")`.
    fn ctime_of_process_start(pid: i32) -> String {
        use libproc::libproc::{bsd_info::BSDInfo, proc_pid::pidinfo};
        let start = pidinfo::<BSDInfo>(pid, 0).expect("pidinfo").pbi_start_tvsec as libc::time_t;
        // SAFETY: gmtime_r and strftime write into buffers we own.
        unsafe {
            let mut tm: libc::tm = std::mem::zeroed();
            libc::gmtime_r(&start, &mut tm);
            let mut buf = [0u8; 64];
            let n = libc::strftime(buf.as_mut_ptr().cast(), buf.len(), c"%a %b %e %H:%M:%S %Y".as_ptr(), &tm);
            String::from_utf8_lossy(&buf[..n]).into_owned()
        }
    }

    fn styling(pane: &str, name: &str, colour: &str) -> Vec<String> {
        vec![
            format!("[set-option][-p][-t][{pane}][window-style][bg=default,fg={colour}]"),
            format!("[set-option][-p][-t][{pane}][pane-border-style][fg={colour}]"),
            format!("[set-option][-p][-t][{pane}][pane-active-border-style][fg={colour}]"),
            format!("[select-pane][-t][{pane}][-T][{name}]"),
            format!("[set-option][-p][-t][{pane}][pane-border-format][#[fg={colour},bold] #{{pane_title}} #[default]]"),
        ]
    }

    #[test]
    fn teammate_lead_issues_the_external_sequence_and_writes_the_team_files() {
        let lw = LeadWorld::new();
        let settings = "{\"permissions\":{\"defaultMode\":\"default\"}}";
        let mut p = lw.spawn(
            &[
                "teammate-lead", "--teammates", "2", "--team", "team-x", "--hold",
                "--permission-mode", "default", "--settings", settings, "--model", "haiku",
            ],
            &[("ANTHROPIC_BASE_URL", "http://127.0.0.1:9/CANARY-T06-ENV")],
        );
        p.wait_text("lead ready\r\n");
        let pid = p.pid();
        let calls = lw.calls(pid);
        let respawn = |pane: &str| format!("[respawn-pane][-k][-t][{pane}][--][{}]", lw.command(pane));
        let mut expected: Vec<String> = vec![
            // #11: no session yet → new-session; the first teammate reuses its pane.
            "[has-session][-t][claude-swarm]".into(),
            "[new-session][-d][-s][claude-swarm][-n][swarm-view][-P][-F][#{pane_id}][--][cat]".into(),
            "[set-option][-w][-t][claude-swarm:swarm-view][pane-border-status][top]".into(),
        ];
        expected.extend(styling("%0", "alpha", "blue"));
        expected.push("[set-option][-p][-t][%0][remain-on-exit][failed]".into());
        expected.push(respawn("%0"));
        // #12/#13: the session exists → list-windows, split %0 (-v: 1 teammate), tiled.
        expected.extend([
            "[has-session][-t][claude-swarm]".to_string(),
            "[list-windows][-t][claude-swarm][-F][#{window_name}]".into(),
            "[list-panes][-t][claude-swarm:swarm-view][-F][#{pane_id}]".into(),
            "[split-window][-d][-t][%0][-v][-P][-F][#{pane_id}][--][cat]".into(),
            "[select-layout][-t][claude-swarm:swarm-view][tiled]".into(),
        ]);
        expected.extend(styling("%1", "beta", "green"));
        expected.push("[set-option][-p][-t][%1][remain-on-exit][failed]".into());
        expected.push(respawn("%1"));
        assert_eq!(calls, expected);
        // Every call was printed too.
        assert!(contains(&p.out, format!("tmux: -L claude-swarm-{pid} has-session -t claude-swarm\r\n").as_bytes()));

        // The respawn-pane payload (R3 §4.2 / R6 §5 LP-10 shape).
        let proj = lw.w.proj();
        let exe = std::fs::canonicalize(fake_agent_path()).expect("exe");
        let cmd = lw.command("%0");
        assert!(cmd.starts_with(&format!("cd {} && env CLAUDECODE=1 CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1 ", proj.display())), "{cmd}");
        assert!(cmd.contains("ANTHROPIC_BASE_URL=http://127.0.0.1:9/CANARY-T06-ENV "), "{cmd}");
        assert!(cmd.contains(&format!("CLAUDE_CONFIG_DIR={} ", lw.w.cfg().display())), "{cmd}");
        assert!(cmd.contains(" CLAUDE_CODE_ATTRIBUTION_PROJECT='' "), "{cmd}");
        let registry_path = lw.w.cfg().join(format!("sessions/{pid}.json"));
        let registry: Value = serde_json::from_slice(&std::fs::read(&registry_path).expect("registry")).expect("registry JSON");
        let sid = registry["sessionId"].as_str().expect("sessionId").to_string();
        let tail = format!(
            " teammate --agent-id alpha@team-x --agent-name alpha --team-name team-x --agent-color blue \
             --parent-session-id {sid} --permission-mode default --settings '{settings}' --model haiku"
        );
        assert!(cmd.ends_with(&tail), "{cmd}\n--- wanted the tail ---\n{tail}");
        // The teammate binary is the lead's own executable (no override set).
        let exe_named = [exe, fake_agent_path()]
            .iter()
            .any(|e| cmd.contains(&format!(" {} teammate --agent-id", e.display())));
        assert!(exe_named, "{cmd}");

        // Registry: busy, with the kernel's start time as procStart.
        assert_eq!(registry["pid"], pid);
        assert_eq!(registry["status"], "busy");
        assert_eq!(registry["kind"], "interactive");
        assert_eq!(registry["entrypoint"], "cli");
        assert_eq!(registry["peerProtocol"], 1);
        assert_eq!(registry["procStart"], ctime_of_process_start(pid), "{registry}");

        // config.json: lead + one member per teammate.
        let config: Value = serde_json::from_slice(&std::fs::read(lw.team().join("config.json")).expect("config")).expect("config JSON");
        assert_eq!(config["name"], "team-x");
        assert_eq!(config["leadSessionId"], sid.as_str());
        assert_eq!(config["leadAgentId"], "team-lead@team-x");
        let members = config["members"].as_array().expect("members");
        assert_eq!(members.len(), 3);
        assert_eq!(members[0]["tmuxPaneId"], "leader");
        assert_eq!(members[0]["backendType"], "in-process");
        for (m, (name, pane, colour)) in members[1..].iter().zip([("alpha", "%0", "blue"), ("beta", "%1", "green")]) {
            assert_eq!(m["agentId"], format!("{name}@team-x"));
            assert_eq!(m["name"], name);
            assert_eq!(m["tmuxPaneId"], pane);
            assert_eq!(m["backendType"], "tmux");
            assert_eq!(m["isActive"], true);
            assert_eq!(m["color"], colour);
        }

        // Inboxes: the initial prompt + every frame type, each with the canary.
        for name in ["alpha", "beta"] {
            let inbox: Vec<Value> = serde_json::from_slice(
                &std::fs::read(lw.team().join(format!("inboxes/{name}.json"))).expect("inbox"),
            )
            .expect("inbox JSON");
            assert_eq!(inbox.len(), 7, "{name}");
            let mut types = Vec::new();
            for e in &inbox {
                let text = e["text"].as_str().expect("text");
                assert!(text.contains("CANARY-T06-INBOX"), "{text}");
                assert_eq!(e["read"], false);
                if let Ok(frame) = serde_json::from_str::<Value>(text) {
                    types.push(frame["type"].as_str().expect("type").to_string());
                }
            }
            assert_eq!(
                types,
                ["idle_notification", "permission_request", "shutdown_request", "task_assignment", "plan_approval_request"]
            );
        }
        assert_eq!(std::fs::read(lw.team().join("inboxes/team-lead.json")).expect("lead inbox"), b"[]");

        // The task list.
        let task: Value = serde_json::from_slice(&std::fs::read(lw.w.cfg().join("tasks/team-x/1.json")).expect("task")).expect("task JSON");
        assert_eq!(task["status"], "pending");
        assert_eq!(task["owner"], "alpha");

        // SIGTERM: #16 for every pane, team dir and registry removed, tasks kept.
        // SAFETY: the child this test spawned.
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        let status = p.wait_exit(EXIT);
        assert_eq!(exit_signal(&status), Some(libc::SIGTERM));
        let calls = lw.calls(pid);
        assert_eq!(&calls[calls.len() - 2..], ["[kill-pane][-t][%0]", "[kill-pane][-t][%1]"]);
        assert!(!lw.team().exists(), "team dir removed at exit");
        assert!(!registry_path.exists(), "registry removed at exit");
        assert!(lw.w.cfg().join("tasks/team-x/1.json").exists(), "tasks/<team> stays");
    }

    #[test]
    fn teammate_lead_pane_command_really_starts_the_teammate() {
        let lw = LeadWorld::new();
        let settings = "{\"permissions\":{\"defaultMode\":\"default\"}}";
        let mut lead = lw.spawn(
            &["teammate-lead", "--team", "team-x", "--hold", "--permission-mode", "default", "--settings", settings],
            &[],
        );
        lead.wait_text("lead ready\r\n");
        let command = lw.command("%0");

        // Run the payload as the pane host would: `/bin/sh -c <command>`. Its
        // `env` comes from a PATH dir holding only that one tool.
        let tools = lw.w.h.root().join("tools");
        std::fs::create_dir_all(&tools).expect("tools dir");
        std::os::unix::fs::symlink("/usr/bin/env", tools.join("env")).expect("env link");
        let mut sh = Command::new("/bin/sh");
        sh.arg("-c").arg(&command);
        lw.w.h.apply(&mut sh);
        sh.env("PATH", &tools).env("TERM", "xterm-256color").current_dir(lw.w.h.root());
        let mut mate = Pty::spawn(sh, b"");
        mate.wait_text("teammate alpha@team-x ready\r\n");
        mate.wait_text("arg: --permission-mode\r\narg: default\r\n");
        mate.wait_text(&format!("arg: --settings\r\narg: {settings}\r\n"));
        mate.wait_row(R - 1, "  ⏸ manual mode on · ? for shortcuts");
        // It found the config dir through the forwarded CLAUDE_CONFIG_DIR.
        let rows = transcript_rows(&lw.w);
        assert_eq!(rows[0]["teamName"], "team-x");
        assert_eq!(rows[0]["agentName"], "alpha");
    }

    #[test]
    fn teammate_lead_respawn_failure_is_a_swarm_pane_error() {
        let lw = LeadWorld::new();
        let mut p = lw.spawn(&["teammate-lead", "--team", "team-x", "--hold"], &[("FAKE_TMUX_FAIL", "respawn-pane")]);
        p.wait_text("SwarmPaneError: Failed to send command to pane %0\r\n");
        assert_eq!(p.wait_exit(EXIT).code(), Some(1));
        assert!(!contains(&p.out, b"lead ready"));
    }

    #[test]
    fn teammate_lead_writes_config_json_non_atomically() {
        let lw = LeadWorld::new();
        let config = lw.team().join("config.json");
        let mut p = lw.spawn(&["teammate-lead", "--team", "team-x", "--torn-ms", "1500", "--hold"], &[]);
        // Inside the torn window the file is half a JSON document.
        let mut torn_seen = false;
        let deadline = Instant::now() + Duration::from_secs(60);
        while !torn_seen && Instant::now() < deadline && !contains(&p.out, b"lead ready") {
            if let Ok(bytes) = std::fs::read(&config) {
                torn_seen = !bytes.is_empty() && serde_json::from_slice::<Value>(&bytes).is_err();
            }
            p.pump(Duration::from_millis(5));
        }
        assert!(torn_seen, "never saw a torn config.json");
        p.wait_text("lead ready\r\n");
        let parsed: Value = serde_json::from_slice(&std::fs::read(&config).expect("config")).expect("whole after the window");
        assert_eq!(parsed["members"].as_array().map(Vec::len), Some(2));
    }

    #[test]
    fn teammate_lead_resets_a_completed_task_after_5_s() {
        let lw = LeadWorld::new();
        let mut p = lw.spawn(&["teammate-lead", "--team", "team-x", "--hold"], &[]);
        p.wait_text("lead ready\r\n");
        let path = lw.w.cfg().join("tasks/team-x/1.json");
        let mut task: Value = serde_json::from_slice(&std::fs::read(&path).expect("task")).expect("task JSON");
        task["status"] = Value::from("completed");
        let tmp = path.with_extension("test-tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&task).expect("json")).expect("write");
        std::fs::rename(&tmp, &path).expect("rename");
        let marked = Instant::now();
        p.wait("the task reset", Duration::from_secs(60), |_| {
            std::fs::read(&path)
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .is_some_and(|t| t["status"] == "pending")
        });
        // Not before the 5 s the real lead waits (lower bound, with slack).
        assert!(marked.elapsed() >= Duration::from_secs(4), "reset after {:?}", marked.elapsed());
    }

    #[test]
    fn teammate_lead_without_hold_is_a_claude_tui_and_exit_kills_panes() {
        let lw = LeadWorld::new();
        let mut p = lw.spawn(&["teammate-lead", "--team", "team-x"], &[]);
        p.wait_text("lead ready\r\n");
        p.wait_row(R - 1, "  ⏸ manual mode on · ? for shortcuts");
        p.send(b"/exit\r");
        assert_eq!(p.wait_exit(EXIT).code(), Some(0));
        let calls = lw.calls(p.pid());
        assert_eq!(calls.last().map(String::as_str), Some("[kill-pane][-t][%0]"));
        assert!(!lw.team().exists());
    }

    #[test]
    fn teammate_lead_rejects_unknown_arguments() {
        assert_eq!(super::run(&["teammate-lead", "--bogus", "1"]).status.code(), Some(2));
        assert_eq!(super::run(&["teammate-lead", "--teammates", "x"]).status.code(), Some(2));
    }
}
