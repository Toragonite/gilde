//! Path resolution inside known roots (contract §9.4, §13.1).
//!
//! Owner: T11.
//!
//! A path the WebView sends (`ide_open{File|Diff}`) is joined to the
//! session's location when relative and first checked **lexically**
//! ([`check_lexical`], A33.1: `.`/`..` folded, no file-system access for the
//! target, so an escape is refused whether or not it exists), then
//! `realpath`-ed, and must still lie inside
//! one of the allowed roots — the session's location, its project root,
//! another location of that project, or a folder picked with `project_pick`
//! in this app instance — by a string-prefix check on canonical paths with a
//! trailing `/`. The comparison is Unicode-NFC on both sides (APFS keeps the
//! bytes it was given, HFS+ stored NFD) and case-insensitive when the root's
//! volume is (`pathconf(_PC_CASE_SENSITIVE) == 0`). Paths inside any vendor
//! root are refused the same way (`REFUSED{outsideRoots}`), whatever root
//! they also sit in. `realpath` resolves `..`, symlinks and the
//! `/tmp`↔`/private/tmp` firmlink, so an escape through any of them lands
//! outside the roots and is refused.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use parking_lot::RwLock;
use unicode_normalization::UnicodeNormalization as _;

use crate::contract::error::IpcError;

/// The refusal of a path outside every allowed root (or inside a vendor root).
pub const REASON_OUTSIDE_ROOTS: &str = "outsideRoots";

/// Folders picked with `project_pick` in this app instance (canonical).
/// Memory only: a new launch starts empty (§9.4).
#[derive(Debug, Default)]
pub struct PickedRoots {
    roots: RwLock<Vec<PathBuf>>,
}

impl PickedRoots {
    pub fn new() -> Arc<PickedRoots> {
        Arc::new(PickedRoots::default())
    }

    /// Remember `root` (canonicalized; a path that does not resolve is ignored).
    pub fn add(&self, root: &Path) {
        let Ok(real) = std::fs::canonicalize(root) else { return };
        let mut roots = self.roots.write();
        if !roots.contains(&real) {
            roots.push(real);
        }
    }

    pub fn list(&self) -> Vec<PathBuf> {
        self.roots.read().clone()
    }
}

/// Whether the volume holding `path` (or its nearest existing ancestor)
/// compares file names case-insensitively. Unknown → `false` (the stricter
/// reading for an allowed root).
pub fn case_insensitive(path: &Path) -> bool {
    let mut probe = path.to_path_buf();
    while !probe.exists() {
        if !probe.pop() {
            return false;
        }
    }
    let Ok(c) = CString::new(probe.as_os_str().as_bytes()) else { return false };
    // SAFETY: `c` is a valid NUL-terminated path for the call's duration.
    let r = unsafe { libc::pathconf(c.as_ptr(), libc::_PC_CASE_SENSITIVE) };
    r == 0
}

/// NFC, then lowercase when `fold`.
fn norm(s: &str, fold: bool) -> String {
    let nfc: String = s.nfc().collect();
    if fold {
        nfc.to_lowercase()
    } else {
        nfc
    }
}

/// `path` is `root` or lies under it (canonical inputs; trailing-`/` rule).
pub fn within(path: &Path, root: &Path, fold: bool) -> bool {
    let (Some(p), Some(r)) = (path.to_str(), root.to_str()) else { return false };
    let p = norm(p, fold);
    let mut r = norm(r, fold);
    if p == r {
        return true;
    }
    if !r.ends_with('/') {
        r.push('/');
    }
    p.starts_with(&r)
}

/// The vendor directories under `home` whose contents are never opened in an
/// editor (§9.4, §9.7, §9.10): every `~/.claude*` entry (config dirs and
/// discovered siblings, `.claude.json`, the orchestrator dir), `~/.codex`,
/// `~/.cursor`, `~/.gemini` — judged by the first component under the
/// canonical home (case-folded: refusing more is the safe side), so dirs
/// created after launch are covered too.
pub fn is_vendor_path(path: &Path, home: &Path) -> bool {
    let home = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    [path.to_path_buf(), lexical(path)].iter().any(|p| {
        let Some(rel) = strip_prefix_folded(p, &home) else { return false };
        let Some(Component::Normal(first)) = rel.components().next() else { return false };
        let name = norm(&first.to_string_lossy(), true);
        name.starts_with(".claude") || name == ".codex" || name == ".cursor" || name == ".gemini"
    })
}

/// `p` relative to `root` when `root` is a prefix (NFC, case-folded).
fn strip_prefix_folded(p: &Path, root: &Path) -> Option<PathBuf> {
    let ps = norm(p.to_str()?, true);
    let mut rs = norm(root.to_str()?, true);
    if !rs.ends_with('/') {
        rs.push('/');
    }
    let rest = ps.strip_prefix(&rs)?;
    Some(PathBuf::from(rest))
}

