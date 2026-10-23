//! The three identity fences (DESIGN §8-4).
//!
//! **Nothing in this codebase may signal a process without a [`Fence::Verified`]
//! from here.** Killing the wrong process is the worst thing this app can do,
//! and a recycled pid is not a hypothetical: macOS wraps pids in the low tens of
//! thousands, and an agent CLI that runs for an hour is exactly the workload
//! that outlives a pid generation.
//!
//! The policy, verbatim from DESIGN §8-4:
//!
//! > **Fence 1 must pass, and (fence 2 ∨ fence 3) must pass.** Everything else
//! > marks the row `lost` with a reason, emits a warning, and sends **no
//! > signal at all.**
//!
//! | # | fence | source | what it catches |
//! |---|-------|--------|-----------------|
//! | 1 | kernel start time ±1 s | `libproc` `pbi_start_tvsec`, cross-checked by `sysinfo` | pid reuse — a recycled pid always starts *later* than the row that recorded it |
//! | 2 | argv signature | `sysinfo::Process::cmd()` (`KERN_PROCARGS2`), falling back to `ps -o args=` | a different program that happens to hold our pid |
//! | 3 | `GILDE_SPAWN_ID` env marker | `sysinfo::Process::environ()` | our own child that `exec`'d itself into a different argv |
//!
//! Fences 2 and 3 are an **or** rather than an **and** on purpose: each one is
//! independently unavailable in practice. Reading another process's environment
//! block is progressively restricted on macOS and fails routinely for signed and
//! hardened binaries even at the same uid (DESIGN §8-4 "알려진 한계"), and a
//! process is free to rewrite its own argv. Requiring both would turn a normal
//! platform limitation into "we can never clean anything up"; requiring either
//! keeps the check alive when one source goes dark.
//!
//! Every unknown answer fails **closed**: an unreadable fence is not a pass.

use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::store::models::{argv_signature, ProcRow};
use crate::supervise::procscan::{self, Snapshot};
use crate::supervise::spawn::is_alive;

/// Environment variable carrying the registry row id of a child we started.
pub const ENV_SPAWN_ID: &str = "GILDE_SPAWN_ID";

/// Kernel start times are recorded and compared in **seconds**; a one second
/// window absorbs the rounding between `libproc`'s `pbi_start_tvsec` and
/// `sysinfo`'s `start_time()` without letting a recycled pid through — a pid can
/// only be reused after the original exited, which on a real machine is orders
/// of magnitude further away than a second.
pub const START_TIME_TOLERANCE_SECS: i64 = 1;

/// Wider window for the degraded path where the only start time available comes
/// from `ps`'s whole-second `etime` arithmetic rather than the kernel.
const START_TIME_TOLERANCE_SECS_DEGRADED: i64 = 2;

// ---------------------------------------------------------------------------
// verdict
// ---------------------------------------------------------------------------

/// Why a live process is *not* the one a registry row describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MismatchReason {
    /// Fence 1: the kernel start time is not the one we recorded — almost
    /// always pid reuse.
    StartTime,
    /// Fence 2 failed and fence 3 could not save it.
    Argv,
    /// Fence 3 read an `GILDE_SPAWN_ID` belonging to a different row, and fence 2
    /// was unreadable.
    EnvMarker,
    /// The pid belongs to another user. Never ours, never signalled.
    Uid,
    /// The row never recorded a pid (a `planned` row that never started).
    NoPid,
}

impl MismatchReason {
    /// Stable identifier for logs and the `note` column.
    pub const fn as_str(self) -> &'static str {
        match self {
            MismatchReason::StartTime => "startTime",
            MismatchReason::Argv => "argv",
            MismatchReason::EnvMarker => "envMarker",
            MismatchReason::Uid => "uid",
            MismatchReason::NoPid => "noPid",
        }
    }
}

