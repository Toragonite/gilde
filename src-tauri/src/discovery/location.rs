//! Project and location identity without spawning git (contract §5.6).
//!
//! From `realpath(cwd)` walk up to the first `.git` (stopping at `$HOME` and
//! `/`): a directory → that git dir; a file `gitdir: <path>` → a worktree git
//! dir whose `commondir` file (relative) gives the common dir. Project id =
//! sha256(realpath(common dir)); location = the directory holding `.git`;
//! `HEAD` gives the branch. No `.git` → a Folder project (the cwd itself).
//! Bare repos, `GIT_DIR` tricks and unreadable `.git` → Folder with a
//! diagnostic. `/tmp` and `/private/tmp` compare equal after realpath.
//! `commands/repo_identity.rs` keeps its bounded-`git` path only for the
//! Changes view.

use std::path::{Path, PathBuf};

use crate::contract::ids::{LocationId, ProjectId};
use crate::contract::services::LocationFacts;
use crate::discovery::abs_path;
use crate::discovery::safe_read::{self, EntryKind, Root};

const GIT_FILE_CAP: u64 = 4 * 1024;
const WALK_MAX: usize = 64;

/// Why a path with a `.git` became a Folder project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocationDiag {
    BareOrGitDir,
    Unreadable,
}

/// The identity of one cwd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub project_id: ProjectId,
    /// Main checkout root (git) or the folder.
    pub project_root: PathBuf,
    pub git: bool,
    pub location_id: LocationId,
    pub location_path: PathBuf,
    pub worktree: bool,
    pub branch: Option<String>,
    pub diag: Option<LocationDiag>,
    /// Under `/tmp` or `/private/tmp`.
    pub is_temp: bool,
}

impl Identity {
    /// As the contract's [`LocationFacts`] (non-UTF-8 paths → `None`).
    pub fn facts(&self) -> Option<LocationFacts> {
        Some(LocationFacts {
            project_id: self.project_id.clone(),
            project_root: abs_path(&self.project_root)?,
            git: self.git,
            location_id: self.location_id.clone(),
            location_path: abs_path(&self.location_path)?,
            worktree: self.worktree,
            branch: self.branch.clone(),
        })
    }
}

fn folder(real: &Path, diag: Option<LocationDiag>) -> Option<Identity> {
    let s = real.to_str()?;
    Some(Identity {
        project_id: ProjectId::derive(s),
        project_root: real.to_path_buf(),
        git: false,
        location_id: LocationId::derive(s),
        location_path: real.to_path_buf(),
        worktree: false,
        branch: None,
        diag,
        is_temp: is_temp(real),
    })
}

pub fn is_temp(real: &Path) -> bool {
    real.starts_with("/private/tmp") || real.starts_with("/tmp")
}

fn read_small(dir: &Path, file: &Path) -> Option<String> {
    let root = Root::new(dir).ok()?;
    let (bytes, _) = safe_read::read_file(&root, file, GIT_FILE_CAP).ok()?;
    String::from_utf8(bytes.to_vec()).ok()
}

/// `ref: refs/heads/<b>` → `b` (≤ 255, printable); detached → `None`.
pub fn branch_from_head(head: &str) -> Option<String> {
    let b = head.trim().strip_prefix("ref:")?.trim().strip_prefix("refs/heads/")?;
    (!b.is_empty() && b.len() <= 255 && !b.chars().any(char::is_control)).then(|| b.to_string())
}

/// Identify `cwd` (`home` is the walk boundary).
pub fn identify(cwd: &Path, home: &Path) -> Option<Identity> {
    let real = std::fs::canonicalize(cwd).ok()?;
    real.to_str()?;
    let home_real = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    let mut dir = real.clone();
    for _ in 0..WALK_MAX {
        if dir == home_real || dir.as_os_str() == "/" {
            break;
        }
        let dotgit = dir.join(".git");
        match safe_read::lstat(&dotgit) {
            Ok(st) if st.kind == EntryKind::Dir => {
                return git_identity(&dir, &dotgit, &dotgit, false, &real);
            }
            Ok(st) if st.kind == EntryKind::File => {
                let Some(text) = read_small(&dir, &dotgit) else { return folder(&real, Some(LocationDiag::Unreadable)) };
                let Some(rel) = text.lines().next().and_then(|l| l.strip_prefix("gitdir:")).map(str::trim) else {
                    return folder(&real, Some(LocationDiag::Unreadable));
                };
                let gitdir = if Path::new(rel).is_absolute() { PathBuf::from(rel) } else { dir.join(rel) };
                let Ok(gitdir) = std::fs::canonicalize(&gitdir) else { return folder(&real, Some(LocationDiag::Unreadable)) };
                let common = match read_small(&gitdir, &gitdir.join("commondir")) {
                    Some(c) => {
                        let c = c.trim();
                        let p = if Path::new(c).is_absolute() { PathBuf::from(c) } else { gitdir.join(c) };
                        match std::fs::canonicalize(p) {
                            Ok(p) => p,
                            Err(_) => return folder(&real, Some(LocationDiag::Unreadable)),
                        }
                    }
                    None => gitdir.clone(),
                };
                return git_identity(&dir, &gitdir, &common, true, &real);
            }
            Ok(_) => return folder(&real, Some(LocationDiag::Unreadable)),
            Err(safe_read::ReadError::NotFound) => {}
            Err(_) => return folder(&real, Some(LocationDiag::Unreadable)),
        }
        match dir.parent() {
            Some(p) => dir = p.to_path_buf(),
            None => break,
        }
    }
    folder(&real, None)
}

