//! The editor CLI / `open -b` launcher (contract §9.4) — a launch site of
//! §9.3, the only one in `ide/`.
//!
//! Owner: T11.
//!
//! * **argv**: the bundle CLI (`…/Contents/Resources/app/bin/code` or
//!   `…/bin/cursor`) with `--goto <abs>:<line>` / `--diff <left> <right>` /
//!   `<path>`; without a bundle CLI, `/usr/bin/open -b <bundleId> <abs path>`
//!   (no line, and no diff → `UNSUPPORTED{diffNotSupportedByEditor}`). Paths
//!   are always absolute (they start with `/`, so they can never be read as a
//!   flag) and never interpolated into a shell string.
//! * **registry**: every launch is planned through `SupervisorApi` as kind
//!   `ide` before it exists; a refusal launches nothing.
//! * **env**: a base allowlist (`PATH`, `HOME`, `USER`, `LOGNAME`, `LANG`,
//!   `LC_ALL`, `TMPDIR`) plus `GILDE_SPAWN_ID`; nothing else is inherited.
//! * **timeout**: 10 s, then the launcher's process group gets SIGTERM.
//! * **diff inputs**: the base side is written into
//!   `<app_data>/tmp/diff/<uuid>/<basename>` (file 0400, dir 0700), swept
//!   after 24 h and at quit ([`sweep_diff_dir`]).
//!
//! No test launches an editor: tests use a fake [`Launcher`]; the real one is
//! exercised against harmless programs and by one `#[ignore]` test.

use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::AsyncReadExt as _;

use crate::contract::activity::ExitInfo;
use crate::contract::common::AbsPath;
use crate::contract::error::IpcError;
use crate::contract::process::ProcKind;
use crate::contract::services::{SpawnRequest, SupervisorApi};
use crate::ide::editors::{bundle_id, FoundEditor};

/// The launcher's wall-clock cap (§9.4).
pub const IDE_TIMEOUT: Duration = Duration::from_secs(10);
/// `/usr/bin/open` (allowlisted for kind `ide`, §9.3).
pub const OPEN_BIN: &str = "/usr/bin/open";
/// Diff temp dirs older than this are deleted (§9.4).
pub const DIFF_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// The largest base side written for a diff.
pub const DIFF_CAP: usize = 64 * 1024 * 1024;

/// What to open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenAction {
    File { path: PathBuf, line: Option<u32> },
    Diff { left: PathBuf, right: PathBuf },
    Folder { path: PathBuf },
}

/// One launch: an absolute executable and its argv (no shell).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    pub argv0: PathBuf,
    pub args: Vec<String>,
}

fn path_arg(p: &Path) -> Result<String, IpcError> {
    let s = p.to_str().ok_or_else(|| IpcError::not_found("file"))?;
    if !s.starts_with('/') {
        return Err(IpcError::internal("ide-relative-path"));
    }
    Ok(s.to_string())
}

/// The argv of `action` for `editor` (§9.4).
pub fn plan(editor: &FoundEditor, action: &OpenAction) -> Result<LaunchPlan, IpcError> {
    match (&editor.cli, action) {
        (Some(cli), OpenAction::File { path, line }) => {
            let p = path_arg(path)?;
            let args = match line {
                Some(l) => vec!["--goto".to_string(), format!("{p}:{l}")],
                None => vec![p],
            };
            Ok(LaunchPlan { argv0: cli.clone(), args })
        }
        (Some(cli), OpenAction::Diff { left, right }) => Ok(LaunchPlan {
            argv0: cli.clone(),
            args: vec!["--diff".to_string(), path_arg(left)?, path_arg(right)?],
        }),
        (Some(cli), OpenAction::Folder { path }) => Ok(LaunchPlan { argv0: cli.clone(), args: vec![path_arg(path)?] }),
        (None, OpenAction::Diff { .. }) => Err(IpcError::unsupported("diffNotSupportedByEditor")),
        (None, OpenAction::File { path, .. }) | (None, OpenAction::Folder { path }) => Ok(LaunchPlan {
            argv0: PathBuf::from(OPEN_BIN),
            args: vec!["-b".to_string(), bundle_id(editor.editor).to_string(), path_arg(path)?],
        }),
    }
}

/// Runs a [`LaunchPlan`].
#[async_trait]
pub trait Launcher: Send + Sync {
    async fn launch(&self, plan: &LaunchPlan) -> Result<(), IpcError>;
}

/// The production launcher: write-before-spawn through `SupervisorApi`
/// (kind `ide`), base-allowlist env, 10 s cap.
pub struct SupervisedLauncher {
    api: Arc<dyn SupervisorApi>,
    timeout: Duration,
}