/// The verdict on one registry row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Fence {
    /// The live process at `row.pid` is provably the one this row describes.
    /// **The only value that authorises a signal.**
    Verified,
    /// Something lives at `row.pid` but it is not ours — mark the row `lost`,
    /// warn, and do nothing else.
    Mismatch(MismatchReason),
    /// Nothing is there any more. The row is finished (`exited`, note
    /// `reaped`); no signal is needed and none is sent.
    Gone,
}

impl Fence {
    /// Whether a signal may be sent. Written as a method so that no call site
    /// can accidentally match `Mismatch(_)` as "close enough".
    pub const fn may_signal(self) -> bool {
        matches!(self, Fence::Verified)
    }

    pub const fn is_gone(self) -> bool {
        matches!(self, Fence::Gone)
    }

    /// Short reason for the `note` column of a row we refuse to touch.
    pub const fn note(self) -> &'static str {
        match self {
            Fence::Verified => "verified",
            Fence::Gone => "gone",
            Fence::Mismatch(r) => r.as_str(),
        }
    }
}

// ---------------------------------------------------------------------------
// argv signature
// ---------------------------------------------------------------------------

/// `sha256(cmd || '\0' || args.join('\0'))` — DESIGN §8-2 / §8-4 fence 2.
///
/// Exact rather than heuristic, because **every** execution in this app uses an
/// absolute binary path. That is the improvement over cco's
/// `args.includes('claude') && args.includes(' -p')`.
pub fn argv_sig(cmd: &str, args: &[String]) -> String {
    let mut all: Vec<&str> = Vec::with_capacity(args.len() + 1);
    all.push(cmd);
    all.extend(args.iter().map(String::as_str));
    argv_signature(&all)
}

/// Signature of a whole argv vector (`argv[0]` included).
pub fn argv_sig_of(argv: &[String]) -> String {
    argv_signature(argv)
}

// ---------------------------------------------------------------------------
// probe seam
// ---------------------------------------------------------------------------

/// Everything [`verify_with`] needs to know about a live process.
///
/// A trait, not free functions, so the fence policy can be tested against
/// *every* combination of readable and unreadable sources — including the ones
/// this machine will not reproduce on demand (an unreadable environment block,
/// a foreign uid, a stale snapshot).
///
/// Implementations return `None` for "could not read", which is different from
/// `Some(empty)` and is never treated as a pass.
pub trait ProcProbe {
    fn is_alive(&self, pid: i32) -> bool;
    /// Kernel start time in seconds.
    fn start_time(&self, pid: i32) -> Option<i64>;
    /// Degraded start time (whole-second `ps` arithmetic), used only when
    /// [`ProcProbe::start_time`] is unavailable.
    fn start_time_degraded(&self, pid: i32) -> Option<i64> {
        let _ = pid;
        None
    }
    fn uid(&self, pid: i32) -> Option<u32>;
    /// Login name of the process owner — the fallback when `uid` is unreadable.
    fn user_name(&self, pid: i32) -> Option<String> {
        let _ = pid;
        None
    }
    /// Exact argv of the live process, `argv[0]` first.
    fn argv(&self, pid: i32) -> Option<Vec<String>>;
    /// Whitespace-joined command line — the degraded form `ps` gives us.
    fn args_string(&self, pid: i32) -> Option<String>;
    /// `KEY=VALUE` entries, or `None` when the block could not be read.
    fn environ(&self, pid: i32) -> Option<Vec<String>>;
    /// Whether the environment holds exactly `key=value`: `None` when the
    /// block could not be read (or was empty). The live probe compares in
    /// place without copying any value (contract §5.2.2, §9.7).
    fn env_entry(&self, pid: i32, key: &str, value: &str) -> Option<bool> {
        let environ = self.environ(pid)?;
        if environ.is_empty() {
            return None;
        }
        let needle = format!("{key}={value}");
        Some(environ.iter().any(|e| e == &needle))
    }
    fn our_uid(&self) -> Option<u32>;
    fn our_user_name(&self) -> Option<String> {
        std::env::var("USER").ok()
    }
}

