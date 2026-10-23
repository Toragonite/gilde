//! Settings (contract §4.2.8, §9.6): ordinary and executable classes.
//!
//! Owner: T09.
//!
//! **Ordinary** keys (locale, theme, attention, notifications, `ide.editor`)
//! change with `settings_set` — a typed patch, unknown keys rejected by the
//! envelope, whole-patch validation. **Executable** keys
//! (`agents.<vendor>.binary`, `sidecar.nodePath`, `paths.override`,
//! `agents.envPassthrough`) change only with `settings_set_executable`, which
//! checks the value and shows a Rust-owned native confirmation naming the
//! exact value; the WebView cannot answer it. `applied:false` when cancelled.
//!
//! A45.1: a path value is resolved first, and the confirmation shows — and
//! the write applies — the **canonical** path (realpath), never the request's
//! string; any character left that does not print is shown escaped
//! ([`confirmation_text`]).

use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

use crate::commands::app::{state_of, ConfigState, PathsState};
use crate::config::model::AppConfig;
use crate::config::ConfigStore;
use crate::contract::common::AbsPath;
use crate::contract::ipc::{
    ExecutableKey, ExecutableValue, Settings, SettingsGetReq, SettingsSetExecutableReq,
    SettingsSetExecutableRes, SettingsSetReq,
};
use crate::contract::{IpcError, Req};
use crate::paths::AppPaths;

/// The native confirmation of `settings_set_executable`.
#[async_trait]
pub trait ExecutableConfirm: Send + Sync {
    /// `true` when the user confirmed `value_text` for `key`.
    async fn confirm(&self, key: &str, value_text: &str) -> bool;
}

/// The production confirmation: a native dialog (dialog plugin, Rust API).
pub struct DialogConfirm<R: tauri::Runtime> {
    app: tauri::AppHandle<R>,
    korean: bool,
}

impl<R: tauri::Runtime> DialogConfirm<R> {
    pub fn new(app: tauri::AppHandle<R>, korean: bool) -> DialogConfirm<R> {
        DialogConfirm { app, korean }
    }
}

