//! The hosted-team modes of `fake_agent`: `teammate` (a pane teammate a
//! hosted lead started) and `teammate-lead` (a hosted lead in tmux external
//! mode). Shapes: R3 §2.2 rows #4–#16 and §4 (launch, command line, shared
//! state), R5 §1–§3 (team file, inboxes, task files, transcripts), R6 §5
//! (LP-10 call sequence, LP-14b Esc turn, LP-20 lead registry) and the
//! contract §7.4 / §14.1.
//!
//! Every file goes under `$CLAUDE_CONFIG_DIR`, which must be set (a temp dir
//! in every test); without it the modes refuse to run rather than fall back to
//! a real `~/.claude`.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::term::{self, line, RawMode};
use crate::tui::{self, Scenario, TuiOpts, TurnHooks};

/// The canary every inbox `text` carries (T-priv-3: it must never reach
/// Gilde's state, logs or exports).
pub const INBOX_CANARY: &str = "CANARY-T06-INBOX";
/// The canary in task subjects / descriptions.
pub const TASK_CANARY: &str = "CANARY-T06-TASK";

/// The version the fake writes into transcript rows and the registry (the
/// frozen version of the contract, 2.1.307).
const FAKE_VERSION: &str = "2.1.307";

/// The real lead resets a finished task list 5 s after the last task
/// completed (R5 §3, `g=5000`).
const TASK_RESET_AFTER: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

/// The config dir the team files go under: `$CLAUDE_CONFIG_DIR`, which must
/// be set. There is deliberately **no** fallback to `$HOME/.claude`: a fixture
/// must never write into a real vendor directory (contract §9.10), so a run
/// without the variable exits with [`crate::EXIT_MISSING_ENV`].
fn config_dir() -> PathBuf {
    match std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => {
            eprintln!("fake_agent: CLAUDE_CONFIG_DIR must point at a temp dir; refusing to write team files");
            std::process::exit(crate::EXIT_MISSING_ENV);
        }
    }
}

fn cwd() -> PathBuf {
    let dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    fs::canonicalize(&dir).unwrap_or(dir)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Days since 1970-01-01 → (year, month, day) (H. Hinnant's civil_from_days).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// `2026-10-22T02:00:00.000Z` for epoch milliseconds.
fn iso(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let sod = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        sod / 3600,
        sod % 3600 / 60,
        sod % 60,
        ms.rem_euclid(1000)
    )
}

/// UTC ctime (`Wed Oct 21 04:25:05 2026`, day space-padded) — the registry's
/// `procStart` format [R2 E7].
fn ctime_utc(secs: i64) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let days = secs.div_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let sod = secs.rem_euclid(86_400);
    format!(
        "{} {} {d:>2} {:02}:{:02}:{:02} {y}",
        DAYS[(days + 4).rem_euclid(7) as usize],
        MONTHS[(m - 1) as usize],
        sod / 3600,
        sod % 3600 / 60,
        sod % 60
    )
}

/// The kernel's start time of this process (UTC seconds), for `procStart`.
fn own_start_secs() -> Option<i64> {
    let pid = std::process::id() as i32;
    libproc::libproc::proc_pid::pidinfo::<libproc::libproc::bsd_info::BSDInfo>(pid, 0)
        .ok()
        .map(|b| b.pbi_start_tvsec as i64)
}