/// The production probe: a `ps` snapshot for cheap answers, `libproc` and
/// `sysinfo` for the authoritative ones.
pub struct LiveProbe<'a> {
    snap: &'a Snapshot,
    our_uid: Option<u32>,
}

impl<'a> LiveProbe<'a> {
    pub fn new(snap: &'a Snapshot) -> LiveProbe<'a> {
        LiveProbe {
            snap,
            our_uid: procscan::uid_of(std::process::id() as i32),
        }
    }
}

impl ProcProbe for LiveProbe<'_> {
    fn is_alive(&self, pid: i32) -> bool {
        // `kill(pid, 0)` is authoritative and cheap; the snapshot is only a
        // corroborating second opinion for the window between the two.
        is_alive(pid) || self.snap.contains(pid)
    }

    fn start_time(&self, pid: i32) -> Option<i64> {
        procscan::start_time_of(pid)
    }

    fn start_time_degraded(&self, pid: i32) -> Option<i64> {
        let row = self.snap.get(pid)?;
        Some(self.snap.at() / 1000 - row.elapsed_secs as i64)
    }

    fn uid(&self, pid: i32) -> Option<u32> {
        procscan::uid_of(pid)
    }

    fn user_name(&self, pid: i32) -> Option<String> {
        self.snap.get(pid).map(|r| r.user.clone())
    }

    fn argv(&self, pid: i32) -> Option<Vec<String>> {
        live_argv(pid)
    }

    fn args_string(&self, pid: i32) -> Option<String> {
        // Re-read rather than trusting the snapshot: it can be seconds old and
        // this string is the last thing standing between us and a signal.
        procscan::ps_args_of(pid).or_else(|| self.snap.get(pid).map(|r| r.args.clone()))
    }

    fn environ(&self, pid: i32) -> Option<Vec<String>> {
        procscan::environ_of(pid)
    }

    fn env_entry(&self, pid: i32, key: &str, value: &str) -> Option<bool> {
        procscan::env_entry_matches(pid, key, value)
    }

    fn our_uid(&self) -> Option<u32> {
        self.our_uid
    }
}

/// Exact argv of a live process, `argv[0]` first.
///
/// `sysinfo` reads `KERN_PROCARGS2` on macOS, which preserves the argument
/// boundaries — the reason fence 2 can be an exact hash comparison instead of
/// substring matching on a space-joined string.
pub fn live_argv(pid: i32) -> Option<Vec<String>> {
    if pid <= 0 {
        return None;
    }
    let spid = Pid::from_u32(pid as u32);
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[spid]),
        false,
        ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always),
    );
    let process = system.process(spid)?;
    let argv: Vec<String> = process
        .cmd()
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    if argv.is_empty() {
        None
    } else {
        Some(argv)
    }
}

// ---------------------------------------------------------------------------
// verify
// ---------------------------------------------------------------------------

/// Run the three fences against a registry row (DESIGN §8-4).
pub fn verify(row: &ProcRow, snap: &Snapshot) -> Fence {
    verify_with(row, &LiveProbe::new(snap))
}

/// The fence policy itself, against any [`ProcProbe`].
pub fn verify_with<P: ProcProbe + ?Sized>(row: &ProcRow, probe: &P) -> Fence {
    // 0. A row with no pid never had a process to check.
    let Some(pid) = row.pid.filter(|p| *p > 0).map(|p| p as i32) else {
        return Fence::Mismatch(MismatchReason::NoPid);
    };

    // 1. Liveness. `Gone` closes the row without a signal.
    if !probe.is_alive(pid) {
        return Fence::Gone;
    }

    // 2. Ownership. Checked before the start time so that a foreign process
    //    reports the reason that actually matters to the operator.
    if owner_matches(pid, probe) == Some(false) {
        return Fence::Mismatch(MismatchReason::Uid);
    }

    // 3. Fence 1 — kernel start time. Mandatory, no exceptions.
    if !start_time_matches(row, pid, probe) {
        return Fence::Mismatch(MismatchReason::StartTime);
    }

    // 4. Fences 2 and 3 — either one suffices, neither may be assumed.
    let by_argv = argv_matches(row, pid, probe);
    let by_env = env_marker_matches(row, pid, probe);

    match (by_argv, by_env) {
        (Some(true), _) | (_, Some(true)) => Fence::Verified,
        (Some(false), _) => Fence::Mismatch(MismatchReason::Argv),
        (None, Some(false)) => Fence::Mismatch(MismatchReason::EnvMarker),
        // Neither fence could be read: fail closed. A process we cannot
        // identify is a process we do not touch.
        (None, None) => Fence::Mismatch(MismatchReason::Argv),
    }
}

