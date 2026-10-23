//! T-shim-* end to end through the real `gilde-tmux` binary and the socket
//! listener (contract §7.4, §4.6): the full external sequence with
//! byte-exact answers, the pane process spec and env, the member's session
//! evidence, the token-bearing grandchild (T-shim-auth), deviations
//! (T-shim-deviation), the raw-socket refusals and the socket self-heal
//! (T-shim-socketHeal). Owner: T07.
//!
//! The lead is this test process (the shim runs as its direct child); the PTY
//! is a recorder, so no pane process ever runs and nothing is signalled.

#[path = "fixtures/fake_tmux_client/mod.rs"]
mod fake_tmux_client;

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use fake_tmux_client::host::{me, wait_for, Rig, VERSION};
use fake_tmux_client::{deviations, external_sequence, run, run_as_grandchild, shim_env};
use gilde_code_lib::contract::common::IdentityColour;
use gilde_code_lib::contract::compat::CompatCapability;
use gilde_code_lib::contract::process::ProcKind;
use gilde_code_lib::contract::services::{PaneHostApi, SignalPlan, SourceFacts};
use gilde_code_lib::contract::ids::SessionUid;
use gilde_code_lib::panehost::auth::Refusal;
use gilde_code_lib::panehost::proto::{self, messages, ShimResponse};
use gilde_code_lib::panehost::{PaneKind, PaneState, Reconcile};

fn swarm(rig: &Rig) -> String {
    format!("claude-swarm-{}", rig.lead_pid)
}

