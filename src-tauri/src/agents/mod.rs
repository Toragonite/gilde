//! Agent CLI discovery — DESIGN §6-6, §3 `agents`, M1-PLAN T09.
//!
//! Four things live under `agents/`:
//!
//! * [`path`] — what `PATH` every child of this app gets.
//! * this file — which agent CLIs exist, where, and at what version.
//! * [`launch`] — how to start each one interactively.
//! * [`signatures`] — the hot-reloadable pattern file both the status engine
//!   and the Process Monitor read.
//!
//! ## Detection is allowed to find nothing
//!
//! `cursor-agent` and `gemini` are not installed on this machine (DESIGN R11).
//! That is a fact to report, not an error to raise: [`detect`] always returns
//! one [`AgentBinary`] per [`AgentKind`], with `installed: false` for the ones
//! that are absent. A boot that fails because an optional CLI is missing would
//! be a worse app than one that says "Cursor: not installed".
//!
//! ## Every probe is registered before it runs
//!
//! M1-PLAN 절대 규칙 4: *all* spawns go through the PID registry, `--version`
//! probes included. A cold `claude` probe has been measured at 20+ seconds, and
//! a user staring at the Process Monitor deserves to see what that `claude`
//! process is. The registry itself is T08's; this module talks to it through
//! [`SpawnRecorder`] so the two tasks can land in either order and so tests can
//! run with no database at all.

pub mod classify;
pub mod launch;
pub mod path;
pub mod signatures;

use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};

use crate::config::model::AppConfig;
use crate::config::ConfigStore;
use crate::error::Result;
use crate::paths::AppPaths;
use crate::store::dao;
use crate::store::models::AgentBinaryRow;
use crate::store::Db;

pub use launch::{templates, LaunchTemplate, PromptMode};
pub use path::{resolve_path, PathReport, PathSource};
pub use signatures::{load_signatures, watch_signatures, Signatures, SignaturesReport};

/// Hard cap on a single `--version` probe (M1-PLAN T09.4).
pub const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a detection result stays warm (M1-PLAN T09.4).
pub const DETECT_TTL: Duration = Duration::from_secs(60);

/// How long a probe that overran gets after `SIGTERM` (never `SIGKILL`).
const PROBE_KILL_GRACE: Duration = Duration::from_millis(1_500);

// ---------------------------------------------------------------------------
// AgentKind
// ---------------------------------------------------------------------------

/// The CLIs this app knows how to drive (DECISIONS §제품 정의).
///
/// The serde spelling is `ipc.ts::AgentKindKey`, which is also the key used in
/// `config.agents.binaries` / `.launchOverrides` and in the `agent_binary`
/// table — one vocabulary end to end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AgentKind {
    Claude,
    Codex,
    #[serde(rename = "cursorAgent")]
    Cursor,
    #[serde(rename = "agy")]
    Antigravity,
    #[serde(rename = "gemini")]
    GeminiCli,
    Shell,
}

impl AgentKind {
    /// The detection kind of a contract vendor.
    pub fn of_vendor(vendor: crate::contract::common::Vendor) -> AgentKind {
        use crate::contract::common::Vendor;
        match vendor {
            Vendor::Claude => AgentKind::Claude,
            Vendor::Codex => AgentKind::Codex,
            Vendor::Cursor => AgentKind::Cursor,
            Vendor::Agy => AgentKind::Antigravity,
        }
    }

    /// Every kind, in the order the UI lists them.
    pub const ALL: [AgentKind; 6] = [
        AgentKind::Claude,
        AgentKind::Codex,
        AgentKind::Cursor,
        AgentKind::Antigravity,
        AgentKind::GeminiCli,
        AgentKind::Shell,
    ];

    /// The `ipc.ts::AgentKindKey` / `config.json` spelling.
    pub const fn key(self) -> &'static str {
        match self {
            AgentKind::Claude => "claude",
            AgentKind::Codex => "codex",
            AgentKind::Cursor => "cursorAgent",
            AgentKind::Antigravity => "agy",
            AgentKind::GeminiCli => "gemini",
            AgentKind::Shell => "shell",
        }
    }

    pub fn from_key(key: &str) -> Option<AgentKind> {
        AgentKind::ALL.into_iter().find(|k| k.key() == key)
    }

    /// The key used inside `agent-signatures.json` (DESIGN §6-4 spells Cursor
    /// as `cursor`).
    pub const fn signature_key(self) -> &'static str {
        match self {
            AgentKind::Cursor => "cursor",
            other => other.key(),
        }
    }

    /// Executable names to look for, most preferred first.
    ///
    /// Cursor ships as `cursor-agent` and, on older installs, as the
    /// dangerously generic `agent` — which is why it is the *fallback* and never
    /// the first candidate.
    pub const fn binary_names(self) -> &'static [&'static str] {
        match self {
            AgentKind::Claude => &["claude"],
            AgentKind::Codex => &["codex"],
            AgentKind::Cursor => &["cursor-agent", "agent"],
            AgentKind::Antigravity => &["agy"],
            AgentKind::GeminiCli => &["gemini"],
            AgentKind::Shell => &[],
        }
    }

    /// The environment variable that selects an account directory
    /// (DESIGN §6-6, DECISIONS §우선순위 2).
    pub const fn account_env_key(self) -> Option<&'static str> {
        match self {
            AgentKind::Claude => Some("CLAUDE_CONFIG_DIR"),
            AgentKind::Codex => Some("CODEX_HOME"),
            AgentKind::Antigravity => Some("AGY_HOME"),
            // cursor-agent has no config-dir switch: ambient login only.
            AgentKind::Cursor | AgentKind::GeminiCli | AgentKind::Shell => None,
        }
    }

    /// The `major.minor` this build has actually been exercised against.
    /// `None` ⇒ nothing to compare, so nothing is claimed.
    pub const fn verified_range(self) -> Option<&'static str> {
        match self {
            AgentKind::Claude => Some("2.1"),
            // Re-baselined 2026-09-08: the two-vendor smoke passed on codex-cli
            // 0.153.4 (docs/PINNED-VERSIONS.md §26).
            AgentKind::Codex => Some("0.153"),
            AgentKind::Antigravity => Some("1.1"),
            AgentKind::Cursor | AgentKind::GeminiCli | AgentKind::Shell => None,
        }
    }

    /// The argument that makes this CLI print its version.
    pub const fn version_flag(self) -> &'static str {
        "--version"
    }
}

impl std::fmt::Display for AgentKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.key())
    }
}

// ---------------------------------------------------------------------------
// AgentBinary
// ---------------------------------------------------------------------------

/// Where a binary path came from. Shown in diagnostics so "it picked the wrong
/// `claude`" is answerable without guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BinarySource {
    /// `config.agents.binaries.<kind>` (which the cco import can seed).
    Configured,
    /// Found by scanning the resolved `PATH`.
    Detected,
    /// `$SHELL` / `config.terminal.shell`.
    LoginShell,
    /// Nothing was found.
    NotFound,
}

impl BinarySource {
    pub const fn as_str(self) -> &'static str {
        match self {
            BinarySource::Configured => "configured",
            BinarySource::Detected => "detected",
            BinarySource::LoginShell => "loginShell",
            BinarySource::NotFound => "notFound",
        }
    }
}

/// One row of `agents_detect()` — a superset of `ipc.ts::AgentBinary`.
///
/// `installed` and `ok` are deliberately different questions: `installed` means
/// "an executable is there", `ok` means "it answered `--version`". A `codex`
/// shim whose `node` has been uninstalled is installed but not ok, and that
/// distinction is exactly what the user needs to see. (It is also a real
/// failure mode on this machine: both `claude` and `codex` are
/// `#!/usr/bin/env node` shims, so the probe has to run with the resolved
/// `PATH` or it reports `env: node: No such file or directory`.)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentBinary {
    pub kind: AgentKind,
    pub abs_path: Option<PathBuf>,
    pub version: Option<String>,
    /// An executable exists at [`Self::abs_path`].
    pub installed: bool,
    /// It answered `--version` within the cap.
    pub ok: bool,
    /// The version falls inside [`AgentKind::verified_range`]. `false` ⇒ the UI
    /// marks it "unverified on this machine".
    pub verified: bool,
    pub verified_range: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    pub source: BinarySource,
    /// Unix millis of the probe that produced this row.
    #[serde(default)]
    pub detected_at: Option<i64>,
}

