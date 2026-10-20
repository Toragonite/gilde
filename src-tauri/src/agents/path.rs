//! Login-shell `PATH` resolution — DESIGN §6-6, DESIGN R8, M1-PLAN T09.1–T09.3.
//!
//! ## Why this file exists at all
//!
//! A macOS app launched from Finder inherits a `PATH` of roughly
//! `/usr/bin:/bin:/usr/sbin:/sbin`. Every agent CLI on this machine lives
//! somewhere else — `claude` and `codex` under `~/.nvm/versions/node/<v>/bin`,
//! `agy` under `~/.local/bin`. Worse, `claude` and `codex` are `#!/usr/bin/env
//! node` shims: even with an absolute path to the shim, the child needs `node`
//! **on its own `PATH`** or it dies with `env: node: No such file or directory`.
//! (Measured on this machine, 2026-09-07.) So resolution is not a nicety; the
//! app is non-functional without it.
//!
//! ## The resolution ladder (M1-PLAN T09 acceptance 1)
//!
//! 1. `GILDE_PATH_OVERRIDE` / `config.agents.pathOverride`
//! 2. fingerprint-validated `<app-data>/cache/path.json`
//! 3. **sentinel** login-shell probe: `$SHELL -l -i -c 'printf "__GILDE_B__%s__GILDE_E__" "$PATH"'`
//! 4. the same probe without `-i` (some `.zshrc`s refuse to run interactively)
//! 5. the inherited `PATH`
//!
//! and **static candidates are merged into every one of those outcomes**, so a
//! stale cache or a hostile `.zshrc` can never hide `~/.nvm/...` or
//! `~/.local/bin`. Non-existent directories are dropped, order is preserved,
//! duplicates collapse to their first occurrence.
//!
//! The sentinel matters: a login shell prints motd banners, `direnv` chatter and
//! version-manager noise on stdout. Taking the whole stdout as `PATH` would
//! poison the environment of every agent we launch. Anything outside
//! `__GILDE_B__…__GILDE_E__` is discarded.
//!
//! The 3-second hard cap matters for the same reason R8 is on the risk list: a
//! login shell can block forever on a network mount, a `read`, or a
//! `keychain` prompt. The probe is killed by **process group** (it is spawned
//! into its own group), because the thing that hangs is usually a grandchild.

use std::collections::BTreeSet;
use std::io::Read as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::model::AppConfig;
use crate::paths::AppPaths;

/// Environment variable that short-circuits the whole ladder.
pub const PATH_OVERRIDE_ENV: &str = "GILDE_PATH_OVERRIDE";

/// Hard cap on the login-shell probe (M1-PLAN T09.1). Exceeding it is normal,
/// not exceptional: we fall through to the next rung and carry a warning.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Grace between `SIGTERM` and `SIGKILL` for a probe that overran.
const PROBE_KILL_GRACE: Duration = Duration::from_millis(200);

/// Sentinel bracket. Long and unlikely enough that a shell rc file printing it
/// by accident is not a scenario worth designing for.
const SENTINEL_BEGIN: &str = "__GILDE_B__";
const SENTINEL_END: &str = "__GILDE_E__";

/// The one command we ask a login shell to run. A compile-time constant with no
/// interpolation whatsoever — there is no injection surface here.
const PROBE_SCRIPT: &str = "printf '__GILDE_B__%s__GILDE_E__' \"$PATH\"";

/// Fallback when `$SHELL` is unset (Finder launches sometimes have no `SHELL`).
const DEFAULT_SHELL: &str = "/bin/zsh";

/// Always present at the end so a child always finds `sh`, `git`, `ps`.
const SYSTEM_FLOOR: &[&str] = &["/usr/bin", "/bin", "/usr/sbin", "/sbin"];

/// Version of the on-disk `cache/path.json` envelope.
const CACHE_VERSION: u32 = 1;

/// Shell rc files whose mtime invalidates the cache (M1-PLAN T09.2).
const FINGERPRINT_RC_FILES: &[&str] = &[
    ".zshenv",
    ".zprofile",
    ".zshrc",
    ".bash_profile",
    ".bashrc",
];

// ---------------------------------------------------------------------------
// public shapes
// ---------------------------------------------------------------------------

