//! Discovery (contract §5), read-only: readers, libproc scanner, FSEvents/kqueue, reconcile, `safe_read` with the never-open filter [C6].
//!
//! Owner: T03. The `mod` lines below are pre-declared by T01 and frozen
//! after wave 0 (contract §15.1, F-18): a new module is a contract change.

pub mod agy;
pub mod claude;
pub mod codex;
pub mod cursor;
pub mod fsevents;
pub mod history;
pub mod kqueue;
pub mod location;
pub mod orchestrator;
pub mod procs;
pub mod reconcile;
pub mod safe_read;
pub mod siblings;
pub mod sqlite;

use std::path::Path;
use std::sync::Arc;

use crate::contract::common::{AbsPath, EpochMs};
use crate::contract::display::TextCap;

pub use reconcile::{
    AccountSpec, Discovery, DiscoveryConfig, DiscoveryDiagnostics, Engine, LiveMember, NoTeamObserver,
    TeamObservation, TeamObserver,
};

// ---------------------------------------------------------------------------
// clocks (§3.5.2: wall for display and source timestamps, uptime for durations)
// ---------------------------------------------------------------------------

/// Wall clock, UTC ms.
pub fn now_wall_ms() -> EpochMs {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `CLOCK_UPTIME_RAW` in ms (stops during sleep, §3.5.2).
pub fn now_up_ms() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `ts` is a valid out-parameter.
    if unsafe { libc::clock_gettime(libc::CLOCK_UPTIME_RAW, &mut ts) } != 0 {
        return 0;
    }
    (ts.tv_sec as u64).saturating_mul(1000) + (ts.tv_nsec as u64) / 1_000_000
}

// ---------------------------------------------------------------------------
// the one display-text boundary (§3.1 rule 6)
// ---------------------------------------------------------------------------

/// Every vendor-originated string that leaves discovery for the store or a
/// command response (titles, first prompts, previews, member names) passes
/// here — the single call site of T04's `sanitize_display_text` in this module
/// tree. Returns `None` for text that is empty after sanitizing.
pub fn display_text(raw: &str, cap: TextCap) -> Option<String> {
    let s = crate::render::sanitize::sanitize_display_text(raw, cap);
    let t = s.text.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

// ---------------------------------------------------------------------------
// hooks the wiring provides (T09 boot, T02 World)
// ---------------------------------------------------------------------------

/// The §9.7 argv redactor — in production `SupervisorApi::redact_argv`
/// (`supervise/redact.rs`, T09): the **one** helper. Discovery applies it at
/// the cache boundary: raw argv never outlives classification [F-03].
pub type Redactor = Arc<dyn Fn(&[String]) -> Vec<String> + Send + Sync>;

/// The redactor used until the wiring installs T09's: fail-closed — the
/// executable's basename, an allowlisted subcommand word (argv[1], or argv[2]
/// under `node`/`bun`) and a `claude-swarm-<pid>` socket name (both are
/// labels of §5.2.2, never values), and nothing else.
pub fn fail_closed_redactor() -> Redactor {
    Arc::new(|argv: &[String]| {
        let mut out = Vec::with_capacity(3);
        if let Some(first) = argv.first() {
            out.push(
                Path::new(first)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("?")
                    .to_string(),
            );
        }
        if let Some(sub) = argv.iter().skip(1).take(2).find(|a| procs::KNOWN_SUBCOMMANDS.contains(&a.as_str())) {
            out.push(sub.clone());
        }
        if let Some(sock) = argv.iter().find(|a| {
            a.strip_prefix("claude-swarm-").is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        }) {
            out.push(sock.clone());
        }
        if argv.len() > out.len() {
            out.push("<arguments hidden>".to_string());
        }
        out
    })
}

/// A process whose `(pid, start)` matches a row of Gilde's own PID registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnMatch {
    /// The row's spawn id (`IdentityHint::gilde_spawn_id`).
    pub spawn_id: String,
    /// The row belongs to the instance running now (else a crash leftover).
    pub this_instance: bool,
}

/// A Gilde-hosted lead (§5.2.4 `Hosted` join).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostedLead {
    pub pid: i32,
    pub spawned_at: EpochMs,
    /// Realpath of the lead's config dir.
    pub config_dir: String,
}

/// Gilde's own processes, as T09's supervisor knows them (`proc_registry` of
/// this `GILDE_HOME`, any instance). Discovery asks before it classifies, so a
/// Gilde process is never Watching, not even for one scan (PM-22, §3.3.2).
pub trait OwnProcesses: Send + Sync {
    /// The row whose `(pid, kernel start seconds)` matches, any instance.
    fn lookup(&self, pid: i32, start_sec: i64) -> Option<OwnMatch>;
    /// A `planned` row whose argv signature and cwd match (the spawn window).
    /// `argv` is the raw argv, borrowed only for the duration of the call.
    fn match_planned(&self, argv: &[String], cwd: Option<&str>) -> Option<String>;
    /// Live hosted leads (for the `Hosted` team join).
    fn hosted_leads(&self) -> Vec<HostedLead>;
}

/// No own processes — a **test double only** (A11.3): the app always wires
/// T09's supervisor, so it is absent from builds without `test-fixtures`.
#[cfg(any(test, feature = "test-fixtures"))]
pub struct NoOwnProcesses;

#[cfg(any(test, feature = "test-fixtures"))]
impl OwnProcesses for NoOwnProcesses {
    fn lookup(&self, _pid: i32, _start_sec: i64) -> Option<OwnMatch> {
        None
    }
    fn match_planned(&self, _argv: &[String], _cwd: Option<&str>) -> Option<String> {
        None
    }
    fn hosted_leads(&self) -> Vec<HostedLead> {
        Vec::new()
    }
}

// ---------------------------------------------------------------------------
// small shared helpers
// ---------------------------------------------------------------------------

/// A realpath'd UTF-8 path as the contract's `AbsPath` (non-UTF-8 → `None`).
pub fn abs_path(p: &Path) -> Option<AbsPath> {
    AbsPath::parse(p.to_str()?).ok()
}

/// `realpath` then [`abs_path`]; `/tmp` and `/private/tmp` spellings compare
/// equal after this (APFS firmlinks, §5.6).
pub fn real_abs_path(p: &Path) -> Option<AbsPath> {
    abs_path(&std::fs::canonicalize(p).ok()?)
}

/// Lower the calling thread to a QoS class (§2.5: discovery at utility; the
/// History fill and the index writer at utility too, A51.4/A51.5).
pub fn set_thread_qos(background: bool) {
    extern "C" {
        fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
    }
    const QOS_CLASS_UTILITY: u32 = 0x11;
    const QOS_CLASS_BACKGROUND: u32 = 0x09;
    let class = if background { QOS_CLASS_BACKGROUND } else { QOS_CLASS_UTILITY };
    // SAFETY: a plain libSystem call on the current thread.
    unsafe {
        pthread_set_qos_class_self_np(class, 0);
    }
}

/// The canonical lowercase UUID rule of §3.2.
pub fn is_vendor_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_digit() || (b'a'..=b'f').contains(c),
        })
}