impl AgentBinary {
    /// The "nothing here" row. Never an error (M1-PLAN T09.4).
    pub fn absent(kind: AgentKind, warning: Option<String>) -> AgentBinary {
        AgentBinary {
            kind,
            abs_path: None,
            version: None,
            installed: false,
            ok: false,
            verified: false,
            verified_range: kind.verified_range().map(str::to_string),
            warning,
            source: BinarySource::NotFound,
            detected_at: Some(path::now_ms()),
        }
    }

    fn to_row(&self, fingerprint: &str) -> AgentBinaryRow {
        AgentBinaryRow {
            kind: self.kind.key().to_string(),
            abs_path: self
                .abs_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            version: self.version.clone(),
            detected_at: self.detected_at.unwrap_or_else(path::now_ms),
            path_fingerprint: Some(fingerprint.to_string()),
            verified_range: self.verified_range.clone(),
            ok: self.ok,
        }
    }

    fn from_row(row: &AgentBinaryRow) -> Option<AgentBinary> {
        let kind = AgentKind::from_key(&row.kind)?;
        let abs_path = row.abs_path.as_ref().map(PathBuf::from);
        let installed = abs_path.as_deref().is_some_and(path::is_executable);
        Some(AgentBinary {
            kind,
            verified: verified_against(row.version.as_deref(), kind.verified_range()),
            abs_path,
            version: row.version.clone(),
            installed,
            ok: row.ok && installed,
            verified_range: row.verified_range.clone(),
            warning: None,
            source: if installed {
                BinarySource::Detected
            } else {
                BinarySource::NotFound
            },
            detected_at: Some(row.detected_at),
        })
    }
}

/// Whether `version` sits inside the `major.minor` prefix we measured.
fn verified_against(version: Option<&str>, range: Option<&str>) -> bool {
    match (version, range) {
        (Some(v), Some(r)) => v == r || v.starts_with(&format!("{r}.")),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// spawn registration seam
// ---------------------------------------------------------------------------

/// A spawn about to happen, handed to the registry before it does.
#[derive(Debug, Clone)]
pub struct ProbePlan {
    /// `proc_registry.kind` — `"vendorcli"` for a `--version` probe (§3.8).
    pub kind: &'static str,
    pub vendor: Option<String>,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
}

/// The half of `supervise::registry::SpawnGuard` this module needs.
///
/// Declared here, implemented by T08's registry (and by T29 when it wires the
/// two together). Keeping it an interface rather than a direct dependency is
/// what lets T08 and T09 land in the same wave without one blocking the other —
/// and it means every test in this file can run with no database.
pub trait ProbeGuard: Send + Sync {
    /// The value to stamp as `GILDE_SPAWN_ID` on the child (DESIGN §8-4 fence 3).
    fn env_marker(&self) -> String;
    fn mark_running(&self, pid: i32, pgid: i32);
    fn mark_exited(&self, exit_code: Option<i32>, signal: Option<i32>);
}

/// Write-before-spawn (DESIGN §8-3). `None` ⇒ the caller spawns unregistered,
/// which only ever happens in tests and before T29 has wired the registry.
pub trait SpawnRecorder: Send + Sync + std::fmt::Debug {
    fn plan(&self, plan: ProbePlan) -> Option<Box<dyn ProbeGuard>>;

    /// Whether `plan` may launch at all (§9.3). `false` = the probe does not
    /// run — a refused launch never falls back to running unregistered
    /// (A12.8). Recorders without a registry allow everything.
    fn allows(&self, plan: &ProbePlan) -> bool {
        let _ = plan;
        true
    }
}

/// Records nothing. The default until T29 injects the real registry.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopRecorder;

impl SpawnRecorder for NoopRecorder {
    fn plan(&self, _plan: ProbePlan) -> Option<Box<dyn ProbeGuard>> {
        None
    }
}

/// A shareable [`SpawnRecorder`].
pub type Recorder = Arc<dyn SpawnRecorder>;

/// The default recorder.
pub fn noop_recorder() -> Recorder {
    Arc::new(NoopRecorder)
}

// -- the real registry ------------------------------------------------------

/// [`SpawnRecorder`] backed by T08's PID registry.
///
/// This is the recorder the app runs with: it turns every `--version` probe
/// into a durable `proc_registry` row with `kind = 'probe'` **before** the
/// process exists (DESIGN §8-3), so a probe that hangs is visible in the
/// Process Monitor and reachable by the reaper instead of being an anonymous
/// `claude` nobody can explain.
///
/// `SupervisorApi::plan_spawn` is `async` while detection runs on a blocking
/// thread (a `--version` probe can take 20 s and must not occupy a runtime
/// worker), so the plan is driven through a captured [`tokio::runtime::Handle`].
/// That is safe **because the call site is a `spawn_blocking` thread**, never a
/// runtime worker — `Handle::block_on` panics only on the latter.
pub struct SupervisorRecorder {
    supervisor: Arc<crate::supervise::Supervisor>,
    handle: tokio::runtime::Handle,
}

impl SupervisorRecorder {
    /// Capture the current runtime handle. Call from inside the app's runtime
    /// (T29's `setup()` qualifies).
    pub fn new(supervisor: Arc<crate::supervise::Supervisor>) -> SupervisorRecorder {
        SupervisorRecorder::with_handle(supervisor, tokio::runtime::Handle::current())
    }

    pub fn with_handle(
        supervisor: Arc<crate::supervise::Supervisor>,
        handle: tokio::runtime::Handle,
    ) -> SupervisorRecorder {
        SupervisorRecorder {
            supervisor,
            handle,
        }
    }
}

impl std::fmt::Debug for SupervisorRecorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SupervisorRecorder")
            .field("app_instance", &self.supervisor.app_instance_id())
            .finish()
    }
}

impl SpawnRecorder for SupervisorRecorder {
    fn allows(&self, plan: &ProbePlan) -> bool {
        let kind = crate::store::models::ProcKind::from_db_str(plan.kind)
            .unwrap_or(crate::store::models::ProcKind::Vendorcli);
        plan.argv
            .first()
            .is_some_and(|argv0| crate::supervise::spawn::is_allowed(Path::new(argv0), kind))
    }

    fn plan(&self, plan: ProbePlan) -> Option<Box<dyn ProbeGuard>> {
        use crate::contract::process::ProcKind as K;
        use crate::contract::services::{SpawnRequest, SupervisorApi};
        // `--version` probes of vendor CLIs are `vendorcli` rows (contract
        // §3.8), planned through the one spawn path (A12.8, A22.3).
        let kind = if plan.kind == "probe" { K::Probe } else { K::Vendorcli };
        let (argv0, args) = plan.argv.split_first()?;
        let argv0 = crate::contract::common::AbsPath::parse(argv0.clone()).ok()?;
        let request = SpawnRequest {
            kind,
            argv0,
            args: args.to_vec(),
            cwd: plan
                .cwd
                .to_str()
                .and_then(|c| crate::contract::common::AbsPath::parse(c.to_string()).ok()),
            env_keys: vec!["PATH".into(), "HOME".into(), "LANG".into(), "GILDE_SPAWN_ID".into()],
            parent_uid: None,
            generation: None,
            turn_id: None,
        };
        let supervisor = self.supervisor.clone();
        match self
            .handle
            .block_on(async move { SupervisorApi::plan_spawn(supervisor.as_ref(), request).await })
        {
            Ok(ticket) => Some(Box::new(RegistryProbeGuard {
                supervisor: self.supervisor.clone(),
                ticket,
                handle: self.handle.clone(),
            })),
            Err(e) => {
                // A registry that cannot write is a real problem, but refusing
                // to detect agents because of it would take the whole app down
                // with it. Probe unregistered, loudly.
                tracing::error!(target: "agents", code = e.code.as_str(), "could not register a version probe");
                None
            }
        }
    }
}

/// A row planned through `SupervisorApi` behind the [`ProbeGuard`] seam (the
/// calls run on the detection's blocking thread through the captured handle).
struct RegistryProbeGuard {
    supervisor: Arc<crate::supervise::Supervisor>,
    ticket: crate::contract::services::SpawnTicket,
    handle: tokio::runtime::Handle,
}

impl ProbeGuard for RegistryProbeGuard {
    fn env_marker(&self) -> String {
        self.ticket.env_marker.clone()
    }