/// The project-dir slug: every non-alphanumeric character → `-` [R2 §2].
fn slug(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// POSIX single-quote quoting as the CLI's `shellQuote` does it: a string of
/// `[A-Za-z0-9_./:=@+,-]` stays bare, anything else (and the empty string) is
/// single-quoted with `'\''` for embedded quotes [R3 §4.2].
pub fn shell_quote(s: &str) -> String {
    let bare = !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_./:=@+,-".contains(&b));
    if bare {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Write `bytes` via a temp file + rename (the CLI's `atomicWrite` for
/// inboxes, R5 §2.1).
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

/// Append one JSONL row (rows are written whole, each with its `\n`).
fn append_row(path: &Path, row: &Value) {
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let mut bytes = serde_json::to_vec(row).unwrap_or_default();
        bytes.push(b'\n');
        let _ = f.write_all(&bytes);
        let _ = f.flush();
    }
}

/// Read a JSON array file (an inbox), retrying a torn read; missing → `[]`.
fn read_array(path: &Path) -> Vec<Value> {
    for _ in 0..5 {
        match fs::read(path) {
            Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
                Ok(Value::Array(a)) => return a,
                _ => std::thread::sleep(Duration::from_millis(20)),
            },
            Err(_) => return Vec::new(),
        }
    }
    Vec::new()
}

/// One inbox envelope as the CLI writes it (R5 §2.1).
fn envelope(from: &str, text: String, ts: i64) -> Value {
    json!({
        "from": from,
        "text": text,
        "timestamp": iso(ts),
        "read": false,
        "msgV": 1,
        "msg_id": uuid::Uuid::new_v4().to_string(),
        "type": "message",
    })
}

// ---------------------------------------------------------------------------
// argv
// ---------------------------------------------------------------------------

/// Flags of the claude CLI that take no value (R3 §4.2); every other `--x`
/// flag takes the next argument as its value.
const BOOL_FLAGS: &[&str] = &[
    "--fake-esc-turn",
    "--plan-mode-required",
    "--dangerously-skip-permissions",
    "--strict-mcp-config",
    "--include-unsupported-models",
    "--chrome",
    "--no-chrome",
    "--restricted",
    "--hold",
];

/// `--flag value` pairs and boolean flags, in order. Positional arguments are
/// kept with an empty flag name.
fn parse_flags(args: &[String]) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if let Some((k, v)) = a.strip_prefix("--").and_then(|_| a.split_once('=')) {
            out.push((k.to_string(), Some(v.to_string())));
            i += 1;
        } else if a.starts_with("--") && !BOOL_FLAGS.contains(&a.as_str()) && i + 1 < args.len() {
            out.push((a.clone(), Some(args[i + 1].clone())));
            i += 2;
        } else if a.starts_with("--") {
            out.push((a.clone(), None));
            i += 1;
        } else {
            out.push((String::new(), Some(a.clone())));
            i += 1;
        }
    }
    out
}

fn flag<'a>(flags: &'a [(String, Option<String>)], name: &str) -> Option<&'a str> {
    flags
        .iter()
        .rev()
        .find(|(k, _)| k == name)
        .and_then(|(_, v)| v.as_deref())
}

fn has_flag(flags: &[(String, Option<String>)], name: &str) -> bool {
    flags.iter().any(|(k, _)| k == name)
}

// ---------------------------------------------------------------------------
// teammate
// ---------------------------------------------------------------------------

/// Transcript rows and the lead-inbox `idle_notification` of a pane teammate.
struct Teammate {
    transcript: PathBuf,
    session_id: String,
    cwd: String,
    agent_name: String,
    team_name: String,
    color: Option<String>,
    lead_inbox: PathBuf,
    last_uuid: Option<String>,
}

impl Teammate {
    fn row(&mut self, mut row: Value) {
        let uuid = uuid::Uuid::new_v4().to_string();
        if let Value::Object(m) = &mut row {
            m.insert(
                "parentUuid".into(),
                self.last_uuid.clone().map_or(Value::Null, Value::String),
            );
            m.insert("uuid".into(), Value::String(uuid.clone()));
            m.insert("timestamp".into(), Value::String(iso(now_ms())));
            m.insert("sessionId".into(), Value::String(self.session_id.clone()));
            m.insert("cwd".into(), Value::String(self.cwd.clone()));
            m.insert("entrypoint".into(), Value::String("cli".into()));
            m.insert("version".into(), Value::String(FAKE_VERSION.into()));
            m.insert("agentName".into(), Value::String(self.agent_name.clone()));
            m.insert("teamName".into(), Value::String(self.team_name.clone()));
        }
        append_row(&self.transcript, &row);
        self.last_uuid = Some(uuid);
    }

    fn user_text(&mut self, text: &str) {
        self.row(json!({"type": "user", "message": {"role": "user", "content": text}}));
    }

    fn assistant_text(&mut self, text: &str) {
        let id = format!("msg_fake_{}", uuid::Uuid::new_v4().simple());
        self.row(json!({
            "type": "assistant",
            "message": {
                "id": id,
                "role": "assistant",
                "content": [{"type": "text", "text": text}],
                "stop_reason": "end_turn",
            }
        }));
    }

