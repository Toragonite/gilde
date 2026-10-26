//! The in-process process scan (contract §5.2.2, §5.2.3, §9.8): libproc,
//! never `ps` per tick.
//!
//! * [`Libproc`] is the real [`ProcSource`]: `proc_listallpids`,
//!   `proc_pidinfo(PROC_PIDTBSDINFO | PROC_PIDTASKINFO | PROC_PIDVNODEPATHINFO)`,
//!   `proc_pidpath`, `sysctl(KERN_PROCARGS2)` and `kill(pid, 0)`. It only
//!   observes: nothing here sends a signal (`kill(pid, 0)` delivers none).
//! * [`parse_procargs2`] reads `argc`, the exec path and exactly `argc`
//!   strings; the environment block that follows is **not copied** — it is
//!   scanned in place for the *names* `CCO_TEAM_MEMBER=` and
//!   `GILDE_APP_INSTANCE=` — and the caller zeroes the buffer (§5.2.2).
//! * [`classify`] is the vendor-CLI classifier (also the §9.8 signal policy)
//!   and the role rule of F-47: `--bg` helpers are `VendorHelper`, never
//!   sessions.
//! * [`Scanner`] keeps one entry per `(pid, start)`, parsed once per process
//!   lifetime. **Raw argv never leaves the cache boundary** [F-03]: it is used
//!   to classify and to read the allowlisted label flags, then the redactor's
//!   output is all that is kept, and the raw strings are zeroed.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use zeroize::Zeroize;

use crate::contract::common::{EpochMs, Vendor};
use crate::contract::ids::ProcessKey;
use crate::contract::process::ProcRole;
use crate::contract::services::{ProcInfo, ProcSnapshot, TeammateLabels};
use crate::contract::session::{Origin, OwnerApp, OwnerKind};

use super::{is_vendor_uuid, OwnMatch, OwnProcesses, Redactor};

/// Scan cadence while Gilde is frontmost / otherwise (§5.2.2).
pub const SCAN_FOCUSED_MS: u64 = 3_000;
pub const SCAN_BACKGROUND_MS: u64 = 15_000;
/// An unregistered Claude process is not shown for its first 10 s (§5.2.2).
pub const UNREGISTERED_GRACE_MS: u64 = 10_000;
/// Ancestor walk depth for the owner app (§5.2.3).
pub const OWNER_WALK_MAX: usize = 32;

// ---------------------------------------------------------------------------
// the source
// ---------------------------------------------------------------------------

/// `PROC_PIDTBSDINFO` facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BsdInfo {
    pub pid: i32,
    pub ppid: i32,
    pub uid: u32,
    /// Kernel start time, seconds (`pbi_start_tvsec`).
    pub start_sec: i64,
    /// `pbi_name` (or `pbi_comm`).
    pub comm: String,
}

/// `kill(pid, 0)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alive {
    Yes,
    No,
    /// `EPERM`: another user's process — excluded, never signalled (§5.2.1).
    OtherUser,
}

/// argv as `KERN_PROCARGS2` returned it. Raw: it exists only inside the cache
/// boundary and is zeroed on drop.
#[derive(Default)]
pub struct RawArgs {
    pub exec_path: Option<String>,
    /// `None` when any argument is not valid UTF-8 (classified by exec path only).
    pub argv: Option<Vec<String>>,
    /// Env-name markers; `None` = the env block could not be read.
    pub cco_team_member: Option<bool>,
    pub gilde_app_instance: Option<bool>,
}

impl Drop for RawArgs {
    fn drop(&mut self) {
        if let Some(argv) = self.argv.as_mut() {
            for a in argv.iter_mut() {
                a.zeroize();
            }
        }
    }
}

impl std::fmt::Debug for RawArgs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RawArgs")
            .field("argc", &self.argv.as_ref().map(Vec::len))
            .field("cco_team_member", &self.cco_team_member)
            .field("gilde_app_instance", &self.gilde_app_instance)
            .finish()
    }
}

/// `PROC_PIDTASKINFO` facts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TaskInfo {
    pub rss_bytes: u64,
    pub cpu_ns: u64,
}

/// Where process facts come from (the real libproc, or a fake in tests).
pub trait ProcSource: Send + Sync {
    fn list_pids(&self) -> Vec<i32>;
    fn bsd_info(&self, pid: i32) -> Option<BsdInfo>;
    fn exec_path(&self, pid: i32) -> Option<String>;
    fn args(&self, pid: i32) -> Option<RawArgs>;
    fn cwd(&self, pid: i32) -> Option<String>;
    fn task_info(&self, pid: i32) -> Option<TaskInfo>;
    fn alive(&self, pid: i32) -> Alive;
    fn my_uid(&self) -> u32;
    /// The parent pid and command name, for the owner walk. Unlike
    /// [`Self::bsd_info`] it also answers for another user's process: the
    /// root-owned `login` between Terminal and the shell.
    fn parent_of(&self, pid: i32) -> Option<(i32, String)> {
        self.bsd_info(pid).map(|b| (b.ppid, b.comm))
    }
}

/// The real source.
pub struct Libproc {
    argmax: usize,
    numer: u64,
    denom: u64,
    /// The `KERN_PROCARGS2` buffer, reused and zeroed after every read.
    scratch: Mutex<Vec<u8>>,
}

impl Default for Libproc {
    fn default() -> Self {
        Self::new()
    }
}

impl Libproc {
    pub fn new() -> Libproc {
        let mut argmax: libc::c_int = 0;
        let mut size = std::mem::size_of::<libc::c_int>();
        let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
        // SAFETY: valid mib, out-pointer and size.
        let ok = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                2,
                (&mut argmax as *mut libc::c_int).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        } == 0;
        let argmax = if ok && argmax > 0 { argmax as usize } else { 1 << 20 };
        #[repr(C)]
        struct Timebase {
            numer: u32,
            denom: u32,
        }
        extern "C" {
            fn mach_timebase_info(info: *mut Timebase) -> i32;
        }
        let mut tb = Timebase { numer: 1, denom: 1 };
        // SAFETY: valid out-pointer to a `mach_timebase_info_data_t`-shaped struct.
        unsafe {
            mach_timebase_info(&mut tb);
        }
        Libproc {
            argmax,
            numer: u64::from(tb.numer.max(1)),
            denom: u64::from(tb.denom.max(1)),
            scratch: Mutex::new(Vec::new()),
        }
    }
}

