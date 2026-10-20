//! Process-group discipline for every child this app creates (DESIGN §8-1).
//!
//! One rule, enforced by construction: **every process we start is a process
//! group leader** (`pgid == pid`). Killing a stuck agent then means signalling
//! *its* group, and never a group we do not own — the mistake that
//! `claude-code-orchestrator`'s `signalWorker` had to learn the hard way.
//!
//! Three spawn sites exist and each gets its group differently:
//!
//!   * **Rust direct spawn** (sidecar, `git`, `du`, `--version` probes) — this
//!     module. `process_group(0)` keeps the `posix_spawn` fast path and avoids
//!     `pre_exec`, whose closure runs in the forked child where allocation is
//!     undefined behaviour.
//!   * **PTY spawn** — `portable-pty` already calls `setsid()` + `TIOCSCTTY`,
//!     which it must do to acquire a controlling terminal, so `pgid == pid`.
//!   * **Sidecar-spawned vendor CLIs** — agent-mesh's exec uses Node's
//!     `detached: true`, i.e. `setsid()`.
//!
//! Note that `tokio::process::Command` has its **own inherent**
//! `process_group()`; importing `std::os::unix::process::CommandExt` for it is
//! unnecessary and would be ambiguous.

use std::path::Path;
use std::process::ExitStatus;

/// A `tokio` command that will start its child in a **new process group**
/// (`setpgid(0, 0)` between fork and exec), with the child as group leader.
///
/// stdio is left at tokio's default (inherit); callers configure it.
pub fn tokio_command_grouped(program: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(program);
    // Inherent method on tokio's Command — no `CommandExt` import (DESIGN §8-1).
    cmd.process_group(0);
    cmd
}

/// The blocking twin of [`tokio_command_grouped`], for boot-time and
/// `spawn_blocking` code paths that have no async context.
pub fn std_command_grouped(program: &Path) -> std::process::Command {
    use std::os::unix::process::CommandExt as _;
    let mut cmd = std::process::Command::new(program);
    cmd.process_group(0);
    cmd
}

/// Signal that terminated a child, if a signal did.
pub fn exit_signal_of(status: &ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt as _;
    status.signal()
}

/// Process group of `pid` as the kernel reports it. `None` when the process is
/// gone (or we may not ask).
///
/// The registry records the pgid we *observed*; DESIGN §8-1 forbids inferring
/// one from a pid, so this is the only sanctioned source besides "we just
/// created the group ourselves".
pub fn pgid_of(pid: i32) -> Option<i32> {
    if pid <= 0 {
        return None;
    }
    // SAFETY: `getpgid` reads kernel state for `pid` and touches no memory of
    // ours; a failure is reported as -1 and turned into `None` here.
    let pgid = unsafe { libc::getpgid(pid as libc::pid_t) };
    if pgid < 0 {
        None
    } else {
        Some(pgid as i32)
    }
}

/// Whether `pid` exists right now, regardless of who owns it.
///
/// `kill(pid, 0)` is the portable existence probe: `EPERM` means "it is there
/// but not yours", which is still *alive* and must never read as gone — a row
/// reported `Gone` is closed without a signal, and closing a live row would
/// leak the process.
pub fn is_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    match kill(Pid::from_raw(pid), None) {
        Ok(()) => true,
        Err(Errno::EPERM) => true,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn our_own_pid_is_alive_and_has_a_group() {
        let me = std::process::id() as i32;
        assert!(is_alive(me));
        assert!(pgid_of(me).is_some());
    }

    #[test]
    fn an_impossible_pid_is_not_alive() {
        assert!(!is_alive(0));
        assert!(!is_alive(-1));
        assert!(pgid_of(-1).is_none());
    }
}
