//! `fake_claude` — the scenario-driven stream-json peer of contract §14.1.
//!
//! Owner: T05. Behind the `test-fixtures` feature; never shipped. It never
//! talks to any network or vendor service and writes only to the paths a test
//! gives it (`FAKE_CLAUDE_LOG`, and — for `writeRegistry`/`appendTranscript` —
//! below `CLAUDE_CONFIG_DIR`, which must be set: the default account's
//! `~/.claude` is never touched).
//!
//! ```text
//! fake_claude [any claude argv]          # --version prints the scenario version
//! env FAKE_CLAUDE_SCENARIO=<scenario.json>
//!     FAKE_CLAUDE_LOG=<log.jsonl>        # optional: argv, env markers, every stdin frame
//! ```
//!
//! A scenario is `{"version": "2.1.306", "rejectFlag": "--x"?, "eofExit": 0,
//! "steps": [ … ]}`; each step has one action:
//!
//! | step | effect |
//! |---|---|
//! | `{"expect": {"/ptr": value \| "*"}, "as": "name", "timeoutMs": n}` | wait for a stdin frame matching every pointer (`"*"` = present); frames already received but not consumed are searched first, in order |
//! | `{"expectEof": true, "timeoutMs": n}` | wait until stdin closes |
//! | `{"emit": <json>}` | one stdout line; strings `${name/ptr}` take a captured frame's value (a whole-string placeholder keeps the JSON type); `${pid}`, `${sessionId}` (from `--session-id`/`--resume`) |
//! | `{"emitCaptured": "name"}` | echo a captured frame verbatim (the `--replay-user-messages` echo) |
//! | `{"emitRaw": "text"}` / `{"emitPartial": "text"}` | a raw line / bytes without a newline |
//! | `{"emitBig": n}` | one valid JSON line of about `n` bytes, streamed in chunks |
//! | `{"emitDeep": n}` | a frame nested `n` levels deep |
//! | `{"stderr": "text"}` | a line on stderr |
//! | `{"sleepMs": n}` | sleep |
//! | `{"exitWith": code}` | exit now |
//! | `{"writeRegistry": <json>}` | `<CLAUDE_CONFIG_DIR>/sessions/<pid>.json` |
//! | `{"appendTranscript": {"slug": s, "row": <json>}}` | `<CLAUDE_CONFIG_DIR>/projects/<slug>/<sessionId>.jsonl` |
//! | `{"touchFile": "/abs/path"}` | create/overwrite a file the test named (the settings-file guard) |
//! | `{"assertOpen": true}` | fail (exit 98) if stdin was already closed — e.g. Gilde closed it before the turn's `result` |
//! | `{"ignoreTerm": true}` | ignore SIGTERM from now on (the survivor path; the fake still ends by its own steps) |
//!
//! Scenario option `"pauseAfter": n`: stdin is no longer read after `n` lines
//! (the CLI "stops reading"; stdin backpressure).
//! After the last step the fake keeps reading (and logging) stdin until EOF,
//! then exits with `eofExit` (default 0) — like an idle CLI on stdin EOF.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const EXPECT_DEFAULT_MS: u64 = 10_000;

struct Fake {
    rx: Receiver<Option<String>>,
    backlog: VecDeque<Value>,
    eof: bool,
    captured: HashMap<String, Value>,
    log: Option<std::fs::File>,
    session_id: Option<String>,
}

fn die(code: i32, msg: &str) -> ! {
    eprintln!("fake_claude: {msg}");
    std::process::exit(code);
}

fn out_line(s: &str) {
    let stdout = std::io::stdout();
    let mut o = stdout.lock();
    if o.write_all(s.as_bytes()).and_then(|_| o.write_all(b"\n")).and_then(|_| o.flush()).is_err() {
        std::process::exit(0);
    }
}

impl Fake {
    fn log(&mut self, v: Value) {
        if let Some(f) = self.log.as_mut() {
            let _ = writeln!(f, "{v}");
            let _ = f.flush();
        }
    }

    /// Pull one line from stdin (blocking up to `timeout`); `false` on timeout or EOF.
    fn pull(&mut self, timeout: Duration) -> bool {
        if self.eof {
            return false;
        }
        match self.rx.recv_timeout(timeout) {
            Ok(Some(line)) => {
                let v: Value = serde_json::from_str(&line).unwrap_or_else(|_| json!({ "raw": line }));
                self.log(json!({ "in": v.clone() }));
                self.backlog.push_back(v);
                true
            }
            Ok(None) | Err(RecvTimeoutError::Disconnected) => {
                self.eof = true;
                self.log(json!({ "eof": true }));
                false
            }
            Err(RecvTimeoutError::Timeout) => false,
        }
    }

