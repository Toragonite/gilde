//! T13 — worktree lifecycle safety (DESIGN §7).
//!
//! These tests drive a real `git` against a real temporary repository. They
//! exist to hold the data-loss invariants of DESIGN §0/§7-7 in place:
//!
//! * `git worktree remove --force` and `git branch -d/-D` must not exist in the
//!   source at all — checked by grepping the tree, not by convention.
//! * A live registry process blocks removal in every mode.
//! * A dirty tree cannot be dropped with `KeepBranch`.
//! * `CommitThenRemove` makes a real commit that survives the removal.
//! * `prune` never deletes a directory git forgot about.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use gilde_code_lib::events::bus::{EventBus, RecordingSink};
use gilde_code_lib::model::graph::GraphState;
use gilde_code_lib::paths::AppPaths;
use gilde_code_lib::store::models::{NewPlannedProc, ProcKind, ProjectRow};
use gilde_code_lib::store::{dao, now_ms, Db};
use gilde_code_lib::worktree::{
    git, naming, CreateWorktreeReq, RemoveMode, WorktreeError, WorktreeFailure, WorktreeManager,
};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn git_ok(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} in {} failed: {}",
        cwd.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A one-commit repository under `root`.
fn init_repo(root: &Path) -> PathBuf {
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git_ok(&repo, &["init", "-q", "-b", "main", "."]);
    git_ok(&repo, &["config", "user.email", "test@example.com"]);
    git_ok(&repo, &["config", "user.name", "Test"]);
    git_ok(&repo, &["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.join("README.md"), b"hello\n").unwrap();
    git_ok(&repo, &["add", "-A"]);
    git_ok(&repo, &["commit", "-qm", "init"]);
    std::fs::canonicalize(&repo).unwrap()
}

struct Fixture {
    _tmp: tempfile::TempDir,
    repo: PathBuf,
    db: Db,
    manager: Arc<WorktreeManager>,
    sink: Arc<RecordingSink>,
    project_id: String,
}

async fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let home = std::fs::canonicalize(tmp.path()).unwrap();
    let repo = init_repo(&home);

    let paths = Arc::new(AppPaths::for_home(&home));
    paths.ensure_all().unwrap();

    let db = Db::open_in_memory().unwrap();

    let sink = Arc::new(RecordingSink::default());
    let bus = EventBus::with_sink(
        sink.clone(),
        Arc::new(parking_lot::RwLock::new(GraphState::new())),
    );

    let project = ProjectRow {
        id: uuid::Uuid::now_v7().to_string(),
        path: repo.display().to_string(),
        canonical_path: repo.display().to_string(),
        repo_slug: naming::repo_slug(&repo),
        name: "repo".into(),
        color: None,
        default_base_ref: "main".into(),
        added_at: now_ms(),
        last_opened_at: None,
    };
    let project_id = project.id.clone();
    db.write(move |conn| dao::project::upsert(conn, &project))
        .await
        .unwrap();

    let manager = WorktreeManager::new(db.clone(), paths, Some(bus));

    Fixture {
        _tmp: tmp,
        repo,
        db,
        manager,
        sink,
        project_id,
    }
}

fn create_req(project_id: &str, name: &str) -> CreateWorktreeReq {
    CreateWorktreeReq {
        project_id: project_id.to_string(),
        task_name: name.to_string(),
        ..Default::default()
    }
}

/// Register a fake live PTY process against a worktree, the way `SpawnGuard`
/// would (DESIGN §8-2/§8-3) — enough for guard 1 to see it.
async fn register_live_session(db: &Db, worktree_id: &str, session_id: &str) -> String {
    let spawn_id = uuid::Uuid::now_v7().to_string();
    let instance = uuid::Uuid::now_v7().to_string();

    let inst = gilde_code_lib::store::models::AppInstanceRow {
        id: instance.clone(),
        pid: std::process::id() as i64,
        boot_id: "test-boot".into(),
        version: "0.0.0".into(),
        started_at: now_ms(),
        ended_at: None,
    };
    db.write(move |conn| dao::app_instance::insert(conn, &inst))
        .await
        .unwrap();

    let mut planned = NewPlannedProc::new(
        spawn_id.clone(),
        instance,
        ProcKind::Pty,
        vec!["/usr/local/bin/claude".to_string()],
        "/tmp",
        now_ms(),
    );
    planned.vendor = Some("claude".into());
    planned.session_id = Some(session_id.to_string());
    planned.worktree_id = Some(worktree_id.to_string());

    db.write(move |conn| dao::proc::insert_planned(conn, &planned))
        .await
        .unwrap();
    let id = spawn_id.clone();
    db.write(move |conn| {
        dao::proc::mark_running(conn, &id, 424_242, Some(424_242), Some(1), now_ms()).map(|_| ())
    })
    .await
    .unwrap();
    spawn_id
}

// ---------------------------------------------------------------------------
// 1. the source itself
// ---------------------------------------------------------------------------

fn rust_sources() -> Vec<(PathBuf, String)> {
    fn walk(dir: &Path, out: &mut Vec<(PathBuf, String)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    out.push((path, text));
                }
            }
        }
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut out = Vec::new();
    walk(&root.join("src"), &mut out);
    walk(&root.join("tests"), &mut out);
    out
}

