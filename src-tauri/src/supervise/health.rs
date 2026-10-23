//! "Is everything running well?" — the health surface (agents, dispatch hosts,
//! leftovers, memory) and the alerts that tell the user what to do.
//!
//! Gilde's own registry only knows Gilde's own terminal sessions. The agents a
//! preview user cares about are somebody else's children: the members of a
//! Claude Code Orchestrator TEAM (each turn a vendor CLI in a worktree under
//! `<data root>/worktrees/<hash>/<team slug>/<member>`, spawned by the dispatch
//! host of the lead's session) and those dispatch HOSTS themselves. This module
//! looks at them through the watchdog's own 5 s `ps` snapshot — it never takes a
//! second one — joined with the team facts Gilde already reads (the sidecar's
//! read-only observer, `crate::sidecar::TeamCache`).
//!
//! The split is the watchdog's: [`HealthBuilder::build`] is pure apart from the
//! injected [`Probe`] (cwd and start-time reads), so every rule here — the
//! attribution, the two-tick orphan debounce, the alert lifecycle — is tested
//! on synthetic scans without a process in sight.
//!
//! ## What this module never does
//!
//! It signals nothing. A leftover is OFFERED for clean-up, through the existing
//! kill-confirm flow and `procmon_kill`'s identity fences; a team member using
//! too much memory is reported with the `cco` line that stops it from the lead
//! session — Gilde must not kill a member behind the team engine's back, it
//! would corrupt the turn.
//!
//! ## Which processes are agents
//!
//! A `ps` row is an agent when its argv\[0\] IS a CLI — its basename is
//! claude, codex, cursor-agent, agy or gemini, or it is `node`/`bun` running
//! that CLI's entry script ([`agent_kind`]). A token elsewhere in argv never
//! makes a process an agent: `screen … claude`, `caffeinate -i claude`,
//! `script -q log claude`, `nohup`/`time`/`env`/a shell/`ssh`/an editor/a
//! python wrapper are wrappers, and the agent is their child.
//!
//! ## Attribution
//!
//! First match wins:
//!
//! 1. `teamMember` — its pid is a member's running `turn.pid` in a team whose
//!    host is believed alive (a pid read from a dead host's `state.json` may be
//!    recycled, so it is not trusted);
//! 2. `gildeSession` — it sits inside a live registry row's process subtree
//!    that belongs to a Gilde terminal session;
//! 3. `teamMember` — its environment carries the engine's
//!    `CCO_TEAM_MEMBER=<teamId>:<member>:<incarnation>` ([`member_marker`];
//!    same-uid processes, best effort, cached per pid + start time). This is
//!    the ONLY engine-origin evidence, and the only attribution a leftover can
//!    stand on; the team is named from the marker's id, never from a directory;
//! 4. `teamMember`, display only — its cwd is the worktree of a member that is
//!    running right now under a live host (the memory figure on that member's
//!    row). Never a leftover;
//! 5. `other` — a user's own CLI: totals only, never alerted on except for
//!    memory, never a leftover. An environment that cannot be read lands here
//!    for leftover purposes (fail closed).
//!
//! ## Leftovers (orphans)
//!
//! Only an agent attributed by the marker (rule 3), held for [`ORPHAN_TICKS`]
//! consecutive ticks (keyed by pid + start time, so a turn that is just ending
//! does not flap):
//!
//! * (a) its parent chain holds no live dispatch host (recognised by argv —
//!   `server.js --host`, installed or from a checkout — or by the observer's
//!   host pids) and not Gilde, and every ancestor below launchd is a launch
//!   wrapper (`sh -c`, `node`, `env`, …). A login or interactive shell, a
//!   terminal, a multiplexer or anything else above it means a person runs it;
//! * (b) the marker's team left sight, or it ended / was suspended / lost its
//!   host, or the member is gone, is another incarnation, or is not running
//!   the turn the marker names; Gilde session: its registry row is classified
//!   `oursOrphan` and Gilde is gone.
//!
//! The clean-up offered for a leftover names the leftover agent pid only; the
//! kill path takes that pid and its own descendants, nothing above it.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};

use crate::supervise::PressureLevel;
use crate::sidecar::{CcoHostInfo, CcoTeamView};
use crate::store::models::ProcRow as RegistryRow;
use crate::supervise::procscan::Snapshot;
use crate::supervise::{ProcGroupKind, ProcRow, ProcTag};

// ---------------------------------------------------------------------------
// constants
// ---------------------------------------------------------------------------

const GIB: u64 = 1024 * 1024 * 1024;

/// An agent's pgid subtree at or above this is worth a look (attention).
pub const MEM_ATTENTION_BYTES: u64 = 4 * GIB;
/// …and at or above this — or [`MEM_ACTION_RAM_FRACTION`] of physical RAM,
/// whichever is lower — it needs the user (action).
pub const MEM_ACTION_BYTES: u64 = 8 * GIB;
/// See [`MEM_ACTION_BYTES`].
pub const MEM_ACTION_RAM_FRACTION: f64 = 0.25;
/// A raised memory alert holds until the subtree falls below this fraction of
/// its threshold (the watchdog's own hysteresis ratio).
pub const MEM_CLEAR_RATIO: f64 = 0.90;
/// Consecutive ticks a leftover condition must hold before it is one.
pub const ORPHAN_TICKS: u32 = 2;
/// A dismissed alert is not raised again for this long unless it escalates.
pub const DISMISS_COOLDOWN_MS: i64 = 10 * 60_000;
/// The longest turn timeout any team can be given (the engine allows 5–240
/// minutes). The snapshot does not carry a team's own limit, so a turn is
/// only called stuck once it has run longer than ANY team allows — never
/// while it may still be inside its own team's limit.
pub const MAX_TEAM_TURN_TIMEOUT_MINUTES: i64 = 240;
/// A turn is stuck only this long after that limit…
pub const STUCK_GRACE_MS: i64 = 2 * 60_000;
/// …with no activity for this whole window: no `lastActivity` / `lastOutputAt`
/// in it when the snapshot carries either, and its processes under
/// [`STUCK_CPU_PCT`] when sampled.
pub const STUCK_IDLE_WINDOW_MS: i64 = 5 * 60_000;
/// See [`STUCK_IDLE_WINDOW_MS`].
pub const STUCK_CPU_PCT: f32 = 1.0;
/// A dispatch host whose lead session is gone is reported after this long (it
/// exits by itself; this is informational).
pub const HOST_NO_LEAD_MS: i64 = 10 * 60_000;
/// Two start-time readings within this many milliseconds name one process
/// (`START_SLACK_MS` of the host protocol).
pub const START_SLACK_MS: i64 = 5_000;
/// An unreadable cwd / environment is asked again after this long.
pub const READ_RETRY_MS: i64 = 60_000;
/// A dispatch host's script: argv is `<runtime> […/]server.js --host <spec>`,
/// whether it runs from the installed data root or from a checkout.
pub const HOST_SCRIPT: &str = "server.js";
/// The token right after the script (`HOST_ARG` of the host protocol).
pub const HOST_ARG: &str = "--host";
/// `<teamId>:<member>:<incarnation>` — set by the Orchestrator engine in the
/// environment of every member turn process and take-over terminal
/// (host-protocol `TEAM_MEMBER_ENV`). The ONLY evidence that makes a process
/// a team leftover candidate.
pub const TEAM_MEMBER_ENV: &str = "CCO_TEAM_MEMBER";
/// The turn id the process was started for (host-protocol `DISPATCHED_TASK_ENV`).
pub const DISPATCHED_TASK_ENV: &str = "CCO_DISPATCHED_TASK";
/// Ancestors that only launch a CLI and are no owner of their own. Anything
/// else above an agent (a login or interactive shell, a terminal, a
/// multiplexer, an editor, `caffeinate`, `script`, …) means a person runs it.
const LAUNCH_WRAPPERS: &[&str] = &["sh", "bash", "zsh", "dash", "env", "node", "npm", "npx", "bun", "nohup"];
/// At most this many wrappers between launchd and a leftover.
const MAX_WRAPPERS: usize = 3;

/// The tunable thresholds. `Default` is the constants above; tests lower them.
#[derive(Debug, Clone, PartialEq)]
pub struct HealthConfig {
    pub mem_attention_bytes: u64,
    pub mem_action_bytes: u64,
    pub mem_action_ram_fraction: f64,
    pub orphan_ticks: u32,
    pub dismiss_cooldown_ms: i64,
    /// The turn length past which (plus the grace) a quiet turn is stuck.
    pub turn_timeout_minutes: i64,
    pub stuck_grace_ms: i64,
    pub stuck_idle_window_ms: i64,
    pub stuck_cpu_pct: f32,
    pub host_no_lead_ms: i64,
}

impl Default for HealthConfig {
    fn default() -> Self {
        HealthConfig {
            mem_attention_bytes: MEM_ATTENTION_BYTES,
            mem_action_bytes: MEM_ACTION_BYTES,
            mem_action_ram_fraction: MEM_ACTION_RAM_FRACTION,
            orphan_ticks: ORPHAN_TICKS,
            dismiss_cooldown_ms: DISMISS_COOLDOWN_MS,
            turn_timeout_minutes: MAX_TEAM_TURN_TIMEOUT_MINUTES,
            stuck_grace_ms: STUCK_GRACE_MS,
            stuck_idle_window_ms: STUCK_IDLE_WINDOW_MS,
            stuck_cpu_pct: STUCK_CPU_PCT,
            host_no_lead_ms: HOST_NO_LEAD_MS,
        }
    }
}

impl HealthConfig {
    /// `(attention, action)` byte thresholds for one machine. The action line is
    /// the lower of the absolute figure and the RAM fraction; the attention line
    /// never sits above the action line (an 8 GB machine acts at 2 GB).
    pub fn memory_thresholds(&self, physical_ram_bytes: u64) -> (u64, u64) {
        let by_ram = if physical_ram_bytes == 0 {
            u64::MAX
        } else {
            (physical_ram_bytes as f64 * self.mem_action_ram_fraction) as u64
        };
        let action = self.mem_action_bytes.min(by_ram).max(1);
        let attention = self.mem_attention_bytes.min(action).max(1);
        (attention, action)
    }
}

