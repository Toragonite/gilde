//! T-shim-* — the `gilde-tmux` binary's routes (contract §7.4.2, §4.6):
//! passthrough of non-swarm argv and `-V` both ways (T-shim-passthrough),
//! connect-failure fallback, no fallback after a written request, the request
//! frame (the core's build id, kernel facts), and the refusals the shim makes
//! by itself. Owner: T07.
//!
//! The "real tmux" is always `fake_real_tmux`; no system tmux is ever run.
//! Every socket lives in the test's own temp dir.

#[path = "fixtures/fake_tmux_client/mod.rs"]
mod fake_tmux_client;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Write;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fake_tmux_client::host::{me, Rig};
use fake_tmux_client::{run, shim_env};
use gilde_code_lib::panehost::proto::{self, messages, ShimRequest};

fn shim() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gilde-tmux"))
}

fn fake_real_tmux() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fake_real_tmux"))
}

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

struct Recorded {
    argv0: Vec<u8>,
    args: Vec<Vec<u8>>,
    env: BTreeMap<String, String>,
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

fn records(path: &Path) -> Vec<Recorded> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    text.lines()
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            Recorded {
                argv0: unhex(v["argv0"].as_str().unwrap()),
                args: v["args"].as_array().unwrap().iter().map(|a| unhex(a.as_str().unwrap())).collect(),
                env: v["env"]
                    .as_object()
                    .unwrap()
                    .iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
                    .collect(),
            }
        })
        .collect()
}

struct Env {
    dir: tempfile::TempDir,
    record: PathBuf,
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let record = std::fs::canonicalize(dir.path()).unwrap().join("rec.jsonl");
        Env { dir, record }
    }

    fn path(&self) -> PathBuf {
        std::fs::canonicalize(self.dir.path()).unwrap()
    }

    /// The shim env with a fake real tmux that records and exits `code`.
    fn with_real(&self, socket: Option<&Path>, token: Option<&str>, code: i32) -> Vec<(String, String)> {
        let mut env = shim_env(socket, token, Some(&fake_real_tmux()), &self.path());
        env.push(("FAKE_REAL_TMUX_RECORD".into(), self.record.to_str().unwrap().into()));
        env.push(("FAKE_REAL_TMUX_EXIT".into(), code.to_string()));
        env.push(("KEEP_ME".into(), "kept".into()));
        env
    }

    fn missing_socket(&self) -> PathBuf {
        self.path().join("missing.sock")
    }
}

/// PM-39: everything that is not `-L claude-swarm-<digits>` reaches the real
/// tmux byte-for-byte (non-UTF-8 included), with its exit code; the token is
/// not handed to the real tmux, every other variable is.
#[test]
fn t_shim_passthrough_non_swarm_argv_byte_for_byte() {
    let e = Env::new();
    let env = e.with_real(Some(&e.missing_socket()), Some(TOKEN), 7);
    let raw = OsString::from_vec(b"\xff\xfe-raw".to_vec());
    let cases: Vec<Vec<OsString>> = vec![
        vec!["new-session".into(), "-s".into(), "x y".into(), "--".into(), "ç".into(), raw.clone()],
        vec!["-L".into(), "default".into(), "ls".into()],
        vec!["-L".into(), "claude-swarm-12x".into(), "ls".into()],
        vec!["load-buffer".into(), "-".into()],
        vec![],
    ];
    for (i, args) in cases.iter().enumerate() {
        let out = run(&shim(), &env, &e.path(), &args[..]);
        assert_eq!(out.code, 7, "case {i}: the real tmux's exit code");
        let recs = records(&e.record);
        let rec = recs.last().expect("the real tmux ran");
        let expect: Vec<Vec<u8>> = args.iter().map(|a| a.clone().into_vec()).collect();
        assert_eq!(rec.args, expect, "case {i}: byte-for-byte");
        assert_eq!(rec.argv0, shim().into_os_string().into_vec(), "argv[0] is passed on");
        assert!(!rec.env.contains_key("GILDE_PANE_HOST_TOKEN"), "the token never reaches the real tmux");
        assert_eq!(rec.env.get("KEEP_ME").map(String::as_str), Some("kept"));
        assert!(rec.env.contains_key("GILDE_PANE_HOST"));
        assert_eq!(records(&e.record).len(), i + 1);
    }
}

