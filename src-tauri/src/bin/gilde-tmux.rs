//! `gilde-tmux` — the shim a hosted Claude lead runs as `tmux` (contract §7.4.2).
//!
//! Owner: T07. It includes only `src/panehost/proto.rs` by path (std + serde +
//! serde_json) and never links `gilde_code_lib`. It holds no state, parses no
//! tmux semantics (PaneHost does, §7.4.4), spawns nothing and only ever
//! `execv`s the real tmux. Dispatch on its own argv (after `argv[0]`):
//!
//! 1. exactly `["-V"]` → the real tmux's `-V` when `GILDE_REAL_TMUX` is a file;
//!    else Gilde answers (`tmux 3.4`), the connect retried for up to 1 s
//!    [N-09]; Gilde unreachable and no real tmux → exit 1 [F-36];
//! 2. `["-L", "claude-swarm-<digits>", …]` with `GILDE_PANE_HOST` set → one
//!    request to the app (§4.6); a connect failure execs the real tmux (or
//!    exits 1 without one); **once the request is written nothing is ever
//!    re-executed elsewhere** (a pane is never created twice);
//! 3. anything else → the real tmux byte-for-byte, else exit 127.
//!
//! The per-lead token is removed from the environment of the real tmux it
//! execs (the real server would otherwise hand it to every pane it starts);
//! argv passes byte-for-byte. The exec is a direct `execve(2)` (the `libc`
//! crate, this one call): the shim replaces its own image and never creates a
//! process, so it is not a §9.3 launch site. The build id is the core's rule (A8.1/A12.7):
//! the compile-time `GILDE_BUILD_ID` when set and non-empty, else the version
//! — the same constant as `gilde_code_lib::BUILD_ID`, never read at run time.

#[path = "../panehost/proto.rs"]
#[allow(dead_code, unused_imports)]
mod proto;

use std::ffi::{CString, OsStr, OsString};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

use proto::{extra, messages, ShimRequest, ShimResponse};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The build id every request carries (§4.7, A8.1, A12.7).
const BUILD_ID: &str = build_id_from(option_env!("GILDE_BUILD_ID"), VERSION);

const _: () = assert!(
    is_valid_build_id(BUILD_ID),
    "GILDE_BUILD_ID must match ^[0-9A-Za-z][0-9A-Za-z._+-]{{0,63}}$ (it names the shim/<buildId> directory)"
);

/// A8.1: an empty value is unset; never trimmed or replaced. (The shim cannot
/// link the core, so the two `const fn`s of `lib.rs` are repeated here; a
/// `panehost_shim` test asserts the shim's request carries the core's id.)
const fn build_id_from<'a>(env: Option<&'a str>, version: &'a str) -> &'a str {
    match env {
        Some(id) if !id.is_empty() => id,
        _ => version,
    }
}

/// A8.1: `^[0-9A-Za-z][0-9A-Za-z._+-]{0,63}$`.
const fn is_valid_build_id(id: &str) -> bool {
    let b = id.as_bytes();
    if b.is_empty() || b.len() > 64 || !b[0].is_ascii_alphanumeric() {
        return false;
    }
    let mut i = 1;
    while i < b.len() {
        let c = b[i];
        if !(c.is_ascii_alphanumeric() || c == b'.' || c == b'_' || c == b'+' || c == b'-') {
            return false;
        }
        i += 1;
    }
    true
}

/// What the shim does in the end.
enum Action {
    /// Print and exit.
    Exit { code: i32, stdout: String, stderr: String },
    /// `execv` the real tmux; if that fails, print `fail_msg` and exit `fail_code`.
    Exec { real: PathBuf, fail_code: i32, fail_msg: &'static str },
}

struct Outcome {
    /// For the `probe-log` line only.
    route: &'static str,
    retries: u32,
    action: Action,
}

impl Outcome {
    fn fail(route: &'static str, code: i32, msg: &str) -> Outcome {
        Outcome {
            route,
            retries: 0,
            action: Action::Exit { code, stdout: String::new(), stderr: msg.to_string() },
        }
    }

    fn exec(route: &'static str, real: PathBuf, fail_code: i32, fail_msg: &'static str) -> Outcome {
        Outcome { route, retries: 0, action: Action::Exec { real, fail_code, fail_msg } }
    }

