//! The shared test harness (Gilde 2 contract §2.2.2, §14.1; owner T01, frozen
//! after wave 0) and the test-side driver for the `fake_agent` fixture binary.
//!
//! Include it from any integration test with a plain `mod fixtures;`
//! (`tests/<name>.rs` → `tests/fixtures/mod.rs`).
//!
//! **Hermetic environment** ([`Hermetic`], contract §14.1, A38.2): every test
//! that runs product code sets `GILDE_HOME` to a temp dir, `HOME` into that
//! temp dir, a `PATH` that contains only fixture binaries, `SHELL` to a path
//! that does not exist, `DISABLE_AUTOUPDATER=1` and
//! `AGENT_CLI_CREDENTIAL_STORE=memory`, and refuses to start when a vendor CLI
//! (see [`VENDOR_CLIS`]) is reachable outside a fixture dir — on the
//! **process** `PATH` or on the static detection ladder of the process `HOME`
//! (`agents::path::static_candidates`, incl. `/opt/homebrew/bin` and
//! `/usr/local/bin`). A rig that boots the core also pins detection with
//! [`Hermetic::detection_pins`]. No test ever runs a real vendor CLI; cargo
//! itself runs under `scripts/gates/hermetic-cargo.sh` (A38.1).
//!
//! What the `fake_agent` driver gives you:
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

// ---------------------------------------------------------------------------
// the hermetic environment (contract §14.1)
// ---------------------------------------------------------------------------

/// Every name agent detection may run (`AgentKind::binary_names` of the vendor
/// kinds: `agent` is cursor's fallback name, `gemini` a kind of its own), the
/// Windows-style `claude.exe`, and tmux. A test must never reach one of them
/// outside a fixture dir.
pub const VENDOR_CLIS: &[&str] = &["claude", "claude.exe", "codex", "cursor-agent", "agent", "agy", "gemini", "tmux"];

/// The `SHELL` of every hermetic process (A38.2): a path that does not exist
/// (and that no user can create), so a login-shell probe can never read a real
/// profile and detection's shell row finds nothing.
pub const NO_SHELL: &str = "/nonexistent/gilde-hermetic-shell";

/// A temp root with `home/`, `home/.gilde` (`GILDE_HOME`) and `bin/` (the only
/// `PATH` entry). Dropping it removes everything.
pub struct Hermetic {
    root: tempfile::TempDir,
}

/// Serializes [`Hermetic::enter`] and [`EnvPin`]: the process environment is global.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The `bin/` dirs of the live [`Hermetic`]s: the only places a vendor-named
/// file may be reachable from (another test's `enter` puts its own on the
/// process `PATH`).
static LIVE_BIN_DIRS: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

impl Hermetic {
    /// Create the tree and refuse to start when a vendor CLI is reachable
    /// from this test process (A38.2, see [`reachable_vendor_clis`]).
    pub fn new() -> Hermetic {
        let root = tempfile::Builder::new()
            .prefix("gilde-test-")
            .tempdir()
            .expect("hermetic temp root");
        let h = Hermetic { root };
        for dir in [h.home(), h.gilde_home(), h.bin_dir(), h.tmp_dir()] {
            std::fs::create_dir_all(&dir).expect("hermetic dirs");
        }
        LIVE_BIN_DIRS.lock().unwrap_or_else(|p| p.into_inner()).push(h.bin_dir());
        h.assert_no_vendor_cli_outside_fixtures();
        h
    }

    pub fn root(&self) -> &Path {
        self.root.path()
    }

    pub fn home(&self) -> PathBuf {
        self.root.path().join("home")
    }

    pub fn gilde_home(&self) -> PathBuf {
        self.home().join(".gilde")
    }

    /// The only `PATH` entry: fixture binaries installed with [`Hermetic::install_fixture`].
    pub fn bin_dir(&self) -> PathBuf {
        self.root.path().join("bin")
    }

    pub fn tmp_dir(&self) -> PathBuf {
        self.root.path().join("tmp")
    }

