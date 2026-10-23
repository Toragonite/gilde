//! Shim ↔ app frames (contract §4.6) and the shim's client side (§7.4.2).
//!
//! Owner: T07. The `gilde-tmux` binary includes this file by path and does not
//! link `gilde_code_lib` (§7.4.2), so it may use only `std`, `serde` and
//! `serde_json`. The frame types themselves are the contract's
//! (`contract/shim.rs`, T01, frozen), included here by path.
//!
//! Everything the shim decides lives here rather than in the binary, so the
//! library's unit tests cover it (the binary has `test = false`): the argv
//! routes, the token form, the connect retry of `-V` [N-09] and the one
//! request/response exchange with its deadlines. The app side uses the same
//! line reader and the same constants.

#[path = "../contract/shim.rs"]
mod frames;

pub use frames::*;

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

/// The socket path the core puts into a hosted lead's env (§7.4.1).
pub const ENV_PANE_HOST: &str = "GILDE_PANE_HOST";
/// The per-lead token (64 lowercase hex); never logged.
pub const ENV_PANE_HOST_TOKEN: &str = "GILDE_PANE_HOST_TOKEN";
/// The absolute path of the real tmux, or empty (§7.4.1).
pub const ENV_REAL_TMUX: &str = "GILDE_REAL_TMUX";
/// `probe-log` builds only: the file the shim appends argv + exit code to (LP-10).
pub const ENV_TMUX_LOG: &str = "GILDE_TMUX_LOG";

/// The `-L` socket name prefix of Claude's external swarm mode [R3 §2.1].
pub const SWARM_SOCKET_PREFIX: &str = "claude-swarm-";
/// What `-V` prints when Gilde answers it (the lead checks only the exit code).
pub const TMUX_VERSION_LINE: &str = "tmux 3.4\n";

/// §4.6 timeouts.
pub const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
pub const TOTAL_TIMEOUT: Duration = Duration::from_secs(5);
pub const RESPAWN_TIMEOUT: Duration = Duration::from_secs(10);
/// `-V` retries its connect for up to 1 s: the listener may be restarting
/// (§7.4.2 rule 1, §10.4) [N-09].
pub const VERSION_CONNECT_WINDOW: Duration = Duration::from_secs(1);
pub const VERSION_RETRY_STEP: Duration = Duration::from_millis(50);

/// stderr texts of this implementation that the contract's `messages` does
/// not list. Each starts with `gilde-tmux:` so the lead's error names the cause.
pub mod extra {
    /// A frame the app cannot read (bad JSON, unknown field, caps, NUL).
    pub const BAD_REQUEST: &str = "gilde-tmux: bad request\n";
    /// A swarm call whose argv is not UTF-8 (cannot be framed; never sent).
    pub const NOT_UTF8: &str = "gilde-tmux: argument is not UTF-8\n";
    /// A swarm call whose request line would exceed 64 KiB (never sent).
    pub const REQUEST_TOO_LARGE: &str = "gilde-tmux: request too large\n";
    /// The prefix of the unsupported-command line (§7.4.4 last row).
    pub const UNSUPPORTED_PREFIX: &str = "gilde-tmux: unsupported command: ";
}

/// `Some(pid digits)` when `argv[0..2] == ["-L", "claude-swarm-<digits>"]`
/// (§7.4.2 rule 2). Digits only (at least one); anything else is rule 3.
pub fn swarm_target<S: AsRef<str>>(argv: &[S]) -> Option<&str> {
    if argv.len() < 2 || argv[0].as_ref() != "-L" {
        return None;
    }
    let digits = argv[1].as_ref().strip_prefix(SWARM_SOCKET_PREFIX)?;
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())).then_some(digits)
}

/// `argv == ["-V"]` (§7.4.2 rule 1).
pub fn is_version_query<S: AsRef<str>>(argv: &[S]) -> bool {
    argv.len() == 1 && argv[0].as_ref() == "-V"
}

/// The token form of §4.6: exactly 64 lowercase hex characters.
pub fn is_token_form(token: &str) -> bool {
    token.len() == 64 && token.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// How long the shim waits for the answer to `argv` (§4.6): 10 s for
/// `respawn-pane` (the app answers only once the process exists), else 5 s.
pub fn exchange_timeout<S: AsRef<str>>(argv: &[S]) -> Duration {
    let sub = if swarm_target(argv).is_some() { argv.get(2) } else { argv.first() };
    match sub.map(AsRef::as_ref) {
        Some("respawn-pane") => RESPAWN_TIMEOUT,
        _ => TOTAL_TIMEOUT,
    }
}

/// One connect with a timeout. A Unix-socket connect does not block on macOS
/// (a full backlog answers `ECONNREFUSED`), but the 500 ms bound of §4.6 is
/// kept by construction: the connect runs on a helper thread. A connect that
/// completes after the timeout is dropped unused (the app sees an empty
/// connection and ignores it), so a timed-out connect never carries a request.
pub fn connect_once(path: &Path, timeout: Duration) -> io::Result<UnixStream> {
    let (tx, rx) = std::sync::mpsc::channel();
    let target = path.to_path_buf();
    std::thread::Builder::new()
        .name("gilde-tmux-connect".into())
        .spawn(move || {
            let _ = tx.send(UnixStream::connect(&target));
        })?;
    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "connect timed out")),
    }
}

