//! Owner-app activation, "Go to {owner}" (contract §4.2.5 `owner_focus`, §9.5) [C6].
//!
//! Owner: T12. The decision is policy code tested with fakes
//! ([`OwnerFocusService`] over `WorldApi`, an optional `IdeApi` and an
//! [`AppActivator`]); the real activation ([`MacActivator`]:
//! `NSRunningApplication.activateWithOptions`, no subprocess) runs only in the
//! app, never in a test.
//!
//! Rules pinned here:
//! - Only a **Watching** session with a known owner app can be focused; In
//!   Gilde and Not running sessions have no owner → `UNSUPPORTED{noOwner}`. A
//!   merged session (tombstone) is followed one hop to the session it was
//!   merged into.
//! - **pid-reuse fence:** the pid recorded for the owner is activated only if
//!   it still belongs to the app with the recorded bundle id; a pid that
//!   exited or now belongs to another app → `UNSUPPORTED{noOwner}`, never the
//!   wrong app brought forward. pid ≤ 1, Gilde's own pid and an empty bundle
//!   id are refused the same way.
//! - **Window precision** (PROVISIONAL LP-24, app flag `vscodeWindowFocus`):
//!   only with the flag on, for a VS Code or Cursor owner, with T11's `IdeApi`
//!   bound and the project root known, T11's bundle-CLI candidate runs first;
//!   `Ok(true)` → `precision: "window"`. Anything else falls back to bringing
//!   the owner app forward (`precision: "app"`, the ux-ia Q9 fallback). The
//!   IDE lock files that would map a session to a window are never opened.
//! - No AppleScript, no `it2`, no subprocess, never another process's
//!   environment values (§9.5).

use std::sync::{Arc, OnceLock};

use objc2::rc::Retained;
use objc2::{msg_send, ClassType};
use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication};

use crate::contract::common::AbsPath;
use crate::contract::ids::SessionUid;
use crate::contract::ipc::{EditorKind, FocusPrecision, OwnerFocusRes};
use crate::contract::services::{IdeApi, WorldApi};
use crate::contract::session::{Control, OwnerApp, Session};
use crate::contract::IpcError;

/// VS Code's bundle id (§9.4).
pub const VSCODE_BUNDLE_ID: &str = "com.microsoft.VSCode";
/// Cursor's bundle id (§9.4).
pub const CURSOR_BUNDLE_ID: &str = "com.todesktop.230313mzl4w4u92";

/// What an activation attempt found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activation {
    /// The app was asked to come forward.
    Activated,
    /// No running app has that pid (it exited).
    NotRunning,
    /// The pid now belongs to another app (pid reuse) — not activated.
    NotThatApp,
    /// macOS declined the activation.
    Refused,
}

/// Brings an owner app forward (the real one in the app, a fake in tests).
pub trait AppActivator: Send + Sync {
    /// Activates the app with `pid` only if it is still the app with
    /// `bundle_id`. Never spawns a process. May be called off the main thread.
    fn activate(&self, pid: i32, bundle_id: &str) -> Activation;
}

/// `NSRunningApplication` by pid (thread-safe AppKit class; no subprocess).
#[derive(Debug, Default, Clone, Copy)]
pub struct MacActivator;

impl AppActivator for MacActivator {
    fn activate(&self, pid: i32, bundle_id: &str) -> Activation {
        // `+runningApplicationWithProcessIdentifier:` (the generated binding
        // sits behind objc2-app-kit's `libc` feature, which this crate does
        // not enable; the manifests are frozen)
        // SAFETY: class method `(pid_t) -> NSRunningApplication * _Nullable`.
        let app: Option<Retained<NSRunningApplication>> =
            unsafe { msg_send![NSRunningApplication::class(), runningApplicationWithProcessIdentifier: pid] };
        let Some(app) = app else {
            return Activation::NotRunning;
        };
        if app.isTerminated() {
            return Activation::NotRunning;
        }
        match app.bundleIdentifier() {
            Some(found) if found.to_string() == bundle_id => {}
            _ => return Activation::NotThatApp,
        }
        if app.activateWithOptions(NSApplicationActivationOptions::ActivateAllWindows) {
            Activation::Activated
        } else {
            Activation::Refused
        }
    }
}

/// The editor an owner app is, by bundle id (VS Code or Cursor only).
pub fn editor_of(owner: &OwnerApp) -> Option<EditorKind> {
    match owner.bundle_id.as_str() {
        VSCODE_BUNDLE_ID => Some(EditorKind::VsCode),
        CURSOR_BUNDLE_ID => Some(EditorKind::Cursor),
        _ => None,
    }
}

