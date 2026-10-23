//! Shim ↔ app frames (contract §4.6, PaneHost socket).
//!
//! **Included by path, never declared in `contract/mod.rs`**: it is compiled
//! as `crate::panehost::proto::frames` in the app and as `proto::frames` in the
//! `gilde-tmux` binary, which does not link `gilde_code_lib` (§7.4.2). So this
//! file may use only `std`, `serde` and `serde_json`.
//!
//! | Aspect | Rule |
//! |---|---|
//! | Transport | Unix stream socket `<app_data>/run/ph.sock` (≤ 100 bytes) else `/tmp/gilde-<uid>/ph-<8 hex of sha256(app_data)>.sock`; dir 0700, socket 0600 |
//! | Framing | one request per connection: the shim writes one UTF-8 JSON line (≤ 64 KiB) and shuts down its write side; the app writes one JSON line (≤ 1 MiB) and closes. Both ends reject unknown fields |
//! | Auth | peer uid == app uid; constant-time token; `claude-swarm-<pid>` == the lead's pid; kernel peer pid's parent == the lead for mutating subcommands [F-07] |
//! | Versioning | `v` and `build` must equal the app's |
//! | Limits | ≤ 200 requests/min per lead; argv ≤ 64 elements, each ≤ 64 KiB, no NUL |

use serde::{Deserialize, Serialize};

/// The protocol version (`"v":1`).
pub const SHIM_PROTOCOL_V: u32 = 1;
/// Request line cap.
pub const MAX_REQUEST_LINE: usize = 64 * 1024;
/// Response line cap.
pub const MAX_RESPONSE_LINE: usize = 1024 * 1024;
/// argv element count cap.
pub const MAX_ARGV: usize = 64;
/// argv element byte cap.
pub const MAX_ARG_BYTES: usize = 64 * 1024;

/// `{"v":1,"build":"<buildId>","token":"<64 hex>","shimPid":n,"shimPpid":n,
/// "argv":["-L","claude-swarm-123", …],"cwd":"/abs"}`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ShimRequest {
    pub v: u32,
    pub build: String,
    /// 64 lowercase hex chars; never logged.
    pub token: String,
    /// Self-reported; auth uses the kernel peer pid instead (`LOCAL_PEERPID`).
    pub shim_pid: i32,
    pub shim_ppid: i32,
    pub argv: Vec<String>,
    pub cwd: String,
}

/// `{"v":1,"exit":0,"stdout":"%3\n","stderr":""}` — the shim writes `stdout`
/// and `stderr` verbatim and exits with `exit`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ShimResponse {
    pub v: u32,
    pub exit: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Why a request frame is malformed (before any auth or parsing of tmux argv).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    TooLong,
    NotJson,
    Version,
    Token,
    Argv,
    Cwd,
}

impl ShimRequest {
    /// Form checks of §4.6 (`v`, token shape, argv caps, absolute cwd). `build`
    /// equality and auth are the app's (they need its state).
    pub fn check_form(&self) -> Result<(), FrameError> {
        if self.v != SHIM_PROTOCOL_V {
            return Err(FrameError::Version);
        }
        let t = self.token.as_bytes();
        if t.len() != 64 || !t.iter().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b)) {
            return Err(FrameError::Token);
        }
        if self.argv.is_empty()
            || self.argv.len() > MAX_ARGV
            || self
                .argv
                .iter()
                .any(|a| a.len() > MAX_ARG_BYTES || a.contains('\0'))
        {
            return Err(FrameError::Argv);
        }
        if !self.cwd.starts_with('/') || self.cwd.contains('\0') {
            return Err(FrameError::Cwd);
        }
        Ok(())
    }

    /// Parse one request line (without its trailing newline).
    pub fn parse_line(line: &[u8]) -> Result<ShimRequest, FrameError> {
        if line.len() > MAX_REQUEST_LINE {
            return Err(FrameError::TooLong);
        }
        let req: ShimRequest = serde_json::from_slice(line).map_err(|_| FrameError::NotJson)?;
        req.check_form()?;
        Ok(req)
    }
}

impl ShimResponse {
    pub fn ok(stdout: impl Into<String>) -> ShimResponse {
        ShimResponse {
            v: SHIM_PROTOCOL_V,
            exit: 0,
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    /// Exit 1 with a `gilde-tmux:`-prefixed (or tmux-shaped) stderr line.
    pub fn fail(stderr: impl Into<String>) -> ShimResponse {
        ShimResponse {
            v: SHIM_PROTOCOL_V,
            exit: 1,
            stdout: String::new(),
            stderr: stderr.into(),
        }
    }

    /// Any auth failure (§4.6): never a fallback to real tmux afterwards.
    pub fn not_authorized() -> ShimResponse {
        ShimResponse::fail("gilde-tmux: not authorized\n")
    }

    /// `v`/`build` mismatch: the version is not marked failed.
    pub fn version_mismatch() -> ShimResponse {
        ShimResponse::fail("gilde-tmux: Gilde was updated; restart Gilde to use teammate panes\n")
    }
}

/// The stderr texts of §4.6 and §7.4.2 that the shim prints itself.
pub mod messages {
    pub const NO_ANSWER: &str = "gilde-tmux: no answer from Gilde\n";
    pub const BUSY: &str = "gilde-tmux: busy\n";
    pub const UNREACHABLE_NO_TMUX: &str = "gilde-tmux: Gilde is not reachable and tmux is not installed\n";
    pub const NO_REAL_TMUX: &str = "tmux: command not found (gilde-tmux: no real tmux installed)\n";
}
