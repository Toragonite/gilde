//! IDE and links (contract §4.2.5, §9.4) [C9]: Open/Diff in IDE for VS Code
//! and Cursor by bundle id, Reveal in Finder, `link_open`, the LP-24 focus
//! candidate — the `IdeApi` (T11 → T10, T12).
//!
//! Owner: T11. The `mod` lines below are pre-declared by T01 and frozen
//! after wave 0 (contract §15.1, F-18): a new module is a contract change.
//!
//! Every effect on the desktop goes through a seam of [`Platform`]
//! (launcher, Finder, URL opener, native confirmation, bundle reader, app
//! dirs). There is **no default**: the composition root passes
//! [`Platform::macos`], tests pass fakes, so no test can open an app,
//! a Finder window or a URL by forgetting an override.

pub mod editors;
pub mod links;
pub mod opener;
pub mod paths;
pub mod reveal;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

use crate::contract::error::IpcError;
use crate::contract::ids::SessionUid;
use crate::contract::ipc::changes::{EditorKind, IdeEditorsRes, IdeOpenReq, IdeRevealReq, IdeTarget, RevealTarget};
use crate::contract::common::AbsPath;
use crate::contract::services::{IdeApi, SupervisorApi};
use crate::worktree::{place_of, PlaceSource};

use self::editors::{BundleReader, FoundEditor};
use self::links::{LinkConfirm, UrlOpener};
use self::opener::{LaunchPlan, Launcher, OpenAction};
use self::paths::PickedRoots;
use self::reveal::Revealer;

/// The desktop effects of the IDE service.
pub struct Platform {
    pub launcher: Arc<dyn Launcher>,
    pub revealer: Arc<dyn Revealer>,
    pub url_opener: Arc<dyn UrlOpener>,
    pub link_confirm: Arc<dyn LinkConfirm>,
    pub bundles: Arc<dyn BundleReader>,
    /// Where editors are looked up (`/Applications`, `~/Applications`).
    pub app_dirs: Vec<PathBuf>,
}

impl Platform {
    /// The production platform: launches through `SupervisorApi`, Finder and
    /// `openURL` by NSWorkspace, the native link confirmation given.
    pub fn macos(supervisor: Arc<dyn SupervisorApi>, home: &Path, link_confirm: Arc<dyn LinkConfirm>) -> Platform {
        Platform {
            launcher: Arc::new(opener::SupervisedLauncher::new(supervisor)),
            revealer: Arc::new(reveal::FinderRevealer),
            url_opener: Arc::new(links::WorkspaceOpener),
            link_confirm,
            bundles: Arc::new(editors::InfoPlistReader),
            app_dirs: editors::default_dirs(home),
        }
    }
}

/// The base side of a diff (T11's `GitService`).
#[async_trait]
pub trait DiffSource: Send + Sync {
    /// The bytes of `rel` (relative to the session's location) at its diff
    /// base; `Ok(None)` when the file did not exist there.
    async fn base_bytes(&self, uid: &SessionUid, rel: &str, cap: usize) -> Result<Option<Vec<u8>>, IpcError>;
}

#[async_trait]
impl DiffSource for crate::worktree::GitService {
    async fn base_bytes(&self, uid: &SessionUid, rel: &str, cap: usize) -> Result<Option<Vec<u8>>, IpcError> {
        self.diff_base(uid, rel, cap).await
    }
}

/// The chosen editor (the ordinary setting `ide.editor`, §9.6).
pub type ChosenEditor = Arc<dyn Fn() -> Option<EditorKind> + Send + Sync>;

/// The `IdeApi`.
pub struct IdeService {
    src: Arc<dyn PlaceSource>,
    home: PathBuf,
    diff_dir: PathBuf,
    picked: Arc<PickedRoots>,
    chosen: ChosenEditor,
    platform: Platform,
    diff: Option<Arc<dyn DiffSource>>,
    extra_vendor_roots: Vec<PathBuf>,
}

