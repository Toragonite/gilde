//! `fake_agent` — the fixture binary every supervision / PTY / sidecar test
//! spawns instead of a real agent CLI.
//!
//! Real CLIs are not usable as test subjects: they need credentials, they are
//! not installed on CI, and they cannot be asked to misbehave on demand. This
//! binary is a deterministic stand-in that can be told to misbehave in exactly
//! the ways the supervisor must survive (DESIGN §8): escaping its process
//! group, outliving its parent, ignoring `SIGTERM`, flooding the PTY with
//! non-UTF-8 bytes, or sitting on hundreds of megabytes of RSS.
//!
//! The first argv element selects the mode:
//!
//! | mode                     | behaviour                                                   |
//! |--------------------------|-------------------------------------------------------------|
//! | `normal <code>`          | print a short line, exit `<code>`                            |
//! | `hang`                   | do nothing, live forever                                     |
//! | `escape-group`           | fork a `setsid()` child that `killpg` alone cannot reach     |
//! | `spawn-grandchild <s>`   | fork a stdio-inheriting child that outlives this process     |
//! | `binary-flood <bytes>`   | emit `<bytes>` of hostile, deterministic, non-UTF-8 data     |
//! | `ignore-term`            | ignore `SIGTERM`; only `SIGKILL` ends it                     |
//! | `slow-exit <s>`          | exit `<s>` seconds after receiving `SIGTERM`                 |
//! | `print-env <VAR>`        | print `$VAR` verbatim (used to prove `GILDE_SPAWN_ID` lands)    |
//! | `emit-jsonl <file>`      | replay a captured vendor stream line by line                 |
//! | `alloc <mib> <s>`        | touch `<mib>` MiB of RSS and hold it for `<s>` seconds       |
//! | `claude-tui <scenario>`  | a Claude-like TUI: idle screen, permission / choice / worker |
//! |                          | prompts, trust / theme / browser-extension setup dialogs     |
//! | `tui-idle-ready <ms> <style>` | drops input for `<ms>`, then a claude/codex/cursor/agy  |
//! |                          | ready screen that echoes submitted lines                    |
//! | `term-query`             | DA1 / DSR / XTVERSION queries, prints the replies            |
//! | `teammate --agent-id …`  | a pane teammate: transcript head, argv echo, claude TUI      |
//! | `teammate-lead [...]`    | a hosted lead: the tmux external-mode sequence + team files |
//!
//! The screen modes live in `tui.rs` (screens are a binding interface for
//! Gilde's grid emulator, prompt grammar and ready signatures), the team
//! modes in `team.rs`, the terminal plumbing (raw mode, input log, key
//! decoding, frame rendering) in `term.rs`. `$FAKE_AGENT_INPUT_LOG`, when set,
//! receives every raw byte any of them reads from stdin.
//!
//! Two private modes (`__escaped`, `__sleep`) exist only as re-exec targets for
//! `escape-group` and `spawn-grandchild`.
//!
//! Invariants every mode upholds:
//!   * `SIGKILL` always ends it — no mode blocks in an uninterruptible state;
//!   * stdout carries exactly the bytes the mode promises (raw fd writes, no
//!     `LineWriter` in the binary paths);
//!   * announcements ("ready", pids) are flushed before the mode starts doing
//!     the thing the test is waiting for, so tests never race the setup.

mod flood;
mod team;
mod term;
mod tui;

use std::env;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::mem::ManuallyDrop;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::FromRawFd;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

/// Exit code for "you called me wrong".
const EXIT_USAGE: i32 = 2;
/// Exit code for "the environment did not contain what you asked for".
const EXIT_MISSING_ENV: i32 = 3;
/// Exit code for "I could not write my output".
const EXIT_IO: i32 = 1;

fn main() {
    let argv: Vec<String> = env::args().skip(1).collect();
    let mode = argv.first().map(String::as_str).unwrap_or("");
    let rest: &[String] = if argv.is_empty() { &[] } else { &argv[1..] };

    let code = match mode {
        "normal" => mode_normal(rest),
        "hang" => mode_hang(),
        "escape-group" => mode_escape_group(),
        "spawn-grandchild" => mode_spawn_grandchild(rest),
        "binary-flood" => mode_binary_flood(rest),
        "ignore-term" => mode_ignore_term(),
        "slow-exit" => mode_slow_exit(rest),
        "print-env" => mode_print_env(rest),
        "emit-jsonl" => mode_emit_jsonl(rest),
        "alloc" => mode_alloc(rest),
        "claude-tui" => tui::mode_claude_tui(rest),
        "tui-idle-ready" => tui::mode_tui_idle_ready(rest),
        "term-query" => tui::mode_term_query(),
        "teammate" => team::mode_teammate(rest),
        "teammate-lead" => team::mode_teammate_lead(rest),
        "__escaped" => mode_escaped_child(),
        "__sleep" => mode_sleep(rest),
        other => {
            usage(other);
            EXIT_USAGE
        }
    };
    // Flush explicitly: process::exit does not run destructors.
    let _ = io::stdout().flush();
    let _ = io::stderr().flush();
    std::process::exit(code);
}