    /// The complete environment of a hermetic child (nothing is inherited).
    pub fn env(&self) -> Vec<(String, String)> {
        let s = |p: PathBuf| p.to_string_lossy().into_owned();
        vec![
            ("HOME".into(), s(self.home())),
            ("GILDE_HOME".into(), s(self.gilde_home())),
            ("PATH".into(), s(self.bin_dir())),
            ("SHELL".into(), NO_SHELL.into()),
            ("TMPDIR".into(), s(self.tmp_dir())),
            ("LANG".into(), "en_US.UTF-8".into()),
            ("DISABLE_AUTOUPDATER".into(), "1".into()),
            ("AGENT_CLI_CREDENTIAL_STORE".into(), "memory".into()),
        ]
    }

    /// Clear `cmd`'s environment and give it exactly [`Hermetic::env`].
    pub fn apply<'c>(&self, cmd: &'c mut Command) -> &'c mut Command {
        cmd.env_clear();
        for (k, v) in self.env() {
            cmd.env(k, v);
        }
        cmd
    }

    /// Put a fixture binary on the hermetic `PATH` under `name` (e.g. a
    /// `fake_claude` as `claude`). Only fixture binaries may live here.
    pub fn install_fixture(&self, name: &str, fixture: &Path) -> PathBuf {
        let dest = self.bin_dir().join(name);
        std::os::unix::fs::symlink(fixture, &dest)
            .unwrap_or_else(|e| panic!("install fixture {name}: {e}"));
        dest
    }

    /// Set the hermetic variables in this process (for in-process code that
    /// reads `GILDE_HOME`, `HOME`, `PATH` or `SHELL`) until the guard drops.
    /// Tests that use it are serialized by a global lock.
    pub fn enter(&self) -> HermeticGuard<'_> {
        let vars = self.env();
        let vars: Vec<(&str, &str)> = vars.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        HermeticGuard { _pin: EnvPin::set(&vars), _env: self }
    }

    /// The `config.json` patch every rig that boots the core applies before
    /// it wires it (A38.2): `agents.pathOverride` = [`Hermetic::bin_dir`] (the
    /// override is the whole search path, A38.3), each vendor binary pinned to
    /// `<bin>/<name>` (the fixture a test installed there, or a path that does
    /// not exist), and `terminal.shell` = [`NO_SHELL`].
    pub fn detection_pins(&self) -> serde_json::Value {
        detection_pins_at(&self.bin_dir())
    }

    /// Contract §14.1, A38.2: no vendor CLI is reachable from this test
    /// process outside a live fixture dir — neither on the process `PATH` nor
    /// on the static detection ladder of the process `HOME` or of this tree's
    /// home. Panics with what was found and how to run hermetically.
    pub fn assert_no_vendor_cli_outside_fixtures(&self) {
        let found = reachable_vendor_clis(&[self.home()]);
        assert!(found.is_empty(), "{}", refusal(&found));
    }
}

impl Default for Hermetic {
    fn default() -> Self {
        Hermetic::new()
    }
}

impl Drop for Hermetic {
    fn drop(&mut self) {
        let bin = self.bin_dir();
        LIVE_BIN_DIRS.lock().unwrap_or_else(|p| p.into_inner()).retain(|d| *d != bin);
    }
}

/// Restores the process environment when dropped.
pub struct HermeticGuard<'h> {
    _pin: EnvPin,
    _env: &'h Hermetic,
}

/// Process environment variables set under the harness lock and restored when
/// dropped (also on a panic). [`Hermetic::enter`] is built on it; a rig whose
/// lifetime is not one `enter` scope (`boot.rs`'s `Booted`) holds one itself.
/// Every holder serializes with every other on the one lock.
pub struct EnvPin {
    _lock: std::sync::MutexGuard<'static, ()>,
    saved: Vec<(String, Option<std::ffi::OsString>)>,
}