#[async_trait]
impl<R: tauri::Runtime> ExecutableConfirm for DialogConfirm<R> {
    async fn confirm(&self, key: &str, value_text: &str) -> bool {
        use tauri_plugin_dialog::{DialogExt as _, MessageDialogButtons, MessageDialogKind};
        let (message, ok, cancel) = if self.korean {
            (
                format!("{key}에 다음 값을 적용합니다.\n\n{value_text}"),
                "적용",
                "취소",
            )
        } else {
            (
                format!("Apply this value to {key}?\n\n{value_text}"),
                "Apply",
                "Cancel",
            )
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.app
            .dialog()
            .message(message)
            .title("Gilde")
            .kind(MessageDialogKind::Warning)
            .buttons(MessageDialogButtons::OkCancelCustom(ok.to_string(), cancel.to_string()))
            .show(move |answer| {
                let _ = tx.send(answer);
            });
        rx.await.unwrap_or(false)
    }
}

/// Managed confirmation (tests manage a fake).
pub type ConfirmState = Arc<dyn ExecutableConfirm>;

/// `settings_get` (contract §4.2.8).
#[tauri::command]
pub async fn settings_get<R: tauri::Runtime>(
    req: Req<SettingsGetReq>,
    app: tauri::AppHandle<R>,
) -> Result<Settings, IpcError> {
    let _ = req;
    let config: ConfigState = state_of(&app)?;
    Ok(config.get().to_settings())
}

/// `settings_set` (contract §4.2.8): ordinary keys only.
#[tauri::command]
pub async fn settings_set<R: tauri::Runtime>(
    req: Req<SettingsSetReq>,
    app: tauri::AppHandle<R>,
) -> Result<Settings, IpcError> {
    let config: ConfigState = state_of(&app)?;
    set_ordinary(&config, &req.into_inner().patch)
}

pub fn set_ordinary(config: &ConfigStore, patch: &crate::contract::ipc::SettingsPatch) -> Result<Settings, IpcError> {
    let json = AppConfig::ordinary_patch_json(patch);
    let updated = config.patch(json).map_err(|e| IpcError::from(&e))?;
    Ok(updated.to_settings())
}

/// `settings_set_executable` (contract §4.2.8, §9.6).
#[tauri::command]
pub async fn settings_set_executable<R: tauri::Runtime>(
    req: Req<SettingsSetExecutableReq>,
    app: tauri::AppHandle<R>,
) -> Result<SettingsSetExecutableRes, IpcError> {
    use tauri::Manager as _;
    let req = req.into_inner();
    let config: ConfigState = state_of(&app)?;
    let paths: PathsState = state_of(&app)?;
    let confirm: ConfirmState = match app.try_state::<ConfirmState>() {
        Some(c) => c.inner().clone(),
        None => {
            let korean = crate::commands::app::locale_and_theme(&config).0
                == crate::contract::ipc::Locale::Ko;
            Arc::new(DialogConfirm::new(app.clone(), korean))
        }
    };
    set_executable_then(&config, &paths, confirm.as_ref(), req, |_| rescan_agents(&app)).await
}

/// A29.1: whether an applied value of `key` reruns agent detection — the
/// vendor binary pins and the PATH override (they change what is found).
pub fn reruns_detection(key: ExecutableKey) -> bool {
    matches!(
        key,
        ExecutableKey::ClaudeBinary
            | ExecutableKey::CodexBinary
            | ExecutableKey::CursorBinary
            | ExecutableKey::AgyBinary
            | ExecutableKey::PathsOverride
    )
}

/// [`set_executable`], then `rescan(key)` once the value **was applied** and
/// [`reruns_detection`] holds for its key (never for a cancelled
/// confirmation or a refused value). `rescan` must only schedule: the
/// command's answer never waits for the detection.
pub async fn set_executable_then(
    config: &ConfigStore,
    paths: &AppPaths,
    confirm: &dyn ExecutableConfirm,
    req: SettingsSetExecutableReq,
    rescan: impl FnOnce(ExecutableKey),
) -> Result<SettingsSetExecutableRes, IpcError> {
    let key = req.key;
    let res = set_executable(config, paths, confirm, req).await?;
    if res.applied && reruns_detection(key) {
        rescan(key);
    }
    Ok(res)
}

/// Rerun detection in the background when the agent registry is up (before
/// B6 there is nothing to rerun: B9's first detection reads the new value).
/// A45.3: a rerun already in flight may have read the old value, so this asks
/// it for one more pass instead of joining it.
fn rescan_agents<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    use tauri::Manager as _;
    let (Some(lifecycle), Some(agents)) = (
        app.try_state::<crate::commands::app::LifecycleState>(),
        app.try_state::<crate::commands::app::AgentsState>(),
    ) else {
        return;
    };
    let compat = app
        .try_state::<crate::commands::app::ServicesState>()
        .and_then(|s| s.compat());
    crate::lifecycle::boot::spawn_rescan_as(
        lifecycle.inner().clone(),
        agents.inner().clone(),
        compat,
        crate::lifecycle::boot::RescanMode::AfterChange,
    );
}

/// The wire name of an executable key.
pub fn key_name(key: ExecutableKey) -> &'static str {
    match key {
        ExecutableKey::ClaudeBinary => "agents.claude.binary",
        ExecutableKey::CodexBinary => "agents.codex.binary",
        ExecutableKey::CursorBinary => "agents.cursor.binary",
        ExecutableKey::AgyBinary => "agents.agy.binary",
        ExecutableKey::SidecarNodePath => "sidecar.nodePath",
        ExecutableKey::PathsOverride => "paths.override",
        ExecutableKey::EnvPassthrough => "agents.envPassthrough",
    }
}