/// Which rung of the ladder produced the answer. Surfaced in
/// `app_diagnostics()` so "why can't it find `codex`" is one click away.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PathSource {
    /// `GILDE_PATH_OVERRIDE` or `config.agents.pathOverride`.
    Override,
    /// `cache/path.json`, fingerprint matched.
    Cache,
    /// `$SHELL -l -i -c` succeeded.
    LoginShellInteractive,
    /// `$SHELL -l -c` succeeded (the `-i` attempt did not).
    LoginShell,
    /// Every probe failed; the inherited `PATH` plus the static candidates.
    Static,
}

impl PathSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            PathSource::Override => "override",
            PathSource::Cache => "cache",
            PathSource::LoginShellInteractive => "loginShellInteractive",
            PathSource::LoginShell => "loginShell",
            PathSource::Static => "static",
        }
    }
}

impl std::fmt::Display for PathSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The resolved search path plus everything needed to explain it.
///
/// `entries` serializes as `resolved` because that is the name
/// `src/lib/ipc.ts::PathReport` (owned by T04) declares.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PathReport {
    #[serde(rename = "resolved")]
    pub entries: Vec<PathBuf>,
    pub source: PathSource,
    pub fingerprint: String,
    /// Unix millis.
    pub probed_at: i64,
    /// Non-fatal problems (probe timed out, cache unreadable, …). Rendered in
    /// the diagnostics sheet; never thrown.
    #[serde(default)]
    pub warnings: Vec<String>,
}

impl PathReport {
    /// The value to hand a child process as `PATH`.
    pub fn joined(&self) -> String {
        self.entries
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(":")
    }

    /// Whether `dir` survived into the resolved list.
    pub fn contains(&self, dir: impl AsRef<Path>) -> bool {
        let dir = dir.as_ref();
        self.entries.iter().any(|e| e == dir)
    }
}

// ---------------------------------------------------------------------------
// entry point
// ---------------------------------------------------------------------------

/// Resolve the search path for every child this app spawns.
///
/// Never fails and never blocks longer than two [`PROBE_TIMEOUT`]s (the `-i`
/// attempt plus the `-l` retry). A failure at any rung becomes a warning on the
/// report, not an error to the caller.
pub fn resolve_path(cfg: &AppConfig, paths: &AppPaths) -> PathReport {
    let mut warnings = Vec::new();
    let home = paths.home_dir().to_path_buf();
    let fingerprint = fingerprint(&home);

    // 1. explicit override -------------------------------------------------
    let override_raw = std::env::var(PATH_OVERRIDE_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            cfg.agents
                .path_override
                .clone()
                .filter(|v| !v.trim().is_empty())
        });

    if let Some(raw) = override_raw {
        let report = finish(split(&raw), PathSource::Override, fingerprint, warnings, &home);
        // An override is authoritative about *ordering*, not about hiding the
        // static candidates: DESIGN's rule is that they are always merged.
        return report;
    }

    // 2. fingerprint-validated cache --------------------------------------
    match read_cache(&paths.path_cache(), &fingerprint) {
        Ok(Some(entries)) => {
            return finish(entries, PathSource::Cache, fingerprint, warnings, &home);
        }
        Ok(None) => {}
        Err(e) => warnings.push(format!("path cache unusable: {e}")),
    }

    // 3./4. login-shell probe ---------------------------------------------
    let shell = login_shell();
    let mut probed: Option<(Vec<PathBuf>, PathSource)> = None;

    if is_executable(&shell) {
        match probe_shell(&shell, true) {
            Ok(entries) => probed = Some((entries, PathSource::LoginShellInteractive)),
            Err(e) => {
                warnings.push(format!("{} -l -i -c failed: {e}", shell.display()));
                match probe_shell(&shell, false) {
                    Ok(entries) => probed = Some((entries, PathSource::LoginShell)),
                    Err(e) => warnings.push(format!("{} -l -c failed: {e}", shell.display())),
                }
            }
        }
    } else {
        warnings.push(format!("login shell {} is not executable", shell.display()));
    }

    // 5. the inherited PATH ------------------------------------------------
    let (entries, source) = probed.unwrap_or_else(|| {
        (
            split(&std::env::var("PATH").unwrap_or_default()),
            PathSource::Static,
        )
    });

    let report = finish(entries, source, fingerprint, warnings, &home);
    if let Err(e) = write_cache(&paths.path_cache(), &report) {
        tracing::warn!(error = %e, "could not persist the resolved PATH cache");
    }
    report
}