fn c_chars_to_string(chars: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = chars.iter().take_while(|c| **c != 0).map(|c| *c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

impl ProcSource for Libproc {
    fn list_pids(&self) -> Vec<i32> {
        // SAFETY: a NULL buffer asks for the count.
        let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        if n <= 0 {
            return Vec::new();
        }
        let cap = n as usize + 64;
        let mut buf = vec![0i32; cap];
        // SAFETY: `buf` holds `cap` i32s; the size is passed in bytes.
        let got = unsafe {
            libc::proc_listallpids(
                buf.as_mut_ptr().cast(),
                (cap * std::mem::size_of::<i32>()) as libc::c_int,
            )
        };
        if got <= 0 {
            return Vec::new();
        }
        buf.truncate((got as usize).min(cap));
        buf.retain(|p| *p > 0);
        buf
    }

    fn bsd_info(&self, pid: i32) -> Option<BsdInfo> {
        // SAFETY: all-zero is a valid `proc_bsdinfo`.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // SAFETY: `info` is valid for `size` bytes.
        let r = unsafe {
            libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&mut info as *mut libc::proc_bsdinfo).cast(), size)
        };
        if r != size {
            return None;
        }
        let name = c_chars_to_string(&info.pbi_name);
        let comm = if name.is_empty() { c_chars_to_string(&info.pbi_comm) } else { name };
        Some(BsdInfo {
            pid,
            ppid: info.pbi_ppid as i32,
            uid: info.pbi_uid,
            start_sec: info.pbi_start_tvsec as i64,
            comm,
        })
    }

    /// `PROC_PIDTBSDINFO` is refused (`EPERM`) for another user's process;
    /// `PROC_PIDT_SHORTBSDINFO` is not.
    fn parent_of(&self, pid: i32) -> Option<(i32, String)> {
        if let Some(b) = self.bsd_info(pid) {
            return Some((b.ppid, b.comm));
        }
        // SAFETY: all-zero is a valid `proc_bsdshortinfo`.
        let mut info: libc::proc_bsdshortinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdshortinfo>() as libc::c_int;
        // SAFETY: `info` is valid for `size` bytes.
        let r = unsafe {
            libc::proc_pidinfo(pid, libc::PROC_PIDT_SHORTBSDINFO, 0, (&mut info as *mut libc::proc_bsdshortinfo).cast(), size)
        };
        (r == size).then(|| (info.pbsi_ppid as i32, c_chars_to_string(&info.pbsi_comm)))
    }

    fn exec_path(&self, pid: i32) -> Option<String> {
        let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // SAFETY: `buf` is valid for its length.
        let r = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
        if r <= 0 {
            return None;
        }
        buf.truncate(r as usize);
        String::from_utf8(buf).ok()
    }

    fn args(&self, pid: i32) -> Option<RawArgs> {
        let mut scratch = self.scratch.lock().ok()?;
        if scratch.len() < self.argmax {
            scratch.resize(self.argmax, 0);
        }
        let mut size = scratch.len();
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        // SAFETY: `scratch` is valid for `size` bytes.
        let r = unsafe {
            libc::sysctl(mib.as_mut_ptr(), 3, scratch.as_mut_ptr().cast(), &mut size, std::ptr::null_mut(), 0)
        };
        if r != 0 {
            return None;
        }
        let size = size.min(scratch.len());
        let parsed = parse_procargs2(&scratch[..size]);
        // The env block was read into this buffer: wipe what the kernel wrote.
        scratch[..size].zeroize();
        parsed
    }

    fn cwd(&self, pid: i32) -> Option<String> {
        // SAFETY: all-zero is a valid `proc_vnodepathinfo`.
        let mut vpi: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
        // SAFETY: `vpi` is valid for `size` bytes.
        let r = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                (&mut vpi as *mut libc::proc_vnodepathinfo).cast(),
                size,
            )
        };
        if r != size {
            return None;
        }
        let flat: Vec<libc::c_char> = vpi.pvi_cdir.vip_path.iter().flatten().copied().collect();
        let s = c_chars_to_string(&flat);
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }

    fn task_info(&self, pid: i32) -> Option<TaskInfo> {
        // SAFETY: all-zero is a valid `proc_taskinfo`.
        let mut ti: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
        // SAFETY: `ti` is valid for `size` bytes.
        let r = unsafe {
            libc::proc_pidinfo(pid, libc::PROC_PIDTASKINFO, 0, (&mut ti as *mut libc::proc_taskinfo).cast(), size)
        };
        if r != size {
            return None;
        }
        let ticks = ti.pti_total_user.saturating_add(ti.pti_total_system);
        Some(TaskInfo {
            rss_bytes: ti.pti_resident_size,
            cpu_ns: ticks.saturating_mul(self.numer) / self.denom,
        })
    }

    fn alive(&self, pid: i32) -> Alive {
        if pid <= 0 {
            return Alive::No;
        }
        // SAFETY: signal 0 performs the permission and existence checks only;
        // no signal is delivered.
        if unsafe { libc::kill(pid, 0) } == 0 {
            return Alive::Yes;
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::EPERM) => Alive::OtherUser,
            _ => Alive::No,
        }
    }

    fn my_uid(&self) -> u32 {
        // SAFETY: always succeeds.
        unsafe { libc::getuid() }
    }
}