impl SupervisedLauncher {
    pub fn new(api: Arc<dyn SupervisorApi>) -> SupervisedLauncher {
        SupervisedLauncher { api, timeout: IDE_TIMEOUT }
    }

    /// A shorter cap (tests).
    pub fn with_timeout(mut self, timeout: Duration) -> SupervisedLauncher {
        self.timeout = timeout;
        self
    }
}

/// The base env allowlist of a launched editor CLI (§9.4).
pub fn base_env() -> Vec<(String, String)> {
    let mut env = Vec::new();
    let path = std::env::var("PATH").ok().filter(|p| !p.is_empty()).unwrap_or_else(|| "/usr/bin:/bin:/usr/sbin:/sbin".into());
    env.push(("PATH".to_string(), path));
    for key in ["HOME", "USER", "LOGNAME", "TMPDIR", "LANG", "LC_ALL"] {
        if let Ok(v) = std::env::var(key) {
            if !v.is_empty() {
                env.push((key.to_string(), v));
            }
        }
    }
    if !env.iter().any(|(k, _)| k == "LANG" || k == "LC_ALL") {
        env.push(("LANG".to_string(), "en_US.UTF-8".to_string()));
    }
    env
}

#[async_trait]
impl Launcher for SupervisedLauncher {
    async fn launch(&self, plan: &LaunchPlan) -> Result<(), IpcError> {
        if plan.argv0 != Path::new(OPEN_BIN) {
            // The resolved bundle CLI is allowlisted for `ide` rows (§9.3).
            crate::supervise::spawn::allow_executable(&plan.argv0, &[ProcKind::Ide]);
        }
        let argv0 = AbsPath::parse(plan.argv0.to_str().unwrap_or("")).map_err(|_| IpcError::spawn_failed())?;
        let env = base_env();
        let mut env_keys: Vec<String> = env.iter().map(|(k, _)| k.clone()).collect();
        env_keys.push("GILDE_SPAWN_ID".into());
        let ticket = self
            .api
            .plan_spawn(SpawnRequest {
                kind: ProcKind::Ide,
                argv0,
                args: plan.args.clone(),
                cwd: None,
                env_keys,
                parent_uid: None,
                generation: None,
                turn_id: None,
            })
            .await?;

        let mut cmd = tokio::process::Command::new(&plan.argv0);
        cmd.args(&plan.args)
            .env_clear()
            .envs(env)
            .env("GILDE_SPAWN_ID", &ticket.env_marker)
            .current_dir("/")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                self.api.mark_exited(&ticket, ExitInfo { code: None, signal: None }).await;
                return Err(IpcError::spawn_failed().with_detail(crate::error::home_to_tilde(&e.to_string())));
            }
        };
        let pid = child.id().map(|p| p as i32).unwrap_or(0);
        if let Err(e) = self.api.mark_running(&ticket, pid).await {
            tracing::warn!(target: "ide", code = e.code.as_str(), "could not record the editor launch as running");
        }
        let stderr = child.stderr.take();
        // Wait for the CLI itself, never for EOF on its stderr: an editor it
        // starts may inherit the pipe and keep it open.
        match tokio::time::timeout(self.timeout, child.wait()).await {
            Ok(Ok(status)) => {
                use std::os::unix::process::ExitStatusExt as _;
                self.api.mark_exited(&ticket, ExitInfo { code: status.code(), signal: status.signal() }).await;
                if status.success() {
                    Ok(())
                } else {
                    let mut tail = Vec::new();
                    if let Some(s) = stderr {
                        let _ = tokio::time::timeout(Duration::from_millis(500), s.take(4096).read_to_end(&mut tail)).await;
                    }
                    let text = String::from_utf8_lossy(&tail);
                    let clean = crate::render::sanitize::sanitize_display_text(
                        &crate::error::home_to_tilde(text.trim()),
                        crate::contract::display::TextCap::Bytes(4096),
                    );
                    Err(IpcError::spawn_failed().with_detail(clean.text))
                }
            }
            Ok(Err(e)) => {
                self.api.mark_exited(&ticket, ExitInfo { code: None, signal: None }).await;
                Err(IpcError::spawn_failed().with_detail(e.to_string()))
            }
            Err(_) => {
                // Our own child, not yet reaped (its pid cannot be reused): a
                // non-vendor helper, stopped with SIGTERM (§9.8).
                if pid > 0 {
                    let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid), nix::sys::signal::Signal::SIGTERM);
                }
                let status = child.wait().await.ok();
                use std::os::unix::process::ExitStatusExt as _;
                self.api
                    .mark_exited(&ticket, ExitInfo { code: status.and_then(|s| s.code()), signal: status.and_then(|s| s.signal()) })
                    .await;
                Err(IpcError::timeout())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// diff temp files (§9.4)