// ---------------------------------------------------------------------------
// modes
// ---------------------------------------------------------------------------

/// `normal <code>` — a well-behaved agent: says something, exits.
fn mode_normal(rest: &[String]) -> i32 {
    let code = match parse_i32(rest.first(), "code") {
        Ok(v) => v,
        Err(c) => return c,
    };
    println!("fake_agent normal {code}");
    eprintln!("fake_agent stderr {code}");
    let _ = io::stdout().flush();
    code
}

/// `hang` — the simplest orphan candidate: never exits on its own.
fn mode_hang() -> i32 {
    park_forever()
}

/// `escape-group` — spawns a child that calls `setsid(2)` and therefore leaves
/// this process's process group *and* session. `killpg` on our pgid will not
/// reach it; only a descendant walk (or the `GILDE_SPAWN_ID` env fence) will.
fn mode_escape_group() -> i32 {
    // The escapee detaches its stdio so that killing us closes the pipe: tests
    // can read to EOF without the escapee holding the write end open.
    let child = match spawn_self(&["__escaped"], Detach::Setsid, ChildStdio::Null) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("fake_agent: failed to spawn escapee: {e}");
            return EXIT_IO;
        }
    };
    println!("parent-pid {}", std::process::id());
    println!("escaped-pid {}", child.id());
    let _ = io::stdout().flush();
    park_forever()
}

/// The `setsid()` half of `escape-group`. Never exits on its own.
fn mode_escaped_child() -> i32 {
    park_forever()
}

/// `spawn-grandchild <secs>` — spawns a child that *keeps our process group and
/// our stdio* and lives `<secs>` seconds, then hangs. Killing this process by
/// pid leaves the grandchild running and the stdout pipe open, which is exactly
/// the leak the reaper has to notice.
fn mode_spawn_grandchild(rest: &[String]) -> i32 {
    let secs = match parse_u64(rest.first(), "secs") {
        Ok(v) => v,
        Err(c) => return c,
    };
    let child = match spawn_self(
        &["__sleep", &secs.to_string()],
        Detach::SameGroup,
        ChildStdio::Inherit,
    ) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("fake_agent: failed to spawn grandchild: {e}");
            return EXIT_IO;
        }
    };
    println!("parent-pid {}", std::process::id());
    println!("grandchild-pid {}", child.id());
    let _ = io::stdout().flush();
    park_forever()
}

/// The sleeping half of `spawn-grandchild`.
fn mode_sleep(rest: &[String]) -> i32 {
    let secs = match parse_u64(rest.first(), "secs") {
        Ok(v) => v,
        Err(c) => return c,
    };
    sleep_secs(secs);
    0
}

/// `binary-flood <bytes>` — emits `<bytes>` of deterministic, mostly-random
/// data with invalid UTF-8 throughout and a multi-byte character straddling
/// every 64 KiB boundary. See `flood.rs`.
fn mode_binary_flood(rest: &[String]) -> i32 {
    let total = match parse_u64(rest.first(), "bytes") {
        Ok(v) => v,
        Err(c) => return c,
    };
    // Bypass std's stdout: it is a LineWriter, which would both flush on every
    // 0x0A in the random data and (more importantly) is the wrong abstraction
    // for a byte-fidelity fixture.
    let mut out = raw_stdout();
    match flood::write_flood(&mut *out, total) {
        Ok(()) => 0,
        Err(e) => {
            // A closed pipe is a legitimate way for this mode to end.
            eprintln!("fake_agent: flood write failed: {e}");
            EXIT_IO
        }
    }
}

/// `ignore-term` — installs `SIG_IGN` for `SIGTERM` and announces readiness, so
/// a test can send `SIGTERM` without racing the handler installation.
fn mode_ignore_term() -> i32 {
    // SAFETY: `SIG_IGN` is async-signal-safe by construction; no handler runs.
    unsafe {
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
    }
    println!("ready");
    let _ = io::stdout().flush();
    park_forever()
}

static TERM_SEEN: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigterm(_sig: libc::c_int) {
    // Only an atomic store — async-signal-safe.
    TERM_SEEN.store(true, Ordering::SeqCst);
}

