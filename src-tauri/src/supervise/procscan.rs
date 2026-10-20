//! The process scanner — DESIGN §8-7 "데이터 소스".
//!
//! One `ps` invocation gives us everything except the working directory:
//!
//! ```text
//! ps -A -o pid=,ppid=,pgid=,rss=,%cpu=,etime=,user=,args=
//! ```
//!
//! The first seven whitespace-separated fields are parsed; **everything after
//! them is `args`**, because argv contains spaces and splitting it would corrupt
//! the very string the panel exists to show.
//!
//! Three rules this module never breaks:
//!
//! 1. **`cwd` resolution is lazy.** It is a syscall per pid and this machine can
//!    have 800 processes. [`ps_snapshot`] never calls [`cwd_of`]; only the rows
//!    the panel actually renders do (`ScanFilter::include_cwd`).
//! 2. **Failing to read another process's environment is a normal outcome**, not
//!    an error (DESIGN §8-4 limitation). [`environ_of`] returns `None`, callers
//!    fall through to the argv fence.
//! 3. **Masking happens here**, at the source, because the same string is
//!    rendered *and* written to `event_log`.
//!
//! Every process this module starts is put in its own process group, so a
//! signal aimed at a scan helper can never walk up into the app.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::os::raw::{c_char, c_int};
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use libproc::bsd_info::BSDInfo;
use libproc::libproc::proc_pid::{pidinfo, pidpath, PIDInfo, PidInfoFlavor};
use regex::Regex;
use sysinfo::{MemoryRefreshKind, Pid, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System, UpdateKind};

use crate::error::{GildeError, Result};
use crate::supervise::{ProcGroupKind, ProcRow, ProcTag, RegistryIndex};

/// Absolute paths: DESIGN §8 requires every execution to name an absolute binary.
const PS_BIN: &str = "/bin/ps";
const LSOF_BIN: &str = "/usr/sbin/lsof";

const PS_ARGS: [&str; 3] = ["-A", "-o", "pid=,ppid=,pgid=,rss=,%cpu=,etime=,user=,args="];

/// A `ps` that has not answered in this long is not going to.
const PS_TIMEOUT: Duration = Duration::from_secs(5);
/// `lsof` is the fallback path and is known to block on unresponsive mounts.
const LSOF_TIMEOUT: Duration = Duration::from_secs(3);

/// Display cap for the user prompt that follows `--` in an agent argv
/// (DESIGN §8-7: the prompt is not a secret, but it must not eat the row).
pub const PROMPT_DISPLAY_LIMIT: usize = 200;
/// Hard cap on a rendered argv. Protects the panel and `event_log` from a
/// pathological command line; the registry keeps the full argv.
pub const ARGV_DISPLAY_LIMIT: usize = 4096;

/// What secrets are replaced with.
pub const REDACTED: &str = "\u{ab}redacted\u{bb}";

// ---------------------------------------------------------------------------
// PsRow
// ---------------------------------------------------------------------------

/// One line of the `ps` snapshot, parsed but not yet interpreted.
#[derive(Debug, Clone, PartialEq)]
pub struct PsRow {
    pub pid: i32,
    pub ppid: i32,
    pub pgid: i32,
    /// Resident set size in KiB, as `ps` reports it.
    pub rss_kb: u64,
    pub cpu_pct: f32,
    pub elapsed_secs: u64,
    pub user: String,
    /// The full command line, spaces intact.
    pub args: String,
}

impl PsRow {
    /// Parse one `ps` line. Returns `None` for headers, blanks and malformed
    /// lines — a single unparsable process must never fail a whole scan.
    pub fn parse_line(line: &str) -> Option<PsRow> {
        let (fields, rest) = split_leading_fields(line, 7)?;
        Some(PsRow {
            pid: fields[0].parse().ok()?,
            ppid: fields[1].parse().ok()?,
            pgid: fields[2].parse().ok()?,
            rss_kb: fields[3].parse().ok()?,
            cpu_pct: fields[4].parse().ok()?,
            elapsed_secs: parse_etime(fields[5])?,
            user: fields[6].to_string(),
            args: rest.to_string(),
        })
    }

    pub fn rss_bytes(&self) -> u64 {
        self.rss_kb.saturating_mul(1024)
    }

    /// `argv[0]` as `ps` printed it — the first whitespace-delimited token.
    pub fn argv0(&self) -> &str {
        self.args.split_whitespace().next().unwrap_or("")
    }

    /// Basename of [`PsRow::argv0`], with a leading `-` (login shells) removed.
    pub fn name(&self) -> String {
        let argv0 = self.argv0();
        let base = argv0.rsplit('/').next().unwrap_or(argv0);
        base.strip_prefix('-').unwrap_or(base).to_string()
    }
}

/// Split `line` into `n` whitespace-separated fields plus the untouched
/// remainder. Returns `None` if there are fewer than `n` fields.
fn split_leading_fields(line: &str, n: usize) -> Option<(Vec<&str>, &str)> {
    let mut fields = Vec::with_capacity(n);
    let mut rest = line;
    for _ in 0..n {
        rest = rest.trim_start();
        if rest.is_empty() {
            return None;
        }
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        fields.push(&rest[..end]);
        rest = &rest[end..];
    }
    Some((fields, rest.trim_start()))
}

/// Parse the four shapes `ps` uses for `etime`: `SS`, `MM:SS`, `HH:MM:SS` and
/// `DD-HH:MM:SS`.
pub fn parse_etime(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (days, clock) = match s.split_once('-') {
        Some((d, rest)) => (d.parse::<u64>().ok()?, rest),
        None => (0, s),
    };
    let mut parts = clock.split(':');
    let mut values = [0u64; 3];
    let mut count = 0usize;
    for part in parts.by_ref() {
        if count == 3 {
            return None; // more than HH:MM:SS — not a shape ps emits
        }
        values[count] = part.parse::<u64>().ok()?;
        count += 1;
    }
    let (h, m, sec) = match count {
        1 => (0, 0, values[0]),
        2 => (0, values[0], values[1]),
        3 => (values[0], values[1], values[2]),
        _ => return None,
    };
    Some(days * 86_400 + h * 3_600 + m * 60 + sec)
}

// ---------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------

/// One `ps` snapshot with the two indexes every supervision path needs:
/// `pid → row` and `ppid → children`.
#[derive(Debug, Clone)]
pub struct Snapshot {
    rows: Vec<PsRow>,
    by_pid: HashMap<i32, usize>,
    children: HashMap<i32, Vec<i32>>,
    at: i64,
}

impl Snapshot {
    /// Build a snapshot from already-parsed rows. Public so tests can construct
    /// synthetic process trees (including impossible ones — see the cycle test).
    pub fn from_rows(rows: Vec<PsRow>) -> Snapshot {
        let mut by_pid = HashMap::with_capacity(rows.len());
        let mut children: HashMap<i32, Vec<i32>> = HashMap::with_capacity(rows.len());
        for (i, row) in rows.iter().enumerate() {
            by_pid.insert(row.pid, i);
            children.entry(row.ppid).or_default().push(row.pid);
        }
        Snapshot {
            rows,
            by_pid,
            children,
            at: now_millis(),
        }
    }

    pub fn rows(&self) -> &[PsRow] {
        &self.rows
    }

    pub fn get(&self, pid: i32) -> Option<&PsRow> {
        self.by_pid.get(&pid).map(|i| &self.rows[*i])
    }