/// DESIGN §7-7: `--force` and branch deletion are not in the vocabulary.
///
/// A git argv can only carry these as string literals, so that is what we look
/// for; prose in doc comments (which explains *why* they are absent) is not a
/// match.
#[test]
fn the_destructive_git_flags_do_not_exist_in_the_source() {
    // A `branch` subcommand immediately followed by a lowercase or uppercase
    // `-d` flag, however the argv is spelled.
    let branch_delete = regex::Regex::new(r#""branch"\s*,\s*"-{1,2}[dD]"#).unwrap();

    for (path, text) in rust_sources() {
        assert!(
            !text.contains("\"--force\""),
            "{}: `--force` must never be passed to git (DESIGN §0 invariant 5)",
            path.display()
        );
        assert!(
            !branch_delete.is_match(&text),
            "{}: the app never deletes a branch (DESIGN §7-7)",
            path.display()
        );
        // §7-4: no in-place checkout in the user's own repository.
        assert!(
            !text.contains("\"checkout\""),
            "{}: `git checkout` is never run by this app (DESIGN §7-4)",
            path.display()
        );
    }
}

// ---------------------------------------------------------------------------
// 2. git.rs against a real repository
// ---------------------------------------------------------------------------

/// The case a naive "walk up to `.git`" check gets wrong.
#[tokio::test]
async fn repo_root_is_correct_from_inside_a_linked_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let home = std::fs::canonicalize(tmp.path()).unwrap();
    let repo = init_repo(&home);

    let wt = home.join("linked");
    git_ok(
        &repo,
        &[
            "worktree",
            "add",
            "--no-track",
            "-b",
            "gilde/probe",
            "--",
            wt.to_str().unwrap(),
            "HEAD",
        ],
    );

    // Inside the linked worktree `.git` is a *file*, so an existence check would
    // answer `linked` instead of `repo`.
    assert!(wt.join(".git").is_file());
    assert_eq!(git::repo_root(&wt).await.unwrap(), repo);
    assert_eq!(git::repo_root(&repo).await.unwrap(), repo);
    assert!(git::repo_root(&home).await.is_err());
}

#[tokio::test]
async fn status_v2_handles_space_quote_and_newline_filenames() {
    let tmp = tempfile::tempdir().unwrap();
    let home = std::fs::canonicalize(tmp.path()).unwrap();
    let repo = init_repo(&home);

    std::fs::write(repo.join("with space.txt"), b"a").unwrap();
    std::fs::write(repo.join("say \"hi\".txt"), b"b").unwrap();
    std::fs::write(repo.join("line\nbreak.txt"), b"c").unwrap();

    let status = git::status_v2(&repo).await.unwrap();
    assert_eq!(status.untracked, 3, "{status:?}");
    let paths = status.paths();
    for expected in ["with space.txt", "say \"hi\".txt", "line\nbreak.txt"] {
        assert!(paths.contains(&expected.to_string()), "missing {expected:?} in {paths:?}");
    }

    // Once staged and modified they become tracked entries with the same names.
    git_ok(&repo, &["add", "-A"]);
    git_ok(&repo, &["commit", "-qm", "odd names"]);
    std::fs::write(repo.join("line\nbreak.txt"), b"changed").unwrap();

    let status = git::status_v2(&repo).await.unwrap();
    assert_eq!(status.dirty_files, 1);
    assert_eq!(status.untracked, 0);
    assert_eq!(status.paths(), vec!["line\nbreak.txt".to_string()]);
    assert!(!status.is_clean());
}

