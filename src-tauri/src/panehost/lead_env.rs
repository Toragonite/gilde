//! The hosted lead's env (§7.4.1), the pane env (§7.4.5) and the per-build
//! shim copy (§4.7).
//!
//! Owner: T07. Consumed by T10 (`apply` on the lead's composed env) and by
//! the composition root (T09b: `install_shim`, `prune_shims`).
//!
//! A hosted lead's env is the §6.0 composed env **plus**
//! `CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1`, `PATH=<shim dir>:<resolved PATH>`,
//! `GILDE_PANE_HOST=<socket>`, `GILDE_PANE_HOST_TOKEN=<64 hex>`,
//! `GILDE_REAL_TMUX=<real tmux or empty>` and `TERM_PROGRAM=gilde-code`, and
//! **absent by construction**: `TMUX`, `TMUX_PANE`, `ITERM_*`,
//! `LC_TERMINAL*` (`$TMUX` wins Claude's detection [R3b C.1]; an inherited
//! iTerm2 identity would route teammates to the real `it2` [R3 §7c]) — and
//! the variables §6.0 never lets through (`CLAUDECODE`,
//! `CLAUDE_CODE_TEAMMATE_COMMAND`, `CLAUDE_CODE_PROCESS_WRAPPER`,
//! `CLAUDE_CODE_MESSAGING_*`, `CLAUDE_CODE_ENTRYPOINT`). [`apply`] enforces
//! the removals whatever the base env carries, so an `envPassthrough` entry
//! cannot bring them back.
//!
//! A pane's env is the lead's composed env **minus** the token, the `TMUX*`
//! keys and the lead's own `GILDE_SPAWN_ID` (the PTY stamps a fresh one).

use std::collections::BTreeMap;
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::contract::services::LeadEnv;
use crate::panehost::proto;

pub const ENV_AGENT_TEAMS: &str = "CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS";
pub const ENV_TERM_PROGRAM: &str = "TERM_PROGRAM";
/// `TERM_PROGRAM` of every Gilde child (§6.0).
pub const TERM_PROGRAM_GILDE: &str = crate::supervise::registry::TERM_PROGRAM_VALUE;
/// The spawn marker the PTY stamps per process.
pub const ENV_SPAWN_ID: &str = crate::supervise::identity::ENV_SPAWN_ID;

/// Keys a hosted lead's env never carries; an entry ending in `*` is a prefix.
pub const ABSENT: [&str; 13] = [
    "TMUX",
    "TMUX_PANE",
    "TMUX_*",
    "ITERM_*",
    "LC_TERMINAL*",
    "TERM_PROGRAM_VERSION",
    "TERM_SESSION_ID",
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_TEAMMATE_COMMAND",
    "CLAUDE_CODE_PROCESS_WRAPPER",
    "CLAUDE_CODE_MESSAGING_*",
    proto::ENV_PANE_HOST_TOKEN,
];