    fn mark_running(&self, pid: i32, _pgid: i32) {
        use crate::contract::services::SupervisorApi;
        let sup = self.supervisor.clone();
        let ticket = self.ticket.clone();
        if let Err(e) = self
            .handle
            .block_on(async move { SupervisorApi::mark_running(sup.as_ref(), &ticket, pid).await })
        {
            tracing::warn!(target: "agents", code = e.code.as_str(), pid, "probe row not marked running");
        }
    }

    fn mark_exited(&self, exit_code: Option<i32>, signal: Option<i32>) {
        use crate::contract::services::SupervisorApi;
        let sup = self.supervisor.clone();
        let ticket = self.ticket.clone();
        let exit = crate::contract::activity::ExitInfo { code: exit_code, signal };
        self.handle
            .block_on(async move { SupervisorApi::mark_exited(sup.as_ref(), &ticket, exit).await });
    }
}

// ---------------------------------------------------------------------------
// detection
// ---------------------------------------------------------------------------

/// Locate and probe every [`AgentKind`].
///
/// `refresh: false` answers from the `agent_binary` table when those rows are
/// younger than [`DETECT_TTL`] *and* were written against the same `PATH`
/// fingerprint; `true` always re-probes. Never fails because a CLI is missing.
pub async fn detect(
    refresh: bool,
    cfg: &AppConfig,
    report: &PathReport,
    db: Option<&Db>,
    recorder: &Recorder,
) -> Result<Vec<AgentBinary>> {
    if !refresh {
        if let Some(db) = db {
            if let Some(warm) = warm_from_db(db, &report.fingerprint).await? {
                return Ok(warm);
            }
        }
    }

    let cfg = cfg.clone();
    let probe_report = report.clone();
    let recorder = recorder.clone();
    let found = tokio::task::spawn_blocking(move || detect_blocking(&cfg, &probe_report, &recorder))
        .await
        .map_err(|e| crate::error::GildeError::internal(format!("detection task panicked: {e}")))?;

    if let Some(db) = db {
        let rows: Vec<AgentBinaryRow> =
            found.iter().map(|b| b.to_row(&report.fingerprint)).collect();
        db.write(move |conn| {
            for row in &rows {
                dao::agent_binary::upsert(conn, row)?;
            }
            Ok(())
        })
        .await?;
    }

    Ok(found)
}

/// Rows younger than the TTL, written against this `PATH` fingerprint, and
/// still backed by an executable file. Any of those failing is a cache miss.
async fn warm_from_db(db: &Db, fingerprint: &str) -> Result<Option<Vec<AgentBinary>>> {
    let rows = db.read(dao::agent_binary::list).await?;
    if rows.len() != AgentKind::ALL.len() {
        return Ok(None);
    }
    let cutoff = path::now_ms() - DETECT_TTL.as_millis() as i64;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        if row.detected_at < cutoff || row.path_fingerprint.as_deref() != Some(fingerprint) {
            return Ok(None);
        }
        match AgentBinary::from_row(row) {
            Some(binary) => out.push(binary),
            None => return Ok(None),
        }
    }
    out.sort_by_key(|b| AgentKind::ALL.iter().position(|k| *k == b.kind));
    Ok(Some(out))
}

/// The blocking half: file lookups plus one `--version` probe per kind.
fn detect_blocking(cfg: &AppConfig, report: &PathReport, recorder: &Recorder) -> Vec<AgentBinary> {
    let path_env = report.joined();
    AgentKind::ALL
        .into_iter()
        .map(|kind| detect_one(kind, cfg, report, &path_env, recorder))
        .collect()
}

fn detect_one(
    kind: AgentKind,
    cfg: &AppConfig,
    report: &PathReport,
    path_env: &str,
    recorder: &Recorder,
) -> AgentBinary {
    let Some((abs_path, source)) = locate(kind, cfg, report) else {
        return AgentBinary::absent(kind, None);
    };

    // A located vendor binary (the configured pin or the resolved PATH) is
    // allowlisted for the vendor kinds before its first launch (§9.3).
    if kind != AgentKind::Shell {
        crate::supervise::spawn::allow_executable(&abs_path, &VENDOR_KINDS);
    }
    let argv = vec![
        abs_path.to_string_lossy().into_owned(),
        kind.version_flag().to_string(),
    ];
    let plan = ProbePlan {
        kind: "vendorcli",
        vendor: Some(kind.key().to_string()),
        argv: argv.clone(),
        cwd: PathBuf::from("/"),
    };
    let outcome = if recorder.allows(&plan) {
        // Registered *before* the spawn, like every other process this app creates.
        let guard = recorder.plan(plan);
        run_probe(&abs_path, kind.version_flag(), path_env, guard.as_deref())
    } else {
        ProbeOutcome::Failed("not allowlisted".to_string())
    };

    let (version, warning, ok) = match outcome {
        ProbeOutcome::Answered { stdout, stderr, code: Some(0) } => {
            match parse_version(&stdout).or_else(|| parse_version(&stderr)) {
                Some(v) => (Some(v), None, true),
                None => (
                    None,
                    Some(format!("{} --version printed nothing usable", kind.key())),
                    false,
                ),
            }
        }
        ProbeOutcome::Answered { stderr, code, .. } => (
            None,
            Some(format!(
                "{} --version exited {}: {}",
                kind.key(),
                code.map(|c| c.to_string()).unwrap_or_else(|| "by signal".into()),
                first_line(&stderr)
            )),
            false,
        ),
        ProbeOutcome::TimedOut => (
            None,
            Some(format!(
                "{} --version did not answer within {:?}",
                kind.key(),
                VERSION_PROBE_TIMEOUT
            )),
            false,
        ),
        ProbeOutcome::Failed(e) => (None, Some(format!("{} could not be run: {e}", kind.key())), false),
    };

    let range = kind.verified_range();
    AgentBinary {
        kind,
        verified: verified_against(version.as_deref(), range),
        abs_path: Some(abs_path),
        version,
        installed: true,
        ok,
        verified_range: range.map(str::to_string),
        warning,
        source,
        detected_at: Some(path::now_ms()),
    }
}

