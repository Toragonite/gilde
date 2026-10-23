//! The memory watchdog — DESIGN §8-8, M1-PLAN T12.
//!
//! One 5 second task, independent of the Process Monitor's own polling. The
//! panel may be closed, hidden or blurred — the budget must still be watched,
//! because the failure this whole feature exists to prevent (a machine paged to
//! its knees by four agents nobody is looking at) happens precisely when nobody
//! is looking.
//!
//! Each tick does four things, in this order:
//!
//! 1. **scan** — the shared process scan (discovery's, through
//!    [`ScanHub`], A12.2; the in-process kernel scan only while discovery is
//!    not running), classified against the live registry and the
//!    hot-reloaded signature file, and cached so that `procmon_scan` can
//!    answer from it instead of scanning again;
//! 2. **attribute** — every live registry row that belongs to a session gets the
//!    RSS of its whole **pgid subtree** summed onto it, which is the only number
//!    that means anything for an agent CLI: the CLI itself is a few hundred MB
//!    and its `Bash` tool's `cargo build` is the six gigabytes;
//! 3. **record** — those numbers go to `proc_sample`;
//! 4. **judge** — [`BudgetTracker`] turns the sample into alarms, and only then
//!    does anything user-visible happen.
//!
//! ## What this module deliberately does not do
//!
//! **Memory pressure never kills anything.** [`memory_pressure`] is reported —
//! in the footer, in `app_diagnostics()` — and it ranks the suggestions the
//! monitor makes, but no branch here reads it and then signals. macOS reports
//! `warn` on a perfectly healthy laptop with a big page cache; a watchdog that
//! killed on it would kill constantly and be turned off within a day.
//!
//! **The hard ceiling does not kill by default.** `autoKillOverHardCeiling` is
//! `false` (DECISIONS §M1 세부 기본값: "하드 RSS kill 기본 off(경고만)"), so
//! crossing `hardRssMiB` produces an alert and nothing else. When the user turns
//! it on, the kill is armed behind [`reaper::UNDO_WINDOW`] and the alert carries
//! the token, so an undo genuinely prevents a signal rather than apologising for
//! one. With the feature off there is no token, because a token that cancels
//! nothing is theatre.
//!
//! **A soft alert is not a stream.** A 6 GB Rust build inside an agent's `Bash`
//! tool is legitimate work; it crosses `softRssMiB` on the first tick and stays
//! there for ten minutes. Two gates make that exactly one toast: hysteresis (the
//! session must fall to 90 % of the threshold before the alarm re-arms) and a
//! one-per-hour rate limit, per session, conjunctively.

use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use tauri::async_runtime::JoinHandle;

use crate::agents::AgentRegistry;
use crate::config::model::SupervisionConfig;
use crate::error::{GildeError, Result};
use crate::events::Events;
use crate::events::{
    now_ms, BudgetAlertEvent, BudgetLevel, ProcScanEvent, ToastEvent, ToastLevel, BUDGET_ALERT,
    HEALTH, PROC_SCAN, TOAST,
};
use crate::store::dao;
use crate::store::models::ProcRow as RegistryRow;
use crate::supervise::kill::KillOpts;
use crate::supervise::procscan::{self, cwd_of, ps_snapshot, RowContext, ScanHub, Snapshot};
use crate::supervise::PressureLevel;
use crate::supervise::health::{
    HealthBuilder, HealthConfig, HealthInput, HealthSnapshot, LiveProbe, TeamFacts, TeamSource,
};
use crate::supervise::reaper;
use crate::supervise::registry::Supervisor;
use crate::supervise::{ProcGroupKind, ProcRow, ProcTotals, RegistryIndex, ScanFilter, ScanResult};

// ---------------------------------------------------------------------------
// constants
// ---------------------------------------------------------------------------

/// The watchdog's own cadence (DESIGN §8-8: "5초 주기(스캐너와 독립)").
pub const TICK: Duration = Duration::from_secs(5);

/// The oldest shared scan the watchdog reads (discovery scans every 3 s while
/// frontmost and every 15 s otherwise); older → the kernel fallback.
pub const SHARED_SCAN_MAX_AGE_MS: i64 = 20_000;

/// [`Watchdog::on_memory_pressure`]'s listener.
pub type MemoryListener = Arc<dyn Fn(bool) + Send + Sync>;

/// How often the kernel is asked about system-wide memory pressure. Slower than
/// the tick because the answer moves slowly and each probe is a syscall.
pub const PRESSURE_INTERVAL: Duration = Duration::from_secs(15);

/// At most one soft-budget alert per session per hour (DESIGN §8-8).
pub const SOFT_ALERT_INTERVAL: Duration = Duration::from_secs(3600);

/// A tripped alarm re-arms only once the value falls to this fraction of its
/// threshold — DESIGN §8-8's "히스테리시스(10% 아래로 떨어져야 재무장)".
pub const REARM_RATIO: f64 = 0.90;

/// Upper bound on how many rows one `procmon_scan` will resolve a `cwd` for.
/// `cwd` is a syscall per pid and this machine can have 800 processes, so it is
/// resolved lazily and only for rows a filter already narrowed down.
pub const CWD_RESOLVE_LIMIT: usize = 200;

const MIB: u64 = 1024 * 1024;

/// Toast keys this module emits. Mirrored in `src/i18n/{ko,en}.ts`.
pub mod toast_key {
    pub const SOFT: &str = "toast.budget.soft";
    pub const HARD: &str = "toast.budget.hard";
    pub const HARD_AUTOKILL: &str = "toast.budget.hardAutoKill";
    pub const IDLE: &str = "toast.budget.idle";
    pub const SYSTEM: &str = "toast.budget.system";
}

// ---------------------------------------------------------------------------
// memory pressure
// ---------------------------------------------------------------------------

/// Which probe actually answered. Reported by `app_diagnostics()` (DESIGN
/// §10-6) and by [`BudgetState`]; the serde spellings are the sysctl names
/// themselves, exactly as `ipc.ts::MemoryPressureSource` declares them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PressureSource {
    #[serde(rename = "kern.memorystatus_vm_pressure_level")]
    KernMemorystatus,
    #[serde(rename = "vm.memory_pressure")]
    VmMemoryPressure,
    #[serde(rename = "vm_stat")]
    VmStat,
    #[default]
    #[serde(rename = "unavailable")]
    Unavailable,
}

impl PressureSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            PressureSource::KernMemorystatus => "kern.memorystatus_vm_pressure_level",
            PressureSource::VmMemoryPressure => "vm.memory_pressure",
            PressureSource::VmStat => "vm_stat",
            PressureSource::Unavailable => "unavailable",
        }
    }
}

/// System memory pressure, and the probe that produced it.
///
/// The order is DESIGN §8-8's: `kern.memorystatus_vm_pressure_level` first
/// (1 normal / 2 warn / 4 critical — measured to return `2` on this machine),
/// then the legacy `vm.memory_pressure`, then the pageout rate. **The answer is
/// never a reason to signal anything**; it ranks suggestions and it is displayed.
pub fn memory_pressure() -> (PressureLevel, PressureSource) {
    if let Some(raw) = sysctl_int("kern.memorystatus_vm_pressure_level") {
        if let Some(level) = memorystatus_level(raw) {
            return (level, PressureSource::KernMemorystatus);
        }
    }
    if let Some(raw) = sysctl_int("vm.memory_pressure") {
        return (legacy_pressure_level(raw), PressureSource::VmMemoryPressure);
    }
    match pageout_pressure() {
        Some(level) => (level, PressureSource::VmStat),
        None => (PressureLevel::Unknown, PressureSource::Unavailable),
    }
}

/// `kern.memorystatus_vm_pressure_level`: the documented constants are 1, 2 and
/// 4. Anything else is a kernel this build does not understand, which falls
/// through to the next probe rather than being guessed at.
fn memorystatus_level(raw: i64) -> Option<PressureLevel> {
    match raw {
        1 => Some(PressureLevel::Normal),
        2 => Some(PressureLevel::Warn),
        4 => Some(PressureLevel::Critical),
        _ => None,
    }
}