/// The R6 §5 LP-10 trace, replayed call by call through the shim: every
/// stdout, stderr and exit code byte-exact to §7.4.4; three teammates spawned
/// as `/bin/sh -c <command>` panes with the lead's env minus the token and
/// `TMUX*`; `/exit`'s kill-pane closes each with SIGHUP → SIGTERM after 2 s.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t_shim_full_external_sequence_is_byte_exact() {
    let rig = Rig::new(me()).start();
    let env = shim_env(Some(&rig.socket()), Some(&rig.token), None, &rig.root);
    let seq = external_sequence(me(), &|name, colour| rig.teammate_command(name, colour));
    for (i, c) in seq.iter().enumerate() {
        let out = run(&rig.shim(), &env, &rig.project, &c.args[..]);
        let sub = c.args.get(2).or(c.args.first()).cloned().unwrap_or_default();
        assert_eq!(out.code, c.code, "call {i} ({sub}) exit; stderr {:?}", out.stderr());
        assert_eq!(out.stdout(), c.stdout, "call {i} ({sub}) stdout");
        assert_eq!(out.stderr(), c.stderr, "call {i} ({sub}) stderr");
    }
    assert_eq!(rig.pty.open_count(), 3, "one process per respawn-pane");

    let shim_dir = rig.host.config().shim_dir.to_str().unwrap().to_string();
    for i in 0..3 {
        let spec = rig.pty.spec(i);
        assert_eq!(spec.kind, ProcKind::Pane);
        assert_eq!(spec.argv0.as_str(), "/bin/sh");
        assert_eq!(spec.args.len(), 2);
        assert_eq!(spec.args[0], "-c");
        assert_eq!(spec.parent_uid.as_ref(), Some(&rig.lead));
        assert_eq!((spec.cols, spec.rows), (120, 40));
        assert_eq!(spec.cwd.as_str(), rig.project.to_str().unwrap());
        let env: BTreeMap<String, String> = spec.env.into_iter().collect();
        for absent in ["TMUX", "TMUX_PANE", "ITERM_SESSION_ID", "GILDE_PANE_HOST_TOKEN", "GILDE_SPAWN_ID"] {
            assert!(!env.contains_key(absent), "pane {i}: {absent} must be absent");
        }
        assert_eq!(env["GILDE_PANE_HOST"], rig.socket().to_str().unwrap());
        assert!(env["PATH"].starts_with(&format!("{shim_dir}:")), "the shim dir first on PATH");
        assert_eq!(env["TERM_PROGRAM"], "gilde-code");
        assert_eq!(env["CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS"], "1");
        assert_eq!(env["GILDE_APP_INSTANCE"], "instance-1");
    }

    for i in 0..3 {
        let h = rig.pty.handle(i);
        assert_eq!(rig.pty.wait_close_of(&h.pty_id), SignalPlan::HupThenTerm { grace_ms: 2_000 });
    }
    assert_eq!(rig.pty.closes().len(), 3, "nothing else was signalled");

    let table = rig.table();
    let names = ["alpha", "beta", "gamma"];
    let colours = [IdentityColour::Blue, IdentityColour::Green, IdentityColour::Yellow];
    for (i, p) in table.panes.iter().enumerate() {
        assert_eq!(p.id.as_str(), format!("%{i}"));
        assert!(matches!(&p.kind, PaneKind::Teammate { agent_id, .. } if agent_id == &format!("{}@session-72cb5f46", names[i])), "{:?}", p.kind);
        assert_eq!(p.title.as_deref(), Some(names[i]));
        assert_eq!(p.colour, Some(colours[i]));
        assert!(matches!(p.state, PaneState::Killed { .. }));
        assert!(!p.listed);
        // F-03: the stored command is the redaction (no config dir, no settings JSON).
        let cmd = p.command.as_ref().expect("a stored command");
        assert!(!cmd.redacted.contains("/u/.claude-g") && !cmd.redacted.contains("defaultMode"), "{}", cmd.redacted);
        assert_eq!(cmd.sha256.len(), 64);
    }
    assert!(!table.session, "the last pane left: the session is gone, like tmux");
    assert!(rig.compat.deviations().is_empty(), "the frozen surface has 0 deviations");
    assert!(rig.compat.notes().is_empty());

    // A18.1: each teammate pane's spawn id is bound to a session with its PTY.
    let bound = rig.sessions.bound.lock().unwrap().clone();
    assert_eq!(bound.len(), 3);
    for (i, (spawn, _account, pty, uid)) in bound.iter().enumerate() {
        assert_eq!(spawn, &format!("spawn-{i}"));
        assert_eq!(pty, &rig.pty.handle(i).pty_id);
        assert_eq!(table.panes[i].session.as_ref(), Some(uid));
    }

    // The member's session row at once (In Gilde · Terminal, by spawn id).
    let ev = rig.world.evidence();
    for i in 0..3 {
        let spawn = format!("spawn-{i}");
        assert!(
            ev.iter().any(|u| u.hint.gilde_spawn_id.as_deref() == Some(spawn.as_str()) && matches!(u.facts, SourceFacts::Gilde(_))),
            "evidence for {spawn}"
        );
    }
    let all = format!("{ev:?}");
    assert!(!all.contains("defaultMode") && !all.contains("--agent-id"), "the command never reaches World");
}

