//! `procmon_*` and `budget_get` — DESIGN §3 `monitor / recovery`, §8-7, §8-8.
//!
//! Six thin commands over the scanner (T07), the kill escalation (T10) and the
//! watchdog (T12). Two of them carry rules that DESIGN insists live **in Rust**
//! rather than in the panel, because a safety rule enforced only by the UI is a
//! safety rule one `invoke()` away from not existing:
//!
//!   * [`procmon_kill`] re-runs the three identity fences **per row, immediately
//!     before the signal** — the scan the user clicked on can be seconds old and
//!     a pid can be recycled in that time — and, when the request covers more
//!     than one row (a bulk action), refuses young processes, `.app` bundle
//!     mains, our own ancestors, other users' processes and unmanaged agent CLIs
//!     with a per-row reason instead of a signal;
//!   * `dryRun` returns exactly the target and exclusion sets the real run would
//!     use and sends nothing, so "what would this do" is answerable without
//!     doing it.
//!
//! Nothing here computes anything the watchdog already knows. [`procmon_scan`]
//! answers from the watchdog's cache while it is fresher than the panel's own
//! poll interval, so a visible panel plus a running watchdog cost one `ps`
//! between them rather than two.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::State;

use crate::error::{GildeError, CmdResult};
use crate::events::{ProcRegistryEvent, PROC_REGISTRY};
use crate::model::graph::PressureLevel;
use crate::store::dao;
use crate::store::models::{ProcKind, ProcRow as RegistryRow};
use crate::supervise::kill::{self, reason, KillOpts, KillOutcome, KillReport, KillTarget};
use crate::supervise::procscan::{self, ps_snapshot, Snapshot};
use crate::supervise::registry::Supervisor;
use crate::supervise::spawn::is_alive;
use crate::supervise::health::HealthSnapshot;
use crate::supervise::watchdog::{BudgetState, Watchdog};
use crate::supervise::{ProcGroupKind, ScanFilter, ScanResult};

/// Managed state: the memory watchdog, which also owns the scan cache.
pub type WatchdogState = Arc<Watchdog>;

/// Managed state: the supervisor. Same alias `commands::recovery` declares —
/// Tauri looks state up by type, so both names must resolve to one type.
pub type SupervisorState = Arc<Supervisor>;

/// A bulk action may not touch a process that no registry row claims and that
/// looks like somebody else's agent CLI. DESIGN §8-7: "`AgentUnmanaged`에는
/// 일괄 액션이 절대 없다" — a second orchestrator on this machine owns those.
pub const REASON_UNMANAGED_BULK: &str = "unmanagedBulk";

/// A health clean-up target that is not (or no longer) a confirmed leftover of
/// the latest health snapshot — or whose pid now names another process.
pub const REASON_NOT_LEFTOVER: &str = "notLeftover";

/// `procmon_kill`'s options: [`KillOpts`] plus the health clean-up switch
/// (`ipc.ts::KillOpts.onlyHealthOrphans`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcmonKillOpts {
    #[serde(flatten)]
    pub opts: KillOpts,
    #[serde(default)]
    pub only_health_orphans: bool,
}

// ---------------------------------------------------------------------------
// wire types
// ---------------------------------------------------------------------------

/// One `proc_registry` row as `ipc.ts::RegistryRow` declares it.
///
/// Not `store::models::ProcRow` itself, for two reasons: `argv` is a JSON string
/// in the column and an array on the wire, and `state` must serialize with the
/// database spellings (`kill_incomplete`), which the row struct's `camelCase`
/// derive would render as `killIncomplete`. The registry's internal columns
/// (`argvSig`, `envMarker`, `procStartTs`) are deliberately not published.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryRowWire {
    pub spawn_id: String,
    pub app_instance: String,
    pub generation: i64,
    pub kind: ProcKind,
    pub vendor: Option<String>,
    pub session_id: Option<String>,
    pub run_id: Option<String>,
    /// `planned` | `running` | `exited` | `killed` | `kill_incomplete` | `lost`
    /// | `adopted` — the database spellings, which are the wire spellings.
    pub state: &'static str,
    pub pid: Option<i64>,
    pub pgid: Option<i64>,
    /// Already masked when the row was written (DESIGN §8-6).
    pub argv: Vec<String>,
    pub cwd: String,
    pub config_dir: Option<String>,
    pub worktree_id: Option<String>,
    pub keep_alive: bool,
    pub planned_at: i64,
    pub started_at: Option<i64>,
    pub exited_at: Option<i64>,
    pub exit_code: Option<i64>,
    pub exit_signal: Option<i64>,
    pub note: Option<String>,
}

