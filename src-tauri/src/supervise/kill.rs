//! Kill escalation — DESIGN §8-6.
//!
//! Killing the wrong process is the worst thing this application can do, so
//! this module is written so that **every unknown answer leaks a process rather
//! than signalling a stranger**. Nothing here sends a signal that is not
//! preceded by an identity verdict ([`crate::supervise::identity::verify`]) and
//! an exclusion pass.
//!
//! The sequence, verbatim from DESIGN §8-6:
//!
//! ```text
//! verify() gate
//!   → target union   pids_in_group(pgid) ∪ descendants_of(pid) ∪ with_env(GILDE_SPAWN_ID)
//!   → exclusion set  our ancestor chain, pid ≤ 1, ourselves, foreign uid,
//!                    younger than 200 ms, .app/Contents/MacOS/*
//!   → killpg(TERM) + per-pid TERM
//!   → grace poll (default 5 s, 100 ms interval)
//!   → ★ re-check kernel start times before escalating
//!   → killpg(KILL) + per-pid KILL
//!   → 2 s verification poll
//!   → waitpid(WNOHANG) zombie harvest
//!   → registry state
//!   → event_log
//!   → KillReport
//! ```
//!
//! Three things are deliberately stricter than the pseudocode in the design
//! document, all of them in the "do not kill" direction:
//!
//! 1. **The group signal is itself fenced.** `killpg` is only used when the
//!    recorded pgid is not our own process group, contains no protected pid,
//!    and still contains at least one verified target. A stale pgid whose
//!    leader pid was recycled would otherwise take out an unrelated group.
//! 2. **A protected primary refuses the whole operation.** If the pid the row
//!    names is our ancestor, ourselves, pid ≤ 1, another user's, or a `.app`
//!    bundle main, nothing is signalled at all — not even the group.
//! 3. **The SIGKILL gate drops a pid whose start time we could not read.**
//!    `None == None` is not a match; an unidentifiable pid is never escalated.
//!
//! The age filter uses `libproc`'s microsecond start time rather than `ps`'s
//! whole-second `etime`, because a 900 ms old child reports `etime` 0 and would
//! be excluded from every kill as "too young" — the exclusion is meant to catch
//! a pid recycled *since the snapshot*, not a child we started a moment ago.
//! The pid the registry row names is exempt from it entirely: fence 1 has
//! already proven that pid's identity by kernel start time.
//!
//! Name-matching bulk terminators are banned codebase-wide (DESIGN §8-6) and
//! `scripts/lint-forbidden.sh` enforces it: matching on a name would reach the
//! user's own shell sessions. Everything here signals a pid or a process group
//! we have proven is ours.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use libproc::bsd_info::BSDInfo;
use libproc::libproc::proc_pid::pidinfo;
use nix::errno::Errno;
use nix::sys::signal::{kill as signal_pid, killpg as signal_group, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag};
use nix::unistd::Pid;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::config::model::SupervisionConfig;
use crate::events;
use crate::store::dao;
use crate::store::models::{ProcRow, ProcState};
use crate::supervise::identity::{verify, Fence, ENV_SPAWN_ID};
use crate::supervise::procscan::{self, ps_snapshot, Snapshot};
use crate::supervise::registry::Supervisor;
use crate::supervise::spawn::{is_alive, pgid_of};

// ---------------------------------------------------------------------------
// tunables
// ---------------------------------------------------------------------------

/// DESIGN §8-6 / `config.supervision.killGraceMs`.
pub const DEFAULT_GRACE: Duration = Duration::from_millis(5_000);
/// How long the post-SIGKILL verification poll runs before a pid is declared a
/// survivor.
pub const KILL_VERIFY_BUDGET: Duration = Duration::from_secs(2);
/// Poll interval for both waits.
pub const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// A process younger than this appeared after the snapshot was taken and is
/// therefore not provably one of the processes we discovered.
pub const MIN_TARGET_AGE: Duration = Duration::from_millis(200);

/// macOS `SZOMB` from `<sys/proc.h>`: exited, not yet reaped.
const SZOMB: u32 = 5;
/// What `ps` prints as the command line of a zombie.
const DEFUNCT: &str = "<defunct>";

/// Stable, non-localized reasons. The UI maps them to i18n keys; they are also
/// what lands in `proc_registry.note` and in `event_log`.
pub mod reason {
    /// No registry row with that spawn id.
    pub const NOT_FOUND: &str = "notFound";
    /// The registry could not be read at all.
    pub const REGISTRY_UNAVAILABLE: &str = "registryUnavailable";
    /// `ps` gave us no snapshot, so no target set can be computed.
    pub const SCAN_UNAVAILABLE: &str = "scanUnavailable";
    /// The process is already gone. Not a failure — nothing to signal.
    pub const GONE: &str = "gone";
    /// The row never recorded a pid.
    pub const NO_PID: &str = "noPid";
    /// pid ≤ 1. Never signalled under any confirmation (DESIGN §8-7).
    pub const INIT: &str = "init";
    /// Ourselves.
    pub const SELF: &str = "self";
    /// One of our ancestors — the process that launched us.
    pub const ANCESTOR: &str = "ancestor";
    /// Owned by another user (or ownership unreadable).
    pub const FOREIGN_UID: &str = "foreignUid";
    /// `.*\.app/Contents/MacOS/[^/]+$` — a foreground GUI application.
    pub const APP_BUNDLE_MAIN: &str = "appBundleMain";
    /// Younger than [`MIN_TARGET_AGE`]: it appeared after the snapshot.
    pub const TOO_YOUNG: &str = "tooYoung";
    /// Every discovered pid was excluded.
    pub const NO_TARGETS: &str = "noTargets";
    /// The recorded process group could not be signalled safely.
    pub const GROUP_UNSAFE: &str = "groupUnsafe";
    /// The kernel start time changed between SIGTERM and SIGKILL — the pid was
    /// recycled during the grace window and belongs to somebody else now.
    pub const START_TIME_CHANGED: &str = "startTimeChanged";
}

/// Exclusion reasons that make the *primary* pid untouchable, which refuses the
/// whole operation rather than silently signalling the rest of the tree.
const PROTECTION_REASONS: [&str; 5] = [
    reason::INIT,
    reason::SELF,
    reason::ANCESTOR,
    reason::FOREIGN_UID,
    reason::APP_BUNDLE_MAIN,
];

// ---------------------------------------------------------------------------
// options
// ---------------------------------------------------------------------------