#[tokio::test]
async fn run_git_reports_a_typed_error_with_raw_stderr() {
    let tmp = tempfile::tempdir().unwrap();
    let home = std::fs::canonicalize(tmp.path()).unwrap();
    let repo = init_repo(&home);

    let err = git::verify_ref(&repo, "no/such/ref").await.unwrap_err();
    let payload = gilde_code_lib::error::GildeErrorPayload::from(err);
    assert!(
        payload.code == "GIT" || payload.code == "WORKTREE",
        "{payload:?}"
    );
}

// ---------------------------------------------------------------------------
// 3. the lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_places_the_worktree_where_design_says_and_locks_it() {
    let fx = fixture().await;
    let wt = fx
        .manager
        .create(create_req(&fx.project_id, "Fix Login"))
        .await
        .unwrap();

    assert!(wt.slug.starts_with("fix-login-"));
    assert_eq!(wt.branch, format!("gilde/{}", wt.slug));
    let expected = fx
        .manager
        .paths()
        .worktrees_root()
        .join(naming::repo_slug(&fx.repo))
        .join(&wt.slug);
    assert_eq!(PathBuf::from(&wt.path), expected);
    assert!(expected.join("README.md").exists());
    assert!(wt.locked);
    assert!(!wt.base_sha.is_empty());

    // git agrees, and the lock reason is ours.
    let entries = git::worktree_list(&fx.repo).await.unwrap();
    let entry = entries
        .iter()
        .find(|e| e.path == expected)
        .expect("git knows the worktree");
    assert_eq!(entry.short_branch(), Some(wt.branch.as_str()));
    assert_eq!(entry.locked.as_deref(), Some(git::LOCK_REASON));

    // and so does the database.
    let id = wt.id.clone();
    let row = fx
        .db
        .read(move |conn| dao::worktree::get(conn, &id))
        .await
        .unwrap()
        .expect("row inserted");
    assert_eq!(row.path, wt.path);
    assert_eq!(row.branch, wt.branch);
}

/// The create preflight sees "already checked out" *before* calling
/// `worktree add`, which is what lets the UI say "open the existing worktree"
/// instead of surfacing raw git prose.
#[tokio::test]
async fn branch_in_use_is_detected_before_worktree_add() {
    let fx = fixture().await;
    let wt = fx
        .manager
        .create(create_req(&fx.project_id, "shared"))
        .await
        .unwrap();

    let occupied = gilde_code_lib::worktree::branch_in_use(&fx.repo, &wt.branch)
        .await
        .unwrap();
    assert_eq!(occupied, Some(PathBuf::from(&wt.path)));
    assert_eq!(
        gilde_code_lib::worktree::branch_in_use(&fx.repo, "gilde/never-created")
            .await
            .unwrap(),
        None
    );

    // git really does enforce it, which is why the preflight exists.
    let other = fx.manager.paths().worktrees_root().join("other");
    let out = Command::new("git")
        .arg("-C")
        .arg(&fx.repo)
        .args([
            "worktree",
            "add",
            "--",
            other.to_str().unwrap(),
            wt.branch.as_str(),
        ])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "git must refuse a second checkout of {}",
        wt.branch
    );

    let refusal = WorktreeError::BranchInUse {
        branch: wt.branch.clone(),
        existing_path: wt.path.clone(),
    };
    assert_eq!(refusal.key(), "error.worktree.branchInUse");
    assert_eq!(
        refusal.params()["branch"],
        serde_json::Value::from(wt.branch.clone())
    );
    assert_eq!(
        refusal.params()["existingPath"],
        serde_json::Value::from(wt.path)
    );
}

#[tokio::test]
async fn a_live_session_blocks_removal_in_every_mode() {
    let fx = fixture().await;
    let wt = fx
        .manager
        .create(create_req(&fx.project_id, "busy"))
        .await
        .unwrap();
    register_live_session(&fx.db, &wt.id, "session-1").await;

    let preflight = fx.manager.remove_preflight(&wt.id).await.unwrap();
    assert_eq!(preflight.live_sessions, vec!["session-1".to_string()]);
    assert!(
        preflight.allowed_modes.is_empty(),
        "no mode may be offered while a session is live"
    );

    for mode in [
        RemoveMode::KeepBranch,
        RemoveMode::ArchiveThenRemove,
        RemoveMode::CommitThenRemove,
    ] {
        match fx.manager.remove_typed(&wt.id, mode).await {
            Err(WorktreeFailure::Refused(WorktreeError::SessionsLive { count, .. })) => {
                assert_eq!(count, 1)
            }
            other => panic!("{mode:?} should be blocked, got {other:?}"),
        }
    }
    assert!(PathBuf::from(&wt.path).exists(), "nothing was removed");
}