    pub fn contains(&self, pid: i32) -> bool {
        self.by_pid.contains_key(&pid)
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Unix millis at which the snapshot was taken.
    pub fn at(&self) -> i64 {
        self.at
    }

    /// Every descendant of `pid`, breadth first, excluding `pid` itself.
    ///
    /// Cycle safe: a `ppid` cycle (impossible from the kernel, trivially
    /// constructible in a test or by a racing snapshot) terminates.
    pub fn descendants_of(&self, pid: i32) -> Vec<i32> {
        let mut out = Vec::new();
        let mut seen: HashSet<i32> = HashSet::new();
        seen.insert(pid);
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(pid);
        while let Some(current) = queue.pop_front() {
            let Some(kids) = self.children.get(&current) else {
                continue;
            };
            for kid in kids {
                // A process that is its own parent (pid 0 on some systems) must
                // not enqueue itself forever.
                if *kid == current || !seen.insert(*kid) {
                    continue;
                }
                out.push(*kid);
                queue.push_back(*kid);
            }
        }
        out
    }

    /// Every ancestor of `pid`, nearest first, excluding `pid` itself.
    /// Cycle safe for the same reason as [`Snapshot::descendants_of`].
    pub fn ancestors_of(&self, pid: i32) -> Vec<i32> {
        let mut out = Vec::new();
        let mut seen: HashSet<i32> = HashSet::new();
        seen.insert(pid);
        let mut current = pid;
        while let Some(row) = self.get(current) {
            let parent = row.ppid;
            if parent <= 0 || !seen.insert(parent) {
                break;
            }
            out.push(parent);
            current = parent;
        }
        out
    }

    /// Every pid whose process group is `pgid`.
    pub fn pids_in_group(&self, pgid: i32) -> Vec<i32> {
        if pgid <= 0 {
            return Vec::new();
        }
        self.rows
            .iter()
            .filter(|r| r.pgid == pgid)
            .map(|r| r.pid)
            .collect()
    }

    /// Pids whose environment contains `key=value`.
    ///
    /// **Best effort by design** (DESIGN §8-4): reading another process's
    /// environment block is increasingly restricted on macOS and fails for
    /// signed / hardened binaries even at the same uid. An empty result is a
    /// normal outcome and callers must have another fence.
    ///
    /// Only same-uid processes are probed — the rest cannot be read anyway.
    pub fn with_env(&self, key: &str, value: &str) -> Vec<i32> {
        let our_uid = uid_of(std::process::id() as i32);
        let candidates: Vec<Pid> = self
            .rows
            .iter()
            .filter(|r| r.pid > 1)
            .filter(|r| match our_uid {
                Some(uid) => uid_of(r.pid) == Some(uid),
                None => true,
            })
            .map(|r| Pid::from_u32(r.pid as u32))
            .collect();
        if candidates.is_empty() {
            return Vec::new();
        }

        let mut system = System::new();
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&candidates),
            false,
            ProcessRefreshKind::nothing().with_environ(UpdateKind::Always),
        );

        let needle = format!("{key}={value}");
        let mut out: Vec<i32> = candidates
            .iter()
            .filter(|pid| match system.process(**pid) {
                Some(p) => p.environ().iter().any(|e| e.to_string_lossy() == needle),
                None => false,
            })
            .map(|pid| pid.as_u32() as i32)
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Whether `pid` is the main executable of a `.app` bundle — DESIGN §8-6's
    /// concrete implementation of "protect foreground GUI apps".
    ///
    /// Three attempts, cheapest first, and **every one of them errs towards
    /// "protected"**:
    ///
    /// 1. `argv[0]` as a whitespace token. Correct only while no path segment
    ///    contains a space.
    /// 2. The whole command line, because `ps` hands us one string and a bundle
    ///    path routinely contains spaces (`/Applications/Google Chrome.app/…`).
    ///    Reading `argv[0]` as "up to the first space" would report Chrome,
    ///    VS Code and every `* Helper.app` as *not* a bundle main and put a kill
    ///    affordance on the user's foreground apps.
    /// 3. `proc_pidpath`, the authoritative answer, when neither string form
    ///    settled it (a rewritten `argv[0]`, a bare basename, a relative path).
    ///
    /// **Our own bundled sidecar is not a foreground GUI app.** Tauri puts
    /// `externalBin` next to the main executable, so the pinned Node runtime
    /// lives at `Gilde Code.app/Contents/MacOS/gilde-node` and matches the
    /// pattern above exactly as Finder's own binaries do. Treating it as a
    /// foreground app made an orphaned sidecar unkillable from both the
    /// Recovered Sessions sheet and the Process Monitor in the packaged build —
    /// the only remaining remedy was `kill -9` in a shell (PINNED §23-1). See
    /// [`is_our_sidecar_path`] for what "ours" is allowed to mean; any other
    /// `.app` main binary is still refused.
    pub fn is_app_bundle_main(&self, pid: i32) -> bool {
        match self.bundle_main_path(pid) {
            Some(path) => !is_our_sidecar_path(&path),
            None => false,
        }
    }

    /// The `.app` main-executable path this pid runs, if it runs one.
    ///
    /// Same three attempts, same order (`proc_pidpath` is a syscall per pid and
    /// this is called for every row of a 700-process table), but they now yield
    /// the *path* rather than a yes/no: whether the process is protected depends
    /// on which executable inside the bundle it is.
    fn bundle_main_path(&self, pid: i32) -> Option<String> {
        let row = self.get(pid)?;
        if is_app_bundle_path(row.argv0()) {
            return Some(row.argv0().to_string());
        }
        if let Some(path) = bundle_main_head(&row.args) {
            return Some(path.to_string());
        }
        exe_path_of(pid).filter(|path| is_app_bundle_path(path))
    }

    /// Kernel start time of `pid` in seconds — see [`start_time_of`].
    pub fn start_time(&self, pid: i32) -> Option<i64> {
        start_time_of(pid)
    }

    /// Owning uid of `pid` — see [`uid_of`].
    pub fn uid_of(&self, pid: i32) -> Option<u32> {
        uid_of(pid)
    }

    /// How long `pid` has been running, from the snapshot's `etime`.
    /// DESIGN §8-6 excludes very young processes from any kill target set.
    pub fn age_of(&self, pid: i32) -> Option<Duration> {
        self.get(pid).map(|r| Duration::from_secs(r.elapsed_secs))
    }

    /// Turn every scanned process into a monitor row.
    pub fn build_rows(&self, ctx: &RowContext<'_>) -> Vec<ProcRow> {
        self.rows.iter().map(|r| self.build_row(r, ctx)).collect()
    }

    /// Turn one scanned process into a monitor row (DESIGN §8-7 "행 모델").
    pub fn build_row(&self, ps: &PsRow, ctx: &RowContext<'_>) -> ProcRow {
        let (group, mut tags) = classify(ps, ctx.registry, ctx.signatures);

        let bsd = bsd_info_of(ps.pid);
        let uid = bsd.as_ref().map(|b| b.pbi_uid);
        let user_owned = match (uid, ctx.our_uid) {
            (Some(u), Some(ours)) => u == ours,
            _ => ps.user == ctx.our_user,
        };

        let app_bundle_main = self.is_app_bundle_main(ps.pid);
        if app_bundle_main {
            tags.push(ProcTag::AppBundleMain);
        }
        let protected = ps.pid <= 1
            || ps.pid == ctx.self_pid
            || ctx.protected_ancestors.contains(&ps.pid)
            || !user_owned
            || app_bundle_main;
        if protected {
            tags.push(ProcTag::Protected);
        }

        let entry = ctx.registry.get(ps.pid);
        let start_time = bsd
            .as_ref()
            .map(|b| bsd_start_millis(b))
            .unwrap_or_else(|| self.at - (ps.elapsed_secs as i64) * 1000);

        ProcRow {
            pid: ps.pid,
            ppid: ps.ppid,
            pgid: ps.pgid,
            name: ps.name(),
            argv_display: argv_display(&ps.args),
            rss_bytes: ps.rss_bytes(),
            cpu_pct: ps.cpu_pct,
            elapsed_secs: ps.elapsed_secs,
            start_time,
            cwd: if ctx.resolve_cwd { cwd_of(ps.pid) } else { None },
            user_owned,
            group,
            tags,
            owner: entry.and_then(|e| e.owner_ref()),
            spawn_id: entry.map(|e| e.spawn_id.clone()),
        }
    }
}

/// Everything [`Snapshot::build_row`] needs that is not in the snapshot, computed
/// once per scan rather than once per row.
#[derive(Debug)]
pub struct RowContext<'a> {
    pub registry: &'a RegistryIndex,
    pub signatures: &'a Signatures,
    /// This process. Never a kill target.
    pub self_pid: i32,
    pub our_uid: Option<u32>,
    pub our_user: String,
    /// Our ancestor chain. Never a kill target (DESIGN §8-6).
    pub protected_ancestors: HashSet<i32>,
    /// Whether to resolve `cwd` per row. Off by default — one syscall per pid.
    pub resolve_cwd: bool,
}

impl<'a> RowContext<'a> {
    pub fn new(
        snapshot: &Snapshot,
        registry: &'a RegistryIndex,
        signatures: &'a Signatures,
    ) -> RowContext<'a> {
        let self_pid = std::process::id() as i32;
        RowContext {
            registry,
            signatures,
            self_pid,
            our_uid: uid_of(self_pid),
            our_user: std::env::var("USER").unwrap_or_default(),
            protected_ancestors: snapshot.ancestors_of(self_pid).into_iter().collect(),
            resolve_cwd: false,
        }
    }

    pub fn with_cwd(mut self, resolve: bool) -> RowContext<'a> {
        self.resolve_cwd = resolve;
        self
    }
}

// ---------------------------------------------------------------------------
// the scan itself
// ---------------------------------------------------------------------------