    /// The only trace of an Esc-interrupted pane-teammate turn [R6 §5 LP-14b].
    fn interrupted(&mut self) {
        self.row(json!({
            "type": "user",
            "message": {"role": "user", "content": [{"type": "text", "text": "[Request interrupted by user]"}]}
        }));
    }

    /// The Stop-hook `idle_notification` to `team-lead` [R5 §2.5].
    fn idle_notification(&mut self) {
        let ts = now_ms();
        let frame = json!({
            "type": "idle_notification",
            "from": self.agent_name,
            "timestamp": iso(ts),
            "idleReason": "available",
            "summary": "fake turn finished",
            "result": "fake result",
        });
        let mut entry = envelope(&self.agent_name, frame.to_string(), ts);
        if let (Some(c), Value::Object(m)) = (&self.color, &mut entry) {
            m.insert("color".into(), Value::String(c.clone()));
        }
        let mut inbox = read_array(&self.lead_inbox);
        inbox.push(entry);
        let _ = write_atomic(
            &self.lead_inbox,
            serde_json::to_vec_pretty(&Value::Array(inbox))
                .unwrap_or_default()
                .as_slice(),
        );
    }
}

impl TurnHooks for Teammate {
    fn on_submit(&mut self, text: &str) {
        self.user_text(text);
    }

    fn on_turn_end(&mut self, interrupted: bool) {
        if interrupted {
            // LP-14b: the interrupt row and NO idle_notification.
            self.interrupted();
        } else {
            self.assistant_text("fake reply");
            self.idle_notification();
        }
    }
}

/// `teammate --agent-id <name>@<team> --agent-name <name> --team-name <team>
/// [--agent-color c] [--parent-session-id id] [--permission-mode m]
/// [--settings json] [--model m] [--work-ms N] [--fake-esc-turn] …`
pub fn mode_teammate(rest: &[String]) -> i32 {
    let flags = parse_flags(rest);
    let agent_id = flag(&flags, "--agent-id");
    let agent_name = flag(&flags, "--agent-name");
    let team_name = flag(&flags, "--team-name");
    let (agent_id, agent_name, team_name) = match (agent_id, agent_name, team_name) {
        (Some(i), Some(n), Some(t)) => (i.to_string(), n.to_string(), t.to_string()),
        (None, None, None) => {
            eprintln!("fake_agent: teammate needs --agent-id, --agent-name and --team-name");
            return crate::EXIT_USAGE;
        }
        _ => {
            // The CLI's own message [R3 §4.2].
            eprintln!(
                "Error: --agent-id, --agent-name, and --team-name must all be provided together"
            );
            return 1;
        }
    };
    let mut opts = TuiOpts::default();
    if let Some(ms) = flag(&flags, "--work-ms") {
        match ms.parse() {
            Ok(v) => opts.work_ms = v,
            Err(_) => {
                eprintln!("fake_agent: --work-ms needs a non-negative integer");
                return crate::EXIT_USAGE;
            }
        }
    }

    let cfg = config_dir();
    let cwd = cwd();
    let session_id = uuid::Uuid::new_v4().to_string();
    let project_dir = cfg.join("projects").join(slug(&cwd));
    if let Err(e) = fs::create_dir_all(&project_dir) {
        eprintln!("fake_agent: cannot create {}: {e}", project_dir.display());
        return crate::EXIT_IO;
    }
    let mut mate = Teammate {
        transcript: project_dir.join(format!("{session_id}.jsonl")),
        session_id,
        cwd: cwd.to_string_lossy().into_owned(),
        agent_name: agent_name.clone(),
        team_name: team_name.clone(),
        color: flag(&flags, "--agent-color").map(str::to_string),
        lead_inbox: cfg
            .join("teams")
            .join(&team_name)
            .join("inboxes")
            .join("team-lead.json"),
        last_uuid: None,
    };
    // Transcript head: the first prompt arrives through the mailbox as a
    // `<teammate-message>` block [R3 §4.1 step 6, R5 §2.2].
    mate.user_text(&format!(
        "<teammate-message teammate_id=\"team-lead\">fake spawn prompt for {agent_name}</teammate-message>"
    ));
    if has_flag(&flags, "--fake-esc-turn") {
        mate.assistant_text("fake partial answer");
        mate.interrupted();
    }

    term::install_signal_handlers();
    let raw = RawMode::enable();
    line(&format!("teammate {agent_id} ready"));
    for arg in rest {
        line(&format!("arg: {arg}"));
    }
    tui::run_claude_tui_in(Scenario::Idle, opts, &mut mate, &mut || {}, raw)
}