impl From<&RegistryRow> for RegistryRowWire {
    fn from(row: &RegistryRow) -> RegistryRowWire {
        RegistryRowWire {
            spawn_id: row.spawn_id.clone(),
            app_instance: row.app_instance.clone(),
            generation: row.generation,
            kind: row.kind,
            vendor: row.vendor.clone(),
            session_id: row.session_id.clone(),
            run_id: row.run_id.clone(),
            state: row.state.as_db_str(),
            pid: row.pid,
            pgid: row.pgid,
            argv: row.argv_vec(),
            cwd: row.cwd.clone(),
            config_dir: row.config_dir.clone(),
            worktree_id: row.worktree_id.clone(),
            keep_alive: row.keep_alive,
            planned_at: row.planned_at,
            started_at: row.started_at,
            exited_at: row.exited_at,
            exit_code: row.exit_code,
            exit_signal: row.exit_signal,
            note: row.note.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

/// The Process Monitor's rows and totals.
///
/// `force` bypasses the watchdog's cache; the panel sets it for an explicit
/// refresh and leaves it off for its polling, which is what keeps two timers
/// from forking two `ps` processes every three seconds.
#[tauri::command]
pub async fn procmon_scan(
    watchdog: State<'_, WatchdogState>,
    filter: ScanFilter,
    force: bool,
) -> CmdResult<ScanResult> {
    Ok(watchdog.inner().scan(&filter, force).await?)
}

/// Resolve one process's working directory.
///
/// Lazy by construction (DESIGN §8-7): one `libproc` call per pid, and this
/// machine can have 800 processes, so the panel asks for the rows it renders.
#[tauri::command]
pub async fn procmon_cwd(pid: i32) -> CmdResult<Option<String>> {
    let cwd = tokio::task::spawn_blocking(move || procscan::cwd_of(pid))
        .await
        .map_err(|err| GildeError::internal(format!("cwd lookup failed: {err}")))?;
    Ok(cwd.map(|p| p.display().to_string()))
}

/// The PID registry as the monitor shows it: every live row on the machine, plus
/// this instance's finished rows, newest first.
#[tauri::command]
pub async fn procmon_registry(
    supervisor: State<'_, SupervisorState>,
) -> CmdResult<Vec<RegistryRowWire>> {
    let instance = supervisor.app_instance_id().to_string();
    let rows = supervisor
        .db()
        .read(move |conn| {
            let mut rows = dao::proc::live_rows(conn, None)?;
            let seen: HashSet<String> = rows.iter().map(|r| r.spawn_id.clone()).collect();
            for row in dao::proc::rows_of_instance(conn, &instance)? {
                if !seen.contains(&row.spawn_id) {
                    rows.push(row);
                }
            }
            Ok(rows)
        })
        .await?;

    let mut wire: Vec<RegistryRowWire> = rows.iter().map(RegistryRowWire::from).collect();
    wire.sort_by(|a, b| b.planned_at.cmp(&a.planned_at).then(a.spawn_id.cmp(&b.spawn_id)));
    Ok(wire)
}

/// Terminate the requested targets, one honest report each.
///
/// Never returns an error for a refusal: "we deliberately declined to signal"
/// and "the operation failed" are different facts and the UI must show which
/// (DESIGN §8-6).
#[tauri::command]
pub async fn procmon_kill(
    supervisor: State<'_, SupervisorState>,
    watchdog: State<'_, WatchdogState>,
    targets: Vec<KillTarget>,
    opts: ProcmonKillOpts,
) -> CmdResult<Vec<KillOutcome>> {
    let sup = supervisor.inner().as_ref();
    let cfg = sup.config().get().supervision;
    let only_orphans = opts.only_health_orphans;
    let opts = with_configured_grace(opts.opts, cfg.kill_grace_ms);
    if only_orphans {
        let orphans = watchdog.inner().health().orphan_identities();
        return Ok(kill_health_orphans(sup, &targets, opts, &orphans).await);
    }
    let groups = if targets.len() > 1 {
        group_index(watchdog.inner(), &targets).await
    } else {
        HashMap::new()
    };
    Ok(kill_targets(sup, &targets, opts, &groups, cfg.min_age_seconds_for_bulk).await)
}

/// The body of [`procmon_kill`], without the Tauri state.
///
/// Separate so the safety rules can be exercised against real processes in
/// `tests/watchdog.rs`: a rule nobody can test is a rule nobody can trust.
pub async fn kill_targets(
    sup: &Supervisor,
    targets: &[KillTarget],
    opts: KillOpts,
    groups: &HashMap<i32, ProcGroupKind>,
    min_age_seconds_for_bulk: u32,
) -> Vec<KillOutcome> {
    if targets.is_empty() {
        return Vec::new();
    }
    // One row is a row the user confirmed individually; more than one is a bulk
    // action, and bulk actions carry the extra refusals of DESIGN §8-7.
    if targets.len() == 1 {
        return kill::kill_pids(sup, targets, opts).await;
    }

    let refusals = bulk_refusals(sup, targets, groups, min_age_seconds_for_bulk).await;

    let allowed: Vec<KillTarget> = targets
        .iter()
        .zip(refusals.iter())
        .filter(|(_, refusal)| refusal.is_none())
        .map(|(target, _)| target.clone())
        .collect();
    let mut executed = kill::kill_pids(sup, &allowed, opts).await.into_iter();

    // Rebuild the answer in the order the caller asked, so row N of the request
    // is row N of the response no matter how many rows were refused.
    let mut out = Vec::with_capacity(targets.len());
    for (target, refusal) in targets.iter().zip(refusals.iter()) {
        match refusal {
            Some(reason) => out.push(KillOutcome {
                target: target.clone(),
                report: KillReport::refused(spawn_id_of(target), "bulk", reason),
            }),
            None => match executed.next() {
                Some(outcome) => out.push(outcome),
                None => out.push(KillOutcome {
                    target: target.clone(),
                    report: KillReport::refused(spawn_id_of(target), "bulk", reason::NO_TARGETS),
                }),
            },
        }
    }
    out
}

/// The health "Clean up" path: only confirmed leftovers, each re-identified.
///
/// Every target must be a pid in `orphans` — the latest health snapshot's
/// `(pid, start time ms)` pairs — whose LIVE kernel start time still matches
/// (±1 s, the identity fence's tolerance): a pid recycled since the snapshot is
/// refused with [`REASON_NOT_LEFTOVER`] and never signalled. The accepted ones
/// go through the ordinary kill path (bulk refusals for init / self /
/// ancestors / bundle mains / other users, then the kill escalation with its
/// own exclusions and fences); the min-age floor and the unmanaged-agent rule
/// are not applied to them, because the two-tick leftover proof is the
/// evidence those two rules stand in for.
pub async fn kill_health_orphans(
    sup: &Supervisor,
    targets: &[KillTarget],
    opts: KillOpts,
    orphans: &[(i32, i64)],
) -> Vec<KillOutcome> {
    let known: HashMap<i32, i64> = orphans.iter().copied().collect();
    let accepted: Vec<bool> = targets
        .iter()
        .map(|target| match target {
            KillTarget::Pid { pid } => known.get(pid).is_some_and(|start_ms| {
                procscan::start_time_of(*pid)
                    .is_some_and(|live_secs| (live_secs - start_ms.div_euclid(1000)).abs() <= 1)
            }),
            KillTarget::Spawn { .. } => false,
        })
        .collect();
    let allowed: Vec<KillTarget> = targets
        .iter()
        .zip(&accepted)
        .filter(|(_, ok)| **ok)
        .map(|(t, _)| t.clone())
        .collect();
    let mut executed = if allowed.is_empty() {
        Vec::new().into_iter()
    } else if allowed.len() == 1 {
        // One row still passes the bulk protections: a leftover is never
        // allowed to be an ancestor, ourselves, a bundle main or a stranger's.
        let refusals = bulk_refusals(sup, &allowed, &HashMap::new(), 0).await;
        match &refusals[0] {
            Some(reason) => vec![KillOutcome {
                target: allowed[0].clone(),
                report: KillReport::refused(None, "bulk", reason),
            }]
            .into_iter(),
            None => kill::kill_pids(sup, &allowed, opts).await.into_iter(),
        }
    } else {
        kill_targets(sup, &allowed, opts, &HashMap::new(), 0).await.into_iter()
    };
    let mut out = Vec::with_capacity(targets.len());
    for (target, ok) in targets.iter().zip(accepted) {
        if ok {
            if let Some(outcome) = executed.next() {
                out.push(outcome);
                continue;
            }
        }
        out.push(KillOutcome {
            target: target.clone(),
            report: KillReport::refused(spawn_id_of(target), "health", REASON_NOT_LEFTOVER),
        });
    }
    out
}

/// The health surface as of the watchdog's last tick (`gilde://health`).
#[tauri::command]
pub async fn health_get(watchdog: State<'_, WatchdogState>) -> CmdResult<HealthSnapshot> {
    Ok(watchdog.inner().health())
}

/// Dismiss one health alert: it stays quiet for ten minutes unless it escalates.
#[tauri::command]
pub async fn health_dismiss(
    watchdog: State<'_, WatchdogState>,
    alert_id: String,
) -> CmdResult<HealthSnapshot> {
    if alert_id.is_empty() || alert_id.len() > 512 {
        return Err(GildeError::refused("alertId").into());
    }
    Ok(watchdog.inner().dismiss_health(&alert_id))
}

/// Mark a registry row as deliberately outliving the app (or stop doing so).
#[tauri::command]
pub async fn procmon_keepalive(
    supervisor: State<'_, SupervisorState>,
    spawn_id: String,
    keep: bool,
) -> CmdResult<()> {
    let id = spawn_id.clone();
    let updated = supervisor
        .db()
        .write(move |conn| dao::proc::set_keep_alive(conn, &id, keep))
        .await?;
    if !updated {
        return Err(GildeError::not_found(spawn_id).into());
    }
    if let Some(bus) = supervisor.events() {
        bus.emit(
            PROC_REGISTRY,
            ProcRegistryEvent {
                changed_spawn_ids: vec![spawn_id],
            },
        );
    }
    Ok(())
}

/// The memory budget, as of the watchdog's last tick.
#[tauri::command]
pub async fn budget_get(watchdog: State<'_, WatchdogState>) -> CmdResult<BudgetState> {
    Ok(watchdog.inner().budget())
}

// ---------------------------------------------------------------------------
// the bulk gate
// ---------------------------------------------------------------------------

/// `graceMs` is optional on the wire and deserializes to [`kill::DEFAULT_GRACE`]
/// when absent. A caller that did not ask for a specific grace gets the one the
/// user configured.
fn with_configured_grace(opts: KillOpts, kill_grace_ms: u64) -> KillOpts {
    if opts.grace == kill::DEFAULT_GRACE {
        return opts.grace(Duration::from_millis(kill_grace_ms));
    }
    opts
}

fn spawn_id_of(target: &KillTarget) -> Option<String> {
    match target {
        KillTarget::Spawn { spawn_id } => Some(spawn_id.clone()),
        KillTarget::Pid { .. } => None,
    }
}

/// pid → classification, from the watchdog's scan. Empty when no scan is
/// available, which downgrades only the "unmanaged agent" rule; every other
/// refusal below is computed from a fresh `ps` snapshot.
async fn group_index(
    watchdog: &Arc<Watchdog>,
    targets: &[KillTarget],
) -> HashMap<i32, ProcGroupKind> {
    if !targets.iter().any(|t| matches!(t, KillTarget::Pid { .. })) {
        return HashMap::new();
    }
    match watchdog.scan(&ScanFilter::default(), false).await {
        Ok(scan) => scan.rows.iter().map(|r| (r.pid, r.group)).collect(),
        Err(err) => {
            tracing::warn!(target: "supervise", error = %err, "bulk kill has no classification");
            HashMap::new()
        }
    }
}

/// One refusal reason per target, `None` for the ones that may proceed.
///
/// The rules are DESIGN §8-7's, and they are applied **before** any signal, so a
/// bulk action that contains one protected row does not half-execute.
async fn bulk_refusals(
    sup: &Supervisor,
    targets: &[KillTarget],
    groups: &HashMap<i32, ProcGroupKind>,
    min_age_seconds: u32,
) -> Vec<Option<String>> {
    let snapshot = match tokio::task::spawn_blocking(ps_snapshot).await {
        Ok(Ok(snapshot)) => snapshot,
        Ok(Err(err)) => {
            tracing::warn!(target: "supervise", error = %err, "bulk kill has no process snapshot");
            return vec![Some(reason::SCAN_UNAVAILABLE.to_string()); targets.len()];
        }
        Err(err) => {
            tracing::warn!(target: "supervise", error = %err, "bulk kill snapshot task failed");
            return vec![Some(reason::SCAN_UNAVAILABLE.to_string()); targets.len()];
        }
    };

    let self_pid = std::process::id() as i32;
    let guard = BulkGuard {
        self_pid,
        our_uid: procscan::uid_of(self_pid),
        ancestors: snapshot.ancestors_of(self_pid).into_iter().collect(),
        min_age: Duration::from_secs(min_age_seconds as u64),
    };

    let mut out = Vec::with_capacity(targets.len());
    for target in targets {
        let pid = match target {
            KillTarget::Pid { pid } => Some(*pid),
            KillTarget::Spawn { spawn_id } => sup
                .row(spawn_id)
                .await
                .ok()
                .flatten()
                .and_then(|row| row.pid)
                .map(|p| p as i32),
        };
        // No pid to judge: `kill_pids` refuses it with the precise reason.
        let Some(pid) = pid.filter(|p| *p > 0) else {
            out.push(None);
            continue;
        };
        out.push(guard.refuse(
            pid,
            &snapshot,
            snapshot.uid_of(pid),
            groups.get(&pid).copied(),
        ));
    }
    out
}

/// What every target in one bulk request is judged against. Computed once, and
/// deliberately holding no syscall of its own so the rules are testable.
struct BulkGuard {
    self_pid: i32,
    our_uid: Option<u32>,
    ancestors: HashSet<i32>,
    min_age: Duration,
}

impl BulkGuard {
    /// The bulk rules of DESIGN §8-7, in the order that fails safest first.
    fn refuse(
        &self,
        pid: i32,
        snapshot: &Snapshot,
        their_uid: Option<u32>,
        group: Option<ProcGroupKind>,
    ) -> Option<String> {
        if pid <= 1 {
            return Some(reason::INIT.to_string());
        }
        if pid == self.self_pid {
            return Some(reason::SELF.to_string());
        }
        if self.ancestors.contains(&pid) {
            return Some(reason::ANCESTOR.to_string());
        }
        // Not in the snapshot at all: either it is already gone, in which case
        // the identity fence should be the one to say so — a row refused here as
        // "somebody else's" would be a lie about a process that no longer exists
        // — or it started *after* the snapshot, which no bulk action can have
        // meant, and the age floor is exactly the rule for that.
        if !snapshot.contains(pid) {
            return if !self.min_age.is_zero() && is_alive(pid) {
                Some(reason::TOO_YOUNG.to_string())
            } else {
                None
            };
        }
        if snapshot.is_app_bundle_main(pid) {
            return Some(reason::APP_BUNDLE_MAIN.to_string());
        }
        // An owner we cannot read is somebody else's until proven otherwise.
        match (self.our_uid, their_uid) {
            (Some(ours), Some(theirs)) if ours == theirs => {}
            (None, _) => {}
            _ => return Some(reason::FOREIGN_UID.to_string()),
        }
        // A process younger than the bulk floor appeared after the user looked
        // at the list they are acting on — it cannot be a row they meant.
        if !self.min_age.is_zero() {
            match snapshot.age_of(pid) {
                Some(age) if age >= self.min_age => {}
                _ => return Some(reason::TOO_YOUNG.to_string()),
            }
        }
        if group == Some(ProcGroupKind::AgentUnmanaged) {
            return Some(REASON_UNMANAGED_BULK.to_string());
        }
        None
    }
}

// ---------------------------------------------------------------------------
// diagnostics
// ---------------------------------------------------------------------------

/// The memory pressure source `app_diagnostics()` reports (DESIGN §10-6).
/// Exposed here so T29 does not have to reach into `supervise::watchdog`.
pub fn pressure_source(watchdog: &Watchdog) -> (PressureLevel, &'static str) {
    let (level, source) = watchdog.pressure();
    (level, source.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::models::ProcState;
    use crate::supervise::procscan::PsRow;

    fn ps(pid: i32, ppid: i32, elapsed: u64, args: &str) -> PsRow {
        PsRow {
            pid,
            ppid,
            pgid: pid,
            rss_kb: 1,
            cpu_pct: 0.0,
            elapsed_secs: elapsed,
            user: "tester".into(),
            args: args.to_string(),
        }
    }

    fn snapshot() -> Snapshot {
        Snapshot::from_rows(vec![
            ps(1, 0, 10_000, "/sbin/launchd"),
            ps(50, 1, 9_000, "/Applications/Some.app/Contents/MacOS/Some"),
            ps(60, 50, 8_000, "/usr/bin/parent-of-us"),
            ps(70, 60, 500, "/usr/local/bin/claude"),
            ps(80, 1, 5, "/usr/local/bin/codex"),
        ])
    }

    const OUR_UID: u32 = 501;

    /// The guard as it looks for a bulk request from a process whose ancestor
    /// chain is 70 → 60 → 50 → 1.
    fn guard(ancestors: &[i32]) -> BulkGuard {
        BulkGuard {
            self_pid: 70,
            our_uid: Some(OUR_UID),
            ancestors: ancestors.iter().copied().collect(),
            min_age: Duration::from_secs(120),
        }
    }

    #[test]
    fn a_bulk_action_refuses_pid_one_ourselves_and_our_ancestors() {
        let guard = guard(&[60, 50, 1]);
        let snapshot = snapshot();
        for (pid, expected) in [
            (1, reason::INIT),
            (70, reason::SELF),
            (60, reason::ANCESTOR),
        ] {
            assert_eq!(
                guard
                    .refuse(pid, &snapshot, Some(OUR_UID), None)
                    .as_deref(),
                Some(expected),
                "pid {pid}"
            );
        }
    }

    #[test]
    fn a_bulk_action_refuses_a_foreground_application() {
        // 50 is a bundle main; take it out of the ancestor chain so the bundle
        // rule is the one under test.
        let refused = guard(&[]).refuse(50, &snapshot(), Some(OUR_UID), None);
        assert_eq!(refused.as_deref(), Some(reason::APP_BUNDLE_MAIN));
    }

    #[test]
    fn a_bulk_action_refuses_a_process_younger_than_the_floor() {
        let snapshot = snapshot();
        // pid 80 has been alive for five seconds; the floor is 120.
        let refused = guard(&[]).refuse(80, &snapshot, Some(OUR_UID), None);
        assert_eq!(refused.as_deref(), Some(reason::TOO_YOUNG));

        // …and an old enough one goes through.
        let allowed = guard(&[]).refuse(
            60,
            &snapshot,
            Some(OUR_UID),
            Some(ProcGroupKind::OursOrphan),
        );
        assert_eq!(allowed, None);
    }

    #[test]
    fn a_bulk_action_never_touches_an_unmanaged_agent_cli() {
        let refused = guard(&[]).refuse(
            60,
            &snapshot(),
            Some(OUR_UID),
            Some(ProcGroupKind::AgentUnmanaged),
        );
        assert_eq!(refused.as_deref(), Some(REASON_UNMANAGED_BULK));
    }

    #[test]
    fn another_users_process_and_an_unreadable_owner_are_both_refused() {
        let snapshot = snapshot();
        assert_eq!(
            guard(&[]).refuse(60, &snapshot, Some(0), None).as_deref(),
            Some(reason::FOREIGN_UID),
            "root's process is not ours to end"
        );
        assert_eq!(
            guard(&[]).refuse(60, &snapshot, None, None).as_deref(),
            Some(reason::FOREIGN_UID),
            "an owner we cannot read must fail closed"
        );
    }

    #[test]
    fn a_zero_floor_disables_only_the_age_rule() {
        let mut guard = guard(&[]);
        guard.min_age = Duration::ZERO;
        assert_eq!(guard.refuse(80, &snapshot(), Some(OUR_UID), None), None);
        assert_eq!(
            guard.refuse(1, &snapshot(), Some(OUR_UID), None).as_deref(),
            Some(reason::INIT)
        );
    }

    #[test]
    fn the_configured_grace_fills_in_only_when_the_caller_did_not_ask() {
        let default = with_configured_grace(KillOpts::default(), 9_000);
        assert_eq!(default.grace, Duration::from_millis(9_000));

        let explicit = with_configured_grace(
            KillOpts::default().grace(Duration::from_millis(250)),
            9_000,
        );
        assert_eq!(explicit.grace, Duration::from_millis(250));
    }

    #[test]
    fn the_registry_row_wire_shape_matches_ipc_ts() {
        let row = RegistryRow {
            spawn_id: "sp-1".into(),
            app_instance: "inst-1".into(),
            generation: 3,
            kind: ProcKind::Pty,
            vendor: Some("claude".into()),
            session_id: Some("sess-1".into()),
            run_id: None,
            state: ProcState::KillIncomplete,
            pid: Some(4321),
            pgid: Some(4321),
            proc_start_ts: Some(1_700_000_000),
            argv: "[\"/usr/local/bin/claude\",\"--\",\"hi\"]".into(),
            argv_sig: "sig".into(),
            cwd: "/tmp".into(),
            env_marker: "sp-1".into(),
            config_dir: None,
            worktree_id: Some("wt-1".into()),
            keep_alive: true,
            planned_at: 10,
            started_at: Some(11),
            exited_at: None,
            exit_code: None,
            exit_signal: None,
            note: Some("n".into()),
        };
        let value = serde_json::to_value(RegistryRowWire::from(&row)).expect("json");

        assert_eq!(value["state"], serde_json::json!("kill_incomplete"));
        assert_eq!(
            value["argv"],
            serde_json::json!(["/usr/local/bin/claude", "--", "hi"])
        );
        assert_eq!(value["keepAlive"], serde_json::json!(true));
        assert_eq!(value["spawnId"], serde_json::json!("sp-1"));
        assert!(
            value.get("argvSig").is_none() && value.get("envMarker").is_none(),
            "registry internals must not leave the process"
        );
        assert_eq!(value.as_object().expect("object").len(), 21);
    }

    #[test]
    fn a_malformed_argv_column_still_produces_a_row() {
        let mut row = RegistryRow {
            spawn_id: "sp-1".into(),
            app_instance: "inst-1".into(),
            generation: 0,
            kind: ProcKind::Git,
            vendor: None,
            session_id: None,
            run_id: None,
            state: ProcState::Exited,
            pid: None,
            pgid: None,
            proc_start_ts: None,
            argv: "not json".into(),
            argv_sig: String::new(),
            cwd: "/".into(),
            env_marker: String::new(),
            config_dir: None,
            worktree_id: None,
            keep_alive: false,
            planned_at: 0,
            started_at: None,
            exited_at: None,
            exit_code: None,
            exit_signal: None,
            note: None,
        };
        assert!(RegistryRowWire::from(&row).argv.is_empty());
        row.argv = "[\"git\",\"status\"]".into();
        assert_eq!(RegistryRowWire::from(&row).argv.len(), 2);
    }
}