/// One `ps` call for the whole machine — DESIGN §8-7.
///
/// Never resolves `cwd`: that is [`cwd_of`], called lazily for rendered rows.
pub fn ps_snapshot() -> Result<Snapshot> {
    let out = run_capture(PS_BIN, &PS_ARGS, PS_TIMEOUT).ok_or_else(|| {
        GildeError::Spawn(format!(
            "{PS_BIN} produced no snapshot within {}s",
            PS_TIMEOUT.as_secs()
        ))
    })?;
    // `args` can hold arbitrary bytes; the monitor is a display surface, so a
    // lossy decode here is the honest choice (the byte-exact rule is the PTY's).
    let text = String::from_utf8_lossy(&out);
    let rows: Vec<PsRow> = text.lines().filter_map(PsRow::parse_line).collect();
    if rows.is_empty() {
        return Err(GildeError::Spawn(format!("{PS_BIN} returned no parsable rows")));
    }
    Ok(Snapshot::from_rows(rows))
}

/// Fresh command line of a single pid — the argv fence re-reads this right
/// before signalling, because a snapshot may be seconds old.
pub fn ps_args_of(pid: i32) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    let pid_s = pid.to_string();
    let out = run_capture(PS_BIN, &["-o", "args=", "-p", &pid_s], PS_TIMEOUT)?;
    let text = String::from_utf8_lossy(&out);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Working directory of `pid`.
///
/// macOS has no `/proc`, so this is `proc_pidinfo(PROC_PIDVNODEPATHINFO)` with
/// `lsof` as the fallback. **Lazy only** — never called from [`ps_snapshot`].
pub fn cwd_of(pid: i32) -> Option<PathBuf> {
    if pid <= 0 {
        return None;
    }
    cwd_of_no_lsof(pid).or_else(|| cwd_via_lsof(pid))
}

/// [`cwd_of`] without the `lsof` fallback: one `proc_pidinfo` call, never a
/// child process — for callers that hold a lock or run every tick.
pub fn cwd_of_no_lsof(pid: i32) -> Option<PathBuf> {
    if pid <= 0 {
        return None;
    }
    let info = pidinfo::<ProcVnodePathInfo>(pid, 0).ok()?;
    let path = c_array_to_string(&info.pvi_cdir.vip_path);
    (!path.is_empty()).then(|| PathBuf::from(path))
}

fn cwd_via_lsof(pid: i32) -> Option<PathBuf> {
    let pid_s = pid.to_string();
    let out = run_capture(LSOF_BIN, &["-a", "-p", &pid_s, "-d", "cwd", "-Fn"], LSOF_TIMEOUT)?;
    let text = String::from_utf8_lossy(&out);
    text.lines()
        .filter_map(|l| l.strip_prefix('n'))
        .map(str::trim)
        .find(|p| !p.is_empty())
        .map(PathBuf::from)
}

/// Environment of `pid`, one `KEY=VALUE` string per entry.
///
/// **Failure is a normal path**, not an error (DESIGN §8-4): macOS restricts
/// reading another process's environment block, and it fails routinely for
/// signed and hardened binaries even at the same uid. `None` means "could not
/// read"; `Some(vec![])` means "read, and it was empty". Neither is a bug and
/// neither panics.
pub fn environ_of(pid: i32) -> Option<Vec<String>> {
    if pid <= 0 {
        return None;
    }
    let spid = Pid::from_u32(pid as u32);
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[spid]),
        false,
        ProcessRefreshKind::nothing().with_environ(UpdateKind::Always),
    );
    let process = system.process(spid)?;
    Some(
        process
            .environ()
            .iter()
            .map(|e| e.to_string_lossy().into_owned())
            .collect(),
    )
}

/// Kernel start time of `pid` in **seconds** since the epoch.
///
/// This is identity fence 1 (DESIGN §8-4) and the strongest general defence
/// against pid reuse: a recycled pid always starts later than the row that
/// recorded it. Primary source is `libproc`'s `proc_bsdinfo.pbi_start_tvsec`;
/// `sysinfo` is the cross-check the design asks for.
pub fn start_time_of(pid: i32) -> Option<i64> {
    if pid <= 0 {
        return None;
    }
    if let Some(info) = bsd_info_of(pid) {
        return Some(info.pbi_start_tvsec as i64);
    }
    let spid = Pid::from_u32(pid as u32);
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[spid]),
        false,
        ProcessRefreshKind::nothing(),
    );
    system.process(spid).map(|p| p.start_time() as i64)
}

/// Owning uid of `pid`. `None` when the process is gone.
pub fn uid_of(pid: i32) -> Option<u32> {
    if pid <= 0 {
        return None;
    }
    bsd_info_of(pid).map(|info| info.pbi_uid)
}

/// Physical RAM in bytes — the denominator of the memory budget (DESIGN §8-8).
pub fn total_memory() -> u64 {
    let system = System::new_with_specifics(
        RefreshKind::nothing().with_memory(MemoryRefreshKind::nothing().with_ram()),
    );
    system.total_memory()
}

/// Absolute executable path of `pid`, when the kernel will tell us.
pub fn exe_path_of(pid: i32) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    pidpath(pid).ok().filter(|p| !p.is_empty())
}

fn bsd_info_of(pid: i32) -> Option<BSDInfo> {
    if pid <= 0 {
        return None;
    }
    pidinfo::<BSDInfo>(pid, 0).ok()
}

fn bsd_start_millis(info: &BSDInfo) -> i64 {
    (info.pbi_start_tvsec as i64) * 1000 + (info.pbi_start_tvusec as i64) / 1000
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// argv masking — DESIGN §8-7
// ---------------------------------------------------------------------------

fn secret_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r"sk-[A-Za-z0-9_-]{16,}",
            r"|ghp_[A-Za-z0-9]{20,}",
            r"|xox[baprs]-\S+",
            r"|Bearer\s+\S+",
            r"|--?(?i:api[-_]?key|token|password|secret)(?:=|\s+)\S+",
        ))
        .expect("static secret regex compiles")
    })
}

/// Replace credential-shaped substrings with [`REDACTED`].
///
/// Applied at the source, before the string reaches the UI *or* `event_log`.
/// The user's own prompt is not a secret and is not masked — it is truncated
/// for display by [`argv_display`].
pub fn mask_argv(argv: &str) -> String {
    secret_regex()
        .replace_all(argv, regex::NoExpand(REDACTED))
        .into_owned()
}

/// Truncate the prompt that follows a `--` separator to [`PROMPT_DISPLAY_LIMIT`]
/// characters. The full argv stays in the registry.
pub fn truncate_prompt(argv: &str) -> String {
    let Some(idx) = find_bare_double_dash(argv) else {
        return argv.to_string();
    };
    let (head, tail) = argv.split_at(idx);
    // `tail` starts at the `--`; keep the separator, truncate what follows it.
    let after = &tail[2..];
    let trimmed = truncate_chars(after, PROMPT_DISPLAY_LIMIT);
    format!("{head}--{trimmed}")
}

/// Byte offset of a standalone `--` argument, if there is one.
fn find_bare_double_dash(argv: &str) -> Option<usize> {
    let bytes = argv.as_bytes();
    let mut i = 0usize;
    while let Some(found) = argv[i..].find("--") {
        let start = i + found;
        let before_ok = start == 0 || bytes[start - 1] == b' ';
        let after = start + 2;
        let after_ok = after >= bytes.len() || bytes[after] == b' ';
        if before_ok && after_ok {
            return Some(start);
        }
        i = start + 2;
        if i >= argv.len() {
            break;
        }
    }
    None
}

/// Truncate to `limit` characters (never inside a character), adding an ellipsis
/// when anything was cut.
fn truncate_chars(s: &str, limit: usize) -> String {
    let mut count = 0usize;
    for (byte_idx, _) in s.char_indices() {
        if count == limit {
            let mut out = String::with_capacity(byte_idx + 3);
            out.push_str(&s[..byte_idx]);
            out.push('\u{2026}');
            return out;
        }
        count += 1;
    }
    s.to_string()
}

/// The string the monitor renders and `event_log` records: masked, prompt
/// truncated, and hard-capped at [`ARGV_DISPLAY_LIMIT`].
pub fn argv_display(argv: &str) -> String {
    truncate_chars(&truncate_prompt(&mask_argv(argv)), ARGV_DISPLAY_LIMIT)
}

// ---------------------------------------------------------------------------
// classification signatures
// ---------------------------------------------------------------------------

/// Compiled monitor-classification patterns.
///
/// [`Signatures::defaults`] is the built-in set used before T09 wires the
/// hot-reloaded `agent-signatures.json`; T09 calls [`Signatures::compile`] with
/// the merged user patterns and hands the result to [`classify`]. A pattern that
/// fails to compile is reported as a warning and skipped — it never disables the
/// rest of the set.
#[derive(Debug, Clone, Default)]
pub struct Signatures {
    agent: Vec<Regex>,
    mcp_server: Vec<Regex>,
    external_residue: Vec<Regex>,
}