/// Merge the static candidates, drop what does not exist, dedupe, and stamp.
fn finish(
    primary: Vec<PathBuf>,
    source: PathSource,
    fingerprint: String,
    warnings: Vec<String>,
    home: &Path,
) -> PathReport {
    let mut merged = primary;
    merged.extend(static_candidates(home));
    merged.extend(SYSTEM_FLOOR.iter().map(PathBuf::from));

    PathReport {
        entries: dedupe_existing(merged),
        source,
        fingerprint,
        probed_at: now_ms(),
        warnings,
    }
}

// ---------------------------------------------------------------------------
// static candidates
// ---------------------------------------------------------------------------

/// Directories merged into every resolution (M1-PLAN T09.1).
///
/// nvm first: on this machine `claude` and `codex` live there, and a `node`
/// earlier on the path than nvm's would silently change which runtime the shims
/// pick up.
pub fn static_candidates(home: &Path) -> Vec<PathBuf> {
    let mut out = nvm_candidates(home);
    out.push(PathBuf::from("/opt/homebrew/bin"));
    out.push(PathBuf::from("/usr/local/bin"));
    for rel in [
        ".local/bin",
        ".cargo/bin",
        ".bun/bin",
        ".volta/bin",
        ".asdf/shims",
        "Library/pnpm",
    ] {
        out.push(home.join(rel));
    }
    out
}