/// Executable regular file (follows symlinks).
pub fn is_executable_file(p: &Path) -> bool {
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// `<shim dir>:<base PATH without any entry below the shim root>`.
pub fn path_with_shim(shim_dir: &Path, base_path: &str, shim_root: &Path) -> String {
    let mut parts: Vec<String> = vec![shim_dir.to_string_lossy().into_owned()];
    for entry in base_path.split(':') {
        if entry.is_empty() || Path::new(entry).starts_with(shim_root) {
            continue;
        }
        if !parts.iter().any(|p| p == entry) {
            parts.push(entry.to_string());
        }
    }
    parts.join(":")
}

/// The real tmux on `PATH` minus the shim dirs: the first absolute entry with
/// an executable `tmux` that is not a shim copy. Nothing is executed.
pub fn find_real_tmux(path: &str, shim_root: &Path) -> Option<PathBuf> {
    let root = std::fs::canonicalize(shim_root).unwrap_or_else(|_| shim_root.to_path_buf());
    for entry in path.split(':') {
        let dir = Path::new(entry);
        if entry.is_empty() || !dir.is_absolute() || dir.starts_with(shim_root) {
            continue;
        }
        let candidate = dir.join("tmux");
        if !is_executable_file(&candidate) {
            continue;
        }
        let real = std::fs::canonicalize(&candidate).unwrap_or_else(|_| candidate.clone());
        if real.starts_with(&root) || real.starts_with(shim_root) {
            continue;
        }
        return Some(candidate);
    }
    None
}

/// The additions and removals of a hosted lead (§7.4.1).
pub fn build(
    socket: &Path,
    shim_dir: &Path,
    shim_root: &Path,
    token: &str,
    base_path: &str,
    real_tmux: Option<&Path>,
) -> LeadEnv {
    LeadEnv {
        set: vec![
            (ENV_AGENT_TEAMS.to_string(), "1".to_string()),
            ("PATH".to_string(), path_with_shim(shim_dir, base_path, shim_root)),
            (proto::ENV_PANE_HOST.to_string(), socket.to_string_lossy().into_owned()),
            (proto::ENV_PANE_HOST_TOKEN.to_string(), token.to_string()),
            (
                proto::ENV_REAL_TMUX.to_string(),
                real_tmux.map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
            ),
            (ENV_TERM_PROGRAM.to_string(), TERM_PROGRAM_GILDE.to_string()),
        ],
        remove: ABSENT.iter().map(|s| s.to_string()).collect(),
    }
}

fn remove_key(env: &mut BTreeMap<String, String>, entry: &str) {
    match entry.strip_suffix('*') {
        Some(prefix) => env.retain(|k, _| !k.starts_with(prefix)),
        None => {
            env.remove(entry);
        }
    }
}

/// Apply a [`LeadEnv`] to a composed env: removals first (exact keys and
/// `*` prefixes), then the additions; `TMUX*`/`ITERM_*`/`LC_TERMINAL*` are
/// removed even when the `LeadEnv` came from elsewhere. This is the function
/// T10 calls on the lead's composed env.
pub fn apply(env: &mut BTreeMap<String, String>, lead: &LeadEnv) {
    for entry in lead.remove.iter().map(String::as_str).chain(ABSENT.iter().copied()) {
        // The token is in ABSENT (for panes) but a lead gets it from `set`.
        remove_key(env, entry);
    }
    for (k, v) in &lead.set {
        env.insert(k.clone(), v.clone());
    }
}

/// A pane's env (§7.4.5): the lead's composed env (`base` + the lead's
/// additions without the token), with no token, no `TMUX*` and no inherited
/// `GILDE_SPAWN_ID`.
pub fn pane_env(mut base: BTreeMap<String, String>, lead_set: &[(String, String)]) -> Vec<(String, String)> {
    let lead = LeadEnv {
        set: lead_set
            .iter()
            .filter(|(k, _)| k != proto::ENV_PANE_HOST_TOKEN)
            .cloned()
            .collect(),
        remove: Vec::new(),
    };
    apply(&mut base, &lead);
    base.remove(proto::ENV_PANE_HOST_TOKEN);
    base.remove(ENV_SPAWN_ID);
    base.into_iter().collect()
}

/// The `gilde-tmux` next to the running app binary (`Contents/MacOS/`).
pub fn bundled_shim() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let candidate = exe.parent()?.join("gilde-tmux");
    is_executable_file(&candidate).then_some(candidate)
}