// ---------------------------------------------------------------------------

/// Write one side of a diff: `<root>/<uuid>/<name>` (dir 0700, file 0400,
/// created exclusively). `name` is reduced to its last component.
pub fn write_diff_side(root: &Path, name: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
    use std::io::Write as _;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(root)?;
    let dir = root.join(uuid::Uuid::new_v4().to_string());
    std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
    let base = Path::new(name)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty() && *n != "." && *n != "..")
        .unwrap_or("base");
    let file = dir.join(base);
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o400).open(&file)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(file)
}

/// Delete diff temp dirs under `root` older than `older_than`
/// (`Duration::ZERO` = all, at quit). Only `<uuid>` children are touched.
/// Returns how many were removed.
pub fn sweep_diff_dir(root: &Path, older_than: Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(root) else { return 0 };
    let mut removed = 0;
    for e in entries.flatten() {
        let p = e.path();
        let is_uuid = p.file_name().and_then(|n| n.to_str()).is_some_and(|n| uuid::Uuid::parse_str(n).is_ok());
        let Ok(meta) = std::fs::symlink_metadata(&p) else { continue };
        if !is_uuid || !meta.is_dir() {
            continue;
        }
        let old = meta.modified().ok().and_then(|m| m.elapsed().ok()).is_none_or(|age| age >= older_than);
        if old && std::fs::remove_dir_all(&p).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::ipc::changes::EditorKind;

    fn editor(cli: Option<&str>) -> FoundEditor {
        FoundEditor {
            editor: EditorKind::VsCode,
            app: PathBuf::from("/Applications/Visual Studio Code.app"),
            cli: cli.map(PathBuf::from),
        }
    }

    #[test]
    fn the_cli_argv_for_each_action() {
        let e = editor(Some("/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code"));
        let p = plan(&e, &OpenAction::File { path: "/w/a b.rs".into(), line: Some(12) }).unwrap();
        assert_eq!(p.args, vec!["--goto".to_string(), "/w/a b.rs:12".to_string()]);
        let p = plan(&e, &OpenAction::File { path: "/w/a.rs".into(), line: None }).unwrap();
        assert_eq!(p.args, vec!["/w/a.rs".to_string()]);
        let p = plan(&e, &OpenAction::Diff { left: "/t/base".into(), right: "/w/a.rs".into() }).unwrap();
        assert_eq!(p.args, vec!["--diff".to_string(), "/t/base".to_string(), "/w/a.rs".to_string()]);
        let p = plan(&e, &OpenAction::Folder { path: "/w".into() }).unwrap();
        assert_eq!(p.args, vec!["/w".to_string()]);
        assert!(p.argv0.ends_with("bin/code"));
    }

    #[test]
    fn without_a_bundle_cli_open_b_opens_files_but_not_diffs() {
        let e = editor(None);
        let p = plan(&e, &OpenAction::File { path: "/w/a.rs".into(), line: Some(3) }).unwrap();
        assert_eq!(p.argv0, PathBuf::from(OPEN_BIN));
        assert_eq!(p.args, vec!["-b".to_string(), "com.microsoft.VSCode".to_string(), "/w/a.rs".to_string()]);
        let err = plan(&e, &OpenAction::Diff { left: "/a".into(), right: "/b".into() }).unwrap_err();
        assert_eq!(err.params["cap"], "diffNotSupportedByEditor");
    }

    #[test]
    fn diff_sides_are_read_only_in_a_private_dir_and_swept() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("diff");
        let f = write_diff_side(&root, "src/../a.rs", b"base\n").unwrap();
        assert_eq!(f.file_name().unwrap(), "a.rs");
        assert_eq!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o400);
        assert_eq!(std::fs::metadata(f.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::read(&f).unwrap(), b"base\n");
        // A foreign entry is never touched.
        std::fs::write(root.join("keep.txt"), b"x").unwrap();
        assert_eq!(sweep_diff_dir(&root, DIFF_MAX_AGE), 0, "fresh dirs stay");
        assert_eq!(sweep_diff_dir(&root, Duration::ZERO), 1);
        assert!(!f.exists());
        assert!(root.join("keep.txt").exists());
    }

    #[test]
    fn the_env_is_an_allowlist() {
        let env = base_env();
        assert!(env.iter().any(|(k, _)| k == "PATH"));
        assert!(env.iter().all(|(k, _)| ["PATH", "HOME", "USER", "LOGNAME", "TMPDIR", "LANG", "LC_ALL"].contains(&k.as_str())));
    }
}