/// F-07 / PM-64 (T-shim-auth): a child of the lead's Bash tool inherits the
/// token but not the lead as its parent: read-only queries work, mutating
/// calls are refused, nothing is spawned or signalled, and its unsupported
/// calls are never deviations; the lead's own unsupported call is
/// (T-shim-deviation), once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t_shim_auth_a_token_bearing_grandchild_cannot_mutate() {
    let rig = Rig::new(me()).start();
    let env = shim_env(Some(&rig.socket()), Some(&rig.token), None, &rig.root);
    let sw = swarm(&rig);
    let run_lead = |args: &[&str]| {
        let mut a = vec!["-L", sw.as_str()];
        a.extend_from_slice(args);
        run(&rig.shim(), &env, &rig.project, &a[..])
    };
    let run_child = |args: &[&str]| {
        let mut a = vec!["-L", sw.as_str()];
        a.extend_from_slice(args);
        run_as_grandchild(&rig.shim(), &env, &rig.project, &a)
    };
    assert_eq!(run_lead(&["new-session", "-d", "-s", "claude-swarm", "-n", "swarm-view", "-P", "-F", "#{pane_id}", "--", "cat"]).stdout(), "%0\n");

    let ls = run_child(&["list-panes", "-t", "claude-swarm:swarm-view", "-F", "#{pane_id}"]);
    assert_eq!((ls.code, ls.stdout()), (0, "%0\n".to_string()), "read-only queries keep token-only auth");

    let cmd = rig.teammate_command("mallory", "red");
    let mutating: Vec<Vec<&str>> = vec![
        vec!["respawn-pane", "-k", "-t", "%0", "--", &cmd],
        vec!["kill-pane", "-t", "%0"],
        vec!["select-pane", "-t", "%0", "-T", "x"],
        vec!["set-option", "-p", "-t", "%0", "remain-on-exit", "on"],
        vec!["split-window", "-d", "-t", "%0", "-h", "-P", "-F", "#{pane_id}", "--", "cat"],
        vec!["new-window", "-t", "claude-swarm", "-n", "swarm-view", "-P", "-F", "#{pane_id}", "--", "cat"],
    ];
    for args in &mutating {
        let out = run_child(args);
        assert_eq!((out.code, out.stdout(), out.stderr()), (1, String::new(), "gilde-tmux: not authorized\n".to_string()), "{}", args[0]);
    }
    assert_eq!(rig.host.counters().refusals(Refusal::NotParent), mutating.len() as u64);
    assert_eq!(rig.pty.open_count(), 0, "nothing spawned");
    assert!(rig.pty.closes().is_empty(), "nothing signalled");
    assert_eq!(run_lead(&["list-panes", "-t", "claude-swarm:swarm-view", "-F", "#{pane_id}"]).stdout(), "%0\n");
    assert_eq!(rig.table().panes[0].title, None);

    let out = run_child(&["display-message", "-p", "#{pane_id}"]);
    assert_eq!((out.code, out.stderr()), (1, "gilde-tmux: unsupported command: display-message\n".to_string()));
    assert!(rig.compat.deviations().is_empty(), "a child's call is never a deviation (N-02)");

    let out = run_lead(&["display-message", "-p", "#{pane_id}"]);
    assert_eq!((out.code, out.stderr()), (1, "gilde-tmux: unsupported command: display-message\n".to_string()));
    assert_eq!(
        rig.compat.deviations(),
        vec![(VERSION.to_string(), CompatCapability::PaneHosting, "unsupported shim command: display-message".to_string())]
    );
    let _ = run_lead(&["send-keys", "-t", "%0", "x"]);
    assert_eq!(rig.compat.deviations().len(), 1, "the first deviation of a lead is the one recorded");
}

/// PM-11 (T-shim-deviation): each kind of surface change from the lead —
/// subcommand, flag, format, argv shape — answers exit 1 and flips
/// `paneHosting` for the lead's version; nothing runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t_shim_deviation_marks_pane_hosting_failed() {
    for argv in deviations(me()) {
        let rig = Rig::new(me()).start();
        let env = shim_env(Some(&rig.socket()), Some(&rig.token), None, &rig.root);
        let new_session = ["-L", &swarm(&rig), "new-session", "-d", "-s", "claude-swarm", "-n", "swarm-view", "-P", "-F", "#{pane_id}", "--", "cat"];
        assert_eq!(run(&rig.shim(), &env, &rig.project, &new_session).code, 0);
        let out = run(&rig.shim(), &env, &rig.project, &argv[..]);
        assert_eq!(out.code, 1, "{argv:?}");
        assert!(out.stderr().starts_with(proto::extra::UNSUPPORTED_PREFIX), "{argv:?}: {}", out.stderr());
        let dev = rig.compat.deviations();
        assert_eq!(dev.len(), 1, "{argv:?}");
        assert_eq!((dev[0].0.as_str(), &dev[0].1), (VERSION, &CompatCapability::PaneHosting));
        assert!(!dev[0].2.contains("#{pane_index}") && !dev[0].2.contains(" a b"), "notes carry names, never values: {}", dev[0].2);
        assert_eq!(rig.pty.open_count(), 0);
    }
}

fn raw_call(rig: &Rig, line: &[u8]) -> ShimResponse {
    let mut s = UnixStream::connect(rig.socket()).unwrap();
    // An over-long line is refused mid-write; the answer is still readable.
    let _ = s.write_all(line);
    let _ = s.shutdown(std::net::Shutdown::Write);
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).unwrap();
    let line = out.split(|b| *b == b'\n').next().unwrap();
    serde_json::from_slice(line).unwrap()
}