/// `vm.memory_pressure` is undocumented and reads `0` on an idle machine. It is
/// treated as "percent of memory under pressure": zero is normal, anything at
/// all is a warning, half the machine is critical.
fn legacy_pressure_level(raw: i64) -> PressureLevel {
    match raw {
        i64::MIN..=0 => PressureLevel::Normal,
        1..=49 => PressureLevel::Warn,
        _ => PressureLevel::Critical,
    }
}

/// Read an integer sysctl by name. Returns `None` when the key does not exist,
/// is not readable, or is not a 4/8 byte integer.
fn sysctl_int(name: &str) -> Option<i64> {
    let cname = CString::new(name).ok()?;
    let mut len: libc::size_t = 0;
    // First call with a null buffer: ask how big the value is.
    let rc = unsafe {
        libc::sysctlbyname(
            cname.as_ptr(),
            std::ptr::null_mut(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || (len != 4 && len != 8) {
        return None;
    }
    let mut buf = [0u8; 8];
    let mut got = len;
    let rc = unsafe {
        libc::sysctlbyname(
            cname.as_ptr(),
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut got,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    match got {
        4 => Some(i32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]) as i64),
        8 => Some(i64::from_ne_bytes(buf)),
        _ => None,
    }
}

/// The host port. Acquired once: `mach_host_self()` hands out a send right on
/// every call and `libc` exposes no `mach_port_deallocate` to give them back, so
/// a per-tick call would leak one port name every five seconds.
fn host_port() -> libc::mach_port_t {
    static PORT: OnceLock<libc::mach_port_t> = OnceLock::new();
    // `libc` deprecates this in favour of the `mach2` crate, which is not on
    // T00's frozen dependency list. The symbol is the one `vm_stat` itself uses
    // and it is not going anywhere; the allow is narrower than a new crate.
    #[allow(deprecated)]
    {
        *PORT.get_or_init(|| unsafe { libc::mach_host_self() })
    }
}

/// Cumulative pageout count — the same counter `vm_stat` prints as "Pageouts".
///
/// Read through `host_statistics64(HOST_VM_INFO64)` rather than by running
/// `/usr/bin/vm_stat` and parsing it: identical numbers, one syscall, no child
/// process on a five-second timer.
fn vm_pageouts() -> Option<u64> {
    let mut info = std::mem::MaybeUninit::<libc::vm_statistics64>::zeroed();
    let mut count = libc::HOST_VM_INFO64_COUNT;
    let rc = unsafe {
        libc::host_statistics64(
            host_port(),
            libc::HOST_VM_INFO64,
            info.as_mut_ptr() as libc::host_info64_t,
            &mut count,
        )
    };
    if rc != libc::KERN_SUCCESS {
        return None;
    }
    Some(unsafe { info.assume_init() }.pageouts)
}

struct PageoutMark {
    at: Instant,
    pageouts: u64,
}

fn pageout_mark() -> &'static Mutex<Option<PageoutMark>> {
    static MARK: OnceLock<Mutex<Option<PageoutMark>>> = OnceLock::new();
    MARK.get_or_init(|| Mutex::new(None))
}

/// Sustained pageout **rate**, which is a far better signal of real pressure
/// than an instantaneous free-page count (a healthy machine keeps very few free
/// pages on purpose). The first call has no delta to compare against and
/// therefore reports [`PressureLevel::Unknown`] rather than inventing one.
fn pageout_pressure() -> Option<PressureLevel> {
    let pageouts = vm_pageouts()?;
    let now = Instant::now();
    let mut mark = pageout_mark().lock();
    let previous = mark.replace(PageoutMark { at: now, pageouts });
    let Some(previous) = previous else {
        return Some(PressureLevel::Unknown);
    };
    let seconds = now.duration_since(previous.at).as_secs_f64();
    if seconds <= 0.0 {
        return Some(PressureLevel::Unknown);
    }
    let per_second = pageouts.saturating_sub(previous.pageouts) as f64 / seconds;
    Some(match per_second {
        r if r >= 1000.0 => PressureLevel::Critical,
        r if r >= 100.0 => PressureLevel::Warn,
        _ => PressureLevel::Normal,
    })
}

/// Forget the pageout baseline (a wake, and tests, which share one process).
fn clear_pageout_mark() {
    *pageout_mark().lock() = None;
}

#[cfg(test)]
fn reset_pageout_mark() {
    clear_pageout_mark();
}

// ---------------------------------------------------------------------------
// wire types
// ---------------------------------------------------------------------------

/// `budget_get()` — mirrored by `ipc.ts::BudgetState`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetState {
    /// Our sessions plus the residue we detected, summed.
    pub rss_bytes: u64,
    pub physical_ram_bytes: u64,
    /// `rss_bytes` as a percentage of physical RAM.
    pub percent: f32,
    pub warn_pct: u8,
    pub critical_pct: u8,
    pub pressure_level: PressureLevel,
    pub pressure_source: PressureSource,
}

impl Default for BudgetState {
    fn default() -> Self {
        let defaults = SupervisionConfig::default();
        BudgetState {
            rss_bytes: 0,
            physical_ram_bytes: 0,
            percent: 0.0,
            warn_pct: defaults.global_warn_pct,
            critical_pct: defaults.global_critical_pct,
            pressure_level: PressureLevel::Unknown,
            pressure_source: PressureSource::Unavailable,
        }
    }
}

// ---------------------------------------------------------------------------
// the sample
// ---------------------------------------------------------------------------

/// One session's share of the machine, as one tick saw it.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSample {
    pub session_id: String,
    pub spawn_id: String,
    pub vendor: Option<String>,
    /// Summed over the pgid subtree, not just the CLI process.
    pub rss_bytes: u64,
    pub cpu_pct: f32,
    /// Written by the PTY reader threads and the sidecar (T16). `None` means the
    /// session has produced no I/O at all yet.
    pub last_io_at: Option<i64>,
    /// Falls back to `pty_session.opened_at` when there has been no I/O — a
    /// session that never said anything is still allowed to be idle.
    pub since_at: i64,
}

impl SessionSample {
    /// Seconds since this session last did anything observable.
    pub fn idle_secs(&self, now: i64) -> u64 {
        let last = self.last_io_at.unwrap_or(self.since_at);
        ((now - last).max(0) / 1000) as u64
    }
}

/// Everything [`BudgetTracker`] judges. Constructing one of these by hand is how
/// the hysteresis and undo paths are tested without a process in sight.
#[derive(Debug, Clone, PartialEq)]
pub struct WatchdogSample {
    pub at: i64,
    pub sessions: Vec<SessionSample>,
    /// Orphans, unmanaged agent CLIs, MCP servers and editor residue — the
    /// "탐지된 잔여물" half of DESIGN §8-8's global budget.
    pub residue_rss_bytes: u64,
    pub physical_ram_bytes: u64,
    pub pressure: PressureLevel,
    pub pressure_source: PressureSource,
}

impl Default for WatchdogSample {
    fn default() -> Self {
        WatchdogSample {
            at: 0,
            sessions: Vec::new(),
            residue_rss_bytes: 0,
            physical_ram_bytes: 0,
            pressure: PressureLevel::Unknown,
            pressure_source: PressureSource::Unavailable,
        }
    }
}

impl WatchdogSample {
    /// Our sessions plus the residue. The denominator is physical RAM.
    pub fn total_rss(&self) -> u64 {
        self.sessions
            .iter()
            .fold(self.residue_rss_bytes, |acc, s| acc.saturating_add(s.rss_bytes))
    }

    pub fn percent(&self) -> f32 {
        if self.physical_ram_bytes == 0 {
            return 0.0;
        }
        (self.total_rss() as f64 / self.physical_ram_bytes as f64 * 100.0) as f32
    }

