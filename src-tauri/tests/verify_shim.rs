//! G6 probes on the shim ↔ PaneHost boundary (contract §14.5, A31.3): SP-4
//! shim auth refusals and HP-3 shim grammar abuse. Each probe prints one
//! `PROBE <id>: …` line. Owner: T33.
//!
//! The rig is the T07 `fake_tmux_client` one: the real `PaneHost` with fakes
//! (a recorder PTY with fake pids — no pane process ever runs, nothing is
//! signalled), the real listener on a socket in the rig's temp dir, and the
//! real `gilde-tmux` binary built by cargo. Two leads are live in **one**
//! host:
//!
//! * lead A — this test process (`me()`): the shim runs as its direct child,
//!   so A's own calls go through the real binary over the real socket
//!   (`socket` in the PROBE lines);
//! * lead B — this process's parent (`parent_pid()`, only ever compared,
//!   never signalled): its calls go straight to `PaneHost::handle` with this
//!   process as the kernel peer, whose parent is B (`handle`).
//!
//! The "real tmux" the shim would fall back to is `fake_real_tmux` (it only
//! records that it ran); no system tmux and no vendor CLI is ever run.

mod fixtures;
#[path = "fixtures/verify/mod.rs"]
mod verify;
#[path = "fixtures/fake_tmux_client/mod.rs"]
mod fake_tmux_client;

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use fake_tmux_client::host::{me, parent_pid, Rig};
use fake_tmux_client::{run, run_as_grandchild, shim_env, Outcome};
use gilde_code_lib::contract::ids::SessionUid;
use gilde_code_lib::contract::services::PaneHostApi;
use gilde_code_lib::panehost::auth::{self, PeerCred, Refusal};
use gilde_code_lib::panehost::proto::{self, ShimRequest, ShimResponse, SHIM_PROTOCOL_V};
use gilde_code_lib::panehost::server::MAX_PANES;
use gilde_code_lib::panehost::subcommands::texts;
use verify::{Logs, Probe};

/// Serializes the two probes of this binary: both read the process-wide log
/// capture by offset, and HP-3's bad frames write security lines too.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const NOT_AUTHORIZED: &str = "gilde-tmux: not authorized\n";
const BAD_REQUEST: &str = "gilde-tmux: bad request\n";
/// Over the 64 KiB request-line cap (contract §4.6).
const OVERSIZED_LINE: usize = 64 * 1024;
const NEW_SESSION: [&str; 11] = ["new-session", "-d", "-s", "claude-swarm", "-n", "swarm-view", "-P", "-F", "#{pane_id}", "--", "cat"];
const NEW_WINDOW: [&str; 10] = ["new-window", "-t", "claude-swarm", "-n", "swarm-view", "-P", "-F", "#{pane_id}", "--", "cat"];
const LIST: [&str; 5] = ["list-panes", "-t", "claude-swarm:swarm-view", "-F", "#{pane_id}"];

fn split(target: &str) -> Vec<&str> {
    vec!["split-window", "-d", "-t", target, "-v", "-P", "-F", "#{pane_id}", "--", "cat"]
}

fn respawn<'a>(target: &'a str, command: &'a str) -> Vec<&'a str> {
    vec!["respawn-pane", "-k", "-t", target, "--", command]
}

fn swarm(pid: i32) -> String {
    format!("claude-swarm-{pid}")
}

