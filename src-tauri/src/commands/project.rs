//! `project` commands (DESIGN §3, `projects / worktree`).
//!
//! The folder itself is chosen by `plugin-dialog` on the frontend; Rust only
//! ever receives a path. Its job here is to refuse anything that is not a git
//! repository — with a localizable, typed error rather than a sentence — and to
//! canonicalize the path exactly the way the worktree layer will later hash it.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::State;

use crate::error::{GildeError, GildeErrorPayload, CmdResult};
use crate::store::models::ProjectRow;
use crate::store::{dao, now_ms, Db};
use crate::worktree::{git as gitcmd, naming};

/// Managed state alias, mirroring `commands::app::PathsState`.
pub type DbState = Db;

/// Deterministic accent for a project, so the same repository always reads the
/// same colour in the sidebar and in the graph.
const PROJECT_COLORS: &[&str] = &[
    "#7c8cf8", "#59b39b", "#d08b5b", "#b57edc", "#5aa6d6", "#c4736f", "#8fa63f", "#c98fb0",
];

/// A project as the frontend sees it (`src/lib/ipc.ts` §7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub id: String,
    pub path: String,
    pub canonical_path: String,
    pub repo_slug: String,
    pub name: String,
    pub color: String,
    pub default_base_ref: String,
    pub added_at: i64,
    pub last_opened_at: Option<i64>,
    /// Whether the folder is still on disk (live check G-b). A project whose
    /// folder is gone is NOT dropped from the list: it stays, marked "Folder not
    /// found", until the user removes it — the list and its count never change
    /// behind the user's back. Computed when the list is read; `true` when absent
    /// on the wire (an older backend).
    #[serde(default = "default_true")]
    pub path_exists: bool,
}

fn default_true() -> bool {
    true
}

/// Whether a project's folder is still there: its path as added, or the
/// repository root it was canonicalized to.
pub fn folder_exists(path: &str, canonical_path: &str) -> bool {
    Path::new(path).is_dir() || Path::new(canonical_path).is_dir()
}

impl From<ProjectRow> for Project {
    fn from(row: ProjectRow) -> Self {
        let color = row
            .color
            .clone()
            .unwrap_or_else(|| color_for(&row.canonical_path));
        Project {
            id: row.id,
            path: row.path,
            canonical_path: row.canonical_path,
            repo_slug: row.repo_slug,
            name: row.name,
            color,
            default_base_ref: row.default_base_ref,
            added_at: row.added_at,
            last_opened_at: row.last_opened_at,
            path_exists: true,
        }
    }
}

fn color_for(canonical_path: &str) -> String {
    let hash = naming::path_hash(Path::new(canonical_path));
    let idx = usize::from_str_radix(&hash[..2], 16).unwrap_or(0) % PROJECT_COLORS.len();
    PROJECT_COLORS[idx].to_string()
}

/// Add (or re-add) a repository.
///
/// Idempotent by canonical path: pointing the picker at a repository that is
/// already registered returns the existing row instead of creating a duplicate,
/// which is what "add" means to a user who forgot they had added it.
#[tauri::command]
pub async fn project_add(path: String, db: State<'_, DbState>) -> CmdResult<Project> {
    add_project(db.inner(), &path).await
}

/// The body of [`project_add`], free of `tauri::State` so it can be tested.
pub async fn add_project(db: &Db, path: &str) -> CmdResult<Project> {
    let input = PathBuf::from(path.trim());
    if input.as_os_str().is_empty() {
        return Err(GildeErrorPayload::from(GildeError::refused("emptyPath")));
    }
    if !input.exists() {
        return Err(GildeErrorPayload::from(GildeError::not_found(
            input.display().to_string(),
        )));
    }

    // `--git-common-dir` resolves correctly even when the user picked one of our
    // linked worktrees, which a `.git` existence check would get wrong.
    let repo = match gitcmd::repo_root(&input).await {
        Ok(root) => root,
        Err(_) => {
            return Err(GildeErrorPayload {
                code: "REFUSED".into(),
                key: "error.project.notARepository".into(),
                params: {
                    let mut m = serde_json::Map::new();
                    m.insert("path".into(), serde_json::Value::from(input.display().to_string()));
                    m
                },
                detail: None,
                retryable: false,
            })
        }
    };

    let canonical = repo.display().to_string();
    let db = db.clone();

    if let Some(existing) = {
        let canonical = canonical.clone();
        db.read(move |conn| dao::project::get_by_canonical_path(conn, &canonical))
            .await?
    } {
        let id = existing.id.clone();
        let at = now_ms();
        db.write(move |conn| dao::project::touch_opened(conn, &id, at).map(|_| ()))
            .await?;
        let mut project = Project::from(existing);
        project.last_opened_at = Some(at);
        return Ok(project);
    }

    let name = repo
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| canonical.clone());
    let default_base_ref = gitcmd::current_branch(&repo)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "HEAD".to_string());

    let now = now_ms();
    let row = ProjectRow {
        id: uuid::Uuid::now_v7().to_string(),
        path: input.display().to_string(),
        canonical_path: canonical.clone(),
        repo_slug: naming::repo_slug(&repo),
        name,
        color: Some(color_for(&canonical)),
        default_base_ref,
        added_at: now,
        last_opened_at: Some(now),
    };

    let insert = row.clone();
    db.write(move |conn| dao::project::upsert(conn, &insert))
        .await?;

    Ok(Project::from(row))
}