/// Retry `connect` until it succeeds or `window` has passed, sleeping `step`
/// between attempts. Returns the last result and the number of **retries**
/// (attempts after the first) — each one is counted [N-09].
pub fn connect_retrying<F>(mut connect: F, window: Duration, step: Duration) -> (io::Result<UnixStream>, u32)
where
    F: FnMut() -> io::Result<UnixStream>,
{
    let start = Instant::now();
    let mut retries = 0u32;
    loop {
        match connect() {
            Ok(stream) => return (Ok(stream), retries),
            Err(err) => {
                if start.elapsed() + step > window {
                    return (Err(err), retries);
                }
                std::thread::sleep(step);
                retries += 1;
            }
        }
    }
}

/// Why an exchange failed. `TooLarge` happens before a single byte of the
/// request was written (the caller refuses locally); `NoAnswer` happens after
/// the connect (the caller never re-executes elsewhere, so a pane is never
/// created twice, §4.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExchangeError {
    /// The request line would exceed [`MAX_REQUEST_LINE`].
    TooLarge,
    /// Writing, reading, a timeout, a malformed or over-long answer.
    NoAnswer,
}

/// Encode one request line (without the newline); `TooLarge` past 64 KiB.
pub fn encode_request(req: &ShimRequest) -> Result<Vec<u8>, ExchangeError> {
    let line = serde_json::to_vec(req).map_err(|_| ExchangeError::TooLarge)?;
    if line.len() > MAX_REQUEST_LINE {
        return Err(ExchangeError::TooLarge);
    }
    Ok(line)
}

/// Write `line` + `\n`, shut down the write side, read one response line
/// before `deadline`. Any failure is `NoAnswer`.
pub fn exchange(mut stream: UnixStream, line: &[u8], deadline: Instant) -> Result<ShimResponse, ExchangeError> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(ExchangeError::NoAnswer);
    }
    stream.set_write_timeout(Some(left)).map_err(|_| ExchangeError::NoAnswer)?;
    // The read timeout is set while the connection is certainly open: macOS
    // refuses `SO_RCVTIMEO` (EINVAL) once the peer closed, which a fast
    // answer-and-close from the app would otherwise turn into "no answer".
    stream.set_read_timeout(Some(left)).map_err(|_| ExchangeError::NoAnswer)?;
    let mut framed = Vec::with_capacity(line.len() + 1);
    framed.extend_from_slice(line);
    framed.push(b'\n');
    stream.write_all(&framed).map_err(|_| ExchangeError::NoAnswer)?;
    // The app reads until EOF or the newline; a failed shutdown is harmless.
    let _ = stream.shutdown(Shutdown::Write);
    let raw = read_line_until(&mut stream, MAX_RESPONSE_LINE, deadline).map_err(|_| ExchangeError::NoAnswer)?;
    let resp: ShimResponse = serde_json::from_slice(&raw).map_err(|_| ExchangeError::NoAnswer)?;
    if resp.v != SHIM_PROTOCOL_V {
        return Err(ExchangeError::NoAnswer);
    }
    Ok(resp)
}

/// Why a line could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineError {
    /// More than `cap` bytes before the newline/EOF.
    TooLong,
    /// The deadline passed or the read failed.
    Io,
    /// EOF with nothing read (a connect-only probe or an abandoned connect).
    Empty,
}

/// A reader whose blocking reads can be bounded (a socket; a test double).
pub trait ReadTimeout: Read {
    fn set_deadline(&mut self, left: Duration) -> io::Result<()>;
}

impl ReadTimeout for UnixStream {
    fn set_deadline(&mut self, left: Duration) -> io::Result<()> {
        self.set_read_timeout(Some(left))
    }
}