// ---------------------------------------------------------------------------
// teammate-lead
// ---------------------------------------------------------------------------

/// Teammate names, in spawn order.
const NAMES: [&str; 8] = [
    "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta",
];
/// Teammate colours: the lead takes the first palette colour (red), members
/// the rest [R3 §2.2 colour map].
const COLOURS: [&str; 7] = [
    "blue", "green", "yellow", "purple", "orange", "pink", "cyan",
];

fn tmux_colour(c: &str) -> &str {
    match c {
        "purple" => "magenta",
        "orange" => "colour208",
        "pink" => "colour205",
        other => other,
    }
}

/// Env the CLI forwards into the pane command when the lead has it set
/// [R3 §4.2 "Forwarded env"]. Lets a test push a canary env value through a
/// pane command (T-priv-argv).
const FORWARDED_ENV: [&str; 8] = [
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "ANTHROPIC_BASE_URL",
    "CLAUDE_CONFIG_DIR",
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "NO_PROXY",
    "CLAUDE_CODE_SUBAGENT_MODEL",
];

struct Member {
    name: String,
    colour: String,
    pane: String,
    joined_at: i64,
}

struct Lead {
    socket: String,
    team: String,
    session_id: String,
    cwd: String,
    model: String,
    cfg: PathBuf,
    torn: Duration,
    created_at: i64,
    members: Vec<Member>,
    /// Every pane this lead created (rollback / exit cleanup).
    panes: Vec<String>,
}

/// Exit status and stdout of one tmux call.
struct TmuxOut {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Lead {
    fn team_dir(&self) -> PathBuf {
        self.cfg.join("teams").join(&self.team)
    }

    fn registry(&self) -> PathBuf {
        self.cfg
            .join("sessions")
            .join(format!("{}.json", std::process::id()))
    }

    /// Run `tmux -L claude-swarm-<pid> <args>` (tmux from `PATH`), print the
    /// argv, return its exit code and output. A tmux that cannot be executed
    /// is exit 127.
    fn tmux(&self, args: &[&str]) -> TmuxOut {
        let mut argv: Vec<String> = vec!["-L".into(), self.socket.clone()];
        argv.extend(args.iter().map(|a| a.to_string()));
        let res = Command::new("tmux")
            .args(&argv)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output();
        line(&format!("tmux: {}", argv.join(" ")));
        match res {
            Ok(o) => TmuxOut {
                code: o.status.code().unwrap_or(1),
                stdout: String::from_utf8_lossy(&o.stdout).trim().to_string(),
                stderr: String::from_utf8_lossy(&o.stderr).trim().to_string(),
            },
            Err(e) => TmuxOut {
                code: 127,
                stdout: String::new(),
                stderr: e.to_string(),
            },
        }
    }

    /// A call whose stdout must be a pane id (`-P -F #{pane_id}`).
    fn tmux_pane(&mut self, args: &[&str]) -> Result<String, String> {
        let o = self.tmux(args);
        let pane = o.stdout.lines().next().unwrap_or("").trim().to_string();
        if o.code == 0 && pane.starts_with('%') {
            self.panes.push(pane.clone());
            Ok(pane)
        } else {
            Err(format!(
                "SwarmPaneError: Failed to create teammate pane: {}",
                o.stderr
            ))
        }
    }