    /// The `budget_get()` answer for this sample.
    pub fn budget_state(&self, cfg: &SupervisionConfig) -> BudgetState {
        BudgetState {
            rss_bytes: self.total_rss(),
            physical_ram_bytes: self.physical_ram_bytes,
            percent: self.percent(),
            warn_pct: cfg.global_warn_pct,
            critical_pct: cfg.global_critical_pct,
            pressure_level: self.pressure,
            pressure_source: self.pressure_source,
        }
    }
}

// ---------------------------------------------------------------------------
// alarms
// ---------------------------------------------------------------------------

/// What a tick decided. Producing these is pure; acting on them is not, which is
/// what makes every rule in DESIGN §8-8 testable without a process.
#[derive(Debug, Clone, PartialEq)]
pub enum Alarm {
    /// `softRssMiB` crossed. Badge plus one toast, no signal, ever.
    Soft {
        session_id: String,
        spawn_id: String,
        rss_bytes: u64,
    },
    /// `hardRssMiB` crossed. A signal only when `autoKillOverHardCeiling` is on.
    Hard {
        session_id: String,
        spawn_id: String,
        rss_bytes: u64,
    },
    /// `now - last_io_at` past `idleTimeoutMinutes`.
    Idle {
        session_id: String,
        spawn_id: String,
        idle_secs: u64,
    },
    /// The machine-wide budget crossed `globalWarnPct` / `globalCriticalPct`.
    System {
        rss_bytes: u64,
        percent: f32,
        critical: bool,
    },
}

impl Alarm {
    pub fn session_id(&self) -> Option<&str> {
        match self {
            Alarm::Soft { session_id, .. }
            | Alarm::Hard { session_id, .. }
            | Alarm::Idle { session_id, .. } => Some(session_id),
            Alarm::System { .. } => None,
        }
    }

    pub fn level(&self) -> BudgetLevel {
        match self {
            Alarm::Soft { .. } => BudgetLevel::Soft,
            // An idle expiry is reported on the hard lane: like a hard crossing
            // it opens a 10 s undo window on a real termination.
            Alarm::Hard { .. } | Alarm::Idle { .. } => BudgetLevel::Hard,
            Alarm::System { .. } => BudgetLevel::System,
        }
    }
}

// ---------------------------------------------------------------------------
// the gate
// ---------------------------------------------------------------------------

/// One threshold's memory: armed/disarmed plus when it last fired.
///
/// Both rules are conjunctive, which is the whole point. Hysteresis alone would
/// let a sawtooth workload toast every minute; a rate limit alone would toast
/// again an hour into one continuous six-gigabyte build.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Gate {
    armed: bool,
    last_fired_at: Option<i64>,
}

impl Default for Gate {
    fn default() -> Self {
        Gate {
            armed: true,
            last_fired_at: None,
        }
    }
}

impl Gate {
    /// Whether crossing `threshold` with `value` should fire right now.
    fn observe(&mut self, value: u64, threshold: u64, now: i64, cooldown: Duration) -> bool {
        if threshold == 0 {
            return false;
        }
        if value >= threshold {
            if !self.armed {
                return false;
            }
            if let Some(last) = self.last_fired_at {
                if now.saturating_sub(last) < cooldown.as_millis() as i64 {
                    // Inside the rate limit. Disarm anyway: the crossing has been
                    // seen, and re-arming is what the fall below is for.
                    self.armed = false;
                    return false;
                }
            }
            self.armed = false;
            self.last_fired_at = Some(now);
            return true;
        }
        if (value as f64) < threshold as f64 * REARM_RATIO {
            self.armed = true;
        }
        false
    }
}

/// Per-session alarm memory.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct SessionGates {
    soft: Gate,
    hard: Gate,
    /// Whether an idle termination has already been proposed for the current
    /// quiet stretch. Reset the moment the session does anything.
    idle_fired: bool,
    /// The `last_io_at` the idle decision was made against.
    idle_mark: Option<i64>,
    /// The last sample this session was in. Gates outlive a gap in the samples
    /// for one rate-limit window — see [`BudgetTracker::observe`].
    last_seen: i64,
}

// ---------------------------------------------------------------------------
// the tracker
// ---------------------------------------------------------------------------

/// The pure half of the watchdog: samples in, alarms out, no side effects.
#[derive(Debug, Default)]
pub struct BudgetTracker {
    sessions: HashMap<String, SessionGates>,
    system_warn: Gate,
    system_critical: Gate,
}

impl BudgetTracker {
    pub fn new() -> BudgetTracker {
        BudgetTracker::default()
    }

    /// Judge one sample against the settings. Order is stable: per-session
    /// alarms in sample order, then the machine-wide one.
    pub fn observe(&mut self, sample: &WatchdogSample, cfg: &SupervisionConfig) -> Vec<Alarm> {
        let now = sample.at;
        let soft_bytes = cfg.soft_rss_mi_b.saturating_mul(MIB);
        let hard_bytes = cfg.hard_rss_mi_b.saturating_mul(MIB);
        let idle_limit_secs = (cfg.idle_timeout_minutes as u64).saturating_mul(60);

        let mut alarms = Vec::new();
        let mut seen: HashSet<&str> = HashSet::new();

        for session in &sample.sessions {
            seen.insert(session.session_id.as_str());
            let gates = self
                .sessions
                .entry(session.session_id.clone())
                .or_default();
            gates.last_seen = now;

            // Idle first: a session that is about to be terminated for being
            // idle does not also need a memory badge.
            if idle_limit_secs > 0 {
                let last_io = session.last_io_at;
                if gates.idle_mark != last_io {
                    // The session did something since the last decision.
                    gates.idle_mark = last_io;
                    gates.idle_fired = false;
                }
                let idle_secs = session.idle_secs(now);
                if !gates.idle_fired && idle_secs >= idle_limit_secs {
                    gates.idle_fired = true;
                    alarms.push(Alarm::Idle {
                        session_id: session.session_id.clone(),
                        spawn_id: session.spawn_id.clone(),
                        idle_secs,
                    });
                }
            }

            if gates
                .hard
                .observe(session.rss_bytes, hard_bytes, now, Duration::ZERO)
            {
                alarms.push(Alarm::Hard {
                    session_id: session.session_id.clone(),
                    spawn_id: session.spawn_id.clone(),
                    rss_bytes: session.rss_bytes,
                });
            }
            if gates
                .soft
                .observe(session.rss_bytes, soft_bytes, now, SOFT_ALERT_INTERVAL)
            {
                alarms.push(Alarm::Soft {
                    session_id: session.session_id.clone(),
                    spawn_id: session.spawn_id.clone(),
                    rss_bytes: session.rss_bytes,
                });
            }
        }

        // A session missing from ONE sample has not necessarily ended: a failed
        // registry read under load (`attribute` then sees no rows), or a tick
        // that briefly could not place its pid, drops it for a tick. Forgetting
        // its gates right away re-armed the alarm and the rate limit together,
        // so the next tick toasted the same crossing again (A4). Session ids are
        // UUIDs and never recycled, so keeping the gates for one rate-limit
        // window costs a map entry and cannot hand a cooldown to anybody else;
        // a session that really ended is forgotten once the window has passed.
        let window = SOFT_ALERT_INTERVAL.as_millis() as i64;
        self.sessions.retain(|id, gates| {
            seen.contains(id.as_str()) || now.saturating_sub(gates.last_seen) < window
        });

        let percent = sample.percent();
        let total = sample.total_rss();
        let scale = sample.physical_ram_bytes as f64 / 100.0;
        let warn_bytes = (scale * cfg.global_warn_pct as f64) as u64;
        let critical_bytes = (scale * cfg.global_critical_pct as f64) as u64;
        if self
            .system_critical
            .observe(total, critical_bytes, now, SOFT_ALERT_INTERVAL)
        {
            alarms.push(Alarm::System {
                rss_bytes: total,
                percent,
                critical: true,
            });
        } else if self
            .system_warn
            .observe(total, warn_bytes, now, SOFT_ALERT_INTERVAL)
        {
            alarms.push(Alarm::System {
                rss_bytes: total,
                percent,
                critical: false,
            });
        }

        alarms
    }

