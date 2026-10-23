//! `fake_tmux_client` — replays the external-mode tmux sequence of a hosted
//! Claude lead against the `gilde-tmux` shim with real argv and checks every
//! answer byte-for-byte against §7.4.4 (contract §14.1).
//!
//! Owner: T07. The sequence is the 46 calls of the 2.1.307 trace (R6 §5
//! LP-10, `probes/teams/shim.jsonl`): `-V`, `has-session` (1), `new-session`,
//! the three teammates alpha/beta/gamma with their colours, titles, layouts,
//! `remain-on-exit failed` and `respawn-pane`, then the `/exit` cleanup that
//! sends `kill-pane` for every pane. The respawn payloads are built by the
//! caller (they name the rig's stand-in claude install).

#![allow(dead_code)]

#[path = "host.rs"]
pub mod host;

use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Stdio};

/// What one shim run printed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Outcome {
    pub fn stdout(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
    pub fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

/// The env a hosted lead's tmux child sees (a cleared env plus these).
pub fn shim_env(socket: Option<&Path>, token: Option<&str>, real_tmux: Option<&Path>, path_dir: &Path) -> Vec<(String, String)> {
    let mut env = vec![("PATH".to_string(), path_dir.to_str().unwrap().to_string())];
    if let Some(s) = socket {
        env.push(("GILDE_PANE_HOST".to_string(), s.to_str().unwrap().to_string()));
    }
    if let Some(t) = token {
        env.push(("GILDE_PANE_HOST_TOKEN".to_string(), t.to_string()));
    }
    env.push((
        "GILDE_REAL_TMUX".to_string(),
        real_tmux.map(|p| p.to_str().unwrap().to_string()).unwrap_or_default(),
    ));
    env
}

/// Run the shim once as a direct child of this process.
pub fn run<S: AsRef<OsStr>>(shim: &Path, env: &[(String, String)], cwd: &Path, args: &[S]) -> Outcome {
    let out = Command::new(shim)
        .args(args)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .expect("the shim runs");
    Outcome { code: out.status.code().unwrap_or(-1), stdout: out.stdout, stderr: out.stderr }
}

/// Run the shim as a **grandchild** (this process → `/bin/sh` → shim), as a
/// lead's Bash-tool child would: the token is inherited, the parent is not
/// the lead. The trailing `exit $?` keeps the shell from exec-ing the shim.
pub fn run_as_grandchild(shim: &Path, env: &[(String, String)], cwd: &Path, args: &[&str]) -> Outcome {
    let out = Command::new("/bin/sh")
        .arg("-c")
        .arg("\"$0\" \"$@\"; exit $?")
        .arg(shim)
        .args(args)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .expect("the shell runs");
    Outcome { code: out.status.code().unwrap_or(-1), stdout: out.stdout, stderr: out.stderr }
}

/// One call of the sequence and its exact expected answer.
#[derive(Debug, Clone)]
pub struct Call {
    pub args: Vec<String>,
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

fn call(args: &[&str], code: i32, stdout: &str, stderr: &str) -> Call {
    Call { args: args.iter().map(|s| s.to_string()).collect(), code, stdout: stdout.into(), stderr: stderr.into() }
}

/// Prefix `-L claude-swarm-<lead>` to every call but `-V`.
fn swarm(lead: i32, mut c: Call) -> Call {
    let mut args = vec!["-L".to_string(), format!("claude-swarm-{lead}")];
    args.append(&mut c.args);
    c.args = args;
    c
}

/// The colours, titles and options the lead sets on a new teammate pane.
fn decorate(lead: i32, pane: &str, colour: &str, name: &str) -> Vec<Call> {
    vec![
        swarm(lead, call(&["set-option", "-p", "-t", pane, "window-style", &format!("bg=default,fg={colour}")], 0, "", "")),
        swarm(lead, call(&["set-option", "-p", "-t", pane, "pane-border-style", &format!("fg={colour}")], 0, "", "")),
        swarm(lead, call(&["set-option", "-p", "-t", pane, "pane-active-border-style", &format!("fg={colour}")], 0, "", "")),
        swarm(lead, call(&["select-pane", "-t", pane, "-T", name], 0, "", "")),
        swarm(
            lead,
            call(
                &["set-option", "-p", "-t", pane, "pane-border-format", &format!("#[fg={colour},bold] #{{pane_title}} #[default]")],
                0,
                "",
                "",
            ),
        ),
    ]
}

const LIST_PANES: [&str; 5] = ["list-panes", "-t", "claude-swarm:swarm-view", "-F", "#{pane_id}"];
const LIST_WINDOWS: [&str; 5] = ["list-windows", "-t", "claude-swarm", "-F", "#{window_name}"];
const HAS_SESSION: [&str; 3] = ["has-session", "-t", "claude-swarm"];

/// The 46-call 2.1.307 sequence (R6 §5 LP-10) with byte-exact answers.
/// `respawn(name, colour)` builds each teammate's one-string command.
pub fn external_sequence(lead: i32, respawn: &dyn Fn(&str, &str) -> String) -> Vec<Call> {
    let s = |c: Call| swarm(lead, c);
    let mut v = vec![
        call(&["-V"], 0, "tmux 3.4\n", ""),
        s(call(&HAS_SESSION, 1, "", "can't find session: claude-swarm\n")),
        s(call(&["new-session", "-d", "-s", "claude-swarm", "-n", "swarm-view", "-P", "-F", "#{pane_id}", "--", "cat"], 0, "%0\n", "")),
        s(call(&LIST_PANES, 0, "%0\n", "")),
        s(call(&["set-option", "-w", "-t", "claude-swarm:swarm-view", "pane-border-status", "top"], 0, "", "")),
    ];
    v.extend(decorate(lead, "%0", "blue", "alpha"));
    v.push(s(call(&LIST_PANES, 0, "%0\n", "")));
    v.push(s(call(&HAS_SESSION, 0, "", "")));
    v.push(s(call(&["set-option", "-p", "-t", "%0", "remain-on-exit", "failed"], 0, "", "")));
    v.push(s(call(&LIST_WINDOWS, 0, "swarm-view\n", "")));
    v.push(s(call(&["respawn-pane", "-k", "-t", "%0", "--", &respawn("alpha", "blue")], 0, "", "")));
    // beta
    for _ in 0..3 {
        v.push(s(call(&LIST_PANES, 0, "%0\n", "")));
    }
    v.push(s(call(&["split-window", "-d", "-t", "%0", "-v", "-P", "-F", "#{pane_id}", "--", "cat"], 0, "%1\n", "")));
    v.extend(decorate(lead, "%1", "green", "beta"));
    v.push(s(call(&LIST_PANES, 0, "%0\n%1\n", "")));
    v.push(s(call(&["select-layout", "-t", "claude-swarm:swarm-view", "tiled"], 0, "", "")));
    v.push(s(call(&["set-option", "-p", "-t", "%1", "remain-on-exit", "failed"], 0, "", "")));
    v.push(s(call(&["respawn-pane", "-k", "-t", "%1", "--", &respawn("beta", "green")], 0, "", "")));
    // gamma
    v.push(s(call(&HAS_SESSION, 0, "", "")));
    v.push(s(call(&LIST_WINDOWS, 0, "swarm-view\n", "")));
    for _ in 0..3 {
        v.push(s(call(&LIST_PANES, 0, "%0\n%1\n", "")));
    }
    v.push(s(call(&["split-window", "-d", "-t", "%0", "-h", "-P", "-F", "#{pane_id}", "--", "cat"], 0, "%2\n", "")));
    v.extend(decorate(lead, "%2", "yellow", "gamma"));
    v.push(s(call(&LIST_PANES, 0, "%0\n%1\n%2\n", "")));
    v.push(s(call(&["select-layout", "-t", "claude-swarm:swarm-view", "tiled"], 0, "", "")));
    v.push(s(call(&["set-option", "-p", "-t", "%2", "remain-on-exit", "failed"], 0, "", "")));
    v.push(s(call(&["respawn-pane", "-k", "-t", "%2", "--", &respawn("gamma", "yellow")], 0, "", "")));
    // /exit → "Exit and stop tasks": kill-pane for every pane.
    for p in ["%0", "%1", "%2"] {
        v.push(s(call(&["kill-pane", "-t", p], 0, "", "")));
    }
    assert_eq!(v.len(), 46, "the recorded trace has 46 calls");
    v
}

/// Calls outside the frozen surface (each must be a deviation when the lead
/// itself sends it): an unknown subcommand, an unknown flag, an unknown
/// format, a respawn with two command words.
pub fn deviations(lead: i32) -> Vec<Vec<String>> {
    let s = |args: &[&str]| -> Vec<String> {
        let mut v = vec!["-L".to_string(), format!("claude-swarm-{lead}")];
        v.extend(args.iter().map(|a| a.to_string()));
        v
    };
    vec![
        s(&["display-message", "-p", "#{pane_id}"]),
        s(&["split-window", "-d", "-Z", "-t", "%0", "-h", "-P", "-F", "#{pane_id}", "--", "cat"]),
        s(&["list-panes", "-t", "claude-swarm:swarm-view", "-F", "#{pane_index}"]),
        s(&["respawn-pane", "-k", "-t", "%0", "--", "a", "b"]),
    ]
}