    /// `config.json`, written the CLI's way: truncate + write, **not**
    /// atomic [R5 §1.3] — here with a configurable torn window between the
    /// two halves.
    fn write_config(&self) {
        let mut members = vec![json!({
            "agentId": format!("team-lead@{}", self.team),
            "name": "team-lead",
            "agentType": "team-lead",
            "joinedAt": self.created_at,
            "tmuxPaneId": "leader",
            "cwd": self.cwd,
            "subscriptions": [],
            "backendType": "in-process",
        })];
        for m in &self.members {
            members.push(json!({
                "agentId": format!("{}@{}", m.name, self.team),
                "name": m.name,
                "agentType": "general-purpose",
                "model": self.model,
                "color": m.colour,
                "prompt": spawn_prompt(&m.name),
                "planModeRequired": false,
                "joinedAt": m.joined_at,
                "tmuxPaneId": m.pane,
                "cwd": self.cwd,
                "subscriptions": [],
                "backendType": "tmux",
                "isActive": true,
            }));
        }
        let config = json!({
            "name": self.team,
            "createdAt": self.created_at,
            "leadAgentId": format!("team-lead@{}", self.team),
            "leadSessionId": self.session_id,
            "members": members,
        });
        let bytes = serde_json::to_vec_pretty(&config).unwrap_or_default();
        let path = self.team_dir().join("config.json");
        let _ = fs::create_dir_all(self.team_dir());
        if let Ok(mut f) = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
        {
            let half = bytes.len() / 2;
            let _ = f.write_all(&bytes[..half]);
            let _ = f.flush();
            if !self.torn.is_zero() {
                std::thread::sleep(self.torn);
            }
            let _ = f.write_all(&bytes[half..]);
            let _ = f.flush();
        }
    }

    /// The teammate's inbox: cleared, then the initial prompt [R3 §4.1 step
    /// 6] plus one example of every frame type of R3 §4.3 / R5 §2.3, each
    /// `text` carrying [`INBOX_CANARY`]. Protocol frames are JSON strings
    /// inside `text`. Every envelope is from `team-lead` (a fixture choice:
    /// the inbox exists to give readers every shape, not a real exchange).
    fn write_inbox(&self, name: &str) {
        let ts = now_ms();
        let c = INBOX_CANARY;
        let frames = [
            spawn_prompt(name),
            format!("{c} plain message for {name}"),
            json!({"type": "idle_notification", "from": "team-lead", "timestamp": iso(ts),
                   "idleReason": "available", "summary": c, "result": format!("{c} result")})
            .to_string(),
            json!({"type": "permission_request", "request_id": format!("perm-{ts}-fake01"),
                   "agent_id": name, "tool_name": "Bash", "tool_use_id": "toolu_fake01",
                   "description": c, "input": {"command": c}, "permission_suggestions": []})
            .to_string(),
            json!({"type": "shutdown_request", "requestId": format!("shutdown-{ts}@{name}"),
                   "from": "team-lead", "reason": c, "timestamp": iso(ts)})
            .to_string(),
            json!({"type": "task_assignment", "taskId": "1", "subject": c, "description": c,
                   "assignedBy": "team-lead", "timestamp": iso(ts)})
            .to_string(),
            json!({"type": "plan_approval_request", "from": "team-lead", "timestamp": iso(ts),
                   "planFilePath": "/tmp/fake-project/plan.md", "planContent": c,
                   "requestId": format!("plan_approval-{ts}@{name}")})
            .to_string(),
        ];
        let entries: Vec<Value> = frames
            .into_iter()
            .map(|t| envelope("team-lead", t, ts))
            .collect();
        let path = self.team_dir().join("inboxes").join(format!("{name}.json"));
        let _ = write_atomic(
            &path,
            &serde_json::to_vec_pretty(&Value::Array(entries)).unwrap_or_default(),
        );
    }

    /// The respawn-pane payload [R3 §4.2, R6 §5 LP-10]: one shell string.
    fn command(&self, m: &Member, passthrough: &[(&str, String)]) -> String {
        let mut parts: Vec<String> = vec![
            "cd".into(),
            shell_quote(&self.cwd),
            "&&".into(),
            "env".into(),
            "CLAUDECODE=1".into(),
            "CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1".into(),
        ];
        for var in FORWARDED_ENV {
            if let Some(v) = std::env::var_os(var) {
                parts.push(format!("{var}={}", shell_quote(&v.to_string_lossy())));
            }
        }
        parts.push("CLAUDE_CODE_ATTRIBUTION_PROJECT=''".into());
        match std::env::var_os("CLAUDE_CODE_TEAMMATE_COMMAND").filter(|v| !v.is_empty()) {
            // Quoted as one token: no arguments [R3 §4.2].
            Some(cmd) => parts.push(shell_quote(&cmd.to_string_lossy())),
            None => {
                let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("fake_agent"));
                parts.push(shell_quote(&exe.to_string_lossy()));
                parts.push("teammate".into());
            }
        }
        let id = format!("{}@{}", m.name, self.team);
        for (k, v) in [
            ("--agent-id", id.as_str()),
            ("--agent-name", m.name.as_str()),
            ("--team-name", self.team.as_str()),
            ("--agent-color", m.colour.as_str()),
            ("--parent-session-id", self.session_id.as_str()),
        ] {
            parts.push(k.into());
            parts.push(shell_quote(v));
        }
        for (k, v) in passthrough {
            parts.push((*k).into());
            parts.push(shell_quote(v));
        }
        parts.join(" ")
    }