/// `p` without `.` and `..` (no filesystem access).
fn lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Resolve `raw` (absolute, or relative to `base`) to a canonical path.
///
/// `allow_missing`: a missing **last** component is accepted when its parent
/// resolves (the Diff of a deleted file); its name must be an ordinary name.
/// Anything else that does not exist → `NOT_FOUND{file}`.
pub fn resolve(raw: &str, base: &Path, allow_missing: bool) -> Result<PathBuf, IpcError> {
    let p = Path::new(raw);
    let joined = if p.is_absolute() { p.to_path_buf() } else { base.join(p) };
    match std::fs::canonicalize(&joined) {
        Ok(real) => Ok(real),
        Err(e) if allow_missing && e.kind() == std::io::ErrorKind::NotFound => {
            let lex = lexical(&joined);
            let (Some(parent), Some(Component::Normal(name))) = (lex.parent(), lex.components().next_back()) else {
                return Err(IpcError::not_found("file"));
            };
            let parent = std::fs::canonicalize(parent).map_err(|_| IpcError::not_found("file"))?;
            Ok(parent.join(name))
        }
        Err(_) => Err(IpcError::not_found("file")),
    }
}

/// `path` (canonical) passes the §9.4 rule: inside an allowed root and in no
/// vendor root (`extra_vendor`: user-added config dirs elsewhere).
pub fn check(path: &Path, allowed: &[PathBuf], home: &Path, extra_vendor: &[PathBuf]) -> Result<(), IpcError> {
    if is_vendor_path(path, home)
        || extra_vendor.iter().any(|v| {
            let v = std::fs::canonicalize(v).unwrap_or_else(|_| v.clone());
            within(path, &v, true)
        })
    {
        return Err(IpcError::refused(REASON_OUTSIDE_ROOTS));
    }
    let inside = allowed.iter().any(|root| {
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
        within(path, &root, case_insensitive(&root))
    });
    if inside {
        Ok(())
    } else {
        Err(IpcError::refused(REASON_OUTSIDE_ROOTS))
    }
}

/// The macOS firmlinks a path may be spelled through: `/tmp`, `/var` and
/// `/etc` are `/private/tmp`, `/private/var` and `/private/etc`. Unified by
/// name, without asking the file system.
fn firmlinked(p: &Path) -> PathBuf {
    let mut c = p.components();
    if let (Some(Component::RootDir), Some(Component::Normal(first))) = (c.next(), c.next()) {
        if matches!(first.to_str(), Some("tmp" | "var" | "etc")) {
            return Path::new("/private").join(p.strip_prefix("/").unwrap_or(p));
        }
    }
    p.to_path_buf()
}

/// A33.1: the **lexical** roots check, made before the target is ever
/// `stat`-ed or `realpath`-ed — `raw` (absolute, or relative to `base`) with
/// `.`/`..` folded and the firmlink spellings unified must lie inside an
/// allowed root and in no vendor root, else `REFUSED{outsideRoots}`. So a
/// path that leaves the roots answers the same whether or not it exists (no
/// existence oracle); a symlink that leads out is caught by [`check`] on the
/// resolved path afterwards. A literal `~` is an ordinary name.
pub fn check_lexical(raw: &str, base: &Path, allowed: &[PathBuf], home: &Path, extra_vendor: &[PathBuf]) -> Result<(), IpcError> {
    let p = Path::new(raw);
    let joined = if p.is_absolute() { p.to_path_buf() } else { base.join(p) };
    check(&firmlinked(&lexical(&joined)), allowed, home, extra_vendor)
}