    /// How many sessions the tracker is remembering. Diagnostics and tests.
    pub fn tracked_sessions(&self) -> usize {
        self.sessions.len()
    }
}

// ---------------------------------------------------------------------------
// the watchdog
// ---------------------------------------------------------------------------

/// What one tick did. Returned so tests and the log can see it; nothing in the
/// running app consumes it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TickReport {
    pub rows: usize,
    pub sessions: usize,
    pub alarms: Vec<Alarm>,
    /// Undo tokens armed by this tick, in alarm order.
    pub undo_tokens: Vec<String>,
    pub at: i64,
}

struct PressureCache {
    at: Option<Instant>,
    level: PressureLevel,
    source: PressureSource,
}

impl Default for PressureCache {
    fn default() -> Self {
        PressureCache {
            at: None,
            level: PressureLevel::Unknown,
            source: PressureSource::Unavailable,
        }
    }
}

/// The 5 second budget task, plus the scan cache `procmon_scan` answers from.
pub struct Watchdog {
    sup: Arc<Supervisor>,
    events: Events,
    /// The shared process scan (A12.2): discovery publishes, the watchdog
    /// reads. `None` (tests) or a stale hub → the in-process kernel scan,
    /// which stays only as the fallback while discovery is not running.
    scans: Option<Arc<ScanHub>>,
    /// The memory-pressure edge for notifications (`NotifyService::health_memory`).
    on_memory: Option<MemoryListener>,
    /// The hysteresis state behind [`Watchdog::on_memory_pressure`].
    memory_high: Mutex<bool>,
    /// Only for the hot-reloaded monitor classification patterns (T09). Absent
    /// in tests and in any boot where detection has not run yet, in which case
    /// the built-in defaults are used.
    agents: Option<Arc<AgentRegistry>>,
    interval: Duration,
    tracker: Mutex<BudgetTracker>,
    scan: RwLock<Option<ScanResult>>,
    budget: RwLock<BudgetState>,
    pressure: Mutex<PressureCache>,
    /// Physical RAM never changes while the process runs; `sysinfo` is asked once.
    physical_ram_bytes: u64,
    /// Where the health surface reads the Claude Code Orchestrator teams from.
    /// Absent in tests that do not care and before the sidecar exists.
    teams: Option<Arc<dyn TeamSource>>,
    /// The health builder's caches and debounces (`supervise::health`).
    health: Arc<Mutex<HealthBuilder>>,
    /// The last `gilde://health` payload — `health_get` answers from it.
    latest_health: RwLock<HealthSnapshot>,
}

impl std::fmt::Debug for Watchdog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watchdog")
            .field("app_instance", &self.sup.app_instance_id())
            .field("interval", &self.interval)
            .field("physical_ram_bytes", &self.physical_ram_bytes)
            .finish()
    }
}

impl Watchdog {
    pub fn new(sup: Arc<Supervisor>, events: Events) -> Watchdog {
        Watchdog {
            sup,
            events,
            scans: None,
            on_memory: None,
            memory_high: Mutex::new(false),
            agents: None,
            interval: TICK,
            tracker: Mutex::new(BudgetTracker::new()),
            scan: RwLock::new(None),
            budget: RwLock::new(BudgetState::default()),
            pressure: Mutex::new(PressureCache::default()),
            physical_ram_bytes: procscan::total_memory(),
            teams: None,
            health: Arc::new(Mutex::new(HealthBuilder::new(HealthConfig::default()))),
            latest_health: RwLock::new(HealthSnapshot::empty()),
        }
    }

    /// Read the shared process scan from `hub` (A12.2: one scan, discovery's).
    pub fn with_scan_hub(mut self, hub: Arc<ScanHub>) -> Watchdog {
        self.scans = Some(hub);
        self
    }

    /// Tell `listener` about each memory-pressure transition: `true` once the
    /// kernel reports `critical`, `false` once it is back to `normal` (`warn`
    /// and `unknown` keep the previous state — the hysteresis that keeps a
    /// laptop with a big page cache from alerting, §8.4).
    pub fn on_memory_pressure(mut self, listener: MemoryListener) -> Watchdog {
        self.on_memory = Some(listener);
        self
    }

    /// The machine woke (§10.4): the pressure probes start over — a pageout
    /// delta measured across the sleep says nothing about now.
    pub fn woke(&self) {
        *self.pressure.lock() = PressureCache::default();
        clear_pageout_mark();
    }

    /// Read the team facts for the health surface from `teams`.
    pub fn with_teams(mut self, teams: Arc<dyn TeamSource>) -> Watchdog {
        self.teams = Some(teams);
        self
    }

    /// Different health thresholds (tests lower them).
    pub fn with_health_config(self, cfg: HealthConfig) -> Watchdog {
        self.health.lock().set_config(cfg);
        self
    }

    /// The last health snapshot (`health_get`).
    pub fn health(&self) -> HealthSnapshot {
        self.latest_health.read().clone()
    }

    /// Dismiss one health alert (`health_dismiss`). Returns the snapshot as it
    /// now stands and re-publishes it; an unknown id changes nothing.
    pub fn dismiss_health(&self, alert_id: &str) -> HealthSnapshot {
        let snapshot = {
            let mut builder = self.health.lock();
            builder.dismiss(alert_id, now_ms());
            let mut held = self.latest_health.write();
            builder.restamp(&mut held);
            held.clone()
        };
        self.events.emit(HEALTH, snapshot.clone());
        snapshot
    }

    /// Use the user's hot-reloaded `agent-signatures.json` for classification.
    pub fn with_agents(mut self, agents: Arc<AgentRegistry>) -> Watchdog {
        self.agents = Some(agents);
        self
    }

    /// Tick faster (tests). Production always uses [`TICK`].
    pub fn with_interval(mut self, interval: Duration) -> Watchdog {
        self.interval = interval;
        self
    }

    /// Start the 5 second loop. The handle owns the task: dropping it without
    /// calling `abort` leaves it running, aborting it stops the watchdog.
    ///
    /// This is the signature M1-PLAN T12 pins; [`Watchdog::start`] is the same
    /// thing for a watchdog the caller wants to keep a handle on (the command
    /// layer reads its scan cache).
    pub fn spawn(sup: Arc<Supervisor>, events: Events) -> JoinHandle<()> {
        Arc::new(Watchdog::new(sup, events)).start()
    }