    /// #7/#8 styling (exits ignored) [R3 §2.2].
    fn style_pane(&self, pane: &str, name: &str, colour: &str) {
        let c = tmux_colour(colour);
        let fg = format!("fg={c}");
        self.tmux(&[
            "set-option",
            "-p",
            "-t",
            pane,
            "window-style",
            &format!("bg=default,{fg}"),
        ]);
        self.tmux(&["set-option", "-p", "-t", pane, "pane-border-style", &fg]);
        self.tmux(&[
            "set-option",
            "-p",
            "-t",
            pane,
            "pane-active-border-style",
            &fg,
        ]);
        self.tmux(&["select-pane", "-t", pane, "-T", name]);
        let format = format!("#[fg={c},bold] #{{pane_title}} #[default]");
        self.tmux(&[
            "set-option",
            "-p",
            "-t",
            pane,
            "pane-border-format",
            &format,
        ]);
    }

    /// Create the pane of teammate number `n` (0-based) [R3 §2.2 #11–#13,
    /// R6 §5 LP-10 sequence]:
    ///
    ///   * `has-session -t claude-swarm`; non-zero → `new-session … -- cat`
    ///     and the teammate reuses that initial pane (#11);
    ///   * session exists → `list-windows` (#12); no `swarm-view` →
    ///     `new-window … -- cat`; otherwise `list-panes` (#4, read like the
    ///     CLI does before a split; its output is not needed here) and a
    ///     `split-window` (#13 = #6 in the swarm window) **targeting the pane
    ///     of `teammates[floor((n-1)/2)]`** — LP-10 observed `-t %0` for both
    ///     later teammates and the contract's PaneHost row 5/6 requires a
    ///     `%n` target, so the target is that pane, not the window name —
    ///     `-v` for an odd count of existing teammates, else `-h`; then
    ///     `select-layout -t claude-swarm:swarm-view tiled`.
    fn create_pane(&mut self, n: usize) -> Result<String, String> {
        if self.tmux(&["has-session", "-t", "claude-swarm"]).code != 0 {
            return self.tmux_pane(&[
                "new-session",
                "-d",
                "-s",
                "claude-swarm",
                "-n",
                "swarm-view",
                "-P",
                "-F",
                "#{pane_id}",
                "--",
                "cat",
            ]);
        }
        let windows = self.tmux(&["list-windows", "-t", "claude-swarm", "-F", "#{window_name}"]);
        if !windows.stdout.lines().any(|w| w.trim() == "swarm-view") {
            return self.tmux_pane(&[
                "new-window",
                "-t",
                "claude-swarm",
                "-n",
                "swarm-view",
                "-P",
                "-F",
                "#{pane_id}",
                "--",
                "cat",
            ]);
        }
        let listed = self.tmux(&[
            "list-panes",
            "-t",
            "claude-swarm:swarm-view",
            "-F",
            "#{pane_id}",
        ]);
        if n == 0 {
            // A session we did not create (a reused server): its first pane.
            let first = listed
                .stdout
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            if first.starts_with('%') {
                self.panes.push(first.clone());
                return Ok(first);
            }
            return Err("SwarmPaneError: Failed to create teammate pane: no pane".into());
        }
        let target = self.members[(n - 1) / 2].pane.clone();
        let dir = if n % 2 == 1 { "-v" } else { "-h" };
        let pane = self.tmux_pane(&[
            "split-window",
            "-d",
            "-t",
            &target,
            dir,
            "-P",
            "-F",
            "#{pane_id}",
            "--",
            "cat",
        ])?;
        self.tmux(&["select-layout", "-t", "claude-swarm:swarm-view", "tiled"]);
        Ok(pane)
    }