/// The canonical form of a checked path as the value to show and apply
/// (A45.1): it must itself be an [`AbsPath`] — a realpath that is not UTF-8,
/// or that reaches a name with a forbidden character through a symlink, is
/// refused like the request would have been (`INVALID_ARGUMENT{value.path}`).
fn canonical_value(real: &Path) -> Result<AbsPath, IpcError> {
    let text = real.to_str().ok_or_else(|| IpcError::invalid_argument("value.path", "path"))?;
    AbsPath::parse(text).map_err(|e| IpcError::invalid_argument("value.path", e.rule))
}

/// The value as the native confirmation shows it (A45.1, §9.6): every
/// character that does not print as itself — controls, line and paragraph
/// separators, whitespace other than the space, format and other
/// default-ignorable characters, private-use characters and noncharacters —
/// is written `\u{…}`, and a backslash is doubled, so two different values
/// never read the same.
pub fn confirmation_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if c == '\\' {
            out.push_str("\\\\");
        } else if is_unprintable(c) {
            out.push_str(&format!("\\u{{{:X}}}", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

/// Characters that do not print as themselves (see [`confirmation_text`]).
fn is_unprintable(c: char) -> bool {
    let cp = c as u32;
    c.is_control()
        || (c.is_whitespace() && c != ' ')
        || matches!(
            cp,
            // format characters and the Default_Ignorable_Code_Point set
            0x00AD
                | 0x034F
                | 0x0600..=0x0605
                | 0x061C
                | 0x06DD
                | 0x070F
                | 0x08E2
                | 0x115F..=0x1160
                | 0x17B4..=0x17B5
                | 0x180B..=0x180F
                | 0x200B..=0x200F
                | 0x202A..=0x202E
                | 0x2060..=0x206F
                | 0x3164
                | 0xFE00..=0xFE0F
                | 0xFEFF
                | 0xFFA0
                | 0xFFF0..=0xFFFB
                | 0x110BD
                | 0x110CD
                | 0x13430..=0x1343F
                | 0x1BCA0..=0x1BCA3
                | 0x1D173..=0x1D17A
                | 0xE0000..=0xE0FFF
                // private use
                | 0xE000..=0xF8FF
                | 0xF0000..=0x10FFFF
                // noncharacters
                | 0xFDD0..=0xFDEF
        )
        || (cp & 0xFFFE) == 0xFFFE
}

/// The checks, the confirmation, then the write (shared with tests).
///
/// A45.1: a path is resolved by the checks; the canonical path is what the
/// confirmation names and what is written and allowlisted. The request's
/// own string is never shown or stored.
pub async fn set_executable(
    config: &ConfigStore,
    paths: &AppPaths,
    confirm: &dyn ExecutableConfirm,
    req: SettingsSetExecutableReq,
) -> Result<SettingsSetExecutableRes, IpcError> {
    let (text, value) = match &req.value {
        ExecutableValue::Path { path: Some(p) } => {
            let real = match req.key {
                ExecutableKey::PathsOverride => check_directory(Path::new(p.as_str()), paths)?,
                _ => check_executable(Path::new(p.as_str()), paths)?,
            };
            let canonical = canonical_value(&real)?;
            (confirmation_text(canonical.as_str()), ExecutableValue::Path { path: Some(canonical) })
        }
        ExecutableValue::Path { path: None } => ("—".to_string(), req.value.clone()),
        ExecutableValue::EnvNames { names } => (confirmation_text(&names.join(", ")), req.value.clone()),
    };
    if !confirm.confirm(key_name(req.key), &text).await {
        return Ok(SettingsSetExecutableRes { applied: false });
    }
    let before = config.get();
    let json = AppConfig::executable_patch_json(req.key, &value);
    config.patch(json).map_err(|e| IpcError::from(&e))?;
    // Keep the launch allowlist (§9.3) in step with the binaries.
    let vendor_key = match req.key {
        ExecutableKey::ClaudeBinary => Some("claude"),
        ExecutableKey::CodexBinary => Some("codex"),
        ExecutableKey::CursorBinary => Some("cursorAgent"),
        ExecutableKey::AgyBinary => Some("agy"),
        _ => None,
    };
    if let Some(k) = vendor_key {
        if let Some(old) = before.agent_binary(k) {
            crate::supervise::spawn::disallow_executable(Path::new(old));
        }
        if let ExecutableValue::Path { path: Some(p) } = &value {
            crate::supervise::spawn::allow_executable(Path::new(p.as_str()), &crate::agents::VENDOR_KINDS);
        }
    }
    if req.key == ExecutableKey::SidecarNodePath {
        if let Some(old) = before.sidecar.node_path.as_deref() {
            crate::supervise::spawn::disallow_executable(Path::new(old));
        }
        if let ExecutableValue::Path { path: Some(p) } = &value {
            crate::supervise::spawn::allow_executable(
                Path::new(p.as_str()),
                &[crate::supervise::spawn::AllowKind::Sidecar],
            );
        }
    }
    tracing::info!(target: "app", key = key_name(req.key), "executable setting applied");
    Ok(SettingsSetExecutableRes { applied: true })
}

/// Vendor directories a binary may never live in (§9.7, §9.10).
fn vendor_roots(paths: &AppPaths) -> Vec<std::path::PathBuf> {
    let home = paths.home_dir();
    let mut roots = vec![
        home.join(".codex"),
        home.join(".cursor"),
        home.join(".gemini"),
        home.join(".claude-code-orchestrator"),
        home.join(".claude"),
    ];
    if let Ok(entries) = std::fs::read_dir(home) {
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().starts_with(".claude-") {
                roots.push(e.path());
            }
        }
    }
    roots
}

fn inside_vendor_root(path: &Path, paths: &AppPaths) -> bool {
    vendor_roots(paths).iter().any(|root| {
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
        path.starts_with(&root)
    })
}

fn owner_ok(meta: &std::fs::Metadata) -> bool {
    let me = nix::unistd::getuid().as_raw();
    meta.uid() == me || meta.uid() == 0
}

fn world_writable(meta: &std::fs::Metadata) -> bool {
    meta.permissions().mode() & 0o002 != 0
}

fn sticky(meta: &std::fs::Metadata) -> bool {
    meta.permissions().mode() & 0o1000 != 0
}

/// A50.2: whoever can write a directory on the way to `real` (a realpath)
/// can swap what lies below it. `true` (refuse) when an **ancestor** of
/// `real` is world-writable without the sticky bit, or when an entry below
/// a sticky world-writable ancestor (like `/tmp`) — down to `real` itself —
/// is not owned by the user or root (the sticky bit protects only the
/// owner's entries). An ancestor that cannot be read refuses too.
fn writable_ancestry(real: &Path) -> bool {
    writable_ancestry_by(real, owner_ok)
}

/// [`writable_ancestry`] with the owner rule as a parameter (tests stand in
/// for an entry another user owns, which they cannot create).
fn writable_ancestry_by(real: &Path, owned: impl Fn(&std::fs::Metadata) -> bool) -> bool {
    let mut dirs: Vec<&Path> = real.ancestors().skip(1).collect();
    dirs.reverse(); // from `/` down to `real`'s directory
    let mut below_sticky = false;
    for dir in dirs.into_iter().chain(std::iter::once(real)) {
        let Ok(meta) = std::fs::metadata(dir) else { return true };
        if below_sticky && !owned(&meta) {
            return true;
        }
        if dir != real && world_writable(&meta) {
            if !sticky(&meta) {
                return true;
            }
            below_sticky = true;
        }
    }
    false
}

/// A binary setting: a regular, executable Mach-O or `#!` script owned by the
/// user or root and not itself world-writable (A50.4; group-writable stays
/// accepted), not in a world-writable directory, under no ancestor others
/// can write (A50.2, [`writable_ancestry`]), not inside a vendor dir.
/// Answers the canonical path (realpath) that was checked.
pub fn check_executable(path: &Path, paths: &AppPaths) -> Result<PathBuf, IpcError> {
    let refuse = |why: &str| Err(IpcError::refused(why));
    let Ok(real) = std::fs::canonicalize(path) else {
        return Err(IpcError::not_found("file"));
    };
    let Ok(meta) = std::fs::metadata(&real) else {
        return Err(IpcError::not_found("file"));
    };
    if !meta.is_file() {
        return refuse("notAFile");
    }
    if meta.permissions().mode() & 0o111 == 0 {
        return refuse("notExecutable");
    }
    if !owner_ok(&meta) {
        return refuse("foreignOwner");
    }
    // A50.4: anyone could rewrite it after the dialog.
    if world_writable(&meta) {
        return refuse("worldWritableFile");
    }
    if let Some(dir) = real.parent() {
        if std::fs::metadata(dir).map(|m| world_writable(&m)).unwrap_or(true) {
            return refuse("worldWritableDir");
        }
    }
    if writable_ancestry(&real) {
        return refuse("worldWritableDir");
    }
    if inside_vendor_root(&real, paths) {
        return refuse("insideVendorDir");
    }
    let mut head = [0u8; 4];
    let n = std::fs::File::open(&real)
        .and_then(|mut f| std::io::Read::read(&mut f, &mut head))
        .unwrap_or(0);
    let magic = u32::from_be_bytes(head);
    let mach_o = n == 4
        && matches!(
            magic,
            0xFEED_FACE | 0xFEED_FACF | 0xCEFA_EDFE | 0xCFFA_EDFE | 0xCAFE_BABE | 0xBEBA_FECA
        );
    let script = n >= 2 && &head[..2] == b"#!";
    if !mach_o && !script {
        return refuse("notAnExecutable");
    }
    Ok(real)
}

/// `paths.override`: a directory owned by the user or root, not world-writable,
/// under no ancestor others can write (A50.2, [`writable_ancestry`]).
/// Answers the canonical path (realpath) that was checked.
pub fn check_directory(path: &Path, paths: &AppPaths) -> Result<PathBuf, IpcError> {
    let Ok(real) = std::fs::canonicalize(path) else {
        return Err(IpcError::not_found("file"));
    };
    let Ok(meta) = std::fs::metadata(&real) else {
        return Err(IpcError::not_found("file"));
    };
    if !meta.is_dir() {
        return Err(IpcError::refused("notADirectory"));
    }
    if !owner_ok(&meta) {
        return Err(IpcError::refused("foreignOwner"));
    }
    if world_writable(&meta) || writable_ancestry(&real) {
        return Err(IpcError::refused("worldWritableDir"));
    }
    if inside_vendor_root(&real, paths) {
        return Err(IpcError::refused("insideVendorDir"));
    }
    Ok(real)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A50.2: an ancestor others can write refuses before any dialog — a
    /// 0777 one always, a sticky 1777 one (like `/tmp`) only when an entry
    /// below it is owned by someone other than the user or root; the
    /// binary's own directory may not be world-writable at all.
    #[test]
    fn a50_2_no_world_writable_ancestor() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let paths = AppPaths::for_home(&base.join("home"));
        let script = |p: &Path| {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        let mode = |p: &Path, m: u32| std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
        let reason = |r: Result<PathBuf, IpcError>| r.err().and_then(|e| e.params["reason"].as_str().map(str::to_string));
        let ww = Some("worldWritableDir".to_string());

        // 0777 two levels up, owner-only dirs below it.
        let open = base.join("open");
        script(&open.join("sub/bin/claude"));
        mode(&open.join("sub"), 0o755);
        mode(&open, 0o777);
        assert_eq!(reason(check_executable(&open.join("sub/bin/claude"), &paths)), ww, "0777 ancestor");
        assert_eq!(reason(check_directory(&open.join("sub/bin"), &paths)), ww, "0777 ancestor of pathsOverride");
        // Through a clean symlink: the realpath is what is checked.
        std::os::unix::fs::symlink(open.join("sub/bin/claude"), base.join("clean-link")).unwrap();
        assert_eq!(reason(check_executable(&base.join("clean-link"), &paths)), ww, "a symlink to it");

        // 1777 (sticky) two levels up: the user's entries below are fine …
        let sticky = base.join("sticky");
        script(&sticky.join("mine/claude"));
        mode(&sticky, 0o1777);
        let mine = sticky.join("mine/claude");
        assert!(check_executable(&mine, &paths).is_ok(), "sticky ancestor, the user's entries");
        assert!(check_directory(&sticky.join("mine"), &paths).is_ok());
        // … an entry another user owns is not (stood in by the owner rule) …
        let foreign = std::fs::metadata(sticky.join("mine")).unwrap().ino();
        assert!(writable_ancestry_by(&mine, |m| m.ino() != foreign), "another user's entry below a sticky dir");
        assert!(!writable_ancestry_by(&mine, |_| true));
        // … and the binary's own directory may not be world-writable at all.
        script(&sticky.join("claude"));
        assert_eq!(reason(check_executable(&sticky.join("claude"), &paths)), ww, "directly in a sticky dir");

        // Control.
        script(&base.join("clean/claude"));
        assert!(check_executable(&base.join("clean/claude"), &paths).is_ok());
    }

    /// A50.4: a binary file anyone can write is refused before any dialog;
    /// group-writable (admin-group installs) and owner-only are accepted.
    #[test]
    fn a50_4_a_world_writable_binary_file_is_refused() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let paths = AppPaths::for_home(&base.join("home"));
        let bin = base.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let script = |name: &str, mode: u32| {
            let p = bin.join(name);
            std::fs::write(&p, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
            p
        };
        let r = check_executable(&script("anyone-writes", 0o777), &paths);
        assert_eq!(r.err().and_then(|e| e.params["reason"].as_str().map(str::to_string)).as_deref(), Some("worldWritableFile"));
        assert!(check_executable(&script("group-writes", 0o775), &paths).is_ok(), "group-writable stays accepted");
        assert!(check_executable(&script("owner-only", 0o755), &paths).is_ok(), "control");
    }

    #[test]
    fn the_confirmation_escapes_everything_that_does_not_print() {
        assert_eq!(confirmation_text("/Users/a/bin/claude"), "/Users/a/bin/claude");
        assert_eq!(confirmation_text("/Users/a/프로젝트/claude"), "/Users/a/프로젝트/claude");
        assert_eq!(confirmation_text("/a b"), "/a b");
        for (c, shown) in [
            ('\n', "\\u{A}"),
            ('\u{1B}', "\\u{1B}"),
            ('\u{85}', "\\u{85}"),
            ('\u{A0}', "\\u{A0}"),
            ('\u{AD}', "\\u{AD}"),
            ('\u{200B}', "\\u{200B}"),
            ('\u{202E}', "\\u{202E}"),
            ('\u{2028}', "\\u{2028}"),
            ('\u{2029}', "\\u{2029}"),
            ('\u{2060}', "\\u{2060}"),
            ('\u{2067}', "\\u{2067}"),
            ('\u{3164}', "\\u{3164}"),
            ('\u{FE0F}', "\\u{FE0F}"),
            ('\u{FEFF}', "\\u{FEFF}"),
            ('\u{E000}', "\\u{E000}"),
            ('\u{FFFF}', "\\u{FFFF}"),
            ('\u{E0041}', "\\u{E0041}"),
        ] {
            assert_eq!(confirmation_text(&format!("/x{c}y")), format!("/x{shown}y"), "{c:?}");
        }
        // A literal backslash-u in a name never reads like an escaped character.
        assert_eq!(confirmation_text("/x\\u{202E}y"), "/x\\\\u{202E}y");
        assert_ne!(confirmation_text("/x\\u{202E}y"), confirmation_text("/x\u{202E}y"));
    }
}
