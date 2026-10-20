//! Test-side driver for the `fake_agent` fixture binary.
//!
//! Include it from any integration test with a plain `mod fixtures;`
//! (`tests/<name>.rs` → `tests/fixtures/mod.rs`).
//!
//! What it gives you:
//!
//!   * [`fake_agent_path`] — the built fixture binary, for tests that need to
//!     hand a *path* to something else (a PTY session, the sidecar's `cmd`);
//!   * [`command`] — a preconfigured [`Command`] (own process group, piped
//!     stdio) for tests that want to spawn it themselves;
//!   * one starter per mode, each returning a [`Running`] that reaps the child
//!     and every descendant it told us about when it is dropped — no test can
//!     leak a `hang` into the developer's machine, even on panic;
//!   * [`flood`] — the same byte generator the fixture emits, so byte-fidelity
//!     assertions compare against an independently computed buffer.
//!
//! Every process this module spawns is put in **its own process group**, which
//! is what makes `killpg`-based assertions safe: a stray `killpg` can never
//! walk up into the test harness.

#![allow(dead_code)]

#[path = "fake_agent/flood.rs"]
pub mod flood;

use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use std::os::unix::process::CommandExt;

/// Default patience for "the fixture should have said something by now".
pub const READY_TIMEOUT: Duration = Duration::from_secs(10);
/// Default patience for "this process should be gone by now".
pub const EXIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Every publicly documented mode, with arguments that make it terminate
/// quickly enough to be used in a loop. Used by the "everything dies on
/// SIGKILL" sweep.
pub const ALL_MODES: &[&[&str]] = &[
    &["normal", "0"],
    &["hang"],
    &["escape-group"],
    &["spawn-grandchild", "30"],
    &["binary-flood", "20000000"],
    &["ignore-term"],
    &["slow-exit", "30"],
    &["print-env", "PATH"],
    &["alloc", "64", "30"],
];

// ---------------------------------------------------------------------------
// locating the binary
// ---------------------------------------------------------------------------

/// Absolute path of the built `fake_agent` binary.
///
/// Inside an integration test Cargo hands us the path directly through
/// `CARGO_BIN_EXE_fake_agent`. The `current_exe` walk is the fallback for
/// contexts where that variable is not set (unit tests inside the lib, or a
/// harness that re-execs).
pub fn fake_agent_path() -> PathBuf {
    if let Some(p) = option_env!("CARGO_BIN_EXE_fake_agent") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return p;
        }
    }
    let mut dir = std::env::current_exe().expect("current_exe");
    dir.pop(); // .../target/<profile>/deps/<test-binary>
    for _ in 0..2 {
        let candidate = dir.join("fake_agent");
        if candidate.is_file() {
            return candidate;
        }
        if !dir.pop() {
            break;
        }
    }
    panic!(
        "fake_agent binary not found — build it with `cargo test` or \
         `cargo build --bin fake_agent`"
    );
}

/// A [`Command`] for the fixture: own process group, stdin closed, stdout and
/// stderr piped.
pub fn command(args: &[&str]) -> Command {
    let mut cmd = Command::new(fake_agent_path());
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // DESIGN §8: every spawn gets its own process group. Here it is also a
        // safety fence — killpg in a test can never reach the test runner.
        .process_group(0);
    cmd
}