/// How a kill is to be carried out.
///
/// The wire form (`ipc.ts::KillOpts`) carries only `dryRun` and
/// `allowBundleMain`; the grace comes from `config.supervision.killGraceMs` and
/// is filled in by the command layer, so `graceMs` is optional on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "KillOptsWire", into = "KillOptsWire")]
pub struct KillOpts {
    /// How long a target may take to honour SIGTERM before SIGKILL.
    pub grace: Duration,
    /// Compute and report the target and exclusion sets, signal nothing.
    pub dry_run: bool,
    /// Permit `.app/Contents/MacOS/*` targets. Off unless the user explicitly
    /// confirmed one row at a time.
    pub allow_bundle_main: bool,
}

impl Default for KillOpts {
    fn default() -> Self {
        KillOpts {
            grace: DEFAULT_GRACE,
            dry_run: false,
            allow_bundle_main: false,
        }
    }
}

impl KillOpts {
    pub fn from_config(cfg: &SupervisionConfig) -> KillOpts {
        KillOpts {
            grace: Duration::from_millis(cfg.kill_grace_ms),
            ..KillOpts::default()
        }
    }

    pub fn grace(mut self, grace: Duration) -> KillOpts {
        self.grace = grace;
        self
    }

    pub fn dry_run(mut self, dry_run: bool) -> KillOpts {
        self.dry_run = dry_run;
        self
    }

    pub fn allow_bundle_main(mut self, allow: bool) -> KillOpts {
        self.allow_bundle_main = allow;
        self
    }
}

/// Serde shape of [`KillOpts`] — `ipc.ts::KillOpts` plus the optional grace.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct KillOptsWire {
    dry_run: bool,
    allow_bundle_main: bool,
    grace_ms: Option<u64>,
}

impl Default for KillOptsWire {
    fn default() -> Self {
        KillOptsWire {
            dry_run: false,
            allow_bundle_main: false,
            grace_ms: None,
        }
    }
}

impl From<KillOptsWire> for KillOpts {
    fn from(w: KillOptsWire) -> KillOpts {
        KillOpts {
            grace: w.grace_ms.map(Duration::from_millis).unwrap_or(DEFAULT_GRACE),
            dry_run: w.dry_run,
            allow_bundle_main: w.allow_bundle_main,
        }
    }
}

impl From<KillOpts> for KillOptsWire {
    fn from(o: KillOpts) -> KillOptsWire {
        KillOptsWire {
            dry_run: o.dry_run,
            allow_bundle_main: o.allow_bundle_main,
            grace_ms: Some(o.grace.as_millis() as u64),
        }
    }
}

// ---------------------------------------------------------------------------
// report
// ---------------------------------------------------------------------------

/// One pid we refused to signal, and why. Mirrors `ipc.ts::KillReport.excluded`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KillExclusion {
    pub pid: i32,
    pub reason: String,
}

impl KillExclusion {
    pub fn new(pid: i32, reason: impl Into<String>) -> KillExclusion {
        KillExclusion {
            pid,
            reason: reason.into(),
        }
    }
}

impl From<(i32, String)> for KillExclusion {
    fn from((pid, reason): (i32, String)) -> KillExclusion {
        KillExclusion { pid, reason }
    }
}

impl From<(i32, &str)> for KillExclusion {
    fn from((pid, reason): (i32, &str)) -> KillExclusion {
        KillExclusion::new(pid, reason)
    }
}

/// The result of a kill — **never a bare "ok"** (DESIGN §8-6).
///
/// The UI reports it verbatim: "6 terminated, 1 refused (argv mismatch), 1
/// survived SIGKILL". Mirrors `ipc.ts::KillReport`; `fence`, `escalated` and
/// `at` are additions this module records for the audit trail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KillReport {
    /// The registry row this kill was for, when there was one.
    pub spawn_id: Option<String>,
    /// Every pid that was (or, in a dry run, would be) signalled.
    pub requested: Vec<i32>,
    /// Every pid that was discovered and then deliberately spared.
    pub excluded: Vec<KillExclusion>,
    /// Pids still alive after the escalation finished.
    pub survivors: Vec<i32>,
    /// Set when nothing was signalled at all.
    pub refused_reason: Option<String>,
    pub dry_run: bool,
    /// Pids that ignored SIGTERM and had to be escalated to SIGKILL.
    pub escalated: Vec<i32>,
    /// The identity verdict that authorised this kill: `"verified"`, `"gone"`,
    /// a mismatch reason, or `"unfenced"` for a bare pid with no registry row.
    pub fence: String,
    /// Unix millis at which the report was produced.
    pub at: i64,
    /// What each pid in `requested` is (argv\[0\] basename, as the plan's scan
    /// saw it) — the confirm sheet names every pid it is about to signal.
    #[serde(default)]
    pub names: Vec<KillName>,
}

/// One pid of a kill plan and its process name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KillName {
    pub pid: i32,
    pub name: String,
}

impl KillReport {
    /// A kill that never sent a signal.
    pub fn refused(spawn_id: Option<String>, fence: &str, reason: &str) -> KillReport {
        KillReport {
            spawn_id,
            requested: Vec::new(),
            excluded: Vec::new(),
            survivors: Vec::new(),
            refused_reason: Some(reason.to_string()),
            dry_run: false,
            escalated: Vec::new(),
            fence: fence.to_string(),
            at: now_ms(),
            names: Vec::new(),
        }
    }

    /// Whether any signal was sent.
    pub fn is_refused(&self) -> bool {
        self.refused_reason.is_some()
    }

    /// Whether everything that was targeted is gone.
    pub fn is_complete(&self) -> bool {
        !self.is_refused() && self.survivors.is_empty()
    }

    /// Stamp a refusal onto a report that was built from a plan.
    ///
    /// A refusal means **no signal was sent at all** (DESIGN §8-4: nothing in
    /// this codebase may signal without `Verified`; DESIGN §8-6 answers a
    /// refusal with `KillReport::refused`, whose `requested` is empty), so the
    /// would-be target set must not survive into `requested` — that field is
    /// documented as "every pid that was (or, in a dry run, would be)
    /// signalled", and a refused plan signals none of them. The exclusion set
    /// is kept: it is the evidence for *why* the refusal happened.
    ///
    /// The rest of the system already reads this invariant:
    /// `src/store/procSlice.ts::dryRunSets` treats `refusedReason` with a
    /// non-empty `requested` as a partial kill, `dispatch::engine`'s report
    /// merge decides "nothing was signalled" from `requested.is_empty()`, and
    /// `tests/watchdog.rs` / `tests/kill_safety.rs` / `tests/pty_fidelity.rs`
    /// assert it on every other refusal path.
    fn with_reason(mut self, reason: &str) -> KillReport {
        self.refused_reason = Some(reason.to_string());
        self.requested.clear();
        self.names.clear();
        self
    }
}