/// `slow-exit <secs>` — the "graceful but slow" shutdown case the TERM→KILL
/// escalation ladder has to time out on (or wait for, depending on the grace).
fn mode_slow_exit(rest: &[String]) -> i32 {
    let secs = match parse_u64(rest.first(), "secs") {
        Ok(v) => v,
        Err(c) => return c,
    };
    // SAFETY: the handler only performs an atomic store.
    unsafe {
        libc::signal(
            libc::SIGTERM,
            on_sigterm as extern "C" fn(libc::c_int) as *const () as libc::sighandler_t,
        );
    }
    println!("ready");
    let _ = io::stdout().flush();
    while !TERM_SEEN.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(20));
    }
    println!("term-received");
    let _ = io::stdout().flush();
    sleep_secs(secs);
    0
}

/// `print-env <VAR>` — writes the raw bytes of `$VAR` (env values are not
/// required to be UTF-8) followed by a newline.
fn mode_print_env(rest: &[String]) -> i32 {
    let Some(name) = rest.first() else {
        eprintln!("fake_agent: print-env needs a variable name");
        return EXIT_USAGE;
    };
    match env::var_os(name) {
        Some(value) => {
            let mut out = raw_stdout();
            if out.write_all(value.as_bytes()).and_then(|()| out.write_all(b"\n")).is_err() {
                return EXIT_IO;
            }
            0
        }
        None => {
            eprintln!("fake_agent: env var {name} is not set");
            EXIT_MISSING_ENV
        }
    }
}

/// `emit-jsonl <file>` — replays a captured vendor stream one line at a time,
/// flushing after each so the consumer sees real streaming. The concatenation
/// of the writes is byte-identical to the file. `FAKE_AGENT_LINE_DELAY_MS`
/// inserts a pause between lines when a test wants to observe intermediate
/// states.
fn mode_emit_jsonl(rest: &[String]) -> i32 {
    let Some(path) = rest.first() else {
        eprintln!("fake_agent: emit-jsonl needs a file path");
        return EXIT_USAGE;
    };
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("fake_agent: cannot open {path}: {e}");
            return EXIT_IO;
        }
    };
    let delay_ms: u64 = env::var("FAKE_AGENT_LINE_DELAY_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let mut reader = BufReader::new(file);
    let mut out = raw_stdout();
    let mut line: Vec<u8> = Vec::with_capacity(8192);
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                eprintln!("fake_agent: read failed: {e}");
                return EXIT_IO;
            }
        }
        if out.write_all(&line).and_then(|()| out.flush()).is_err() {
            return EXIT_IO;
        }
        if delay_ms > 0 {
            thread::sleep(Duration::from_millis(delay_ms));
        }
    }
    0
}