/// `Some(false)` only when we can prove the process belongs to somebody else.
fn owner_matches<P: ProcProbe + ?Sized>(pid: i32, probe: &P) -> Option<bool> {
    if let (Some(theirs), Some(ours)) = (probe.uid(pid), probe.our_uid()) {
        return Some(theirs == ours);
    }
    match (probe.user_name(pid), probe.our_user_name()) {
        (Some(theirs), Some(ours)) if !theirs.is_empty() && !ours.is_empty() => {
            Some(theirs == ours)
        }
        _ => None,
    }
}

/// Fence 1. A row without a recorded start time can never pass it — that is the
/// point: the reaper adopts such rows through `planned`-row adoption, which
/// fills the column in, rather than by guessing here.
fn start_time_matches<P: ProcProbe + ?Sized>(row: &ProcRow, pid: i32, probe: &P) -> bool {
    let Some(recorded) = row.proc_start_ts else {
        return false;
    };
    if let Some(live) = probe.start_time(pid) {
        return (live - recorded).abs() <= START_TIME_TOLERANCE_SECS;
    }
    match probe.start_time_degraded(pid) {
        Some(live) => (live - recorded).abs() <= START_TIME_TOLERANCE_SECS_DEGRADED,
        None => false,
    }
}

/// Fence 2. `None` ⇒ the live argv could not be read at all.
fn argv_matches<P: ProcProbe + ?Sized>(row: &ProcRow, pid: i32, probe: &P) -> Option<bool> {
    let recorded = row.argv_vec();

    if let Some(live) = probe.argv(pid).filter(|a| !a.is_empty()) {
        if argv_sig_of(&live) == row.argv_sig {
            return Some(true);
        }
        // Relaxed fallback for rows migrated from the old schema, and for rows
        // whose stored argv was masked before the signature existed: the
        // command must be identical and at least one distinctive argument must
        // still be present. Fence 1 has already passed at this point, so this
        // can never stand alone.
        return Some(relaxed_match(&recorded, &live));
    }

    // Degraded source: `ps` hands back one space-joined string, so argument
    // boundaries are gone and an exact hash is impossible.
    let live = probe.args_string(pid)?;
    let live = live.trim();
    if live.is_empty() {
        return None;
    }
    if recorded.is_empty() {
        return None;
    }
    let joined = recorded.join(" ");
    if joined == live {
        return Some(true);
    }
    let live_tokens: Vec<String> = live.split_whitespace().map(str::to_string).collect();
    Some(relaxed_match(&recorded, &live_tokens))
}

/// `argv[0]` identical **and** at least one distinctive (non-flag, non-empty)
/// recorded argument still present in the live command line.
fn relaxed_match(recorded: &[String], live: &[String]) -> bool {
    let (Some(want0), Some(got0)) = (recorded.first(), live.first()) else {
        return false;
    };
    if want0 != got0 {
        return false;
    }
    let distinctive: Vec<&String> = recorded
        .iter()
        .skip(1)
        .filter(|a| !a.is_empty() && !a.starts_with('-'))
        .collect();
    if distinctive.is_empty() {
        // Nothing but flags was recorded; `argv[0]` alone is not enough to
        // claim a match, so this fence abstains rather than passing.
        return false;
    }
    distinctive.iter().any(|a| live.iter().any(|l| l == *a))
}