    /// Read whatever stdin already delivered, without waiting.
    fn drain_now(&mut self) {
        loop {
            match self.rx.try_recv() {
                Ok(Some(line)) => {
                    let v: Value = serde_json::from_str(&line).unwrap_or_else(|_| json!({ "raw": line }));
                    self.log(json!({ "in": v.clone() }));
                    self.backlog.push_back(v);
                }
                Ok(None) | Err(mpsc::TryRecvError::Disconnected) => {
                    if !self.eof {
                        self.eof = true;
                        self.log(json!({ "eof": true }));
                    }
                    return;
                }
                Err(mpsc::TryRecvError::Empty) => return,
            }
        }
    }

    fn matches(frame: &Value, shape: &serde_json::Map<String, Value>) -> bool {
        shape.iter().all(|(ptr, want)| match frame.pointer(ptr) {
            Some(got) => want == "*" || got == want,
            None => false,
        })
    }

    fn expect(&mut self, shape: &serde_json::Map<String, Value>, timeout: Duration, step: usize) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(pos) = self.backlog.iter().position(|f| Fake::matches(f, shape)) {
                return self.backlog.remove(pos).expect("in range");
            }
            if self.eof {
                die(96, &format!("stdin closed while expecting step {step}: {}", Value::Object(shape.clone())));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                die(97, &format!("expect timeout at step {step}: {}", Value::Object(shape.clone())));
            }
            self.pull(left);
        }
    }

    fn template(&self, v: &Value) -> Value {
        match v {
            Value::String(s) => self.template_str(s),
            Value::Array(a) => Value::Array(a.iter().map(|x| self.template(x)).collect()),
            Value::Object(o) => Value::Object(o.iter().map(|(k, x)| (k.clone(), self.template(x))).collect()),
            other => other.clone(),
        }
    }

    fn lookup(&self, key: &str) -> Value {
        match key {
            "pid" => json!(std::process::id()),
            "sessionId" => json!(self.session_id.clone().unwrap_or_default()),
            _ => {
                let (name, ptr) = match key.find('/') {
                    Some(i) => (&key[..i], &key[i..]),
                    None => (key, ""),
                };
                let frame = self.captured.get(name).unwrap_or_else(|| die(95, &format!("no captured frame {name}")));
                frame.pointer(ptr).cloned().unwrap_or(Value::Null)
            }
        }
    }

    fn template_str(&self, s: &str) -> Value {
        if let Some(inner) = s.strip_prefix("${").and_then(|r| r.strip_suffix('}')) {
            if !inner.contains("${") {
                return self.lookup(inner);
            }
        }
        let mut out = String::new();
        let mut rest = s;
        while let Some(start) = rest.find("${") {
            out.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            let Some(end) = after.find('}') else {
                out.push_str(&rest[start..]);
                return Value::String(out);
            };
            match self.lookup(&after[..end]) {
                Value::String(x) => out.push_str(&x),
                other => out.push_str(&other.to_string()),
            }
            rest = &after[end + 1..];
        }
        out.push_str(rest);
        Value::String(out)
    }
}