/// No real tmux: exit 127 with the shim's own message; an unusable
/// `GILDE_REAL_TMUX` (missing, not executable, relative, the shim itself)
/// counts as none — the shim never execs itself.
#[test]
fn t_shim_passthrough_without_a_real_tmux_exits_127() {
    let e = Env::new();
    let not_exec = e.path().join("not-exec");
    std::fs::write(&not_exec, b"x").unwrap();
    for real in [String::new(), e.path().join("none").display().to_string(), not_exec.display().to_string(), "relative/tmux".into(), shim().display().to_string()] {
        let mut env = shim_env(None, None, None, &e.path());
        env.retain(|(k, _)| k != "GILDE_REAL_TMUX");
        env.push(("GILDE_REAL_TMUX".into(), real.clone()));
        let out = run(&shim(), &env, &e.path(), &["ls"]);
        assert_eq!(out.code, 127, "{real}");
        assert_eq!(out.stderr(), messages::NO_REAL_TMUX, "{real}");
        assert!(out.stdout.is_empty());
    }
}

/// `-V` with a real tmux prints the real version (T-shim-passthrough "both ways").
#[test]
fn t_shim_version_execs_the_real_tmux_when_there_is_one() {
    let e = Env::new();
    let env = e.with_real(Some(&e.missing_socket()), Some(TOKEN), 0);
    let out = run(&shim(), &env, &e.path(), &["-V"]);
    assert_eq!(out.code, 0);
    assert_eq!(out.stdout(), "tmux 9.9-fake\n");
    assert_eq!(records(&e.record)[0].args, vec![b"-V".to_vec()]);
}

/// `-V` without a real tmux: Gilde answers `tmux 3.4` (exit 0).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t_shim_version_is_answered_by_gilde_without_a_real_tmux() {
    let rig = Rig::new(me()).start();
    let env = shim_env(Some(&rig.socket()), Some(&rig.token), None, &rig.root);
    let out = run(&rig.shim(), &env, &rig.root, &["-V"]);
    assert_eq!((out.code, out.stdout(), out.stderr()), (0, "tmux 3.4\n".to_string(), String::new()));
}

/// F-36: Gilde unreachable (after the 1 s connect retry) and no real tmux →
/// exit 1, so the CLI falls back to in-process teammates at once.
#[test]
fn t_shim_version_fails_when_neither_gilde_nor_tmux_is_there() {
    let e = Env::new();
    let env = shim_env(Some(&e.missing_socket()), Some(TOKEN), None, &e.path());
    let out = run(&shim(), &env, &e.path(), &["-V"]);
    assert_eq!(out.code, 1);
    assert_eq!(out.stderr(), messages::UNREACHABLE_NO_TMUX);
    let env = shim_env(None, None, None, &e.path());
    let out = run(&shim(), &env, &e.path(), &["-V"]);
    assert_eq!(out.code, 1);
}

/// §7.4.2 rule 2: a connect failure (nothing written) runs stock external
/// mode through the real tmux; without one, exit 1 with the shim's message.
#[test]
fn t_shim_connect_failure_falls_back_to_the_real_tmux() {
    let e = Env::new();
    let args = ["-L", "claude-swarm-4242", "has-session", "-t", "claude-swarm"];
    let env = e.with_real(Some(&e.missing_socket()), Some(TOKEN), 3);
    let out = run(&shim(), &env, &e.path(), &args);
    assert_eq!(out.code, 3);
    let rec = records(&e.record);
    assert_eq!(rec.len(), 1);
    assert_eq!(rec[0].args, args.iter().map(|a| a.as_bytes().to_vec()).collect::<Vec<_>>());

    let env = shim_env(Some(&e.missing_socket()), Some(TOKEN), None, &e.path());
    let out = run(&shim(), &env, &e.path(), &args);
    assert_eq!(out.code, 1);
    assert_eq!(out.stderr(), messages::UNREACHABLE_NO_TMUX);
}

/// Without `GILDE_PANE_HOST` a swarm call is plain passthrough (rule 3).
#[test]
fn t_shim_swarm_calls_without_a_pane_host_pass_through() {
    let e = Env::new();
    let env = e.with_real(None, None, 0);
    let out = run(&shim(), &env, &e.path(), &["-L", "claude-swarm-1", "has-session", "-t", "claude-swarm"]);
    assert_eq!(out.code, 0);
    assert_eq!(records(&e.record).len(), 1);
}