/// What a caller asked to kill. Mirrors `ipc.ts::KillTarget`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "target", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum KillTarget {
    Spawn { spawn_id: String },
    Pid { pid: i32 },
}

impl KillTarget {
    pub fn spawn(spawn_id: impl Into<String>) -> KillTarget {
        KillTarget::Spawn {
            spawn_id: spawn_id.into(),
        }
    }

    pub fn pid(pid: i32) -> KillTarget {
        KillTarget::Pid { pid }
    }
}

/// One row of `procmon_kill`'s answer. Mirrors `ipc.ts::KillOutcome`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KillOutcome {
    pub target: KillTarget,
    pub report: KillReport,
}

// ---------------------------------------------------------------------------
// public entry points
// ---------------------------------------------------------------------------

/// Terminate the process tree behind one registry row (DESIGN §8-6).
///
/// Never returns an error: a kill that could not happen is a [`KillReport`]
/// with a `refused_reason`, because "the operation failed" and "we deliberately
/// declined to signal" are different facts and the UI must show which.
pub async fn kill_tree(sup: &Supervisor, spawn_id: &str, opts: KillOpts) -> KillReport {
    let row = match sup.row(spawn_id).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            return KillReport::refused(
                Some(spawn_id.to_string()),
                reason::NOT_FOUND,
                reason::NOT_FOUND,
            )
        }
        Err(e) => {
            tracing::warn!(target: "supervise", spawn_id, error = %e, "kill: registry unavailable");
            return KillReport::refused(
                Some(spawn_id.to_string()),
                reason::REGISTRY_UNAVAILABLE,
                reason::REGISTRY_UNAVAILABLE,
            );
        }
    };

    let snap = match ps_snapshot() {
        Ok(snap) => snap,
        Err(e) => {
            tracing::warn!(target: "supervise", spawn_id, error = %e, "kill: no process snapshot");
            return KillReport::refused(
                Some(spawn_id.to_string()),
                reason::SCAN_UNAVAILABLE,
                reason::SCAN_UNAVAILABLE,
            );
        }
    };

    // ---- the fence gate. Nothing below this point runs without `Verified`.
    let fence = verify(&row, &snap);
    match fence {
        Fence::Verified => {}
        Fence::Gone => {
            let report = KillReport::refused(Some(spawn_id.to_string()), "gone", reason::GONE);
            close_gone_row(sup, &row).await;
            write_event_log(sup, &row, &report, &[], row.pid.map(|p| p as i32), None).await;
            return report;
        }
        Fence::Mismatch(m) => {
            let report =
                KillReport::refused(Some(spawn_id.to_string()), m.as_str(), m.as_str());
            mark_lost(sup, &row, m.as_str()).await;
            warn_refusal(sup, spawn_id, m.as_str());
            write_event_log(sup, &row, &report, &[], row.pid.map(|p| p as i32), None).await;
            return report;
        }
    }

    let Some(primary) = row.pid.map(|p| p as i32).filter(|p| *p > 0) else {
        // Unreachable after `Verified`, but the type says it is possible.
        return KillReport::refused(Some(spawn_id.to_string()), "verified", reason::NO_PID);
    };

    let guard = SelfGuard::of(&snap);
    let plan = KillPlan::build(
        primary,
        row.pgid.map(|p| p as i32),
        Some(row.env_marker.as_str()),
        Some(spawn_id.to_string()),
        &snap,
        &opts,
        &guard,
    );

    if let Some(refused) = plan.refused.clone() {
        let (primary, group) = (plan.primary, plan.group);
        let report = plan.into_report("verified", &opts).with_reason(&refused);
        warn_refusal(sup, spawn_id, &refused);
        write_event_log(sup, &row, &report, &[], Some(primary), group).await;
        return report;
    }

    if opts.dry_run {
        let (primary, group) = (plan.primary, plan.group);
        let report = plan.into_report("verified", &opts);
        write_event_log(sup, &row, &report, &[], Some(primary), group).await;
        return report;
    }

    // Rows this instance created have an owner holding the `Child` handle.
    let harvest = row.app_instance != sup.app_instance_id();
    let outcome = execute(&plan, &opts, &snap, harvest).await;
    let report = outcome.report("verified", &plan, &opts);

    record_result(sup, &row, &report, outcome.last_signal).await;
    write_event_log(sup, &row, &report, &outcome.per_pid, Some(plan.primary), plan.group).await;
    invalidate_registry(sup, spawn_id);
    report
}

/// Kill a batch of targets, one report per target, in the order given
/// (DESIGN §8-7: "반환은 행별 `KillOutcome` 목록").
///
/// A `Pid` target that turns out to belong to a live registry row is routed
/// through [`kill_tree`] so that the three identity fences apply; a pid with no
/// row — the Process Monitor's `ExternalResidue` and `McpServer` groups — is
/// killed with the exclusion set and the start-time re-check as its only
/// protection, and **never** by process group: an unmanaged helper's group
/// usually belongs to the GUI application that spawned it.
pub async fn kill_pids(
    sup: &Supervisor,
    targets: &[KillTarget],
    opts: KillOpts,
) -> Vec<KillOutcome> {
    let mut out = Vec::with_capacity(targets.len());
    for target in targets {
        let report = match target {
            KillTarget::Spawn { spawn_id } => kill_tree(sup, spawn_id, opts).await,
            KillTarget::Pid { pid } => match live_row_for_pid(sup, *pid).await {
                Some(row) => kill_tree(sup, &row.spawn_id, opts).await,
                None => kill_bare_pid(sup, *pid, opts).await,
            },
        };
        out.push(KillOutcome {
            target: target.clone(),
            report,
        });
    }
    out
}

/// Kill a pid that no registry row claims. Exposed because the Process Monitor
/// legitimately terminates residue we never spawned; it is deliberately the
/// weakest path and does no group signalling.
pub async fn kill_bare_pid(sup: &Supervisor, pid: i32, opts: KillOpts) -> KillReport {
    let snap = match ps_snapshot() {
        Ok(snap) => snap,
        Err(_) => return KillReport::refused(None, "unfenced", reason::SCAN_UNAVAILABLE),
    };
    if !snap.contains(pid) && !is_alive(pid) {
        return KillReport::refused(None, "gone", reason::GONE);
    }

    let guard = SelfGuard::of(&snap);
    // `None` for the pgid: no group signal for a process we did not create.
    let plan = KillPlan::build(pid, None, None, None, &snap, &opts, &guard);

    if let Some(refused) = plan.refused.clone() {
        let report = plan.into_report("unfenced", &opts).with_reason(&refused);
        write_bare_event_log(sup, pid, &report, &[]).await;
        return report;
    }
    if opts.dry_run {
        return plan.into_report("unfenced", &opts);
    }

    // Nothing in this process owns a handle to a pid we never registered.
    let outcome = execute(&plan, &opts, &snap, true).await;
    let report = outcome.report("unfenced", &plan, &opts);
    write_bare_event_log(sup, pid, &report, &outcome.per_pid).await;
    report
}