/// Read one line (up to the first `\n`, or EOF) of at most `cap` bytes,
/// excluding the newline, before `deadline`.
pub fn read_line_until<R: ReadTimeout>(src: &mut R, cap: usize, deadline: Instant) -> Result<Vec<u8>, LineError> {
    let mut out: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(LineError::Io);
        }
        // Best effort: tightening the timeout fails (EINVAL on macOS) only
        // once the peer closed, and then a read returns the buffered bytes
        // and EOF without blocking. Callers set the first timeout themselves.
        let _ = src.set_deadline(left);
        let n = match src.read(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(LineError::Io),
        };
        if n == 0 {
            return if out.is_empty() { Err(LineError::Empty) } else { Ok(out) };
        }
        let chunk = &buf[..n];
        if let Some(pos) = chunk.iter().position(|&b| b == b'\n') {
            if out.len() + pos > cap {
                return Err(LineError::TooLong);
            }
            out.extend_from_slice(&chunk[..pos]);
            return Ok(out);
        }
        if out.len() + n > cap {
            return Err(LineError::TooLong);
        }
        out.extend_from_slice(chunk);
    }
}

/// The exit code the shim exits with for a response (`exit` outside 0..=255
/// is a malformed answer: 1).
pub fn exit_code_of(resp: &ShimResponse) -> i32 {
    if (0..=255).contains(&resp.exit) {
        resp.exit
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn swarm_target_needs_the_exact_two_leading_words() {
        assert_eq!(swarm_target(&["-L", "claude-swarm-14560", "has-session"]), Some("14560"));
        assert_eq!(swarm_target(&["-L", "claude-swarm-1"]), Some("1"));
        assert_eq!(swarm_target(&["-L", "claude-swarm-"]), None);
        assert_eq!(swarm_target(&["-L", "claude-swarm-12a"]), None);
        assert_eq!(swarm_target(&["-L", "claude-swarm--1"]), None);
        assert_eq!(swarm_target(&["-L", "default", "ls"]), None);
        assert_eq!(swarm_target(&["-S", "claude-swarm-1"]), None);
        assert_eq!(swarm_target(&["has-session", "-L", "claude-swarm-1"]), None);
        assert_eq!(swarm_target::<&str>(&[]), None);
    }

    #[test]
    fn version_query_is_exactly_one_word() {
        assert!(is_version_query(&["-V"]));
        assert!(!is_version_query(&["-V", "-V"]));
        assert!(!is_version_query(&["-v"]));
        assert!(!is_version_query::<&str>(&[]));
    }

    #[test]
    fn token_form_is_64_lowercase_hex() {
        assert!(is_token_form(&"a".repeat(64)));
        assert!(is_token_form(&"0123456789abcdef".repeat(4)));
        assert!(!is_token_form(&"A".repeat(64)));
        assert!(!is_token_form(&"a".repeat(63)));
        assert!(!is_token_form(&"a".repeat(65)));
        assert!(!is_token_form(&"g".repeat(64)));
        assert!(!is_token_form(""));
    }

    #[test]
    fn respawn_pane_gets_the_long_timeout() {
        assert_eq!(exchange_timeout(&["-L", "claude-swarm-1", "respawn-pane", "-k"]), RESPAWN_TIMEOUT);
        assert_eq!(exchange_timeout(&["-L", "claude-swarm-1", "kill-pane"]), TOTAL_TIMEOUT);
        assert_eq!(exchange_timeout(&["-V"]), TOTAL_TIMEOUT);
    }

    /// N-09: the connect is retried and every retry is counted.
    #[test]
    fn version_connect_retries_until_it_succeeds_and_counts_each_retry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let mut attempts = 0;
        let (res, retries) = connect_retrying(
            || {
                attempts += 1;
                if attempts <= 3 {
                    Err(io::Error::from(io::ErrorKind::ConnectionRefused))
                } else {
                    UnixStream::connect(&path)
                }
            },
            Duration::from_secs(60),
            Duration::from_millis(1),
        );
        assert!(res.is_ok());
        assert_eq!(retries, 3);
        drop(listener);
    }

    #[test]
    fn version_connect_gives_up_after_its_window() {
        let mut attempts = 0u32;
        let (res, retries) = connect_retrying(
            || {
                attempts += 1;
                Err(io::Error::from(io::ErrorKind::NotFound))
            },
            Duration::from_millis(30),
            Duration::from_millis(5),
        );
        assert!(res.is_err());
        assert_eq!(retries + 1, attempts, "every attempt after the first is a counted retry");
        assert!(retries >= 1);
    }

    #[test]
    fn connect_to_a_missing_socket_fails_fast() {
        let dir = tempfile::tempdir().unwrap();
        let err = connect_once(&dir.path().join("none.sock"), CONNECT_TIMEOUT).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    fn sample_request() -> ShimRequest {
        ShimRequest {
            v: SHIM_PROTOCOL_V,
            build: "1.0.0".into(),
            token: "a".repeat(64),
            shim_pid: 2,
            shim_ppid: 1,
            argv: vec!["-L".into(), "claude-swarm-1".into(), "has-session".into()],
            cwd: "/tmp".into(),
        }
    }

    #[test]
    fn a_request_over_64_kib_is_never_sent() {
        let mut req = sample_request();
        req.argv.push("x".repeat(MAX_REQUEST_LINE));
        assert_eq!(encode_request(&req), Err(ExchangeError::TooLarge));
        assert!(encode_request(&sample_request()).is_ok());
    }

    #[test]
    fn exchange_round_trips_one_line_each_way() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let line = read_line_until(&mut s, MAX_REQUEST_LINE, Instant::now() + Duration::from_secs(30)).unwrap();
            let req = ShimRequest::parse_line(&line).unwrap();
            assert_eq!(req.argv[2], "has-session");
            let mut out = serde_json::to_vec(&ShimResponse::ok("%0\n")).unwrap();
            out.push(b'\n');
            s.write_all(&out).unwrap();
        });
        let stream = UnixStream::connect(&path).unwrap();
        let line = encode_request(&sample_request()).unwrap();
        let resp = exchange(stream, &line, Instant::now() + Duration::from_secs(30)).unwrap();
        assert_eq!(resp, ShimResponse::ok("%0\n"));
        server.join().unwrap();
    }

    /// §4.6: a peer that closes after the request was written is `NoAnswer`
    /// (the caller then never falls back).
    #[test]
    fn a_closed_connection_after_the_write_is_no_answer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let _ = read_line_until(&mut s, MAX_REQUEST_LINE, Instant::now() + Duration::from_secs(30));
        });
        let stream = UnixStream::connect(&path).unwrap();
        let line = encode_request(&sample_request()).unwrap();
        assert_eq!(exchange(stream, &line, Instant::now() + Duration::from_secs(30)), Err(ExchangeError::NoAnswer));
        server.join().unwrap();
    }

    #[test]
    fn malformed_or_foreign_answers_are_no_answer() {
        for answer in [
            &b"not json\n"[..],
            &b"{\"v\":2,\"exit\":0,\"stdout\":\"\",\"stderr\":\"\"}\n"[..],
            &b"{\"v\":1,\"exit\":0,\"stdout\":\"\",\"stderr\":\"\",\"extra\":1}\n"[..],
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("s.sock");
            let listener = UnixListener::bind(&path).unwrap();
            let answer = answer.to_vec();
            let server = std::thread::spawn(move || {
                let (mut s, _) = listener.accept().unwrap();
                let _ = read_line_until(&mut s, MAX_REQUEST_LINE, Instant::now() + Duration::from_secs(30));
                s.write_all(&answer).unwrap();
            });
            let stream = UnixStream::connect(&path).unwrap();
            let line = encode_request(&sample_request()).unwrap();
            assert_eq!(exchange(stream, &line, Instant::now() + Duration::from_secs(30)), Err(ExchangeError::NoAnswer));
            server.join().unwrap();
        }
    }

    struct Chunks(Vec<Vec<u8>>);
    impl Read for Chunks {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.0.is_empty() {
                return Ok(0);
            }
            let c = self.0.remove(0);
            buf[..c.len()].copy_from_slice(&c);
            Ok(c.len())
        }
    }
    impl ReadTimeout for Chunks {
        fn set_deadline(&mut self, _left: Duration) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn line_reader_stops_at_the_newline_and_enforces_the_cap() {
        let far = Instant::now() + Duration::from_secs(60);
        let mut r = Chunks(vec![b"ab".to_vec(), b"c\nd".to_vec()]);
        assert_eq!(read_line_until(&mut r, 10, far).unwrap(), b"abc");
        let mut r = Chunks(vec![b"abc".to_vec()]);
        assert_eq!(read_line_until(&mut r, 10, far).unwrap(), b"abc", "EOF ends a line");
        let mut r = Chunks(vec![b"abcdef".to_vec(), b"ghijkl".to_vec()]);
        assert_eq!(read_line_until(&mut r, 10, far), Err(LineError::TooLong));
        let mut r = Chunks(vec![b"0123456789\n".to_vec()]);
        assert_eq!(read_line_until(&mut r, 10, far).unwrap(), b"0123456789", "exactly the cap is accepted");
        let mut r = Chunks(vec![]);
        assert_eq!(read_line_until(&mut r, 10, far), Err(LineError::Empty));
    }

    #[test]
    fn out_of_range_exit_codes_become_1() {
        let mut r = ShimResponse::ok("");
        assert_eq!(exit_code_of(&r), 0);
        r.exit = 127;
        assert_eq!(exit_code_of(&r), 127);
        r.exit = -1;
        assert_eq!(exit_code_of(&r), 1);
        r.exit = 256;
        assert_eq!(exit_code_of(&r), 1);
    }
}