/// The agent CLIs of DECISIONS §제품 정의. Matched as a command token so that a
/// path segment called `gemini` in an unrelated command does not count.
const DEFAULT_AGENT_PATTERNS: &[&str] =
    &[r"(?:^|\s)(?:\S*/)?(?:claude|codex|cursor-agent|agy|gemini)(?:\s|$)"];

const DEFAULT_MCP_PATTERNS: &[&str] = &[
    r"(?i)mcp[-_]server",
    r"(?i)\bnpx\s+(?:-y\s+)?\S*mcp",
    r"(?i)python[0-9.]*\s+-m\s+mcp",
    r"(?i)@modelcontextprotocol",
];

const DEFAULT_EXTERNAL_RESIDUE_PATTERNS: &[&str] = &[
    r"Cursor Helper",
    r"Code Helper",
    r"extension-host",
    r"(?:^|/)tsserver(?:\.js)?(?:\s|$)",
];

impl Signatures {
    /// The built-in patterns (DESIGN §8-7 `ProcGroupKind` comments).
    pub fn defaults() -> Signatures {
        let (sigs, warnings) = Signatures::compile(
            DEFAULT_AGENT_PATTERNS,
            DEFAULT_MCP_PATTERNS,
            DEFAULT_EXTERNAL_RESIDUE_PATTERNS,
        );
        debug_assert!(warnings.is_empty(), "built-in patterns must compile: {warnings:?}");
        sigs
    }

    /// Compile user-supplied patterns. Returns the set plus one warning per
    /// pattern that did not compile.
    pub fn compile<S: AsRef<str>>(
        agent: &[S],
        mcp_server: &[S],
        external_residue: &[S],
    ) -> (Signatures, Vec<String>) {
        let mut warnings = Vec::new();
        let mut compile_all = |group: &str, patterns: &[S]| -> Vec<Regex> {
            patterns
                .iter()
                .filter_map(|p| match Regex::new(p.as_ref()) {
                    Ok(re) => Some(re),
                    Err(e) => {
                        warnings.push(format!("{group}: {:?} did not compile: {e}", p.as_ref()));
                        None
                    }
                })
                .collect()
        };
        let sigs = Signatures {
            agent: compile_all("agent", agent),
            mcp_server: compile_all("mcpServer", mcp_server),
            external_residue: compile_all("externalResidue", external_residue),
        };
        (sigs, warnings)
    }

    pub fn is_agent(&self, args: &str) -> bool {
        self.agent.iter().any(|re| re.is_match(args))
    }

    pub fn is_mcp_server(&self, args: &str) -> bool {
        self.mcp_server.iter().any(|re| re.is_match(args))
    }

    pub fn is_external_residue(&self, args: &str) -> bool {
        self.external_residue.iter().any(|re| re.is_match(args))
    }
}

/// Assign a process to its monitor group and its intrinsic tags.
///
/// The `Protected` and `AppBundleMain` tags need the whole snapshot (our
/// ancestor chain, the executable path) and are added by
/// [`Snapshot::build_row`], not here.
///
/// Order matters: a registry row wins over everything, and an unmanaged agent
/// CLI is recognised before the generic MCP / residue patterns, because
/// [`ProcGroupKind::AgentUnmanaged`] carries the stricter safety policy.
pub fn classify(
    row: &PsRow,
    registry: &RegistryIndex,
    sigs: &Signatures,
) -> (ProcGroupKind, Vec<ProcTag>) {
    if let Some(entry) = registry.get(row.pid) {
        let mut tags = vec![ProcTag::Ours];
        if entry.worktree_id.is_some() {
            tags.push(ProcTag::InWorktree);
        }
        if entry.keep_alive {
            tags.push(ProcTag::Detached);
        }
        let live = registry.is_current(entry) && entry.owner_alive;
        if live {
            return (ProcGroupKind::OursLive, tags);
        }
        tags.push(ProcTag::Orphan);
        return (ProcGroupKind::OursOrphan, tags);
    }

    if sigs.is_agent(&row.args) {
        return (ProcGroupKind::AgentUnmanaged, vec![ProcTag::External]);
    }
    if sigs.is_mcp_server(&row.args) {
        return (ProcGroupKind::McpServer, vec![ProcTag::McpServer]);
    }
    if sigs.is_external_residue(&row.args) {
        return (ProcGroupKind::ExternalResidue, vec![ProcTag::External]);
    }
    (ProcGroupKind::Other, Vec::new())
}

fn app_bundle_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\.app/Contents/MacOS/[^/]+$").expect("static app bundle regex compiles")
    })
}

/// DESIGN §8-6: `.*\.app/Contents/MacOS/[^/]+$`.
pub fn is_app_bundle_path(path: &str) -> bool {
    app_bundle_regex().is_match(path)
}

/// The part of a bundle main's path that is invariant, spaces or not.
const BUNDLE_MAIN_MARKER: &str = ".app/Contents/MacOS/";

/// The head of the bundle-main executable path a `ps` command line *begins*
/// with, if it begins with one — the space-tolerant form of
/// [`is_app_bundle_path`].
///
/// It returns a path rather than a yes/no because the caller has to know *which*
/// executable inside the bundle this is: our own vendored `gilde-node` sits in the
/// same directory as an application's main binary.
///
/// **The executable name is cut at the first whitespace**, because `ps` cannot
/// tell the rest of a name with a space in it from the first argument. That is
/// exact for the only name this is asked about — our runtime's `gilde-node` has no
/// space — and conservative for every other: a truncated `Google` is not our
/// binary's name, so the app keeps its protection.
///
/// `ps` gives us `argv` as one string, so the executable path cannot be
/// recovered by splitting on whitespace: both the bundle name and the
/// executable name commonly contain spaces. Instead the marker is located and
/// what surrounds it is checked for still being one path:
///
/// * everything before the marker must look like an absolute path — no flag
///   (`" -"`) and no quote, so `open -a /Applications/Foo.app/Contents/MacOS/Foo`
///   is not mistaken for Foo itself;
/// * the segment after it must be a non-empty executable name, i.e. the first
///   whitespace-delimited token must carry no further `/`.
fn bundle_main_head(args: &str) -> Option<&str> {
    let idx = args.find(BUNDLE_MAIN_MARKER)?;
    let prefix = &args[..idx];
    let looks_like_one_path = prefix.starts_with('/')
        && !prefix.contains(" -")
        && !prefix.contains('"')
        && !prefix.contains('\'');
    if !looks_like_one_path {
        return None;
    }
    let start = idx + BUNDLE_MAIN_MARKER.len();
    let rest = &args[start..];
    // `split`, not `split_whitespace`: a leading space means the name is empty.
    let exe = rest.split(char::is_whitespace).next().unwrap_or("");
    if exe.is_empty() || exe.contains('/') {
        return None;
    }
    Some(&args[..start + exe.len()])
}

/// The file names our vendored Node runtime can have.
///
/// `gilde-node` inside a bundle (Tauri drops the target-triple suffix when it
/// copies an `externalBin`), `gilde-node-<target-triple>` in `src-tauri/binaries`
/// during development.
fn is_sidecar_exe_name(name: &str) -> bool {
    match name.strip_prefix(SIDECAR_EXE_NAME) {
        Some("") => true,
        Some(rest) => rest.starts_with('-'),
        None => false,
    }
}

/// The name Tauri gives our vendored Node runtime inside the bundle.
const SIDECAR_EXE_NAME: &str = "gilde-node";

/// This application's own main executable, as `mainBinaryName` names it in
/// `tauri.conf.json`: `Gilde Code.app/Contents/MacOS/gilde`. It is the `[[bin]]`
/// name, not the package name (`gilde-code`), so it cannot come from
/// `CARGO_PKG_NAME`.
const OUR_EXE_NAME: &str = "gilde";