// ---------------------------------------------------------------------------
// wire types — mirrored by `src/lib/ipc.ts` §11b
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum HealthLevel {
    #[default]
    Ok,
    Attention,
    Action,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HealthSeverity {
    Attention,
    Action,
}

impl HealthSeverity {
    pub fn level(self) -> HealthLevel {
        match self {
            HealthSeverity::Attention => HealthLevel::Attention,
            HealthSeverity::Action => HealthLevel::Action,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum HealthAttribution {
    TeamMember {
        team_id: Option<String>,
        team: String,
        member: String,
        worktree: String,
    },
    GildeSession {
        session_id: String,
    },
    Other,
}

impl HealthAttribution {
    fn kind_str(&self) -> &'static str {
        match self {
            HealthAttribution::TeamMember { .. } => "teamMember",
            HealthAttribution::GildeSession { .. } => "gildeSession",
            HealthAttribution::Other => "other",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthAgent {
    pub pid: i32,
    pub pgid: i32,
    pub ppid: i32,
    /// `claude` | `codex` | `cursor-agent` | `agy` | `gemini`.
    pub agent: String,
    pub rss_bytes: u64,
    pub cpu_pct: f32,
    pub elapsed_secs: u64,
    /// Unix millis (`ProcRow::start_time`).
    pub start_time: i64,
    pub attribution: HealthAttribution,
    pub orphan: bool,
    pub pids: Vec<i32>,
    pub row: ProcRow,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthHost {
    pub pid: i32,
    pub rss_bytes: u64,
    pub elapsed_secs: u64,
    pub session_pid: Option<i64>,
    pub lead_alive: Option<bool>,
    pub team_ids: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HealthMemberState {
    Running,
    Idle,
    Leftover,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthMember {
    pub name: String,
    pub state: HealthMemberState,
    pub rss_bytes: u64,
    pub pids: Vec<i32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthTeam {
    pub team_id: Option<String>,
    pub team: String,
    pub status: String,
    pub host_alive: bool,
    pub rss_bytes: u64,
    pub members: Vec<HealthMember>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HealthMemory {
    pub pressure: PressureLevel,
    pub agents_rss_bytes: u64,
    pub physical_ram_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HealthAlertKind {
    Orphans,
    MemAgent,
    MemLow,
    HostLost,
    TurnStuck,
    HostGone,
    HostNoLead,
}

impl HealthAlertKind {
    const fn key(self) -> &'static str {
        match self {
            HealthAlertKind::Orphans => "orphans",
            HealthAlertKind::MemAgent => "memAgent",
            HealthAlertKind::MemLow => "memLow",
            HealthAlertKind::HostLost => "hostLost",
            HealthAlertKind::TurnStuck => "turnStuck",
            HealthAlertKind::HostGone => "hostGone",
            HealthAlertKind::HostNoLead => "hostNoLead",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HealthActionKind {
    CleanUp,
    Details,
    CopyCommand,
    ShowAgents,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthAction {
    pub kind: HealthActionKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pids: Option<Vec<i32>>,
}

impl HealthAction {
    fn plain(kind: HealthActionKind) -> HealthAction {
        HealthAction {
            kind,
            command: None,
            pids: None,
        }
    }

    fn copy(command: String) -> HealthAction {
        HealthAction {
            kind: HealthActionKind::CopyCommand,
            command: Some(command),
            pids: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthAlert {
    pub id: String,
    pub kind: HealthAlertKind,
    pub severity: HealthSeverity,
    pub title_key: String,
    pub body_key: String,
    pub params: serde_json::Map<String, serde_json::Value>,
    pub actions: Vec<HealthAction>,
    pub since: i64,
    pub raised_at: i64,
    pub team_id: Option<String>,
    pub dismissed: bool,
    /// When `dismissed`: the end of its quiet period (the dismissal time plus
    /// the cooldown, epoch ms) — what the UI shows as "back at". `None` while
    /// the alert is active. A dismissal does not make the condition go away:
    /// the popover keeps the alert listed (snoozed) with this time.
    #[serde(default)]
    pub dismissed_until: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthSnapshot {
    pub at: i64,
    pub overall: HealthLevel,
    pub agents: Vec<HealthAgent>,
    pub hosts: Vec<HealthHost>,
    pub orphans: Vec<i32>,
    pub teams: Vec<HealthTeam>,
    pub memory: HealthMemory,
    pub alerts: Vec<HealthAlert>,
    pub teams_known: bool,
}

impl HealthSnapshot {
    /// What `health_get` answers before the first tick.
    pub fn empty() -> HealthSnapshot {
        HealthSnapshot {
            at: 0,
            overall: HealthLevel::Ok,
            agents: Vec::new(),
            hosts: Vec::new(),
            orphans: Vec::new(),
            teams: Vec::new(),
            memory: HealthMemory::default(),
            alerts: Vec::new(),
            teams_known: false,
        }
    }

    /// `(pid, start time in ms)` of every confirmed leftover — the only pids a
    /// health clean-up may signal (`procmon_kill { onlyHealthOrphans }`).
    pub fn orphan_identities(&self) -> Vec<(i32, i64)> {
        self.agents
            .iter()
            .filter(|a| a.orphan)
            .map(|a| (a.pid, a.start_time))
            .collect()
    }

    /// Recompute `overall` from the alerts that are not dismissed.
    fn settle_overall(&mut self) {
        self.overall = self
            .alerts
            .iter()
            .filter(|a| !a.dismissed)
            .map(|a| a.severity.level())
            .max()
            .unwrap_or(HealthLevel::Ok);
    }
}

// ---------------------------------------------------------------------------
// inputs
// ---------------------------------------------------------------------------

/// One team as the health builder sees it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TeamFact {
    pub view: CcoTeamView,
    /// The members that had a running turn in the last LIVE view of this team
    /// (what "lost its host while members were working" is measured against).
    pub running_at_last_live: Vec<String>,
}

/// What Gilde knows about the Claude Code Orchestrator teams right now.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TeamFacts {
    /// False when there is no fresh answer from the observer (the sidecar is
    /// down, or nothing was asked yet): no leftover and no team alert is decided.
    pub known: bool,
    /// `<data root>` (the sidecar's `dataRoot`). Informational: no team is
    /// ever named from a path under it (attribution needs the engine marker).
    pub data_root: Option<String>,
    pub teams: Vec<TeamFact>,
    pub hosts: Vec<CcoHostInfo>,
}

/// Where the health builder gets its team facts. The production one is the
/// sidecar's team cache; tests hand in a fixed set.
pub trait TeamSource: Send + Sync {
    /// The cached facts. Must not block.
    fn facts(&self) -> TeamFacts;
    /// Ask for fresher facts in the background (rate limited by the source).
    fn poke(&self) {}
}

/// The few kernel reads the builder needs, injectable for tests.
pub trait Probe {
    /// Working directory of a live pid.
    fn cwd(&self, pid: i32) -> Option<PathBuf>;
    /// Kernel start time of a live pid, unix millis.
    fn start_ms(&self, pid: i32) -> Option<i64>;
    /// Environment of a live pid (`KEY=VALUE` each). `None` = could not read.
    fn environ(&self, _pid: i32) -> Option<Vec<String>> {
        None
    }
}

/// The kernel, as the running app reads it. The builder runs under the
/// health lock (which `health_dismiss` also takes), so every read here is one
/// syscall and never a child process: `cwd` is `proc_pidinfo` only (no `lsof`
/// fallback — a cwd is display-only), the environment is the same
/// `sysctl(KERN_PROCARGS2)` read the kill path's env marker uses. The builder
/// caches both per pid + start time.
#[derive(Debug, Default, Clone, Copy)]
pub struct LiveProbe;

impl Probe for LiveProbe {
    fn cwd(&self, pid: i32) -> Option<PathBuf> {
        crate::supervise::procscan::cwd_of_no_lsof(pid)
    }

    fn environ(&self, pid: i32) -> Option<Vec<String>> {
        // Same-uid processes only: another user's environment is not ours to
        // read, and could not be read anyway.
        let ours = crate::supervise::procscan::uid_of(std::process::id() as i32)?;
        if crate::supervise::procscan::uid_of(pid)? != ours {
            return None;
        }
        crate::supervise::procscan::environ_of(pid).filter(|env| !env.is_empty())
    }

    fn start_ms(&self, pid: i32) -> Option<i64> {
        if !crate::supervise::spawn::is_alive(pid) {
            return None;
        }
        crate::supervise::procscan::start_time_of(pid).map(|s| s.saturating_mul(1000))
    }
}

/// One tick's worth of input.
pub struct HealthInput<'a> {
    pub at: i64,
    pub snapshot: &'a Snapshot,
    /// The monitor rows the watchdog built from `snapshot`.
    pub rows: &'a [ProcRow],
    /// The live registry rows (Gilde's own processes).
    pub registry: &'a [RegistryRow],
    pub facts: &'a TeamFacts,
    pub pressure: PressureLevel,
    pub physical_ram_bytes: u64,
    /// This process (Gilde). In tests: the test binary.
    pub self_pid: i32,
}

// ---------------------------------------------------------------------------
// classification helpers
// ---------------------------------------------------------------------------

/// The agent CLIs, by their executable's name.
const AGENT_NAMES: &[&str] = &["claude", "codex", "cursor-agent", "agy", "gemini"];

/// Script runners an agent CLI installed from a package registry runs under:
/// `node …/claude-code/cli.js`, `node …/bin/codex` (a `#!/usr/bin/env node`
/// script exec'd by name). Only these — a `python` / `perl` / shell running
/// something that mentions a CLI is a wrapper, never the agent.
const SCRIPT_RUNNERS: &[&str] = &["node", "nodejs", "bun"];

/// Shells (a `sh -c "claude …"` is a wrapper, its child is the agent).
const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "fish", "ksh", "tcsh", "csh"];

/// Commands that run ANOTHER command. Never an agent, whatever follows them;
/// also never the first segment of an install path with a space in it.
const RUNS_ANOTHER: &[&str] = &[
    "env", "nohup", "time", "caffeinate", "script", "screen", "SCREEN", "tmux", "sudo", "nice",
    "ssh", "exec", "timeout", "xargs", "watch", "stdbuf", "arch", "sandbox-exec", "open",
    "python", "python3", "perl", "ruby", "vim", "nvim", "vi", "emacs", "make", "npm", "npx",
];

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn agent_name(name: &str) -> Option<&'static str> {
    AGENT_NAMES.iter().copied().find(|n| *n == name)
}

/// The CLI a script run by a [`SCRIPT_RUNNERS`] runtime is, when it is one.
fn script_agent(script: &str) -> Option<&'static str> {
    let trimmed = script.trim_end_matches('/');
    for entry in ["/claude-code/cli.js", "/claude-code/cli.mjs"] {
        if trimmed.ends_with(entry) {
            return Some("claude");
        }
    }
    let base = basename(trimmed);
    let stem = base
        .strip_suffix(".js")
        .or_else(|| base.strip_suffix(".mjs"))
        .or_else(|| base.strip_suffix(".cjs"))
        .unwrap_or(base);
    agent_name(stem)
}

/// The agent CLI a command line IS — decided by argv\[0\] alone.
///
/// * argv\[0\]'s basename is a CLI name (`claude`, `/opt/bin/claude`);
/// * argv\[0\] is a script runtime (`node`, `bun`) whose script (its first
///   non-option argument) is a CLI's entry (`…/claude-code/cli.js`,
///   `…/bin/codex`);
/// * argv\[0\] is an absolute install path with spaces in its directories
///   (`ps` prints argv joined by spaces): `/…/Application Support/…/claude`.
///   Every continuation must carry on a directory (contain `/`, not start
///   with `/` or `-`) and the first segment must not be a command that runs
///   another one.
///
/// A token ELSEWHERE in argv never makes a process an agent: `screen -dmS x
/// claude`, `caffeinate -i claude`, `script -q log claude`, `/usr/bin/time
/// claude`, `env claude`, `sh -c claude`, `python3 wrap.py claude` are
/// wrappers, and their child is the agent.
pub fn agent_kind(args: &str) -> Option<&'static str> {
    let tokens: Vec<&str> = args.split_whitespace().collect();
    let argv0 = *tokens.first()?;
    let base = basename(argv0);
    if let Some(kind) = agent_name(base) {
        return Some(kind);
    }
    if SCRIPT_RUNNERS.contains(&base) {
        let script = tokens[1..].iter().find(|t| !t.starts_with('-'))?;
        return script_agent(script);
    }
    if argv0.starts_with('/') && !RUNS_ANOTHER.contains(&base) && !SHELLS.contains(&base) {
        let mut path = argv0.to_string();
        for t in tokens.iter().skip(1).take(8) {
            if t.starts_with('/') || t.starts_with('-') || !t.contains('/') {
                break;
            }
            path.push(' ');
            path.push_str(t);
            if let Some(kind) = agent_name(basename(&path)) {
                return Some(kind);
            }
        }
    }
    None
}

/// Whether a command line is a Claude Code Orchestrator dispatch host:
/// `<runtime> […/]server.js --host …` — from the installed data root or from
/// a checkout (`…/out/mcp/server.js`).
pub fn is_dispatch_host(args: &str) -> bool {
    let tokens: Vec<&str> = args.split_whitespace().collect();
    let Some(runtime) = tokens.first().map(|t| basename(t)) else {
        return false;
    };
    if !(SCRIPT_RUNNERS.contains(&runtime) || runtime.ends_with("node")) {
        return false;
    }
    tokens
        .windows(2)
        .skip(1)
        .any(|w| basename(w[0]) == HOST_SCRIPT && w[1] == HOST_ARG)
}

/// Whether an ancestor only launched what runs under it.
///
/// A login or interactive shell (`-zsh`, `zsh -l`, `bash --login`, `zsh -i`,
/// `zsh -lc …`) is a person's — Gilde's own terminals and an editor's
/// integrated terminal look exactly like that — and never a wrapper link.
pub fn is_launch_wrapper(args: &str) -> bool {
    let mut tokens = args.split_whitespace();
    let argv0 = tokens.next().unwrap_or("");
    if argv0.starts_with('-') {
        return false; // a login shell: a person's terminal
    }
    let base = basename(argv0);
    if !LAUNCH_WRAPPERS.contains(&base) {
        return false;
    }
    if SHELLS.contains(&base) {
        // Only the shell's own options, before its command string.
        for t in tokens {
            if t == "--login" || t == "--interactive" || t == "-" {
                return false;
            }
            if t == "--" || !t.starts_with('-') {
                break;
            }
            if !t.starts_with("--") {
                if t.contains('l') || t.contains('i') {
                    return false;
                }
                if t.contains('c') {
                    break; // the next token is the command string
                }
            }
        }
    }
    true
}

/// The engine's mark on a member turn process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberMarker {
    pub team_id: String,
    pub member: String,
    pub incarnation: i64,
    /// `CCO_DISPATCHED_TASK`: the turn id (or `attach-<lease>`).
    pub task: Option<String>,
}

/// [`TEAM_MEMBER_ENV`] (+ [`DISPATCHED_TASK_ENV`]) read from an environment.
/// `None` when the marker is absent or not well formed (a team id the engine
/// would never mint, an empty member, a non-numeric incarnation).
pub fn member_marker(env: &[String]) -> Option<MemberMarker> {
    let value = env
        .iter()
        .find_map(|e| e.strip_prefix(TEAM_MEMBER_ENV).and_then(|r| r.strip_prefix('=')))?;
    let (team_id, rest) = value.split_once(':')?;
    let (member, incarnation) = rest.rsplit_once(':')?;
    if !crate::sidecar::is_cco_team_id(team_id)
        || member.is_empty()
        || member.len() > 128
        || member.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return None;
    }
    let incarnation: i64 = incarnation.parse().ok().filter(|n: &i64| *n >= 0)?;
    let task = env
        .iter()
        .find_map(|e| e.strip_prefix(DISPATCHED_TASK_ENV).and_then(|r| r.strip_prefix('=')))
        .filter(|t| !t.is_empty())
        .map(str::to_string);
    Some(MemberMarker {
        team_id: team_id.to_string(),
        member: member.to_string(),
        incarnation,
        task,
    })
}

/// The `cco` command line, with the name as a plain token (team and member
/// names are `[a-z][a-z0-9-]*`; anything else is quoted so a paste stays one
/// argument).
fn shell_token(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

pub fn stop_member_command(member: &str) -> String {
    format!("cco team_member {} --stop", shell_token(member))
}

pub fn interrupt_member_command(member: &str) -> String {
    format!("cco team_member {} --interrupt", shell_token(member))
}

pub fn resume_team_command(team: &str) -> String {
    format!("cco team_resume --team {}", shell_token(team))
}

/// Whether a team's dispatch host is believed alive right now.
fn host_alive(view: &CcoTeamView) -> bool {
    match view.state.status.as_str() {
        "active" | "readonly" | "damaged" => {
            view.live || (view.believed && view.state.owner.as_ref().is_some_and(|o| o.host_pid > 0))
        }
        _ => false,
    }
}

fn is_inside(path: &Path, root: &str) -> bool {
    if root.is_empty() {
        return false;
    }
    let root = Path::new(root);
    path == root || path.starts_with(root)
}

// ---------------------------------------------------------------------------
// the alert book
// ---------------------------------------------------------------------------

/// One alert the rules want shown this tick.
#[derive(Debug, Clone, PartialEq)]
struct Candidate {
    id: String,
    kind: HealthAlertKind,
    severity: HealthSeverity,
    body_key: String,
    params: serde_json::Map<String, serde_json::Value>,
    actions: Vec<HealthAction>,
    team_id: Option<String>,
    /// What the alert is about, when it is about a SET of things (the
    /// leftovers' `pid-start`s): a member of this set the user has not seen
    /// yet is news; a subset of what they saw is not.
    subjects: BTreeSet<String>,
}

/// What the user was last told (a raise) or last waved away (a dismissal).
#[derive(Debug, Clone)]
struct SeenRecord {
    at: i64,
    severity: HealthSeverity,
    subjects: BTreeSet<String>,
}

impl SeenRecord {
    /// Whether `c` tells the user something this record has not.
    fn is_news(&self, c: &Candidate) -> bool {
        c.severity > self.severity || !c.subjects.is_subset(&self.subjects)
    }
}

/// Raise / update / clear / dismiss, with the 10 minute cooldown.
///
/// Two records per id, both kept for the cooldown and both outliving a clear:
///
/// * `notified` — the last RAISE (what a toast is keyed on: `raisedAt`). A
///   raise is only repeated inside the cooldown for news (a higher severity
///   than the one raised, or a subject not raised yet); otherwise a condition
///   that comes back — or a severity that flaps back up — keeps the old
///   `raisedAt`, so it can never produce a second toast within ten minutes.
/// * `dismissed` — the user's dismissal, at the HIGHER of the severity shown
///   when they dismissed and the one last raised. It holds for the cooldown
///   unless news arrives.
#[derive(Debug, Default, Clone)]
pub struct AlertBook {
    active: BTreeMap<String, HealthAlert>,
    subjects: HashMap<String, BTreeSet<String>>,
    dismissed: HashMap<String, SeenRecord>,
    notified: HashMap<String, SeenRecord>,
}

impl AlertBook {
    fn severity_of(&self, id: &str) -> Option<HealthSeverity> {
        self.active.get(id).map(|a| a.severity)
    }

    fn reconcile(&mut self, candidates: Vec<Candidate>, now: i64, cooldown_ms: i64) -> Vec<HealthAlert> {
        self.dismissed
            .retain(|_, rec| now.saturating_sub(rec.at) < cooldown_ms);
        self.notified
            .retain(|_, rec| now.saturating_sub(rec.at) < cooldown_ms);
        let mut seen: HashSet<String> = HashSet::new();
        for c in candidates {
            if !seen.insert(c.id.clone()) {
                continue; // first candidate of an id wins
            }
            let title_key = format!("health.alert.{}.title", c.kind.key());
            let may_notify = self.notified.get(&c.id).is_none_or(|n| n.is_news(&c));
            let held_by_dismissal = self.dismissed.get(&c.id).is_some_and(|d| !d.is_news(&c));
            let notify = |book: &mut AlertBook, c: &Candidate| {
                book.notified.insert(
                    c.id.clone(),
                    SeenRecord {
                        at: now,
                        severity: c.severity,
                        subjects: c.subjects.clone(),
                    },
                );
            };
            match self.active.get(&c.id).cloned() {
                Some(mut alert) => {
                    let previous = self.subjects.get(&c.id).cloned().unwrap_or_default();
                    let grew = c.severity > alert.severity || !c.subjects.is_subset(&previous);
                    alert.severity = c.severity;
                    alert.title_key = title_key;
                    alert.body_key = c.body_key.clone();
                    alert.params = c.params.clone();
                    alert.actions = c.actions.clone();
                    alert.team_id = c.team_id.clone();
                    if alert.dismissed {
                        if !held_by_dismissal {
                            // News, or the cooldown ran out while it still holds.
                            self.dismissed.remove(&c.id);
                            alert.dismissed = false;
                            alert.dismissed_until = None;
                            if may_notify {
                                alert.raised_at = now;
                                notify(self, &c);
                            }
                        }
                    } else if grew && may_notify {
                        alert.raised_at = now;
                        notify(self, &c);
                    }
                    self.active.insert(c.id.clone(), alert);
                }
                None => {
                    // One line per alert transition: what support needs to answer "why did Gilde say that".
                    tracing::info!(target: "health", id = %c.id, severity = ?c.severity, params = ?c.params, "alert raised");
                    if !held_by_dismissal {
                        self.dismissed.remove(&c.id);
                    }
                    // Back inside a dismissal's cooldown: quiet until that dismissal ends.
                    let dismissed_until = if held_by_dismissal {
                        self.dismissed.get(&c.id).map(|d| d.at.saturating_add(cooldown_ms))
                    } else {
                        None
                    };
                    let raised_at = match self.notified.get(&c.id) {
                        Some(n) if !may_notify => n.at,
                        _ => {
                            notify(self, &c);
                            now
                        }
                    };
                    self.active.insert(
                        c.id.clone(),
                        HealthAlert {
                            id: c.id.clone(),
                            kind: c.kind,
                            severity: c.severity,
                            title_key,
                            body_key: c.body_key.clone(),
                            params: c.params.clone(),
                            actions: c.actions.clone(),
                            since: now,
                            raised_at,
                            team_id: c.team_id.clone(),
                            dismissed: held_by_dismissal,
                            dismissed_until,
                        },
                    );
                }
            }
            self.subjects.insert(c.id, c.subjects);
        }
        self.active.retain(|id, _| {
            let keep = seen.contains(id);
            if !keep {
                tracing::info!(target: "health", id = %id, "alert cleared");
            }
            keep
        });
        self.subjects.retain(|id, _| seen.contains(id));
        self.alerts()
    }

    fn alerts(&self) -> Vec<HealthAlert> {
        let mut out: Vec<HealthAlert> = self.active.values().cloned().collect();
        out.sort_by(|a, b| {
            b.severity
                .cmp(&a.severity)
                .then(a.since.cmp(&b.since))
                .then(a.id.cmp(&b.id))
        });
        out
    }

    /// Dismiss an active alert for `cooldown_ms`. `false` when no such alert is active.
    pub fn dismiss(&mut self, id: &str, now: i64, cooldown_ms: i64) -> bool {
        let Some(alert) = self.active.get_mut(id) else {
            return false;
        };
        alert.dismissed = true;
        alert.dismissed_until = Some(now.saturating_add(cooldown_ms));
        let mut severity = alert.severity;
        if let Some(n) = self.notified.get(id) {
            severity = severity.max(n.severity);
        }
        self.dismissed.insert(
            id.to_string(),
            SeenRecord {
                at: now,
                severity,
                subjects: self.subjects.get(id).cloned().unwrap_or_default(),
            },
        );
        true
    }
}

// ---------------------------------------------------------------------------
// the builder
// ---------------------------------------------------------------------------

type ProcKey = (i32, i64);

/// What an environment read said about a process.
#[derive(Debug, Clone, PartialEq, Eq)]
enum EnvRead {
    /// Could not be read (another uid, a restricted binary): fail closed —
    /// such a process is never a leftover candidate.
    Unreadable,
    NoMarker,
    Marker(MemberMarker),
}

/// Why an agent is attributed to a team member.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Evidence {
    /// Not a team member.
    None,
    /// Its pid is a running turn's pid in a team whose host is alive.
    TurnPid,
    /// The engine's own environment marker — the only leftover evidence.
    Marker(MemberMarker),
    /// Its cwd is the worktree of a member that is running right now under a
    /// live host: DISPLAY only (a member row's memory figure), never a
    /// leftover, never a team named from a directory.
    Cwd,
}

/// The stateful half: caches and debounces that span ticks.
#[derive(Debug, Default)]
pub struct HealthBuilder {
    cfg: HealthConfig,
    /// `(cwd, read at)`. An unreadable one is asked again after [`READ_RETRY_MS`].
    cwd_cache: HashMap<ProcKey, (Option<PathBuf>, i64)>,
    /// `(marker read, read at)`, same retry rule.
    env_cache: HashMap<ProcKey, (EnvRead, i64)>,
    /// Worktree path as written → its symlink-resolved form.
    canon_cache: HashMap<String, Option<PathBuf>>,
    orphan_streak: HashMap<ProcKey, u32>,
    /// `(team key, member, turn id)` → `(at, cpu)` samples.
    turn_cpu: HashMap<(String, String, String), VecDeque<(i64, f32)>>,
    host_no_lead_since: HashMap<ProcKey, i64>,
    book: AlertBook,
    /// How many cwd lookups the last build made (cost diagnostics, tests).
    pub last_cwd_lookups: usize,
    /// How many environment reads the last build made.
    pub last_env_reads: usize,
}

/// One attributed agent before the snapshot is assembled.
struct Built {
    agent: HealthAgent,
    key: ProcKey,
    team_key: Option<String>,
    evidence: Evidence,
}

impl HealthBuilder {
    pub fn new(cfg: HealthConfig) -> HealthBuilder {
        HealthBuilder {
            cfg,
            ..HealthBuilder::default()
        }
    }

    pub fn config(&self) -> &HealthConfig {
        &self.cfg
    }

    pub fn set_config(&mut self, cfg: HealthConfig) {
        self.cfg = cfg;
    }

    /// Dismiss an alert; the caller re-reads [`HealthBuilder::alerts`].
    pub fn dismiss(&mut self, id: &str, now: i64) -> bool {
        self.book.dismiss(id, now, self.cfg.dismiss_cooldown_ms)
    }

    /// Apply the book's current state to a held snapshot (after a dismiss).
    pub fn restamp(&self, snapshot: &mut HealthSnapshot) {
        snapshot.alerts = self.book.alerts();
        snapshot.settle_overall();
    }

    /// One tick.
    pub fn build(&mut self, input: &HealthInput<'_>, probe: &dyn Probe) -> HealthSnapshot {
        let now = input.at;
        let snap = input.snapshot;
        let facts = input.facts;
        let rows_by_pid: HashMap<i32, &ProcRow> = input.rows.iter().map(|r| (r.pid, r)).collect();

        // -- dispatch hosts and agent candidates (one pass over the scan) ----
        // A host is known by its argv (`server.js --host`, installed or from a
        // checkout) or by the host pids the observer read from `run/s<pid>.json`
        // / the teams' owners — the latter only while that pid's argv still
        // carries `--host` (a recycled pid is somebody else).
        let fact_host_pids: HashSet<i32> = facts
            .hosts
            .iter()
            .map(|h| h.host_pid)
            .chain(
                facts
                    .teams
                    .iter()
                    .filter_map(|f| f.view.state.owner.as_ref().map(|o| o.host_pid)),
            )
            .filter_map(|p| i32::try_from(p).ok())
            .filter(|p| *p > 1)
            .collect();
        let mut host_pids: HashSet<i32> = HashSet::new();
        let mut candidates: Vec<(i32, &'static str)> = Vec::new();
        for ps in snap.rows() {
            if ps.pid <= 1 || ps.pid == input.self_pid {
                continue;
            }
            if is_dispatch_host(&ps.args)
                || (fact_host_pids.contains(&ps.pid) && ps.args.split_whitespace().any(|t| t == HOST_ARG))
            {
                host_pids.insert(ps.pid);
                continue;
            }
            if let Some(kind) = agent_kind(&ps.args) {
                candidates.push((ps.pid, kind));
            }
        }
        let candidate_set: HashSet<i32> = candidates.iter().map(|(p, _)| *p).collect();

        // Nested agents (a CLI a CLI started) are folded into the outermost —
        // but never across Gilde: what runs under Gilde is Gilde's, even when
        // Gilde itself was started from an agent's terminal.
        let top: Vec<(i32, &'static str)> = candidates
            .into_iter()
            .filter(|(pid, _)| {
                !snap
                    .ancestors_of(*pid)
                    .iter()
                    .take_while(|a| **a != input.self_pid)
                    .any(|a| candidate_set.contains(a))
            })
            .collect();
        // Subtree walks stop at Gilde, at the dispatch hosts and at every other
        // top-level agent: each pid belongs to one owner.
        let mut children: HashMap<i32, Vec<i32>> = HashMap::with_capacity(snap.len());
        for r in snap.rows() {
            if r.pid != r.ppid {
                children.entry(r.ppid).or_default().push(r.pid);
            }
        }
        let mut stops: HashSet<i32> = host_pids.clone();
        stops.insert(input.self_pid);
        stops.extend(top.iter().map(|(p, _)| *p));

        // -- Gilde's own sessions: pid → session id over their subtrees -------
        let mut gilde_session_of: HashMap<i32, String> = HashMap::new();
        let mut gilde_orphan_session: HashSet<String> = HashSet::new();
        for reg in input.registry {
            let (Some(session_id), Some(pid)) = (
                reg.session_id.as_deref(),
                reg.pid.map(|p| p as i32).filter(|p| *p > 0),
            ) else {
                continue;
            };
            if !reg.state.is_live() || !snap.contains(pid) {
                continue;
            }
            if rows_by_pid
                .get(&pid)
                .is_some_and(|r| r.group == ProcGroupKind::OursOrphan)
            {
                gilde_orphan_session.insert(session_id.to_string());
            }
            let mut pids = vec![pid];
            pids.extend(snap.descendants_of(pid));
            if let Some(pgid) = reg.pgid.map(|p| p as i32).filter(|p| *p > 1) {
                pids.extend(snap.pids_in_group(pgid));
            }
            for p in pids {
                gilde_session_of.entry(p).or_insert_with(|| session_id.to_string());
            }
        }

        // -- team indexes -----------------------------------------------------
        // Running turn pids, only from teams whose host is believed alive.
        let mut turn_owner: HashMap<i32, (usize, String)> = HashMap::new();
        // (worktree path, team index, member) of members RUNNING right now
        // under a live host — the only worktrees a cwd may be matched to, and
        // only for display.
        let mut worktrees: Vec<(String, usize, String)> = Vec::new();
        for (ti, fact) in facts.teams.iter().enumerate() {
            let alive = host_alive(&fact.view);
            if !alive {
                continue;
            }
            for m in &fact.view.state.members {
                if let Some(pid) = m.turn.as_ref().and_then(|t| t.pid) {
                    if let Ok(pid) = i32::try_from(pid) {
                        if pid > 1 {
                            turn_owner.insert(pid, (ti, m.name.clone()));
                        }
                    }
                }
                let running = m.turn.is_some() || matches!(m.state.as_str(), "running" | "attached" | "starting");
                if running && !m.worktree.path.is_empty() {
                    worktrees.push((m.worktree.path.clone(), ti, m.name.clone()));
                }
            }
        }
        // Longest path first: a nested worktree wins over its parent.
        worktrees.sort_by_key(|w| std::cmp::Reverse(w.0.len()));
        // The kernel reports a cwd with symlinks resolved (`/var` →
        // `/private/var`); the engine records the path it was given. Both forms
        // are matched, the resolved one read once per path.
        let wanted_paths: HashSet<String> = worktrees.iter().map(|(p, _, _)| p.clone()).collect();
        for path in &wanted_paths {
            if !self.canon_cache.contains_key(path) {
                let resolved = std::fs::canonicalize(path).ok().filter(|c| c.as_os_str() != path.as_str());
                self.canon_cache.insert(path.clone(), resolved);
            }
        }
        self.canon_cache.retain(|p, _| wanted_paths.contains(p));
        let canon_cache = &self.canon_cache;
        let inside = |cwd: &Path, path: &str| -> bool {
            is_inside(cwd, path)
                || canon_cache
                    .get(path)
                    .and_then(|c| c.as_ref())
                    .is_some_and(|c| cwd.starts_with(c))
        };
        let team_name_of = |team_id: &str| -> String {
            facts
                .teams
                .iter()
                .find(|f| f.view.team_id == team_id)
                .map(|f| f.view.state.name.clone())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| team_id.to_string())
        };

        // -- build the agents -------------------------------------------------
        let mut counted: HashSet<i32> = host_pids.clone();
        let mut built: Vec<Built> = Vec::with_capacity(top.len());
        let mut live_keys: HashSet<ProcKey> = HashSet::new();
        let mut cwd_lookups = 0usize;
        let mut env_reads = 0usize;

        for (pid, kind) in top {
            let Some(ps) = snap.get(pid) else { continue };
            let Some(row) = rows_by_pid.get(&pid).copied() else { continue };
            let key: ProcKey = (pid, row.start_time);
            live_keys.insert(key);

            // pid ∪ descendants ∪ its group — the group only when it LEADS it:
            // a CLI that shares its launcher's group would otherwise sum the
            // launcher's whole family (the host and every other member).
            let mut subtree: Vec<i32> = vec![pid];
            let mut seen: HashSet<i32> = HashSet::from([pid]);
            let mut queue: VecDeque<i32> = VecDeque::from([pid]);
            while let Some(current) = queue.pop_front() {
                for kid in children.get(&current).map(Vec::as_slice).unwrap_or(&[]) {
                    if stops.contains(kid) || !seen.insert(*kid) {
                        continue;
                    }
                    subtree.push(*kid);
                    queue.push_back(*kid);
                }
            }
            if ps.pgid == pid {
                subtree.extend(
                    snap.pids_in_group(pid)
                        .into_iter()
                        .filter(|p| !stops.contains(p) && !seen.contains(p)),
                );
            }
            let mut pids: Vec<i32> = Vec::new();
            let mut rss = 0u64;
            let mut cpu = 0f32;
            for p in subtree {
                if !counted.insert(p) {
                    continue;
                }
                if let Some(r) = snap.get(p) {
                    rss = rss.saturating_add(r.rss_bytes());
                    cpu += r.cpu_pct;
                    pids.push(p);
                }
            }
            pids.sort_unstable();

            // Attribution, first match wins (module docs).
            let mut attribution = HealthAttribution::Other;
            let mut team_key: Option<String> = None;
            let mut evidence = Evidence::None;
            if let Some((ti, member)) = turn_owner.get(&pid) {
                let view = &facts.teams[*ti].view;
                let worktree = view
                    .state
                    .members
                    .iter()
                    .find(|m| &m.name == member)
                    .map(|m| m.worktree.path.clone())
                    .unwrap_or_default();
                attribution = HealthAttribution::TeamMember {
                    team_id: Some(view.team_id.clone()),
                    team: team_name_of(&view.team_id),
                    member: member.clone(),
                    worktree,
                };
                team_key = Some(view.team_id.clone());
                evidence = Evidence::TurnPid;
            } else if let Some(session_id) = gilde_session_of.get(&pid) {
                attribution = HealthAttribution::GildeSession {
                    session_id: session_id.clone(),
                };
            } else {
                let env = match self.env_cache.get(&key) {
                    Some((read, at)) if *read != EnvRead::Unreadable || now - at < READ_RETRY_MS => read.clone(),
                    _ => {
                        env_reads += 1;
                        let read = match probe.environ(pid) {
                            None => EnvRead::Unreadable,
                            Some(env) => match member_marker(&env) {
                                Some(m) => EnvRead::Marker(m),
                                None => EnvRead::NoMarker,
                            },
                        };
                        self.env_cache.insert(key, (read.clone(), now));
                        read
                    }
                };
                if let EnvRead::Marker(marker) = env {
                    let worktree = facts
                        .teams
                        .iter()
                        .find(|f| f.view.team_id == marker.team_id)
                        .and_then(|f| f.view.state.members.iter().find(|m| m.name == marker.member))
                        .map(|m| m.worktree.path.clone())
                        .unwrap_or_default();
                    attribution = HealthAttribution::TeamMember {
                        team_id: Some(marker.team_id.clone()),
                        team: team_name_of(&marker.team_id),
                        member: marker.member.clone(),
                        worktree,
                    };
                    team_key = Some(marker.team_id.clone());
                    evidence = Evidence::Marker(marker);
                } else if !worktrees.is_empty() {
                    let cwd = match self.cwd_cache.get(&key) {
                        Some((cached, at)) if cached.is_some() || now - at < READ_RETRY_MS => cached.clone(),
                        _ => {
                            cwd_lookups += 1;
                            let cwd = probe.cwd(pid);
                            self.cwd_cache.insert(key, (cwd.clone(), now));
                            cwd
                        }
                    };
                    if let Some((path, ti, member)) = cwd
                        .as_ref()
                        .and_then(|cwd| worktrees.iter().find(|(path, _, _)| inside(cwd, path)))
                    {
                        let view = &facts.teams[*ti].view;
                        attribution = HealthAttribution::TeamMember {
                            team_id: Some(view.team_id.clone()),
                            team: team_name_of(&view.team_id),
                            member: member.clone(),
                            worktree: path.clone(),
                        };
                        team_key = Some(view.team_id.clone());
                        evidence = Evidence::Cwd;
                    }
                }
            }

            built.push(Built {
                agent: HealthAgent {
                    pid,
                    pgid: ps.pgid,
                    ppid: ps.ppid,
                    agent: kind.to_string(),
                    rss_bytes: rss,
                    cpu_pct: cpu,
                    elapsed_secs: ps.elapsed_secs,
                    start_time: row.start_time,
                    attribution,
                    orphan: false,
                    pids,
                    row: row.clone(),
                },
                key,
                team_key,
                evidence,
            });
        }
        self.last_cwd_lookups = cwd_lookups;
        self.last_env_reads = env_reads;
        self.cwd_cache.retain(|k, _| live_keys.contains(k));
        self.env_cache.retain(|k, _| live_keys.contains(k));

        // -- leftovers ----------------------------------------------------------
        for b in &mut built {
            let left = self.left_behind(b, input, &host_pids);
            if left {
                let streak = self.orphan_streak.entry(b.key).or_insert(0);
                *streak = streak.saturating_add(1);
                b.agent.orphan = *streak >= self.cfg.orphan_ticks.max(1);
            } else {
                self.orphan_streak.remove(&b.key);
            }
        }
        self.orphan_streak.retain(|k, _| live_keys.contains(k));

        // -- hosts ---------------------------------------------------------------
        let mut hosts: Vec<HealthHost> = Vec::new();
        let mut host_keys: HashSet<ProcKey> = HashSet::new();
        for &pid in &host_pids {
            let Some(ps) = snap.get(pid) else { continue };
            let start = rows_by_pid.get(&pid).map(|r| r.start_time).unwrap_or(0);
            // The lead session: `run/s<pid>.json` (with its start time when the
            // observer reports it), else a team owner naming this host.
            let owner_session = facts.teams.iter().find_map(|f| {
                f.view
                    .state
                    .owner
                    .as_ref()
                    .filter(|o| o.host_pid == i64::from(pid) && o.session.pid > 0)
                    .map(|o| (o.session.pid, Some(o.session.started_at).filter(|t| *t > 0)))
            });
            let session = facts
                .hosts
                .iter()
                .find(|h| h.host_pid == i64::from(pid) && h.session_pid > 0)
                .map(|h| {
                    let started = h.session_started_at.filter(|t| *t > 0).or_else(|| {
                        owner_session
                            .filter(|(p, _)| *p == h.session_pid)
                            .and_then(|(_, t)| t)
                    });
                    (h.session_pid, started)
                })
                .or(owner_session);
            let session_pid = session.map(|(p, _)| p);
            // Alive = in this scan AND (when its start time is known) the same
            // process: a session pid recycled by another program is not the lead.
            let lead_alive = session.and_then(|(s, started)| {
                let s = i32::try_from(s).ok()?;
                Some(
                    snap.contains(s)
                        && started.is_none_or(|t| {
                            probe
                                .start_ms(s)
                                .is_some_and(|ms| (ms - t).abs() <= START_SLACK_MS)
                        }),
                )
            });
            let mut team_ids: Vec<String> = facts
                .teams
                .iter()
                .filter(|f| {
                    host_alive(&f.view)
                        && f.view
                            .state
                            .owner
                            .as_ref()
                            .is_some_and(|o| o.host_pid == i64::from(pid))
                })
                .map(|f| f.view.team_id.clone())
                .collect();
            team_ids.sort();
            let key = (pid, start);
            host_keys.insert(key);
            if lead_alive == Some(false) {
                self.host_no_lead_since.entry(key).or_insert(now);
            } else {
                self.host_no_lead_since.remove(&key);
            }
            hosts.push(HealthHost {
                pid,
                rss_bytes: ps.rss_bytes(),
                elapsed_secs: ps.elapsed_secs,
                session_pid,
                lead_alive,
                team_ids,
            });
        }
        hosts.sort_by_key(|h| h.pid);
        self.host_no_lead_since.retain(|k, _| host_keys.contains(k));

        // -- memory --------------------------------------------------------------
        let agents_rss: u64 = built
            .iter()
            .map(|b| b.agent.rss_bytes)
            .chain(hosts.iter().map(|h| h.rss_bytes))
            .fold(0u64, |acc, v| acc.saturating_add(v));
        let memory = HealthMemory {
            pressure: input.pressure,
            agents_rss_bytes: agents_rss,
            physical_ram_bytes: input.physical_ram_bytes,
        };

        // -- teams roll-up --------------------------------------------------------
        let teams = self.roll_up(&built, facts);

        // -- turn CPU history (for "stuck") --------------------------------------
        let mut live_turns: HashSet<(String, String, String)> = HashSet::new();
        for fact in &facts.teams {
            if !host_alive(&fact.view) {
                continue;
            }
            for m in &fact.view.state.members {
                let Some(turn) = m.turn.as_ref() else { continue };
                let member_agents: Vec<&Built> = built
                    .iter()
                    .filter(|b| {
                        b.team_key.as_deref() == Some(fact.view.team_id.as_str())
                            && matches!(&b.agent.attribution, HealthAttribution::TeamMember { member, .. } if member == &m.name)
                    })
                    .collect();
                if member_agents.is_empty() {
                    continue;
                }
                let cpu: f32 = member_agents.iter().map(|b| b.agent.cpu_pct).sum();
                let key = (fact.view.team_id.clone(), m.name.clone(), turn.id.clone());
                live_turns.insert(key.clone());
                let hist = self.turn_cpu.entry(key).or_default();
                hist.push_back((now, cpu));
                let keep_from = now - self.cfg.stuck_idle_window_ms * 2;
                while hist.front().is_some_and(|(at, _)| *at < keep_from) {
                    hist.pop_front();
                }
            }
        }
        self.turn_cpu.retain(|k, _| live_turns.contains(k));

        // -- alerts ----------------------------------------------------------------
        let candidates = self.candidates(&built, &hosts, facts, input, probe);
        let alerts = self
            .book
            .reconcile(candidates, now, self.cfg.dismiss_cooldown_ms);

        let mut agents: Vec<HealthAgent> = built.into_iter().map(|b| b.agent).collect();
        agents.sort_by(|a, b| b.rss_bytes.cmp(&a.rss_bytes).then(a.pid.cmp(&b.pid)));
        let mut orphans: Vec<i32> = agents.iter().filter(|a| a.orphan).map(|a| a.pid).collect();
        orphans.sort_unstable();

        let mut out = HealthSnapshot {
            at: now,
            overall: HealthLevel::Ok,
            agents,
            hosts,
            orphans,
            teams,
            memory,
            alerts,
            teams_known: facts.known,
        };
        out.settle_overall();
        out
    }

    /// Conditions (a) and (b) of the leftover rule for one agent, this tick.
    fn left_behind(&self, b: &Built, input: &HealthInput<'_>, host_pids: &HashSet<i32>) -> bool {
        let snap = input.snapshot;
        let pid = b.agent.pid;

        // (b) first — it decides whether (a) is even asked.
        let owner_gone = match &b.agent.attribution {
            HealthAttribution::Other => return false,
            HealthAttribution::GildeSession { .. } => {
                // Its registry row (this pid or the session leader above it) is
                // classified `oursOrphan` by the monitor.
                let row_orphan = std::iter::once(pid)
                    .chain(snap.ancestors_of(pid))
                    .filter_map(|p| input.rows.iter().find(|r| r.pid == p))
                    .any(|r| r.group == ProcGroupKind::OursOrphan && r.has_tag(ProcTag::Ours));
                if !row_orphan {
                    return false;
                }
                // A Gilde session's leftover is one whose Gilde is gone.
                return !snap.ancestors_of(pid).contains(&input.self_pid);
            }
            HealthAttribution::TeamMember { .. } => {
                // Only the engine's own marker makes a team leftover; a turn
                // pid is a running turn, a cwd is display only.
                let Evidence::Marker(marker) = &b.evidence else {
                    return false;
                };
                if !input.facts.known {
                    return false;
                }
                match input.facts.teams.iter().find(|f| f.view.team_id == marker.team_id) {
                    // The team left sight: the marker alone is the evidence.
                    None => true,
                    Some(f) => {
                        let status = f.view.state.status.as_str();
                        if status == "ended" || status == "suspended" || !host_alive(&f.view) {
                            true
                        } else {
                            match f.view.state.members.iter().find(|m| m.name == marker.member) {
                                None => true, // the member was removed
                                Some(m) if m.incarnation != marker.incarnation => true,
                                // A take-over terminal of an attached member.
                                Some(m) if m.state == "attached" || m.attach.is_some() => false,
                                Some(m) => match m.turn.as_ref() {
                                    None => true,
                                    Some(turn) => {
                                        if marker.task.as_deref() == Some(turn.id.as_str()) {
                                            false // this very turn
                                        } else {
                                            match turn.pid.and_then(|p| i32::try_from(p).ok()) {
                                                Some(tp) => {
                                                    let related = tp == pid
                                                        || snap.ancestors_of(pid).contains(&tp)
                                                        || snap.ancestors_of(tp).contains(&pid);
                                                    !related
                                                }
                                                // No pid to compare: a marker for
                                                // another turn is stale, no task
                                                // id says nothing.
                                                None => marker.task.is_some(),
                                            }
                                        }
                                    }
                                },
                            }
                        }
                    }
                }
            }
        };
        if !owner_gone {
            return false;
        }

        // (a) the parent chain: no live host, not Gilde, wrappers only.
        let chain = snap.ancestors_of(pid);
        let mut wrappers = 0usize;
        for a in chain {
            if a <= 1 {
                break;
            }
            if host_pids.contains(&a) || a == input.self_pid {
                return false;
            }
            let Some(row) = snap.get(a) else { return false };
            if !is_launch_wrapper(&row.args) {
                return false;
            }
            wrappers += 1;
            if wrappers > MAX_WRAPPERS {
                return false;
            }
        }
        true
    }

    fn roll_up(&self, built: &[Built], facts: &TeamFacts) -> Vec<HealthTeam> {
        // team key → (team id, name, status, host alive, ordered members)
        struct Acc {
            team_id: Option<String>,
            team: String,
            status: String,
            host_alive: bool,
            members: Vec<HealthMember>,
        }
        let mut teams: BTreeMap<String, Acc> = BTreeMap::new();
        for fact in &facts.teams {
            let v = &fact.view;
            let status = v.state.status.as_str();
            let has_agents = built
                .iter()
                .any(|b| b.team_key.as_deref() == Some(v.team_id.as_str()));
            if !(status == "active" || status == "suspended" || has_agents) {
                continue;
            }
            let alive = host_alive(v);
            let members = v
                .state
                .members
                .iter()
                .map(|m| HealthMember {
                    name: m.name.clone(),
                    state: if alive && matches!(m.state.as_str(), "running" | "attached" | "starting") {
                        HealthMemberState::Running
                    } else {
                        HealthMemberState::Idle
                    },
                    rss_bytes: 0,
                    pids: Vec::new(),
                })
                .collect();
            teams.insert(
                v.team_id.clone(),
                Acc {
                    team_id: Some(v.team_id.clone()),
                    team: if v.state.name.is_empty() { v.team_id.clone() } else { v.state.name.clone() },
                    status: v.state.status.clone(),
                    host_alive: alive,
                    members,
                },
            );
        }
        for b in built {
            let (Some(key), HealthAttribution::TeamMember { team_id, team, member, .. }) =
                (b.team_key.as_ref(), &b.agent.attribution)
            else {
                continue;
            };
            let acc = teams.entry(key.clone()).or_insert_with(|| Acc {
                team_id: team_id.clone(),
                team: team.clone(),
                status: String::new(),
                host_alive: false,
                members: Vec::new(),
            });
            let idx = match acc.members.iter().position(|m| &m.name == member) {
                Some(i) => i,
                None => {
                    acc.members.push(HealthMember {
                        name: member.clone(),
                        state: HealthMemberState::Idle,
                        rss_bytes: 0,
                        pids: Vec::new(),
                    });
                    acc.members.len() - 1
                }
            };
            let m = &mut acc.members[idx];
            m.rss_bytes = m.rss_bytes.saturating_add(b.agent.rss_bytes);
            m.pids.extend(b.agent.pids.iter().copied());
            m.pids.sort_unstable();
            if b.agent.orphan {
                m.state = HealthMemberState::Leftover;
            } else if m.state == HealthMemberState::Idle && acc.host_alive {
                m.state = HealthMemberState::Running;
            }
        }
        let mut out: Vec<HealthTeam> = teams
            .into_values()
            .map(|acc| HealthTeam {
                rss_bytes: acc.members.iter().fold(0u64, |s, m| s.saturating_add(m.rss_bytes)),
                team_id: acc.team_id,
                team: acc.team,
                status: acc.status,
                host_alive: acc.host_alive,
                members: acc.members,
            })
            .collect();
        out.sort_by(|a, b| a.team.cmp(&b.team).then(a.team_id.cmp(&b.team_id)));
        out
    }

    fn candidates(
        &self,
        built: &[Built],
        hosts: &[HealthHost],
        facts: &TeamFacts,
        input: &HealthInput<'_>,
        probe: &dyn Probe,
    ) -> Vec<Candidate> {
        let now = input.at;
        let mut out: Vec<Candidate> = Vec::new();

        // 1. leftovers, one alert per team (and one for Gilde's sessions). The
        //    team is named from the engine's marker (its id → the name the
        //    facts give it), never from a directory name.
        struct Group {
            team_id: Option<String>,
            team: String,
            members: BTreeSet<String>,
            pids: Vec<i32>,
            subjects: BTreeSet<String>,
            rss: u64,
            gilde: bool,
        }
        let mut groups: BTreeMap<String, Group> = BTreeMap::new();
        for b in built.iter().filter(|b| b.agent.orphan) {
            let (key, team_id, team, member, gilde) = match &b.agent.attribution {
                HealthAttribution::TeamMember { team_id: Some(team_id), team, member, .. } => (
                    team_id.clone(),
                    Some(team_id.clone()),
                    team.clone(),
                    Some(member.clone()),
                    false,
                ),
                HealthAttribution::GildeSession { .. } => {
                    ("gilde".to_string(), None, String::new(), None, true)
                }
                _ => continue,
            };
            let entry = groups.entry(key).or_insert_with(|| Group {
                team_id,
                team,
                members: BTreeSet::new(),
                pids: Vec::new(),
                subjects: BTreeSet::new(),
                rss: 0,
                gilde,
            });
            if let Some(m) = member {
                entry.members.insert(m);
            }
            entry.pids.push(b.agent.pid);
            entry.subjects.insert(format!("{}-{}", b.agent.pid, b.agent.start_time));
            entry.rss = entry.rss.saturating_add(b.agent.rss_bytes);
        }
        for (key, mut g) in groups {
            g.pids.sort_unstable();
            let mut params = serde_json::Map::new();
            params.insert("count".into(), g.pids.len().into());
            params.insert("team".into(), g.team.clone().into());
            params.insert(
                "members".into(),
                serde_json::Value::Array(g.members.into_iter().map(Into::into).collect()),
            );
            params.insert("rssBytes".into(), g.rss.into());
            out.push(Candidate {
                id: format!("orphans:{key}"),
                kind: HealthAlertKind::Orphans,
                severity: HealthSeverity::Action,
                body_key: if g.gilde {
                    "health.alert.orphans.bodyGilde".into()
                } else {
                    "health.alert.orphans.body".into()
                },
                params,
                actions: vec![
                    HealthAction {
                        kind: HealthActionKind::CleanUp,
                        command: None,
                        pids: Some(g.pids),
                    },
                    HealthAction::plain(HealthActionKind::Details),
                ],
                team_id: g.team_id,
                subjects: g.subjects,
            });
        }

        // 2. an agent using too much memory (leftovers are covered above).
        let (attention, action) = self.cfg.memory_thresholds(input.physical_ram_bytes);
        for b in built.iter().filter(|b| !b.agent.orphan) {
            let id = format!("mem-agent:{}-{}", b.agent.pid, b.agent.start_time);
            // Hysteresis on BOTH lines: a held alert keeps its level until the
            // subtree falls below [`MEM_CLEAR_RATIO`] of that level's line, so
            // a process hovering at a line does not flap.
            let held = self.book.severity_of(&id);
            let floor = if held.is_some() {
                (attention as f64 * MEM_CLEAR_RATIO) as u64
            } else {
                attention
            };
            if b.agent.rss_bytes < floor {
                continue;
            }
            let action_line = if held == Some(HealthSeverity::Action) {
                (action as f64 * MEM_CLEAR_RATIO) as u64
            } else {
                action
            };
            let severity = if b.agent.rss_bytes >= action_line {
                HealthSeverity::Action
            } else {
                HealthSeverity::Attention
            };
            let mut params = serde_json::Map::new();
            params.insert("agent".into(), b.agent.agent.clone().into());
            params.insert("attribution".into(), b.agent.attribution.kind_str().into());
            params.insert("rssBytes".into(), b.agent.rss_bytes.into());
            let (body_key, actions, team_id) = match &b.agent.attribution {
                HealthAttribution::TeamMember { team_id, team, member, .. } => {
                    let command = stop_member_command(member);
                    params.insert("team".into(), team.clone().into());
                    params.insert("member".into(), member.clone().into());
                    params.insert("command".into(), command.clone().into());
                    (
                        "health.alert.memAgent.bodyMember",
                        vec![HealthAction::copy(command)],
                        team_id.clone(),
                    )
                }
                HealthAttribution::GildeSession { session_id } => {
                    params.insert("sessionId".into(), session_id.clone().into());
                    (
                        "health.alert.memAgent.bodySession",
                        vec![HealthAction::plain(HealthActionKind::Details)],
                        None,
                    )
                }
                HealthAttribution::Other => (
                    "health.alert.memAgent.bodyOther",
                    vec![HealthAction::plain(HealthActionKind::Details)],
                    None,
                ),
            };
            out.push(Candidate {
                id,
                kind: HealthAlertKind::MemAgent,
                severity,
                body_key: body_key.into(),
                params,
                actions,
                team_id,
                subjects: BTreeSet::new(),
            });
        }

        // 3. the machine is short on memory.
        let pressure_severity = match input.pressure {
            PressureLevel::Warn => Some(HealthSeverity::Attention),
            PressureLevel::Critical => Some(HealthSeverity::Action),
            _ => None,
        };
        if let Some(severity) = pressure_severity {
            let mut ranked: Vec<&Built> = built.iter().collect();
            ranked.sort_by_key(|b| std::cmp::Reverse(b.agent.rss_bytes));
            let top: Vec<serde_json::Value> = ranked
                .iter()
                .take(3)
                .map(|b| {
                    let label = match &b.agent.attribution {
                        HealthAttribution::TeamMember { team, member, .. } => format!("{member} ({team})"),
                        _ => b.agent.agent.clone(),
                    };
                    serde_json::json!({ "label": label, "rssBytes": b.agent.rss_bytes })
                })
                .collect();
            let mut params = serde_json::Map::new();
            params.insert(
                "rssBytes".into(),
                built
                    .iter()
                    .map(|b| b.agent.rss_bytes)
                    .chain(hosts.iter().map(|h| h.rss_bytes))
                    .fold(0u64, |a, v| a.saturating_add(v))
                    .into(),
            );
            params.insert("top".into(), serde_json::Value::Array(top));
            out.push(Candidate {
                id: "mem-low".into(),
                kind: HealthAlertKind::MemLow,
                severity,
                body_key: "health.alert.memLow.body".into(),
                params,
                actions: vec![HealthAction::plain(HealthActionKind::ShowAgents)],
                team_id: None,
                subjects: BTreeSet::new(),
            });
        }

        if facts.known {
            let mut lost_teams: HashSet<String> = HashSet::new();
            // 4. a team lost its host while members were working.
            for fact in &facts.teams {
                let v = &fact.view;
                let status = v.state.status.as_str();
                if !(status == "active" || status == "suspended") || host_alive(v) {
                    continue;
                }
                if fact.running_at_last_live.is_empty() {
                    continue;
                }
                let team = if v.state.name.is_empty() { v.team_id.clone() } else { v.state.name.clone() };
                let command = resume_team_command(&team);
                let mut params = serde_json::Map::new();
                params.insert("team".into(), team.into());
                params.insert("command".into(), command.clone().into());
                params.insert(
                    "members".into(),
                    serde_json::Value::Array(
                        fact.running_at_last_live.iter().cloned().map(Into::into).collect(),
                    ),
                );
                lost_teams.insert(v.team_id.clone());
                out.push(Candidate {
                    id: format!("host-lost:{}", v.team_id),
                    kind: HealthAlertKind::HostLost,
                    severity: HealthSeverity::Action,
                    body_key: "health.alert.hostLost.body".into(),
                    params,
                    actions: vec![HealthAction::copy(command)],
                    team_id: Some(v.team_id.clone()),
                    subjects: BTreeSet::new(),
                });
            }

            // 5. a turn is stuck: it has run longer than ANY team allows (the
            //    snapshot does not carry the team's own limit) plus the grace,
            //    with no activity for the idle window. Activity is the
            //    engine's own `lastActivity` / `turn.lastOutputAt` when the
            //    snapshot carries either — low CPU alone never decides then —
            //    and the CPU samples otherwise.
            let timeout_ms = self.cfg.turn_timeout_minutes.max(1) * 60_000;
            for fact in &facts.teams {
                let v = &fact.view;
                if !host_alive(v) {
                    continue;
                }
                for m in &v.state.members {
                    let Some(turn) = m.turn.as_ref() else { continue };
                    if turn.started_at <= 0 {
                        continue;
                    }
                    let elapsed = now - turn.started_at;
                    if elapsed <= timeout_ms + self.cfg.stuck_grace_ms {
                        continue;
                    }
                    let window_start = now - self.cfg.stuck_idle_window_ms;
                    let key = (v.team_id.clone(), m.name.clone(), turn.id.clone());
                    let cpu_quiet = self.turn_cpu.get(&key).map(|hist| {
                        let covered = hist.front().is_some_and(|(at, _)| *at <= window_start);
                        let quiet = hist
                            .iter()
                            .filter(|(at, _)| *at >= window_start)
                            .all(|(_, cpu)| *cpu < self.cfg.stuck_cpu_pct);
                        covered && quiet
                    });
                    let activity_at = [turn.last_output_at, m.last_activity.as_ref().map(|a| a.ts)]
                        .into_iter()
                        .flatten()
                        .filter(|t| *t > 0)
                        .max();
                    let stuck = match activity_at {
                        Some(last) => {
                            let last = last.max(turn.started_at);
                            last <= window_start && cpu_quiet != Some(false)
                        }
                        None => cpu_quiet == Some(true),
                    };
                    if !stuck {
                        continue;
                    }
                    let team = if v.state.name.is_empty() { v.team_id.clone() } else { v.state.name.clone() };
                    let command = interrupt_member_command(&m.name);
                    let mut params = serde_json::Map::new();
                    params.insert("team".into(), team.into());
                    params.insert("member".into(), m.name.clone().into());
                    params.insert("minutes".into(), (elapsed / 60_000).into());
                    params.insert("limitMinutes".into(), self.cfg.turn_timeout_minutes.into());
                    params.insert("command".into(), command.clone().into());
                    out.push(Candidate {
                        id: format!("turn-stuck:{}:{}:{}", v.team_id, m.name, turn.id),
                        kind: HealthAlertKind::TurnStuck,
                        severity: HealthSeverity::Attention,
                        body_key: "health.alert.turnStuck.body".into(),
                        params,
                        actions: vec![HealthAction::copy(command)],
                        team_id: Some(v.team_id.clone()),
                        subjects: BTreeSet::new(),
                    });
                }
            }

            // 6a. a host is gone but its lead session is alive.
            for fact in &facts.teams {
                let v = &fact.view;
                let status = v.state.status.as_str();
                if !(status == "active" || status == "suspended") || host_alive(v) {
                    continue;
                }
                if lost_teams.contains(&v.team_id) {
                    continue; // the action alert already says what to do
                }
                let Some(owner) = v.state.owner.as_ref() else { continue };
                let Ok(session_pid) = i32::try_from(owner.session.pid) else { continue };
                if session_pid <= 1 {
                    continue;
                }
                let lead_alive = probe
                    .start_ms(session_pid)
                    .is_some_and(|ms| (ms - owner.session.started_at).abs() <= START_SLACK_MS);
                if !lead_alive {
                    continue;
                }
                let team = if v.state.name.is_empty() { v.team_id.clone() } else { v.state.name.clone() };
                let mut params = serde_json::Map::new();
                params.insert("team".into(), team.into());
                out.push(Candidate {
                    // One per team: one lead session can own several teams.
                    id: format!("host-gone:{session_pid}:{}", v.team_id),
                    kind: HealthAlertKind::HostGone,
                    severity: HealthSeverity::Attention,
                    body_key: "health.alert.hostGone.body".into(),
                    params,
                    actions: vec![HealthAction::plain(HealthActionKind::Details)],
                    team_id: Some(v.team_id.clone()),
                    subjects: BTreeSet::new(),
                });
            }
        }

        // 6b. a host runs with no live lead session for a while.
        for h in hosts {
            if h.lead_alive != Some(false) {
                continue;
            }
            let start = input
                .rows
                .iter()
                .find(|r| r.pid == h.pid)
                .map(|r| r.start_time)
                .unwrap_or(0);
            let Some(since) = self.host_no_lead_since.get(&(h.pid, start)) else { continue };
            let lasted = now - since;
            if lasted < self.cfg.host_no_lead_ms {
                continue;
            }
            let mut params = serde_json::Map::new();
            params.insert("minutes".into(), (lasted / 60_000).into());
            out.push(Candidate {
                id: format!("host-no-lead:{}", h.pid),
                kind: HealthAlertKind::HostNoLead,
                severity: HealthSeverity::Attention,
                body_key: "health.alert.hostNoLead.body".into(),
                params,
                actions: vec![HealthAction::plain(HealthActionKind::Details)],
                team_id: h.team_ids.first().cloned(),
                subjects: BTreeSet::new(),
            });
        }

        out
    }
}

// ===========================================================================
// Gilde 2: process records and leftovers (contract §3.4.10, §4.2.7, §8.1 Q8)
// ===========================================================================
//
// One pass per shared scan (`procscan::ScanHub`): every agent-ish process
// becomes a `ProcessRecord` (`health_processes`); a record is a **leftover**
// only after its reason held for [`LEFTOVER_SCANS`] consecutive scans. Nothing
// here signals — the Clean-up sheet (`health_stop`) does, after a preview,
// an identity re-check and the 10 s undo window.

use crate::contract::common::Vendor as CVendor;
use crate::contract::ids::ProcessKey as CProcessKey;
use crate::contract::process::{Leftover, LeftoverReason, ProcRole, ProcessRecord};
use crate::contract::services::{ProcInfo, ProcSnapshot};

fn default_signatures() -> &'static crate::supervise::procscan::Signatures {
    static SIGS: std::sync::OnceLock<crate::supervise::procscan::Signatures> = std::sync::OnceLock::new();
    SIGS.get_or_init(crate::supervise::procscan::Signatures::defaults)
}

/// Scans a reason must hold before a record is a leftover.
pub const LEFTOVER_SCANS: u8 = 2;

/// `claude <argv[1]>` invocations that are helpers or short-lived commands,
/// never sessions (contract §5.2.2) [F-47].
pub const VENDOR_HELPER_ARGV1: [&str; 8] =
    ["daemon", "bg-pty-host", "agents", "stop", "logs", "rm", "--version", "-v"];

/// What the registry knows about one live `(pid, start)`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RegistryFact {
    pub spawn_id: String,
    pub kind: Option<crate::store::models::ProcKind>,
    /// The row belongs to this running instance.
    pub current_instance: bool,
    pub parent_uid: Option<String>,
    pub crash_leftover: bool,
    pub quit_survivor: bool,
    /// A40.3: a hosted pane whose processes survived its lead's exit
    /// (`reaper::note_lead_gone_survivor`).
    pub lead_gone_survivor: bool,
    /// A41.2: a process carrying a stopped row's marker outside its tree
    /// (`kill::escapees`); `spawn_id`/`parent_uid` name where it came from.
    pub escapee: bool,
}

impl RegistryFact {
    /// A43.1: the leftover is Gilde's own record (the boot sweep, the quit,
    /// a lead's exit, a stop that left an escapee), not inferred from scans:
    /// it is a leftover from the first scan, without the 2-scan hold.
    pub fn proves_leftover(&self) -> bool {
        self.crash_leftover || self.quit_survivor || self.lead_gone_survivor || self.escapee
    }
}

/// `(pid, start_sec)` → registry fact, for every row of this `GILDE_HOME`
/// (any instance: own rows win over `OtherGilde`, N-05).
pub type RegistryView = HashMap<(i32, i64), RegistryFact>;

/// Memory as the store shows it: 10 MiB steps below 1 GiB, 100 MiB above.
pub fn quantize_bytes(bytes: u64) -> u64 {
    const MIB: u64 = 1 << 20;
    let step = if bytes < 1 << 30 { 10 * MIB } else { 100 * MIB };
    (bytes + step / 2) / step * step
}

fn exec_basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn swarm_lead_pid(argv: &[String]) -> Option<i32> {
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        let value = if a == "-L" {
            it.next().map(String::as_str)
        } else {
            a.strip_prefix("-L")
        };
        if let Some(name) = value.and_then(|v| v.strip_prefix("claude-swarm-")) {
            return name.parse().ok();
        }
    }
    None
}

/// The role of one scanned process (contract §3.4.10).
pub fn role_of(p: &ProcInfo, fact: Option<&RegistryFact>) -> ProcRole {
    let exec = p.exec_path.as_deref().unwrap_or("");
    if exec_basename(exec) == "tmux" && swarm_lead_pid(&p.argv_redacted).is_some() {
        return ProcRole::PaneServer;
    }
    if p.labels.agent_id.is_some() && p.labels.agent_name.is_some() && p.labels.team_name.is_some() {
        return ProcRole::Teammate;
    }
    if p.vendor == Some(CVendor::Claude)
        && p.argv_redacted.get(1).is_some_and(|a| VENDOR_HELPER_ARGV1.contains(&a.as_str()))
    {
        return ProcRole::VendorHelper;
    }
    if let Some(kind) = fact.and_then(|f| f.kind) {
        use crate::store::models::ProcKind as K;
        if matches!(kind, K::Sidecar | K::Probe | K::Git | K::Ide | K::Vendorcli) {
            return ProcRole::GildeHelper;
        }
    }
    if p.vendor.is_some() {
        return ProcRole::Session;
    }
    let joined = p.argv_redacted.join(" ");
    if default_signatures().is_mcp_server(&joined) {
        return ProcRole::ToolServer;
    }
    ProcRole::Other
}

/// Whether a scanned process is shown at all (agent CLIs, swarm servers, tool
/// servers, Gilde's own rows).
pub fn is_candidate(p: &ProcInfo, fact: Option<&RegistryFact>) -> bool {
    fact.is_some()
        || p.vendor.is_some()
        || exec_basename(p.exec_path.as_deref().unwrap_or("")) == "tmux"
        || default_signatures().is_mcp_server(&p.argv_redacted.join(" "))
}

/// The leftover condition of one record this scan, before the 2-scan hold.
pub fn leftover_condition(
    p: &ProcInfo,
    role: ProcRole,
    fact: Option<&RegistryFact>,
    alive: &HashSet<i32>,
) -> Option<LeftoverReason> {
    if let Some(f) = fact {
        if f.quit_survivor {
            return Some(LeftoverReason::QuitSurvivor);
        }
        if f.crash_leftover {
            return Some(LeftoverReason::GildeCrashLeftover);
        }
        if f.lead_gone_survivor {
            return Some(LeftoverReason::TeammateOfGoneLead);
        }
        if f.escapee {
            return Some(LeftoverReason::ToolServerOfEndedSession);
        }
        if f.current_instance {
            return None;
        }
    }
    let parent_gone = p.ppid <= 1 || !alive.contains(&p.ppid);
    match role {
        ProcRole::Teammate if parent_gone => Some(LeftoverReason::TeammateOfGoneLead),
        ProcRole::PaneServer => match swarm_lead_pid(&p.argv_redacted) {
            Some(lead) if !alive.contains(&lead) => Some(LeftoverReason::PaneServerOfDeadLead),
            _ => None,
        },
        ProcRole::ToolServer if p.ppid <= 1 => Some(LeftoverReason::ToolServerOfEndedSession),
        _ => None,
    }
}

/// Holds leftover reasons across scans (keyed by `(pid, start)`).
#[derive(Debug, Default)]
pub struct LeftoverTracker {
    held: HashMap<String, (LeftoverReason, i64, u8)>,
}

impl LeftoverTracker {
    /// One scan: every candidate as a record; `leftover` only after the same
    /// reason held [`LEFTOVER_SCANS`] scans in a row — except a leftover
    /// Gilde's own records prove ([`RegistryFact::proves_leftover`], A43.1),
    /// which is one from the first scan.
    pub fn observe(&mut self, snap: &ProcSnapshot, registry: &RegistryView) -> Vec<ProcessRecord> {
        let alive: HashSet<i32> = snap.procs.iter().map(|p| p.pid).collect();
        let mut seen: HashSet<String> = HashSet::new();
        let mut out = Vec::new();
        for p in &snap.procs {
            let fact = registry.get(&(p.pid, p.start_sec));
            if !is_candidate(p, fact) {
                continue;
            }
            let role = role_of(p, fact);
            let key = p.key.as_str().to_string();
            let leftover = match leftover_condition(p, role, fact, &alive) {
                Some(reason) => {
                    let entry = self.held.entry(key.clone()).or_insert((reason, snap.taken_at, 0));
                    if entry.0 != reason {
                        *entry = (reason, snap.taken_at, 0);
                    }
                    entry.2 = entry.2.saturating_add(1);
                    seen.insert(key.clone());
                    // A43.1: the 2-scan hold applies to inferred leftovers only.
                    let proven = fact.is_some_and(RegistryFact::proves_leftover);
                    (proven || entry.2 >= LEFTOVER_SCANS).then_some(Leftover {
                        reason,
                        since: entry.1,
                        held_scans: entry.2,
                    })
                }
                None => None,
            };
            out.push(ProcessRecord {
                key: p.key.clone(),
                pid: p.pid,
                start_sec: p.start_sec,
                vendor: p.vendor,
                role,
                session_uid: fact
                    .and_then(|f| f.parent_uid.as_deref())
                    .and_then(|u| crate::contract::ids::SessionUid::parse(u).ok()),
                gilde_owned: fact.is_some(),
                rss_bytes: quantize_bytes(p.rss_bytes),
                leftover,
            });
        }
        self.held.retain(|k, _| seen.contains(k));
        out
    }
}

/// The parent chain of `pid` in `snap` as `name (pid)` strings, nearest first
/// (names only, never arguments).
pub fn parent_chain(snap: &ProcSnapshot, pid: i32) -> Vec<String> {
    let by_pid: HashMap<i32, &ProcInfo> = snap.procs.iter().map(|p| (p.pid, p)).collect();
    let mut out = Vec::new();
    let mut cur = by_pid.get(&pid).map(|p| p.ppid);
    let mut guard = 0;
    while let Some(ppid) = cur {
        if ppid <= 0 || guard > 64 {
            break;
        }
        guard += 1;
        match by_pid.get(&ppid) {
            Some(p) => {
                let name = exec_basename(p.exec_path.as_deref().unwrap_or("?"));
                out.push(format!("{name} ({ppid})"));
                cur = Some(p.ppid);
            }
            None => {
                out.push(format!("({ppid})"));
                break;
            }
        }
    }
    out
}

/// A process key for `(pid, start)`.
pub fn process_key(pid: i32, start_sec: i64) -> CProcessKey {
    CProcessKey::new(pid.max(0) as u32, start_sec.max(0) as u64)
}

/// The registry view of one scan: every row with a pid in a state where its
/// process may still live, of any instance of this `GILDE_HOME` (own rows win
/// over `OtherGilde`, N-05), with this run's crash-leftover and quit-survivor
/// marks.
pub async fn registry_view(sup: &crate::supervise::registry::Supervisor) -> RegistryView {
    let me = sup.app_instance_id().to_string();
    let rows = sup
        .db()
        .read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT spawn_id, app_instance, kind, pid, proc_start_ts, parent_uid, session_id
                   FROM proc_registry
                  WHERE pid IS NOT NULL AND proc_start_ts IS NOT NULL
                    AND state IN ('running','adopted','kill_incomplete','planned')",
            )?;
            let mapped = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in mapped {
                out.push(row?);
            }
            Ok(out)
        })
        .await
        .unwrap_or_default();
    let crash = crate::supervise::reaper::crash_leftover_spawn_ids();
    let quit = crate::supervise::reaper::quit_survivor_spawn_ids();
    let lead_gone = crate::supervise::reaper::lead_gone_survivor_spawn_ids();
    let mut view = RegistryView::new();
    for (spawn_id, inst, kind, pid, start, parent, session) in rows {
        view.insert(
            (pid as i32, start),
            RegistryFact {
                kind: crate::store::models::ProcKind::from_db_str(&kind),
                current_instance: inst == me,
                parent_uid: parent.or(session),
                crash_leftover: crash.contains(&spawn_id),
                quit_survivor: quit.contains(&spawn_id),
                lead_gone_survivor: lead_gone.contains(&spawn_id),
                escapee: false,
                spawn_id,
            },
        );
    }
    // A41.2: escapees of stopped rows, where no row names the process.
    for ((pid, start), e) in crate::supervise::kill::escapees() {
        view.entry((pid, start)).or_insert_with(|| RegistryFact {
            spawn_id: e.spawn_id,
            kind: None,
            current_instance: true,
            parent_uid: e.session_uid,
            crash_leftover: false,
            quit_survivor: false,
            lead_gone_survivor: false,
            escapee: true,
        });
    }
    view
}

/// One previewed stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedStop {
    pub key: CProcessKey,
    pub hup_first: bool,
}

/// The health surface of Gilde 2: records, leftovers, the Clean-up flow.
pub struct HealthState {
    tracker: parking_lot::Mutex<LeftoverTracker>,
    records: parking_lot::RwLock<Vec<ProcessRecord>>,
    snapshot: parking_lot::RwLock<Option<std::sync::Arc<ProcSnapshot>>>,
    tokens: crate::lifecycle::quit::ConfirmTokens,
    previews: parking_lot::Mutex<HashMap<String, Vec<PlannedStop>>>,
    groups: std::sync::Arc<parking_lot::Mutex<HashMap<String, std::sync::Arc<std::sync::atomic::AtomicBool>>>>,
}

impl std::fmt::Debug for HealthState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HealthState")
            .field("records", &self.records.read().len())
            .finish()
    }
}

impl Default for HealthState {
    fn default() -> Self {
        HealthState {
            tracker: parking_lot::Mutex::new(LeftoverTracker::default()),
            records: parking_lot::RwLock::new(Vec::new()),
            snapshot: parking_lot::RwLock::new(None),
            tokens: crate::lifecycle::quit::ConfirmTokens::new(),
            previews: parking_lot::Mutex::new(HashMap::new()),
            groups: std::sync::Arc::new(parking_lot::Mutex::new(HashMap::new())),
        }
    }
}

impl HealthState {
    pub fn new() -> std::sync::Arc<HealthState> {
        std::sync::Arc::new(HealthState::default())
    }

    /// One scan: refresh the records; returns the leftovers (for the store).
    pub fn observe(&self, snap: std::sync::Arc<ProcSnapshot>, registry: &RegistryView) -> Vec<ProcessRecord> {
        let records = self.tracker.lock().observe(&snap, registry);
        let leftovers = records.iter().filter(|r| r.leftover.is_some()).cloned().collect();
        *self.records.write() = records;
        *self.snapshot.write() = Some(snap);
        leftovers
    }

    /// `health_processes` (argv never included).
    pub fn records(&self) -> Vec<ProcessRecord> {
        self.records.read().clone()
    }

    pub fn leftovers(&self) -> Vec<ProcessRecord> {
        self.records.read().iter().filter(|r| r.leftover.is_some()).cloned().collect()
    }

    /// `health_process_detail`: the redacted command line, names of the
    /// parent chain, the start time. `NOT_FOUND{process}` when not scanned.
    pub fn detail(
        &self,
        key: &CProcessKey,
    ) -> Result<crate::contract::ipc::HealthProcessDetailRes, crate::contract::IpcError> {
        let snap = self.snapshot.read().clone();
        let Some(snap) = snap else {
            return Err(crate::contract::IpcError::not_found("process"));
        };
        let Some(p) = snap.procs.iter().find(|p| &p.key == key) else {
            return Err(crate::contract::IpcError::not_found("process"));
        };
        let mut command = p.argv_redacted.clone();
        if command.is_empty() {
            if let Some(exec) = &p.exec_path {
                command.push(exec.clone());
            }
        }
        Ok(crate::contract::ipc::HealthProcessDetailRes {
            command_redacted: crate::supervise::redact::redact_argv(&command),
            parent_chain: parent_chain(&snap, p.pid),
            started_at: p.start_sec.saturating_mul(1000),
        })
    }

    /// `health_stop_preview`: each key must be a current leftover and still
    /// the same process (start time); everything else is `refused`.
    pub fn stop_preview(&self, keys: &[CProcessKey]) -> crate::contract::ipc::HealthStopPreviewRes {
        use crate::contract::ipc::{StopMethod, StopRefusal, StopTarget};
        let records = self.records.read().clone();
        let mut targets = Vec::new();
        let mut refused = Vec::new();
        let mut planned = Vec::new();
        for key in keys {
            let Some(rec) = records.iter().find(|r| &r.key == key && r.leftover.is_some()) else {
                refused.push(StopRefusal { process_key: key.clone(), reason: "notLeftover".into() });
                continue;
            };
            match crate::supervise::procscan::start_time_of(rec.pid) {
                Some(s) if (s - rec.start_sec).abs() <= 1 => {}
                Some(_) => {
                    refused.push(StopRefusal { process_key: key.clone(), reason: "identityChanged".into() });
                    continue;
                }
                None => {
                    refused.push(StopRefusal { process_key: key.clone(), reason: "gone".into() });
                    continue;
                }
            }
            let reason = rec.leftover.as_ref().map(|l| l.reason).unwrap_or(LeftoverReason::GildeCrashLeftover);
            let what = match (rec.vendor, rec.role) {
                (Some(v), role) => format!("{} · {:?} ({})", v.as_str(), role, rec.pid),
                (None, role) => format!("{role:?} ({})", rec.pid),
            };
            targets.push(StopTarget {
                process_key: key.clone(),
                what,
                why: reason,
                memory_bytes: rec.rss_bytes,
                how: if rec.gilde_owned { StopMethod::Fenced } else { StopMethod::TermAfterIdentity },
            });
            planned.push(PlannedStop {
                key: key.clone(),
                hup_first: matches!(rec.role, ProcRole::PaneServer),
            });
        }
        let parts: Vec<String> = planned.iter().map(|p| p.key.as_str().to_string()).collect();
        let token = self.tokens.issue(crate::lifecycle::quit::digest_of(&parts));
        self.previews.lock().insert(token.as_str().to_string(), planned);
        crate::contract::ipc::HealthStopPreviewRes { targets, refused, confirm_token: token }
    }

    /// `health_stop`: redeem the token (the previewed targets must still be
    /// leftovers), then stop them **after the 10 s undo window**, each with an
    /// identity re-check at execution and never SIGKILL for a vendor CLI.
    pub fn stop(
        &self,
        token: &crate::contract::ids::ConfirmToken,
        sup: std::sync::Arc<crate::supervise::registry::Supervisor>,
        window: std::time::Duration,
    ) -> Result<crate::contract::ipc::HealthStopRes, crate::contract::IpcError> {
        let planned = self
            .previews
            .lock()
            .remove(token.as_str())
            .ok_or_else(|| crate::contract::IpcError::conflict("token"))?;
        let live: HashSet<String> = self
            .leftovers()
            .into_iter()
            .map(|r| r.key.as_str().to_string())
            .collect();
        let still: Vec<PlannedStop> = planned
            .iter()
            .filter(|p| live.contains(p.key.as_str()))
            .cloned()
            .collect();
        let parts_now: Vec<String> = still.iter().map(|p| p.key.as_str().to_string()).collect();
        self.tokens
            .redeem(token, &crate::lifecycle::quit::digest_of(&parts_now), "targets")?;
        if still.is_empty() {
            return Ok(crate::contract::ipc::HealthStopRes { stopped: 0, survivors: 0, undo_token: None });
        }
        let undo = crate::contract::ids::UndoToken::mint();
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.groups.lock().insert(undo.as_str().to_string(), cancelled.clone());
        let n = still.len() as u32;
        let token_key = undo.as_str().to_string();
        let groups = self.groups.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(window).await;
            // The window closed: the token is spent either way.
            let armed = groups.lock().remove(&token_key).is_some();
            if !armed || cancelled.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            for stop in still {
                let plan = if stop.hup_first {
                    crate::contract::services::SignalPlan::HupThenTerm { grace_ms: 1_500 }
                } else {
                    crate::contract::services::SignalPlan::Term { grace_ms: 2_000 }
                };
                match sup.signal_key(&stop.key, plan).await {
                    Ok(out) if out.survivor => {
                        tracing::info!(target: "supervise", "a leftover survived its stop; it stays a leftover");
                    }
                    Ok(_) => {}
                    Err(err) => {
                        let (code, key, _) = err.log_fields();
                        tracing::info!(target: "supervise", code, key, "a leftover stop was refused at execution");
                    }
                }
            }
        });
        Ok(crate::contract::ipc::HealthStopRes { stopped: n, survivors: 0, undo_token: Some(undo) })
    }

    /// `undo_cancel` for a Clean-up group. `false` once expired or unknown.
    pub fn undo(&self, token: &str) -> bool {
        match self.groups.lock().remove(token) {
            Some(flag) => !flag.swap(true, std::sync::atomic::Ordering::SeqCst),
            None => false,
        }
    }
}

#[cfg(test)]
mod tests;