    /// Start the loop for an already-constructed watchdog.
    pub fn start(self: &Arc<Self>) -> JoinHandle<()> {
        let me = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            let mut ticker = tokio::time::interval(me.interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                // The first tick completes immediately, which is deliberate: the
                // panel should not wait five seconds for its first rows.
                ticker.tick().await;
                let report = me.tick().await;
                tracing::trace!(
                    target: "supervise",
                    rows = report.rows,
                    sessions = report.sessions,
                    alarms = report.alarms.len(),
                    "watchdog tick"
                );
            }
        })
    }

    /// The last full scan, if there has been one.
    pub fn latest_scan(&self) -> Option<ScanResult> {
        self.scan.read().clone()
    }

    /// `budget_get()`.
    pub fn budget(&self) -> BudgetState {
        self.budget.read().clone()
    }

    /// System pressure, refreshed at most every [`PRESSURE_INTERVAL`].
    pub fn pressure(&self) -> (PressureLevel, PressureSource) {
        let mut cache = self.pressure.lock();
        let stale = match cache.at {
            None => true,
            Some(at) => at.elapsed() >= PRESSURE_INTERVAL,
        };
        if stale {
            let (level, source) = memory_pressure();
            cache.at = Some(Instant::now());
            cache.level = level;
            cache.source = source;
        }
        (cache.level, cache.source)
    }

    /// `procmon_scan(filter, force)`.
    ///
    /// Answers from the watchdog's own cache when it is fresher than the panel's
    /// active poll interval, so a visible panel and a running watchdog cost one
    /// `ps` between them rather than two.
    pub async fn scan(&self, filter: &ScanFilter, force: bool) -> Result<ScanResult> {
        let max_age_ms = (self
            .sup
            .config()
            .get()
            .supervision
            .scan_interval_seconds_active
            .max(1) as i64)
            * 1000;
        let cached = self.latest_scan().filter(|scan| {
            !force && now_ms().saturating_sub(scan.at) <= max_age_ms
        });
        let full = match cached {
            Some(scan) => scan,
            None => self.collect().await?.result,
        };
        Ok(apply_filter(full, filter))
    }

    /// One pass: scan, attribute, record, judge.
    pub async fn tick(&self) -> TickReport {
        let collected = match self.collect().await {
            Ok(collected) => collected,
            Err(err) => {
                tracing::warn!(target: "supervise", error = %err, "watchdog tick could not scan");
                return TickReport {
                    at: now_ms(),
                    ..TickReport::default()
                };
            }
        };

        self.events.emit(
            PROC_SCAN,
            ProcScanEvent {
                at: collected.result.at,
                rows: collected.result.rows.clone(),
                totals: collected.result.totals.clone(),
            },
        );

        self.record(&collected.sample).await;
        self.memory_edge(collected.sample.pressure);

        let cfg = self.sup.config().get().supervision;
        let alarms = self.tracker.lock().observe(&collected.sample, &cfg);
        let undo_tokens = self.apply(&alarms, &collected.sample, &cfg);

        self.publish_health(&collected).await;

        TickReport {
            rows: collected.result.rows.len(),
            sessions: collected.sample.sessions.len(),
            alarms,
            undo_tokens,
            at: collected.sample.at,
        }
    }

    /// Step five: the health surface, from this tick's own scan — never a
    /// second `ps`. Built off the async workers (a cwd read can fall back to
    /// `lsof` for a process `proc_pidinfo` will not describe).
    async fn publish_health(&self, collected: &Collected) {
        let facts: TeamFacts = match &self.teams {
            Some(teams) => {
                teams.poke();
                teams.facts()
            }
            None => TeamFacts::default(),
        };
        let builder = Arc::clone(&self.health);
        let snapshot = collected.snapshot.clone();
        let rows = collected.result.rows.clone();
        let registry = collected.registry.clone();
        let at = collected.result.at;
        let pressure = collected.sample.pressure;
        let physical_ram_bytes = self.physical_ram_bytes;
        let built = tokio::task::spawn_blocking(move || {
            let input = HealthInput {
                at,
                snapshot: &snapshot,
                rows: &rows,
                registry: &registry,
                facts: &facts,
                pressure,
                physical_ram_bytes,
                self_pid: std::process::id() as i32,
            };
            builder.lock().build(&input, &LiveProbe)
        })
        .await;
        match built {
            Ok(health) => {
                *self.latest_health.write() = health.clone();
                self.events.emit(HEALTH, health);
            }
            Err(err) => {
                tracing::warn!(target: "supervise", error = %err, "health build failed");
            }
        }
    }

    // -- the four steps -----------------------------------------------------

    /// Scan and attribute. Updates the scan and budget caches; emits nothing.
    async fn collect(&self) -> Result<Collected> {
        let rows = self
            .sup
            .db()
            .read(|conn| dao::proc::live_rows(conn, None))
            .await
            .unwrap_or_else(|err| {
                tracing::warn!(target: "supervise", error = %err, "watchdog could not read the registry");
                Vec::new()
            });
        let sessions = self
            .sup
            .db()
            .read(dao::pty_session::list_open)
            .await
            .unwrap_or_else(|err| {
                tracing::warn!(target: "supervise", error = %err, "watchdog could not read open sessions");
                Vec::new()
            });

        // A12.2: discovery's scan when a fresh one is shared; the in-process
        // kernel scan only while discovery is not running. Either way off the
        // async worker (a pgid read per pid, or the whole kernel table).
        let shared = self.scans.as_ref().and_then(|hub| hub.fresh(now_ms(), SHARED_SCAN_MAX_AGE_MS));
        let snapshot = tokio::task::spawn_blocking(move || match shared {
            Some(scan) => Ok(Snapshot::from_proc_snapshot(&scan)),
            None => ps_snapshot(),
        })
        .await
        .map_err(|err| GildeError::internal(format!("watchdog scan task failed: {err}")))??;

        let (pressure, pressure_source) = self.pressure();
        let index = self.index_of(&rows, &sessions);

        let signature_holder = self.agents.as_ref().map(|a| a.signatures());
        let fallback;
        let monitor = match &signature_holder {
            Some(sigs) => sigs.monitor(),
            None => {
                fallback = procscan::Signatures::defaults();
                &fallback
            }
        };

        let ctx = RowContext::new(&snapshot, &index, monitor);
        let proc_rows = snapshot.build_rows(&ctx);
        let totals = ProcTotals::of(&proc_rows, self.physical_ram_bytes, pressure);
        let at = now_ms();

        let sample = self.attribute(
            &snapshot,
            &proc_rows,
            &rows,
            &sessions,
            at,
            pressure,
            pressure_source,
        );

        let result = ScanResult {
            rows: proc_rows,
            totals,
            at,
        };
        *self.scan.write() = Some(result.clone());
        *self.budget.write() = sample.budget_state(&self.sup.config().get().supervision);

        Ok(Collected {
            result,
            sample,
            snapshot,
            registry: rows,
        })
    }

    /// pid → registry entry, with `owner_alive` set from the open PTY sessions.
    ///
    /// A row whose session is closed is an orphan even though the process is
    /// alive; that distinction is the difference between a row the user may bulk
    /// clean up and one they may not.
    fn index_of(
        &self,
        rows: &[RegistryRow],
        sessions: &[crate::store::models::PtySessionRow],
    ) -> RegistryIndex {
        let open: HashSet<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        let mut index = RegistryIndex::from_rows(self.sup.app_instance_id(), rows);
        for row in rows {
            if let Some(session_id) = &row.session_id {
                index.set_owner_alive(&row.spawn_id, open.contains(session_id.as_str()));
            }
        }
        index
    }

    /// Sum each session's pgid subtree, and everything that is neither ours nor
    /// the machine's own business.
    #[allow(clippy::too_many_arguments)]
    fn attribute(
        &self,
        snapshot: &Snapshot,
        proc_rows: &[ProcRow],
        registry: &[RegistryRow],
        sessions: &[crate::store::models::PtySessionRow],
        at: i64,
        pressure: PressureLevel,
        pressure_source: PressureSource,
    ) -> WatchdogSample {
        let by_pid: HashMap<i32, &ProcRow> = proc_rows.iter().map(|r| (r.pid, r)).collect();
        let session_rows: HashMap<&str, &crate::store::models::PtySessionRow> =
            sessions.iter().map(|s| (s.id.as_str(), s)).collect();

        let mut counted: HashSet<i32> = HashSet::new();
        let mut samples = Vec::new();

        for row in registry {
            let (Some(session_id), Some(pid)) = (
                row.session_id.as_deref(),
                row.pid.map(|p| p as i32).filter(|p| *p > 0),
            ) else {
                continue;
            };
            if !row.state.is_live() {
                continue;
            }
            let pids = subtree_of(snapshot, pid, row.pgid.map(|p| p as i32));
            let mut rss_bytes = 0u64;
            let mut cpu_pct = 0.0f32;
            for pid in &pids {
                if let Some(proc_row) = by_pid.get(pid) {
                    rss_bytes = rss_bytes.saturating_add(proc_row.rss_bytes);
                    cpu_pct += proc_row.cpu_pct;
                }
            }
            counted.extend(pids);

            let session_row = session_rows.get(session_id);
            samples.push(SessionSample {
                session_id: session_id.to_string(),
                spawn_id: row.spawn_id.clone(),
                vendor: row.vendor.clone(),
                rss_bytes,
                cpu_pct,
                last_io_at: session_row.and_then(|s| s.last_io_at),
                since_at: session_row
                    .map(|s| s.opened_at)
                    .or(row.started_at)
                    .unwrap_or(at),
            });
        }

        let residue_rss_bytes = proc_rows
            .iter()
            .filter(|row| is_residue(row.group) && !counted.contains(&row.pid))
            .fold(0u64, |acc, row| acc.saturating_add(row.rss_bytes));

        WatchdogSample {
            at,
            sessions: samples,
            residue_rss_bytes,
            physical_ram_bytes: self.physical_ram_bytes,
            pressure,
            pressure_source,
        }
    }

    /// `proc_sample` — the graph's sparkline backfill. One statement per session,
    /// one database round trip per tick.
    async fn record(&self, sample: &WatchdogSample) {
        if sample.sessions.is_empty() {
            return;
        }
        let at = sample.at;
        let rows: Vec<(String, u64, f32)> = sample
            .sessions
            .iter()
            .map(|s| (s.session_id.clone(), s.rss_bytes, s.cpu_pct))
            .collect();
        let written = self
            .sup
            .db()
            .write(move |conn| {
                for (session_id, rss, cpu) in &rows {
                    dao::proc_sample::insert(
                        conn,
                        at,
                        "session",
                        Some(session_id),
                        *rss as i64,
                        *cpu as f64,
                    )?;
                }
                Ok(rows.len())
            })
            .await;
        if let Err(err) = written {
            // Losing a sample costs a gap in a sparkline, nothing more.
            tracing::debug!(target: "supervise", error = %err, "proc_sample insert failed");
        }
    }

    /// The memory-pressure edge (see [`Watchdog::on_memory_pressure`]).
    fn memory_edge(&self, level: PressureLevel) {
        let Some(listener) = &self.on_memory else { return };
        let changed = {
            let mut high = self.memory_high.lock();
            let next = match level {
                PressureLevel::Critical => true,
                PressureLevel::Normal => false,
                PressureLevel::Warn | PressureLevel::Unknown => *high,
            };
            let changed = next != *high;
            *high = next;
            changed.then_some(next)
        };
        if let Some(high) = changed {
            listener(high);
        }
    }

    /// Act on the alarms. Returns the undo tokens armed, in alarm order.
    fn apply(
        &self,
        alarms: &[Alarm],
        sample: &WatchdogSample,
        cfg: &SupervisionConfig,
    ) -> Vec<String> {
        let mut tokens = Vec::new();
        for alarm in alarms {
            let undo_token = match alarm {
                // Pressure and soft budgets never signal. Ever.
                Alarm::Soft { .. } | Alarm::System { .. } => None,
                Alarm::Hard { spawn_id, .. } => {
                    if cfg.auto_kill_over_hard_ceiling {
                        Some(self.arm_kill(spawn_id, cfg))
                    } else {
                        // DECISIONS: hard-ceiling kill ships off — warning only.
                        // No token, because a token that cancels nothing is a lie.
                        None
                    }
                }
                Alarm::Idle { spawn_id, .. } => Some(self.arm_kill(spawn_id, cfg)),
            };
            if let Some(token) = &undo_token {
                tokens.push(token.clone());
            }

            let rss_bytes = match alarm {
                Alarm::Soft { rss_bytes, .. } | Alarm::Hard { rss_bytes, .. } => *rss_bytes,
                Alarm::System { rss_bytes, .. } => *rss_bytes,
                Alarm::Idle { session_id, .. } => sample
                    .sessions
                    .iter()
                    .find(|s| &s.session_id == session_id)
                    .map(|s| s.rss_bytes)
                    .unwrap_or(0),
            };

            self.events.emit(
                BUDGET_ALERT,
                BudgetAlertEvent {
                    level: alarm.level(),
                    session_id: alarm.session_id().map(|s| s.to_string()),
                    rss_bytes,
                    budget_percent: sample.percent(),
                    pressure_level: sample.pressure,
                    undo_token: undo_token.clone(),
                },
            );
            self.events
                .emit(TOAST, self.toast_for(alarm, rss_bytes, cfg, undo_token));

            tracing::info!(
                target: "supervise",
                level = ?alarm.level(),
                session = alarm.session_id().unwrap_or("-"),
                rss_mib = rss_bytes / MIB,
                percent = sample.percent(),
                "budget alert"
            );
        }
        tokens
    }

    /// Arm an automatic termination behind the 10 s undo window.
    fn arm_kill(&self, spawn_id: &str, cfg: &SupervisionConfig) -> String {
        reaper::schedule_kill(
            Arc::clone(&self.sup),
            spawn_id,
            reaper::UNDO_WINDOW,
            KillOpts::from_config(cfg),
        )
    }

    fn toast_for(
        &self,
        alarm: &Alarm,
        rss_bytes: u64,
        cfg: &SupervisionConfig,
        undo_token: Option<String>,
    ) -> ToastEvent {
        let mib = rss_bytes / MIB;
        let toast = match alarm {
            Alarm::Soft { .. } => ToastEvent::new(ToastLevel::Warn, toast_key::SOFT)
                .param("rssMiB", mib)
                .param("limitMiB", cfg.soft_rss_mi_b),
            Alarm::Hard { .. } => {
                let key = if cfg.auto_kill_over_hard_ceiling {
                    toast_key::HARD_AUTOKILL
                } else {
                    toast_key::HARD
                };
                ToastEvent::new(ToastLevel::Error, key)
                    .param("rssMiB", mib)
                    .param("limitMiB", cfg.hard_rss_mi_b)
            }
            Alarm::Idle { idle_secs, .. } => ToastEvent::new(ToastLevel::Warn, toast_key::IDLE)
                .param("minutes", idle_secs / 60),
            Alarm::System { percent, .. } => ToastEvent::new(
                if matches!(alarm, Alarm::System { critical: true, .. }) {
                    ToastLevel::Error
                } else {
                    ToastLevel::Warn
                },
                toast_key::SYSTEM,
            )
            .param("percent", percent.round() as i64)
            .param("rssMiB", mib),
        };
        ToastEvent {
            undo_token,
            ..toast
        }
    }
}

