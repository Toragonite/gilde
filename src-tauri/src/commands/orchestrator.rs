//! The presence-gated orchestrator layer (contract §5.10, §7.3.1) [R1 §6].
//!
//! Owner: T08 (replaces `cco_team.rs` / `cco_workers.rs`). v1 has **no**
//! orchestrator command in the manifest: teams, worker accounts and attribution
//! reach the WebView through the store. What remains here is the glue between
//! presence and the sidecar:
//!
//!   * presence is detected by discovery (T03: a non-recursive `$HOME` watch plus
//!     one check at boot, B13) and handed to [`apply_presence`], which tells the
//!     sidecar (`providers.orchestrator`) — registering the `orch.*` methods while
//!     present, removing them while absent (PM-22) — and starts the sidecar
//!     lazily when the orchestrator appears (B12);
//!   * [`registry_present`] is the boot-time half of "present": the registry file
//!     exists (by `stat`, never read here).
//!
//! The observer's team views arrive as `orch.snapshot` and are relayed by
//! `SidecarManager::subscribe_orchestrator` to World's orchestrator mapping (T02).

use std::sync::Arc;

use crate::paths::AppPaths;
use crate::sidecar::SidecarManager;

/// Whether `~/.claude-code-orchestrator/registry.json` exists (`lstat` only).
pub fn registry_present(paths: &AppPaths) -> bool {
    std::fs::symlink_metadata(paths.cco_registry()).is_ok()
}

/// Apply a presence change: the sidecar registers or removes its `orch.*`
/// methods, and starts lazily when the orchestrator appears.
pub async fn apply_presence(sidecar: &Arc<SidecarManager>, present: bool) {
    sidecar.set_orchestrator_present(present).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presence_is_the_registry_file_by_stat() {
        let home = tempfile::tempdir().expect("home");
        let paths = AppPaths::for_home(home.path());
        assert!(!registry_present(&paths));
        let registry = paths.cco_registry();
        std::fs::create_dir_all(registry.parent().expect("parent")).expect("mkdir");
        std::fs::write(&registry, "{}").expect("write");
        assert!(registry_present(&paths));
    }
}