    fn answered(route: &'static str, resp: ShimResponse) -> Outcome {
        let code = proto::exit_code_of(&resp);
        Outcome {
            route,
            retries: 0,
            action: Action::Exit { code, stdout: resp.stdout, stderr: resp.stderr },
        }
    }
}

fn main() {
    let started = Instant::now();
    let mut args = std::env::args_os();
    let argv0 = args.next().unwrap_or_else(|| OsString::from("tmux"));
    let rest: Vec<OsString> = args.collect();

    let outcome = decide(&rest, started);
    match outcome.action {
        Action::Exit { code, stdout, stderr } => {
            probe_log(&rest, outcome.route, Some(code), outcome.retries, started);
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(stdout.as_bytes());
            let _ = out.flush();
            let mut err = std::io::stderr().lock();
            let _ = err.write_all(stderr.as_bytes());
            let _ = err.flush();
            std::process::exit(code);
        }
        Action::Exec { real, fail_code, fail_msg } => {
            probe_log(&rest, outcome.route, None, outcome.retries, started);
            let _err = exec_real(&real, &argv0, &rest);
            // Only reached when the exec failed.
            let _ = std::io::stderr().write_all(fail_msg.as_bytes());
            std::process::exit(fail_code);
        }
    }
}

fn decide(rest: &[OsString], started: Instant) -> Outcome {
    let real = real_tmux();
    let host = std::env::var_os(proto::ENV_PANE_HOST)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    let words: Option<Vec<&str>> = rest.iter().map(|a| a.to_str()).collect();

    // Rule 1: `-V`.
    if words.as_deref().is_some_and(proto::is_version_query) {
        if let Some(real) = real {
            return Outcome::exec("version-real", real, 1, messages::UNREACHABLE_NO_TMUX);
        }
        let Some(host) = host else {
            return Outcome::fail("version", 1, messages::UNREACHABLE_NO_TMUX);
        };
        return forward_version(&host, started);
    }

    // Rule 2: `-L claude-swarm-<digits> …` with GILDE_PANE_HOST set.
    let leading: Option<Vec<&str>> = rest.iter().take(2).map(|a| a.to_str()).collect();
    let is_swarm = leading.as_deref().is_some_and(|l| proto::swarm_target(l).is_some());
    if is_swarm {
        if let Some(host) = host {
            let Some(words) = words else {
                return Outcome::fail("host", 1, extra::NOT_UTF8);
            };
            let argv: Vec<String> = words.iter().map(|w| w.to_string()).collect();
            return forward(&host, argv, real, started);
        }
    }

    // Rule 3: everything else passes through byte-for-byte.
    match real {
        Some(real) => Outcome::exec("passthrough", real, 127, messages::NO_REAL_TMUX),
        None => Outcome::fail("passthrough-unavailable", 127, messages::NO_REAL_TMUX),
    }
}

/// The token from the env, if it has the §4.6 form.
fn token() -> Option<String> {
    std::env::var(proto::ENV_PANE_HOST_TOKEN)
        .ok()
        .filter(|t| proto::is_token_form(t))
}

fn request(token: String, argv: Vec<String>) -> ShimRequest {
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .filter(|s| s.starts_with('/') && !s.contains('\0'))
        .unwrap_or_else(|| "/".to_string());
    ShimRequest {
        v: proto::SHIM_PROTOCOL_V,
        build: BUILD_ID.to_string(),
        token,
        shim_pid: std::process::id() as i32,
        shim_ppid: std::os::unix::process::parent_id() as i32,
        argv,
        cwd,
    }
}

fn forward(host: &Path, argv: Vec<String>, real: Option<PathBuf>, started: Instant) -> Outcome {
    // A missing or malformed token is a refusal: never a fallback afterwards.
    let Some(token) = token() else {
        return Outcome::answered("host", ShimResponse::not_authorized());
    };
    let deadline = started + proto::exchange_timeout(&argv);
    let req = request(token, argv);
    let line = match proto::encode_request(&req) {
        Ok(line) => line,
        Err(_) => return Outcome::fail("host", 1, extra::REQUEST_TOO_LARGE),
    };
    let stream = match proto::connect_once(host, proto::CONNECT_TIMEOUT) {
        Ok(stream) => stream,
        // Nothing was written: stock external mode through the real tmux.
        Err(_) => {
            return match real {
                Some(real) => Outcome::exec("fallback", real, 1, messages::UNREACHABLE_NO_TMUX),
                None => Outcome::fail("unreachable", 1, messages::UNREACHABLE_NO_TMUX),
            }
        }
    };
    match proto::exchange(stream, &line, deadline) {
        Ok(resp) => Outcome::answered("host", resp),
        Err(_) => Outcome::fail("no-answer", 1, messages::NO_ANSWER),
    }
}

fn forward_version(host: &Path, started: Instant) -> Outcome {
    let Some(token) = token() else {
        return Outcome::answered("version", ShimResponse::not_authorized());
    };
    let (conn, retries) = proto::connect_retrying(
        || proto::connect_once(host, proto::CONNECT_TIMEOUT),
        proto::VERSION_CONNECT_WINDOW,
        proto::VERSION_RETRY_STEP,
    );
    let mut outcome = match conn {
        // Gilde unreachable and no real tmux: let the CLI fall back to its own
        // in-process mode now rather than fail later on `has-session` [F-36].
        Err(_) => Outcome::fail("version", 1, messages::UNREACHABLE_NO_TMUX),
        Ok(stream) => {
            let argv = vec!["-V".to_string()];
            let deadline = Instant::now().max(started) + proto::TOTAL_TIMEOUT;
            let req = request(token, argv);
            match proto::encode_request(&req).and_then(|line| proto::exchange(stream, &line, deadline)) {
                Ok(resp) => Outcome::answered("version", resp),
                Err(_) => Outcome::fail("no-answer", 1, messages::NO_ANSWER),
            }
        }
    };
    outcome.retries = retries;
    outcome
}

/// `GILDE_REAL_TMUX` when it names an executable regular file that is not this
/// shim itself (a misconfigured env must never make the shim exec itself).
fn real_tmux() -> Option<PathBuf> {
    let raw = std::env::var_os(proto::ENV_REAL_TMUX)?;
    if raw.is_empty() {
        return None;
    }
    let path = PathBuf::from(raw);
    if !path.is_absolute() {
        return None;
    }
    let meta = std::fs::metadata(&path).ok()?;
    if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
        return None;
    }
    let target = std::fs::canonicalize(&path).ok()?;
    let me = std::env::current_exe().ok().and_then(|e| std::fs::canonicalize(e).ok());
    if me.as_ref() == Some(&target) {
        return None;
    }
    Some(path)
}