// ---------------------------------------------------------------------------
// target discovery and exclusion
// ---------------------------------------------------------------------------

/// The pids that may never be signalled, computed once per kill.
#[derive(Debug, Clone)]
struct SelfGuard {
    pid: i32,
    pgid: Option<i32>,
    uid: Option<u32>,
    ancestors: HashSet<i32>,
}

impl SelfGuard {
    fn of(snap: &Snapshot) -> SelfGuard {
        let pid = std::process::id() as i32;
        SelfGuard {
            pid,
            pgid: pgid_of(pid),
            uid: procscan::uid_of(pid),
            ancestors: snap.ancestors_of(pid).into_iter().collect(),
        }
    }

    /// Why `pid` must not be signalled, if it must not be.
    ///
    /// `is_primary` exempts the pid the registry row names from the age filter:
    /// its identity is already proven by the kernel start-time fence, and `ps`
    /// reports whole seconds, so a legitimately young child would otherwise be
    /// unkillable for its first second of life.
    fn exclusion_for(
        &self,
        pid: i32,
        snap: &Snapshot,
        opts: &KillOpts,
        is_primary: bool,
    ) -> Option<&'static str> {
        if pid <= 1 {
            return Some(reason::INIT);
        }
        if pid == self.pid {
            return Some(reason::SELF);
        }
        if self.ancestors.contains(&pid) {
            return Some(reason::ANCESTOR);
        }
        // An unreadable uid is not a pass: DESIGN §8-6 keeps only pids that
        // provably belong to us.
        match (procscan::uid_of(pid), self.uid) {
            (Some(theirs), Some(ours)) if theirs == ours => {}
            _ => return Some(reason::FOREIGN_UID),
        }
        if snap.is_app_bundle_main(pid) && !opts.allow_bundle_main {
            return Some(reason::APP_BUNDLE_MAIN);
        }
        if !is_primary {
            match age_of(pid, snap) {
                Some(age) if age > MIN_TARGET_AGE => {}
                _ => return Some(reason::TOO_YOUNG),
            }
        }
        None
    }
}

/// The computed target set of one kill. Built once and used by both the dry run
/// and the real path, so the two can never disagree — DESIGN §8-7 requires the
/// dry-run toggle to render exactly what a real kill would do.
#[derive(Debug, Clone)]
struct KillPlan {
    spawn_id: Option<String>,
    primary: i32,
    /// Sorted, de-duplicated.
    targets: Vec<i32>,
    excluded: Vec<KillExclusion>,
    /// The process group to signal, when signalling it is provably safe.
    group: Option<i32>,
    refused: Option<String>,
    /// Kernel start times read *before* any signal — the SIGKILL gate compares
    /// against these.
    start_times: HashMap<i32, Option<i64>>,
    /// `targets` with their names, from the plan's own scan.
    names: Vec<KillName>,
}

impl KillPlan {
    fn build(
        primary: i32,
        pgid: Option<i32>,
        env_marker: Option<&str>,
        spawn_id: Option<String>,
        snap: &Snapshot,
        opts: &KillOpts,
        guard: &SelfGuard,
    ) -> KillPlan {
        // --- the recorded group counts only when the row's own pid leads it.
        //     DESIGN §8-1: "pid로부터 pgid를 유추하지 않는다" — and the converse
        //     holds for discovery too: a group the primary does not lead is
        //     somebody else's (the shell that started an adopted process, or a
        //     stale id whose leader left and was recycled into a stranger's
        //     group). `group_is_safe` below fenced only the `killpg`; the
        //     members of such a group still became per-pid targets (A4: a row
        //     whose recorded pgid named a bystander's group TERM'd the
        //     bystander). Such a row is killed by pid, descendants and env
        //     marker only, and the refusal is reported.
        let (pgid, foreign_group) = match pgid.filter(|p| *p > 0) {
            Some(g) if g == primary => (Some(g), None),
            Some(g) => (None, Some(g)),
            None => (None, None),
        };

        // --- union of the three independent discovery paths (DESIGN §8-6).
        let mut discovered: HashSet<i32> = HashSet::new();
        discovered.insert(primary);
        if let Some(pgid) = pgid {
            discovered.extend(snap.pids_in_group(pgid));
        }
        discovered.extend(snap.descendants_of(primary));
        if let Some(marker) = env_marker.filter(|m| !m.is_empty()) {
            // Best effort by design: reading another process's environment is
            // restricted on macOS (DESIGN §8-4).
            discovered.extend(snap.with_env(ENV_SPAWN_ID, marker));
        }

        // --- exclusion set, applied last and unconditionally.
        let mut targets: Vec<i32> = Vec::with_capacity(discovered.len());
        let mut excluded: Vec<KillExclusion> = Vec::new();
        let mut sorted: Vec<i32> = discovered.into_iter().collect();
        sorted.sort_unstable();
        let mut primary_excluded: Option<&'static str> = None;
        for pid in sorted {
            match guard.exclusion_for(pid, snap, opts, pid == primary) {
                Some(why) => {
                    if pid == primary {
                        primary_excluded = Some(why);
                    }
                    excluded.push(KillExclusion::new(pid, why));
                }
                None => targets.push(pid),
            }
        }

        // --- refusals. A protected primary stops everything, including the
        //     group signal: the group of an ancestor is very likely our own.
        let refused = match primary_excluded {
            Some(why) if PROTECTION_REASONS.contains(&why) => Some(why.to_string()),
            _ if targets.is_empty() => Some(reason::NO_TARGETS.to_string()),
            _ => None,
        };

        // --- the group signal is fenced too.
        let group = match pgid.filter(|p| *p > 0) {
            Some(pgid) if refused.is_none() && group_is_safe(pgid, snap, guard, &targets) => {
                Some(pgid)
            }
            Some(pgid) => {
                excluded.push(KillExclusion::new(pgid, reason::GROUP_UNSAFE));
                None
            }
            None => None,
        };
        if let Some(g) = foreign_group {
            excluded.push(KillExclusion::new(g, reason::GROUP_UNSAFE));
        }

        let start_times = if refused.is_some() || opts.dry_run {
            HashMap::new()
        } else {
            targets
                .iter()
                .map(|pid| (*pid, procscan::start_time_of(*pid)))
                .collect()
        };

        let names = targets
            .iter()
            .map(|pid| KillName {
                pid: *pid,
                name: snap.get(*pid).map(|r| r.name()).unwrap_or_default(),
            })
            .collect();

        KillPlan {
            spawn_id,
            primary,
            targets,
            excluded,
            group,
            refused,
            start_times,
            names,
        }
    }