/// `~/.nvm/versions/node/*/bin`, newest first, with the version named by
/// `~/.nvm/alias/default` promoted to the front.
///
/// The alias file holds a *partial* version ("20") or another alias name
/// ("lts/iron"), so it is treated as a prefix hint and never as a hard
/// requirement — an alias pointing at an uninstalled version must not empty the
/// list.
pub fn nvm_candidates(home: &Path) -> Vec<PathBuf> {
    let versions_dir = home.join(".nvm/versions/node");
    let Ok(read) = std::fs::read_dir(&versions_dir) else {
        return Vec::new();
    };

    let mut versions: Vec<(Vec<u64>, String)> = read
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .map(|name| (parse_version(&name), name))
        .collect();
    // Newest first, ties broken by name for determinism.
    versions.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));

    let default_alias = std::fs::read_to_string(home.join(".nvm/alias/default"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let mut ordered: Vec<String> = Vec::new();
    if let Some(alias) = &default_alias {
        let wanted = format!("v{}", alias.trim_start_matches('v'));
        if let Some(hit) = versions.iter().find(|(_, name)| {
            *name == wanted || name.strip_prefix(&wanted).is_some_and(|r| r.starts_with('.'))
        }) {
            ordered.push(hit.1.clone());
        }
    }
    for (_, name) in &versions {
        if !ordered.contains(name) {
            ordered.push(name.clone());
        }
    }

    ordered
        .into_iter()
        .map(|name| versions_dir.join(name).join("bin"))
        .collect()
}

/// `"v20.19.2"` → `[20, 19, 2]`. Unparsable segments become 0, which sorts them
/// last without dropping the directory.
fn parse_version(name: &str) -> Vec<u64> {
    name.trim_start_matches('v')
        .split('.')
        .map(|seg| {
            seg.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse()
                .unwrap_or(0)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// the probe
// ---------------------------------------------------------------------------

/// The user's login shell.
pub fn login_shell() -> PathBuf {
    std::env::var("SHELL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SHELL))
}

/// Run the sentinel probe once. `interactive` adds `-i`.
///
/// Returns the entries between the sentinels, or a human-readable reason.
fn probe_shell(shell: &Path, interactive: bool) -> Result<Vec<PathBuf>, String> {
    let mut cmd = Command::new(shell);
    cmd.arg("-l");
    if interactive {
        cmd.arg("-i");
    }
    // The single sanctioned shell invocation in the codebase: the *point* is to
    // observe what the user's login shell computes for `PATH`. `PROBE_SCRIPT` is
    // a compile-time constant with no interpolation, so there is nothing to
    // inject into. Everything else in this app spawns argv arrays directly.
    cmd.arg("-c"); // gilde-lint-allow: shell — fixed script, see the comment above
    cmd.arg(PROBE_SCRIPT);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        // Own process group: a login shell that hangs usually hangs inside a
        // grandchild (`nvm`, `direnv`, a keychain prompt), and killing only the
        // direct child would leak it.
        .process_group(0);

    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let pid = child.id() as i32;

    let mut stdout = child.stdout.take().ok_or("no stdout pipe")?;
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });

    let deadline = Instant::now() + PROBE_TIMEOUT;
    let mut timed_out = false;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(e) => return Err(e.to_string()),
        }
        if Instant::now() >= deadline {
            timed_out = true;
            kill_group(pid);
            let _ = child.wait();
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }

    // Bounded either way: the reader ends at EOF, and EOF arrives once every
    // copy of the write end is closed.
    let out = rx
        .recv_timeout(Duration::from_millis(500))
        .unwrap_or_default();
    let text = String::from_utf8_lossy(&out);

    match extract_sentinel(&text) {
        Some(raw) if !raw.trim().is_empty() => Ok(split(raw)),
        _ if timed_out => Err(format!("timed out after {:?}", PROBE_TIMEOUT)),
        _ => Err("no sentinel in the shell's output".to_string()),
    }
}

/// `SIGTERM` then `SIGKILL` to the probe's own process group.
///
/// Not routed through `supervise::kill`: that path exists to protect processes
/// we merely *believe* are ours, verified against the PID registry. This one we
/// created microseconds ago, into a group of its own, and still hold the
/// `Child` for — there is nothing to disambiguate.
fn kill_group(pid: i32) {
    use nix::sys::signal::{killpg, Signal};
    use nix::unistd::Pid;

    if pid <= 0 {
        return;
    }
    let group = Pid::from_raw(pid);
    let _ = killpg(group, Signal::SIGTERM);
    thread::sleep(PROBE_KILL_GRACE);
    let _ = killpg(group, Signal::SIGKILL);
}

/// The text between the sentinels, if both are present in order.
fn extract_sentinel(text: &str) -> Option<&str> {
    let start = text.find(SENTINEL_BEGIN)? + SENTINEL_BEGIN.len();
    let rest = &text[start..];
    let end = rest.find(SENTINEL_END)?;
    Some(&rest[..end])
}

// ---------------------------------------------------------------------------
// fingerprint + cache
// ---------------------------------------------------------------------------

/// `sha256` over the mtimes of the shell rc files, `/etc/paths`,
/// `/etc/paths.d/*`, plus `$SHELL` and the uid (M1-PLAN T09.2).
///
/// Missing files contribute a `missing` marker rather than nothing, so deleting
/// `~/.zshrc` invalidates the cache just like editing it.
pub fn fingerprint(home: &Path) -> String {
    let mut hasher = Sha256::new();

    let mut inputs: Vec<PathBuf> = FINGERPRINT_RC_FILES.iter().map(|f| home.join(f)).collect();
    inputs.push(PathBuf::from("/etc/paths"));

    let mut paths_d: BTreeSet<PathBuf> = BTreeSet::new();
    if let Ok(read) = std::fs::read_dir("/etc/paths.d") {
        for entry in read.flatten() {
            paths_d.insert(entry.path());
        }
    }
    inputs.extend(paths_d);

    for path in inputs {
        hasher.update(path.as_os_str().as_encoded_bytes());
        hasher.update([0u8]);
        match std::fs::metadata(&path).and_then(|m| m.modified()) {
            Ok(mtime) => {
                let nanos = mtime
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                hasher.update(nanos.to_le_bytes());
            }
            Err(_) => hasher.update(b"missing"),
        }
        hasher.update([0u8]);
    }

    hasher.update(login_shell().as_os_str().as_encoded_bytes());
    hasher.update([0u8]);
    hasher.update(unsafe { libc::getuid() }.to_le_bytes());

    hex::encode(hasher.finalize())
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PathCacheFile {
    version: u32,
    fingerprint: String,
    entries: Vec<PathBuf>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    probed_at: i64,
}

/// `Ok(Some(entries))` only when the file parses **and** the fingerprint still
/// matches. A stale or foreign cache is a miss, never an error.
fn read_cache(path: &Path, fingerprint: &str) -> Result<Option<Vec<PathBuf>>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    let cached: PathCacheFile = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    if cached.version != CACHE_VERSION || cached.fingerprint != fingerprint {
        return Ok(None);
    }
    if cached.entries.is_empty() {
        return Ok(None);
    }
    Ok(Some(cached.entries))
}

fn write_cache(path: &Path, report: &PathReport) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = PathCacheFile {
        version: CACHE_VERSION,
        fingerprint: report.fingerprint.clone(),
        entries: report.entries.clone(),
        source: Some(report.source.as_str().to_string()),
        probed_at: report.probed_at,
    };
    let body = serde_json::to_vec_pretty(&file)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

// ---------------------------------------------------------------------------
// small helpers
// ---------------------------------------------------------------------------

/// Split a `PATH`-shaped string. Empty segments (the `::` that means "cwd" in
/// POSIX) are dropped — an implicit cwd on an agent's `PATH` is a footgun.
fn split(raw: &str) -> Vec<PathBuf> {
    raw.split(':')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Drop non-directories, collapse duplicates, keep first-seen order.
fn dedupe_existing(entries: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        if !entry.is_dir() {
            continue;
        }
        if seen.insert(entry.clone()) {
            out.push(entry);
        }
    }
    out
}

/// Whether `path` is a regular file (or a link to one) with an execute bit.
pub fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_paths() -> (tempfile::TempDir, AppPaths) {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = AppPaths::for_home(dir.path());
        paths.ensure_all().expect("dirs");
        (dir, paths)
    }

    #[test]
    fn sentinel_extraction_ignores_login_banners() {
        let noisy = "Last login: Sun Sep  7\nnvm: using 20\n__GILDE_B__/a:/b__GILDE_E__\n";
        assert_eq!(extract_sentinel(noisy), Some("/a:/b"));
        assert_eq!(extract_sentinel("no sentinel here"), None);
        assert_eq!(extract_sentinel("__GILDE_B__unterminated"), None);
    }

    #[test]
    fn split_drops_empty_segments_so_no_child_gets_an_implicit_cwd() {
        assert_eq!(
            split("/a::/b: /c :"),
            vec![PathBuf::from("/a"), PathBuf::from("/b"), PathBuf::from("/c")]
        );
    }

    #[test]
    fn dedupe_keeps_first_occurrence_and_drops_what_does_not_exist() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        fs::create_dir_all(&a).expect("a");
        fs::create_dir_all(&b).expect("b");
        let gone = dir.path().join("gone");

        let out = dedupe_existing(vec![a.clone(), gone, b.clone(), a.clone()]);
        assert_eq!(out, vec![a, b]);
    }

    #[test]
    fn nvm_versions_are_newest_first_with_the_default_alias_promoted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path();
        for v in ["v18.20.4", "v20.19.2", "v22.1.0"] {
            fs::create_dir_all(home.join(".nvm/versions/node").join(v).join("bin")).expect("mk");
        }
        fs::create_dir_all(home.join(".nvm/alias")).expect("alias dir");
        fs::write(home.join(".nvm/alias/default"), "20\n").expect("alias");

        let names: Vec<String> = nvm_candidates(home)
            .iter()
            .map(|p| {
                p.parent()
                    .and_then(|d| d.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            })
            .collect();

        assert_eq!(
            names,
            vec!["v20.19.2", "v22.1.0", "v18.20.4"],
            "the default alias wins, then newest first, each listed once"
        );
    }

    #[test]
    fn an_unknown_default_alias_does_not_empty_the_list() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path();
        fs::create_dir_all(home.join(".nvm/versions/node/v20.19.2/bin")).expect("mk");
        fs::create_dir_all(home.join(".nvm/alias")).expect("alias dir");
        fs::write(home.join(".nvm/alias/default"), "lts/iron").expect("alias");

        assert_eq!(
            nvm_candidates(home),
            vec![home.join(".nvm/versions/node/v20.19.2/bin")]
        );
    }

    #[test]
    fn fingerprint_changes_when_an_rc_file_changes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path();
        let before = fingerprint(home);

        fs::write(home.join(".zshrc"), "export PATH=/x:$PATH\n").expect("write");
        let after = fingerprint(home);

        assert_ne!(before, after, "a new .zshrc must invalidate the cache");
        assert_eq!(after, fingerprint(home), "and be stable when nothing moves");
    }

    #[test]
    fn a_cache_with_a_foreign_fingerprint_is_a_miss_not_an_error() {
        let (_dir, paths) = temp_paths();
        let file = paths.path_cache();
        fs::write(
            &file,
            serde_json::json!({
                "version": 1, "fingerprint": "old", "entries": ["/usr/bin"]
            })
            .to_string(),
        )
        .expect("write");

        assert_eq!(read_cache(&file, "new").expect("read"), None);
        assert_eq!(
            read_cache(&file, "old").expect("read"),
            Some(vec![PathBuf::from("/usr/bin")])
        );
    }

    #[test]
    #[serial_test::serial(gilde_env)]
    fn a_corrupt_cache_reports_a_warning_instead_of_failing_resolution() {
        let (_dir, paths) = temp_paths();
        fs::write(paths.path_cache(), "{ not json").expect("write");

        let report = temp_env(&[("SHELL", Some("/nonexistent/shell"))], || {
            resolve_path(&AppConfig::default(), &paths)
        });
        assert!(
            report.warnings.iter().any(|w| w.contains("cache")),
            "expected a cache warning, got {:?}",
            report.warnings
        );
        assert!(!report.entries.is_empty());
    }

    /// M1-PLAN T09 acceptance 3, second half: a shell that never returns must
    /// not stall the caller.
    #[test]
    #[serial_test::serial(gilde_env)]
    fn a_hanging_login_shell_times_out_and_falls_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = AppPaths::for_home(dir.path());
        paths.ensure_all().expect("dirs");

        let fake = dir.path().join("hanging-shell");
        fs::write(&fake, "#!/bin/sh\nsleep 30\n").expect("write");
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).expect("chmod");
        }

        let started = Instant::now();
        let report = temp_env(
            &[("SHELL", Some(fake.to_string_lossy().as_ref())), ("PATH", Some("/usr/bin:/bin"))],
            || resolve_path(&AppConfig::default(), &paths),
        );
        let elapsed = started.elapsed();

        assert_eq!(report.source, PathSource::Static, "must fall through");
        assert!(
            elapsed < Duration::from_secs(12),
            "two 3s probes plus kill grace, not 30s: took {elapsed:?}"
        );
        assert!(
            report.warnings.iter().any(|w| w.contains("timed out")),
            "expected a timeout warning, got {:?}",
            report.warnings
        );
        assert!(report.contains("/usr/bin"));
    }

    /// M1-PLAN T09 acceptance 3, first half. With nothing but `/usr/bin:/bin`
    /// on the inherited `PATH`, every static candidate that exists on this
    /// machine — `~/.nvm/versions/node/<v>/bin` and `~/.local/bin` included —
    /// still has to come out the other side.
    #[test]
    #[serial_test::serial(gilde_env)]
    fn a_stripped_path_still_resolves_the_static_candidates() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A home whose *layout* is the real one, so the assertion is about this
        // machine (acceptance 5), while the cache stays inside the temp dir.
        let home = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| dir.path().to_path_buf());
        let paths = AppPaths::new(dir.path().join("app-data"), home.clone());
        paths.ensure_all().expect("dirs");

        let expected: Vec<PathBuf> = static_candidates(&home)
            .into_iter()
            .filter(|p| p.is_dir())
            .collect();

        // A shell that cannot run forces the ladder down to the static rung,
        // which is the branch acceptance 3 is really about.
        let report = temp_env(
            &[
                ("PATH", Some("/usr/bin:/bin")),
                ("SHELL", Some("/nonexistent/shell")),
                (PATH_OVERRIDE_ENV, None),
            ],
            || resolve_path(&AppConfig::default(), &paths),
        );

        assert_eq!(report.source, PathSource::Static);
        for candidate in &expected {
            assert!(
                report.contains(candidate),
                "{} must survive into the resolved PATH: {:?}",
                candidate.display(),
                report.entries
            );
        }
        assert!(report.contains("/usr/bin"), "the system floor is always there");

        // The two directories acceptance 3 names by hand, asserted by name on
        // the machine T09 was measured on. Elsewhere they simply do not exist,
        // and the loop above has already covered whatever does.
        for named in [
            home.join(".nvm/versions/node/v20.19.2/bin"),
            home.join(".local/bin"),
        ] {
            if named.is_dir() {
                assert!(
                    report.contains(&named),
                    "{} must be found with nothing but /usr/bin:/bin inherited: {:?}",
                    named.display(),
                    report.entries
                );
            } else {
                eprintln!("skipped {}: not present on this machine", named.display());
            }
        }
    }

    /// Set env vars, run `f`, restore. Serialized by `serial_test` at the call
    /// sites that need it; the two users here do not overlap in variables with
    /// any other test in this crate.
    fn temp_env<T>(vars: &[(&str, Option<&str>)], f: impl FnOnce() -> T) -> T {
        let saved: Vec<(String, Option<String>)> = vars
            .iter()
            .map(|(k, _)| ((*k).to_string(), std::env::var(k).ok()))
            .collect();
        for (k, v) in vars {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        let out = f();
        for (k, v) in saved {
            match v {
                Some(v) => std::env::set_var(&k, v),
                None => std::env::remove_var(&k),
            }
        }
        out
    }
}
