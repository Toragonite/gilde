//! Orchestrator presence detection (contract §5.10) [ux-ia §8, R1 §6].
//!
//! Present = `~/.claude-code-orchestrator/registry.json` exists, or its
//! `run/` dir holds a host socket (existence only: discovery never connects to
//! a socket — the observer handshake belongs to the sidecar's `orch.*`, which
//! is registered only while present). Detection = the kqueue watch of `$HOME`
//! (+ the orchestrator dir once it exists) and a check at boot and on every
//! reconcile; no polling. Nothing in that directory is opened.

use std::path::{Path, PathBuf};

use crate::discovery::safe_read::{self, EntryKind, Root};

/// The orchestrator's data root.
pub fn root_of(home: &Path) -> PathBuf {
    home.join(".claude-code-orchestrator")
}

/// Whether the orchestrator is present (stat and a listing only).
pub fn present(home: &Path) -> bool {
    let root = root_of(home);
    if safe_read::lstat(&root.join("registry.json")).is_ok_and(|s| s.kind == EntryKind::File) {
        return true;
    }
    let Ok(r) = Root::new(&root) else { return false };
    let run = root.join("run");
    match safe_read::list_dir(&r, &run, 1024) {
        Ok(l) => l.entries.iter().any(|e| e.kind == EntryKind::Other && e.name.ends_with(".sock")),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presence_by_registry_or_socket() {
        // A short root: socket paths are limited to 104 bytes.
        let home = tempfile::Builder::new().prefix("g").tempdir_in("/tmp").unwrap();
        assert!(!present(home.path()));
        std::fs::create_dir_all(root_of(home.path()).join("run")).unwrap();
        assert!(!present(home.path()), "an empty dir is not presence");
        let sock = root_of(home.path()).join("run/host-1.sock");
        let _l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        assert!(present(home.path()));
        drop(_l);
        std::fs::remove_file(&sock).unwrap();
        std::fs::write(root_of(home.path()).join("registry.json"), b"{}").unwrap();
        assert!(present(home.path()));
    }
}