/// Copy `src` to `<shim_dir>/tmux` (mode 0755; `shim_dir` and its parent
/// 0700) unless an executable copy is already there (§4.7: a copy, never a
/// symlink into the bundle). The copy is written to a temp file and renamed,
/// so a crash never leaves a truncated shim behind.
pub fn install_shim(src: &Path, shim_dir: &Path) -> io::Result<PathBuf> {
    let target = shim_dir.join("tmux");
    if is_executable_file(&target) && !std::fs::symlink_metadata(&target)?.file_type().is_symlink() {
        return Ok(target);
    }
    if let Some(root) = shim_dir.parent() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(root)?;
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))?;
    }
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(shim_dir)?;
    std::fs::set_permissions(shim_dir, std::fs::Permissions::from_mode(0o700))?;
    let tmp = shim_dir.join(format!(".tmux.tmp-{}", std::process::id()));
    std::fs::copy(src, &tmp)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&tmp, &target)?;
    Ok(target)
}

/// Delete the shim dirs of other builds that no live session references
/// (§4.7). Only plain directories directly below `shim_root` whose names look
/// like build ids are touched; symlinks are never followed. Returns how many
/// were removed.
pub fn prune_shims(shim_root: &Path, keep: &[&str]) -> io::Result<usize> {
    let mut removed = 0;
    for entry in std::fs::read_dir(shim_root)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if keep.contains(&name) || !crate::is_valid_build_id(name) {
            continue;
        }
        if !entry.file_type()?.is_dir() {
            continue;
        }
        std::fs::remove_dir_all(entry.path())?;
        removed += 1;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exec_file(p: &Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"#!/bin/false\n").unwrap();
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// T-team-1 (unit half): the lead env lacks TMUX*, ITERM_*, LC_TERMINAL*
    /// and carries TERM_PROGRAM=gilde-code, the agent-teams flag and the shim
    /// dir first on PATH, whatever the composed env brought along.
    #[test]
    fn t_team_1_the_hosted_lead_env() {
        let root = Path::new("/gh/shim");
        let shim = root.join("1.0.0");
        let le = build(Path::new("/gh/run/ph.sock"), &shim, root, &"a".repeat(64), "/usr/bin:/gh/shim/0.9.0:/bin", None);
        let mut env: BTreeMap<String, String> = [
            ("PATH", "/usr/bin:/bin"),
            ("HOME", "/Users/t"),
            ("TMUX", "/tmp/tmux-501/default,1,0"),
            ("TMUX_PANE", "%3"),
            ("ITERM_SESSION_ID", "w0t0p0:x"),
            ("ITERM_PROFILE", "Default"),
            ("LC_TERMINAL", "iTerm2"),
            ("LC_TERMINAL_VERSION", "3.5"),
            ("TERM_PROGRAM", "iTerm.app"),
            ("TERM_PROGRAM_VERSION", "3.5"),
            ("CLAUDECODE", "1"),
            ("CLAUDE_CODE_TEAMMATE_COMMAND", "/evil"),
            ("GILDE_SPAWN_ID", "lead-marker"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        apply(&mut env, &le);
        for absent in [
            "TMUX",
            "TMUX_PANE",
            "ITERM_SESSION_ID",
            "ITERM_PROFILE",
            "LC_TERMINAL",
            "LC_TERMINAL_VERSION",
            "TERM_PROGRAM_VERSION",
            "CLAUDECODE",
            "CLAUDE_CODE_TEAMMATE_COMMAND",
        ] {
            assert!(!env.contains_key(absent), "{absent} must be absent");
        }
        assert_eq!(env["TERM_PROGRAM"], "gilde-code");
        assert_eq!(env[ENV_AGENT_TEAMS], "1");
        assert_eq!(env["PATH"], "/gh/shim/1.0.0:/usr/bin:/bin", "shim first, other builds' shims dropped");
        assert_eq!(env[proto::ENV_PANE_HOST], "/gh/run/ph.sock");
        assert_eq!(env[proto::ENV_PANE_HOST_TOKEN], "a".repeat(64));
        assert_eq!(env[proto::ENV_REAL_TMUX], "");
        assert_eq!(env["GILDE_SPAWN_ID"], "lead-marker", "the lead keeps its own marker");
        let shown = format!("{le:?}");
        assert!(!shown.contains(&"a".repeat(64)), "Debug never shows the token");
    }

    #[test]
    fn the_pane_env_has_no_token_no_tmux_and_no_inherited_marker() {
        let base: BTreeMap<String, String> = [("PATH", "/usr/bin"), ("TMUX", "x"), ("GILDE_SPAWN_ID", "lead"), ("GILDE_APP_INSTANCE", "i")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let set = vec![
            ("PATH".to_string(), "/gh/shim/1:/usr/bin".to_string()),
            (proto::ENV_PANE_HOST.to_string(), "/gh/run/ph.sock".to_string()),
            (proto::ENV_PANE_HOST_TOKEN.to_string(), "b".repeat(64)),
        ];
        let env: BTreeMap<String, String> = pane_env(base, &set).into_iter().collect();
        assert!(!env.contains_key(proto::ENV_PANE_HOST_TOKEN));
        assert!(!env.contains_key("TMUX"));
        assert!(!env.contains_key("GILDE_SPAWN_ID"));
        assert_eq!(env["GILDE_APP_INSTANCE"], "i");
        assert_eq!(env[proto::ENV_PANE_HOST], "/gh/run/ph.sock");
        assert_eq!(env["PATH"], "/gh/shim/1:/usr/bin");
    }

    #[test]
    fn the_real_tmux_is_found_on_path_minus_the_shim_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("shim");
        exec_file(&root.join("1.0.0").join("tmux"));
        exec_file(&dir.path().join("bin").join("tmux"));
        std::fs::create_dir_all(dir.path().join("empty")).unwrap();
        let path = format!(
            "{}:relative:{}:{}",
            root.join("1.0.0").display(),
            dir.path().join("empty").display(),
            dir.path().join("bin").display()
        );
        assert_eq!(find_real_tmux(&path, &root), Some(dir.path().join("bin").join("tmux")));
        let only_shim = format!("{}", root.join("1.0.0").display());
        assert_eq!(find_real_tmux(&only_shim, &root), None);
        // A symlink elsewhere that resolves into the shim root is not "real".
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(root.join("1.0.0").join("tmux"), elsewhere.join("tmux")).unwrap();
        assert_eq!(find_real_tmux(&elsewhere.display().to_string(), &root), None);
    }

    #[test]
    fn the_shim_is_copied_once_with_private_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("gilde-tmux");
        exec_file(&src);
        let shim_dir = dir.path().join("shim").join("1.0.0");
        let target = install_shim(&src, &shim_dir).unwrap();
        assert_eq!(target, shim_dir.join("tmux"));
        let meta = std::fs::symlink_metadata(&target).unwrap();
        assert!(meta.file_type().is_file(), "a copy, not a symlink");
        assert_eq!(meta.permissions().mode() & 0o777, 0o755);
        assert_eq!(std::fs::metadata(&shim_dir).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(dir.path().join("shim")).unwrap().permissions().mode() & 0o777, 0o700);
        // Present already: left alone.
        std::fs::write(&src, b"changed").unwrap();
        install_shim(&src, &shim_dir).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"#!/bin/false\n");
    }

    #[test]
    fn old_shim_dirs_are_pruned_but_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("shim");
        for b in ["1.0.0", "0.9.0", "0.8.0+abc.20261022T000000"] {
            std::fs::create_dir_all(root.join(b)).unwrap();
        }
        std::fs::create_dir_all(root.join(".hidden")).unwrap();
        std::fs::write(root.join("0.7.0"), b"a file").unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("0.6.0")).unwrap();
        assert_eq!(prune_shims(&root, &["1.0.0", "0.9.0"]).unwrap(), 1);
        assert!(root.join("1.0.0").is_dir() && root.join("0.9.0").is_dir());
        assert!(!root.join("0.8.0+abc.20261022T000000").exists());
        assert!(root.join(".hidden").is_dir(), "not a build id");
        assert!(root.join("0.7.0").is_file(), "not a directory");
        assert!(outside.is_dir(), "symlinks are never followed");
    }
}