#[tauri::command]
pub async fn project_list(db: State<'_, DbState>) -> CmdResult<Vec<Project>> {
    list_projects(db.inner()).await
}

/// The body of [`project_list`], free of `tauri::State` so it can be tested.
/// Every row is returned — a folder that is gone included, flagged.
pub async fn list_projects(db: &Db) -> CmdResult<Vec<Project>> {
    let rows = db.clone().read(dao::project::list).await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let exists = folder_exists(&row.path, &row.canonical_path);
            let mut project = Project::from(row);
            project.path_exists = exists;
            project
        })
        .collect())
}

/// Forget a project. This drops our rows (worktree rows cascade); it never
/// touches the user's repository or their worktree directories.
#[tauri::command]
pub async fn project_remove(id: String, db: State<'_, DbState>) -> CmdResult<()> {
    let db = db.inner().clone();
    let found = db
        .write(move |conn| dao::project::remove(conn, &id))
        .await?;
    if !found {
        return Err(GildeErrorPayload::from(GildeError::not_found("project")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_serializes_in_the_documented_shape() {
        let row = ProjectRow {
            id: "p1".into(),
            path: "/Users/x/work/api".into(),
            canonical_path: "/Users/x/work/api".into(),
            repo_slug: "api-deadbeef".into(),
            name: "api".into(),
            color: None,
            default_base_ref: "main".into(),
            added_at: 1,
            last_opened_at: None,
        };
        let project = Project::from(row);
        assert!(
            project.color.starts_with('#'),
            "color is non-null in the IPC contract: {}",
            project.color
        );

        let v = serde_json::to_value(&project).unwrap();
        assert_eq!(v["canonicalPath"], "/Users/x/work/api");
        assert_eq!(v["repoSlug"], "api-deadbeef");
        assert_eq!(v["defaultBaseRef"], "main");
        assert_eq!(v["lastOpenedAt"], serde_json::Value::Null);
        assert_eq!(v["pathExists"], serde_json::Value::Bool(true));
        assert_eq!(v.as_object().unwrap().len(), 10);

        // An older backend's answer (no `pathExists`) reads as "present".
        let mut old = v.clone();
        old.as_object_mut().unwrap().remove("pathExists");
        let back: Project = serde_json::from_value(old).unwrap();
        assert!(back.path_exists);
    }

    fn row(id: &str, path: &str) -> ProjectRow {
        ProjectRow {
            id: id.into(),
            path: path.into(),
            canonical_path: path.into(),
            repo_slug: format!("{id}-deadbeef"),
            name: id.into(),
            color: None,
            default_base_ref: "main".into(),
            added_at: 1,
            last_opened_at: None,
        }
    }

    /// Live check G-b: a project whose folder is gone stays in the list,
    /// flagged, read after read — it is never pruned by a refresh.
    #[tokio::test]
    async fn a_project_whose_folder_is_gone_stays_listed_and_flagged() {
        let db = Db::open_in_memory().expect("db");
        let here = tempfile::tempdir().expect("tempdir");
        let present = here.path().join("api");
        std::fs::create_dir_all(&present).expect("mkdir");
        let gone = here.path().join("ops-was-here");

        let rows = vec![
            row("api", &present.display().to_string()),
            row("ops", &gone.display().to_string()),
        ];
        db.write(move |conn| {
            for r in &rows {
                dao::project::upsert(conn, r)?;
            }
            Ok(())
        })
        .await
        .expect("insert");

        for read in 0..3 {
            let listed = list_projects(&db).await.expect("list");
            assert_eq!(listed.len(), 2, "read {read}: nothing is dropped");
            let api = listed.iter().find(|p| p.id == "api").expect("api");
            let ops = listed.iter().find(|p| p.id == "ops").expect("ops stays listed");
            assert!(api.path_exists, "read {read}: a present folder is present");
            assert!(!ops.path_exists, "read {read}: a gone folder is flagged");
        }

        // It leaves the list only when the user removes it.
        assert!(db
            .write(|conn| dao::project::remove(conn, "ops"))
            .await
            .expect("remove"));
        let listed = list_projects(&db).await.expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "api");
    }

    #[test]
    fn a_folder_counts_as_present_by_either_of_its_paths() {
        let here = tempfile::tempdir().expect("tempdir");
        let dir = here.path().display().to_string();
        assert!(folder_exists(&dir, "/definitely/not/here"));
        assert!(folder_exists("/definitely/not/here", &dir));
        assert!(!folder_exists("/definitely/not/here", "/nor/here"));
        // A file is not a folder.
        let file = here.path().join("f");
        std::fs::write(&file, b"x").expect("write");
        let file = file.display().to_string();
        assert!(!folder_exists(&file, &file));
    }

    #[test]
    fn color_is_stable_per_repository() {
        let a = color_for("/Users/x/work/api");
        assert_eq!(a, color_for("/Users/x/work/api"));
        assert!(PROJECT_COLORS.contains(&a.as_str()));
    }
}