/// `path` relative to `root` (both canonical), with the same folding rules;
/// `None` when it is not strictly inside.
pub fn relative_to(path: &Path, root: &Path) -> Option<String> {
    if !within(path, root, case_insensitive(root)) {
        return None;
    }
    let ps: String = path.to_str()?.nfc().collect();
    let rs: String = root.to_str()?.nfc().collect();
    let rest: String = ps.chars().skip(rs.chars().count()).collect();
    let rest = rest.trim_start_matches('/');
    (!rest.is_empty()).then(|| rest.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prefix_rule_needs_a_trailing_slash() {
        assert!(within(Path::new("/w/repo"), Path::new("/w/repo"), false));
        assert!(within(Path::new("/w/repo/a.rs"), Path::new("/w/repo"), false));
        assert!(!within(Path::new("/w/repo2/a.rs"), Path::new("/w/repo"), false));
        assert!(!within(Path::new("/w/rep"), Path::new("/w/repo"), false));
        assert!(within(Path::new("/W/Repo/a"), Path::new("/w/repo"), true));
        assert!(!within(Path::new("/W/Repo/a"), Path::new("/w/repo"), false));
    }

    #[test]
    fn nfc_and_nfd_spellings_agree() {
        let nfc = "/w/한글/a";
        let nfd: String = nfc.nfd().collect();
        assert_ne!(nfc, nfd);
        assert!(within(Path::new(&nfd), Path::new("/w/한글"), false));
    }

    #[test]
    fn vendor_paths_by_their_first_component_under_home() {
        let home = Path::new("/Users/u");
        for p in [
            "/Users/u/.claude/settings.json",
            "/Users/u/.claude-g/sessions/1.json",
            "/Users/u/.claude.json",
            "/Users/u/.claude-code-orchestrator/x",
            "/Users/u/.codex/auth.json",
            "/Users/u/.cursor/chats/a",
            "/Users/u/.gemini/oauth_creds.json",
            "/Users/u/.CLAUDE/x",
            "/Users/u/work/../.codex/x",
        ] {
            assert!(is_vendor_path(Path::new(p), home), "{p}");
        }
        for p in ["/Users/u/work/.claude/settings.json", "/Users/u/claude/x", "/tmp/.claude/x"] {
            assert!(!is_vendor_path(Path::new(p), home), "{p}");
        }
    }

    #[test]
    fn resolve_joins_relative_paths_and_allows_a_missing_last_component_on_request() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), b"x").unwrap();
        assert_eq!(resolve("src/a.rs", &root, false).unwrap(), root.join("src/a.rs"));
        assert_eq!(resolve("src/../src/a.rs", &root, false).unwrap(), root.join("src/a.rs"));
        assert_eq!(resolve("src/gone.rs", &root, false).unwrap_err().code, crate::contract::ErrorCode::NotFound);
        assert_eq!(resolve("src/gone.rs", &root, true).unwrap(), root.join("src/gone.rs"));
        assert!(resolve("nodir/gone.rs", &root, true).is_err(), "the parent must resolve");
        assert_eq!(relative_to(&root.join("src/a.rs"), &root).as_deref(), Some("src/a.rs"));
        assert_eq!(relative_to(&root, &root), None);
    }

    fn refused(r: Result<(), IpcError>) -> bool {
        r.is_err_and(|e| e.code == crate::contract::ErrorCode::Refused && e.params["reason"] == REASON_OUTSIDE_ROOTS)
    }

    #[test]
    fn the_lexical_check_refuses_an_escape_whether_or_not_it_exists() {
        let dir = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(dir.path()).unwrap();
        let root = home.join("work/proj");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(home.join("work/outside.txt"), b"x").unwrap();
        let allowed = vec![root.clone()];
        let deep = format!("{}etc/passwd", "../".repeat(root.components().count() + 1));
        for raw in [
            "../outside.txt",
            "../missing.txt",
            "src/../../outside.txt",
            "src/../../nowhere/at/all.txt",
            deep.as_str(),
            "/etc/passwd",
            "/private/etc/no-such-file",
            "../../.claude/settings.json",
        ] {
            assert!(refused(check_lexical(raw, &root, &allowed, &home, &[])), "{raw}");
        }
        // Inside names pass, existing or not (resolve answers NOT_FOUND later).
        for raw in ["src/a.rs", "src/../b.rs", "~/.claude/settings.json", "./x/../y"] {
            assert!(check_lexical(raw, &root, &allowed, &home, &[]).is_ok(), "{raw}");
        }
        // An absolute spelling of an inside path passes; a vendor root inside
        // an allowed root is refused by name.
        assert!(check_lexical(root.join("a.rs").to_str().unwrap(), &root, &allowed, &home, &[]).is_ok());
        let at_home = vec![home.clone()];
        assert!(refused(check_lexical(".claude/settings.json", &home, &at_home, &home, &[])));
        assert!(refused(check_lexical(".codex/x", &home, &at_home, &home, &[])));
    }

    #[test]
    fn the_firmlink_spellings_are_one_place() {
        assert_eq!(firmlinked(Path::new("/tmp/a")), PathBuf::from("/private/tmp/a"));
        assert_eq!(firmlinked(Path::new("/var/folders/x")), PathBuf::from("/private/var/folders/x"));
        assert_eq!(firmlinked(Path::new("/etc")), PathBuf::from("/private/etc"));
        assert_eq!(firmlinked(Path::new("/private/tmp/a")), PathBuf::from("/private/tmp/a"));
        assert_eq!(firmlinked(Path::new("/Users/u/tmp")), PathBuf::from("/Users/u/tmp"));
        assert_eq!(firmlinked(Path::new("/tmpx/a")), PathBuf::from("/tmpx/a"));
        // A `/tmp` spelling of an allowed `/private/tmp` root passes lexically.
        let root = PathBuf::from("/private/tmp/gilde-lexical-root");
        assert!(check_lexical("/tmp/gilde-lexical-root/f.txt", &root, std::slice::from_ref(&root), Path::new("/Users/nobody"), &[]).is_ok());
        assert!(refused(check_lexical("/tmp/other/f.txt", &root, std::slice::from_ref(&root), Path::new("/Users/nobody"), &[])));
    }
}