fn config_dir() -> std::path::PathBuf {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(d) if !d.is_empty() => std::path::PathBuf::from(d),
        _ => die(3, "writeRegistry/appendTranscript need CLAUDE_CONFIG_DIR (the default account is never touched)"),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let scenario: Value = match std::env::var_os("FAKE_CLAUDE_SCENARIO") {
        Some(p) => {
            let text = std::fs::read_to_string(&p).unwrap_or_else(|e| die(2, &format!("cannot read the scenario: {e}")));
            serde_json::from_str(&text).unwrap_or_else(|e| die(2, &format!("bad scenario: {e}")))
        }
        None => json!({ "steps": [] }),
    };
    let version = scenario.get("version").and_then(Value::as_str).unwrap_or("2.1.306").to_owned();
    if args.iter().any(|a| a == "--version") {
        println!("{version} (Claude Code)");
        return;
    }
    if let Some(flag) = scenario.get("rejectFlag").and_then(Value::as_str) {
        if args.iter().any(|a| a == flag) {
            eprintln!("error: unknown option '{flag}'");
            std::process::exit(1);
        }
    }
    let arg_after = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let session_id = arg_after("--session-id").or_else(|| arg_after("--resume"));

    let log = std::env::var_os("FAKE_CLAUDE_LOG").map(|p| {
        std::fs::OpenOptions::new().create(true).append(true).open(p).unwrap_or_else(|e| die(2, &format!("log: {e}")))
    });
    let (tx, rx) = mpsc::channel::<Option<String>>();
    let pause_after = scenario.get("pauseAfter").and_then(Value::as_u64);
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut n = 0u64;
        for line in stdin.lock().lines() {
            match line {
                Ok(l) => {
                    if tx.send(Some(l)).is_err() {
                        return;
                    }
                    n += 1;
                    if pause_after.is_some_and(|p| n >= p) {
                        // Stop reading for good: the pipe fills up behind us.
                        loop {
                            std::thread::park();
                        }
                    }
                }
                Err(_) => break,
            }
        }
        let _ = tx.send(None);
    });
    let mut fake = Fake { rx, backlog: VecDeque::new(), eof: false, captured: HashMap::new(), log, session_id };
    let env_seen: serde_json::Map<String, Value> = std::env::vars()
        .filter(|(k, _)| k.starts_with("CLAUDE_CODE_") || k == "GILDE_SPAWN_ID" || k == "DISABLE_AUTOUPDATER" || k == "CLAUDE_CONFIG_DIR")
        .map(|(k, v)| (k, Value::String(v)))
        .collect();
    fake.log(json!({ "argv": args, "env": env_seen, "pid": std::process::id() }));

    let steps = scenario.get("steps").and_then(Value::as_array).cloned().unwrap_or_default();
    for (i, step) in steps.iter().enumerate() {
        let timeout = Duration::from_millis(step.get("timeoutMs").and_then(Value::as_u64).unwrap_or(EXPECT_DEFAULT_MS));
        if let Some(shape) = step.get("expect").and_then(Value::as_object) {
            let f = fake.expect(shape, timeout, i);
            if let Some(name) = step.get("as").and_then(Value::as_str) {
                fake.captured.insert(name.to_owned(), f);
            }
        } else if step.get("expectEof").is_some() {
            let deadline = Instant::now() + timeout;
            while !fake.eof {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    die(97, &format!("expectEof timeout at step {i}"));
                }
                fake.pull(left);
            }
        } else if let Some(frame) = step.get("emit") {
            out_line(&fake.template(frame).to_string());
        } else if let Some(name) = step.get("emitCaptured").and_then(Value::as_str) {
            let f = fake.captured.get(name).cloned().unwrap_or_else(|| die(95, &format!("no captured frame {name}")));
            out_line(&f.to_string());
        } else if let Some(raw) = step.get("emitRaw").and_then(Value::as_str) {
            out_line(raw);
        } else if let Some(raw) = step.get("emitPartial").and_then(Value::as_str) {
            let stdout = std::io::stdout();
            let mut o = stdout.lock();
            let _ = o.write_all(raw.as_bytes()).and_then(|_| o.flush());
        } else if let Some(n) = step.get("emitBig").and_then(Value::as_u64) {
            let stdout = std::io::stdout();
            let mut o = stdout.lock();
            let head = br#"{"type":"assistant","pad":""#;
            let _ = o.write_all(head);
            let chunk = vec![b'x'; 64 * 1024];
            let mut left = n as usize;
            while left > 0 {
                let k = left.min(chunk.len());
                if o.write_all(&chunk[..k]).is_err() {
                    std::process::exit(0);
                }
                left -= k;
            }
            let _ = o.write_all(b"\"}\n").and_then(|_| o.flush());
        } else if let Some(n) = step.get("emitDeep").and_then(Value::as_u64) {
            let n = n as usize;
            out_line(&format!(r#"{{"type":"system","subtype":"deep","a":{}{}}}"#, "[".repeat(n), "]".repeat(n)));
        } else if let Some(path) = step.get("touchFile").and_then(Value::as_str) {
            if !path.starts_with('/') {
                die(2, "touchFile needs an absolute path");
            }
            if let Some(parent) = std::path::Path::new(path).parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(path, b"{}");
        } else if step.get("assertOpen").is_some() {
            fake.drain_now();
            if fake.eof {
                die(98, &format!("stdin was closed too early (step {i})"));
            }
        } else if step.get("ignoreTerm").is_some() {
            // SAFETY: setting a signal disposition to SIG_IGN has no handler code.
            unsafe {
                libc::signal(libc::SIGTERM, libc::SIG_IGN);
            }
        } else if let Some(text) = step.get("stderr").and_then(Value::as_str) {
            eprintln!("{text}");
        } else if let Some(ms) = step.get("sleepMs").and_then(Value::as_u64) {
            std::thread::sleep(Duration::from_millis(ms));
        } else if let Some(code) = step.get("exitWith").and_then(Value::as_i64) {
            std::process::exit(code as i32);
        } else if let Some(reg) = step.get("writeRegistry") {
            let dir = config_dir().join("sessions");
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(dir.join(format!("{}.json", std::process::id())), fake.template(reg).to_string());
        } else if let Some(t) = step.get("appendTranscript") {
            let slug = t.get("slug").and_then(Value::as_str).unwrap_or("project");
            let dir = config_dir().join("projects").join(slug);
            let _ = std::fs::create_dir_all(&dir);
            let sid = fake.session_id.clone().unwrap_or_else(|| "unknown".into());
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join(format!("{sid}.jsonl"))) {
                let row = t.get("row").map(|r| fake.template(r)).unwrap_or(Value::Null);
                let _ = writeln!(f, "{row}");
            }
        } else {
            die(2, &format!("unknown step {i}: {step}"));
        }
    }
    while !fake.eof {
        fake.pull(Duration::from_secs(3600));
    }
    let code = scenario.get("eofExit").and_then(Value::as_i64).unwrap_or(0);
    std::process::exit(code as i32);
}