/// Whether `path` is **our own** sidecar binary rather than some application's
/// main executable — DESIGN §8-4 identity, applied to a path.
///
/// Two facts must line up, and neither of them alone is enough:
///
/// 1. the file is named like our vendored Node runtime ([`is_sidecar_exe_name`]);
/// 2. it lives where *we* put it — either in the directory this very process was
///    launched from (`current_exe()/../gilde-node` is precisely how
///    `sidecar::vendored_node` resolves it, so this covers the bundle we are
///    running out of and the dev tree), or in the `Contents/MacOS` of another
///    copy of this same application, recognised by our own main executable
///    sitting next to it.
///
/// A foreground app of anyone else's fails both halves: its main executable is
/// not called `gilde-node`, and it does not live beside our binary. The protection
/// DESIGN §8-6 describes is therefore unchanged for every process except the one
/// we shipped ourselves — and that one is still fenced by uid, ancestry, age and
/// (when it has a registry row) the three identity fences of §8-4 before any
/// signal is sent.
pub fn is_our_sidecar_path(path: &str) -> bool {
    let path = std::path::Path::new(path);
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if !is_sidecar_exe_name(name) {
        return false;
    }
    let Some(dir) = path.parent() else {
        return false;
    };
    if our_exe_dir().is_some_and(|ours| ours == dir) {
        return true;
    }
    // Another copy of this application: same layout, our main executable next
    // to the sidecar. The `is_file` check runs only for a path already named
    // like our runtime, so it costs nothing on a full process table.
    dir.ends_with("Contents/MacOS") && dir.join(OUR_EXE_NAME).is_file()
}

/// The directory the running executable lives in, resolved once.
fn our_exe_dir() -> Option<&'static std::path::Path> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(PathBuf::from))
    })
    .as_deref()
}


// ---------------------------------------------------------------------------
// bounded process capture
// ---------------------------------------------------------------------------

/// Run `program` with `args`, capture stdout, and give up after `timeout`.
///
/// The scanner runs on a timer; a helper that blocks (`lsof` on an unresponsive
/// mount is the classic case) must not take the timer with it. The child gets
/// its own process group, so nothing here can signal the app.
///
/// T08 replaces the body's spawn with `SpawnGuard`-backed one when the registry
/// exists; the single call site is deliberate.
fn run_capture(program: &str, args: &[&str], timeout: Duration) -> Option<Vec<u8>> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .ok()?;

    let mut stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::channel();
    // Read concurrently: the pipe buffer is 64 KiB and a full `ps` snapshot is
    // bigger than that, so a wait-then-read would deadlock.
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(_) => break,
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        thread::sleep(Duration::from_millis(1));
    }
    let _ = child.wait();
    // The writer is gone, so this resolves immediately unless a grandchild
    // inherited the pipe. Bounded either way; the reader thread ends at EOF.
    rx.recv_timeout(Duration::from_secs(1)).ok()
}

// ---------------------------------------------------------------------------
// PROC_PIDVNODEPATHINFO
// ---------------------------------------------------------------------------
//
// `libproc::proc_pid::pidcwd` is documented as "not implemented for macos", so
// the flavor is requested directly through the crate's generic `pidinfo::<T>()`
// by implementing its public `PIDInfo` trait for a faithful mirror of
// `<sys/proc_info.h>`'s `struct proc_vnodepathinfo`. The layout is asserted at
// compile time below and end-to-end by the `cwd_of(self) == current_dir()` test.