fn git_identity(location: &Path, gitdir: &Path, common: &Path, linked: bool, real_cwd: &Path) -> Option<Identity> {
    let Ok(common) = std::fs::canonicalize(common) else { return folder(real_cwd, Some(LocationDiag::Unreadable)) };
    if common.file_name().and_then(|n| n.to_str()) != Some(".git") {
        return folder(real_cwd, Some(LocationDiag::BareOrGitDir));
    }
    let project_root = common.parent()?.to_path_buf();
    let location = std::fs::canonicalize(location).ok()?;
    let branch = read_small(gitdir, &gitdir.join("HEAD")).and_then(|h| branch_from_head(&h));
    Some(Identity {
        project_id: ProjectId::derive(common.to_str()?),
        project_root,
        git: true,
        location_id: LocationId::derive(location.to_str()?),
        location_path: location.clone(),
        worktree: linked,
        branch,
        diag: None,
        is_temp: is_temp(&location),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_checkout_worktree_folder_and_bare() {
        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("repo");
        std::fs::create_dir_all(repo.join(".git/worktrees/wt1")).unwrap();
        std::fs::create_dir_all(repo.join("src/deep")).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        let wt = home.path().join("wt1");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", repo.join(".git/worktrees/wt1").display())).unwrap();
        std::fs::write(repo.join(".git/worktrees/wt1/commondir"), "../..\n").unwrap();
        std::fs::write(repo.join(".git/worktrees/wt1/HEAD"), "ref: refs/heads/feat/x\n").unwrap();

        let main = identify(&repo.join("src/deep"), home.path()).unwrap();
        assert!(main.git && !main.worktree);
        assert_eq!(main.branch.as_deref(), Some("main"));
        assert_eq!(main.location_path, std::fs::canonicalize(&repo).unwrap());
        let linked = identify(&wt, home.path()).unwrap();
        assert!(linked.worktree);
        assert_eq!(linked.project_id, main.project_id, "one project per common dir");
        assert_ne!(linked.location_id, main.location_id);
        assert_eq!(linked.branch.as_deref(), Some("feat/x"));

        let plain = home.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let f = identify(&plain, home.path()).unwrap();
        assert!(!f.git);
        assert_eq!(f.project_root, std::fs::canonicalize(&plain).unwrap());

        let bare_wt = home.path().join("bare_wt");
        std::fs::create_dir_all(home.path().join("bare.git/worktrees/b")).unwrap();
        std::fs::create_dir_all(&bare_wt).unwrap();
        std::fs::write(bare_wt.join(".git"), format!("gitdir: {}\n", home.path().join("bare.git/worktrees/b").display())).unwrap();
        std::fs::write(home.path().join("bare.git/worktrees/b/commondir"), "../..\n").unwrap();
        let b = identify(&bare_wt, home.path()).unwrap();
        assert_eq!((b.git, b.diag), (false, Some(LocationDiag::BareOrGitDir)));
    }

    #[test]
    fn home_is_a_boundary_and_tmp_spellings_agree() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".git")).unwrap();
        std::fs::create_dir_all(home.path().join("p")).unwrap();
        assert!(!identify(&home.path().join("p"), home.path()).unwrap().git, "a dotfiles repo in $HOME never swallows folders");
        let t = tempfile::Builder::new().prefix("gilde-loc-").tempdir_in("/tmp").unwrap();
        let a = identify(t.path(), home.path()).unwrap();
        let private = Path::new("/private").join(t.path().strip_prefix("/").unwrap());
        let b = identify(&private, home.path()).unwrap();
        assert_eq!(a.project_id, b.project_id);
        assert!(a.is_temp);
        assert_eq!(branch_from_head("0123abcd\n"), None);
    }
}