impl IdeService {
    /// `diff_dir` = `AppPaths::tmp_diff_dir()`.
    pub fn new(
        src: Arc<dyn PlaceSource>,
        home: PathBuf,
        diff_dir: PathBuf,
        picked: Arc<PickedRoots>,
        chosen: ChosenEditor,
        platform: Platform,
    ) -> IdeService {
        IdeService {
            src,
            home,
            diff_dir,
            picked,
            chosen,
            platform,
            diff: None,
            extra_vendor_roots: Vec::new(),
        }
    }

    /// Where `Diff` targets get their base side (without one, a diff is
    /// `UNSUPPORTED{diffNotSupportedByEditor}`).
    pub fn with_diff_source(mut self, diff: Arc<dyn DiffSource>) -> IdeService {
        self.diff = Some(diff);
        self
    }

    /// Config dirs outside `~/.claude*` (user-added accounts) to refuse too.
    pub fn with_vendor_roots(mut self, roots: Vec<PathBuf>) -> IdeService {
        self.extra_vendor_roots = roots;
        self
    }

    pub fn picked(&self) -> Arc<PickedRoots> {
        self.picked.clone()
    }

    /// The installed editors (directory listing only).
    pub fn found(&self) -> Vec<FoundEditor> {
        editors::scan(&self.platform.app_dirs, self.platform.bundles.as_ref())
    }

    /// The chosen editor when it is installed, else `NOT_READY{editorNotChosen}`
    /// (the UI then shows the editors popover).
    fn editor(&self) -> Result<FoundEditor, IpcError> {
        let chosen = (self.chosen)().ok_or_else(|| IpcError::not_ready("editorNotChosen"))?;
        self.found()
            .into_iter()
            .find(|f| f.editor == chosen)
            .ok_or_else(|| IpcError::not_ready("editorNotChosen"))
    }

    /// The root of `project` (World's, or a folder picked in this instance).
    fn project_root(&self, project: &crate::contract::ids::ProjectId) -> Result<PathBuf, IpcError> {
        let view = self.src.view();
        if let Some(p) = view.projects.iter().find(|p| &p.id == project) {
            return std::fs::canonicalize(p.root.as_str()).map_err(|_| IpcError::not_found("project"));
        }
        self.picked
            .list()
            .into_iter()
            .find(|root| crate::discovery::location::identify(root, &self.home).is_some_and(|i| &i.project_id == project))
            .ok_or_else(|| IpcError::not_found("project"))
    }

    /// Resolve a `File`/`Diff` path of `uid` with the §9.4 rules; returns the
    /// canonical path and the session's location. A33.1: the lexical roots
    /// check comes first — a path that leaves every root is refused before
    /// the target is ever `stat`-ed (no existence oracle); then it is
    /// resolved (`NOT_FOUND{file}` for an inside path that does not exist)
    /// and checked again, so a symlink that leads out is refused too.
    fn resolve_in_session(&self, uid: &SessionUid, raw: &str, allow_missing: bool) -> Result<(PathBuf, PathBuf), IpcError> {
        let (place, _) = place_of(self.src.as_ref(), &self.home, uid)?;
        let mut allowed = vec![place.location.clone(), place.project_root.clone()];
        allowed.extend(place.other_roots.iter().cloned());
        allowed.extend(self.picked.list());
        paths::check_lexical(raw, &place.location, &allowed, &self.home, &self.extra_vendor_roots)?;
        let resolved = paths::resolve(raw, &place.location, allow_missing)?;
        paths::check(&resolved, &allowed, &self.home, &self.extra_vendor_roots)?;
        Ok((resolved, place.location))
    }

    async fn launch(&self, editor: &FoundEditor, action: &OpenAction) -> Result<(), IpcError> {
        let plan: LaunchPlan = opener::plan(editor, action)?;
        self.platform.launcher.launch(&plan).await
    }
}