/// The owner app of a session, if it is Watching with a known owner.
pub fn owner_of(session: &Session) -> Option<&OwnerApp> {
    match &session.control {
        Control::Watching { owner: Some(owner), .. } => Some(owner),
        _ => None,
    }
}

/// `owner_focus` (§4.2.5): the decision and the activation.
pub struct OwnerFocusService {
    world: Arc<dyn WorldApi>,
    activator: Arc<dyn AppActivator>,
    ide: OnceLock<Arc<dyn IdeApi>>,
    /// App flag `vscodeWindowFocus` (LP-24); absent = fallback.
    window_focus: bool,
    own_pid: i32,
}

impl std::fmt::Debug for OwnerFocusService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnerFocusService")
            .field("window_focus", &self.window_focus)
            .field("ide", &self.ide.get().is_some())
            .finish()
    }
}

impl OwnerFocusService {
    /// `window_focus` = `CompatApi::app_feature("vscodeWindowFocus")`.
    pub fn new(world: Arc<dyn WorldApi>, activator: Arc<dyn AppActivator>, window_focus: bool) -> Arc<OwnerFocusService> {
        Arc::new(OwnerFocusService {
            world,
            activator,
            ide: OnceLock::new(),
            window_focus,
            own_pid: std::process::id() as i32,
        })
    }

    /// Binds T11's `IdeApi` (once; `false` if already bound). Until bound,
    /// window precision is never attempted.
    pub fn set_ide(&self, ide: Arc<dyn IdeApi>) -> bool {
        self.ide.set(ide).is_ok()
    }

    fn project_root(&self, session: &Session) -> Option<AbsPath> {
        let project = session.project_id.as_ref()?;
        self.world.snapshot().projects.into_iter().find(|p| &p.id == project).map(|p| p.root)
    }

    /// Brings the owner of `uid` forward.
    pub async fn focus(&self, uid: &SessionUid) -> Result<OwnerFocusRes, IpcError> {
        let mut session = self.world.session(uid).ok_or_else(|| IpcError::not_found("session"))?;
        if let Some(target) = session.merged_into.clone() {
            session = self.world.session(&target).ok_or_else(|| IpcError::not_found("session"))?;
        }
        let owner = owner_of(&session).cloned().ok_or_else(|| IpcError::unsupported("noOwner"))?;
        if owner.app_pid <= 1 || owner.app_pid == self.own_pid || owner.bundle_id.trim().is_empty() {
            return Err(IpcError::unsupported("noOwner"));
        }
        if self.window_focus {
            if let (Some(editor), Some(ide)) = (editor_of(&owner), self.ide.get()) {
                if let Some(root) = self.project_root(&session) {
                    if let Ok(true) = ide.focus_editor_window(editor, &root).await {
                        return Ok(OwnerFocusRes { precision: FocusPrecision::Window });
                    }
                }
            }
        }
        let activator = self.activator.clone();
        let (pid, bundle) = (owner.app_pid, owner.bundle_id.clone());
        let outcome = tauri::async_runtime::spawn_blocking(move || activator.activate(pid, &bundle))
            .await
            .unwrap_or(Activation::Refused);
        match outcome {
            Activation::Activated => Ok(OwnerFocusRes { precision: FocusPrecision::App }),
            Activation::NotRunning | Activation::NotThatApp => Err(IpcError::unsupported("noOwner")),
            Activation::Refused => {
                let correlation = uuid::Uuid::new_v4().to_string();
                tracing::warn!(target: "macos", code = "INTERNAL", "owner activation refused");
                Err(IpcError::internal(&correlation))
            }
        }
    }
}

/// The managed state of the `owner_focus` command (the composition root,
/// T09b, calls `app.manage(OwnerState(service))`). Until it is managed the
/// command answers `UNSUPPORTED{notBuilt}`.
pub struct OwnerState(pub Arc<OwnerFocusService>);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::session::OwnerKind;

    fn owner(bundle: &str) -> OwnerApp {
        OwnerApp { app_pid: 4242, app_name: "x".into(), kind: OwnerKind::Other, bundle_id: bundle.into() }
    }

    #[test]
    fn only_vscode_and_cursor_are_editors() {
        assert_eq!(editor_of(&owner(VSCODE_BUNDLE_ID)), Some(EditorKind::VsCode));
        assert_eq!(editor_of(&owner(CURSOR_BUNDLE_ID)), Some(EditorKind::Cursor));
        assert_eq!(editor_of(&owner("com.apple.Terminal")), None);
        assert_eq!(editor_of(&owner("com.microsoft.vscode")), None, "bundle ids are exact");
    }
}
