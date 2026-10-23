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

// ---------------------------------------------------------------------------
// the launch allowlist (contract §9.3)
// ---------------------------------------------------------------------------

/// The kind an allowlist entry is keyed on: the `proc_registry.kind` set,
/// so that resolved `git` (`git` rows, §9.3) has a kind of its own. The
/// contract kinds convert into it.
pub type AllowKind = crate::store::models::ProcKind;

/// What an allowlisted executable may be spawned as.
pub type KindSet = std::collections::HashSet<AllowKind>;

/// Executables that are always allowed, by kind (§9.3): `/bin/sh -c` for
/// PaneHost panes only; `ps`/`lsof` single-pid fallbacks as probes;
/// `/usr/bin/open -b` for the IDE opener.
const FIXED: [(&str, AllowKind); 4] = [
    ("/bin/sh", AllowKind::Pane),
    ("/bin/ps", AllowKind::Probe),
    ("/usr/sbin/lsof", AllowKind::Probe),
    ("/usr/bin/open", AllowKind::Ide),
];

/// A legacy kind checked as the Gilde 2 kind it became: `headless` rows are
/// the sidecar's turn runs (contract §3.8, "`turn` replaces `headless`").
fn allow_kind_of(kind: AllowKind) -> AllowKind {
    match kind {
        AllowKind::Headless => AllowKind::Turn,
        other => other,
    }
}

fn allowlist() -> &'static parking_lot::RwLock<std::collections::HashMap<String, KindSet>> {
    static LIST: std::sync::OnceLock<parking_lot::RwLock<std::collections::HashMap<String, KindSet>>> =
        std::sync::OnceLock::new();
    LIST.get_or_init(Default::default)
}

/// Add `path` (canonicalised) to the live allowlist for `kinds` — the
/// resolved vendor binaries (detection, `agents::resolve_binary`,
/// `settings_set_executable`), resolved `git`, the sidecar's node, the IDE
/// CLIs, the login shell (§9.3).
pub fn allow_executable<K: Copy + Into<AllowKind>>(path: &Path, kinds: &[K]) {
    let key = canonical(path);
    let mut list = allowlist().write();
    let entry = list.entry(key).or_default();
    entry.extend(kinds.iter().map(|k| allow_kind_of((*k).into())));
}

/// Remove `path` from the allowlist (a binary setting was changed away).
pub fn disallow_executable(path: &Path) {
    allowlist().write().remove(&canonical(path));
}

/// Whether `argv0` may be spawned as `kind` (§9.3 enforcement 1). Both
/// spawn paths of the supervisor ask this one function (A12.8).
pub fn is_allowed<K: Into<AllowKind>>(argv0: &Path, kind: K) -> bool {
    let kind = allow_kind_of(kind.into());
    let key = canonical(argv0);
    if FIXED.iter().any(|(p, k)| *p == key && *k == kind) {
        return true;
    }
    allowlist().read().get(&key).is_some_and(|kinds| kinds.contains(&kind))
}

fn canonical(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
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
    fn the_allowlist_is_per_kind() {
        use crate::contract::process::ProcKind as K;
        assert!(is_allowed(Path::new("/bin/sh"), K::Pane));
        assert!(!is_allowed(Path::new("/bin/sh"), K::Convo));
        assert!(is_allowed(Path::new("/bin/ps"), K::Probe));
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("claude");
        std::fs::write(&bin, b"x").unwrap();
        assert!(!is_allowed(&bin, K::Convo));
        allow_executable(&bin, &[K::Convo, K::Pty]);
        assert!(is_allowed(&bin, K::Convo));
        assert!(!is_allowed(&bin, K::Pane));
        disallow_executable(&bin);
        assert!(!is_allowed(&bin, K::Convo));
    }

    #[test]
    fn an_impossible_pid_is_not_alive() {
        assert!(!is_alive(0));
        assert!(!is_alive(-1));
        assert!(pgid_of(-1).is_none());
    }
}