/// What [`Watchdog::collect`] produced.
struct Collected {
    result: ScanResult,
    sample: WatchdogSample,
    /// The `ps` snapshot the rows were built from (the health builder walks it).
    snapshot: Snapshot,
    /// The live registry rows this tick read.
    registry: Vec<RegistryRow>,
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Every pid attributable to one session: the process, its descendants, and its
/// process group. All three, because none alone is enough — a `setsid` child
/// leaves the group, and a re-parented grandchild leaves the descendant walk.
fn subtree_of(snapshot: &Snapshot, pid: i32, pgid: Option<i32>) -> HashSet<i32> {
    let mut pids: HashSet<i32> = HashSet::new();
    pids.insert(pid);
    pids.extend(snapshot.descendants_of(pid));
    if let Some(pgid) = pgid.filter(|p| *p > 0) {
        pids.extend(snapshot.pids_in_group(pgid));
    }
    pids
}

/// Whether a scanned row counts as "탐지된 잔여물" for the global budget.
const fn is_residue(group: ProcGroupKind) -> bool {
    matches!(
        group,
        ProcGroupKind::OursOrphan
            | ProcGroupKind::AgentUnmanaged
            | ProcGroupKind::McpServer
            | ProcGroupKind::ExternalResidue
    )
}

/// Narrow a full scan to what the caller asked for.
///
/// `totals` are deliberately **not** recomputed over the filtered set: DESIGN
/// §8-7's fixed footer reports the machine, and a footer that changed every time
/// somebody typed in the search box would be useless for the one job it has.
pub fn apply_filter(scan: ScanResult, filter: &ScanFilter) -> ScanResult {
    let mut rows: Vec<ProcRow> = scan
        .rows
        .into_iter()
        .filter(|row| filter.matches(row))
        .collect();
    if filter.wants_cwd() {
        for row in rows.iter_mut().take(CWD_RESOLVE_LIMIT) {
            if row.cwd.is_none() {
                row.cwd = cwd_of(row.pid);
            }
        }
    }
    ScanResult {
        rows,
        totals: scan.totals,
        at: scan.at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> SupervisionConfig {
        SupervisionConfig::default()
    }

    fn session(id: &str, rss_mib: u64) -> SessionSample {
        SessionSample {
            session_id: id.to_string(),
            spawn_id: format!("sp-{id}"),
            vendor: Some("claude".into()),
            rss_bytes: rss_mib * MIB,
            cpu_pct: 1.0,
            last_io_at: Some(0),
            since_at: 0,
        }
    }

    fn sample(at: i64, sessions: Vec<SessionSample>) -> WatchdogSample {
        WatchdogSample {
            at,
            sessions,
            residue_rss_bytes: 0,
            physical_ram_bytes: 32 * 1024 * MIB,
            pressure: PressureLevel::Normal,
            pressure_source: PressureSource::KernMemorystatus,
        }
    }

    // -- pressure -----------------------------------------------------------

    #[test]
    fn the_first_pressure_source_answers_on_this_machine() {
        // M1-PLAN T12 acceptance 6: prove the primary probe works here.
        let raw = sysctl_int("kern.memorystatus_vm_pressure_level")
            .expect("kern.memorystatus_vm_pressure_level should be readable on macOS");
        assert!(
            memorystatus_level(raw).is_some(),
            "unexpected pressure level {raw}; the documented values are 1, 2 and 4"
        );

        let (level, source) = memory_pressure();
        assert_eq!(
            source,
            PressureSource::KernMemorystatus,
            "the first probe must win when it answers"
        );
        assert_ne!(level, PressureLevel::Unknown);
    }

    #[test]
    fn every_fallback_probe_is_reachable_and_names_itself() {
        // The second key exists on this machine too (measured: 0).
        assert!(sysctl_int("vm.memory_pressure").is_some());
        assert_eq!(legacy_pressure_level(0), PressureLevel::Normal);
        assert_eq!(legacy_pressure_level(10), PressureLevel::Warn);
        assert_eq!(legacy_pressure_level(80), PressureLevel::Critical);

        // The pageout fallback needs two readings before it can say anything.
        reset_pageout_mark();
        assert_eq!(pageout_pressure(), Some(PressureLevel::Unknown));
        assert!(pageout_pressure().is_some());

        assert!(sysctl_int("kern.this.key.does.not.exist").is_none());
        assert_eq!(
            PressureSource::Unavailable.as_str(),
            "unavailable",
            "the diagnostics string is the wire string"
        );
    }

    #[test]
    fn the_pressure_source_serializes_as_the_sysctl_name() {
        let json = serde_json::to_value(PressureSource::KernMemorystatus).expect("json");
        assert_eq!(json, serde_json::json!("kern.memorystatus_vm_pressure_level"));
        assert_eq!(
            serde_json::to_value(PressureSource::VmStat).expect("json"),
            serde_json::json!("vm_stat")
        );
    }

    // -- hysteresis and the once-an-hour rule -------------------------------

    #[test]
    fn a_sustained_soft_crossing_alerts_exactly_once() {
        let mut tracker = BudgetTracker::new();
        let cfg = cfg();
        let mut alerts = 0;
        // Ten minutes at 3 GB against a 2 GB soft budget, at the real cadence.
        for tick in 0..120 {
            let at = tick * TICK.as_millis() as i64;
            let alarms = tracker.observe(&sample(at, vec![session("s1", 3000)]), &cfg);
            alerts += alarms
                .iter()
                .filter(|a| matches!(a, Alarm::Soft { .. }))
                .count();
        }
        assert_eq!(alerts, 1, "one crossing is one toast, not one hundred and twenty");
    }

    #[test]
    fn the_alarm_rearms_only_after_falling_ten_percent_below_the_threshold() {
        let mut tracker = BudgetTracker::new();
        let cfg = cfg();
        let soft = cfg.soft_rss_mi_b; // 2048

        assert_eq!(
            tracker
                .observe(&sample(0, vec![session("s1", soft + 10)]), &cfg)
                .len(),
            1
        );
        // Just under the threshold, but inside the 10 % hysteresis band.
        assert!(tracker
            .observe(&sample(1_000, vec![session("s1", soft - 10)]), &cfg)
            .is_empty());
        assert!(
            tracker
                .observe(&sample(2_000, vec![session("s1", soft + 10)]), &cfg)
                .is_empty(),
            "a sawtooth inside the band must not re-fire"
        );
        // Below 90 % of the threshold: re-armed.
        assert!(tracker
            .observe(&sample(3_000, vec![session("s1", soft / 2)]), &cfg)
            .is_empty());
        // …but the hourly rate limit still holds it back.
        assert!(
            tracker
                .observe(&sample(4_000, vec![session("s1", soft + 10)]), &cfg)
                .is_empty(),
            "re-armed is not enough: one alert per session per hour"
        );
        // An hour later, both gates open.
        let hour = SOFT_ALERT_INTERVAL.as_millis() as i64;
        assert!(tracker
            .observe(&sample(hour + 4_000, vec![session("s1", soft / 2)]), &cfg)
            .is_empty());
        let alarms = tracker.observe(
            &sample(hour + 5_000, vec![session("s1", soft + 10)]),
            &cfg,
        );
        assert_eq!(alarms.len(), 1);
        assert!(matches!(alarms[0], Alarm::Soft { .. }));
    }

    #[test]
    fn each_session_has_its_own_gates() {
        let mut tracker = BudgetTracker::new();
        let cfg = cfg();
        let alarms = tracker.observe(
            &sample(0, vec![session("s1", 3000), session("s2", 3000)]),
            &cfg,
        );
        assert_eq!(alarms.len(), 2);
        assert_eq!(
            alarms
                .iter()
                .filter_map(|a| a.session_id())
                .collect::<Vec<_>>(),
            vec!["s1", "s2"]
        );
        // A session that ends is forgotten once the rate-limit window has
        // passed — not on the first sample it is missing from (see below).
        tracker.observe(&sample(5_000, vec![session("s1", 10)]), &cfg);
        assert_eq!(tracker.tracked_sessions(), 2);
        let hour = SOFT_ALERT_INTERVAL.as_millis() as i64;
        tracker.observe(&sample(hour + 5_000, vec![session("s1", 10)]), &cfg);
        assert_eq!(tracker.tracked_sessions(), 1);
    }

    #[test]
    fn a_session_missing_from_one_sample_does_not_rearm_its_alerts() {
        // A4: a failed registry read under load makes `attribute()` see no rows
        // for one tick. That must not reset the session's gates: before the
        // fix the soft toast fired again on the very next tick.
        let mut tracker = BudgetTracker::new();
        let cfg = cfg();
        let first = tracker.observe(&sample(0, vec![session("s1", 3000)]), &cfg);
        assert_eq!(
            first.iter().filter(|a| matches!(a, Alarm::Soft { .. })).count(),
            1
        );
        assert!(tracker.observe(&sample(5_000, vec![]), &cfg).is_empty());
        let again = tracker.observe(&sample(10_000, vec![session("s1", 3000)]), &cfg);
        assert!(
            again.is_empty(),
            "a gap in the samples is not a new crossing: {again:?}"
        );
        // Gone for longer than the rate-limit window: forgotten.
        let hour = SOFT_ALERT_INTERVAL.as_millis() as i64;
        assert!(tracker.observe(&sample(hour + 20_000, vec![]), &cfg).is_empty());
        assert_eq!(tracker.tracked_sessions(), 0);
    }

    #[test]
    fn crossing_the_hard_ceiling_reports_hard_and_soft_once_each() {
        let mut tracker = BudgetTracker::new();
        let cfg = cfg();
        let alarms = tracker.observe(&sample(0, vec![session("s1", 7000)]), &cfg);
        assert_eq!(alarms.len(), 2, "{alarms:?}");
        assert!(matches!(alarms[0], Alarm::Hard { .. }));
        assert!(matches!(alarms[1], Alarm::Soft { .. }));
        assert!(tracker
            .observe(&sample(5_000, vec![session("s1", 7000)]), &cfg)
            .is_empty());
    }

    // -- idle ---------------------------------------------------------------

    #[test]
    fn idle_is_off_by_default_and_measured_from_last_io() {
        let mut tracker = BudgetTracker::new();
        let mut idle = session("s1", 10);
        idle.last_io_at = Some(0);
        // Default is `idleTimeoutMinutes = 0` — disabled (DECISIONS).
        let two_hours = 2 * 3600 * 1000;
        assert!(tracker
            .observe(&sample(two_hours, vec![idle.clone()]), &cfg())
            .is_empty());

        let mut on = cfg();
        on.idle_timeout_minutes = 30;
        let alarms = tracker.observe(&sample(two_hours, vec![idle.clone()]), &on);
        assert_eq!(alarms.len(), 1);
        assert!(matches!(alarms[0], Alarm::Idle { idle_secs, .. } if idle_secs == 7200));

        // It does not fire twice for the same quiet stretch…
        assert!(tracker
            .observe(&sample(two_hours + 5_000, vec![idle.clone()]), &on)
            .is_empty());
        // …and any real I/O resets it.
        let mut busy = idle.clone();
        busy.last_io_at = Some(two_hours);
        assert!(tracker
            .observe(&sample(two_hours + 10_000, vec![busy]), &on)
            .is_empty());
    }

    #[test]
    fn a_session_that_never_produced_io_falls_back_to_its_open_time() {
        let mut quiet = session("s1", 10);
        quiet.last_io_at = None;
        quiet.since_at = 0;
        assert_eq!(quiet.idle_secs(600_000), 600);

        let mut on = cfg();
        on.idle_timeout_minutes = 5;
        let alarms = BudgetTracker::new().observe(&sample(600_000, vec![quiet]), &on);
        assert_eq!(alarms.len(), 1);
        assert!(matches!(alarms[0], Alarm::Idle { .. }));
    }

    // -- the machine-wide budget --------------------------------------------

    #[test]
    fn the_global_budget_counts_our_sessions_plus_the_residue() {
        let mut s = sample(0, vec![session("s1", 1024)]);
        s.residue_rss_bytes = 1024 * MIB;
        s.physical_ram_bytes = 4096 * MIB;
        assert_eq!(s.total_rss(), 2048 * MIB);
        assert!((s.percent() - 50.0).abs() < 0.01);

        let mut tracker = BudgetTracker::new();
        assert!(
            tracker.observe(&s, &cfg()).is_empty(),
            "50 % is under the 70 % warning threshold"
        );

        // 2000 + 1024 = 3024 MiB of 4096 = 73.8 %: over the warning line, well
        // under the critical one.
        let mut hot = s.clone();
        hot.sessions = vec![session("s1", 2000)];
        let alarms = tracker.observe(&hot, &cfg());
        let system: Vec<_> = alarms
            .iter()
            .filter(|a| matches!(a, Alarm::System { .. }))
            .collect();
        assert_eq!(system.len(), 1, "{alarms:?}");
        assert!(matches!(system[0], Alarm::System { critical: false, .. }));

        // 2600 + 1024 = 3624 MiB of 4096 = 88.5 %.
        let mut critical = s.clone();
        critical.sessions = vec![session("s1", 2600)];
        let alarms = tracker.observe(&critical, &cfg());
        assert!(
            alarms
                .iter()
                .any(|a| matches!(a, Alarm::System { critical: true, .. })),
            "{alarms:?}"
        );
    }

    #[test]
    fn a_zero_ram_reading_never_divides_by_zero() {
        let mut s = sample(0, vec![session("s1", 10)]);
        s.physical_ram_bytes = 0;
        assert_eq!(s.percent(), 0.0);
        assert!(BudgetTracker::new().observe(&s, &cfg()).is_empty());
    }

    #[test]
    fn the_budget_state_is_the_ipc_shape() {
        let s = sample(0, vec![session("s1", 1024)]);
        let json = serde_json::to_value(s.budget_state(&cfg())).expect("json");
        for key in [
            "rssBytes",
            "physicalRamBytes",
            "percent",
            "warnPct",
            "criticalPct",
            "pressureLevel",
            "pressureSource",
        ] {
            assert!(json.get(key).is_some(), "missing {key} in {json}");
        }
        assert_eq!(json.as_object().expect("object").len(), 7);
        assert_eq!(json["warnPct"], serde_json::json!(70));
        assert_eq!(json["criticalPct"], serde_json::json!(85));
    }

    // -- filtering ----------------------------------------------------------

    fn proc_row(pid: i32, group: ProcGroupKind, rss: u64, name: &str) -> ProcRow {
        ProcRow {
            pid,
            ppid: 1,
            pgid: pid,
            name: name.to_string(),
            argv_display: format!("/usr/bin/{name}"),
            rss_bytes: rss,
            cpu_pct: 0.0,
            elapsed_secs: 300,
            start_time: 0,
            cwd: None,
            user_owned: true,
            group,
            tags: Vec::new(),
            owner: None,
            spawn_id: None,
        }
    }

    #[test]
    fn filtering_narrows_the_rows_and_keeps_the_machine_totals() {
        let rows = vec![
            proc_row(2, ProcGroupKind::OursLive, 1_000, "claude"),
            proc_row(3, ProcGroupKind::Other, 9_000, "zsh"),
        ];
        let totals = ProcTotals::of(&rows, 10_000, PressureLevel::Normal);
        let scan = ScanResult {
            rows,
            totals,
            at: 42,
        };
        let filtered = apply_filter(
            scan,
            &ScanFilter {
                groups: Some(vec![ProcGroupKind::OursLive]),
                ..Default::default()
            },
        );
        assert_eq!(filtered.rows.len(), 1);
        assert_eq!(filtered.rows[0].pid, 2);
        assert_eq!(
            filtered.totals.processes, 2,
            "the footer reports the machine, not the search box"
        );
        assert_eq!(filtered.at, 42);
    }

    #[test]
    fn residue_is_everything_that_is_not_a_live_session_or_a_stranger() {
        assert!(is_residue(ProcGroupKind::OursOrphan));
        assert!(is_residue(ProcGroupKind::AgentUnmanaged));
        assert!(is_residue(ProcGroupKind::McpServer));
        assert!(is_residue(ProcGroupKind::ExternalResidue));
        assert!(!is_residue(ProcGroupKind::OursLive));
        assert!(!is_residue(ProcGroupKind::Other));
    }

    #[test]
    fn the_subtree_is_the_union_of_pid_descendants_and_group() {
        use crate::supervise::procscan::PsRow;
        let rows = vec![
            PsRow {
                pid: 100,
                ppid: 1,
                pgid: 100,
                rss_kb: 1,
                cpu_pct: 0.0,
                elapsed_secs: 1,
                user: "u".into(),
                args: "parent".into(),
            },
            PsRow {
                pid: 101,
                ppid: 100,
                pgid: 100,
                rss_kb: 1,
                cpu_pct: 0.0,
                elapsed_secs: 1,
                user: "u".into(),
                args: "child".into(),
            },
            // setsid: left the group, still a descendant.
            PsRow {
                pid: 102,
                ppid: 101,
                pgid: 102,
                rss_kb: 1,
                cpu_pct: 0.0,
                elapsed_secs: 1,
                user: "u".into(),
                args: "escaped".into(),
            },
            // re-parented to launchd, still in the group.
            PsRow {
                pid: 103,
                ppid: 1,
                pgid: 100,
                rss_kb: 1,
                cpu_pct: 0.0,
                elapsed_secs: 1,
                user: "u".into(),
                args: "reparented".into(),
            },
            PsRow {
                pid: 200,
                ppid: 1,
                pgid: 200,
                rss_kb: 1,
                cpu_pct: 0.0,
                elapsed_secs: 1,
                user: "u".into(),
                args: "stranger".into(),
            },
        ];
        let snapshot = Snapshot::from_rows(rows);
        let pids = subtree_of(&snapshot, 100, Some(100));
        assert_eq!(pids, HashSet::from([100, 101, 102, 103]));
        assert!(!pids.contains(&200));
    }

    #[test]
    fn a_gate_with_a_zero_threshold_never_fires() {
        let mut gate = Gate::default();
        assert!(!gate.observe(u64::MAX, 0, 0, Duration::ZERO));
    }
}