/// Configured pin first, then the resolved `PATH`.
fn locate(kind: AgentKind, cfg: &AppConfig, report: &PathReport) -> Option<(PathBuf, BinarySource)> {
    if kind == AgentKind::Shell {
        let shell = cfg
            .terminal
            .shell
            .clone()
            .filter(|s| !s.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(path::login_shell);
        return path::is_executable(&shell).then_some((shell, BinarySource::LoginShell));
    }

    if let Some(pinned) = cfg.agent_binary(kind.key()) {
        let pinned = PathBuf::from(pinned);
        if path::is_executable(&pinned) {
            return Some((pinned, BinarySource::Configured));
        }
    }

    for name in kind.binary_names() {
        for dir in &report.entries {
            let candidate = dir.join(name);
            if path::is_executable(&candidate) {
                return Some((candidate, BinarySource::Detected));
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// the version probe
// ---------------------------------------------------------------------------

enum ProbeOutcome {
    Answered {
        stdout: String,
        stderr: String,
        code: Option<i32>,
    },
    TimedOut,
    Failed(String),
}

/// Run `<binary> --version` with a hard cap and a deterministic environment.
///
/// `PATH` is the **resolved** one on purpose: `claude` and `codex` are
/// `#!/usr/bin/env node` shims, and probing them under Finder's `PATH` reports
/// a missing binary that is in fact installed and working.
fn run_probe(
    binary: &Path,
    flag: &str,
    path_env: &str,
    guard: Option<&dyn ProbeGuard>,
) -> ProbeOutcome {
    // Own group (the supervise module's grouped builder, a §9.3 launch site),
    // so a probe that forks a helper can be terminated whole.
    let mut cmd = crate::supervise::spawn::std_command_grouped(binary);
    cmd.arg(flag)
        .env_clear()
        .env("PATH", path_env)
        .env("HOME", std::env::var("HOME").unwrap_or_default())
        .env("LANG", "en_US.UTF-8")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(guard) = guard {
        cmd.env("GILDE_SPAWN_ID", guard.env_marker());
    }

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => return ProbeOutcome::Failed(e.to_string()),
    };
    let pid = child.id() as i32;
    if let Some(guard) = guard {
        guard.mark_running(pid, pid);
    }

    let out_rx = drain(child.stdout.take());
    let err_rx = drain(child.stderr.take());

    let deadline = Instant::now() + VERSION_PROBE_TIMEOUT;
    let status;
    loop {
        match child.try_wait() {
            Ok(Some(s)) => {
                status = s;
                break;
            }
            Ok(None) => {}
            Err(e) => return ProbeOutcome::Failed(e.to_string()),
        }
        if Instant::now() >= deadline {
            // A vendor CLI is never SIGKILLed (contract §9.8, C5): SIGTERM to
            // the probe's own group, a short grace, and a survivor is left to
            // the registry (its row stays `running` for the reaper).
            if terminate_group(pid, &mut child) {
                if let Some(guard) = guard {
                    guard.mark_exited(None, Some(libc::SIGTERM));
                }
            }
            return ProbeOutcome::TimedOut;
        }
        thread::sleep(Duration::from_millis(5));
    }

    let stdout = collect(out_rx);
    let stderr = collect(err_rx);
    let code = status.code();
    let signal = status.signal();
    if let Some(guard) = guard {
        guard.mark_exited(code, signal);
    }

    ProbeOutcome::Answered {
        stdout,
        stderr,
        code,
    }
}

/// Read a pipe on its own thread so a chatty probe cannot deadlock on a full
/// 64 KiB pipe buffer while we sit in `try_wait`.
fn drain<R: std::io::Read + Send + 'static>(pipe: Option<R>) -> Option<mpsc::Receiver<Vec<u8>>> {
    let mut pipe = pipe?;
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    Some(rx)
}

fn collect(rx: Option<mpsc::Receiver<Vec<u8>>>) -> String {
    rx.and_then(|rx| rx.recv_timeout(Duration::from_millis(500)).ok())
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

/// `SIGTERM` to the probe's own group (a process we created microseconds ago,
/// whose `Child` we still hold), then up to [`PROBE_KILL_GRACE`] for it to go.
/// Never `SIGKILL`: the probe is a vendor CLI (contract §9.8, C5). Returns
/// whether it exited; a survivor's `Child` is dropped without a wait.
fn terminate_group(pid: i32, child: &mut std::process::Child) -> bool {
    use nix::sys::signal::{killpg, Signal};
    use nix::unistd::Pid;

    if pid <= 0 {
        return false;
    }
    let _ = killpg(Pid::from_raw(pid), Signal::SIGTERM);
    let deadline = Instant::now() + PROBE_KILL_GRACE;
    while Instant::now() < deadline {
        if let Ok(Some(_)) = child.try_wait() {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    matches!(child.try_wait(), Ok(Some(_)))
}

/// Pull a version out of whatever the CLI printed.
///
/// The five CLIs disagree completely: `2.1.263 (Claude Code)`, `codex-cli
/// 0.145.0`, a bare `1.1.11`, `zsh 5.9 (arm-apple-darwin24.0)`. Rather than one
/// parser per vendor — five more things to break on an upgrade — take the first
/// dotted-numeric token of the first line.
pub fn parse_version(text: &str) -> Option<String> {
    let line = text.lines().find(|l| !l.trim().is_empty())?;
    line.split(|c: char| c.is_whitespace() || "()[]{},;=/:".contains(c))
        .filter_map(|token| {
            let token = token.trim_matches(|c: char| !c.is_ascii_alphanumeric());
            // `v1.2.3` and `1.2.3` are the same claim.
            let token = token.strip_prefix('v').or(Some(token))?;
            let looks_like_a_version = token.contains('.')
                && token.starts_with(|c: char| c.is_ascii_digit())
                && token.chars().all(is_version_char);
            looks_like_a_version.then(|| token.to_string())
        })
        .next()
}

fn is_version_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '+' || c == '_'
}

fn first_line(text: &str) -> String {
    text.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .chars()
        .take(200)
        .collect()
}

// ---------------------------------------------------------------------------
// AgentRegistry — the managed-state facade
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Detection {
    at: Instant,
    fingerprint: String,
    binaries: Vec<AgentBinary>,
}

/// Managed state: everything `commands/agents.rs` needs, plus the caches.
///
/// Wired by T29's `setup()`. Holds the live [`Signatures`] so T07's monitor and
/// T16's status engine read the same hot-reloaded set through one handle.
pub struct AgentRegistry {
    paths: AppPaths,
    config: Arc<ConfigStore>,
    db: Option<Db>,
    recorder: Recorder,
    detect_lock: tokio::sync::Mutex<()>,
    detected: Mutex<Option<Detection>>,
    path_report: RwLock<Option<PathReport>>,
    signatures: RwLock<Arc<Signatures>>,
}

impl std::fmt::Debug for AgentRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentRegistry")
            .field("has_db", &self.db.is_some())
            .field("recorder", &self.recorder)
            .finish()
    }
}

impl AgentRegistry {
    /// Load the signature file (seeding it on first run) and build the facade.
    pub fn new(
        paths: AppPaths,
        config: Arc<ConfigStore>,
        db: Option<Db>,
        recorder: Recorder,
    ) -> AgentRegistry {
        let (signatures, warnings) = load_signatures(&paths);
        for warning in &warnings {
            tracing::warn!(target: "agents", warning, "agent signature warning");
        }
        AgentRegistry {
            paths,
            config,
            db,
            recorder,
            detect_lock: tokio::sync::Mutex::new(()),
            detected: Mutex::new(None),
            path_report: RwLock::new(None),
            signatures: RwLock::new(Arc::new(signatures)),
        }
    }

    /// Convenience constructor for tests and for the pre-database boot phase.
    pub fn without_db(paths: AppPaths, config: Arc<ConfigStore>) -> AgentRegistry {
        AgentRegistry::new(paths, config, None, noop_recorder())
    }

    pub fn paths(&self) -> &AppPaths {
        &self.paths
    }

    /// The resolved `PATH`, memoised for the lifetime of the process unless
    /// `refresh` is set. `resolve_path` has its own fingerprint-keyed disk
    /// cache; this one only avoids re-running the login shell per command.
    pub fn path_report(&self, refresh: bool) -> PathReport {
        if !refresh {
            if let Some(report) = self.path_report.read().clone() {
                return report;
            }
        }
        let report = resolve_path(&self.config.get(), &self.paths);
        *self.path_report.write() = Some(report.clone());
        report
    }

    /// A vendor's binary as a spawn would use it (A12.6, the sidecar's
    /// binary resolver, asked on every sidecar start): the configured pin or
    /// the resolved `PATH`, then [`resolve_binary`] (realpath, the A1
    /// fingerprint cache, the §9.3 allowlist). `None` = not installed or not
    /// resolvable. The PATH lookup may run the login-shell probe once, so it
    /// runs on the blocking pool; never on the boot path.
    pub async fn resolve_vendor(
        self: &Arc<Self>,
        vendor: crate::contract::common::Vendor,
    ) -> Option<crate::contract::common::AbsPath> {
        let kind = AgentKind::of_vendor(vendor);
        let me = Arc::clone(self);
        let located = tokio::task::spawn_blocking(move || {
            let report = me.path_report(false);
            locate(kind, &me.config.get(), &report).map(|(path, _)| path)
        })
        .await
        .ok()
        .flatten()?;
        resolve_binary(&located, kind, None).await.ok().map(|r| r.binary.realpath)
    }

    /// `agents_detect(refresh)`.
    pub async fn detect(&self, refresh: bool) -> Result<Vec<AgentBinary>> {
        let _serialised = self.detect_lock.lock().await;
        let report = self.path_report(refresh);

        if !refresh {
            if let Some(hit) = self.detected.lock().as_ref().filter(|d| {
                d.fingerprint == report.fingerprint && d.at.elapsed() < DETECT_TTL
            }) {
                return Ok(hit.binaries.clone());
            }
        }

        let cfg = self.config.get();
        let binaries = detect(refresh, &cfg, &report, self.db.as_ref(), &self.recorder).await?;
        *self.detected.lock() = Some(Detection {
            at: Instant::now(),
            fingerprint: report.fingerprint.clone(),
            binaries: binaries.clone(),
        });
        Ok(binaries)
    }

    /// `agents_launch_templates()`.
    pub async fn templates(&self) -> Result<Vec<LaunchTemplate>> {
        let binaries = self.detect(false).await?;
        Ok(templates(&self.config.get(), &binaries))
    }

    /// The live signature set. Cheap to clone; hand it to
    /// [`crate::supervise::classify`] or the status engine.
    pub fn signatures(&self) -> Arc<Signatures> {
        self.signatures.read().clone()
    }

    /// `agents_reload_signatures()` — also the callback for
    /// [`watch_signatures`].
    pub fn reload_signatures(&self) -> SignaturesReport {
        let (signatures, warnings) = load_signatures(&self.paths);
        let agents = signatures.agent_count() as u32;
        *self.signatures.write() = Arc::new(signatures);
        SignaturesReport { agents, warnings }
    }

    /// Start watching the signature file; the returned handle must be kept
    /// alive (T29 parks it in `AppState`).
    pub fn watch_signatures(self: &Arc<Self>) -> Result<signatures::Watcher> {
        let weak = Arc::downgrade(self);
        watch_signatures(&self.paths, move || {
            if let Some(registry) = weak.upgrade() {
                let report = registry.reload_signatures();
                tracing::info!(
                    target: "agents",
                    agents = report.agents,
                    warnings = report.warnings.len(),
                    "agent signatures reloaded"
                );
            }
        })
    }
}

// ---------------------------------------------------------------------------
// binary resolution at spawn (contract §6.0 *Binary resolution*, §3.10) [A1]
// ---------------------------------------------------------------------------

/// A binary resolved for a spawn, with the compat record of its version
/// (claude only, when the compat service is bound).
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedBinary {
    pub binary: crate::contract::session::BinaryRef,
    pub compat: Option<crate::contract::compat::ClaudeCompat>,
}

#[derive(Debug, Clone)]
struct CachedBinary {
    configured: PathBuf,
    realpath: PathBuf,
    binary: crate::contract::session::BinaryRef,
}

/// Invoked with the realpath of every binary whose cache entry an FSEvents
/// change invalidated (B10: schedule a background static canary).
pub type BinaryChanged = Arc<dyn Fn(&crate::contract::session::BinaryRef) + Send + Sync>;

/// The `BinaryRef` cache behind [`resolve_binary`].
///
/// Keyed on the configured path; an entry is valid while the realpath and its
/// `(size, mtime, inode)` fingerprint are unchanged. The directory of every
/// resolved realpath and the directory of the configured (`PATH`) entry are
/// watched (FSEvents, non-recursive); a change there drops the entries under
/// it, so the **next** spawn re-runs `--version` — running sessions keep the
/// `BinaryRef` recorded at their spawn and are never re-evaluated.
pub struct BinaryResolver {
    cache: Mutex<std::collections::HashMap<PathBuf, CachedBinary>>,
    watched: Mutex<std::collections::HashSet<PathBuf>>,
    watcher: Mutex<Option<notify::RecommendedWatcher>>,
    recorder: RwLock<Recorder>,
    path_env: RwLock<String>,
    on_change: RwLock<Vec<BinaryChanged>>,
    probes: std::sync::atomic::AtomicU64,
}

impl std::fmt::Debug for BinaryResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BinaryResolver")
            .field("cached", &self.cache.lock().len())
            .field("watched", &self.watched.lock().len())
            .finish()
    }
}

impl BinaryResolver {
    pub fn new() -> Arc<BinaryResolver> {
        Arc::new(BinaryResolver {
            cache: Mutex::new(std::collections::HashMap::new()),
            watched: Mutex::new(std::collections::HashSet::new()),
            watcher: Mutex::new(None),
            recorder: RwLock::new(noop_recorder()),
            path_env: RwLock::new(std::env::var("PATH").unwrap_or_default()),
            on_change: RwLock::new(Vec::new()),
            probes: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// The process-wide resolver [`resolve_binary`] uses.
    pub fn global() -> &'static Arc<BinaryResolver> {
        static GLOBAL: std::sync::OnceLock<Arc<BinaryResolver>> = std::sync::OnceLock::new();
        GLOBAL.get_or_init(BinaryResolver::new)
    }

    /// Register `--version` runs as `vendorcli` rows (boot wires the registry).
    pub fn set_recorder(&self, recorder: Recorder) {
        *self.recorder.write() = recorder;
    }

    /// The resolved login-shell `PATH` the `--version` runs get.
    pub fn set_path_env(&self, path: impl Into<String>) {
        *self.path_env.write() = path.into();
    }

    pub fn on_change(&self, f: BinaryChanged) {
        self.on_change.write().push(f);
    }

    /// How many `--version` runs this resolver made (tests: a cache hit runs none).
    pub fn version_runs(&self) -> u64 {
        self.probes.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The cached `BinaryRef` for `configured`, if still valid on disk.
    pub fn cached(&self, configured: &Path) -> Option<crate::contract::session::BinaryRef> {
        let entry = self.cache.lock().get(configured).cloned()?;
        let real = std::fs::canonicalize(configured).ok()?;
        let fp = fingerprint_of(&real).ok()?;
        (real == entry.realpath && fp == entry.binary.fingerprint).then_some(entry.binary)
    }

    /// Resolve `configured` for a spawn (see [`resolve_binary`]).
    pub async fn resolve(
        self: &Arc<Self>,
        configured: &Path,
        kind: AgentKind,
        compat: Option<&dyn crate::contract::services::CompatApi>,
    ) -> std::result::Result<ResolvedBinary, crate::contract::IpcError> {
        use crate::contract::IpcError;
        let binary = match self.cached(configured) {
            Some(hit) => hit,
            None => {
                let real = std::fs::canonicalize(configured)
                    .map_err(|_| IpcError::spawn_failed().with_detail("the agent binary was not found"))?;
                let fp = fingerprint_of(&real)
                    .map_err(|_| IpcError::spawn_failed().with_detail("the agent binary could not be read"))?;
                let realpath = crate::contract::common::AbsPath::parse(real.to_string_lossy().into_owned())
                    .map_err(|_| IpcError::spawn_failed())?;
                // Allowlisted before its first launch, the `--version` run.
                crate::supervise::spawn::allow_executable(&real, &VENDOR_KINDS);
                let version = self.run_version(&real, kind).await;
                let binary = crate::contract::session::BinaryRef {
                    realpath,
                    fingerprint: fp,
                    version,
                    pidpath: None,
                };
                self.cache.lock().insert(
                    configured.to_path_buf(),
                    CachedBinary {
                        configured: configured.to_path_buf(),
                        realpath: real.clone(),
                        binary: binary.clone(),
                    },
                );
                self.watch_dir_of(&real);
                self.watch_dir_of(configured);
                binary
            }
        };
        let compat = match (kind, compat) {
            (AgentKind::Claude, Some(api)) if binary.version.is_some() => Some(api.for_binary(&binary).await),
            _ => None,
        };
        Ok(ResolvedBinary { binary, compat })
    }

    async fn run_version(&self, real: &Path, kind: AgentKind) -> Option<String> {
        self.probes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let recorder = self.recorder.read().clone();
        let path_env = self.path_env.read().clone();
        let binary = real.to_path_buf();
        let flag = kind.version_flag();
        let vendor = kind.key().to_string();
        let out = tokio::task::spawn_blocking(move || {
            let plan = ProbePlan {
                kind: "vendorcli",
                vendor: Some(vendor),
                argv: vec![binary.to_string_lossy().into_owned(), flag.to_string()],
                cwd: PathBuf::from("/"),
            };
            if !recorder.allows(&plan) {
                return ProbeOutcome::Failed("not allowlisted".to_string());
            }
            let guard = recorder.plan(plan);
            run_probe(&binary, flag, &path_env, guard.as_deref())
        })
        .await
        .ok()?;
        match out {
            ProbeOutcome::Answered { stdout, stderr, .. } => {
                parse_version(&stdout).or_else(|| parse_version(&stderr))
            }
            _ => None,
        }
    }

    /// Drop every entry whose realpath or configured path lives in `dir` and
    /// whose binary changed (the configured path resolves elsewhere now, or
    /// the fingerprint differs); returns the dropped `BinaryRef`s (with their
    /// fingerprint as cached). An FSEvents change of another file in the same
    /// folder leaves the entry alone.
    pub fn invalidate_under(&self, dir: &Path) -> Vec<crate::contract::session::BinaryRef> {
        let mut dropped = Vec::new();
        self.cache.lock().retain(|_, e| {
            let in_dir = e.realpath.parent() == Some(dir) || e.configured.parent() == Some(dir);
            let unchanged = || {
                std::fs::canonicalize(&e.configured).ok().as_ref() == Some(&e.realpath)
                    && fingerprint_of(&e.realpath).ok().as_ref() == Some(&e.binary.fingerprint)
            };
            let hit = in_dir && !unchanged();
            if hit {
                dropped.push(e.binary.clone());
            }
            !hit
        });
        dropped
    }

    fn watch_dir_of(self: &Arc<Self>, path: &Path) {
        use notify::Watcher as _;
        let Some(dir) = path.parent().map(Path::to_path_buf) else { return };
        if !self.watched.lock().insert(dir.clone()) {
            return;
        }
        let mut slot = self.watcher.lock();
        if slot.is_none() {
            let me = Arc::downgrade(self);
            let handler = move |res: notify::Result<notify::Event>| {
                let Ok(event) = res else { return };
                let Some(me) = me.upgrade() else { return };
                let mut dirs: Vec<PathBuf> = event
                    .paths
                    .iter()
                    .filter_map(|p| p.parent().map(Path::to_path_buf))
                    .collect();
                dirs.sort();
                dirs.dedup();
                for d in dirs {
                    for old in me.invalidate_under(&d) {
                        tracing::info!(target: "agents", "an agent binary changed on disk; new spawns re-resolve it");
                        let fresh = std::fs::canonicalize(old.realpath.as_str())
                            .ok()
                            .and_then(|real| {
                                let fp = fingerprint_of(&real).ok()?;
                                let realpath = crate::contract::common::AbsPath::parse(
                                    real.to_string_lossy().into_owned(),
                                )
                                .ok()?;
                                Some(crate::contract::session::BinaryRef {
                                    realpath,
                                    fingerprint: fp,
                                    version: None,
                                    pidpath: None,
                                })
                            });
                        if let Some(fresh) = fresh {
                            for f in me.on_change.read().iter() {
                                f(&fresh);
                            }
                        }
                    }
                }
            };
            match notify::recommended_watcher(handler) {
                Ok(w) => *slot = Some(w),
                Err(err) => {
                    tracing::warn!(target: "agents", error = %err, "cannot watch agent binaries; changes are caught at the next spawn");
                    return;
                }
            }
        }
        if let Some(w) = slot.as_mut() {
            if let Err(err) = w.watch(&dir, notify::RecursiveMode::NonRecursive) {
                tracing::debug!(target: "agents", error = %err, "cannot watch a binary directory");
            }
        }
    }
}

/// The kinds a resolved vendor binary is allowlisted for (contract §9.3:
/// sessions, sidecar turns, probes, sign-in).
pub const VENDOR_KINDS: [crate::contract::process::ProcKind; 5] = [
    crate::contract::process::ProcKind::Convo,
    crate::contract::process::ProcKind::Pty,
    crate::contract::process::ProcKind::Attach,
    crate::contract::process::ProcKind::Turn,
    crate::contract::process::ProcKind::Vendorcli,
];

/// `(size, mtime_ms, inode)` of `path` (the `claude_compat_observed` key).
pub fn fingerprint_of(path: &Path) -> std::io::Result<crate::contract::session::BinaryFingerprint> {
    use std::os::unix::fs::MetadataExt as _;
    let meta = std::fs::metadata(path)?;
    Ok(crate::contract::session::BinaryFingerprint {
        size: meta.len(),
        mtime_ms: meta.mtime() * 1000 + meta.mtime_nsec() / 1_000_000,
        inode: meta.ino(),
    })
}

/// Resolve the configured binary of a vendor spawn (contract §6.0) [A1]:
/// realpath + `stat`; a cache hit on the same fingerprint answers at once;
/// otherwise `<realpath> --version` runs (a `vendorcli` row, 5 s cap), the
/// realpath is allowlisted for vendor spawns (§9.3) and its directory and the
/// configured entry's directory are watched. For claude, the version's compat
/// record is looked up or created (waiting up to 1 s for a first static
/// canary, `CompatApi::for_binary`).
///
/// `argv[0]` of the spawn is `binary.realpath`; the caller records the
/// `BinaryRef` on the session and adds `pidpath` once the child runs.
pub async fn resolve_binary(
    configured: &Path,
    kind: AgentKind,
    compat: Option<&dyn crate::contract::services::CompatApi>,
) -> std::result::Result<ResolvedBinary, crate::contract::IpcError> {
    BinaryResolver::global().resolve(configured, kind, compat).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn write_exec(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(path, body).expect("write");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    fn report_for(dirs: &[PathBuf]) -> PathReport {
        PathReport {
            entries: dirs.to_vec(),
            source: PathSource::Static,
            fingerprint: "fp".into(),
            probed_at: 0,
            warnings: Vec::new(),
        }
    }

    // -- vocabulary ---------------------------------------------------------

    #[test]
    fn kind_keys_match_the_ipc_contract_in_both_directions() {
        for kind in AgentKind::ALL {
            assert_eq!(AgentKind::from_key(kind.key()), Some(kind));
            let json = serde_json::to_value(kind).expect("json");
            assert_eq!(json, serde_json::json!(kind.key()));
        }
        assert_eq!(AgentKind::Cursor.key(), "cursorAgent");
        assert_eq!(AgentKind::Cursor.signature_key(), "cursor");
        assert_eq!(AgentKind::from_key("nope"), None);
    }

    #[test]
    fn cursor_falls_back_to_the_generic_agent_name_but_never_prefers_it() {
        assert_eq!(AgentKind::Cursor.binary_names(), &["cursor-agent", "agent"]);
    }

    #[test]
    fn account_env_keys_follow_design_6_6() {
        assert_eq!(AgentKind::Claude.account_env_key(), Some("CLAUDE_CONFIG_DIR"));
        assert_eq!(AgentKind::Codex.account_env_key(), Some("CODEX_HOME"));
        assert_eq!(AgentKind::Antigravity.account_env_key(), Some("AGY_HOME"));
        assert_eq!(AgentKind::Cursor.account_env_key(), None);
        assert_eq!(AgentKind::GeminiCli.account_env_key(), None);
    }

    // -- version parsing ----------------------------------------------------

    #[test]
    fn version_parsing_covers_every_shape_measured_on_this_machine() {
        assert_eq!(parse_version("2.1.263 (Claude Code)").as_deref(), Some("2.1.263"));
        assert_eq!(parse_version("codex-cli 0.145.0").as_deref(), Some("0.145.0"));
        assert_eq!(parse_version("1.1.11\n").as_deref(), Some("1.1.11"));
        assert_eq!(
            parse_version("zsh 5.9 (arm-apple-darwin24.0)").as_deref(),
            Some("5.9")
        );
        assert_eq!(parse_version("v1.2.3-beta.1").as_deref(), Some("1.2.3-beta.1"));
        assert_eq!(parse_version("\n\ngemini 0.4.0").as_deref(), Some("0.4.0"));
        assert_eq!(parse_version("no digits here"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn verified_is_only_claimed_for_a_range_we_measured() {
        assert!(verified_against(Some("2.1.263"), Some("2.1")));
        assert!(verified_against(Some("2.1"), Some("2.1")));
        assert!(!verified_against(Some("2.2.0"), Some("2.1")));
        assert!(!verified_against(Some("2.10.0"), Some("2.1")), "prefix, not substring");
        assert!(!verified_against(None, Some("2.1")));
        assert!(!verified_against(Some("9.9.9"), None));
    }

    // -- location -----------------------------------------------------------

    #[test]
    fn a_configured_binary_wins_over_the_path_and_a_dead_pin_falls_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin_dir = dir.path().join("bin");
        let pinned = dir.path().join("pinned/claude");
        write_exec(&bin_dir.join("claude"), "#!/bin/sh\necho path\n");
        write_exec(&pinned, "#!/bin/sh\necho pinned\n");
        let report = report_for(std::slice::from_ref(&bin_dir));

        let mut cfg = AppConfig::default();
        cfg.agents
            .binaries
            .insert("claude".into(), Some(pinned.to_string_lossy().into_owned()));
        assert_eq!(
            locate(AgentKind::Claude, &cfg, &report),
            Some((pinned, BinarySource::Configured))
        );

        cfg.agents
            .binaries
            .insert("claude".into(), Some("/does/not/exist".into()));
        assert_eq!(
            locate(AgentKind::Claude, &cfg, &report),
            Some((bin_dir.join("claude"), BinarySource::Detected)),
            "a stale pin must not hide an installed CLI"
        );
    }

    #[test]
    fn path_order_decides_and_a_non_executable_file_is_not_a_binary() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        std::fs::create_dir_all(&first).expect("mk");
        write_exec(&second.join("codex"), "#!/bin/sh\n");
        std::fs::write(first.join("codex"), "not executable").expect("write");

        let report = report_for(&[first, second.clone()]);
        assert_eq!(
            locate(AgentKind::Codex, &AppConfig::default(), &report),
            Some((second.join("codex"), BinarySource::Detected))
        );
    }

    #[test]
    fn a_missing_cli_is_a_row_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let report = report_for(&[dir.path().to_path_buf()]);
        assert_eq!(locate(AgentKind::GeminiCli, &AppConfig::default(), &report), None);

        let absent = AgentBinary::absent(AgentKind::GeminiCli, None);
        assert!(!absent.installed && !absent.ok && !absent.verified);
        let value = serde_json::to_value(&absent).expect("json");
        assert_eq!(value["kind"], serde_json::json!("gemini"));
        assert_eq!(value["absPath"], serde_json::Value::Null);
        assert_eq!(value["source"], serde_json::json!("notFound"));
    }

    // -- probing ------------------------------------------------------------

    #[derive(Debug, Default)]
    struct CountingRecorder {
        planned: AtomicUsize,
        running: AtomicUsize,
        exited: AtomicUsize,
    }

    #[derive(Debug)]
    struct CountingGuard(Arc<CountingRecorder>);

    impl ProbeGuard for CountingGuard {
        fn env_marker(&self) -> String {
            "spawn-marker".into()
        }
        fn mark_running(&self, _pid: i32, _pgid: i32) {
            self.0.running.fetch_add(1, Ordering::SeqCst);
        }
        fn mark_exited(&self, _code: Option<i32>, _signal: Option<i32>) {
            self.0.exited.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl SpawnRecorder for Arc<CountingRecorder> {
        fn plan(&self, _plan: ProbePlan) -> Option<Box<dyn ProbeGuard>> {
            self.planned.fetch_add(1, Ordering::SeqCst);
            Some(Box::new(CountingGuard(self.clone())))
        }
    }

    #[tokio::test]
    async fn every_probe_is_registered_before_it_spawns() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("bin");
        write_exec(
            &bin.join("claude"),
            "#!/bin/sh\nprintf '2.1.263 (Claude Code)\\n'\n",
        );

        let counter = Arc::new(CountingRecorder::default());
        let recorder: Recorder = Arc::new(counter.clone());
        let report = report_for(&[bin, PathBuf::from("/usr/bin"), PathBuf::from("/bin")]);

        let found = detect(true, &AppConfig::default(), &report, None, &recorder)
            .await
            .expect("detect");

        let claude = found.iter().find(|b| b.kind == AgentKind::Claude).expect("claude");
        assert_eq!(claude.version.as_deref(), Some("2.1.263"));
        assert!(claude.installed && claude.ok && claude.verified);

        assert!(counter.planned.load(Ordering::SeqCst) >= 1, "probes are registered");
        assert_eq!(
            counter.planned.load(Ordering::SeqCst),
            counter.running.load(Ordering::SeqCst),
            "every planned row is marked running"
        );
        assert_eq!(
            counter.planned.load(Ordering::SeqCst),
            counter.exited.load(Ordering::SeqCst),
            "and closed out"
        );
    }

    #[derive(Debug, Default)]
    struct RefusingRecorder {
        planned: AtomicUsize,
    }

    impl SpawnRecorder for Arc<RefusingRecorder> {
        fn allows(&self, _plan: &ProbePlan) -> bool {
            false
        }
        fn plan(&self, _plan: ProbePlan) -> Option<Box<dyn ProbeGuard>> {
            self.planned.fetch_add(1, Ordering::SeqCst);
            None
        }
    }

    /// A12.8: a probe the allowlist refuses does not run at all — it never
    /// falls back to running unregistered.
    #[tokio::test]
    async fn a_refused_probe_never_runs_unregistered() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("bin");
        let ran = dir.path().join("ran");
        write_exec(
            &bin.join("claude"),
            &format!("#!/bin/sh\ntouch '{}'\nprintf '2.1.263 (Claude Code)\\n'\n", ran.display()),
        );
        let refusing = Arc::new(RefusingRecorder::default());
        let recorder: Recorder = Arc::new(refusing.clone());
        let report = report_for(&[bin, PathBuf::from("/usr/bin"), PathBuf::from("/bin")]);

        let found = detect(true, &AppConfig::default(), &report, None, &recorder)
            .await
            .expect("detect");

        let claude = found.iter().find(|b| b.kind == AgentKind::Claude).expect("claude");
        assert!(claude.installed && !claude.ok);
        assert!(claude.version.is_none());
        assert!(claude.warning.as_deref().is_some_and(|w| w.contains("not allowlisted")));
        assert!(!ran.exists(), "the refused probe never ran");
        assert_eq!(refusing.planned.load(Ordering::SeqCst), 0, "nothing was planned either");
    }

    #[tokio::test]
    async fn a_binary_that_cannot_run_is_installed_but_not_ok() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("bin");
        // Reproduces the real failure of a `#!/usr/bin/env node` shim without
        // node on PATH: the file is there, the probe fails.
        write_exec(&bin.join("codex"), "#!/bin/sh\necho boom >&2\nexit 3\n");

        let report = report_for(&[bin, PathBuf::from("/usr/bin"), PathBuf::from("/bin")]);
        let found = detect(true, &AppConfig::default(), &report, None, &noop_recorder())
            .await
            .expect("detect");

        let codex = found.iter().find(|b| b.kind == AgentKind::Codex).expect("codex");
        assert!(codex.installed, "the file exists");
        assert!(!codex.ok, "but it did not answer");
        assert!(codex.version.is_none());
        assert!(codex.warning.as_deref().is_some_and(|w| w.contains("boom")));
    }

    #[tokio::test]
    async fn a_hanging_probe_is_capped_and_does_not_stall_detection() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("bin");
        write_exec(&bin.join("agy"), "#!/bin/sh\nsleep 60\n");

        let report = report_for(&[bin, PathBuf::from("/usr/bin"), PathBuf::from("/bin")]);
        let started = Instant::now();
        let found = detect(true, &AppConfig::default(), &report, None, &noop_recorder())
            .await
            .expect("detect");
        let elapsed = started.elapsed();

        let agy = found
            .iter()
            .find(|b| b.kind == AgentKind::Antigravity)
            .expect("agy");
        assert!(agy.installed && !agy.ok);
        assert!(agy.warning.as_deref().is_some_and(|w| w.contains("did not answer")));
        assert!(
            elapsed < Duration::from_secs(20),
            "the 5s cap must hold: took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn detection_returns_one_row_per_kind_and_upserts_them() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("bin");
        write_exec(&bin.join("claude"), "#!/bin/sh\necho 2.1.263\n");
        let report = report_for(&[bin, PathBuf::from("/usr/bin"), PathBuf::from("/bin")]);
        let db = Db::open_in_memory().expect("db");

        let found = detect(true, &AppConfig::default(), &report, Some(&db), &noop_recorder())
            .await
            .expect("detect");
        assert_eq!(found.len(), AgentKind::ALL.len());

        let rows = db.read(dao::agent_binary::list).await.expect("list");
        assert_eq!(rows.len(), AgentKind::ALL.len());

        // Warm read: same fingerprint, inside the TTL.
        let warm = detect(false, &AppConfig::default(), &report, Some(&db), &noop_recorder())
            .await
            .expect("warm");
        assert_eq!(warm.len(), found.len());
        assert_eq!(
            warm.iter().find(|b| b.kind == AgentKind::Claude).unwrap().version,
            Some("2.1.263".to_string())
        );

        // A different PATH fingerprint invalidates it.
        let mut other = report.clone();
        other.fingerprint = "different".into();
        assert!(warm_from_db(&db, &other.fingerprint).await.expect("warm").is_none());
    }

    /// M1-PLAN T09.4, end to end against T08's real registry: a probe is a
    /// durable `proc_registry` row with `kind = 'probe'` *before* it runs, and
    /// it is closed out afterwards.
    ///
    /// `multi_thread` because detection probes on a `spawn_blocking` thread and
    /// [`SupervisorRecorder`] drives the async `SupervisorApi::plan_spawn` from there.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn probes_land_in_the_real_pid_registry_as_probe_rows() {
        use crate::store::models::{ProcKind, ProcState};

        let dir = tempfile::tempdir().expect("tempdir");
        let paths = AppPaths::for_home(dir.path());
        paths.ensure_all().expect("dirs");
        let bin = dir.path().join("bin");
        write_exec(&bin.join("claude"), "#!/bin/sh\necho 2.1.263\n");

        let db = Db::open_in_memory().expect("db");
        let config = Arc::new(ConfigStore::load(&paths).expect("config"));
        let supervisor = Arc::new(
            crate::supervise::Supervisor::open_instance(&db, config, "test")
                .await
                .expect("supervisor"),
        );
        let recorder: Recorder = Arc::new(SupervisorRecorder::with_handle(
            supervisor.clone(),
            tokio::runtime::Handle::current(),
        ));

        let report = report_for(&[bin, PathBuf::from("/usr/bin"), PathBuf::from("/bin")]);
        let found = detect(true, &AppConfig::default(), &report, Some(&db), &recorder)
            .await
            .expect("detect");
        assert!(found.iter().any(|b| b.kind == AgentKind::Claude && b.ok));

        let instance = supervisor.app_instance_id().to_string();
        let rows = db
            .read(move |conn| dao::proc::rows_of_instance(conn, &instance))
            .await
            .expect("rows");

        assert!(!rows.is_empty(), "every probe writes a row before it spawns");
        for row in &rows {
            assert_eq!(row.kind, ProcKind::Vendorcli);
            assert_eq!(
                row.state,
                ProcState::Exited,
                "a finished probe must not be left `planned` or `running`"
            );
            assert!(row.pid.is_some(), "the pid is recorded after the spawn");
        }
    }

    // -- registry facade ----------------------------------------------------

    #[tokio::test]
    async fn the_registry_serves_signatures_and_reloads_them_on_demand() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = AppPaths::for_home(dir.path());
        paths.ensure_all().expect("dirs");
        let config = Arc::new(ConfigStore::load(&paths).expect("config"));
        let registry = AgentRegistry::without_db(paths.clone(), config);

        assert_eq!(registry.signatures().agent_count(), 6);
        assert_eq!(registry.signatures().idle_after_ms("claude"), 1_200);

        std::fs::write(
            paths.signatures_file(),
            serde_json::json!({ "version": 1, "agents": { "claude": { "idleAfterMs": 55 } } })
                .to_string(),
        )
        .expect("write");

        let report = registry.reload_signatures();
        assert_eq!(report.agents, 6);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert_eq!(registry.signatures().idle_after_ms("claude"), 55);
    }

    /// M1-PLAN T09 acceptance 5 — the measured truth of *this* machine, run
    /// end to end: real `PATH` resolution, real binaries, real `--version`.
    ///
    /// Skipped (with a printed reason) on a machine that does not have the
    /// nvm install this expectation was written against; asserting someone
    /// else's filesystem would be a test that lies rather than one that passes.
    ///
    /// **What it asserts, and what it deliberately does not (INT-A).** It pins
    /// the *structure* T09 measured — which binary each kind resolves to, that
    /// the probe ran with the resolved `PATH` (the `codex` shim cannot find
    /// `node` without it), and that every installed row parsed a version at all
    /// — but never an exact version string. A CLI the user upgrades between two
    /// runs of `cargo test` is not a regression in this crate, and a test that
    /// fails on `agy 1.1.27` because T09 measured `1.1.11` is measuring the
    /// user's update cadence. The measured-at values are recorded in
    /// docs/PINNED-VERSIONS.md, which is where a version belongs.
    #[tokio::test]
    async fn machine_truth_measured_2026_09_07() {
        // It runs the real CLIs' `--version` and pins one machine's install layout: opt-in only.
        // (On a second Mac with the same nvm path but `claude` from Homebrew first on PATH it failed.)
        if std::env::var_os("GILDE_MACHINE_TRUTH").is_none() {
            eprintln!("skipped: set GILDE_MACHINE_TRUTH=1 on the machine T09 was measured on");
            return;
        }
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            eprintln!("skipped: no HOME");
            return;
        };
        let nvm_bin = home.join(".nvm/versions/node/v20.19.2/bin");
        if !nvm_bin.join("claude").exists() {
            eprintln!("skipped: not the machine T09 was measured on ({nvm_bin:?})");
            return;
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let paths = AppPaths::new(dir.path().join("app-data"), home.clone());
        paths.ensure_all().expect("dirs");
        let config = Arc::new(ConfigStore::load(&paths).expect("config"));
        let registry = AgentRegistry::without_db(paths, config);

        let report = registry.path_report(true);
        assert!(
            report.contains(&nvm_bin),
            "the nvm bin dir must be on the resolved PATH: {:?}",
            report.entries
        );

        let found = registry.detect(true).await.expect("detect");
        let of = |kind: AgentKind| {
            found
                .iter()
                .find(|b| b.kind == kind)
                .unwrap_or_else(|| panic!("no row for {kind}"))
                .clone()
        };

        // A version the probe actually parsed: `MAJOR.MINOR.PATCH`, every part a
        // number. `measured` is what T09 recorded, printed on every run so the
        // drift is visible without being fatal.
        let assert_version_shape = |binary: &AgentBinary, measured: &str| {
            let version = binary
                .version
                .as_deref()
                .unwrap_or_else(|| panic!("{} reported no version", binary.kind));
            let parts: Vec<&str> = version.split('.').collect();
            assert!(
                parts.len() >= 3 && parts.iter().take(3).all(|p| {
                    !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())
                }),
                "{} version {version:?} is not MAJOR.MINOR.PATCH",
                binary.kind
            );
            if version != measured {
                eprintln!(
                    "note: {} is {version} here; T09 measured {measured} on 2026-09-07",
                    binary.kind
                );
            }
        };

        let claude = of(AgentKind::Claude);
        assert_eq!(claude.abs_path.as_deref(), Some(nvm_bin.join("claude").as_path()));
        assert_version_shape(&claude, "2.1.263");
        assert!(claude.ok && claude.verified);

        let codex = of(AgentKind::Codex);
        assert_eq!(
            codex.abs_path.as_deref(),
            Some(nvm_bin.join("codex").as_path()),
            "the probe has to run with the resolved PATH or this shim cannot find node"
        );
        assert_version_shape(&codex, "0.145.0");
        assert!(codex.ok && codex.verified);

        let agy = of(AgentKind::Antigravity);
        assert_eq!(agy.abs_path.as_deref(), Some(home.join(".local/bin/agy").as_path()));
        assert_version_shape(&agy, "1.1.11");
        assert!(agy.ok && agy.verified);

        for kind in [AgentKind::Cursor, AgentKind::GeminiCli] {
            assert!(!of(kind).installed, "{kind} is not installed on this machine");
        }

        assert!(of(AgentKind::Shell).installed, "there is always a shell");
    }

    #[tokio::test]
    async fn templates_are_built_from_whatever_detection_found() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = AppPaths::for_home(dir.path());
        paths.ensure_all().expect("dirs");
        let bin = dir.path().join("bin");
        write_exec(&bin.join("claude"), "#!/bin/sh\necho 2.1.263\n");

        let config = Arc::new(ConfigStore::load(&paths).expect("config"));
        config
            .patch(serde_json::json!({
                // A38.3: the override is the whole search path (no login
                // shell, no static ladder), and the Shell row reads this pin,
                // never `$SHELL`.
                "agents": { "pathOverride": bin.to_string_lossy() },
                "terminal": { "shell": "/nonexistent/shell" }
            }))
            .expect("patch");

        let registry = AgentRegistry::without_db(paths, config);
        let floor = ["/usr/bin", "/bin", "/usr/sbin", "/sbin"].map(PathBuf::from);
        let report = registry.path_report(false);
        assert_eq!(report.source, PathSource::Override);
        assert!(
            report.entries.iter().all(|e| *e == bin || floor.contains(e)),
            "the pinned search path reaches outside the fixture dir: {:?}",
            report.entries
        );
        let found = registry.detect(false).await.expect("detect");
        for b in found.iter().filter(|b| b.installed) {
            assert!(
                b.abs_path.as_deref().is_some_and(|p| p.starts_with(&bin)),
                "{} was found (and probed) outside the fixture dir: {:?}",
                b.kind,
                b.abs_path
            );
        }
        let templates = registry.templates().await.expect("templates");

        let claude = templates
            .iter()
            .find(|t| t.agent == AgentKind::Claude)
            .expect("claude template");
        assert_eq!(claude.binary, bin.join("claude").to_string_lossy());
        assert_eq!(claude.prompt_mode, PromptMode::Argv);
        assert!(claude.verified);
    }
}