/// Parse a `KERN_PROCARGS2` buffer: `argc` (native i32), the exec path, NUL
/// padding, exactly `argc` argument strings, then the environment, which is
/// only scanned in place for the two marker *names*. The caller zeroes `buf`.
pub fn parse_procargs2(buf: &[u8]) -> Option<RawArgs> {
    if buf.len() < 4 {
        return None;
    }
    let argc = i32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if !(0..=65_536).contains(&argc) {
        return None;
    }
    let mut pos = 4usize;
    let end_of = |from: usize| buf[from..].iter().position(|b| *b == 0).map(|i| from + i);
    let exec_end = end_of(pos)?;
    let exec_path = std::str::from_utf8(&buf[pos..exec_end]).ok().map(str::to_string);
    pos = exec_end;
    while pos < buf.len() && buf[pos] == 0 {
        pos += 1;
    }
    let mut argv: Vec<String> = Vec::with_capacity(argc as usize);
    let mut utf8_ok = true;
    for _ in 0..argc {
        if pos >= buf.len() {
            utf8_ok = false;
            break;
        }
        let e = end_of(pos).unwrap_or(buf.len());
        match std::str::from_utf8(&buf[pos..e]) {
            Ok(s) => argv.push(s.to_string()),
            Err(_) => utf8_ok = false,
        }
        pos = e + 1;
    }
    let mut cco = false;
    let mut gilde = false;
    let mut env_count = 0usize;
    while pos < buf.len() {
        let e = end_of(pos).unwrap_or(buf.len());
        if e == pos {
            break; // the empty string that ends the environment
        }
        let s = &buf[pos..e];
        env_count += 1;
        if s.starts_with(b"CCO_TEAM_MEMBER=") {
            cco = true;
        } else if s.starts_with(b"GILDE_APP_INSTANCE=") {
            gilde = true;
        }
        pos = e + 1;
    }
    let readable = env_count > 0;
    let mut out = RawArgs {
        exec_path,
        argv: None,
        cco_team_member: readable.then_some(cco),
        gilde_app_instance: readable.then_some(gilde),
    };
    if utf8_ok {
        out.argv = Some(argv);
    } else {
        for a in argv.iter_mut() {
            a.zeroize();
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// classification (§5.2.2, §9.8, F-47)
// ---------------------------------------------------------------------------

/// `claude` subcommands (argv[1]) that are helpers or short-lived commands,
/// never sessions [F-47, R6 §6 LP-22]. Data, not branches.
pub const CLAUDE_HELPER_SUBCOMMANDS: &[&str] = &[
    "daemon",
    "bg-pty-host",
    "agents",
    "stop",
    "logs",
    "rm",
    "--version",
    "-v",
    "-h",
    "--help",
    "mcp",
    "doctor",
    "update",
    "upgrade",
    "install",
    "config",
    "setup-token",
    "plugin",
    "auth",
    "respawn",
    "migrate-installer",
];

/// Subcommands that may be recorded as a label (never a positional prompt).
pub const KNOWN_SUBCOMMANDS: &[&str] = &[
    "daemon", "bg-pty-host", "agents", "stop", "logs", "rm", "attach", "mcp", "doctor", "update",
    "upgrade", "install", "config", "setup-token", "plugin", "auth", "respawn", "resume", "exec",
    "app-server", "fork", "migrate-installer", "--version", "-v", "-h", "--help",
];

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The vendor-CLI classifier by executable path alone (§5.2.2, also §9.8
/// "vendor CLI"): the one shared classifier, [`crate::agents::classify`]
/// (A39.1(a)). `node`/`bun` need their argv ([`classify`]).
pub fn vendor_of_exec(exec_path: &str) -> Option<Vendor> {
    crate::agents::classify::vendor_of_exec_path(exec_path)
}

fn is_js_runtime(exec_path: &str) -> bool {
    matches!(basename(exec_path), "node" | "bun")
}

/// The allowlisted labels read from argv (§5.2.2): everything else in argv is
/// only ever stored in its redacted form.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArgvLabels {
    /// All three of `--agent-id`, `--agent-name`, `--team-name` present.
    pub teammate: Option<TeammateLabels>,
    pub model: Option<String>,
    /// claude `--resume <uuid>`, cursor `--resume <chatId>`, codex `resume <uuid>`.
    pub resume: Option<String>,
    /// agy `--conversation <uuid>`.
    pub conversation: Option<String>,
    /// The subcommand when it is a known word (never a positional prompt).
    pub subcommand: Option<String>,
    /// codex `app-server` (a VS Code child).
    pub app_server: bool,
    /// `tmux -L claude-swarm-<pid>`.
    pub swarm_lead_pid: Option<i32>,
}

/// The value of `--name value` or `--name=value`, anywhere in argv.
fn flag_value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    for (i, a) in args.iter().enumerate() {
        if a == name {
            return args.get(i + 1).map(String::as_str);
        }
        if let Some(rest) = a.strip_prefix(name).and_then(|r| r.strip_prefix('=')) {
            return Some(rest);
        }
    }
    None
}

fn label_ok(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && s.chars().all(|c| !c.is_control()) && !s.starts_with('-')
}

fn take_label(args: &[String], name: &str, max: usize) -> Option<String> {
    flag_value(args, name).filter(|v| label_ok(v, max)).map(str::to_string)
}

/// Read the allowlisted label flags (§5.2.2) from `args` = argv after the
/// program (and after the script for `node`/`bun`).
pub fn read_labels(vendor: Option<Vendor>, args: &[String]) -> ArgvLabels {
    let mut l = ArgvLabels::default();
    let agent_id = take_label(args, "--agent-id", 160).filter(|v| v.contains('@'));
    let agent_name = take_label(args, "--agent-name", 64);
    let team_name = take_label(args, "--team-name", 128);
    if let (Some(agent_id), Some(agent_name), Some(team_name)) = (agent_id, agent_name, team_name) {
        l.teammate = Some(TeammateLabels {
            agent_id: Some(agent_id),
            agent_name: Some(agent_name),
            team_name: Some(team_name),
            agent_color: take_label(args, "--agent-color", 32),
            parent_session_id: flag_value(args, "--parent-session-id")
                .filter(|v| is_vendor_uuid(v))
                .map(str::to_string),
        });
    }
    l.model = take_label(args, "--model", 96);
    let first = args.first().map(String::as_str);
    if let Some(sub) = first.filter(|s| KNOWN_SUBCOMMANDS.contains(s)) {
        l.subcommand = Some(sub.to_string());
    }
    match vendor {
        Some(Vendor::Claude) => {
            l.resume = flag_value(args, "--resume")
                .or_else(|| flag_value(args, "-r"))
                .filter(|v| is_vendor_uuid(v))
                .map(str::to_string);
        }
        Some(Vendor::Codex) => {
            l.app_server = args.iter().any(|a| a == "app-server");
            if let Some(i) = args.iter().position(|a| a == "resume") {
                l.resume = args.get(i + 1).filter(|v| is_vendor_uuid(v)).cloned();
            }
        }
        Some(Vendor::Cursor) => {
            l.resume = flag_value(args, "--resume")
                .filter(|v| (8..=64).contains(&v.len()) && v.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'))
                .map(str::to_string);
        }
        Some(Vendor::Agy) => {
            l.conversation = flag_value(args, "--conversation")
                .filter(|v| is_vendor_uuid(v))
                .map(str::to_string);
        }
        None => {
            if let Some(sock) = flag_value(args, "-L") {
                l.swarm_lead_pid = sock
                    .strip_prefix("claude-swarm-")
                    .and_then(|p| p.parse::<i32>().ok())
                    .filter(|p| *p > 0);
            }
        }
    }
    l
}

/// What [`classify`] decided.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Classified {
    pub vendor: Option<Vendor>,
    pub role: Option<ProcRole>,
    /// A process discovery tracks (vendor CLIs and `tmux`).
    pub candidate: bool,
    pub labels: ArgvLabels,
}

/// Classify a process from its exec path and (when readable) argv.
///
/// Role rule [F-47]: for Claude, argv[1] (argv[2] under `node`/`bun`) decides —
/// [`CLAUDE_HELPER_SUBCOMMANDS`] are `VendorHelper`; only plain, `-p`,
/// `--resume`, `--session-id`, `--agent-id`, `attach` and other flag or prompt
/// invocations can be sessions; complete teammate flags make a `Teammate`.
pub fn classify(exec_path: &str, argv: Option<&[String]>) -> Classified {
    // A39.1(a): the shared classifier, which also says where the CLI's own
    // arguments begin (after a `node`/`bun` script and its leading flags).
    let all: &[String] = argv.unwrap_or(&[]);
    let launch = crate::agents::classify::vendor_launch(exec_path, all);
    let vendor = launch.map(|l| l.vendor);
    let rest: &[String] = match launch {
        Some(l) => &all[l.args_from..],
        None => all.get(1..).unwrap_or(&[]),
    };
    let is_tmux = basename(exec_path) == "tmux";
    let labels = read_labels(vendor, rest);
    let role = match vendor {
        Some(Vendor::Claude) => Some(if labels.teammate.is_some() {
            ProcRole::Teammate
        } else if rest
            .first()
            .is_some_and(|a| CLAUDE_HELPER_SUBCOMMANDS.contains(&a.as_str()))
        {
            ProcRole::VendorHelper
        } else {
            ProcRole::Session
        }),
        Some(_) => Some(ProcRole::Session),
        None if is_tmux && labels.swarm_lead_pid.is_some() => Some(ProcRole::PaneServer),
        None => None,
    };
    Classified { vendor, role, candidate: vendor.is_some() || is_tmux, labels }
}

/// A one-shot `claude -p` whose working folder is the OS temporary folder:
/// how usage tools ask the CLI for its rate limits (the Claude Code
/// Orchestrator extension probes every account every 5 min from
/// `os.tmpdir()`, so no project's `.mcp.json` loads). It is nobody's session;
/// shown as one it filled a room named after the folder ("T") with ended
/// headless desks. `argv_redacted` keeps flags, so `-p`/`--print` survive.
pub fn is_temp_dir_print_probe(argv_redacted: &[String], cwd: Option<&str>) -> bool {
    static TEMP_DIRS: OnceLock<Vec<PathBuf>> = OnceLock::new();
    let print = argv_redacted.iter().skip(1).any(|a| a == "-p" || a == "--print");
    let Some(cwd) = cwd.filter(|_| print) else { return false };
    let temp = TEMP_DIRS.get_or_init(|| {
        [std::env::temp_dir(), PathBuf::from("/tmp")]
            .into_iter()
            .flat_map(|d| [std::fs::canonicalize(&d).unwrap_or_else(|_| d.clone()), d])
            .map(|d| PathBuf::from(d.to_string_lossy().trim_end_matches('/')))
            .collect()
    });
    let cwd = Path::new(cwd.trim_end_matches('/'));
    temp.iter().any(|t| t == cwd)
}

/// **The one process-role classifier** (A11.2): `ProcInfo` carries no role
/// field; T02 and T09 (procscan, watchdog, health) call this and never
/// re-implement the rule. It reads the vendor, the teammate labels and the
/// redacted argv: the first word after the program that is not a path is the
/// subcommand (subcommand words are allowlisted labels and survive redaction;
/// `node`/`bun` scripts are paths and are skipped). Claude helper subcommands
/// ([`CLAUDE_HELPER_SUBCOMMANDS`]) are `VendorHelper` [F-47]; complete teammate
/// labels are `Teammate`; `tmux -L claude-swarm-<pid>` is `PaneServer`; other
/// vendor CLIs are `Session`; everything else is `Other`.
pub fn role_of(info: &ProcInfo) -> ProcRole {
    if info.labels.agent_id.is_some() {
        return ProcRole::Teammate;
    }
    let sub = info.argv_redacted.iter().skip(1).find(|a| !a.contains('/')).map(String::as_str);
    match info.vendor {
        Some(Vendor::Claude) if sub.is_some_and(|s| CLAUDE_HELPER_SUBCOMMANDS.contains(&s)) => ProcRole::VendorHelper,
        Some(_) => ProcRole::Session,
        None => {
            let tmux = info.exec_path.as_deref().is_some_and(|p| basename(p) == "tmux");
            if tmux && info.argv_redacted.iter().any(|a| a.starts_with("claude-swarm-")) {
                ProcRole::PaneServer
            } else {
                ProcRole::Other
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ancestry: owner app (§5.2.3) and origin inputs (§3.4.4)
// ---------------------------------------------------------------------------

/// Reads `CFBundleIdentifier` and a display name of an `.app` bundle.
pub trait BundleInfo: Send + Sync {
    fn bundle(&self, bundle_path: &str) -> Option<(String, String)>;
}

/// `NSBundle`, cached per bundle path. App bundles are not vendor dirs.
#[derive(Default)]
pub struct NsBundleInfo {
    cache: Mutex<HashMap<String, Option<(String, String)>>>,
}

impl BundleInfo for NsBundleInfo {
    fn bundle(&self, bundle_path: &str) -> Option<(String, String)> {
        if let Ok(c) = self.cache.lock() {
            if let Some(v) = c.get(bundle_path) {
                return v.clone();
            }
        }
        let v = read_ns_bundle(bundle_path);
        if let Ok(mut c) = self.cache.lock() {
            if c.len() > 256 {
                c.clear();
            }
            c.insert(bundle_path.to_string(), v.clone());
        }
        v
    }
}

fn read_ns_bundle(bundle_path: &str) -> Option<(String, String)> {
    use objc2_foundation::{NSBundle, NSString};
    let path = NSString::from_str(bundle_path);
    let bundle = NSBundle::bundleWithPath(&path)?;
    let id = bundle.bundleIdentifier()?.to_string();
    let name = ["CFBundleDisplayName", "CFBundleName"]
        .iter()
        .find_map(|k| {
            let key = NSString::from_str(k);
            let v = bundle.objectForInfoDictionaryKey(&key)?;
            let s = v.downcast::<NSString>().ok()?;
            Some(s.to_string())
        })
        .unwrap_or_else(|| {
            basename(bundle_path).trim_end_matches(".app").to_string()
        });
    Some((id, name))
}

/// The outermost `.app` bundle containing `exec_path` (`…/Visual Studio
/// Code.app/…/Code Helper (Plugin).app/…` → the VS Code bundle).
pub fn app_bundle_of(exec_path: &str) -> Option<&str> {
    let idx = exec_path.find(".app/")?;
    Some(&exec_path[..idx + 4])
}

/// The display name and kind of an owner app by bundle id (§5.2.3).
pub fn owner_kind(bundle_id: &str, display_name: &str) -> (String, OwnerKind) {
    match bundle_id {
        "com.apple.Terminal" => ("Terminal".into(), OwnerKind::Terminal),
        "com.googlecode.iterm2" => ("iTerm2".into(), OwnerKind::Terminal),
        "com.microsoft.VSCode" => ("VS Code".into(), OwnerKind::VsCode),
        _ => (display_name.chars().take(64).collect(), OwnerKind::Other),
    }
}

/// What the ancestor walk found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ancestry {
    pub owner: Option<OwnerApp>,
    /// `iTermServer*`, `Terminal` or `login` in the parent chain [R2 E3].
    pub terminal_ancestor: bool,
    /// `Code Helper (Plugin)` in the parent chain (non-Claude VS Code origin).
    pub vscode_plugin_ancestor: bool,
}

/// Walk `ppid` up to [`OWNER_WALK_MAX`] levels from `start_ppid`.
pub fn walk_ancestry(source: &dyn ProcSource, bundles: &dyn BundleInfo, start_ppid: i32) -> Ancestry {
    let mut out = Ancestry::default();
    let mut pid = start_ppid;
    let mut seen = HashSet::new();
    for _ in 0..OWNER_WALK_MAX {
        if pid <= 1 || !seen.insert(pid) {
            break;
        }
        let Some((ppid, comm)) = source.parent_of(pid) else { break };
        let path = source.exec_path(pid);
        let base = path.as_deref().map(basename).unwrap_or(comm.as_str()).to_string();
        if base.starts_with("iTermServer") || base == "Terminal" || base == "login" || comm == "login" {
            out.terminal_ancestor = true;
        }
        if base == "Code Helper (Plugin)" || comm.starts_with("Code Helper (Plugin") {
            out.vscode_plugin_ancestor = true;
        }
        if out.owner.is_none() {
            if let Some(bundle_path) = path.as_deref().and_then(app_bundle_of) {
                if let Some((id, name)) = bundles.bundle(bundle_path) {
                    let (app_name, kind) = owner_kind(&id, &name);
                    out.owner = Some(OwnerApp { app_pid: pid, app_name, kind, bundle_id: id });
                }
            }
        }
        pid = ppid;
    }
    out
}

/// Inputs of the Claude origin precedence (§3.4.4, F-42, N-05).
#[derive(Debug, Clone, Copy, Default)]
pub struct OriginInputs<'a> {
    pub teammate: bool,
    /// The member's `backendType` from the team roster.
    pub member_backend: Option<&'a str>,
    /// Carries `GILDE_APP_INSTANCE` or registry `entrypoint:"gilde"` and
    /// matches **no** own `proc_registry` row (own rows win, N-05).
    pub other_gilde: bool,
    pub kind_bg: bool,
    pub entrypoint: Option<&'a str>,
    pub cco_team_member: Option<bool>,
    pub terminal_ancestor: bool,
}

/// Claude origin precedence: teammate argv > another Gilde > `kind:"bg"` >
/// `claude-vscode` > `CCO_TEAM_MEMBER` > `sdk-cli` > `cli` + terminal ancestor
/// > Unknown.
pub fn claude_origin(i: OriginInputs<'_>) -> Origin {
    if i.teammate {
        return match i.member_backend {
            Some("iterm2") => Origin::ItermTeam,
            Some("tmux") => Origin::TmuxTeam,
            _ => Origin::TerminalTeam,
        };
    }
    if i.other_gilde {
        return Origin::OtherGilde;
    }
    if i.kind_bg {
        return Origin::Background;
    }
    match i.entrypoint {
        Some("claude-vscode") => return Origin::VsCode,
        _ if i.cco_team_member == Some(true) => return Origin::Orchestrator,
        Some("sdk-cli") => return Origin::Headless,
        Some("cli") | None if i.terminal_ancestor => return Origin::Terminal,
        _ => {}
    }
    Origin::Unknown
}

/// Non-Claude vendors: parent chain only (§3.4.4).
pub fn other_vendor_origin(a: &Ancestry, gilde_marker_without_row: bool) -> Origin {
    if gilde_marker_without_row {
        Origin::OtherGilde
    } else if a.vscode_plugin_ancestor {
        Origin::VsCode
    } else if a.terminal_ancestor {
        Origin::Terminal
    } else {
        Origin::Unknown
    }
}

/// Bundle ids of the OpenAI desktop app, which runs its own `codex app-server`.
pub const CHATGPT_APP_BUNDLES: &[&str] = &["com.openai.codex", "com.openai.chat"];

/// A codex `app-server`: the desktop app's own server when the owner app is
/// the OpenAI desktop app, else an editor extension's (VS Code and its forks).
pub fn app_server_origin(a: &Ancestry) -> Origin {
    let desktop = !a.vscode_plugin_ancestor
        && a.owner.as_ref().is_some_and(|o| CHATGPT_APP_BUNDLES.contains(&o.bundle_id.as_str()));
    if desktop {
        Origin::ChatGptApp
    } else {
        Origin::VsCode
    }
}

// ---------------------------------------------------------------------------
// the scanner
// ---------------------------------------------------------------------------

/// One process in the cache (parsed once per `(pid, start)`).
#[derive(Debug, Clone, PartialEq)]
pub struct ScannedProc {
    pub key: ProcessKey,
    pub pid: i32,
    pub ppid: i32,
    pub uid: u32,
    pub start_sec: i64,
    pub exec_path: Option<String>,
    pub comm: String,
    pub vendor: Option<Vendor>,
    pub role: Option<ProcRole>,
    pub candidate: bool,
    /// The §9.7 redacted argv (the only argv kept) [F-03].
    pub argv_redacted: Vec<String>,
    pub labels: ArgvLabels,
    pub cwd: Option<String>,
    pub cco_team_member: Option<bool>,
    pub gilde_app_instance: Option<bool>,
    /// Gilde's own process (any instance), from the PID registry.
    pub own: Option<OwnMatch>,
    pub ancestry: Option<Ancestry>,
    /// Uptime when first seen (the unregistered 10 s grace).
    pub first_seen_up_ms: u64,
    pub rss_bytes: u64,
    pub cpu_ns: u64,
    /// Last scan at which this process used ≥ 1 % of a core (§3.5.4 R8).
    pub cpu_active_at: Option<EpochMs>,
}

/// What one scan changed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScanOutcome {
    pub snapshot: ProcSnapshot,
    pub appeared: Vec<(i32, i64)>,
    pub vanished: Vec<(i32, i64)>,
}

/// The cache and scan loop. Not `Sync`: owned by the discovery thread.
pub struct Scanner {
    source: Arc<dyn ProcSource>,
    redact: Redactor,
    own: Arc<dyn OwnProcesses>,
    bundles: Arc<dyn BundleInfo>,
    cache: HashMap<(i32, i64), ScannedProc>,
    by_pid: HashMap<i32, i64>,
    /// When set, only these pids may be candidates (tests and benchmarks run
    /// against the real process table without reading other processes' argv).
    scope: Option<HashSet<i32>>,
    last_wall: Option<EpochMs>,
    my_uid: u32,
    /// A49.6: processes whose argv could not be read when they were
    /// classified (e.g. mid-exec), with the reads tried so far: re-inspected
    /// on the next scans instead of keeping the exec-path-only answer for
    /// their whole life.
    argv_retry: HashMap<(i32, i64), u8>,
}

/// A49.6: how many scans re-read an unreadable argv before the exec-path
/// classification is kept.
const ARGV_RETRIES: u8 = 5;

impl Scanner {
    pub fn new(
        source: Arc<dyn ProcSource>,
        redact: Redactor,
        own: Arc<dyn OwnProcesses>,
        bundles: Arc<dyn BundleInfo>,
    ) -> Scanner {
        let my_uid = source.my_uid();
        Scanner {
            source,
            redact,
            own,
            bundles,
            cache: HashMap::new(),
            by_pid: HashMap::new(),
            scope: None,
            last_wall: None,
            my_uid,
            argv_retry: HashMap::new(),
        }
    }

    /// Restrict candidates (argv reads) to `pids`.
    pub fn set_scope(&mut self, pids: Option<HashSet<i32>>) {
        self.scope = pids;
    }

    pub fn source(&self) -> &Arc<dyn ProcSource> {
        &self.source
    }

    pub fn get(&self, pid: i32, start_sec: i64) -> Option<&ScannedProc> {
        self.cache.get(&(pid, start_sec))
    }

    pub fn by_pid(&self, pid: i32) -> Option<&ScannedProc> {
        let start = self.by_pid.get(&pid)?;
        self.cache.get(&(pid, *start))
    }

    pub fn procs(&self) -> impl Iterator<Item = &ScannedProc> {
        self.cache.values()
    }

    pub fn len(&self) -> usize {
        self.cache.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }

    fn in_scope(&self, pid: i32) -> bool {
        self.scope.as_ref().is_none_or(|s| s.contains(&pid))
    }

    /// One `proc_listallpids` pass.
    pub fn scan(&mut self, now_wall: EpochMs, now_up: u64) -> ScanOutcome {
        let pids = self.source.list_pids();
        let mut seen: HashSet<(i32, i64)> = HashSet::with_capacity(pids.len());
        let mut out = ScanOutcome::default();
        let elapsed_ms = self.last_wall.map(|w| (now_wall - w).max(1) as u64);
        for pid in pids {
            let Some(bsd) = self.source.bsd_info(pid) else { continue };
            if bsd.uid != self.my_uid {
                continue; // another user's process: excluded, never signalled
            }
            let key = (pid, bsd.start_sec);
            seen.insert(key);
            let task = self.source.task_info(pid).unwrap_or_default();
            let retry = self.argv_retry.get(&key).is_some_and(|n| *n < ARGV_RETRIES);
            if let Some(p) = self.cache.get_mut(&key).filter(|_| !retry) {
                p.ppid = bsd.ppid;
                if let Some(ms) = elapsed_ms {
                    let delta = task.cpu_ns.saturating_sub(p.cpu_ns);
                    // ≥ 1 % of one core over the interval.
                    if delta.saturating_mul(100) >= ms.saturating_mul(1_000_000) {
                        p.cpu_active_at = Some(now_wall);
                    }
                }
                p.rss_bytes = task.rss_bytes;
                p.cpu_ns = task.cpu_ns;
                if p.candidate && p.own.is_none() {
                    p.own = self.own.lookup(pid, bsd.start_sec);
                }
                continue;
            }
            let (mut proc, argv_unread) = self.inspect(&bsd, task, now_up);
            match self.cache.get(&key) {
                // A49.6: a re-inspection keeps what the first sighting knew.
                Some(prev) => {
                    proc.first_seen_up_ms = prev.first_seen_up_ms;
                    proc.cpu_active_at = prev.cpu_active_at;
                }
                None => out.appeared.push(key),
            }
            if argv_unread {
                *self.argv_retry.entry(key).or_insert(0) += 1;
            } else {
                self.argv_retry.remove(&key);
            }
            self.by_pid.insert(pid, bsd.start_sec);
            self.cache.insert(key, proc);
        }
        let gone: Vec<(i32, i64)> = self.cache.keys().filter(|k| !seen.contains(k)).copied().collect();
        for k in gone {
            self.argv_retry.remove(&k);
            self.cache.remove(&k);
            if self.by_pid.get(&k.0) == Some(&k.1) {
                self.by_pid.remove(&k.0);
            }
            out.vanished.push(k);
        }
        self.last_wall = Some(now_wall);
        out.snapshot = self.snapshot(now_wall);
        out
    }

    /// Classify one process; the flag says its argv was needed but could not
    /// be read (A49.6: the scanner retries it).
    fn inspect(&self, bsd: &BsdInfo, task: TaskInfo, now_up: u64) -> (ScannedProc, bool) {
        let pid = bsd.pid;
        let exec_path = self.source.exec_path(pid);
        let mut proc = ScannedProc {
            key: ProcessKey::new(pid as u32, bsd.start_sec.max(0) as u64),
            pid,
            ppid: bsd.ppid,
            uid: bsd.uid,
            start_sec: bsd.start_sec,
            exec_path: exec_path.clone(),
            comm: bsd.comm.clone(),
            vendor: None,
            role: None,
            candidate: false,
            argv_redacted: Vec::new(),
            labels: ArgvLabels::default(),
            cwd: None,
            cco_team_member: None,
            gilde_app_instance: None,
            own: None,
            ancestry: None,
            first_seen_up_ms: now_up,
            rss_bytes: task.rss_bytes,
            cpu_ns: task.cpu_ns,
            cpu_active_at: None,
        };
        let Some(path) = exec_path else { return (proc, false) };
        let by_path = classify(&path, None);
        let maybe = by_path.candidate || is_js_runtime(&path);
        if !maybe || !self.in_scope(pid) {
            return (proc, false);
        }
        let raw = self.source.args(pid);
        let argv_unread = raw.as_ref().and_then(|r| r.argv.as_ref()).is_none();
        let classified = match raw.as_ref().and_then(|r| r.argv.as_deref()) {
            Some(argv) => classify(&path, Some(argv)),
            None => by_path,
        };
        if !classified.candidate {
            return (proc, argv_unread); // a node/bun that is not a vendor CLI
        }
        proc.cwd = self.source.cwd(pid);
        proc.vendor = classified.vendor;
        proc.role = classified.role;
        proc.candidate = true;
        proc.labels = classified.labels;
        proc.own = self.own.lookup(pid, bsd.start_sec);
        if let Some(raw) = raw.as_ref() {
            proc.cco_team_member = raw.cco_team_member;
            proc.gilde_app_instance = raw.gilde_app_instance;
            if let Some(argv) = raw.argv.as_deref() {
                if proc.own.is_none() {
                    if let Some(spawn_id) = self.own.match_planned(argv, proc.cwd.as_deref()) {
                        proc.own = Some(OwnMatch { spawn_id, this_instance: true });
                    }
                }
                proc.argv_redacted = (self.redact)(argv);
            }
        }
        // `raw` (the only copy of the raw argv) is zeroed when it drops here.
        drop(raw);
        proc.ancestry = Some(walk_ancestry(self.source.as_ref(), self.bundles.as_ref(), bsd.ppid));
        (proc, argv_unread)
    }

    /// Re-run the own-row lookup for every candidate (after a registry change).
    pub fn refresh_own(&mut self) {
        for p in self.cache.values_mut() {
            if p.candidate && p.own.is_none() {
                p.own = self.own.lookup(p.pid, p.start_sec);
            }
        }
    }

    /// The published snapshot (T09's procscan/watchdog/health input): every
    /// process of this user; argv only in the redacted form.
    pub fn snapshot(&self, taken_at: EpochMs) -> ProcSnapshot {
        let mut procs: Vec<ProcInfo> = self
            .cache
            .values()
            .map(|p| ProcInfo {
                key: p.key.clone(),
                pid: p.pid,
                ppid: p.ppid,
                uid: p.uid,
                start_sec: p.start_sec,
                exec_path: p.exec_path.clone(),
                argv_redacted: p.argv_redacted.clone(),
                labels: p.labels.teammate.clone().unwrap_or_default(),
                cwd: p.cwd.clone(),
                rss_bytes: p.rss_bytes,
                cpu_ns: p.cpu_ns,
                cco_team_member: p.cco_team_member,
                gilde_app_instance: p.gilde_app_instance,
                vendor: p.vendor,
            })
            .collect();
        procs.sort_by_key(|p| p.pid);
        ProcSnapshot { taken_at, procs }
    }
}

/// A fixed-table [`ProcSource`] for tests (and the showcase).
#[derive(Default)]
pub struct FakeProcs {
    pub table: Mutex<HashMap<i32, FakeProc>>,
    pub uid: u32,
}

/// One fake process. `argv` is handed out as [`RawArgs`] like the kernel would.
#[derive(Debug, Clone, Default)]
pub struct FakeProc {
    pub ppid: i32,
    pub uid: u32,
    pub start_sec: i64,
    pub comm: String,
    pub exec_path: Option<String>,
    pub argv: Option<Vec<String>>,
    pub env_names: Option<Vec<String>>,
    pub cwd: Option<String>,
    pub rss_bytes: u64,
    pub cpu_ns: u64,
    pub other_user: bool,
}

impl FakeProcs {
    pub fn new(uid: u32) -> FakeProcs {
        FakeProcs { table: Mutex::new(HashMap::new()), uid }
    }

    pub fn insert(&self, pid: i32, p: FakeProc) {
        self.table.lock().expect("fake table").insert(pid, p);
    }

    pub fn remove(&self, pid: i32) {
        self.table.lock().expect("fake table").remove(&pid);
    }

    fn with<T>(&self, pid: i32, f: impl FnOnce(&FakeProc) -> T) -> Option<T> {
        self.table.lock().ok()?.get(&pid).map(f)
    }
}

impl ProcSource for FakeProcs {
    fn list_pids(&self) -> Vec<i32> {
        let mut v: Vec<i32> = self.table.lock().map(|t| t.keys().copied().collect()).unwrap_or_default();
        v.sort_unstable();
        v
    }
    /// Refused for another user's process, as the kernel refuses
    /// `PROC_PIDTBSDINFO` to anyone but root.
    fn bsd_info(&self, pid: i32) -> Option<BsdInfo> {
        self.with(pid, |p| BsdInfo {
            pid,
            ppid: p.ppid,
            uid: p.uid,
            start_sec: p.start_sec,
            comm: p.comm.clone(),
        })
        .filter(|b| self.uid == 0 || b.uid == self.uid)
    }
    fn parent_of(&self, pid: i32) -> Option<(i32, String)> {
        self.with(pid, |p| (p.ppid, p.comm.clone()))
    }
    fn exec_path(&self, pid: i32) -> Option<String> {
        self.with(pid, |p| p.exec_path.clone()).flatten()
    }
    fn args(&self, pid: i32) -> Option<RawArgs> {
        self.with(pid, |p| {
            let names = p.env_names.clone();
            RawArgs {
                exec_path: p.exec_path.clone(),
                argv: p.argv.clone(),
                cco_team_member: names.as_ref().map(|n| n.iter().any(|x| x == "CCO_TEAM_MEMBER")),
                gilde_app_instance: names.as_ref().map(|n| n.iter().any(|x| x == "GILDE_APP_INSTANCE")),
            }
        })
    }
    fn cwd(&self, pid: i32) -> Option<String> {
        self.with(pid, |p| p.cwd.clone()).flatten()
    }
    fn task_info(&self, pid: i32) -> Option<TaskInfo> {
        self.with(pid, |p| TaskInfo { rss_bytes: p.rss_bytes, cpu_ns: p.cpu_ns })
    }
    fn alive(&self, pid: i32) -> Alive {
        match self.with(pid, |p| p.other_user) {
            Some(true) => Alive::OtherUser,
            Some(false) => Alive::Yes,
            None => Alive::No,
        }
    }
    fn my_uid(&self) -> u32 {
        self.uid
    }
}

/// A bundle reader with a fixed table (tests).
#[derive(Default)]
pub struct FakeBundles(pub HashMap<String, (String, String)>);

impl BundleInfo for FakeBundles {
    fn bundle(&self, bundle_path: &str) -> Option<(String, String)> {
        self.0.get(bundle_path).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn a_print_run_in_the_temp_folder_is_a_probe_and_nothing_else_is() {
        let temp = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let temp = temp.to_string_lossy();
        let probe = crate::supervise::redact::redact_argv(&[
            "/opt/homebrew/bin/claude",
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
        ]);
        assert!(is_temp_dir_print_probe(&probe, Some(&temp)), "the usage probe in {temp}");
        assert!(is_temp_dir_print_probe(&probe, Some(&format!("{temp}/"))));
        assert!(is_temp_dir_print_probe(&probe, Some("/private/tmp")), "/tmp, resolved");
        assert!(!is_temp_dir_print_probe(&probe, Some("/Users/u/project")), "a print run in a project is a session");
        assert!(!is_temp_dir_print_probe(&probe, Some(&format!("{temp}/work"))), "only the folder itself");
        assert!(!is_temp_dir_print_probe(&probe, None));
        let interactive = s(&["/opt/homebrew/bin/claude", "--resume", "x"]);
        assert!(!is_temp_dir_print_probe(&interactive, Some(&temp)), "an interactive session in the temp folder stays one");
    }

    fn procargs(argv: &[&str], env: &[&str]) -> Vec<u8> {
        let mut b = (argv.len() as i32).to_ne_bytes().to_vec();
        b.extend_from_slice(b"/usr/local/bin/claude\0\0\0\0");
        for a in argv {
            b.extend_from_slice(a.as_bytes());
            b.push(0);
        }
        for e in env {
            b.extend_from_slice(e.as_bytes());
            b.push(0);
        }
        b.push(0);
        b.extend_from_slice(b"ptr_munge=xyz\0");
        b
    }

    #[test]
    fn procargs2_reads_argc_strings_and_env_names_only() {
        let buf = procargs(
            &["claude", "--model", "opus"],
            &["PATH=/bin", "CCO_TEAM_MEMBER=worker-1", "AWS_SECRET_ACCESS_KEY=zzz"],
        );
        let r = parse_procargs2(&buf).unwrap();
        assert_eq!(r.argv.as_deref().unwrap(), &s(&["claude", "--model", "opus"])[..]);
        assert_eq!(r.cco_team_member, Some(true));
        assert_eq!(r.gilde_app_instance, Some(false));
        assert_eq!(r.exec_path.as_deref(), Some("/usr/local/bin/claude"));
        let dbg = format!("{r:?}");
        assert!(!dbg.contains("zzz") && !dbg.contains("worker-1") && !dbg.contains("opus"));
    }

    #[test]
    fn procargs2_invalid_utf8_argv_and_missing_env() {
        let mut buf = (2i32).to_ne_bytes().to_vec();
        buf.extend_from_slice(b"/x/claude\0claude\0\xff\xfe\0");
        let r = parse_procargs2(&buf).unwrap();
        assert!(r.argv.is_none(), "invalid UTF-8 → exec path only");
        assert_eq!(r.cco_team_member, None, "no env block → unknown");
        assert!(parse_procargs2(&[1, 2]).is_none());
        assert!(parse_procargs2(&(-1i32).to_ne_bytes()).is_none());
    }

    #[test]
    fn bg_helpers_are_vendor_helpers_never_sessions() {
        for argv in [
            s(&["claude", "daemon", "run", "--origin", "transient", "--spawned-by", "{}"]),
            s(&["claude", "bg-pty-host", "--bg-pty-host", "/tmp/cc-daemon-501/h/spare/x.pty.sock", "200", "50", "--"]),
            s(&["claude", "agents", "--json"]),
            s(&["claude", "stop", "04b2b152"]),
            s(&["claude", "logs", "04b2b152"]),
            s(&["claude", "rm", "04b2b152"]),
            s(&["claude", "--version"]),
            s(&["claude", "-v"]),
        ] {
            let c = classify("/opt/claude/bin/claude.exe", Some(&argv));
            assert_eq!(c.role, Some(ProcRole::VendorHelper), "{argv:?}");
            assert_eq!(c.vendor, Some(Vendor::Claude));
        }
        for argv in [
            s(&["claude"]),
            s(&["claude", "-p", "--output-format", "stream-json"]),
            s(&["claude", "--resume", "0e2c0b1a-6b7d-4c51-9a8e-1f2e3d4c5b6a"]),
            s(&["claude", "--session-id", "04b2b152-0000-4000-8000-000000000000", "--model", "haiku"]),
            s(&["claude", "attach", "04b2b152"]),
            s(&["claude", "fix the flaky test"]),
        ] {
            assert_eq!(classify("/x/claude", Some(&argv)).role, Some(ProcRole::Session), "{argv:?}");
        }
    }

    #[test]
    fn teammate_argv_needs_all_three_flags_anywhere() {
        let full = s(&[
            "claude.exe", "--model", "haiku", "--team-name", "session-72cb5f46", "--agent-color=blue",
            "--agent-id", "alpha@session-72cb5f46", "--parent-session-id",
            "c3c103c3-0000-4000-8000-000000000001", "--agent-name", "alpha",
        ]);
        let c = classify("/x/claude.exe", Some(&full));
        assert_eq!(c.role, Some(ProcRole::Teammate));
        let t = c.labels.teammate.unwrap();
        assert_eq!(t.agent_name.as_deref(), Some("alpha"));
        assert_eq!(t.agent_color.as_deref(), Some("blue"));
        assert_eq!(t.parent_session_id.as_deref(), Some("c3c103c3-0000-4000-8000-000000000001"));
        let missing = s(&["claude", "--agent-id", "a@t", "--agent-name", "a"]);
        assert_eq!(classify("/x/claude", Some(&missing)).role, Some(ProcRole::Session));
        assert!(classify("/x/claude", Some(&missing)).labels.teammate.is_none());
    }

    #[test]
    fn classifier_covers_every_vendor_and_runtime() {
        assert_eq!(vendor_of_exec("/a/native-binary/claude"), Some(Vendor::Claude));
        assert_eq!(vendor_of_exec("/a/codex"), Some(Vendor::Codex));
        assert_eq!(vendor_of_exec("/a/cursor-agent"), Some(Vendor::Cursor));
        assert_eq!(vendor_of_exec("/Users/u/.local/share/cursor-agent/versions/1/agent"), Some(Vendor::Cursor));
        assert_eq!(vendor_of_exec("/usr/local/bin/agent"), None);
        assert_eq!(vendor_of_exec("/a/agy"), Some(Vendor::Agy));
        let node = s(&["node", "/u/lib/node_modules/@anthropic-ai/claude-code/cli.js", "--resume", "0e2c0b1a-6b7d-4c51-9a8e-1f2e3d4c5b6a"]);
        let c = classify("/opt/node/bin/node", Some(&node));
        assert_eq!(c.vendor, Some(Vendor::Claude));
        assert_eq!(c.labels.resume.as_deref(), Some("0e2c0b1a-6b7d-4c51-9a8e-1f2e3d4c5b6a"));
        assert!(!classify("/opt/node/bin/node", Some(&s(&["node", "/srv/app.js"]))).candidate);
        let tmux = classify("/opt/homebrew/bin/tmux", Some(&s(&["tmux", "-L", "claude-swarm-4242", "list-panes"])));
        assert_eq!(tmux.role, Some(ProcRole::PaneServer));
        assert_eq!(tmux.labels.swarm_lead_pid, Some(4242));
        let codex = classify("/x/codex", Some(&s(&["codex", "resume", "0e2c0b1a-6b7d-4c51-9a8e-1f2e3d4c5b6a"])));
        assert_eq!(codex.labels.resume.as_deref(), Some("0e2c0b1a-6b7d-4c51-9a8e-1f2e3d4c5b6a"));
        let app = classify("/x/codex", Some(&s(&["codex", "-c", "features.code_mode_host=true", "app-server"])));
        assert!(app.labels.app_server);
        let agy = classify("/x/agy", Some(&s(&["agy", "--conversation", "0e2c0b1a-6b7d-4c51-9a8e-1f2e3d4c5b6a"])));
        assert_eq!(agy.labels.conversation.as_deref(), Some("0e2c0b1a-6b7d-4c51-9a8e-1f2e3d4c5b6a"));
        let cur = classify("/x/cursor-agent", Some(&s(&["cursor-agent", "--resume", "1f2e3d4c-aaaa"])));
        assert_eq!(cur.labels.resume.as_deref(), Some("1f2e3d4c-aaaa"));
        // a positional prompt is never a label
        let p = classify("/x/claude", Some(&s(&["claude", "--model", "opus", "summarize secrets"])));
        assert_eq!(p.labels.subcommand, None);
    }

    #[test]
    fn origin_precedence_table() {
        let base = OriginInputs::default();
        assert_eq!(claude_origin(OriginInputs { teammate: true, member_backend: Some("iterm2"), kind_bg: true, ..base }), Origin::ItermTeam);
        assert_eq!(claude_origin(OriginInputs { teammate: true, member_backend: Some("tmux"), ..base }), Origin::TmuxTeam);
        assert_eq!(claude_origin(OriginInputs { teammate: true, ..base }), Origin::TerminalTeam);
        assert_eq!(claude_origin(OriginInputs { other_gilde: true, kind_bg: true, ..base }), Origin::OtherGilde);
        assert_eq!(claude_origin(OriginInputs { kind_bg: true, entrypoint: Some("claude-vscode"), ..base }), Origin::Background);
        assert_eq!(claude_origin(OriginInputs { entrypoint: Some("claude-vscode"), cco_team_member: Some(true), ..base }), Origin::VsCode);
        assert_eq!(claude_origin(OriginInputs { entrypoint: Some("sdk-cli"), cco_team_member: Some(true), ..base }), Origin::Orchestrator);
        assert_eq!(claude_origin(OriginInputs { entrypoint: Some("sdk-cli"), terminal_ancestor: true, ..base }), Origin::Headless);
        assert_eq!(claude_origin(OriginInputs { entrypoint: Some("cli"), terminal_ancestor: true, ..base }), Origin::Terminal);
        assert_eq!(claude_origin(OriginInputs { entrypoint: Some("cli"), ..base }), Origin::Unknown);
        assert_eq!(claude_origin(OriginInputs { cco_team_member: None, entrypoint: Some("gilde"), ..base }), Origin::Unknown);
    }

    #[test]
    fn owner_app_is_the_outermost_bundle_of_the_first_app_ancestor() {
        let fake = FakeProcs::new(501);
        fake.insert(10, FakeProc { ppid: 1, uid: 501, start_sec: 1, comm: "Code".into(), exec_path: Some("/Applications/Visual Studio Code.app/Contents/MacOS/Electron".into()), ..Default::default() });
        fake.insert(11, FakeProc { ppid: 10, uid: 501, start_sec: 2, comm: "Code Helper (Plugin)".into(), exec_path: Some("/Applications/Visual Studio Code.app/Contents/Frameworks/Code Helper (Plugin).app/Contents/MacOS/Code Helper (Plugin)".into()), ..Default::default() });
        let mut b = FakeBundles::default();
        b.0.insert("/Applications/Visual Studio Code.app".into(), ("com.microsoft.VSCode".into(), "Code".into()));
        let a = walk_ancestry(&fake, &b, 11);
        let owner = a.owner.unwrap();
        assert_eq!(owner.app_name, "VS Code");
        assert_eq!(owner.kind, OwnerKind::VsCode);
        assert_eq!(owner.app_pid, 11);
        assert!(a.vscode_plugin_ancestor);
        assert_eq!(owner_kind("com.example.x", "Example").0, "Example");
        assert_eq!(app_bundle_of("/A/B.app/Contents/MacOS/b"), Some("/A/B.app"));
    }

    #[test]
    fn the_walk_passes_the_root_owned_login_between_terminal_and_the_shell() {
        let fake = FakeProcs::new(501);
        fake.insert(10, FakeProc { ppid: 1, uid: 501, start_sec: 1, comm: "Terminal".into(), exec_path: Some("/System/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal".into()), ..Default::default() });
        fake.insert(11, FakeProc { ppid: 10, uid: 0, start_sec: 2, comm: "login".into(), exec_path: Some("/usr/bin/login".into()), ..Default::default() });
        fake.insert(12, FakeProc { ppid: 11, uid: 501, start_sec: 3, comm: "zsh".into(), exec_path: Some("/bin/zsh".into()), ..Default::default() });
        assert_eq!(fake.bsd_info(11), None, "PROC_PIDTBSDINFO is refused for root's login");
        let mut b = FakeBundles::default();
        b.0.insert("/System/Applications/Utilities/Terminal.app".into(), ("com.apple.Terminal".into(), "Terminal".into()));
        let a = walk_ancestry(&fake, &b, 12);
        assert!(a.terminal_ancestor);
        assert_eq!(a.owner.map(|o| o.app_name).as_deref(), Some("Terminal"), "the walk goes on past login");
        assert_eq!(other_vendor_origin(&walk_ancestry(&fake, &b, 12), false), Origin::Terminal);
    }

    #[test]
    fn an_app_server_is_the_desktop_apps_only_under_the_openai_app() {
        let owner = |bundle: &str| Ancestry {
            owner: Some(OwnerApp { app_pid: 9, app_name: "x".into(), kind: OwnerKind::Other, bundle_id: bundle.into() }),
            ..Ancestry::default()
        };
        assert_eq!(app_server_origin(&owner("com.openai.codex")), Origin::ChatGptApp);
        assert_eq!(app_server_origin(&owner("com.openai.chat")), Origin::ChatGptApp);
        let vscode = Ancestry { vscode_plugin_ancestor: true, ..owner("com.microsoft.VSCode") };
        assert_eq!(app_server_origin(&vscode), Origin::VsCode);
        assert_eq!(app_server_origin(&owner("com.todesktop.cursor")), Origin::VsCode, "another editor's extension");
        assert_eq!(app_server_origin(&Ancestry::default()), Origin::VsCode);
    }
}