    /// #16 for every pane, then the CLI's graceful-exit cleanup: the team
    /// dir is removed (`tasks/<team>` stays) and so is our registry entry
    /// [R3 §5 "Lead exits gracefully"].
    fn shutdown(&self) {
        for pane in &self.panes {
            self.tmux(&["kill-pane", "-t", pane]);
        }
        let _ = fs::remove_dir_all(self.team_dir());
        let _ = fs::remove_file(self.registry());
    }
}

fn spawn_prompt(name: &str) -> String {
    format!("{INBOX_CANARY} fake spawn prompt for {name}")
}

/// The registry entry of a hosted lead: `busy` for as long as it lives
/// (pane-team leads never return to `idle`, R6 §5 LP-20).
fn write_registry(lead: &Lead, started_at: i64) {
    let mut entry = json!({
        "pid": std::process::id(),
        "sessionId": lead.session_id,
        "cwd": lead.cwd,
        "startedAt": started_at,
        "version": FAKE_VERSION,
        "peerProtocol": 1,
        "kind": "interactive",
        "entrypoint": "cli",
        "pidDomain": "darwin",
        "status": "busy",
        "statusUpdatedAt": started_at,
        "updatedAt": started_at,
    });
    if let (Some(start), Value::Object(m)) = (own_start_secs(), &mut entry) {
        m.insert("procStart".into(), Value::String(ctime_utc(start)));
    }
    let path = lead.registry();
    let _ = write_atomic(&path, &serde_json::to_vec(&entry).unwrap_or_default());
}

/// `tasks/<team>/1.json` (+ `.highwatermark`), and a watcher that rewrites
/// it with `status:"pending"` 5 s after it was seen `completed`. The real
/// lead deletes a finished list at that point (R5 §3 `resetTaskList`); this
/// fixture rewrites it instead, so a test can watch one file flip back.
fn start_task_list(lead: &Lead, owner: Option<&str>) {
    let dir = lead.cfg.join("tasks").join(&lead.team);
    let _ = fs::create_dir_all(&dir);
    let mut task = json!({
        "id": "1",
        "subject": format!("{TASK_CANARY} fake task"),
        "description": format!("{TASK_CANARY} fake task description"),
        "activeForm": "Doing the fake task",
        "status": "pending",
        "blocks": [],
        "blockedBy": [],
    });
    // `owner` is optional in the schema: absent, never null [R5 §3].
    if let Some(o) = owner {
        task["owner"] = Value::String(o.to_string());
    }
    let path = dir.join("1.json");
    let _ = write_atomic(&path, &serde_json::to_vec_pretty(&task).unwrap_or_default());
    let _ = fs::write(dir.join(".highwatermark"), "1");
    std::thread::spawn(move || {
        let mut completed_since: Option<Instant> = None;
        loop {
            std::thread::sleep(Duration::from_millis(100));
            let parsed = fs::read(&path)
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
            let Some(mut task) = parsed else { continue };
            if task.get("status").and_then(Value::as_str) != Some("completed") {
                completed_since = None;
                continue;
            }
            let since = *completed_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= TASK_RESET_AFTER {
                task["status"] = Value::String("pending".into());
                let _ = write_atomic(&path, &serde_json::to_vec_pretty(&task).unwrap_or_default());
                completed_since = None;
            }
        }
    });
}