#[async_trait]
impl IdeApi for IdeService {
    async fn open(&self, req: &IdeOpenReq) -> Result<(), IpcError> {
        let editor = self.editor()?;
        let action = match &req.target {
            IdeTarget::File { uid, path, line } => {
                let (resolved, _) = self.resolve_in_session(uid, path, false)?;
                OpenAction::File { path: resolved, line: *line }
            }
            IdeTarget::Diff { uid, path } => {
                if editor.cli.is_none() {
                    return Err(IpcError::unsupported("diffNotSupportedByEditor"));
                }
                let (resolved, location) = self.resolve_in_session(uid, path, true)?;
                // A diff is against the session's own git location.
                let rel = paths::relative_to(&resolved, &location).ok_or_else(|| IpcError::refused(paths::REASON_OUTSIDE_ROOTS))?;
                let diff = self.diff.as_ref().ok_or_else(|| IpcError::unsupported("diffNotSupportedByEditor"))?;
                let base = diff.base_bytes(uid, &rel, opener::DIFF_CAP).await?;
                let exists = resolved.exists();
                if base.is_none() && !exists {
                    return Err(IpcError::not_found("file"));
                }
                let name = resolved.file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string();
                opener::sweep_diff_dir(&self.diff_dir, opener::DIFF_MAX_AGE);
                let io = |e: std::io::Error| IpcError::from(&crate::error::GildeError::Io(e));
                let left = opener::write_diff_side(&self.diff_dir, &name, base.as_deref().unwrap_or(&[])).map_err(io)?;
                let right = if exists {
                    resolved
                } else {
                    opener::write_diff_side(&self.diff_dir, &name, &[]).map_err(io)?
                };
                OpenAction::Diff { left, right }
            }
            IdeTarget::Folder { uid } => {
                let (place, _) = place_of(self.src.as_ref(), &self.home, uid)?;
                paths::check(&place.location, std::slice::from_ref(&place.location), &self.home, &self.extra_vendor_roots)?;
                OpenAction::Folder { path: place.location }
            }
            IdeTarget::ProjectRoot { project_id } => {
                let root = self.project_root(project_id)?;
                paths::check(&root, std::slice::from_ref(&root), &self.home, &self.extra_vendor_roots)?;
                OpenAction::Folder { path: root }
            }
        };
        self.launch(&editor, &action).await
    }

    async fn reveal(&self, req: &IdeRevealReq) -> Result<(), IpcError> {
        let path = match &req.target {
            RevealTarget::Transcript { uid } => {
                if self.src.session(uid).is_none() {
                    return Err(IpcError::not_found("session"));
                }
                let (p, _) = self.src.transcript(uid).ok_or_else(|| IpcError::not_found("transcript"))?;
                PathBuf::from(p.as_str())
            }
            RevealTarget::Location { uid } => place_of(self.src.as_ref(), &self.home, uid)?.0.location,
            RevealTarget::ProjectRoot { project_id } => self.project_root(project_id)?,
        };
        if !path.exists() {
            return Err(IpcError::not_found("file"));
        }
        let revealer = self.platform.revealer.clone();
        tokio::task::spawn_blocking(move || revealer.reveal(&path))
            .await
            .map_err(|_| IpcError::internal("reveal-task"))?
    }

    fn editors(&self) -> IdeEditorsRes {
        IdeEditorsRes {
            found: self.found().iter().map(|f| editors::option(f, &self.home)).collect(),
            chosen: (self.chosen)(),
        }
    }

    async fn link_open(&self, url: &str) -> Result<bool, IpcError> {
        links::open_link(url, self.platform.link_confirm.as_ref(), self.platform.url_opener.clone()).await
    }

    async fn focus_editor_window(&self, editor: EditorKind, project_root: &AbsPath) -> Result<bool, IpcError> {
        let found = self
            .found()
            .into_iter()
            .find(|f| f.editor == editor)
            .ok_or_else(|| IpcError::not_ready("editorNotChosen"))?;
        let root = std::fs::canonicalize(project_root.as_str()).map_err(|_| IpcError::not_found("project"))?;
        paths::check(&root, std::slice::from_ref(&root), &self.home, &self.extra_vendor_roots)?;
        self.launch(&found, &OpenAction::Folder { path: root }).await?;
        Ok(true)
    }
}