#[repr(C)]
#[derive(Clone, Copy)]
struct VinfoStat {
    vst_dev: u32,
    vst_mode: u16,
    vst_nlink: u16,
    vst_ino: u64,
    vst_uid: u32,
    vst_gid: u32,
    vst_atime: i64,
    vst_atimensec: i64,
    vst_mtime: i64,
    vst_mtimensec: i64,
    vst_ctime: i64,
    vst_ctimensec: i64,
    vst_birthtime: i64,
    vst_birthtimensec: i64,
    vst_size: i64,
    vst_blocks: i64,
    vst_blksize: i32,
    vst_flags: u32,
    vst_gen: u32,
    vst_rdev: u32,
    vst_qspare: [i64; 2],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Fsid {
    val: [i32; 2],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VnodeInfo {
    vi_stat: VinfoStat,
    vi_type: c_int,
    vi_pad: c_int,
    vi_fsid: Fsid,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VnodeInfoPath {
    vip_vi: VnodeInfo,
    vip_path: [c_char; 1024],
}

/// `struct proc_vnodepathinfo` — `pvi_cdir` is the working directory.
#[repr(C)]
#[derive(Clone, Copy)]
struct ProcVnodePathInfo {
    pvi_cdir: VnodeInfoPath,
    pvi_rdir: VnodeInfoPath,
}

// If any of these ever fail, the mirror has drifted from the header and the
// build stops instead of reading a garbage path.
const _: () = assert!(std::mem::size_of::<VinfoStat>() == 136);
const _: () = assert!(std::mem::size_of::<VnodeInfo>() == 152);
const _: () = assert!(std::mem::offset_of!(VnodeInfoPath, vip_path) == 152);
const _: () = assert!(std::mem::size_of::<VnodeInfoPath>() == 1176);
const _: () = assert!(std::mem::size_of::<ProcVnodePathInfo>() == 2352);

impl PIDInfo for ProcVnodePathInfo {
    fn flavor() -> PidInfoFlavor {
        PidInfoFlavor::VNodePathInfo
    }
}

/// NUL-terminated C array → `String`, without unsafe pointer arithmetic.
fn c_array_to_string(buf: &[c_char]) -> String {
    let bytes: Vec<u8> = buf
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::models::{ProcKind, ProcState};
    use crate::supervise::RegistryEntry;

    fn ps_row(pid: i32, ppid: i32, args: &str) -> PsRow {
        PsRow {
            pid,
            ppid,
            pgid: pid,
            rss_kb: 1024,
            cpu_pct: 0.0,
            elapsed_secs: 60,
            user: "tester".into(),
            args: args.into(),
        }
    }

    // -- ps parsing ---------------------------------------------------------

    #[test]
    fn parses_a_real_ps_line_and_keeps_argv_spacing() {
        let line = "    1     0     1  21824   0.2 48-12:24:08 root             /sbin/launchd";
        let row = PsRow::parse_line(line).expect("parses");
        assert_eq!(row.pid, 1);
        assert_eq!(row.ppid, 0);
        assert_eq!(row.pgid, 1);
        assert_eq!(row.rss_kb, 21824);
        assert!((row.cpu_pct - 0.2).abs() < 0.001);
        assert_eq!(row.elapsed_secs, 48 * 86_400 + 12 * 3600 + 24 * 60 + 8);
        assert_eq!(row.user, "root");
        assert_eq!(row.args, "/sbin/launchd");
    }

    #[test]
    fn everything_after_the_seventh_field_is_argv() {
        let line = "  912   1   912   4096   1.0 05:00 tester  /usr/bin/claude -p  \"two  spaces\" --model opus";
        let row = PsRow::parse_line(line).expect("parses");
        assert_eq!(row.pid, 912);
        assert_eq!(
            row.args,
            "/usr/bin/claude -p  \"two  spaces\" --model opus",
            "argv spacing must survive parsing"
        );
        assert_eq!(row.name(), "claude");
    }

    #[test]
    fn login_shell_argv0_loses_its_dash_in_the_display_name() {
        let row = ps_row(5, 1, "-zsh");
        assert_eq!(row.name(), "zsh");
    }

    #[test]
    fn malformed_lines_are_skipped_not_fatal() {
        assert!(PsRow::parse_line("").is_none());
        assert!(PsRow::parse_line("   ").is_none());
        assert!(PsRow::parse_line("PID PPID PGID RSS %CPU ELAPSED USER COMMAND").is_none());
        assert!(PsRow::parse_line("1 0 1 100 0.0 root /bin/x").is_none(), "bad etime");
        assert!(PsRow::parse_line("1 0 1 100 0.0 00:01").is_none(), "too few fields");
        // A row whose argv is empty is still a process: keep it. Hiding a
        // process from the monitor is worse than showing it without a name.
        let bare = PsRow::parse_line("1 0 1 100 0.0 00:01 root").expect("kept");
        assert_eq!(bare.args, "");
        assert_eq!(bare.name(), "");
    }

    #[test]
    fn etime_parses_all_four_shapes() {
        assert_eq!(parse_etime("07"), Some(7));
        assert_eq!(parse_etime("12:34"), Some(12 * 60 + 34));
        assert_eq!(parse_etime("01:02:03"), Some(3600 + 2 * 60 + 3));
        assert_eq!(parse_etime("2-03:04:05"), Some(2 * 86_400 + 3 * 3600 + 4 * 60 + 5));
        assert_eq!(parse_etime("48-12:24:08"), Some(48 * 86_400 + 12 * 3600 + 24 * 60 + 8));
        assert_eq!(parse_etime(""), None);
        assert_eq!(parse_etime("nope"), None);
        assert_eq!(parse_etime("1:2:3:4"), None);
    }

    // -- tree walks ---------------------------------------------------------

    #[test]
    fn descendants_and_ancestors_walk_a_normal_tree() {
        let snap = Snapshot::from_rows(vec![
            ps_row(1, 0, "/sbin/launchd"),
            ps_row(10, 1, "/bin/parent"),
            ps_row(11, 10, "/bin/child-a"),
            ps_row(12, 10, "/bin/child-b"),
            ps_row(13, 11, "/bin/grandchild"),
            ps_row(20, 1, "/bin/unrelated"),
        ]);

        let mut kids = snap.descendants_of(10);
        kids.sort_unstable();
        assert_eq!(kids, vec![11, 12, 13]);
        assert_eq!(snap.ancestors_of(13), vec![11, 10, 1]);
        assert!(snap.descendants_of(20).is_empty());
        assert!(snap.ancestors_of(1).is_empty(), "ppid 0 is not an ancestor");
    }

    #[test]
    fn tree_walks_terminate_on_a_synthetic_ppid_cycle() {
        // Impossible from the kernel, trivially producible by a racing snapshot.
        let snap = Snapshot::from_rows(vec![
            ps_row(100, 102, "/bin/a"),
            ps_row(101, 100, "/bin/b"),
            ps_row(102, 101, "/bin/c"),
        ]);

        let mut kids = snap.descendants_of(100);
        kids.sort_unstable();
        assert_eq!(kids, vec![101, 102]);
        assert_eq!(snap.ancestors_of(100), vec![102, 101]);

        // A process that parents itself must not loop either.
        let selfy = Snapshot::from_rows(vec![ps_row(7, 7, "/bin/self")]);
        assert!(selfy.descendants_of(7).is_empty());
        assert!(selfy.ancestors_of(7).is_empty());
    }

    #[test]
    fn pids_in_group_selects_the_whole_group() {
        let mut a = ps_row(30, 1, "/bin/leader");
        a.pgid = 30;
        let mut b = ps_row(31, 30, "/bin/member");
        b.pgid = 30;
        let c = ps_row(40, 1, "/bin/other");
        let snap = Snapshot::from_rows(vec![a, b, c]);

        let mut group = snap.pids_in_group(30);
        group.sort_unstable();
        assert_eq!(group, vec![30, 31]);
        assert!(snap.pids_in_group(0).is_empty(), "pgid 0 is never a target");
        assert!(snap.pids_in_group(-1).is_empty());
    }

    /// A pid the kernel cannot have handed out, so the `proc_pidpath` fallback
    /// inside [`Snapshot::is_app_bundle_main`] is guaranteed to answer `None`
    /// and a negative assertion below cannot depend on what happens to be
    /// running on the machine.
    const IMPOSSIBLE_PID: i32 = 2_000_000_001;

    #[test]
    fn app_bundle_main_matches_only_the_bundle_executable() {
        assert!(is_app_bundle_path(
            "/Applications/Cursor.app/Contents/MacOS/Cursor"
        ));
        assert!(!is_app_bundle_path(
            "/Applications/Cursor.app/Contents/Frameworks/Cursor Helper.app/Contents/MacOS/Cursor Helper/extra"
        ));
        assert!(!is_app_bundle_path("/usr/bin/claude"));

        let snap = Snapshot::from_rows(vec![
            ps_row(50, 1, "/Applications/Notes.app/Contents/MacOS/Notes"),
            ps_row(IMPOSSIBLE_PID, 1, "/usr/bin/claude -p hello"),
        ]);
        assert!(snap.is_app_bundle_main(50));
        assert!(!snap.is_app_bundle_main(IMPOSSIBLE_PID));
        assert!(
            !snap.is_app_bundle_main(IMPOSSIBLE_PID + 1),
            "unknown pid is not protected-by-bundle"
        );
    }

    #[test]
    fn bundle_paths_containing_spaces_are_still_bundle_mains() {
        // The regression this test exists for: `ps` prints argv as one string,
        // so "argv[0] = first token" turns "/Applications/Google Chrome.app/…"
        // into "/Applications/Google" and the user's browser loses its
        // protection (DESIGN §8-6).
        let chrome = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
        let helper = "/Applications/Google Chrome.app/Contents/Frameworks/\
                      Google Chrome Framework.framework/Versions/120/Helpers/\
                      Google Chrome Helper (Renderer).app/Contents/MacOS/\
                      Google Chrome Helper (Renderer)";
        let code = "/Applications/Visual Studio Code.app/Contents/MacOS/Electron";

        for path in [chrome, helper, code] {
            assert!(is_app_bundle_path(path), "path form: {path:?}");
        }

        let snap = Snapshot::from_rows(vec![
            ps_row(60, 1, chrome),
            ps_row(61, 60, &format!("{helper} --type=renderer --lang=en-US")),
            ps_row(62, 1, &format!("{code} --enable-crash-reporter")),
        ]);
        for pid in [60, 61, 62] {
            assert!(
                snap.is_app_bundle_main(pid),
                "pid {pid} is a bundle main and must be protected: {:?}",
                snap.get(pid).map(|r| r.args.as_str())
            );
        }
    }

    #[test]
    fn a_bundle_path_that_is_only_an_argument_does_not_protect_the_process() {
        // Over-protecting is the safe direction, but a bundle path handed to a
        // tool as an argument must not make that tool unkillable.
        assert!(bundle_main_head(
            "/usr/bin/open -a /Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
        )
        .is_none());
        assert!(bundle_main_head(
            "/usr/bin/claude -p '/Applications/Notes.app/Contents/MacOS/Notes'"
        )
        .is_none());
        assert!(bundle_main_head("/usr/bin/claude -p hello").is_none());
        // iTerm2 starts every shell through `login` with its own helper as an
        // argument; the slash-free tail is what fooled a whole-line regex.
        assert!(bundle_main_head(
            "/usr/bin/login -fpl someone /Applications/iTerm.app/Contents/MacOS/ShellLauncher --launch_shell"
        )
        .is_none());
        assert!(bundle_main_head("").is_none());
        // A relative path is not how launchd starts a bundle, and the
        // `proc_pidpath` fallback is what answers for it.
        assert!(bundle_main_head("Foo.app/Contents/MacOS/Foo").is_none());
        // No executable name after the marker.
        assert!(bundle_main_head("/Applications/Foo.app/Contents/MacOS/").is_none());

        // And when it does begin with one, the path comes back without the
        // arguments that follow it — that is what tells our own sidecar apart
        // from an application's main binary.
        // Cut at the first space, as documented: `ps` cannot say whether the
        // rest of the line is the rest of the name or the first argument. The
        // head is only ever asked "is this file ours", and `Google` is not.
        assert_eq!(
            bundle_main_head(
                "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome --no-startup-window"
            ),
            Some("/Applications/Google Chrome.app/Contents/MacOS/Google")
        );
        assert_eq!(
            bundle_main_head("/Applications/Notes.app/Contents/MacOS/Notes"),
            Some("/Applications/Notes.app/Contents/MacOS/Notes")
        );
    }

    #[test]
    fn every_live_bundle_main_on_this_machine_is_tagged_protected() {
        // The unit-level mirror of the integration assertion: a process whose
        // executable is a bundle main must come out of `build_row` with both
        // tags — unless it is our own bundled sidecar, which lives in the same
        // directory and is ours to terminate (PINNED §23-1).
        //
        // "Whose executable is" is the kernel's answer (`proc_pidpath`), not
        // the regex over the whole `ps` string: `[^/]+$` also matches a command
        // that merely *takes* a bundle path as an argument with a slash-free
        // tail, e.g. iTerm2's `/usr/bin/login -fpl <user>
        // /Applications/iTerm.app/Contents/MacOS/ShellLauncher --launch_shell`,
        // which is `login`, not a GUI app. The string form is only the fallback
        // when the kernel will not say (the process is gone, or not ours to ask).
        let snap = ps_snapshot().expect("ps snapshot");
        let registry = RegistryIndex::new("inst-current");
        let sigs = Signatures::defaults();
        let ctx = RowContext::new(&snap, &registry, &sigs);

        let mut checked = 0usize;
        let mut ours = 0usize;
        for ps in snap.rows() {
            let exe = exe_path_of(ps.pid);
            let program = exe.as_deref().unwrap_or(ps.args.as_str());
            if !is_app_bundle_path(program) {
                continue;
            }
            let row = snap.build_row(ps, &ctx);
            if is_our_sidecar_path(program) {
                ours += 1;
                assert!(
                    !row.has_tag(ProcTag::AppBundleMain),
                    "pid {} is our own sidecar, not a foreground app: {:?}",
                    ps.pid,
                    ps.args
                );
                continue;
            }
            checked += 1;
            assert!(
                row.has_tag(ProcTag::AppBundleMain),
                "pid {} ({}) is a bundle main but was not tagged: {:?}",
                ps.pid,
                ps.name(),
                ps.args
            );
            assert!(
                row.is_protected(),
                "pid {} ({}) is a bundle main but is offered as a kill target",
                ps.pid,
                ps.name()
            );
        }
        println!(
            "T07 bundle mains checked on this machine: {checked} \
             (plus {ours} of our own sidecars)"
        );
    }

    // -- our own bundled sidecar is not a foreground app (PINNED §23-1) ------

    /// The packaged defect, as a unit test.
    ///
    /// `externalBin` puts the pinned Node runtime beside the app's own main
    /// executable, so the orphaned sidecar of a crashed run matched
    /// `.*\.app/Contents/MacOS/[^/]+$` and was refused by both the Recovered
    /// Sessions sheet and the Process Monitor. Only the bundle layout differs
    /// between dev and release, which is why `cargo test` never saw it: these
    /// three cases put both layouts in front of the predicate.
    #[test]
    fn our_own_bundled_sidecar_is_killable_and_other_apps_are_not() {
        let dir = tempfile::tempdir().expect("tempdir");
        let macos = dir
            .path()
            .join("Gilde Code.app")
            .join("Contents")
            .join("MacOS");
        std::fs::create_dir_all(&macos).expect("bundle layout");
        // The two files Tauri puts in `Contents/MacOS`: the app and its
        // `externalBin`. The main executable's presence is half of the identity
        // check, so it has to exist on disk.
        std::fs::write(macos.join(OUR_EXE_NAME), b"#!/bin/sh\n").expect("main exe");
        let sidecar = macos.join("gilde-node");
        std::fs::write(&sidecar, b"#!/bin/sh\n").expect("sidecar");
        let sidecar = sidecar.to_string_lossy().into_owned();

        // Someone else's app, in exactly the same shape.
        let theirs = dir
            .path()
            .join("Some Editor.app")
            .join("Contents")
            .join("MacOS");
        std::fs::create_dir_all(&theirs).expect("their bundle");
        let their_main = theirs.join("Some Editor").to_string_lossy().into_owned();
        let their_helper = theirs.join("gilde-node").to_string_lossy().into_owned();

        assert!(
            is_our_sidecar_path(&sidecar),
            "our sidecar sits next to our own main executable: {sidecar}"
        );
        assert!(
            !is_our_sidecar_path(&their_main),
            "another app's main binary is never ours"
        );
        assert!(
            !is_our_sidecar_path(&their_helper),
            "and neither is a same-named binary in a bundle that is not ours: \
             {their_helper}"
        );

        // `ps_row` invents a user; `build_row` protects anything that is not
        // ours, so these rows have to claim this user to isolate the tag under
        // test.
        let ours = |pid: i32, args: &str| {
            let mut row = ps_row(pid, 1, args);
            row.user = std::env::var("USER").unwrap_or_default();
            row
        };
        let snap = Snapshot::from_rows(vec![
            ours(70, &format!("{sidecar} /Contents/Resources/sidecar/sidecar.cjs")),
            ours(71, &their_main),
            ours(72, &their_helper),
        ]);
        assert!(
            !snap.is_app_bundle_main(70),
            "the orphaned sidecar must be a legal kill target"
        );
        assert!(snap.is_app_bundle_main(71), "their app main is still refused");
        assert!(
            snap.is_app_bundle_main(72),
            "a bundle we cannot prove is ours is still refused"
        );

        // And the row model agrees: no `appBundleMain` tag, no `protected`.
        let registry = RegistryIndex::new("inst-current");
        let sigs = Signatures::defaults();
        let ctx = RowContext::new(&snap, &registry, &sigs);
        let row = snap.build_row(snap.get(70).expect("row"), &ctx);
        assert!(!row.has_tag(ProcTag::AppBundleMain));
        assert!(
            !row.is_protected(),
            "`Clean N orphans` filters protected rows out, so this tag is the \
             difference between a UI path and `kill -9` in a shell"
        );
    }

    #[test]
    fn the_dev_tree_sidecar_is_killable_too() {
        // `src-tauri/binaries/gilde-node-<target-triple>` — the path the dev build
        // runs. It never matched the bundle pattern, which is exactly why the
        // defect only ever appeared in the packaged app; this pins that the fix
        // did not move the dev tree into protection either.
        let dev = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/binaries/gilde-node-aarch64-apple-darwin"
        );
        assert!(!is_app_bundle_path(dev), "the dev tree is not a bundle");
        let snap = Snapshot::from_rows(vec![ps_row(
            80,
            1,
            &format!("{dev} /some/where/sidecar.cjs"),
        )]);
        assert!(!snap.is_app_bundle_main(80));

        // The name check on its own: the triple-suffixed form is still our
        // runtime's name, and nothing else is.
        assert!(is_sidecar_exe_name("gilde-node"));
        assert!(is_sidecar_exe_name("gilde-node-aarch64-apple-darwin"));
        assert!(!is_sidecar_exe_name("gilde-nodes"));
        assert!(!is_sidecar_exe_name("node"));
        assert!(!is_sidecar_exe_name("Some Editor"));
    }

    // -- masking ------------------------------------------------------------

    #[test]
    fn mask_argv_redacts_every_documented_secret_shape() {
        let fixtures = [
            "claude --api-key=sk-abcdefghijklmnopqrstuvwx",
            "codex sk-proj_ABCDEFGHIJKLMNOPQRST0123",
            "gh auth login --with-token ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ012345",
            "curl -H 'Authorization: Bearer eyJhbGciOi.J9.sig'",
            "slack-cli --token xoxb-1234-5678-abcdefg",
            "tool --api-key=SUPERSECRETVALUE",
            "tool --api_key SUPERSECRETVALUE",
            "tool -token=SUPERSECRETVALUE",
            "tool --password hunter2hunter2",
            "tool --secret=zzzzzzzzzzzz",
        ];
        for f in fixtures {
            let masked = mask_argv(f);
            assert!(masked.contains(REDACTED), "nothing redacted in {f:?} -> {masked:?}");
            for leak in [
                "sk-abcdefghijklmnopqrstuvwx",
                "sk-proj_ABCDEFGHIJKLMNOPQRST0123",
                "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ012345",
                "eyJhbGciOi.J9.sig",
                "xoxb-1234-5678-abcdefg",
                "SUPERSECRETVALUE",
                "hunter2hunter2",
                "zzzzzzzzzzzz",
            ] {
                assert!(!masked.contains(leak), "{leak} survived masking of {f:?}");
            }
        }
    }

    #[test]
    fn mask_argv_leaves_ordinary_command_lines_alone() {
        let plain = "/usr/bin/claude --model opus --permission-mode acceptEdits";
        assert_eq!(mask_argv(plain), plain);
        // A short `sk-` word is not an API key.
        assert_eq!(mask_argv("edit sk-note.md"), "edit sk-note.md");
    }

    #[test]
    fn user_prompts_are_truncated_for_display_but_never_masked() {
        let prompt = "a".repeat(500);
        let argv = format!("/usr/bin/claude -- {prompt}");
        let shown = argv_display(&argv);

        assert!(shown.starts_with("/usr/bin/claude -- aaa"));
        assert!(shown.ends_with('\u{2026}'), "truncation is elided: {shown:?}");
        assert!(!shown.contains(REDACTED), "a prompt is not a secret");
        // separator + space + limit + ellipsis
        let tail = shown.split("--").nth(1).expect("tail");
        assert_eq!(tail.chars().count(), PROMPT_DISPLAY_LIMIT + 1);
    }

    #[test]
    fn short_prompts_are_left_intact() {
        let argv = "/usr/bin/claude -- fix the resize bug";
        assert_eq!(argv_display(argv), argv);
    }

    #[test]
    fn double_dash_inside_a_flag_is_not_a_prompt_separator() {
        let argv = "/usr/bin/codex --model gpt-5 --sandbox workspace-write";
        assert_eq!(truncate_prompt(argv), argv);
    }

    #[test]
    fn truncation_never_splits_a_character() {
        let prompt = "가".repeat(400);
        let argv = format!("/usr/bin/agy -- {prompt}");
        let shown = argv_display(&argv);
        assert!(shown.is_char_boundary(shown.len()));
        assert!(shown.contains('가'));
    }

    // -- classification -----------------------------------------------------

    fn registry_with(pid: i32, instance: &str, keep_alive: bool, owner_alive: bool) -> RegistryIndex {
        let mut index = RegistryIndex::new("inst-current");
        index.insert(RegistryEntry {
            spawn_id: "sp-1".into(),
            app_instance: instance.into(),
            generation: 0,
            kind: ProcKind::Pty,
            vendor: Some("claude".into()),
            session_id: Some("sess-1".into()),
            run_id: None,
            worktree_id: Some("wt-1".into()),
            state: ProcState::Running,
            pid: Some(pid),
            pgid: Some(pid),
            proc_start_ts: Some(1_700_000_000),
            keep_alive,
            owner_alive,
        });
        index
    }

    #[test]
    fn registered_rows_of_this_instance_are_ours_live() {
        let sigs = Signatures::defaults();
        let registry = registry_with(200, "inst-current", false, true);
        let (group, tags) = classify(&ps_row(200, 1, "/usr/bin/claude"), &registry, &sigs);
        assert_eq!(group, ProcGroupKind::OursLive);
        assert!(tags.contains(&ProcTag::Ours));
        assert!(tags.contains(&ProcTag::InWorktree));
        assert!(!tags.contains(&ProcTag::Orphan));
    }

    #[test]
    fn registered_rows_of_an_older_instance_or_a_dead_owner_are_orphans() {
        let sigs = Signatures::defaults();

        let old = registry_with(200, "inst-previous", false, true);
        let (group, tags) = classify(&ps_row(200, 1, "/usr/bin/claude"), &old, &sigs);
        assert_eq!(group, ProcGroupKind::OursOrphan);
        assert!(tags.contains(&ProcTag::Orphan));

        let dead_owner = registry_with(200, "inst-current", true, false);
        let (group, tags) = classify(&ps_row(200, 1, "/usr/bin/claude"), &dead_owner, &sigs);
        assert_eq!(group, ProcGroupKind::OursOrphan);
        assert!(tags.contains(&ProcTag::Detached), "keep_alive rows are badged");
    }

    #[test]
    fn unregistered_agents_mcp_servers_and_residue_land_in_their_groups() {
        let sigs = Signatures::defaults();
        let empty = RegistryIndex::new("inst-current");
        let cases = [
            ("/Users/me/.nvm/versions/node/v20.19.2/bin/claude -p hi", ProcGroupKind::AgentUnmanaged),
            ("/usr/local/bin/codex exec", ProcGroupKind::AgentUnmanaged),
            ("/opt/homebrew/bin/cursor-agent --resume", ProcGroupKind::AgentUnmanaged),
            ("/Users/me/.local/bin/agy", ProcGroupKind::AgentUnmanaged),
            ("node /Users/me/.nvm/versions/node/v20.19.2/bin/gemini", ProcGroupKind::AgentUnmanaged),
            ("node /opt/pkg/mcp-server-filesystem/index.js", ProcGroupKind::McpServer),
            ("npx -y @modelcontextprotocol/server-github", ProcGroupKind::McpServer),
            ("python3 -m mcp_server.main", ProcGroupKind::McpServer),
            ("/Applications/Cursor.app/Contents/Frameworks/Cursor Helper (Renderer).app/Contents/MacOS/Cursor Helper (Renderer)", ProcGroupKind::ExternalResidue),
            ("/usr/local/lib/node_modules/typescript/lib/tsserver.js --useInferredProjectPerProjectRoot", ProcGroupKind::ExternalResidue),
            ("/bin/zsh -l", ProcGroupKind::Other),
            ("/usr/sbin/cfprefsd agent", ProcGroupKind::Other),
        ];
        for (args, expected) in cases {
            let (group, _) = classify(&ps_row(300, 1, args), &empty, &sigs);
            assert_eq!(group, expected, "misclassified {args:?}");
        }
    }

    #[test]
    fn a_broken_user_pattern_warns_and_keeps_the_rest() {
        let (sigs, warnings) = Signatures::compile(&["claude", "((("], &["mcp"], &["tsserver"]);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("agent"));
        assert!(sigs.is_agent("/usr/bin/claude"), "the good pattern still works");
        assert!(sigs.is_mcp_server("node mcp/index.js"));
        assert!(sigs.is_external_residue("tsserver"));
    }

    // -- live process facts (this machine) ----------------------------------

    #[test]
    fn ps_snapshot_sees_this_process() {
        let snap = ps_snapshot().expect("ps snapshot");
        let me = std::process::id() as i32;
        assert!(snap.len() > 10, "implausibly small snapshot: {}", snap.len());
        let row = snap.get(me).expect("our own row");
        assert_eq!(row.pid, me);
        assert!(row.rss_kb > 0);
        assert!(snap.contains(1), "launchd is always there");
        assert!(snap.at() > 0);
    }

    #[test]
    fn our_ancestor_chain_is_walkable_from_the_live_snapshot() {
        let snap = ps_snapshot().expect("ps snapshot");
        let me = std::process::id() as i32;
        let ancestors = snap.ancestors_of(me);
        assert!(!ancestors.is_empty(), "we have at least one parent");
        assert!(!ancestors.contains(&me), "we are not our own ancestor");
    }

    #[test]
    fn ps_args_of_reports_our_own_command_line() {
        let me = std::process::id() as i32;
        let args = ps_args_of(me).expect("our argv");
        assert!(!args.is_empty());
        assert!(ps_args_of(-1).is_none());
    }

    #[test]
    fn cwd_of_self_matches_current_dir() {
        // This is also the end-to-end check that the `proc_vnodepathinfo`
        // mirror above has the right layout.
        let me = std::process::id() as i32;
        let cwd = cwd_of(me).expect("our cwd");
        let expected = std::env::current_dir().expect("current_dir");
        assert_eq!(
            cwd.canonicalize().unwrap_or(cwd.clone()),
            expected.canonicalize().unwrap_or(expected.clone())
        );
        assert!(cwd_of(0).is_none());
    }

    #[test]
    fn the_lsof_cwd_fallback_works_on_its_own() {
        // `cwd_of` normally never reaches this branch, so exercise it directly:
        // an untested fallback is not a fallback.
        let me = std::process::id() as i32;
        let via_lsof = cwd_via_lsof(me).expect("lsof reports our cwd");
        let expected = std::env::current_dir().expect("current_dir");
        assert_eq!(
            via_lsof.canonicalize().unwrap_or(via_lsof.clone()),
            expected.canonicalize().unwrap_or(expected.clone())
        );
    }

    #[test]
    fn a_timed_out_helper_returns_none_instead_of_blocking_the_scanner() {
        let started = Instant::now();
        let out = run_capture("/bin/sleep", &["30"], Duration::from_millis(150));
        let elapsed = started.elapsed();
        assert!(out.is_none(), "a helper past its deadline yields nothing");
        assert!(
            elapsed < Duration::from_secs(3),
            "run_capture waited {elapsed:?} — the scan timer would have stalled"
        );
    }

    #[test]
    fn start_time_and_uid_of_self_are_sane() {
        let me = std::process::id() as i32;
        let start = start_time_of(me).expect("start time");
        let now = now_millis() / 1000;
        assert!(start > 1_600_000_000, "implausible start time {start}");
        assert!(start <= now + 1, "start time is in the future");

        let uid = uid_of(me).expect("uid");
        // SAFETY: getuid() is always successful and takes no arguments.
        assert_eq!(uid, unsafe { libc::getuid() });

        assert!(start_time_of(0).is_none());
        assert!(uid_of(-5).is_none());
    }

    #[test]
    fn environ_failure_is_a_normal_result_not_a_panic() {
        // Our own environment is readable; a pid that cannot exist is not.
        let me = std::process::id() as i32;
        match environ_of(me) {
            Some(env) => assert!(!env.is_empty(), "our own environment is not empty"),
            None => { /* also acceptable: DESIGN §8-4 */ }
        }
        assert!(environ_of(0).is_none());
        assert!(environ_of(-1).is_none());
        // The whole point: no panic, no Err.
        let _ = environ_of(i32::MAX);
    }

    #[test]
    fn total_memory_reports_physical_ram() {
        let total = total_memory();
        assert!(total > 1 << 30, "implausible physical RAM: {total}");
    }

    #[test]
    fn build_rows_produces_masked_rows_and_protects_the_untouchable() {
        let snap = ps_snapshot().expect("ps snapshot");
        let registry = RegistryIndex::new("inst-current");
        let sigs = Signatures::defaults();
        let ctx = RowContext::new(&snap, &registry, &sigs);
        let rows = snap.build_rows(&ctx);

        assert_eq!(rows.len(), snap.len());
        let me = rows
            .iter()
            .find(|r| r.pid == ctx.self_pid)
            .expect("our own row");
        assert!(me.user_owned);
        assert!(me.is_protected(), "we never offer to kill ourselves");
        assert!(me.cwd.is_none(), "cwd resolution is opt-in");
        assert!(me.start_time > 1_600_000_000_000, "start_time is unix millis");

        let launchd = rows.iter().find(|r| r.pid == 1).expect("launchd");
        assert!(launchd.is_protected(), "pid 1 is always protected");
        assert!(!launchd.user_owned);

        for ancestor in &ctx.protected_ancestors {
            if let Some(row) = rows.iter().find(|r| r.pid == *ancestor) {
                assert!(row.is_protected(), "ancestor {ancestor} must be protected");
            }
        }
    }
}