/// `execve` the real tmux with our own `argv[0]`, the rest byte-for-byte and
/// our environment minus the token. SIGPIPE goes back to its default first
/// (the Rust runtime ignores it, and an ignored signal survives exec).
/// Returns only on failure.
fn exec_real(real: &Path, argv0: &OsStr, rest: &[OsString]) -> std::io::Error {
    let invalid = || std::io::Error::from(std::io::ErrorKind::InvalidInput);
    let Ok(path) = CString::new(real.as_os_str().as_bytes()) else { return invalid() };
    let mut argv: Vec<CString> = Vec::with_capacity(rest.len() + 1);
    for a in std::iter::once(argv0).chain(rest.iter().map(OsString::as_os_str)) {
        match CString::new(a.as_bytes()) {
            Ok(c) => argv.push(c),
            Err(_) => return invalid(),
        }
    }
    let mut envp: Vec<CString> = Vec::new();
    for (k, v) in std::env::vars_os() {
        if k == proto::ENV_PANE_HOST_TOKEN {
            continue;
        }
        let mut kv = k.as_bytes().to_vec();
        kv.push(b'=');
        kv.extend_from_slice(v.as_bytes());
        if let Ok(c) = CString::new(kv) {
            envp.push(c);
        }
    }
    let mut argv_ptrs: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
    argv_ptrs.push(std::ptr::null());
    let mut envp_ptrs: Vec<*const libc::c_char> = envp.iter().map(|e| e.as_ptr()).collect();
    envp_ptrs.push(std::ptr::null());
    // SAFETY: every pointer refers to a NUL-terminated CString that outlives
    // the call, and both arrays end with a null pointer, as execve(2) needs.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        libc::execve(path.as_ptr(), argv_ptrs.as_ptr(), envp_ptrs.as_ptr());
    }
    std::io::Error::last_os_error()
}

/// `probe-log` builds only (never release, §4.6): one JSON line per call with
/// argv, route, exit code and connect retries, appended to `GILDE_TMUX_LOG`
/// (an absolute path; anything else logs nothing).
#[cfg(feature = "probe-log")]
fn probe_log(argv: &[OsString], route: &str, code: Option<i32>, retries: u32, started: Instant) {
    let Some(path) = std::env::var_os(proto::ENV_TMUX_LOG).map(PathBuf::from) else {
        return;
    };
    if !path.is_absolute() {
        return;
    }
    let args: Vec<String> = argv.iter().map(|a| a.to_string_lossy().into_owned()).collect();
    let line = serde_json::json!({
        "argv": args,
        "route": route,
        "exit": code,
        "retries": retries,
        "pid": std::process::id(),
        "ppid": std::os::unix::process::parent_id(),
        "ms": started.elapsed().as_secs_f64() * 1000.0,
    });
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "{line}");
    }
}

#[cfg(not(feature = "probe-log"))]
fn probe_log(_argv: &[OsString], _route: &str, _code: Option<i32>, _retries: u32, _started: Instant) {}