/// `["-L", "claude-swarm-<pid>", args…]`.
fn swarm_argv<'a>(sock: &'a str, args: &[&'a str]) -> Vec<&'a str> {
    let mut v = vec!["-L", sock];
    v.extend_from_slice(args);
    v
}

/// `(exit, stdout, stderr)` of a shim run.
fn triple(o: &Outcome) -> (i32, String, String) {
    (o.code, o.stdout(), o.stderr())
}

fn triple_of(r: &ShimResponse) -> (i32, String, String) {
    (r.exit, r.stdout.clone(), r.stderr.clone())
}

fn fail(stderr: &str) -> (i32, String, String) {
    (1, String::new(), stderr.to_string())
}

// ---------------------------------------------------------------------------
// the two-lead host
// ---------------------------------------------------------------------------

/// Lead B, registered in the rig's host next to the rig's own lead A.
struct LeadB {
    uid: SessionUid,
    pid: i32,
    token: String,
}

/// Mint B's env (token) and mark it started with `parent_pid()`.
fn add_lead_b(rig: &Rig) -> LeadB {
    let uid = SessionUid::mint();
    let env = PaneHostApi::lead_env(&*rig.host, &uid).expect("lead B's env");
    let token = env
        .set
        .iter()
        .find(|(k, _)| k == proto::ENV_PANE_HOST_TOKEN)
        .map(|(_, v)| v.clone())
        .expect("lead B's token");
    assert_ne!(token, rig.token, "a token per lead");
    let pid = parent_pid();
    assert_ne!(pid, rig.lead_pid, "two distinct lead pids");
    PaneHostApi::lead_started(&*rig.host, &uid, pid);
    LeadB { uid, pid, token }
}

/// A request as B's own `tmux` child would send it.
fn b_req(rig: &Rig, b: &LeadB, args: &[&str]) -> ShimRequest {
    let mut argv = vec!["-L".to_string(), swarm(b.pid)];
    argv.extend(args.iter().map(|s| s.to_string()));
    ShimRequest {
        v: SHIM_PROTOCOL_V,
        build: gilde_code_lib::BUILD_ID.to_string(),
        token: b.token.clone(),
        shim_pid: me(),
        shim_ppid: b.pid,
        argv,
        cwd: rig.project.to_str().unwrap().to_string(),
    }
}

/// The kernel peer of B's direct calls: this process, whose parent is B.
fn b_peer() -> PeerCred {
    PeerCred { uid: auth::app_uid(), pid: me() }
}

async fn b_call(rig: &Rig, b: &LeadB, args: &[&str]) -> ShimResponse {
    rig.host.handle(b_req(rig, b, args), b_peer()).await
}

/// The listed pane ids of a lead's table.
fn listed(rig: &Rig, lead: &SessionUid) -> Vec<String> {
    rig.host
        .table(lead)
        .map(|t| t.panes.into_iter().filter(|p| p.listed).map(|p| p.id.as_str().to_string()).collect())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// the shim's environment, the raw socket, counters, logs
// ---------------------------------------------------------------------------

/// The env of lead A's `tmux` child with `token` (none: unset), the real
/// tmux being `fake_real_tmux`, which records into `record` if it ever runs.
fn a_env(rig: &Rig, token: Option<&str>, record: &Path) -> Vec<(String, String)> {
    let mut env = shim_env(Some(&rig.socket()), token, Some(Path::new(env!("CARGO_BIN_EXE_fake_real_tmux"))), &rig.root);
    env.push(("FAKE_REAL_TMUX_RECORD".to_string(), record.to_str().unwrap().to_string()));
    env
}

/// One hand-written request over the real socket from this process (kernel
/// peer = this process: lead A itself, never a child of A).
fn raw_call(rig: &Rig, req: &ShimRequest) -> ShimResponse {
    let mut s = UnixStream::connect(rig.socket()).expect("the listener accepts");
    let mut line = serde_json::to_vec(req).unwrap();
    line.push(b'\n');
    // Set while the connection is certainly open: macOS refuses `SO_RCVTIMEO`
    // (EINVAL) once the peer closed (as the product's `proto::exchange` notes).
    s.set_read_timeout(Some(Duration::from_secs(30))).expect("the read timeout, set while open");
    s.write_all(&line).expect("the request is written");
    half_close_and_read(s)
}

/// Half-close the request side and read the one answer line. The server may
/// answer an early-rejected frame and close before this half-close; the
/// `shutdown` then fails with ENOTCONN (or EPIPE): the connection was closed
/// by the server, which is not a failure — its answer is already in the
/// receive buffer. Any other error still fails the probe.
fn half_close_and_read(mut s: UnixStream) -> ShimResponse {
    if let Err(e) = s.shutdown(std::net::Shutdown::Write) {
        assert!(closed_by_server(&e), "half-closing the request failed: {e}");
    }
    let mut out = Vec::new();
    s.read_to_end(&mut out).expect("the answer");
    let first = out.split(|b| *b == b'\n').next().unwrap();
    serde_json::from_slice(first).expect("a response line")
}

/// ENOTCONN or EPIPE from a half-close: the server already closed.
fn closed_by_server(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::NotConnected | std::io::ErrorKind::BrokenPipe)
}

/// The HP-3 flake, made deterministic: the server end answers and closes
/// *before* the client half-closes (what an early-rejected frame does under
/// load). The client still reads the answer; only ENOTCONN/EPIPE count as
/// "closed by the server", any other error stays a failure.
#[test]
fn raw_call_reads_the_answer_of_a_server_that_already_closed() {
    let answer = ShimResponse { v: SHIM_PROTOCOL_V, exit: 1, stdout: String::new(), stderr: "gilde-tmux: bad request\n".into() };
    let mut wire = serde_json::to_vec(&answer).unwrap();
    wire.push(b'\n');
    let (mut client, mut server) = UnixStream::pair().expect("a socket pair");
    client.set_read_timeout(Some(Duration::from_secs(30))).expect("read timeout");
    let peer = std::thread::spawn(move || {
        let mut got = Vec::new();
        let mut buf = [0u8; 256];
        while !got.contains(&b'\n') {
            let n = server.read(&mut buf).expect("the request");
            assert!(n > 0, "the request line arrives");
            got.extend_from_slice(&buf[..n]);
        }
        server.write_all(&wire).expect("the answer");
        // `server` drops here: closed before the client's half-close.
    });
    client.write_all(b"{\"v\":2,\"argv\":[\"x\\u0000\"]}\n").expect("the request");
    peer.join().expect("the server end answered and closed");
    let half_close = client.try_clone().expect("clone").shutdown(std::net::Shutdown::Write);
    println!("PROBE HP-3-race: half-close after the server closed → {half_close:?}");
    assert_eq!(half_close_and_read(client), answer);

    for (kind, ours) in [
        (std::io::ErrorKind::NotConnected, true),
        (std::io::ErrorKind::BrokenPipe, true),
        (std::io::ErrorKind::ConnectionRefused, false),
        (std::io::ErrorKind::PermissionDenied, false),
    ] {
        assert_eq!(closed_by_server(&std::io::Error::from(kind)), ours, "{kind:?}");
    }
}

fn counts(rig: &Rig) -> [u64; 8] {
    let c = rig.host.counters();
    Refusal::ALL.map(|r| c.refusals(r))
}

/// The refusal counters that moved since `before`, as `reason+n,…`.
fn moved(rig: &Rig, before: [u64; 8]) -> String {
    let now = counts(rig);
    let parts: Vec<String> = Refusal::ALL
        .iter()
        .zip(before.iter().zip(now.iter()))
        .filter(|(_, (b, n))| b != n)
        .map(|(r, (b, n))| format!("{}+{}", r.as_str(), n - b))
        .collect();
    if parts.is_empty() {
        "none".into()
    } else {
        parts.join(",")
    }
}

/// The `panehost::security` refusal lines logged after `mark`.
fn security_lines(logs: &Logs, mark: usize) -> Vec<String> {
    logs.since(mark)
        .lines()
        .filter(|l| l.contains("panehost::security") && l.contains("shim call refused"))
        .map(str::to_string)
        .collect()
}

fn has_reason(line: &str, reason: &str) -> bool {
    line.contains(&format!("reason=\"{reason}\"")) || line.contains(&format!("reason={reason} ")) || line.ends_with(&format!("reason={reason}"))
}

fn records(path: &Path) -> usize {
    std::fs::read_to_string(path).map(|t| t.lines().count()).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// SP-4
// ---------------------------------------------------------------------------

/// One SP-4 refusal: `not authorized` (exit 1, empty stdout), exactly the
/// expected counter +1 (no other), exactly one security line with that
/// reason, nothing spawned, the real tmux never run.
#[allow(clippy::too_many_arguments)]
fn expect_refused(
    p: &mut Probe,
    rig: &Rig,
    logs: &Logs,
    record: &Path,
    variant: &str,
    got: (i32, String, String),
    why: Refusal,
    before: [u64; 8],
    mark: usize,
) {
    let lines = security_lines(logs, mark);
    assert_eq!(got, fail(NOT_AUTHORIZED), "{variant}: the answer");
    let mut expected = before;
    expected[Refusal::ALL.iter().position(|r| *r == why).unwrap()] += 1;
    assert_eq!(counts(rig), expected, "{variant}: exactly {} +1, got {}", why.as_str(), moved(rig, before));
    assert_eq!(lines.len(), 1, "{variant}: one security line: {lines:?}");
    assert!(has_reason(&lines[0], why.as_str()), "{variant}: the line names the reason: {}", lines[0]);
    assert_eq!(rig.pty.open_count(), 0, "{variant}: nothing spawned");
    assert_eq!(records(record), 0, "{variant}: never a fallback to the real tmux");
    p.saw(
        variant,
        format!(
            "exit {} stdout \"\" stderr {:?}, counted {}, logged 1×[panehost::security reason={}], opens 0, real tmux not run",
            got.0,
            got.2,
            moved(rig, before),
            why.as_str()
        ),
    );
}

/// SP-4: a shim request with a foreign token, another (live) lead's pid and
/// a non-lead pid — each `not authorized`, counted, logged; nothing spawned.
/// Every request is a `respawn-pane` of a teammate command into an existing
/// pane (the call that would spawn); a positive control at the end shows the
/// same call from the lead itself does spawn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_sp4_shim_auth_refusals() {
    let logs = Logs::global();
    let _serial = SERIAL.lock().await;
    let start = logs.mark();
    let mut p = Probe::new("SP-4");

    let rig = Rig::new(me()).start();
    let b = add_lead_b(&rig);
    let record = rig.root.join("real-tmux.jsonl");
    let sock_a = swarm(rig.lead_pid);
    let sock_b = swarm(b.pid);
    let cmd = rig.teammate_command("mallory", "red");
    let spawn_args = respawn("%0", &cmd);

    // Both leads have a pane %0 (A through the real shim, B directly).
    let env_a = a_env(&rig, Some(&rig.token), &record);
    let out = run(&rig.shim(), &env_a, &rig.project, &swarm_argv(&sock_a, &NEW_SESSION)[..]);
    assert_eq!(triple(&out), (0, "%0\n".to_string(), String::new()), "lead A's own new-session");
    assert_eq!(b_call(&rig, &b, &NEW_SESSION).await, ShimResponse::ok("%0\n"), "lead B's own new-session");
    let foreign = String::clone(&auth::mint_token());
    assert!(foreign != rig.token && foreign != b.token);

    // (1) socket: a well-formed token no lead holds.
    let (before, mark) = (counts(&rig), logs.mark());
    let out = run(&rig.shim(), &a_env(&rig, Some(&foreign), &record), &rig.project, &swarm_argv(&sock_a, &spawn_args)[..]);
    expect_refused(&mut p, &rig, logs, &record, "socket: foreign token (fresh 64-hex, never minted for a lead), argv names lead A", triple(&out), Refusal::Token, before, mark);

    // (2a) socket: A's valid token naming B's pid (two leads live).
    let (before, mark) = (counts(&rig), logs.mark());
    let out = run(&rig.shim(), &env_a, &rig.project, &swarm_argv(&sock_b, &spawn_args)[..]);
    expect_refused(&mut p, &rig, logs, &record, "socket: lead A's token + claude-swarm-<lead B pid> (B live)", triple(&out), Refusal::LeadPid, before, mark);

    // (2b) socket: B's valid token from a direct child of A, naming A's pid.
    let (before, mark) = (counts(&rig), logs.mark());
    let out = run(&rig.shim(), &a_env(&rig, Some(&b.token), &record), &rig.project, &swarm_argv(&sock_a, &spawn_args)[..]);
    expect_refused(&mut p, &rig, logs, &record, "socket: lead B's token + claude-swarm-<lead A pid> from A's child", triple(&out), Refusal::LeadPid, before, mark);

    // (3a) socket: A's valid token naming a live non-lead pid (launchd).
    let (before, mark) = (counts(&rig), logs.mark());
    let out = run(&rig.shim(), &env_a, &rig.project, &swarm_argv("claude-swarm-1", &spawn_args)[..]);
    expect_refused(&mut p, &rig, logs, &record, "socket: lead A's token + claude-swarm-1 (a non-lead pid)", triple(&out), Refusal::LeadPid, before, mark);

    // (3b) socket: the token-bearing grandchild (A → /bin/sh → shim): the
    // kernel peer's parent is not the lead.
    let (before, mark) = (counts(&rig), logs.mark());
    let out = run_as_grandchild(&rig.shim(), &env_a, &rig.project, &swarm_argv(&sock_a, &spawn_args));
    expect_refused(&mut p, &rig, logs, &record, "socket: non-lead kernel peer (A's grandchild via /bin/sh) with A's token", triple(&out), Refusal::NotParent, before, mark);

    // (3c) socket(raw): a same-user process connects itself with A's token
    // and self-reports shimPpid = A; its kernel peer pid (this process, A
    // itself) is not a child of A.
    let (before, mark) = (counts(&rig), logs.mark());
    let mut req = rig.req(&spawn_args);
    req.shim_pid = me();
    req.shim_ppid = rig.lead_pid;
    let resp = raw_call(&rig, &req);
    expect_refused(
        &mut p,
        &rig,
        logs,
        &record,
        "socket(raw): A's token, self-reported shimPpid=A, kernel peer not A's child",
        triple_of(&resp),
        Refusal::NotParent,
        before,
        mark,
    );

    // Edge (shim-local): a malformed token (uppercase hex) is refused by the
    // shim itself before it connects: nothing reaches the app.
    let (before, mark) = (counts(&rig), logs.mark());
    let malformed = "F".repeat(64);
    let out = run(&rig.shim(), &a_env(&rig, Some(&malformed), &record), &rig.project, &swarm_argv(&sock_a, &spawn_args)[..]);
    let lines = security_lines(logs, mark);
    assert_eq!(triple(&out), fail(NOT_AUTHORIZED));
    assert_eq!(rig.pty.open_count(), 0);
    assert_eq!(records(&record), 0);
    let observed = format!(
        "exit {} stderr {:?}, counted {}, logged {}×, opens 0, real tmux not run",
        out.code,
        out.stderr(),
        moved(&rig, before),
        lines.len()
    );
    // A33.7: the shim refusing a malformed token before it connects is fine —
    // nothing reached the app, so nothing is counted there.
    assert_eq!(counts(&rig), before, "nothing reached the app's counters");
    assert!(lines.is_empty(), "nothing reached the app's log: {lines:?}");
    p.saw("socket(shim-local): malformed token \"F\"×64 (refused by the shim before connecting, A33.7)", observed);

    // Edge (raw socket): the same malformed token sent straight to the socket.
    let (before, mark) = (counts(&rig), logs.mark());
    let mut req = rig.req(&spawn_args);
    req.token = malformed.clone();
    let resp = raw_call(&rig, &req);
    let lines = security_lines(logs, mark);
    assert_eq!(resp.exit, 1);
    assert!(resp.stdout.is_empty());
    assert_eq!(rig.pty.open_count(), 0);
    assert_eq!(counts(&rig).iter().sum::<u64>(), before.iter().sum::<u64>() + 1, "counted once");
    assert_eq!(lines.len(), 1, "logged once: {lines:?}");
    // A33.7: a refused request the server receives — a bad frame too — is
    // counted with the auth refusals and logged, without content.
    assert_eq!(resp.stderr, BAD_REQUEST, "a malformed token is a bad frame");
    assert_eq!(rig.host.counters().total_refusals(), before.iter().sum::<u64>() + 1, "counted in the refusal counter");
    assert_eq!(moved(&rig, before), "badFrame+1");
    assert!(has_reason(&lines[0], "badFrame"), "{lines:?}");
    assert!(!lines[0].contains(&malformed), "no token in the log line");
    let observed = format!("exit 1 stderr {:?}, counted {}, logged 1×, opens 0", resp.stderr, moved(&rig, before));
    p.saw("socket(raw): malformed token \"F\"×64", observed);

    // A33.7: an over-long request line, refused before it is parsed.
    let (before, mark) = (counts(&rig), logs.mark());
    let resp = {
        let mut s = UnixStream::connect(rig.socket()).expect("the listener accepts");
        let mut line = vec![b'x'; OVERSIZED_LINE + 64];
        line.push(b'\n');
        let _ = s.write_all(&line);
        let _ = s.shutdown(std::net::Shutdown::Write);
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        let mut out = Vec::new();
        let _ = s.read_to_end(&mut out);
        let first = out.split(|b| *b == b'\n').next().unwrap_or_default().to_vec();
        serde_json::from_slice::<ShimResponse>(&first).expect("a response line")
    };
    let lines = security_lines(logs, mark);
    assert_eq!((resp.exit, resp.stderr.as_str()), (1, BAD_REQUEST));
    assert_eq!(rig.host.counters().total_refusals(), before.iter().sum::<u64>() + 1, "counted once");
    assert_eq!(lines.len(), 1, "logged once: {lines:?}");
    assert_eq!(rig.pty.open_count(), 0);
    p.saw(
        format!("socket(raw): an over-long request line ({} KiB + 64 B)", OVERSIZED_LINE / 1024),
        format!("exit 1 stderr {:?}, counted {}, logged 1×, opens 0", resp.stderr, moved(&rig, before)),
    );

    // Never a deviation, never a note; nothing signalled; no token in logs.
    assert!(rig.compat.deviations().is_empty() && rig.compat.notes().is_empty(), "refusals are never deviations [N-02]");
    assert!(rig.pty.closes().is_empty(), "nothing signalled");
    let text = logs.since(start);
    for (who, t) in [("A", rig.token.as_str()), ("B", b.token.as_str()), ("foreign", foreign.as_str())] {
        assert!(!text.contains(t), "lead {who}'s token was logged");
    }
    assert_eq!(listed(&rig, &rig.lead), vec!["%0"], "lead A's server unchanged");
    assert_eq!(listed(&rig, &b.uid), vec!["%0"], "lead B's server unchanged");
    p.saw("after all refusals", "compat deviations 0, notes 0, closes 0, no token (A/B/foreign) in the log capture, both servers still [%0]");

    // Positive control: the same respawn from lead A's own direct child spawns.
    let out = run(&rig.shim(), &env_a, &rig.project, &swarm_argv(&sock_a, &spawn_args)[..]);
    assert_eq!(triple(&out), (0, String::new(), String::new()), "the lead's own respawn works");
    assert_eq!(rig.pty.open_count(), 1, "the witness counts a real spawn");
    p.saw("control: lead A's own respawn-pane (socket)", "exit 0, opens 1 (the open counter is live)");
    p.finish();
}

// ---------------------------------------------------------------------------
// HP-3
// ---------------------------------------------------------------------------

/// HP-3: shim grammar abuse — `split-window` on another lead's pane id (and
/// `respawn-pane`/`kill-pane` on it), `respawn-pane` with a NUL byte,
/// `kill-pane %999999999`, the 16th pane by `split-window` and by
/// `new-window` — each exit 1 with the exact stderr; nothing spawned; never
/// a compat deviation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_hp3_shim_grammar_abuse() {
    let _logs = Logs::global();
    let _serial = SERIAL.lock().await;
    let mut p = Probe::new("HP-3");

    let rig = Rig::new(me()).start();
    let b = add_lead_b(&rig);
    let record = rig.root.join("real-tmux.jsonl");
    let sock_a = swarm(rig.lead_pid);
    let env_a = a_env(&rig, Some(&rig.token), &record);
    let a = |args: &[&str]| triple(&run(&rig.shim(), &env_a, &rig.project, &swarm_argv(&sock_a, args)[..]));

    // A: [%0] through the real shim; B: [%0 %1 %2] directly.
    assert_eq!(a(&NEW_SESSION), (0, "%0\n".to_string(), String::new()));
    assert_eq!(b_call(&rig, &b, &NEW_SESSION).await, ShimResponse::ok("%0\n"));
    assert_eq!(b_call(&rig, &b, &split("%0")).await, ShimResponse::ok("%1\n"));
    assert_eq!(b_call(&rig, &b, &split("%0")).await, ShimResponse::ok("%2\n"));
    let cmd = rig.teammate_command("mallory", "red");

    // (1) another lead's pane id, from A over the socket.
    let cases: Vec<(&str, Vec<&str>, String)> = vec![
        ("socket: A split-window -t %2 (only lead B has %2)", split("%2"), texts::cant_find_pane("2")),
        ("socket: A respawn-pane -k -t %2 -- <teammate cmd> (B's pane)", respawn("%2", &cmd), texts::cant_find_pane("2")),
        ("socket: A kill-pane -t %2 (B's pane)", vec!["kill-pane", "-t", "%2"], texts::cant_find_pane("2")),
    ];
    for (variant, args, stderr) in cases {
        let got = a(&args);
        assert_eq!(got, fail(&stderr), "{variant}");
        assert_eq!(rig.pty.open_count(), 0, "{variant}: nothing spawned");
        p.saw(variant, format!("exit 1 stderr {:?}, opens 0", got.2));
    }
    assert_eq!(listed(&rig, &b.uid), vec!["%0", "%1", "%2"], "B's server untouched");
    assert_eq!(listed(&rig, &rig.lead), vec!["%0"], "A's server untouched");
    p.saw("after (1)", "A lists [%0], B lists [%0 %1 %2] (no cross-lead effect)");

    // (2) a NUL byte in respawn-pane's command: only a frame can carry it.
    let refusals = rig.host.counters().refusals(Refusal::BadFrame);
    let got = triple_of(&b_call(&rig, &b, &respawn("%0", "a\u{0}b")).await);
    assert_eq!(got.0, 1);
    assert!(got.1.is_empty());
    assert_eq!(rig.pty.open_count(), 0);
    p.saw("handle: B respawn-pane -k -t %0 -- \"a\\0b\" (authorized lead + peer)", format!("exit 1 stderr {:?}, badFrame+1, opens 0", got.2));
    let nul_stderr = got.2.clone();

    let got = triple_of(&b_call(&rig, &b, &["respawn-pane", "-k", "-t", "%0\u{0}", "--", "echo hi"]).await);
    assert_eq!(got, fail(&nul_stderr), "a NUL in another argv position is the same refusal");
    p.saw("handle: B respawn-pane -k -t \"%0\\0\" -- \"echo hi\" (NUL in the target)", format!("exit 1 stderr {:?}, opens 0", got.2));

    let resp = raw_call(&rig, &rig.req(&respawn("%0", "a\u{0}b")));
    let got = triple_of(&resp);
    assert_eq!(got, fail(&nul_stderr), "the same answer over the socket");
    assert_eq!(rig.pty.open_count(), 0);
    p.saw("socket(raw JSON \\u0000): A respawn-pane -k -t %0 -- \"a\\0b\"", format!("exit 1 stderr {:?}, opens 0", got.2));
    assert_eq!(rig.host.counters().refusals(Refusal::BadFrame), refusals + 3, "each NUL frame counted as badFrame");

    let argv_nul = std::process::Command::new(rig.shim())
        .args(swarm_argv(&sock_a, &respawn("%0", "a\u{0}b")))
        .env_clear()
        .envs(env_a.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .current_dir(&rig.project)
        .stdin(std::process::Stdio::null())
        .output();
    let kind = argv_nul.as_ref().err().map(|e| e.kind());
    assert_eq!(kind, Some(std::io::ErrorKind::InvalidInput), "a NUL cannot be put in an execve argv");
    p.saw("socket(real shim argv)", "not expressible: Command::output() fails InvalidInput (\"nul byte\") before exec, the shim never runs");

    // The closest argv-expressible bytes: a C0 control in the command.
    let got = a(&respawn("%0", "echo \u{1}hi"));
    assert_eq!(got, fail(texts::BAD_COMMAND));
    assert_eq!(rig.pty.open_count(), 0);
    p.saw("socket: A respawn-pane -k -t %0 -- \"echo \\x01hi\" (control byte, argv-expressible)", format!("exit 1 stderr {:?}, opens 0", got.2));

    // (3) kill-pane on ids that were never allocated: two sizes, two paths.
    let got = a(&["kill-pane", "-t", "%999999999"]);
    assert_eq!(got, fail("can't find pane: %999999999\n"));
    p.saw("socket: A kill-pane -t %999999999", format!("exit 1 stderr {:?}", got.2));
    let got = triple_of(&b_call(&rig, &b, &["kill-pane", "-t", "%999999999"]).await);
    assert_eq!(got, fail("can't find pane: %999999999\n"));
    p.saw("handle: B kill-pane -t %999999999", format!("exit 1 stderr {:?}", got.2));
    let got = a(&["kill-pane", "-t", "%999999"]);
    assert_eq!(got, fail("can't find pane: %999999\n"));
    p.saw("socket: A kill-pane -t %999999 (6 digits, in range, never allocated)", format!("exit 1 stderr {:?}", got.2));
    assert!(rig.pty.closes().is_empty(), "nothing signalled");
    assert_eq!(listed(&rig, &rig.lead), vec!["%0"]);
    assert_eq!(listed(&rig, &b.uid), vec!["%0", "%1", "%2"]);

    // (4) the 16th pane: fill A to MAX_PANES, then split-window and new-window.
    for n in 1..MAX_PANES {
        assert_eq!(a(&split("%0")), (0, format!("%{n}\n"), String::new()), "split {n}");
    }
    assert_eq!(listed(&rig, &rig.lead).len(), MAX_PANES);
    let got = a(&split("%0"));
    assert_eq!(got, fail(texts::NO_SPACE));
    p.saw(format!("socket: A split-window with {MAX_PANES} panes listed (the 16th)"), format!("exit 1 stderr {:?}", got.2));
    let got = a(&NEW_WINDOW);
    assert_eq!(got, fail(texts::NO_SPACE));
    p.saw(format!("socket: A new-window with {MAX_PANES} panes listed (the 16th)"), format!("exit 1 stderr {:?}", got.2));
    let got = a(&respawn(&format!("%{MAX_PANES}"), &cmd));
    assert_eq!(got, fail(&texts::cant_find_pane(&MAX_PANES.to_string())));
    p.saw(format!("socket: A respawn-pane -t %{MAX_PANES} (the refused 16th id)"), format!("exit 1 stderr {:?}", got.2));
    let expect: String = (0..MAX_PANES).map(|n| format!("%{n}\n")).collect();
    assert_eq!(a(&LIST), (0, expect, String::new()), "still exactly %0..%14");
    assert_eq!(rig.pty.open_count(), 0, "nothing spawned");

    // (1') the other direction: B targets a pane only A has.
    let last = format!("%{}", MAX_PANES - 1);
    let got = triple_of(&b_call(&rig, &b, &split(&last)).await);
    assert_eq!(got, fail(&texts::cant_find_pane(&(MAX_PANES - 1).to_string())));
    p.saw(format!("handle: B split-window -t {last} (only lead A has {last})"), format!("exit 1 stderr {:?}", got.2));
    assert_eq!(listed(&rig, &b.uid), vec!["%0", "%1", "%2"], "B's server unchanged (its own cap untouched by A's 15)");

    // Grammar abuse is never a deviation; nothing spawned, signalled or
    // passed to the real tmux.
    assert!(rig.compat.deviations().is_empty(), "value refusals never disable pane hosting");
    assert_eq!(rig.pty.open_count(), 0);
    assert!(rig.pty.closes().is_empty());
    assert_eq!(records(&record), 0);
    p.saw("after all", "opens 0, closes 0, compat deviations 0, real tmux not run, A lists %0..%14, B lists [%0 %1 %2]");

    // Positive control: a valid respawn into A's own pane spawns.
    assert_eq!(a(&respawn("%0", &cmd)), (0, String::new(), String::new()));
    assert_eq!(rig.pty.open_count(), 1);
    p.saw("control: A respawn-pane -k -t %0 -- <teammate cmd> (socket)", "exit 0, opens 1 (the open counter is live)");
    p.finish();
}