/// Spawn the fixture and wait for it to finish. Only for modes that exit.
pub fn run(args: &[&str]) -> Output {
    command(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to run fake_agent {args:?}: {e}"))
}

// ---------------------------------------------------------------------------
// a live fixture process
// ---------------------------------------------------------------------------

/// A running `fake_agent`, with line readers on both pipes and a kill-on-drop
/// guarantee that also covers descendants registered with [`Running::track`].
pub struct Running {
    child: Child,
    pid: i32,
    args: Vec<String>,
    stdout_rx: Receiver<String>,
    stderr_rx: Receiver<String>,
    tracked: Vec<i32>,
    reaped: bool,
}

impl Running {
    /// Spawn `args` and attach line readers.
    pub fn start(args: &[&str]) -> Running {
        let mut child = command(args)
            .spawn()
            .unwrap_or_else(|e| panic!("failed to spawn fake_agent {args:?}: {e}"));
        let pid = child.id() as i32;
        let stdout_rx = line_reader(child.stdout.take().expect("piped stdout"));
        let stderr_rx = line_reader(child.stderr.take().expect("piped stderr"));
        Running {
            child,
            pid,
            args: args.iter().map(|s| s.to_string()).collect(),
            stdout_rx,
            stderr_rx,
            tracked: Vec::new(),
            reaped: false,
        }
    }

    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// Process group id of the fixture. Because [`command`] sets
    /// `process_group(0)` this equals [`Running::pid`] while it is alive.
    pub fn pgid(&self) -> i32 {
        getpgid(self.pid).unwrap_or(self.pid)
    }

    /// Register a descendant pid so it is SIGKILLed when this handle drops.
    pub fn track(&mut self, pid: i32) {
        self.tracked.push(pid);
    }

    /// Descendant pids registered with [`Running::track`].
    pub fn tracked(&self) -> &[i32] {
        &self.tracked
    }

    /// Next stdout line, or `None` on timeout / EOF.
    pub fn next_stdout_line(&self, timeout: Duration) -> Option<String> {
        match self.stdout_rx.recv_timeout(timeout) {
            Ok(line) => Some(line),
            Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => None,
        }
    }

    /// Next stderr line, or `None` on timeout / EOF.
    pub fn next_stderr_line(&self, timeout: Duration) -> Option<String> {
        match self.stderr_rx.recv_timeout(timeout) {
            Ok(line) => Some(line),
            Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => None,
        }
    }

    /// Read stdout lines until one starts with `prefix`. Panics on timeout —
    /// a fixture that never announces itself is a bug, not a flake to ignore.
    pub fn expect_stdout_line(&self, prefix: &str, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                panic!(
                    "fake_agent {:?}: no stdout line starting with {prefix:?} within {timeout:?}",
                    self.args
                );
            }
            match self.next_stdout_line(left) {
                Some(line) if line.starts_with(prefix) => return line,
                Some(_) => continue,
                None => panic!(
                    "fake_agent {:?}: stdout ended before a line starting with {prefix:?}",
                    self.args
                ),
            }
        }
    }

    /// Wait for `<prefix> <pid>` and return the pid.
    pub fn expect_pid_line(&self, prefix: &str, timeout: Duration) -> i32 {
        let line = self.expect_stdout_line(prefix, timeout);
        line.trim()
            .rsplit(' ')
            .next()
            .and_then(|s| s.parse::<i32>().ok())
            .unwrap_or_else(|| panic!("fake_agent: cannot parse a pid out of {line:?}"))
    }

    /// Send a signal to the fixture process only.
    pub fn signal(&self, sig: i32) {
        kill_pid(self.pid, sig);
    }

    /// Send a signal to the fixture's whole process group.
    pub fn signal_group(&self, sig: i32) {
        killpg(self.pgid(), sig);
    }

    /// Non-blocking exit check.
    pub fn try_wait(&mut self) -> Option<ExitStatus> {
        match self.child.try_wait() {
            Ok(Some(status)) => {
                self.reaped = true;
                Some(status)
            }
            Ok(None) => None,
            Err(e) => panic!("try_wait failed: {e}"),
        }
    }

    /// Wait up to `timeout` for the fixture to exit.
    pub fn wait_timeout(&mut self, timeout: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.try_wait() {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Wait up to `timeout`, panicking if the fixture is still running.
    pub fn expect_exit(&mut self, timeout: Duration) -> ExitStatus {
        self.wait_timeout(timeout).unwrap_or_else(|| {
            panic!(
                "fake_agent {:?} (pid {}) did not exit within {timeout:?}",
                self.args, self.pid
            )
        })
    }

    /// `SIGKILL` + reap. Idempotent.
    pub fn kill_and_reap(&mut self) -> Option<ExitStatus> {
        if !self.reaped {
            kill_pid(self.pid, libc::SIGKILL);
        }
        self.wait_timeout(EXIT_TIMEOUT)
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        for pid in std::mem::take(&mut self.tracked) {
            kill_pid(pid, libc::SIGKILL);
        }
        if !self.reaped {
            kill_pid(self.pid, libc::SIGKILL);
            let _ = self.child.wait();
        }
    }
}

fn line_reader<R: Read + Send + 'static>(source: R) -> Receiver<String> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let reader = BufReader::new(source);
        for line in reader.lines() {
            match line {
                Ok(line) => {
                    if tx.send(line).is_err() {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    });
    rx
}

// ---------------------------------------------------------------------------
// per-mode helpers
// ---------------------------------------------------------------------------

/// `normal <code>` — runs to completion.
pub fn run_normal(code: i32) -> Output {
    run(&["normal", &code.to_string()])
}

/// `hang` — lives until something kills it.
pub fn start_hang() -> Running {
    Running::start(&["hang"])
}

/// A `escape-group` fixture plus the pid of the `setsid()` child that `killpg`
/// on [`Running::pgid`] cannot reach.
pub struct EscapeGroup {
    pub parent: Running,
    pub escaped_pid: i32,
}

pub fn start_escape_group() -> EscapeGroup {
    let mut parent = Running::start(&["escape-group"]);
    let announced = parent.expect_pid_line("parent-pid ", READY_TIMEOUT);
    assert_eq!(
        announced,
        parent.pid(),
        "fixture announced a different parent pid than it was spawned with"
    );
    let escaped_pid = parent.expect_pid_line("escaped-pid ", READY_TIMEOUT);
    parent.track(escaped_pid);
    EscapeGroup {
        parent,
        escaped_pid,
    }
}

/// A `spawn-grandchild` fixture plus the pid of the child that keeps running
/// after the fixture itself is killed.
pub struct SpawnGrandchild {
    pub parent: Running,
    pub grandchild_pid: i32,
}

pub fn start_spawn_grandchild(secs: u64) -> SpawnGrandchild {
    let mut parent = Running::start(&["spawn-grandchild", &secs.to_string()]);
    let announced = parent.expect_pid_line("parent-pid ", READY_TIMEOUT);
    assert_eq!(announced, parent.pid());
    let grandchild_pid = parent.expect_pid_line("grandchild-pid ", READY_TIMEOUT);
    parent.track(grandchild_pid);
    SpawnGrandchild {
        parent,
        grandchild_pid,
    }
}

/// The bytes `binary-flood <total>` is supposed to produce.
pub fn expected_flood(total: usize) -> Vec<u8> {
    flood::flood_bytes(total)
}

/// Run `binary-flood <total>` and capture stdout in memory.
pub fn run_binary_flood(total: u64) -> Vec<u8> {
    let out = run(&["binary-flood", &total.to_string()]);
    assert!(
        out.status.success(),
        "binary-flood {total} exited with {:?}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// Run `binary-flood <total>` with stdout redirected straight into `path`.
/// Keeps the bytes out of any in-process buffering, which is what the
/// byte-fidelity acceptance test wants.
pub fn run_binary_flood_to_file(total: u64, path: &Path) -> ExitStatus {
    let file = File::create(path).unwrap_or_else(|e| panic!("create {}: {e}", path.display()));
    let status = Command::new(fake_agent_path())
        .arg("binary-flood")
        .arg(total.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::from(file))
        .stderr(Stdio::piped())
        .process_group(0)
        .status()
        .expect("spawn binary-flood");
    status
}

/// `ignore-term` — already past its `signal()` call when this returns.
pub fn start_ignore_term() -> Running {
    let parent = Running::start(&["ignore-term"]);
    parent.expect_stdout_line("ready", READY_TIMEOUT);
    parent
}

/// `slow-exit <secs>` — already past its `signal()` call when this returns.
pub fn start_slow_exit(secs: u64) -> Running {
    let parent = Running::start(&["slow-exit", &secs.to_string()]);
    parent.expect_stdout_line("ready", READY_TIMEOUT);
    parent
}

/// `print-env <var>` with `var` set to `value` in the child's environment.
pub fn run_print_env(var: &str, value: &str) -> Output {
    command(&["print-env", var])
        .env(var, value)
        .output()
        .expect("run print-env")
}

/// `emit-jsonl <path>`.
pub fn run_emit_jsonl(path: &Path) -> Output {
    command(&["emit-jsonl", &path.to_string_lossy()])
        .output()
        .expect("run emit-jsonl")
}

/// `alloc <mib> <secs>` — already resident when this returns.
pub fn start_alloc(mib: u64, secs: u64) -> Running {
    let parent = Running::start(&["alloc", &mib.to_string(), &secs.to_string()]);
    parent.expect_stdout_line("allocated ", READY_TIMEOUT);
    parent
}

// ---------------------------------------------------------------------------
// process primitives (libc, no shelling out)
// ---------------------------------------------------------------------------

/// `kill(pid, sig)`, ignoring `ESRCH`.
pub fn kill_pid(pid: i32, sig: i32) {
    // SAFETY: plain libc call with a pid we spawned or were told about.
    unsafe {
        libc::kill(pid, sig);
    }
}

/// `killpg(pgid, sig)`, ignoring `ESRCH`.
pub fn killpg(pgid: i32, sig: i32) {
    // SAFETY: plain libc call.
    unsafe {
        libc::killpg(pgid, sig);
    }
}

pub fn getpgid(pid: i32) -> Option<i32> {
    // SAFETY: plain libc call.
    let pgid = unsafe { libc::getpgid(pid) };
    if pgid < 0 {
        None
    } else {
        Some(pgid)
    }
}

/// Whether `pid` still exists. `EPERM` counts as alive (the process is there,
/// we merely may not signal it).
pub fn pid_alive(pid: i32) -> bool {
    // SAFETY: signal 0 performs error checking without sending anything.
    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Poll `cond` until it is true or `timeout` elapses.
pub fn wait_for(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Wait until `pid` is gone.
pub fn wait_until_gone(pid: i32, timeout: Duration) -> bool {
    wait_for(timeout, || !pid_alive(pid))
}

/// Resident set size of `pid` in KiB, via `ps`. `None` if the process is gone.
///
/// Test-only convenience — production RSS sampling lives in `supervise`.
pub fn rss_kib(pid: i32) -> Option<u64> {
    let out = Command::new("/bin/ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse::<u64>().ok()
}

/// Exit signal of a status, if it was signalled.
pub fn exit_signal(status: &ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}