/// `teammate-lead [--teammates N] [--team NAME] [--torn-ms MS] [--hold]
/// [--permission-mode m] [--settings json] [--model m]`.
pub fn mode_teammate_lead(rest: &[String]) -> i32 {
    let flags = parse_flags(rest);
    if let Some((k, v)) = flags.iter().find(|(k, _)| {
        !matches!(
            k.as_str(),
            "--teammates"
                | "--team"
                | "--torn-ms"
                | "--hold"
                | "--permission-mode"
                | "--settings"
                | "--model"
                | "--work-ms"
        )
    }) {
        let shown = if k.is_empty() {
            v.as_deref().unwrap_or("")
        } else {
            k.as_str()
        };
        eprintln!("fake_agent: teammate-lead: unknown argument {shown:?}");
        return crate::EXIT_USAGE;
    }
    let num = |name: &str, default: u64| -> Result<u64, i32> {
        match flag(&flags, name) {
            None => Ok(default),
            Some(v) => v.parse().map_err(|_| {
                eprintln!("fake_agent: {name} needs a non-negative integer");
                crate::EXIT_USAGE
            }),
        }
    };
    let (teammates, torn_ms, work_ms) = match (
        num("--teammates", 1),
        num("--torn-ms", 0),
        num("--work-ms", 300),
    ) {
        (Ok(a), Ok(b), Ok(c)) => (a as usize, b, c),
        (Err(c), _, _) | (_, Err(c), _) | (_, _, Err(c)) => return c,
    };

    let session_id = uuid::Uuid::new_v4().to_string();
    let team = flag(&flags, "--team")
        .map(str::to_string)
        .unwrap_or_else(|| format!("session-{}", &session_id[..8]));
    let started_at = now_ms();
    let mut lead = Lead {
        socket: format!("claude-swarm-{}", std::process::id()),
        team,
        session_id,
        cwd: cwd().to_string_lossy().into_owned(),
        model: flag(&flags, "--model").unwrap_or("fake-model").to_string(),
        cfg: config_dir(),
        torn: Duration::from_millis(torn_ms),
        created_at: started_at,
        members: Vec::new(),
        panes: Vec::new(),
    };
    // What the lead forwards into each pane command when it got it itself.
    let passthrough: Vec<(&str, String)> = ["--permission-mode", "--settings", "--model"]
        .into_iter()
        .filter_map(|k| flag(&flags, k).map(|v| (k, v.to_string())))
        .collect();

    term::install_signal_handlers();
    let mut raw = RawMode::enable();

    // Startup: the team exists before any teammate [R5 §1.1], the registry
    // says busy, the lead inbox and the task list are there.
    write_registry(&lead, started_at);
    lead.write_config();
    let _ = write_atomic(
        &lead.team_dir().join("inboxes").join("team-lead.json"),
        b"[]",
    );
    start_task_list(&lead, (teammates > 0).then_some(NAMES[0]));

    for n in 0..teammates {
        if let Some(sig) = term::term_signal() {
            raw.restore();
            lead.shutdown();
            term::reraise(sig);
        }
        let name = NAMES
            .get(n)
            .map_or_else(|| format!("mate-{n}"), |s| s.to_string());
        let colour = COLOURS[n % COLOURS.len()].to_string();
        let pane = match lead.create_pane(n) {
            Ok(p) => p,
            Err(msg) => {
                line(&msg);
                lead.shutdown();
                return 1;
            }
        };
        if n == 0 {
            // #9, first teammate only.
            lead.tmux(&[
                "set-option",
                "-w",
                "-t",
                "claude-swarm:swarm-view",
                "pane-border-status",
                "top",
            ]);
        }
        lead.style_pane(&pane, &name, &colour);
        lead.members.push(Member {
            name: name.clone(),
            colour,
            pane: pane.clone(),
            joined_at: now_ms(),
        });
        // R3 §4.1 steps 4 and 6: record the pane, then the mailbox.
        lead.write_config();
        lead.write_inbox(&name);
        // #14 (exit ignored), #15 (must exit 0).
        lead.tmux(&["set-option", "-p", "-t", &pane, "remain-on-exit", "failed"]);
        let member = lead.members.last().expect("just pushed");
        let command = lead.command(member, &passthrough);
        if lead
            .tmux(&["respawn-pane", "-k", "-t", &pane, "--", &command])
            .code
            != 0
        {
            line(&format!(
                "SwarmPaneError: Failed to send command to pane {pane}"
            ));
            lead.shutdown();
            return 1;
        }
    }
    line("lead ready");

    if has_flag(&flags, "--hold") {
        loop {
            if let Some(sig) = term::term_signal() {
                raw.restore();
                lead.shutdown();
                term::reraise(sig);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    let opts = TuiOpts {
        work_ms,
        ..TuiOpts::default()
    };
    let mut hooks = tui::NoHooks;
    let lead_ref = &lead;
    tui::run_claude_tui_in(
        Scenario::Idle,
        opts,
        &mut hooks,
        &mut || lead_ref.shutdown(),
        raw,
    )
}