/// `alloc <mib> <secs>` — grows RSS by `<mib>` MiB (every page is written, so
/// the pages are really resident, not lazily zero-mapped) and holds it.
fn mode_alloc(rest: &[String]) -> i32 {
    let mib = match parse_u64(rest.first(), "mib") {
        Ok(v) => v,
        Err(c) => return c,
    };
    let secs = match parse_u64(rest.get(1), "secs") {
        Ok(v) => v,
        Err(c) => return c,
    };
    let bytes = (mib as usize).saturating_mul(1024 * 1024);
    let mut buf = vec![0u8; bytes];
    // Fill every byte with pseudo-random data. A page holding one non-zero byte
    // (what this used to write) is almost all zeros, and macOS's memory
    // compressor shrinks such pages to nearly nothing under pressure: on a
    // machine deep in swap the process then never *shows* the RSS it was asked
    // to hold, and every test that waits for that RSS times out. Incompressible
    // contents make the RSS the test asks for the RSS the kernel reports, on any
    // machine.
    let mut state = 0x9e37_79b9_7f4a_7c15_u64 ^ u64::from(std::process::id());
    for chunk in buf.chunks_mut(8) {
        // xorshift64*: cheap, and nothing a page compressor can fold.
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        let word = state.wrapping_mul(0x2545_f491_4f6c_dd1d).to_le_bytes();
        chunk.copy_from_slice(&word[..chunk.len()]);
    }
    println!("allocated {mib}");
    let _ = io::stdout().flush();
    // Hold it — and keep it in use: one write per page every 100 ms keeps the
    // pages recently used, so the kernel prefers other victims for compression
    // and swap-out, as it would for a live agent's working set.
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    let mut tick = 0u8;
    while std::time::Instant::now() < deadline {
        tick = tick.wrapping_add(1);
        let mut i = 0usize;
        while i < buf.len() {
            buf[i] ^= tick | 1;
            i += 4096;
        }
        std::hint::black_box(&buf);
        thread::sleep(Duration::from_millis(100));
    }
    // Keep the allocation alive across the hold.
    std::hint::black_box(&buf);
    0
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Whether a re-exec'd child leaves our process group.
enum Detach {
    /// `setsid(2)`: new session *and* new process group — escapes `killpg`.
    Setsid,
    /// Inherit our process group — reachable by `killpg`, but not by `kill(pid)`.
    SameGroup,
}

/// Whether a re-exec'd child keeps our stdio.
enum ChildStdio {
    Inherit,
    Null,
}

fn spawn_self(args: &[&str], detach: Detach, stdio: ChildStdio) -> io::Result<Child> {
    let exe: PathBuf = env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.args(args).stdin(Stdio::null());
    match stdio {
        ChildStdio::Inherit => {
            cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());
        }
        ChildStdio::Null => {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }
    if let Detach::Setsid = detach {
        // SAFETY: `pre_exec` runs between fork and exec. `setsid` is
        // async-signal-safe and we allocate nothing here.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    cmd.spawn()
}

/// stdout as a raw, unbuffered `File`. Wrapped in `ManuallyDrop` so that
/// dropping it does not close fd 1.
fn raw_stdout() -> ManuallyDrop<File> {
    // SAFETY: fd 1 is open for the lifetime of the process and the returned
    // handle is never dropped, so ownership is not actually taken.
    ManuallyDrop::new(unsafe { File::from_raw_fd(1) })
}

fn park_forever() -> ! {
    loop {
        thread::sleep(Duration::from_secs(3600));
    }
}

fn sleep_secs(secs: u64) {
    // Sleep in slices so a SIGKILL-less test can still observe progress and so
    // very large values do not overflow the platform sleep.
    let mut left = secs;
    while left > 0 {
        let step = left.min(1);
        thread::sleep(Duration::from_secs(step));
        left -= step;
    }
}

fn parse_i32(arg: Option<&String>, what: &str) -> Result<i32, i32> {
    match arg.map(|s| s.parse::<i32>()) {
        Some(Ok(v)) => Ok(v),
        Some(Err(_)) => {
            eprintln!("fake_agent: <{what}> must be an integer");
            Err(EXIT_USAGE)
        }
        None => {
            eprintln!("fake_agent: missing <{what}>");
            Err(EXIT_USAGE)
        }
    }
}

fn parse_u64(arg: Option<&String>, what: &str) -> Result<u64, i32> {
    match arg.map(|s| s.parse::<u64>()) {
        Some(Ok(v)) => Ok(v),
        Some(Err(_)) => {
            eprintln!("fake_agent: <{what}> must be a non-negative integer");
            Err(EXIT_USAGE)
        }
        None => {
            eprintln!("fake_agent: missing <{what}>");
            Err(EXIT_USAGE)
        }
    }
}

fn usage(bad_mode: &str) {
    if bad_mode.is_empty() {
        eprintln!("fake_agent: no mode given");
    } else {
        eprintln!("fake_agent: unknown mode {bad_mode:?}");
    }
    eprintln!(
        "usage: fake_agent <mode> [args]\n\
         \x20 normal <code>            print a line and exit with <code>\n\
         \x20 hang                     live forever\n\
         \x20 escape-group             spawn a setsid() child that killpg misses\n\
         \x20 spawn-grandchild <secs>  spawn a stdio-inheriting child that outlives us\n\
         \x20 binary-flood <bytes>     emit <bytes> of deterministic non-UTF-8 data\n\
         \x20 ignore-term              ignore SIGTERM; die only on SIGKILL\n\
         \x20 slow-exit <secs>         exit <secs> after SIGTERM\n\
         \x20 print-env <VAR>          print $VAR verbatim\n\
         \x20 emit-jsonl <file>        replay a captured stream line by line\n\
         \x20 alloc <mib> <secs>       hold <mib> MiB of RSS for <secs>\n\
         \x20 claude-tui <scenario> [--work-ms N] [--trust-default no|yes] [--trust-flaky]\n\
         \x20                          idle|permission|choice|worker|trust|theme|chrome|onboarding\n\
         \x20 tui-idle-ready <ms> <style>  claude|codex|cursor|agy ready screen after <ms>\n\
         \x20 term-query               send DA1/DSR/XTVERSION, print the replies, hang\n\
         \x20 teammate --agent-id <n>@<t> --agent-name <n> --team-name <t> [...]\n\
         \x20 teammate-lead [--teammates N] [--team NAME] [--torn-ms MS] [--hold]"
    );
}