/// Fence 3. `None` ⇒ the environment block could not be read, which DESIGN
/// §8-4 explicitly calls a normal outcome on macOS.
fn env_marker_matches<P: ProcProbe + ?Sized>(row: &ProcRow, pid: i32, probe: &P) -> Option<bool> {
    if row.env_marker.is_empty() {
        return None;
    }
    // `Some(false)`: the block was readable and either carries somebody else's
    // marker or none — both are "not proven ours" by this fence.
    probe.env_entry(pid, ENV_SPAWN_ID, &row.env_marker)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::models::{ProcKind, ProcState};

    #[derive(Default)]
    struct FakeProbe {
        alive: bool,
        start_time: Option<i64>,
        start_time_degraded: Option<i64>,
        uid: Option<u32>,
        our_uid: Option<u32>,
        argv: Option<Vec<String>>,
        args_string: Option<String>,
        environ: Option<Vec<String>>,
    }

    impl ProcProbe for FakeProbe {
        fn is_alive(&self, _pid: i32) -> bool {
            self.alive
        }
        fn start_time(&self, _pid: i32) -> Option<i64> {
            self.start_time
        }
        fn start_time_degraded(&self, _pid: i32) -> Option<i64> {
            self.start_time_degraded
        }
        fn uid(&self, _pid: i32) -> Option<u32> {
            self.uid
        }
        fn argv(&self, _pid: i32) -> Option<Vec<String>> {
            self.argv.clone()
        }
        fn args_string(&self, _pid: i32) -> Option<String> {
            self.args_string.clone()
        }
        fn environ(&self, _pid: i32) -> Option<Vec<String>> {
            self.environ.clone()
        }
        fn our_uid(&self) -> Option<u32> {
            self.our_uid
        }
        fn our_user_name(&self) -> Option<String> {
            None
        }
    }

    fn argv_of(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|p| (*p).to_string()).collect()
    }

    fn row(argv: &[&str]) -> ProcRow {
        let argv = argv_of(argv);
        ProcRow {
            spawn_id: "spawn-1".into(),
            app_instance: "inst-1".into(),
            generation: 0,
            kind: ProcKind::Pty,
            vendor: Some("claude".into()),
            session_id: None,
            run_id: None,
            state: ProcState::Running,
            pid: Some(4242),
            pgid: Some(4242),
            proc_start_ts: Some(1_700_000_000),
            argv_sig: argv_sig_of(&argv),
            argv: serde_json::to_string(&argv).expect("json"),
            cwd: "/tmp".into(),
            env_marker: "spawn-1".into(),
            config_dir: None,
            worktree_id: None,
            keep_alive: false,
            planned_at: 1,
            started_at: Some(2),
            exited_at: None,
            exit_code: None,
            exit_signal: None,
            note: None,
        }
    }

    fn probe_for(row: &ProcRow) -> FakeProbe {
        FakeProbe {
            alive: true,
            start_time: row.proc_start_ts,
            uid: Some(501),
            our_uid: Some(501),
            argv: Some(row.argv_vec()),
            args_string: Some(row.argv_vec().join(" ")),
            environ: Some(vec![format!("GILDE_SPAWN_ID={}", row.env_marker)]),
            ..Default::default()
        }
    }

    #[test]
    fn argv_sig_hashes_command_and_arguments_with_nul_separators() {
        let sig = argv_sig("/bin/echo", &argv_of(&["a", "b"]));
        assert_eq!(sig, argv_sig_of(&argv_of(&["/bin/echo", "a", "b"])));
        assert_ne!(
            sig,
            argv_sig_of(&argv_of(&["/bin/echo", "a b"])),
            "argument boundaries must change the signature"
        );
        assert_eq!(sig.len(), 64, "sha256 hex");
    }

    #[test]
    fn everything_readable_and_matching_verifies() {
        let row = row(&["/bin/agent", "hang"]);
        assert_eq!(verify_with(&row, &probe_for(&row)), Fence::Verified);
    }

    #[test]
    fn a_row_without_a_pid_is_a_mismatch_not_a_pass() {
        let mut row = row(&["/bin/agent", "hang"]);
        row.pid = None;
        assert_eq!(
            verify_with(&row, &probe_for(&row)),
            Fence::Mismatch(MismatchReason::NoPid)
        );
        row.pid = Some(0);
        assert_eq!(
            verify_with(&row, &probe_for(&row)),
            Fence::Mismatch(MismatchReason::NoPid)
        );
    }

    #[test]
    fn a_dead_pid_is_gone() {
        let row = row(&["/bin/agent", "hang"]);
        let probe = FakeProbe {
            alive: false,
            ..probe_for(&row)
        };
        assert_eq!(verify_with(&row, &probe), Fence::Gone);
    }

    #[test]
    fn a_foreign_uid_is_reported_as_uid_even_when_everything_else_matches() {
        let row = row(&["/bin/agent", "hang"]);
        let probe = FakeProbe {
            uid: Some(0),
            ..probe_for(&row)
        };
        assert_eq!(
            verify_with(&row, &probe),
            Fence::Mismatch(MismatchReason::Uid)
        );
    }

    #[test]
    fn a_start_time_outside_the_window_fails_even_with_both_other_fences_passing() {
        let row = row(&["/bin/agent", "hang"]);
        let probe = FakeProbe {
            start_time: row.proc_start_ts.map(|t| t + 10),
            ..probe_for(&row)
        };
        assert_eq!(
            verify_with(&row, &probe),
            Fence::Mismatch(MismatchReason::StartTime)
        );
    }

    #[test]
    fn a_start_time_inside_the_window_passes() {
        let row = row(&["/bin/agent", "hang"]);
        for delta in [-1, 0, 1] {
            let probe = FakeProbe {
                start_time: row.proc_start_ts.map(|t| t + delta),
                ..probe_for(&row)
            };
            assert_eq!(verify_with(&row, &probe), Fence::Verified, "delta {delta}");
        }
    }

    #[test]
    fn a_row_with_no_recorded_start_time_can_never_verify() {
        let mut row = row(&["/bin/agent", "hang"]);
        row.proc_start_ts = None;
        let probe = FakeProbe {
            start_time: Some(1_700_000_000),
            ..probe_for(&row)
        };
        assert_eq!(
            verify_with(&row, &probe),
            Fence::Mismatch(MismatchReason::StartTime)
        );
    }

    #[test]
    fn the_argv_fence_carries_a_process_with_an_unreadable_environment() {
        let row = row(&["/bin/agent", "hang"]);
        for environ in [None, Some(Vec::new())] {
            let probe = FakeProbe {
                environ,
                ..probe_for(&row)
            };
            assert_eq!(verify_with(&row, &probe), Fence::Verified);
        }
    }

    #[test]
    fn the_argv_fence_carries_a_process_that_cleared_our_marker() {
        let row = row(&["/bin/agent", "hang"]);
        let probe = FakeProbe {
            environ: Some(vec!["PATH=/usr/bin".into()]),
            ..probe_for(&row)
        };
        assert_eq!(verify_with(&row, &probe), Fence::Verified);
    }

    #[test]
    fn the_env_fence_carries_a_process_that_rewrote_its_argv() {
        let row = row(&["/bin/agent", "hang"]);
        let probe = FakeProbe {
            argv: Some(argv_of(&["/bin/something-else", "--now"])),
            args_string: Some("/bin/something-else --now".into()),
            ..probe_for(&row)
        };
        assert_eq!(verify_with(&row, &probe), Fence::Verified);
    }

    #[test]
    fn neither_fence_readable_fails_closed() {
        let row = row(&["/bin/agent", "hang"]);
        let probe = FakeProbe {
            argv: None,
            args_string: None,
            environ: None,
            ..probe_for(&row)
        };
        assert_eq!(
            verify_with(&row, &probe),
            Fence::Mismatch(MismatchReason::Argv)
        );
    }

    #[test]
    fn a_readable_foreign_marker_with_no_argv_reports_the_env_fence() {
        let row = row(&["/bin/agent", "hang"]);
        let probe = FakeProbe {
            argv: None,
            args_string: None,
            environ: Some(vec!["GILDE_SPAWN_ID=somebody-else".into()]),
            ..probe_for(&row)
        };
        assert_eq!(
            verify_with(&row, &probe),
            Fence::Mismatch(MismatchReason::EnvMarker)
        );
    }

    #[test]
    fn a_different_program_on_the_same_pid_is_a_mismatch() {
        let row = row(&["/bin/agent", "hang"]);
        let probe = FakeProbe {
            argv: Some(argv_of(&["/bin/sleep", "30"])),
            args_string: Some("/bin/sleep 30".into()),
            environ: Some(vec!["PATH=/usr/bin".into()]),
            ..probe_for(&row)
        };
        assert_eq!(
            verify_with(&row, &probe),
            Fence::Mismatch(MismatchReason::Argv)
        );
    }

    #[test]
    fn the_degraded_ps_path_matches_a_joined_command_line() {
        let row = row(&["/bin/agent", "hang"]);
        let probe = FakeProbe {
            argv: None,
            args_string: Some("/bin/agent hang".into()),
            environ: None,
            ..probe_for(&row)
        };
        assert_eq!(verify_with(&row, &probe), Fence::Verified);
    }

    #[test]
    fn the_relaxed_fallback_needs_argv0_and_one_distinctive_token() {
        let recorded = argv_of(&["/bin/agent", "--flag", "worktree-x"]);
        assert!(relaxed_match(
            &recorded,
            &argv_of(&["/bin/agent", "--other", "worktree-x"])
        ));
        assert!(
            !relaxed_match(&recorded, &argv_of(&["/bin/other", "worktree-x"])),
            "argv[0] must be identical"
        );
        assert!(
            !relaxed_match(&recorded, &argv_of(&["/bin/agent", "--flag"])),
            "a flag is not distinctive"
        );
        assert!(
            !relaxed_match(&argv_of(&["/bin/agent", "-p"]), &argv_of(&["/bin/agent", "-p"])),
            "a flags-only argv abstains rather than passing"
        );
    }

    #[test]
    fn the_degraded_start_time_is_used_only_when_the_kernel_one_is_missing() {
        let row = row(&["/bin/agent", "hang"]);
        let probe = FakeProbe {
            start_time: None,
            start_time_degraded: row.proc_start_ts.map(|t| t + 2),
            ..probe_for(&row)
        };
        assert_eq!(verify_with(&row, &probe), Fence::Verified);

        let probe = FakeProbe {
            start_time: None,
            start_time_degraded: row.proc_start_ts.map(|t| t + 5),
            ..probe_for(&row)
        };
        assert_eq!(
            verify_with(&row, &probe),
            Fence::Mismatch(MismatchReason::StartTime)
        );
    }

    #[test]
    fn only_verified_may_signal() {
        assert!(Fence::Verified.may_signal());
        assert!(!Fence::Gone.may_signal());
        for reason in [
            MismatchReason::StartTime,
            MismatchReason::Argv,
            MismatchReason::EnvMarker,
            MismatchReason::Uid,
            MismatchReason::NoPid,
        ] {
            assert!(!Fence::Mismatch(reason).may_signal(), "{reason:?}");
        }
    }

    #[test]
    fn live_argv_reads_our_own_command_line() {
        let me = std::process::id() as i32;
        let argv = live_argv(me).expect("our own argv is readable");
        assert!(!argv.is_empty());
    }
}