/// A fake app that reads the request and then misbehaves.
fn fake_app(path: PathBuf, behaviour: &'static str) -> std::thread::JoinHandle<ShimRequest> {
    let listener = UnixListener::bind(&path).unwrap();
    std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let line = proto::read_line_until(&mut s, proto::MAX_REQUEST_LINE, Instant::now() + Duration::from_secs(30)).unwrap();
        let req = ShimRequest::parse_line(&line).unwrap();
        match behaviour {
            "close" => drop(s),
            "garbage" => {
                s.write_all(b"{\"v\":1,\"exit\":0}\n").unwrap();
            }
            "hold" => {
                // Answer nothing until the shim gave up.
                let _ = proto::read_line_until(&mut s, 16, Instant::now() + Duration::from_secs(30));
            }
            _ => unreachable!(),
        }
        req
    })
}

/// §4.6: once the request is written, nothing is re-executed elsewhere — a
/// closed connection, a malformed answer or no answer within 5 s is exit 1
/// `no answer from Gilde`, and the real tmux never runs (a pane is never
/// created twice). The request carries the core's build id and the shim's
/// own pid/ppid and cwd.
#[test]
fn t_shim_no_fallback_after_a_written_request() {
    for behaviour in ["close", "garbage", "hold"] {
        let e = Env::new();
        let sock = e.path().join("app.sock");
        let app = fake_app(sock.clone(), behaviour);
        let env = e.with_real(Some(&sock), Some(TOKEN), 0);
        let args = ["-L", "claude-swarm-4242", "split-window", "-d", "-t", "%0", "-h", "-P", "-F", "#{pane_id}", "--", "cat"];
        let out = run(&shim(), &env, &e.path(), &args);
        assert_eq!(out.code, 1, "{behaviour}");
        assert_eq!(out.stderr(), messages::NO_ANSWER, "{behaviour}");
        assert!(records(&e.record).is_empty(), "{behaviour}: the real tmux never ran");
        let req = app.join().unwrap();
        assert_eq!(req.build, gilde_code_lib::BUILD_ID, "A12.7: the shim carries the core's build id");
        assert_eq!(req.token, TOKEN);
        assert_eq!(req.argv, args.iter().map(|a| a.to_string()).collect::<Vec<_>>());
        assert_eq!(req.shim_ppid, me(), "the shim's parent is the caller");
        assert_ne!(req.shim_pid, me());
        assert_eq!(req.cwd, e.path().to_str().unwrap());
        assert_eq!(req.v, proto::SHIM_PROTOCOL_V);
    }
}

/// A missing or malformed token, non-UTF-8 swarm argv and an over-long
/// request are refused by the shim itself: exit 1, nothing sent, and never a
/// fallback to the real tmux.
#[test]
fn t_shim_local_refusals_never_fall_back() {
    let e = Env::new();
    let sock = e.path().join("absent.sock");
    let swarm = ["-L", "claude-swarm-4242", "has-session", "-t", "claude-swarm"];
    for token in [None, Some("short"), Some(&TOKEN.to_uppercase()[..])] {
        let env = e.with_real(Some(&sock), token, 0);
        let out = run(&shim(), &env, &e.path(), &swarm);
        assert_eq!((out.code, out.stderr()), (1, "gilde-tmux: not authorized\n".to_string()), "{token:?}");
    }
    let env = e.with_real(Some(&sock), Some(TOKEN), 0);
    let raw = [
        OsString::from("-L"),
        OsString::from("claude-swarm-4242"),
        OsString::from("select-pane"),
        OsString::from("-T"),
        OsString::from_vec(b"\xff".to_vec()),
    ];
    let out = run(&shim(), &env, &e.path(), &raw[..]);
    assert_eq!((out.code, out.stderr()), (1, proto::extra::NOT_UTF8.to_string()));
    let big = "x".repeat(70 * 1024);
    let out = run(&shim(), &env, &e.path(), &["-L", "claude-swarm-4242", "respawn-pane", "-k", "-t", "%0", "--", &big]);
    assert_eq!((out.code, out.stderr()), (1, proto::extra::REQUEST_TOO_LARGE.to_string()));
    assert!(records(&e.record).is_empty(), "never a fallback after a refusal");
}
