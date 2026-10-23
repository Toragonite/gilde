//! `~/.claude-*` sibling discovery by `stat` only (contract §5.1) [C7].
//!
//! A non-recursive listing of `$HOME` (at boot and on every reconcile; the
//! kqueue watch of `$HOME` triggers it early). A sibling is a directory named
//! `.claude-*` that contains `.claude.json` — checked with `lstat`, **never
//! opened**. Siblings are watched read-only, shown with their sessions, and
//! offered for New session only after the user confirms them
//! (`AccountSource::Discovered`, `usable_for_new: false`). Gilde never creates
//! or writes anything in any config dir and never runs `claude agents --json`.

use std::path::{Path, PathBuf};

use crate::contract::account::{Account, AccountSource, SignIn};
use crate::contract::common::Vendor;
use crate::contract::ids::AccountId;
use crate::discovery::abs_path;
use crate::discovery::safe_read::{self, EntryKind, ReadError, Root};

/// One discovered sibling config dir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sibling {
    /// Realpath.
    pub path: PathBuf,
    /// The dir name, e.g. `.claude-work`.
    pub name: String,
}

/// List `$HOME` for `.claude-*` dirs holding a `.claude.json`.
pub fn discover(home: &Path) -> Result<Vec<Sibling>, ReadError> {
    let root = Root::new(home)?;
    let listing = safe_read::list_dir(&root, root.path(), safe_read::DIR_ENTRIES_CAP)?;
    let mut out = Vec::new();
    for e in listing.entries {
        if e.kind != EntryKind::Dir || !e.name.starts_with(".claude-") || e.name.len() <= ".claude-".len() {
            continue;
        }
        let dir = root.join(&e.name);
        // Existence by `lstat` only — `.claude.json` is never opened (§9.7).
        if safe_read::lstat(&dir.join(".claude.json")).is_ok_and(|s| s.kind == EntryKind::File) {
            out.push(Sibling { path: dir, name: e.name });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// The `Discovered` account of a sibling (label = the name after `.claude-`).
pub fn account_of(s: &Sibling) -> Option<Account> {
    let path = s.path.to_str()?;
    let label: String = s.name.trim_start_matches(".claude-").chars().filter(|c| !c.is_control()).take(64).collect();
    Some(Account {
        id: AccountId::derive(Vendor::Claude, path),
        vendor: Vendor::Claude,
        config_dir: abs_path(&s.path),
        label: if label.is_empty() { s.name.clone() } else { label },
        is_default: false,
        source: AccountSource::Discovered,
        usable_for_new: false,
        sign_in: SignIn::Unknown,
        quota: None,
        live_sessions: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn siblings_by_stat_only() {
        let home = tempfile::tempdir().unwrap();
        for d in [".claude-work", ".claude-empty", ".claude-code-orchestrator", ".claudex"] {
            std::fs::create_dir_all(home.path().join(d)).unwrap();
        }
        let cj = home.path().join(".claude-work/.claude.json");
        std::fs::write(&cj, b"{\"oauthAccount\":\"CANARY\"}").unwrap();
        std::fs::set_permissions(&cj, std::fs::Permissions::from_mode(0o000)).unwrap();
        safe_read::reset_stats();
        let s = discover(home.path()).unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].name, ".claude-work");
        assert_eq!(safe_read::stats().eacces, 0, ".claude.json was never opened");
        let a = account_of(&s[0]).unwrap();
        assert_eq!(a.source, AccountSource::Discovered);
        assert!(!a.usable_for_new);
        assert_eq!(a.label, "work");
    }
}
