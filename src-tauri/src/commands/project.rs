//! `project_pick` (contract §4.2.2) [A §4.4].
//!
//! Owner: T11.
//!
//! Rust opens the native folder picker (the dialog plugin's **Rust** API; the
//! WebView has no dialog permission, §9.2) and never receives a path from the
//! WebView. The picked folder becomes a known root for the §9.4 path checks
//! for the rest of this app instance, World is told about it (§5.6 identity,
//! no git spawn), and the answer is the project as World shows it — or, when
//! World has not folded it in yet, the same project computed here. A plain
//! folder is a Folder project; any git repository (or one of its worktrees)
//! is the repository's project.
//!
//! `null` = the user cancelled. `REFUSED{notADirectory}` when the choice is
//! not a directory; `PERMISSION_DENIED{category}` when macOS denies reading it
//! (a category, never a path).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

use crate::commands::app::state_of;
use crate::contract::common::AbsPath;
use crate::contract::ipc::{ProjectPickReq, ProjectPickRes};
use crate::contract::project::{Project, ProjectKind};
use crate::contract::{IpcError, Req};
use crate::ide::paths::PickedRoots;
use crate::worktree::PlaceSource;

/// The native folder picker.
#[async_trait]
pub trait FolderPicker: Send + Sync {
    /// The chosen folder, or `None` when the user cancelled.
    async fn pick(&self) -> Option<PathBuf>;
}

/// The production picker: `tauri-plugin-dialog`'s Rust API.
pub struct DialogFolderPicker<R: tauri::Runtime> {
    app: tauri::AppHandle<R>,
}

impl<R: tauri::Runtime> DialogFolderPicker<R> {
    pub fn new(app: tauri::AppHandle<R>) -> DialogFolderPicker<R> {
        DialogFolderPicker { app }
    }
}

#[async_trait]
impl<R: tauri::Runtime> FolderPicker for DialogFolderPicker<R> {
    async fn pick(&self) -> Option<PathBuf> {
        use tauri_plugin_dialog::DialogExt as _;
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.app.dialog().file().pick_folder(move |folder| {
            let _ = tx.send(folder.and_then(|f| f.into_path().ok()));
        });
        rx.await.ok().flatten()
    }
}

/// What `project_pick` needs (managed by the composition root, T09b).
#[derive(Clone)]
pub struct ProjectState {
    pub picker: Arc<dyn FolderPicker>,
    pub picked: Arc<PickedRoots>,
    /// World (absent: the answer is computed here and nobody is told).
    pub places: Option<Arc<dyn PlaceSource>>,
    /// The §5.6 walk boundary (the user's home).
    pub home: PathBuf,
}

/// `project_pick` (contract §4.2.2).
#[tauri::command]
pub async fn project_pick<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    req: Req<ProjectPickReq>,
) -> Result<ProjectPickRes, IpcError> {
    let _ = req;
    let state: ProjectState = state_of(&app)?;
    // A24.4: the World the composition root manages, when there is one.
    let world = {
        use tauri::Manager as _;
        app.try_state::<Arc<crate::world::World>>().map(|w| w.inner().clone())
    };
    pick_project_with(&state, world.as_deref()).await
}

/// The TCC category of a denied folder (never the path, §4.4).
pub fn denied_category(path: &Path, home: &Path) -> &'static str {
    let home = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    for (dir, category) in [("Documents", "documents"), ("Desktop", "desktop"), ("Downloads", "downloads")] {
        if path.starts_with(home.join(dir)) {
            return category;
        }
    }
    if path.starts_with("/Volumes") {
        return "removableVolumes";
    }
    "folder"
}

/// The body of [`project_pick`], free of `tauri` so it can be tested.
pub async fn pick_project(state: &ProjectState) -> Result<ProjectPickRes, IpcError> {
    pick_project_with(state, None).await
}

/// [`pick_project`] with World: the picked folder is registered as a project
/// at once, with or without a session (A24.4), so `newsession_options` and
/// `session_start` find it.
pub async fn pick_project_with(state: &ProjectState, world: Option<&crate::world::World>) -> Result<ProjectPickRes, IpcError> {
    let Some(chosen) = state.picker.pick().await else {
        return Ok(ProjectPickRes { project: None });
    };
    let real = match std::fs::canonicalize(&chosen) {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err(IpcError::permission_denied(denied_category(&chosen, &state.home)))
        }
        Err(_) => return Err(IpcError::refused("notADirectory")),
    };
    if !std::fs::metadata(&real).is_ok_and(|m| m.is_dir()) {
        return Err(IpcError::refused("notADirectory"));
    }
    // TCC answers on the first listing, not on stat.
    if let Err(e) = std::fs::read_dir(&real) {
        if matches!(e.raw_os_error(), Some(libc::EPERM) | Some(libc::EACCES)) {
            return Err(IpcError::permission_denied(denied_category(&real, &state.home)));
        }
        return Err(IpcError::refused("notADirectory"));
    }
    let id = crate::discovery::location::identify(&real, &state.home).ok_or_else(|| IpcError::refused("notADirectory"))?;
    state.picked.add(&id.project_root);
    state.picked.add(&id.location_path);

    let facts = id.facts();
    if let (Some(world), Some(f)) = (world, facts.clone()) {
        world.register_project(f);
    }
    let known = match &state.places {
        Some(places) => {
            if let (None, Some(facts)) = (world, facts) {
                places.location_seen(facts);
            }
            let view = places.view();
            let order = view.projects.iter().filter(|p| !p.earlier).count() as u32;
            view.projects.into_iter().find(|p| p.id == id.project_id).map(Ok).unwrap_or(Err(order))
        }
        None => Err(0),
    };
    let project = match known {
        Ok(p) => p,
        Err(order) => {
            let root = AbsPath::parse(id.project_root.to_str().unwrap_or("")).map_err(|_| IpcError::refused("notADirectory"))?;
            let base = id.project_root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            Project {
                id: id.project_id.clone(),
                name: crate::render::sanitize::sanitize_display_text(&base, crate::contract::display::TextCap::Chars(255)).text,
                root,
                kind: if id.git { ProjectKind::Git } else { ProjectKind::Folder },
                order,
                folded: false,
                earlier: false,
                last_activity_at: None,
                live_sessions: 0,
                is_temp: id.is_temp,
            }
        }
    };
    Ok(ProjectPickRes { project: Some(project) })
}