/// PM-27 (T-shim-version) and the frame rules over the real socket: another
/// build gets the update message (and the version is not marked failed); an
/// unknown field, a NUL in argv or a line over 64 KiB is a bad request; an
/// empty connection is only counted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t_shim_version_mismatch_and_bad_frames_over_the_socket() {
    let rig = Rig::new(me()).start();
    let mut req = rig.req(&["has-session", "-t", "claude-swarm"]);
    req.build = "0.0.0-other".into();
    let resp = raw_call(&rig, format!("{}\n", serde_json::to_string(&req).unwrap()).as_bytes());
    assert_eq!(resp, ShimResponse::version_mismatch());
    assert_eq!(resp.stderr, "gilde-tmux: Gilde was updated; restart Gilde to use teammate panes\n");
    assert!(rig.compat.deviations().is_empty() && rig.compat.notes().is_empty(), "never marked failed");

    let mut v: serde_json::Value = serde_json::to_value(rig.req(&["has-session", "-t", "claude-swarm"])).unwrap();
    v["extra"] = serde_json::json!(1);
    assert_eq!(raw_call(&rig, format!("{v}\n").as_bytes()).stderr, proto::extra::BAD_REQUEST);

    let nul = rig.req(&["respawn-pane", "-k", "-t", "%0", "--", "a\u{0}b"]);
    assert_eq!(raw_call(&rig, format!("{}\n", serde_json::to_string(&nul).unwrap()).as_bytes()).stderr, proto::extra::BAD_REQUEST);

    let mut long = vec![b'x'; proto::MAX_REQUEST_LINE + 10];
    long.push(b'\n');
    assert_eq!(raw_call(&rig, &long).stderr, proto::extra::BAD_REQUEST);

    let before = rig.host.counters().empty_connections();
    drop(UnixStream::connect(rig.socket()).unwrap());
    wait_for("the empty connection to be counted", || (rig.host.counters().empty_connections() > before).then_some(()));
    assert_eq!(rig.pty.open_count(), 0);
}

/// §10.4 (T-shim-socketHeal): the socket path vanishes (e.g. `/tmp` was
/// cleaned) — the shim degrades for that call; the reconcile re-binds it at
/// the same path; a directory that stops being private stops hosting.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t_shim_socket_self_heal() {
    let rig = Rig::new(me()).start();
    let env = shim_env(Some(&rig.socket()), Some(&rig.token), None, &rig.root);
    let has = ["-L", &swarm(&rig), "has-session", "-t", "claude-swarm"];
    let out = run(&rig.shim(), &env, &rig.project, &has);
    assert_eq!(out.stderr(), "can't find session: claude-swarm\n");

    std::fs::remove_file(rig.socket()).unwrap();
    let out = run(&rig.shim(), &env, &rig.project, &has);
    assert_eq!((out.code, out.stderr()), (1, messages::UNREACHABLE_NO_TMUX.to_string()), "one degraded call");

    assert_eq!(rig.host.reconcile(tokio::runtime::Handle::current()), Reconcile::Rebound);
    assert!(rig.socket().exists());
    let out = run(&rig.shim(), &env, &rig.project, &has);
    assert_eq!(out.stderr(), "can't find session: claude-swarm\n", "served again at the same path");
    assert_eq!(rig.host.reconcile(tokio::runtime::Handle::current()), Reconcile::Healthy);

    let run_dir = rig.socket().parent().unwrap().to_path_buf();
    std::fs::set_permissions(&run_dir, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    assert!(matches!(rig.host.reconcile(tokio::runtime::Handle::current()), Reconcile::Unavailable(_)));
    assert!(!rig.host.is_hosting());
    let err = PaneHostApi::lead_env(&*rig.host, &SessionUid::mint()).unwrap_err();
    assert_eq!(serde_json::to_value(&err).unwrap()["params"]["cap"], "paneHosting", "teams run in-process");
}