    fn into_report(self, fence: &str, opts: &KillOpts) -> KillReport {
        KillReport {
            spawn_id: self.spawn_id,
            requested: self.targets,
            excluded: self.excluded,
            survivors: Vec::new(),
            refused_reason: None,
            dry_run: opts.dry_run,
            escalated: Vec::new(),
            fence: fence.to_string(),
            at: now_ms(),
            names: self.names,
        }
    }
}

/// Whether `pgid` may be signalled as a group.
///
/// Requires all four: it is not our own group; every member we can see is ours
/// and unprotected; and at least one of our verified targets is still in it, so
/// a pgid whose leader pid was recycled cannot redirect the signal at a group
/// we never created.
fn group_is_safe(pgid: i32, snap: &Snapshot, guard: &SelfGuard, targets: &[i32]) -> bool {
    if pgid <= 1 || Some(pgid) == guard.pgid {
        return false;
    }
    let members = snap.pids_in_group(pgid);
    if members.is_empty() {
        return false;
    }
    if !members.iter().any(|m| targets.contains(m)) {
        return false;
    }
    members.iter().all(|m| {
        *m > 1
            && *m != guard.pid
            && !guard.ancestors.contains(m)
            && match (procscan::uid_of(*m), guard.uid) {
                (Some(theirs), Some(ours)) => theirs == ours,
                _ => false,
            }
    })
}

// ---------------------------------------------------------------------------
// execution
// ---------------------------------------------------------------------------

/// What one pid received and what became of it — the per-pid audit row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PidResult {
    pub pid: i32,
    pub term: bool,
    pub kill: bool,
    pub gone: bool,
    /// Set when the pid was dropped before escalation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
}

struct Execution {
    survivors: Vec<i32>,
    escalated: Vec<i32>,
    dropped: Vec<KillExclusion>,
    per_pid: Vec<PidResult>,
    last_signal: Option<i32>,
}

impl Execution {
    fn report(&self, fence: &str, plan: &KillPlan, opts: &KillOpts) -> KillReport {
        let mut excluded = plan.excluded.clone();
        excluded.extend(self.dropped.iter().cloned());
        KillReport {
            spawn_id: plan.spawn_id.clone(),
            requested: plan.targets.clone(),
            excluded,
            survivors: self.survivors.clone(),
            refused_reason: None,
            dry_run: opts.dry_run,
            escalated: self.escalated.clone(),
            fence: fence.to_string(),
            at: now_ms(),
            names: plan.names.clone(),
        }
    }
}

/// TERM → grace → start-time re-check → KILL → verify → reap.
///
/// `harvest` says whether this call is entitled to `waitpid` the targets that
/// are our own direct children — see [`reap_zombies`].
async fn execute(
    plan: &KillPlan,
    opts: &KillOpts,
    snap: &Snapshot,
    harvest: bool,
) -> Execution {
    // ---- SIGTERM, group first then per pid.
    if let Some(pgid) = plan.group {
        send_group(pgid, Signal::SIGTERM);
    }
    for pid in &plan.targets {
        send_one(*pid, Signal::SIGTERM);
    }
    let mut last_signal = Some(Signal::SIGTERM as i32);

    let survivors = poll_until_gone(&plan.targets, opts.grace).await;

    // ---- ★ the escalation gate. A pid that died during the grace window may
    //      already belong to somebody else; SIGKILL to a recycled pid kills a
    //      stranger's process.
    let (mut escalate, dropped) = survivors_still_ours(&survivors, &plan.start_times);

    if !escalate.is_empty() {
        // Re-check the group against a fresh snapshot: the world moved while we
        // waited out the grace.
        let fresh = ps_snapshot().unwrap_or_else(|_| snap.clone());
        let guard = SelfGuard::of(&fresh);
        if let Some(pgid) = plan
            .group
            .filter(|pgid| group_is_safe(*pgid, &fresh, &guard, &escalate))
        {
            send_group(pgid, Signal::SIGKILL);
        }
        for pid in &escalate {
            send_one(*pid, Signal::SIGKILL);
        }
        last_signal = Some(Signal::SIGKILL as i32);
    }
    escalate.sort_unstable();

    let still = poll_until_gone(&escalate, KILL_VERIFY_BUDGET).await;

    // ---- harvest the zombies we are actually entitled to reap.
    if harvest {
        reap_zombies(&plan.targets, snap);
    }

    // A zombie is gone for every purpose this report has; only re-check the
    // pids that were still alive a moment ago.
    let survivors_final: Vec<i32> = still.into_iter().filter(|p| !is_gone(*p)).collect();

    let per_pid = plan
        .targets
        .iter()
        .map(|pid| PidResult {
            pid: *pid,
            term: true,
            kill: escalate.contains(pid),
            gone: !survivors_final.contains(pid),
            skipped: dropped
                .iter()
                .find(|e| e.pid == *pid)
                .map(|e| e.reason.clone()),
        })
        .collect();

    Execution {
        survivors: survivors_final,
        escalated: escalate,
        dropped,
        per_pid,
        last_signal,
    }
}

/// The SIGKILL gate of DESIGN §8-6, exported so the re-check can be exercised
/// without waiting for the kernel to actually recycle a pid.
///
/// Keeps only pids whose kernel start time still equals the one recorded before
/// SIGTERM. A pid whose start time is unreadable — then or now — is **dropped**,
/// never escalated: an unidentifiable pid is not ours to kill.
pub fn survivors_still_ours(
    survivors: &[i32],
    recorded: &HashMap<i32, Option<i64>>,
) -> (Vec<i32>, Vec<KillExclusion>) {
    let mut keep = Vec::new();
    let mut dropped = Vec::new();
    for pid in survivors {
        let before = recorded.get(pid).copied().flatten();
        let now = procscan::start_time_of(*pid);
        match (before, now) {
            (Some(a), Some(b)) if a == b => keep.push(*pid),
            _ => dropped.push(KillExclusion::new(*pid, reason::START_TIME_CHANGED)),
        }
    }
    (keep, dropped)
}