/// The whole DESIGN §7-7 gate, end to end.
#[tokio::test]
async fn dirty_worktree_walks_the_full_gate_to_commit_then_remove() {
    let fx = fixture().await;
    let wt = fx
        .manager
        .create(create_req(&fx.project_id, "dirty work"))
        .await
        .unwrap();
    let path = PathBuf::from(&wt.path);

    std::fs::write(path.join("new file.txt"), b"work in progress\n").unwrap();
    std::fs::write(path.join("README.md"), b"hello\nmore\n").unwrap();

    let stats = fx.manager.stats(&wt.id).await.unwrap();
    assert_eq!(stats.dirty_files, 1, "README.md is modified");
    assert_eq!(stats.untracked, 1, "new file.txt is untracked");
    assert_eq!(stats.ahead, 0);
    assert!(stats.last_commit_subject.as_deref() == Some("init"));

    let preflight = fx.manager.remove_preflight(&wt.id).await.unwrap();
    assert_eq!(preflight.dirty_files, 1);
    assert_eq!(preflight.untracked, 1);
    assert!(preflight.live_sessions.is_empty());
    assert!(
        !preflight.allowed_modes.contains(&RemoveMode::KeepBranch),
        "a dirty tree may not be dropped with KeepBranch"
    );
    assert!(preflight
        .allowed_modes
        .contains(&RemoveMode::CommitThenRemove));

    match fx.manager.remove_typed(&wt.id, RemoveMode::KeepBranch).await {
        Err(WorktreeFailure::Refused(WorktreeError::Dirty { dirty_files, untracked })) => {
            assert_eq!((dirty_files, untracked), (1, 1));
        }
        other => panic!("KeepBranch must be refused, got {other:?}"),
    }
    assert!(path.exists(), "the refusal removed nothing");

    let outcome = fx
        .manager
        .remove_typed(&wt.id, RemoveMode::CommitThenRemove)
        .await
        .unwrap();
    assert!(outcome.removed);
    assert_eq!(outcome.mode, RemoveMode::CommitThenRemove);
    assert_eq!(outcome.branch_kept, wt.branch);
    let sha = outcome.commit_sha.expect("a real commit was made");
    assert_eq!(sha.len(), 40, "{sha}");
    assert!(!path.exists(), "the directory is gone");

    // The branch survives, and so does the work that was on it.
    let subject = git_ok(&fx.repo, &["log", "-1", "--format=%s", &wt.branch]);
    assert_eq!(subject, "gilde: checkpoint before worktree removal");
    let files = git_ok(&fx.repo, &["ls-tree", "--name-only", "-r", &wt.branch]);
    assert!(files.contains("new file.txt"), "{files}");

    // The row is marked removed rather than deleted.
    let id = wt.id.clone();
    let row = fx
        .db
        .read(move |conn| dao::worktree::get(conn, &id))
        .await
        .unwrap()
        .unwrap();
    assert!(row.removed_at.is_some());
}

#[tokio::test]
async fn archive_then_remove_rescues_the_files_without_the_git_dir() {
    let fx = fixture().await;
    let wt = fx
        .manager
        .create(create_req(&fx.project_id, "archive me"))
        .await
        .unwrap();
    let path = PathBuf::from(&wt.path);
    std::fs::create_dir_all(path.join("nested")).unwrap();
    std::fs::write(path.join("nested/scratch.txt"), b"keep me\n").unwrap();

    let outcome = fx
        .manager
        .remove_typed(&wt.id, RemoveMode::ArchiveThenRemove)
        .await
        .unwrap();
    assert!(outcome.removed);
    let archive = PathBuf::from(outcome.archive_path.expect("archived"));
    assert!(archive.join("nested/scratch.txt").exists());
    assert!(archive.join("README.md").exists());
    assert!(
        !archive.join(".git").exists(),
        "git metadata is not part of a rescue copy"
    );
    assert!(!path.exists());
}

#[tokio::test]
async fn keep_branch_removes_a_clean_worktree_and_keeps_the_branch() {
    let fx = fixture().await;
    let wt = fx
        .manager
        .create(create_req(&fx.project_id, "clean"))
        .await
        .unwrap();

    let preflight = fx.manager.remove_preflight(&wt.id).await.unwrap();
    assert!(preflight.allowed_modes.contains(&RemoveMode::KeepBranch));
    assert_eq!(preflight.dirty_files, 0);

    let outcome = fx
        .manager
        .remove_typed(&wt.id, RemoveMode::KeepBranch)
        .await
        .unwrap();
    assert!(outcome.removed);
    assert!(outcome.commit_sha.is_none());
    assert!(!PathBuf::from(&wt.path).exists());

    // The branch is still there — the app never deletes one.
    let branches = git_ok(&fx.repo, &["branch", "--list", "gilde/*"]);
    assert!(branches.contains(&wt.branch), "{branches}");
}