impl EnvPin {
    pub fn set(vars: &[(&str, &str)]) -> EnvPin {
        let lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let mut saved = Vec::new();
        for (k, v) in vars {
            saved.push(((*k).to_string(), std::env::var_os(k)));
            // Every caller holds ENV_LOCK; tests that touch the process
            // environment go through a pin (edition 2021: a safe call).
            std::env::set_var(k, v);
        }
        EnvPin { _lock: lock, saved }
    }
}

impl Drop for EnvPin {
    fn drop(&mut self) {
        for (k, v) in self.saved.drain(..).rev() {
            // Still under ENV_LOCK (the lock field drops after this body).
            match v {
                Some(v) => std::env::set_var(&k, v),
                None => std::env::remove_var(&k),
            }
        }
    }
}

/// [`Hermetic::detection_pins`] for a fixture dir of the caller's choice
/// (the smoke run's `--fake-bin`).
pub fn detection_pins_at(bin: &Path) -> serde_json::Value {
    let at = |name: &str| bin.join(name).to_string_lossy().into_owned();
    serde_json::json!({
        "agents": {
            "pathOverride": bin.to_string_lossy(),
            "binaries": {
                "claude": at("claude"),
                "codex": at("codex"),
                "cursorAgent": at("cursor-agent"),
                "agy": at("agy"),
                "gemini": at("gemini"),
            },
        },
        "terminal": { "shell": NO_SHELL },
    })
}

/// Every vendor CLI ([`VENDOR_CLIS`]) this test process could reach outside a
/// live fixture dir (A38.2): in each entry of the **process** `PATH` (what
/// in-process code reads and every child inherits) and in the static
/// detection ladder (`agents::path::static_candidates`, incl.
/// `/opt/homebrew/bin` and `/usr/local/bin`) of the process `HOME` and of
/// each of `homes`. Only `stat`s; nothing runs.
pub fn reachable_vendor_clis(homes: &[PathBuf]) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    let mut all_homes: Vec<PathBuf> = std::env::var_os("HOME").map(PathBuf::from).into_iter().collect();
    all_homes.extend(homes.iter().cloned());
    for home in &all_homes {
        dirs.extend(gilde_code_lib::agents::path::static_candidates(home));
    }
    vendor_clis_in(&dirs)
}

/// The vendor-named files (following symlinks) in `dirs`, skipping the live
/// hermetic fixture dirs. An empty entry is the current directory, as for
/// `exec`.
pub fn vendor_clis_in(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let live: Vec<PathBuf> = LIVE_BIN_DIRS.lock().unwrap_or_else(|p| p.into_inner()).clone();
    let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let live: Vec<PathBuf> = live.iter().flat_map(|d| [d.clone(), canonical(d)]).collect();
    let mut found: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        if live.contains(dir) || live.contains(&canonical(dir)) {
            continue;
        }
        for name in VENDOR_CLIS {
            let candidate = dir.join(name);
            if candidate.is_file() && !found.contains(&candidate) {
                found.push(candidate);
            }
        }
    }
    found
}

fn refusal(found: &[PathBuf]) -> String {
    let list: Vec<String> = found.iter().map(|p| format!("  {}", p.display())).collect();
    format!(
        "hermetic guard (contract §14.1, A38.2): this test process can reach a vendor CLI \
         outside the fixture dirs, which agent detection or a login shell would run:\n{}\n\
         Run cargo through scripts/gates/hermetic-cargo.sh (env -i, a temp HOME, \
         PATH=<cargo bin>:/usr/bin:/bin:/usr/sbin:/sbin, SHELL=/nonexistent).",
        list.join("\n")
    )
}

/// Panic if any entry of `path` other than `allowed_dir` holds a vendor CLI.
pub fn assert_no_vendor_cli_on_path(path: &str, allowed_dir: &Path) {
    for entry in std::env::split_paths(path) {
        if entry == allowed_dir {
            continue;
        }
        for name in VENDOR_CLIS {
            let candidate = entry.join(name);
            assert!(
                !candidate.exists(),
                "hermetic PATH reaches a real vendor CLI: {} (contract §14.1)",
                candidate.display()
            );
        }
    }
}

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