/// Poll `targets` until they are all gone or `budget` expires. Returns the ones
/// still alive.
async fn poll_until_gone(targets: &[i32], budget: Duration) -> Vec<i32> {
    if targets.is_empty() {
        return Vec::new();
    }
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let alive: Vec<i32> = targets.iter().copied().filter(|p| !is_gone(*p)).collect();
        if alive.is_empty() {
            return alive;
        }
        if tokio::time::Instant::now() >= deadline {
            return alive;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// `waitpid(WNOHANG)` for the targets that are our own direct children.
///
/// Two deliberate narrowings, both about not stealing an exit status:
///
///   * **Never `waitpid(-1)`.** This process also owns children driven by
///     `tokio::process` and `portable-pty`; harvesting one of those out from
///     under its owner turns the owner's `wait()` into `ECHILD` and loses the
///     exit code. Only the pids we are killing are ever waited on.
///   * **Never a child of the *current* app instance.** A row this instance
///     created has a live owner (the PTY manager, the sidecar manager,
///     `run_git`) that holds the `Child` handle and will reap it. The harvest
///     therefore runs only for rows inherited from a crashed earlier instance
///     and for bare pids with no registry row — where nobody else is waiting,
///     and where a direct-child zombie would otherwise linger in the monitor.
///
/// A grandchild whose parent we killed is re-parented to `launchd`, so it can
/// never be one of our zombies; `waitpid` on it returns `ECHILD`, which is a
/// no-op here.
///
/// None of the report depends on this running: [`is_gone`] already counts a
/// zombie as gone, so an unharvested child is never mistaken for a survivor.
fn reap_zombies(targets: &[i32], snap: &Snapshot) {
    let me = std::process::id() as i32;
    for pid in targets {
        let is_direct_child = snap.get(*pid).map(|r| r.ppid == me).unwrap_or(false);
        if !is_direct_child {
            continue;
        }
        match waitpid(Pid::from_raw(*pid), Some(WaitPidFlag::WNOHANG)) {
            Ok(_) | Err(Errno::ECHILD) => {}
            Err(e) => {
                tracing::debug!(target: "supervise", pid, error = %e, "waitpid failed");
            }
        }
    }
}

fn send_one(pid: i32, sig: Signal) {
    if pid <= 1 {
        return;
    }
    match signal_pid(Pid::from_raw(pid), sig) {
        Ok(()) | Err(Errno::ESRCH) => {}
        Err(e) => tracing::debug!(target: "supervise", pid, %sig, error = %e, "signal failed"),
    }
}

fn send_group(pgid: i32, sig: Signal) {
    if pgid <= 1 {
        return;
    }
    match signal_group(Pid::from_raw(pgid), sig) {
        Ok(()) | Err(Errno::ESRCH) => {}
        Err(e) => tracing::debug!(target: "supervise", pgid, %sig, error = %e, "group signal failed"),
    }
}

// ---------------------------------------------------------------------------
// process facts
// ---------------------------------------------------------------------------

fn bsd_info(pid: i32) -> Option<BSDInfo> {
    if pid <= 0 {
        return None;
    }
    pidinfo::<BSDInfo>(pid, 0).ok()
}

/// Whether `pid` has exited without being reaped yet.
///
/// This matters more than it looks: `kill(pid, 0)` succeeds on a zombie, so
/// without this check a killed direct child of ours would be reported as a
/// survivor of SIGKILL forever — and `kill_tree` deliberately leaves the reaping
/// to whoever owns the `Child` handle (see [`reap_zombies`]).
///
/// Measured on this machine (Darwin 25.5): the kernel keeps **no**
/// `proc_bsdinfo` for a zombie — `proc_pidinfo` fails — while `ps` reports
/// `stat` `Z` and the command line `<defunct>`. So the cheap pre-filter is
/// "alive but no BSD info", confirmed by one `ps` on that rare branch. Every pid
/// this module targets had readable BSD info a moment earlier (that is how the
/// uid filter cleared it), so the pre-filter costs one syscall on the hot path.
///
/// A pid that vanished entirely between the two probes also answers `true`: the
/// only consumer is [`is_gone`], and "no longer there" is the same verdict.
pub fn is_zombie(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    if let Some(info) = bsd_info(pid) {
        return info.pbi_status == SZOMB;
    }
    match procscan::ps_args_of(pid) {
        Some(args) => args.trim() == DEFUNCT,
        None => true,
    }
}

/// Gone for the purposes of a kill report: either not there, or a zombie.
pub fn is_gone(pid: i32) -> bool {
    !is_alive(pid) || is_zombie(pid)
}

/// Age of `pid` with millisecond resolution, from the kernel start time.
/// Falls back to the snapshot's whole-second `etime`.
fn age_of(pid: i32, snap: &Snapshot) -> Option<Duration> {
    if let Some(info) = bsd_info(pid) {
        let started_ms = (info.pbi_start_tvsec as i64) * 1000 + (info.pbi_start_tvusec as i64) / 1000;
        let age = now_ms() - started_ms;
        return Some(Duration::from_millis(age.max(0) as u64));
    }
    snap.age_of(pid)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// registry + audit trail
// ---------------------------------------------------------------------------

async fn live_row_for_pid(sup: &Supervisor, pid: i32) -> Option<ProcRow> {
    let rows = sup
        .db()
        .read(move |conn| dao::proc::by_pid(conn, pid as i64))
        .await
        .ok()?;
    // Exactly one live claimant, or none: two rows naming the same pid is an
    // ambiguity, and DESIGN §8-5 never signals on an ambiguity.
    match rows.len() {
        1 => rows.into_iter().next(),
        _ => None,
    }
}

async fn close_gone_row(sup: &Supervisor, row: &ProcRow) {
    let spawn_id = row.spawn_id.clone();
    let at = now_ms();
    let _ = sup
        .db()
        .write(move |conn| {
            dao::proc::mark_exited(conn, &spawn_id, None, None, at)?;
            dao::proc::mark_state(conn, &spawn_id, ProcState::Exited, Some("reaped"))?;
            Ok(())
        })
        .await;
}

async fn mark_lost(sup: &Supervisor, row: &ProcRow, note: &str) {
    let spawn_id = row.spawn_id.clone();
    let note = note.to_string();
    let _ = sup
        .db()
        .write(move |conn| {
            dao::proc::mark_state(conn, &spawn_id, ProcState::Lost, Some(&note))?;
            Ok(())
        })
        .await;
}

async fn record_result(sup: &Supervisor, row: &ProcRow, report: &KillReport, signal: Option<i32>) {
    let spawn_id = row.spawn_id.clone();
    let complete = report.survivors.is_empty();
    let note = if complete {
        format!("killed ({} targets)", report.requested.len())
    } else {
        format!("kill_incomplete ({} survivors)", report.survivors.len())
    };
    let at = now_ms();
    let _ = sup
        .db()
        .write(move |conn| {
            if complete {
                dao::proc::mark_exited(conn, &spawn_id, None, signal.map(i64::from), at)?;
                dao::proc::mark_state(conn, &spawn_id, ProcState::Killed, Some(&note))?;
            } else {
                dao::proc::mark_state(conn, &spawn_id, ProcState::KillIncomplete, Some(&note))?;
            }
            Ok(())
        })
        .await;
}

/// Every kill — refused, dry or real — leaves a row. A kill with no `event_log`
/// line is an unexplained kill (DESIGN §8-7).
async fn write_event_log(
    sup: &Supervisor,
    row: &ProcRow,
    report: &KillReport,
    per_pid: &[PidResult],
    primary: Option<i32>,
    group: Option<i32>,
) {
    let detail = json!({
        "fence": report.fence,
        "dryRun": report.dry_run,
        "refusedReason": report.refused_reason,
        "primaryPid": primary,
        "pgidSignalled": group,
        "argv": row.argv_vec().join(" "),
        "cwd": row.cwd,
        "kind": row.kind.as_db_str(),
        "vendor": row.vendor,
        "sessionId": row.session_id,
        "runId": row.run_id,
        "requested": report.requested,
        "excluded": report.excluded,
        "escalated": report.escalated,
        "survivors": report.survivors,
        "perPid": per_pid,
    });
    let level = if report.is_refused() || !report.survivors.is_empty() {
        "warn"
    } else {
        "info"
    };
    let message = kill_message(report);
    append_event(sup, level, Some(row.spawn_id.clone()), message, detail).await;
}

async fn write_bare_event_log(
    sup: &Supervisor,
    pid: i32,
    report: &KillReport,
    per_pid: &[PidResult],
) {
    let detail = json!({
        "fence": report.fence,
        "dryRun": report.dry_run,
        "refusedReason": report.refused_reason,
        "pid": pid,
        "requested": report.requested,
        "excluded": report.excluded,
        "escalated": report.escalated,
        "survivors": report.survivors,
        "perPid": per_pid,
    });
    let level = if report.is_refused() || !report.survivors.is_empty() {
        "warn"
    } else {
        "info"
    };
    let message = format!("pid {pid}: {}", kill_message(report));
    append_event(sup, level, None, message, detail).await;
}

fn kill_message(report: &KillReport) -> String {
    match &report.refused_reason {
        Some(reason) => format!("kill refused: {reason}"),
        None if report.dry_run => format!("kill dry run: {} targets", report.requested.len()),
        None => format!(
            "kill: {} targeted, {} excluded, {} escalated, {} survived",
            report.requested.len(),
            report.excluded.len(),
            report.escalated.len(),
            report.survivors.len()
        ),
    }
}

async fn append_event(
    sup: &Supervisor,
    level: &str,
    subject: Option<String>,
    message: String,
    detail: serde_json::Value,
) {
    let level = level.to_string();
    let detail = detail.to_string();
    let at = now_ms();
    if let Err(e) = sup
        .db()
        .write(move |conn| {
            dao::event_log::append(
                conn,
                at,
                "kill",
                &level,
                subject.as_deref(),
                &message,
                Some(&detail),
            )?;
            Ok(())
        })
        .await
    {
        tracing::warn!(target: "supervise", error = %e, "kill: event_log write failed");
    }
}

/// DESIGN §8-4: a refusal warns rather than failing silently.
fn warn_refusal(sup: &Supervisor, spawn_id: &str, reason: &str) {
    tracing::warn!(target: "supervise", spawn_id, reason, "kill refused; no signal sent");
    if let Some(bus) = sup.events() {
        bus.emit(
            events::LOG,
            events::LogEvent {
                level: "warn".into(),
                target: "supervise".into(),
                msg: format!("kill refused for {spawn_id}: {reason}"),
                at: now_ms(),
            },
        );
    }
}

fn invalidate_registry(sup: &Supervisor, spawn_id: &str) {
    if let Some(bus) = sup.events() {
        bus.emit(
            events::PROC_REGISTRY,
            events::ProcRegistryEvent {
                changed_spawn_ids: vec![spawn_id.to_string()],
            },
        );
    }
}

// ---------------------------------------------------------------------------
// unit tests — policy only. The signal-sending proofs live in
// `tests/kill_safety.rs`, where they run in forked processes.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervise::procscan::PsRow;

    /// A pid the kernel currently has no process for, so `uid_of` answers
    /// `None`. Searched from the top of the pid space downward, where the
    /// density of live processes is lowest, and re-checked immediately before
    /// it is handed back.
    fn a_free_pid() -> i32 {
        use crate::supervise::spawn::is_alive;
        // macOS wraps pids at 99999 (`kern.pidmax`); anything above is never
        // assigned, but staying inside the range keeps the test honest about
        // what it is asserting.
        for candidate in (2..=99_999).rev() {
            if !is_alive(candidate) && procscan::uid_of(candidate).is_none() {
                return candidate;
            }
        }
        panic!("every pid on this machine is in use");
    }

    fn ps_row(pid: i32, ppid: i32, pgid: i32, args: &str) -> PsRow {
        PsRow {
            pid,
            ppid,
            pgid,
            rss_kb: 1024,
            cpu_pct: 0.0,
            elapsed_secs: 60,
            user: "tester".into(),
            args: args.to_string(),
        }
    }

    #[test]
    fn kill_opts_round_trip_through_the_wire_shape() {
        let json = r#"{"dryRun":true,"allowBundleMain":false}"#;
        let opts: KillOpts = serde_json::from_str(json).expect("wire opts parse");
        assert!(opts.dry_run);
        assert!(!opts.allow_bundle_main);
        assert_eq!(
            opts.grace, DEFAULT_GRACE,
            "an absent graceMs falls back to the design default"
        );

        let with_grace: KillOpts =
            serde_json::from_str(r#"{"dryRun":false,"allowBundleMain":true,"graceMs":1500}"#)
                .expect("parse");
        assert_eq!(with_grace.grace, Duration::from_millis(1500));
        assert!(with_grace.allow_bundle_main);

        let back = serde_json::to_value(with_grace).expect("serialize");
        assert_eq!(back["graceMs"], 1500);
        assert_eq!(back["allowBundleMain"], true);
    }

    #[test]
    fn kill_target_matches_the_ipc_contract() {
        let spawn = serde_json::to_value(KillTarget::spawn("s-1")).expect("json");
        assert_eq!(spawn["target"], "spawn");
        assert_eq!(spawn["spawnId"], "s-1");

        let pid = serde_json::to_value(KillTarget::pid(42)).expect("json");
        assert_eq!(pid["target"], "pid");
        assert_eq!(pid["pid"], 42);

        let parsed: KillTarget =
            serde_json::from_str(r#"{"target":"pid","pid":7}"#).expect("parse");
        assert_eq!(parsed, KillTarget::pid(7));
    }

    #[test]
    fn a_report_is_never_a_bare_ok() {
        let report = KillReport::refused(Some("s-1".into()), "argv", reason::NOT_FOUND);
        let value = serde_json::to_value(&report).expect("json");
        for key in [
            "spawnId",
            "requested",
            "excluded",
            "survivors",
            "refusedReason",
            "dryRun",
            "escalated",
            "fence",
            "at",
        ] {
            assert!(value.get(key).is_some(), "missing {key} in {value}");
        }
        assert!(report.is_refused());
        assert!(!report.is_complete());
    }

    #[test]
    fn an_exclusion_serializes_as_pid_and_reason() {
        let value = serde_json::to_value(KillExclusion::new(9, reason::ANCESTOR)).expect("json");
        assert_eq!(value["pid"], 9);
        assert_eq!(value["reason"], "ancestor");
    }

    #[test]
    fn the_escalation_gate_drops_a_pid_whose_start_time_moved() {
        let me = std::process::id() as i32;
        let real = procscan::start_time_of(me).expect("our own start time");

        let mut recorded = HashMap::new();
        recorded.insert(me, Some(real));
        let (keep, dropped) = survivors_still_ours(&[me], &recorded);
        assert_eq!(keep, vec![me], "an unchanged start time is still ours");
        assert!(dropped.is_empty());

        let mut recorded = HashMap::new();
        recorded.insert(me, Some(real + 10));
        let (keep, dropped) = survivors_still_ours(&[me], &recorded);
        assert!(keep.is_empty(), "a recycled pid is never escalated");
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].reason, reason::START_TIME_CHANGED);
    }

    #[test]
    fn the_escalation_gate_drops_a_pid_with_no_recorded_start_time() {
        let me = std::process::id() as i32;
        let mut recorded = HashMap::new();
        recorded.insert(me, None);
        let (keep, dropped) = survivors_still_ours(&[me], &recorded);
        assert!(keep.is_empty(), "an unidentifiable pid fails closed");
        assert_eq!(dropped.len(), 1);
    }

    #[test]
    fn our_own_pid_and_ancestors_are_excluded() {
        let snap = ps_snapshot().expect("ps snapshot");
        let guard = SelfGuard::of(&snap);
        let opts = KillOpts::default();

        assert_eq!(
            guard.exclusion_for(guard.pid, &snap, &opts, false),
            Some(reason::SELF)
        );
        assert_eq!(guard.exclusion_for(1, &snap, &opts, false), Some(reason::INIT));
        assert_eq!(guard.exclusion_for(0, &snap, &opts, false), Some(reason::INIT));
        assert_eq!(
            guard.exclusion_for(-5, &snap, &opts, false),
            Some(reason::INIT)
        );

        for ancestor in snap.ancestors_of(guard.pid) {
            if ancestor <= 1 {
                continue;
            }
            assert_eq!(
                guard.exclusion_for(ancestor, &snap, &opts, false),
                Some(reason::ANCESTOR),
                "ancestor {ancestor} must never be a target"
            );
            assert_eq!(
                guard.exclusion_for(ancestor, &snap, &opts, true),
                Some(reason::ANCESTOR),
                "being the row's own pid does not lift ancestor protection"
            );
        }
    }

    #[test]
    fn a_pid_whose_ownership_cannot_be_read_is_refused() {
        // A synthetic bundle main. Its pid does not exist, so the kernel cannot
        // tell us who owns it — and an unreadable owner is never a pass.
        //
        // The pid has to be *found*, not guessed. This test used to use
        // `getpid() + 1`, which is a live process whenever the kernel has just
        // handed that number out — and on a machine running a test suite that
        // spawns children, it usually has. A live neighbour owned by us reads
        // as `uid == ours`, the refusal never fires, and the test fails for a
        // reason that has nothing to do with the code under test.
        let me = std::process::id() as i32;
        let ghost = a_free_pid();
        let snap = Snapshot::from_rows(vec![
            ps_row(me, 1, me, "/usr/bin/true"),
            ps_row(
                ghost,
                me,
                me,
                "/Applications/Some App.app/Contents/MacOS/Some App --flag",
            ),
        ]);
        assert!(
            snap.is_app_bundle_main(ghost),
            "the exclusion set is fed by this classification"
        );
        let guard = SelfGuard::of(&snap);
        assert_eq!(
            guard.exclusion_for(ghost, &snap, &KillOpts::default(), false),
            Some(reason::FOREIGN_UID),
            "an unreadable uid is refused before anything else"
        );
        assert_eq!(
            guard.exclusion_for(ghost, &snap, &KillOpts::default().allow_bundle_main(true), true),
            Some(reason::FOREIGN_UID),
            "allow_bundle_main does not lift any other protection"
        );
    }

    #[test]
    fn a_group_containing_a_protected_pid_is_never_signalled() {
        let me = std::process::id() as i32;
        let snap = Snapshot::from_rows(vec![
            ps_row(me, 1, 900, "/usr/bin/true"),
            ps_row(900, 1, 900, "/bin/sh"),
        ]);
        let guard = SelfGuard::of(&snap);
        assert!(
            !group_is_safe(900, &snap, &guard, &[900]),
            "our own pid is in group 900; it must never be signalled"
        );
    }

    #[test]
    fn an_empty_group_is_not_signalled() {
        let snap = Snapshot::from_rows(vec![ps_row(4242, 1, 4242, "/usr/bin/true")]);
        let guard = SelfGuard::of(&snap);
        assert!(!group_is_safe(9999, &snap, &guard, &[4242]));
    }

    #[test]
    fn a_group_holding_none_of_our_targets_is_not_signalled() {
        let snap = Snapshot::from_rows(vec![ps_row(4242, 1, 4242, "/usr/bin/true")]);
        let guard = SelfGuard::of(&snap);
        assert!(
            !group_is_safe(4242, &snap, &guard, &[]),
            "a pgid that holds no verified target may have been recycled"
        );
    }

    #[test]
    fn is_gone_is_false_for_a_live_process_and_true_for_an_impossible_pid() {
        let me = std::process::id() as i32;
        assert!(!is_gone(me));
        assert!(is_gone(-1));
        assert!(!is_zombie(me));
    }
}