#[tokio::test]
async fn prune_classifies_all_three_views_and_deletes_nothing_from_disk() {
    let fx = fixture().await;
    let healthy = fx
        .manager
        .create(create_req(&fx.project_id, "healthy"))
        .await
        .unwrap();
    let vanished = fx
        .manager
        .create(create_req(&fx.project_id, "vanished"))
        .await
        .unwrap();

    // Simulate "the user deleted the directory behind our back".
    fx.manager.stop_watch(&vanished.id);
    std::fs::remove_dir_all(&vanished.path).unwrap();

    // And a stray directory git never knew about.
    let stray = fx
        .manager
        .paths()
        .worktrees_root()
        .join(naming::repo_slug(&fx.repo))
        .join("stray-dir");
    std::fs::create_dir_all(stray.join("src")).unwrap();
    std::fs::write(stray.join("src/keep.txt"), b"not ours to delete\n").unwrap();

    let report = fx.manager.prune(&fx.project_id).await.unwrap();

    assert!(report.healthy.contains(&healthy.path), "{report:?}");
    assert!(
        report.disk_only.contains(&stray.display().to_string()),
        "{report:?}"
    );
    assert!(
        report.git_only.contains(&vanished.path) || report.db_only.contains(&vanished.path),
        "{report:?}"
    );
    assert!(
        stray.join("src/keep.txt").exists(),
        "a disk-only directory is reported, never deleted"
    );
    assert!(PathBuf::from(&healthy.path).exists());
}

#[tokio::test]
async fn stats_publish_emits_worktree_changed() {
    let fx = fixture().await;
    let wt = fx
        .manager
        .create(create_req(&fx.project_id, "watched"))
        .await
        .unwrap();
    std::fs::write(PathBuf::from(&wt.path).join("README.md"), b"changed\n").unwrap();

    fx.manager.refresh_and_publish(&wt.id).await;
    // Give the coalescer a tick to hand the event to the sink.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let events = fx.sink.events();
    let changed = events
        .iter()
        .find(|(name, _)| name == "gilde://worktree/changed")
        .unwrap_or_else(|| panic!("no worktree/changed in {events:?}"));
    assert_eq!(changed.1["worktreeId"], serde_json::Value::from(wt.id));
    assert_eq!(changed.1["dirty"], serde_json::Value::from(1));

    // The watcher is armed for a live worktree.
    assert!(fx.manager.watch_count() >= 1);
}

#[tokio::test]
async fn post_create_command_runs_in_the_new_worktree() {
    let fx = fixture().await;
    let wt = fx
        .manager
        .create(CreateWorktreeReq {
            project_id: fx.project_id.clone(),
            task_name: "with hook".into(),
            post_create_cmd: Some("/bin/echo post-create ran".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(wt.post_create_cmd.as_deref(), Some("/bin/echo post-create ran"));

    let result = fx
        .manager
        .run_post_create(&PathBuf::from(&wt.path), "/bin/pwd")
        .await
        .unwrap();
    assert_eq!(result.exit_code, Some(0));
    assert!(
        result.tail.trim().ends_with(&wt.slug),
        "the command runs in the worktree: {}",
        result.tail
    );
}

/// The debounced `notify` watch, end to end: touch a file, wait, and the
/// `gilde://worktree/changed` event arrives without anyone asking for stats.
#[tokio::test]
async fn the_filesystem_watch_publishes_worktree_changed() {
    let fx = fixture().await;
    let wt = fx
        .manager
        .create(create_req(&fx.project_id, "watched live"))
        .await
        .unwrap();
    assert_eq!(fx.manager.watch_count(), 1);

    // Noise inside an ignored directory must not wake anything.
    std::fs::create_dir_all(PathBuf::from(&wt.path).join("node_modules/pkg")).unwrap();
    std::fs::write(
        PathBuf::from(&wt.path).join("node_modules/pkg/index.js"),
        b"noise\n",
    )
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(3_500)).await;
    assert!(
        !fx.sink
            .events()
            .iter()
            .any(|(name, _)| name == "gilde://worktree/changed"),
        "node_modules churn must not publish a change"
    );

    // A real edit does.
    std::fs::write(PathBuf::from(&wt.path).join("README.md"), b"edited\n").unwrap();
    let mut seen = None;
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        if let Some((_, payload)) = fx
            .sink
            .events()
            .into_iter()
            .find(|(name, _)| name == "gilde://worktree/changed")
        {
            seen = Some(payload);
            break;
        }
    }
    let payload = seen.expect("the watcher published gilde://worktree/changed");
    assert_eq!(payload["worktreeId"], serde_json::Value::from(wt.id));
    assert_eq!(payload["dirty"], serde_json::Value::from(1));
}

#[tokio::test]
async fn lock_and_unlock_round_trip() {
    let fx = fixture().await;
    let wt = fx
        .manager
        .create(create_req(&fx.project_id, "lockable"))
        .await
        .unwrap();

    fx.manager.set_locked(&wt.id, false).await.unwrap();
    let entries = git::worktree_list(&fx.repo).await.unwrap();
    let entry = entries
        .iter()
        .find(|e| e.path == PathBuf::from(&wt.path))
        .unwrap();
    assert!(entry.locked.is_none(), "{entry:?}");

    fx.manager.set_locked(&wt.id, true).await.unwrap();
    let entries = git::worktree_list(&fx.repo).await.unwrap();
    let entry = entries
        .iter()
        .find(|e| e.path == PathBuf::from(&wt.path))
        .unwrap();
    assert!(entry.locked.is_some());
}

// ---------------------------------------------------------------------------
// 4. project_add
// ---------------------------------------------------------------------------

#[tokio::test]
async fn project_add_accepts_a_repository_and_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let home = std::fs::canonicalize(tmp.path()).unwrap();
    let repo = init_repo(&home);
    let db = Db::open_in_memory().unwrap();

    let project = gilde_code_lib::commands::project::add_project(
        &db,
        repo.to_str().unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(project.canonical_path, repo.display().to_string());
    assert_eq!(project.repo_slug, naming::repo_slug(&repo));
    assert_eq!(project.default_base_ref, "main");
    assert!(project.color.starts_with('#'));

    // Adding it again returns the same row rather than a duplicate.
    let again =
        gilde_code_lib::commands::project::add_project(&db, repo.to_str().unwrap())
            .await
            .unwrap();
    assert_eq!(again.id, project.id);
    let rows = db.read(dao::project::list).await.unwrap();
    assert_eq!(rows.len(), 1);
}

/// A folder that is not a repository is refused with a localizable key, not a
/// sentence (DESIGN §9-9).
#[tokio::test]
async fn project_add_refuses_a_non_repository_with_a_typed_error() {
    let tmp = tempfile::tempdir().unwrap();
    let home = std::fs::canonicalize(tmp.path()).unwrap();
    let plain = home.join("not-a-repo");
    std::fs::create_dir_all(&plain).unwrap();
    let db = Db::open_in_memory().unwrap();

    let err =
        gilde_code_lib::commands::project::add_project(&db, plain.to_str().unwrap())
            .await
            .expect_err("must be refused");
    assert_eq!(err.code, "REFUSED");
    assert_eq!(err.key, "error.project.notARepository");
    assert_eq!(
        err.params["path"],
        serde_json::Value::from(plain.display().to_string())
    );
    assert!(!err.retryable);

    let missing =
        gilde_code_lib::commands::project::add_project(&db, "/nope/nowhere")
            .await
            .expect_err("must be refused");
    assert_eq!(missing.code, "NOT_FOUND");

    let empty = gilde_code_lib::commands::project::add_project(&db, "   ")
        .await
        .expect_err("must be refused");
    assert_eq!(empty.code, "REFUSED");
}

/// The picker may point at one of *our* linked worktrees; the project must
/// still resolve to the main repository.
#[tokio::test]
async fn project_add_from_inside_a_linked_worktree_resolves_the_repository() {
    let tmp = tempfile::tempdir().unwrap();
    let home = std::fs::canonicalize(tmp.path()).unwrap();
    let repo = init_repo(&home);
    let wt = home.join("linked");
    git_ok(
        &repo,
        &[
            "worktree",
            "add",
            "--no-track",
            "-b",
            "gilde/linked",
            "--",
            wt.to_str().unwrap(),
            "HEAD",
        ],
    );

    let db = Db::open_in_memory().unwrap();
    let project =
        gilde_code_lib::commands::project::add_project(&db, wt.to_str().unwrap())
            .await
            .unwrap();
    assert_eq!(project.canonical_path, repo.display().to_string());
}
