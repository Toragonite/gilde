//! Worktree lifecycle safety (DESIGN §7; Gilde 2 contract §4.2.5, §9.3, A6,
//! A14.2; owner T11).
//!
//! These tests drive a real `git` against real temporary repositories only
//! (never the user's, never this checkout). They hold the data-loss
//! invariants in place:
//!
//! * `git worktree remove --force` and `git branch -d/-D` must not exist in the
//!   source at all — checked by grepping the tree, not by convention.
//! * A live registry process blocks removal in every mode.
//! * A dirty tree cannot be dropped with `KeepBranch`.
//! * `CommitThenRemove` makes a real commit that survives the removal.
//! * `prune` never deletes a directory git forgot about.
//! * The post-create command is planned through `SupervisorApi` and a refused
//!   plan never runs it (A12.8, A14.2) — each case in its own process, since
//!   the supervisor binding is once per process.
//! * A6: the LP-40 worktree root `<repo>/.gilde/worktrees/` only behind its
//!   flag, with one `.git/info/exclude` entry and never a `.gitignore` edit.
//! * Discard keeps the branch, needs every loss acknowledged against an
//!   unchanged preview, and never touches the main checkout; merge refuses a
//!   dirty main checkout and a conflicting merge without touching anything.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use gilde_code_lib::events::{ToastEvent, WorktreeChangedEvent};
use gilde_code_lib::paths::AppPaths;
use gilde_code_lib::store::models::{NewPlannedProc, ProcKind, ProjectRow};
use gilde_code_lib::store::{dao, now_ms, Db};
use gilde_code_lib::worktree::{
    git, naming, CreateWorktreeReq, RemoveMode, WorktreeError, WorktreeEvents, WorktreeFailure, WorktreeManager,
};
use rusty_fork::rusty_fork_test;

/// What the manager emitted (no Tauri app, no legacy graph — A7.9).
#[derive(Default)]
struct RecordingEvents {
    changed: parking_lot::Mutex<Vec<WorktreeChangedEvent>>,
    toasts: parking_lot::Mutex<Vec<ToastEvent>>,
}

impl WorktreeEvents for RecordingEvents {
    fn changed(&self, event: WorktreeChangedEvent) {
        self.changed.lock().push(event);
    }
    fn toast(&self, toast: ToastEvent) {
        self.toasts.lock().push(toast);
    }
}

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
    sink: Arc<RecordingEvents>,
    project_id: String,
}

async fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let home = std::fs::canonicalize(tmp.path()).unwrap();
    let repo = init_repo(&home);

    let paths = Arc::new(AppPaths::for_home(&home));
    paths.ensure_all().unwrap();

    let db = Db::open_in_memory().unwrap();

    let sink = Arc::new(RecordingEvents::default());

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

    let manager = WorktreeManager::with_events(db.clone(), paths, sink.clone());

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

/// DESIGN §7-7, A36.4, A36.5: branch deletion is not in the vocabulary;
/// `--force` exists exactly once, in the one helper that removes a worktree
/// whose every loss was acknowledged; there is no repo-wide `worktree prune`
/// and the worktree module never deletes a directory itself.
///
/// A git argv can only carry these as string literals, so that is what we look
/// for; prose in doc comments (which explains *why* they are absent) is not a
/// match.
#[test]
fn the_destructive_git_flags_do_not_exist_in_the_source() {
    // A `branch` subcommand immediately followed by a lowercase or uppercase
    // `-d` flag, however the argv is spelled.
    let branch_delete = regex::Regex::new(r#""branch"\s*,\s*"-{1,2}[dD]"#).unwrap();
    let prune = regex::Regex::new(r#""worktree"\s*,\s*"prune""#).unwrap();
    let force = "\"--force\"";

    for (path, text) in rust_sources() {
        let forces = text.matches(force).count();
        if path.ends_with("src/worktree/git.rs") {
            assert_eq!(forces, 1, "{}: exactly one `--force` (A36.4)", path.display());
            let at = text.find(force).unwrap();
            let owner = text[..at].rfind("pub async fn ").map(|i| &text[i..at]).unwrap_or("");
            assert!(
                owner.starts_with("pub async fn remove_worktree_acknowledged("),
                "{}: `--force` lives only in remove_worktree_acknowledged",
                path.display()
            );
        } else {
            assert_eq!(forces, 0, "{}: `--force` must never be passed to git outside the one helper (A36.4)", path.display());
        }
        assert!(!prune.is_match(&text), "{}: never a repo-wide `git worktree prune` (A36.5)", path.display());
        if path.ends_with("src/worktree/mod.rs") {
            assert!(!text.contains("remove_dir_all("), "{}: no directory is deleted by Gilde itself (A36.4)", path.display());
        }
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

    let events = fx.sink.changed.lock().clone();
    let changed = events
        .iter()
        .find(|e| e.worktree_id == wt.id)
        .unwrap_or_else(|| panic!("no worktree/changed in {events:?}"));
    assert_eq!(changed.dirty, 1);

    // The watcher is armed for a live worktree.
    assert!(fx.manager.watch_count() >= 1);
}

/// A14.2 without a supervisor: the worktree is created, the post-create
/// command is refused and never runs (no registry, no spawn).
#[tokio::test]
async fn post_create_without_a_supervisor_is_refused_and_never_runs() {
    let fx = fixture().await;
    let mark = fx._tmp.path().join("post-create-ran");
    let script = write_script(fx._tmp.path(), "mark.sh", &format!("touch '{}'", mark.display()));
    let wt = fx
        .manager
        .create(CreateWorktreeReq {
            project_id: fx.project_id.clone(),
            task_name: "with hook".into(),
            post_create_cmd: Some(script.display().to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(wt.post_create_cmd.as_deref(), Some(script.to_str().unwrap()));
    let err = fx
        .manager
        .run_post_create(&PathBuf::from(&wt.path), &script.to_string_lossy())
        .await
        .expect_err("no supervisor → refused, never run untracked");
    let err = gilde_code_lib::contract::IpcError::from(&err);
    assert_eq!(err.params["reason"], "notAllowlisted");
    assert!(!mark.exists(), "the command never ran (neither at create nor here)");
    assert!(
        fx.sink.toasts.lock().iter().any(|t| t.key == "worktree.postCreate.failed"),
        "the refusal is reported, not swallowed"
    );
}

fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let script = dir.join(name);
    std::fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

/// A recording supervisor: refuses every plan, or accepts and records (with
/// the order of plan → running → exited in `log`).
struct FakeSupervisor {
    refuse: Option<gilde_code_lib::contract::IpcError>,
    planned: parking_lot::Mutex<Vec<gilde_code_lib::contract::services::SpawnRequest>>,
    running: parking_lot::Mutex<Vec<String>>,
    exited: parking_lot::Mutex<Vec<(String, Option<i32>)>>,
    log: parking_lot::Mutex<Vec<String>>,
    /// What happens when a `git worktree remove` is planned (A36.6 tests).
    on_remove: parking_lot::Mutex<OnRemove>,
}

/// A forced failure of the removal step only.
#[derive(Clone, Default)]
enum OnRemove {
    #[default]
    Nothing,
    /// The plan is refused: the removal never runs.
    Refuse(gilde_code_lib::contract::IpcError),
    /// A file appears in the tree just before git runs (an agent racing the
    /// discard), so git's own clean check refuses before it deletes anything.
    Touch(PathBuf),
}

impl FakeSupervisor {
    fn new(refuse: Option<gilde_code_lib::contract::IpcError>) -> Arc<FakeSupervisor> {
        Arc::new(FakeSupervisor {
            refuse,
            planned: Default::default(),
            running: Default::default(),
            exited: Default::default(),
            log: Default::default(),
            on_remove: Default::default(),
        })
    }
}

#[async_trait::async_trait]
impl gilde_code_lib::contract::services::SupervisorApi for FakeSupervisor {
    async fn spawn_gate(&self) -> Result<(), gilde_code_lib::contract::IpcError> {
        Ok(())
    }
    async fn plan_spawn(
        &self,
        req: gilde_code_lib::contract::services::SpawnRequest,
    ) -> Result<gilde_code_lib::contract::services::SpawnTicket, gilde_code_lib::contract::IpcError> {
        if let Some(e) = &self.refuse {
            return Err(e.clone());
        }
        let removal = req.args.windows(2).any(|w| w[0] == "worktree" && w[1] == "remove");
        if removal {
            match self.on_remove.lock().clone() {
                OnRemove::Nothing => {}
                OnRemove::Refuse(e) => return Err(e),
                OnRemove::Touch(path) => std::fs::write(path, b"written while the discard ran\n").unwrap(),
            }
        }
        let mut planned = self.planned.lock();
        planned.push(req);
        let id = format!("spawn-{}", planned.len());
        self.log.lock().push(format!("plan {id}"));
        Ok(gilde_code_lib::contract::services::SpawnTicket { spawn_id: id.clone(), env_marker: id })
    }
    async fn mark_running(
        &self,
        ticket: &gilde_code_lib::contract::services::SpawnTicket,
        _pid: i32,
    ) -> Result<(), gilde_code_lib::contract::IpcError> {
        self.running.lock().push(ticket.spawn_id.clone());
        self.log.lock().push(format!("running {}", ticket.spawn_id));
        Ok(())
    }
    async fn mark_exited(
        &self,
        ticket: &gilde_code_lib::contract::services::SpawnTicket,
        exit: gilde_code_lib::contract::activity::ExitInfo,
    ) {
        self.exited.lock().push((ticket.spawn_id.clone(), exit.code));
        self.log.lock().push(format!("exited {} {:?}", ticket.spawn_id, exit.code));
    }
    async fn signal(
        &self,
        _target: &gilde_code_lib::contract::ids::ProcessKey,
        _plan: gilde_code_lib::contract::services::SignalPlan,
    ) -> Result<gilde_code_lib::contract::services::SignalOutcome, gilde_code_lib::contract::IpcError> {
        Ok(gilde_code_lib::contract::services::SignalOutcome { exited: true, survivor: false })
    }
    fn is_vendor_cli(&self, _exec_path: &str) -> bool {
        false
    }
    fn redact_argv(&self, argv: &[String]) -> Vec<String> {
        argv.to_vec()
    }
    fn memory_only(&self) -> Option<String> {
        None
    }
    fn last_scan(&self) -> Option<gilde_code_lib::contract::services::ProcSnapshot> {
        None
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap()
}

rusty_fork_test! {
    /// A14.2 through `SupervisorApi` (A12.8): a refused plan returns the
    /// refusal and launches no process.
    #[test]
    fn a_post_create_refused_by_the_supervisor_launches_no_process() {
        runtime().block_on(async {
            let sup = FakeSupervisor::new(Some(gilde_code_lib::contract::IpcError::refused("notAllowlisted")));
            assert!(git::bind_supervisor(sup.clone()));
            let tmp = tempfile::tempdir().unwrap();
            let mark = tmp.path().join("ran");
            let script = write_script(tmp.path(), "mark.sh", &format!("touch '{}'", mark.display()));
            let db = Db::open_in_memory().unwrap();
            let manager = WorktreeManager::new(db, Arc::new(AppPaths::for_home(tmp.path())), None);
            let err = manager.run_post_create(tmp.path(), &script.to_string_lossy()).await.unwrap_err();
            let err = gilde_code_lib::contract::IpcError::from(&err);
            assert_eq!(err.code, gilde_code_lib::contract::ErrorCode::Refused);
            assert_eq!(err.params["reason"], "notAllowlisted");
            assert!(!mark.exists(), "the refused command never ran");
            assert!(sup.running.lock().is_empty() && sup.exited.lock().is_empty());
        });
    }

    /// A17.1: with a supervisor bound, every git invocation is planned as kind
    /// `git` before it exists, and its row is closed with the exit code.
    #[test]
    fn git_is_planned_as_kind_git_through_a_bound_supervisor() {
        runtime().block_on(async {
            let sup = FakeSupervisor::new(None);
            assert!(git::bind_supervisor(sup.clone()));
            let tmp = tempfile::tempdir().unwrap();
            let dir = std::fs::canonicalize(tmp.path()).unwrap();
            let out = git::run_git(&dir, &["--version"]).await.unwrap();
            assert!(out.text().starts_with("git version"), "{}", out.text());
            let planned = sup.planned.lock().clone();
            assert_eq!(planned.len(), 1);
            assert_eq!(planned[0].kind, gilde_code_lib::contract::process::ProcKind::Git);
            assert_eq!(planned[0].argv0.as_str(), git::git_binary().to_str().unwrap());
            // A37.6: every call switches off the repository's fsmonitor and,
            // outside the user's own commit and merge, its hooks.
            let expected: Vec<String> = ["-C", dir.to_str().unwrap(), "-c", "core.fsmonitor=false", "-c", "core.hooksPath=/dev/null", "--version"]
                .iter()
                .map(|s| s.to_string())
                .collect();
            assert_eq!(planned[0].args, expected);
            assert_eq!(planned[0].env_keys, vec!["GIT_TERMINAL_PROMPT".to_string(), "GILDE_SPAWN_ID".to_string()]);
            assert_eq!(
                sup.log.lock().clone(),
                vec!["plan spawn-1".to_string(), "running spawn-1".to_string(), "exited spawn-1 Some(0)".to_string()],
                "write-before-spawn, then the exit"
            );

            // The Gilde 2 service plans every one of its git calls the same way.
            let repo = init_repo(&dir);
            let before = sup.planned.lock().len();
            let commit = git::resolve_commit(&repo, "HEAD").await.unwrap();
            assert!(commit.is_some());
            let later = sup.planned.lock()[before..].to_vec();
            assert!(!later.is_empty() && later.iter().all(|r| r.kind == gilde_code_lib::contract::process::ProcKind::Git));
            // A read-only query also tells git not to take optional locks.
            assert!(later[0].env_keys.iter().any(|k| k == "GIT_OPTIONAL_LOCKS"), "{:?}", later[0].env_keys);
        });
    }

    /// A refused git plan returns the supervisor's own code and git never runs.
    #[test]
    fn a_refused_git_plan_never_runs_git() {
        runtime().block_on(async {
            let sup = FakeSupervisor::new(Some(gilde_code_lib::contract::IpcError::disk_full()));
            assert!(git::bind_supervisor(sup.clone()));
            let tmp = tempfile::tempdir().unwrap();
            let dir = std::fs::canonicalize(tmp.path()).unwrap();
            match git::exec(&dir, &["init", "-q", "fresh"], git::ExecOpts::default()).await {
                Err(git::GitError::Spawn(e)) => assert_eq!(e.code, gilde_code_lib::contract::ErrorCode::DiskFull),
                other => panic!("expected the refusal, got {other:?}"),
            }
            assert!(!dir.join("fresh").exists(), "git never ran");
            let e = gilde_code_lib::contract::IpcError::from(&git::run_git(&dir, &["status"]).await.unwrap_err());
            assert_eq!(e.code, gilde_code_lib::contract::ErrorCode::DiskFull, "the v1 wrapper keeps the code");
        });
    }

    /// A17.4 fix (a): a refusal inside a helper the Gilde 2 paths use keeps
    /// its code (it used to surface as `SPAWN_FAILED`), and `create_worktree`
    /// passes a spawn refusal through instead of calling it a worktree gate.
    #[test]
    fn a_refusal_inside_the_worktree_helpers_keeps_its_code() {
        runtime().block_on(async {
            let w = v2::world();
            // A linked worktree made with plain git before the refusing
            // supervisor is bound.
            let wt = w.home.join("repo-wt/plain");
            git_ok(&w.repo, &["worktree", "add", "--no-track", "-b", "gilde/plain", "--", wt.to_str().unwrap(), "HEAD"]);
            let s = v2::session("claude", &wt, None, v2::control("notRunning"));
            let uid = s.uid.clone();
            w.places.sessions.lock().push(s);
            let sup = FakeSupervisor::new(Some(gilde_code_lib::contract::IpcError::not_ready("bootSweep")));
            assert!(git::bind_supervisor(sup.clone()));
            let svc = w.service();
            use gilde_code_lib::contract::services::GitApi as _;
            let e = svc
                .discard_preview(&gilde_code_lib::contract::ipc::changes::WorktreeDiscardPreviewReq { uid })
                .await
                .unwrap_err();
            assert_eq!((e.code, e.params["reason"].as_str()), (gilde_code_lib::contract::ErrorCode::NotReady, Some("bootSweep")));
            let e = svc.create_worktree(&w.project, "later", None).await.unwrap_err();
            assert_eq!((e.code, e.params["reason"].as_str()), (gilde_code_lib::contract::ErrorCode::NotReady, Some("bootSweep")));
            assert!(!w.home.join("repo-wt/later").exists());
        });
    }

    /// An accepted plan is written before the spawn, stamped on the child
    /// (`GILDE_SPAWN_ID`) and closed with the exit code.
    #[test]
    fn an_accepted_post_create_is_registered_before_it_runs() {
        runtime().block_on(async {
            let sup = FakeSupervisor::new(None);
            assert!(git::bind_supervisor(sup.clone()));
            let tmp = tempfile::tempdir().unwrap();
            let cwd = std::fs::canonicalize(tmp.path()).unwrap();
            let script = write_script(&cwd, "who.sh", "pwd; echo \"marker=$GILDE_SPAWN_ID\"");
            let db = Db::open_in_memory().unwrap();
            let manager = WorktreeManager::new(db, Arc::new(AppPaths::for_home(&cwd)), None);
            let result = manager.run_post_create(&cwd, &script.to_string_lossy()).await.unwrap();
            assert_eq!(result.exit_code, Some(0));
            assert!(result.tail.contains(cwd.to_str().unwrap()), "it runs in the given cwd: {}", result.tail);
            assert!(result.tail.contains("marker=spawn-1"), "the row id is stamped: {}", result.tail);
            let planned = sup.planned.lock();
            assert_eq!(planned.len(), 1);
            assert_eq!(planned[0].kind, gilde_code_lib::contract::process::ProcKind::Probe);
            assert_eq!(planned[0].argv0.as_str(), script.to_str().unwrap());
            assert_eq!(sup.running.lock().as_slice(), ["spawn-1".to_string()]);
            assert_eq!(sup.exited.lock().as_slice(), [("spawn-1".to_string(), Some(0))]);
        });
    }
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
        fx.sink.changed.lock().is_empty(),
        "node_modules churn must not publish a change"
    );

    // A real edit does (event-driven wait with a generous ceiling, A10.7).
    std::fs::write(PathBuf::from(&wt.path).join("README.md"), b"edited\n").unwrap();
    let mut seen = None;
    for _ in 0..120 {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        if let Some(e) = fx.sink.changed.lock().iter().find(|e| e.worktree_id == wt.id) {
            seen = Some(e.clone());
            break;
        }
    }
    let event = seen.expect("the watcher published gilde://worktree/changed");
    assert_eq!(event.dirty, 1);
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
        .find(|e| e.path.as_path() == Path::new(&wt.path))
        .unwrap();
    assert!(entry.locked.is_none(), "{entry:?}");

    fx.manager.set_locked(&wt.id, true).await.unwrap();
    let entries = git::worktree_list(&fx.repo).await.unwrap();
    let entry = entries
        .iter()
        .find(|e| e.path.as_path() == Path::new(&wt.path))
        .unwrap();
    assert!(entry.locked.is_some());
}

// ---------------------------------------------------------------------------
// 4. Gilde 2: new worktrees (A6), discard and merge gates (§4.2.5)
// ---------------------------------------------------------------------------

mod v2 {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use gilde_code_lib::contract::common::{AbsPath, Notice, Vendor};
    use gilde_code_lib::contract::compat::ClaudeCompat;
    use gilde_code_lib::contract::ids::{LocationId, ProjectId, SessionUid};
    use gilde_code_lib::contract::ipc::changes::{
        GitMergePreviewReq, GitMergeReq, LossKind, MergeKind, WorktreeDiscardPreviewReq, WorktreeDiscardReq,
    };
    use gilde_code_lib::contract::project::{Location, LocationKind, Project, ProjectKind};
    use gilde_code_lib::contract::services::{CompatApi, GitApi, LocationFacts};
    use gilde_code_lib::contract::session::{BinaryRef, Session, SessionDetails};
    use gilde_code_lib::contract::{ErrorCode, IpcError};
    use gilde_code_lib::worktree::{GitService, PlaceSource, PlaceView};
    use serde_json::{json, Value};

    use super::{git_ok, init_repo};

    #[derive(Default)]
    pub(super) struct FakePlaces {
        pub sessions: parking_lot::Mutex<Vec<Session>>,
        pub locations: parking_lot::Mutex<Vec<Location>>,
        pub projects: parking_lot::Mutex<Vec<Project>>,
        pub transcripts: parking_lot::Mutex<HashMap<SessionUid, (AbsPath, Vendor)>>,
        pub seen: parking_lot::Mutex<Vec<LocationFacts>>,
    }

    impl PlaceSource for FakePlaces {
        fn session(&self, uid: &SessionUid) -> Option<Session> {
            self.sessions.lock().iter().find(|s| &s.uid == uid).cloned()
        }
        fn view(&self) -> PlaceView {
            PlaceView {
                sessions: self.sessions.lock().clone(),
                locations: self.locations.lock().clone(),
                projects: self.projects.lock().clone(),
            }
        }
        fn transcript(&self, uid: &SessionUid) -> Option<(AbsPath, Vendor)> {
            self.transcripts.lock().get(uid).cloned()
        }
        fn location_seen(&self, facts: LocationFacts) {
            self.seen.lock().push(facts);
        }
    }

    struct Lp40(bool);

    #[async_trait::async_trait]
    impl CompatApi for Lp40 {
        fn list(&self) -> Vec<ClaudeCompat> {
            Vec::new()
        }
        fn lookup(&self, _version: &str) -> Option<ClaudeCompat> {
            None
        }
        async fn for_binary(&self, _binary: &BinaryRef) -> ClaudeCompat {
            unimplemented!("not used by the worktree service")
        }
        fn feature(&self, _version: &str, _flag: &str) -> bool {
            false
        }
        fn app_feature(&self, flag: &str) -> bool {
            self.0 && flag == gilde_code_lib::worktree::LP40_FLAG
        }
        fn notices(&self, _version: &str) -> Vec<Notice> {
            Vec::new()
        }
        fn schedule_canary(&self, _binary: &BinaryRef) {}
    }

    pub(super) fn control(kind: &str) -> Value {
        match kind {
            "watching" => json!({"kind": "watching", "origin": "terminal", "owner": null}),
            "inGilde" => json!({"kind": "inGilde", "mode": "terminal", "pty": null, "hostedTeam": false, "transport": "l1"}),
            _ => json!({"kind": "notRunning", "lastOrigin": null, "endedHow": "ended"}),
        }
    }

    pub(super) fn session(vendor: &str, cwd: &Path, location: Option<&LocationId>, control: Value) -> Session {
        let details = serde_json::to_value(SessionDetails::default()).unwrap();
        serde_json::from_value(json!({
            "uid": SessionUid::mint(), "vendor": vendor, "accountId": null, "vendorSessionId": null,
            "projectId": null, "locationId": location, "cwd": cwd.to_str().unwrap(), "control": control,
            "activity": {"state": {"kind": "working"}, "since": 1, "confidence": "high", "evidence": null, "episode": "e_0123456789abcdef"},
            "title": null, "team": null, "startedAt": null, "endedAt": null, "lastActivityAt": null,
            "pending": [], "queued": [], "model": null, "permissionMode": null, "binary": null,
            "claudeVersion": null, "memoryBytes": null, "usageToday": null,
            "caps": {"send": "none", "interrupt": false, "forceStop": false, "approve": "none", "setModel": false,
                     "setPermissionMode": [], "continueOptions": [], "stopSession": "none", "terminalView": false,
                     "logView": false, "changes": "full", "ownerFocus": false},
            "details": details, "mergedInto": null
        }))
        .expect("a session")
    }

    pub(super) struct World {
        pub _tmp: tempfile::TempDir,
        pub home: PathBuf,
        pub repo: PathBuf,
        pub project: ProjectId,
        pub places: Arc<FakePlaces>,
    }

    pub(super) fn world() -> World {
        let tmp = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(tmp.path()).unwrap();
        let repo = init_repo(&home);
        std::fs::write(repo.join(".gitignore"), b".env\n").unwrap();
        git_ok(&repo, &["add", "-A"]);
        git_ok(&repo, &["commit", "-qm", "ignore .env"]);
        let id = gilde_code_lib::discovery::location::identify(&repo, &home).expect("identity");
        let places = Arc::new(FakePlaces::default());
        places.projects.lock().push(Project {
            id: id.project_id.clone(),
            name: "repo".into(),
            root: AbsPath::parse(repo.to_str().unwrap()).unwrap(),
            kind: ProjectKind::Git,
            order: 0,
            folded: false,
            earlier: false,
            last_activity_at: None,
            live_sessions: 0,
            is_temp: false,
        });
        World { _tmp: tmp, home, repo, project: id.project_id, places }
    }

    impl World {
        pub fn service(&self) -> GitService {
            GitService::with_source(self.places.clone(), self.home.clone())
        }

        /// A new worktree plus a session (in `control`) placed in it.
        pub async fn worktree(&self, svc: &GitService, slug: &str, control_kind: &str) -> (PathBuf, SessionUid) {
            let facts = svc.create_worktree(&self.project, slug, None).await.expect("create");
            let path = PathBuf::from(facts.location_path.as_str());
            self.places.locations.lock().push(Location {
                id: facts.location_id.clone(),
                project_id: facts.project_id.clone(),
                path: facts.location_path.clone(),
                kind: LocationKind::Worktree,
                branch: facts.branch.clone(),
                worktree_name: Some(slug.into()),
                gilde_created: true,
                exists: true,
            });
            let s = session("claude", &path, Some(&facts.location_id), control(control_kind));
            let uid = s.uid.clone();
            self.places.sessions.lock().push(s);
            (path, uid)
        }
    }

    fn assert_err(e: &IpcError, code: ErrorCode, key: &str, value: &str) {
        assert_eq!(e.code, code, "{e:?}");
        assert_eq!(e.params[key], value, "{e:?}");
    }

    fn exclude_text(repo: &Path) -> String {
        std::fs::read_to_string(repo.join(".git/info/exclude")).unwrap_or_default()
    }

    /// A6: the default root stays beside the repository; the LP-40 root lives
    /// inside it only with the flag on, excluded through `.git/info/exclude`
    /// exactly once — never through the tracked `.gitignore`.
    #[tokio::test]
    async fn a6_the_in_repo_root_is_behind_its_flag_and_uses_info_exclude_only() {
        let w = world();
        let gitignore_before = std::fs::read(w.repo.join(".gitignore")).unwrap();

        let off = w.service();
        let facts = off.create_worktree(&w.project, "feat-a", None).await.unwrap();
        assert_eq!(PathBuf::from(facts.location_path.as_str()), w.home.join("repo-wt/feat-a"));
        assert_eq!(facts.branch.as_deref(), Some("gilde/feat-a"));
        assert!(facts.worktree && facts.git);
        assert!(!exclude_text(&w.repo).contains("/.gilde/"), "no exclude entry without the flag");
        assert_eq!(w.places.seen.lock().len(), 1, "World is told about the new location");
        let listed = git::worktree_list(&w.repo).await.unwrap();
        let entry = listed.iter().find(|e| e.short_branch() == Some("gilde/feat-a")).expect("git knows it");
        assert_eq!(entry.locked.as_deref(), Some(git::LOCK_REASON));

        let on = w.service().with_compat(Arc::new(Lp40(true)));
        let facts = on.create_worktree(&w.project, "feat-b", None).await.unwrap();
        assert_eq!(PathBuf::from(facts.location_path.as_str()), w.repo.join(".gilde/worktrees/feat-b"));
        on.create_worktree(&w.project, "feat-c", None).await.unwrap();
        let text = exclude_text(&w.repo);
        assert_eq!(text.lines().filter(|l| l.trim() == "/.gilde/").count(), 1, "one entry, once: {text}");
        assert_eq!(std::fs::read(w.repo.join(".gitignore")).unwrap(), gitignore_before, ".gitignore untouched");
        assert_eq!(git_ok(&w.repo, &["status", "--porcelain"]), "", "the main checkout stays clean");

        // Value rules and the gates of `session_start{NewWorktree}`.
        let e = off.create_worktree(&w.project, "feat-a", None).await.unwrap_err();
        assert_err(&e, ErrorCode::Conflict, "changed", "worktreeExists");
        let e = off.create_worktree(&w.project, "Bad Slug", None).await.unwrap_err();
        assert_err(&e, ErrorCode::InvalidArgument, "rule", "pattern");
        let e = off.create_worktree(&w.project, "", None).await.unwrap_err();
        assert_err(&e, ErrorCode::InvalidArgument, "rule", "required");
        let e = off.create_worktree(&w.project, "x", Some("-q")).await.unwrap_err();
        assert_err(&e, ErrorCode::InvalidArgument, "field", "location.base");
        let e = off.create_worktree(&w.project, "x", Some("no/such/ref")).await.unwrap_err();
        assert_err(&e, ErrorCode::Refused, "reason", "worktreeGate");
        let e = off.create_worktree(&ProjectId::derive("/nowhere"), "x", None).await.unwrap_err();
        assert_eq!(e.code, ErrorCode::NotFound);
        // An existing branch of that name is never silently reused.
        git_ok(&w.repo, &["branch", "gilde/taken"]);
        let e = off.create_worktree(&w.project, "taken", None).await.unwrap_err();
        assert_err(&e, ErrorCode::Conflict, "changed", "worktreeExists");
    }

    /// Discard keeps the branch, refuses a branch delete, needs every loss
    /// acknowledged against an unchanged preview, and refuses while a session
    /// is live or the owner is alive.
    #[tokio::test]
    async fn discard_walks_every_gate_and_keeps_the_branch() {
        let w = world();
        let svc = w.service();
        let (path, uid) = w.worktree(&svc, "throwaway", "notRunning").await;
        std::fs::write(path.join("README.md"), b"changed\n").unwrap();
        std::fs::write(path.join("new.txt"), b"new\n").unwrap();
        std::fs::write(path.join(".env"), b"SECRET=1\n").unwrap();

        // A token that was never issued.
        let e = svc
            .discard(&WorktreeDiscardReq {
                uid: uid.clone(),
                confirm_token: gilde_code_lib::contract::ids::ConfirmToken::mint(),
                accepted_losses: vec![],
            })
            .await
            .unwrap_err();
        assert_err(&e, ErrorCode::Conflict, "changed", "changed");

        let preview = svc.discard_preview(&WorktreeDiscardPreviewReq { uid: uid.clone() }).await.unwrap();
        assert!(preview.branch_kept);
        let kinds: Vec<(LossKind, u32)> = preview.losses.iter().map(|l| (l.kind, l.count)).collect();
        assert_eq!(kinds, vec![(LossKind::UncommittedChanges, 1), (LossKind::UntrackedFiles, 2)], "the ignored .env counts");

        let partial = WorktreeDiscardReq {
            uid: uid.clone(),
            confirm_token: preview.confirm_token.clone(),
            accepted_losses: vec!["uncommittedChanges".into()],
        };
        let e = svc.discard(&partial).await.unwrap_err();
        assert_err(&e, ErrorCode::Refused, "reason", "unacknowledgedLoss");
        assert!(path.exists(), "nothing was removed");

        // The tree changes after the preview: the losses are no longer the
        // ones the user saw.
        std::fs::write(path.join("later.txt"), b"x\n").unwrap();
        let all = WorktreeDiscardReq {
            uid: uid.clone(),
            confirm_token: preview.confirm_token.clone(),
            accepted_losses: preview.losses.iter().map(|l| l.id.clone()).collect(),
        };
        let e = svc.discard(&all).await.unwrap_err();
        assert_err(&e, ErrorCode::Conflict, "changed", "changed");
        assert!(path.exists());

        let preview = svc.discard_preview(&WorktreeDiscardPreviewReq { uid: uid.clone() }).await.unwrap();
        let req = WorktreeDiscardReq {
            uid: uid.clone(),
            confirm_token: preview.confirm_token.clone(),
            accepted_losses: preview.losses.iter().map(|l| l.id.clone()).collect(),
        };
        svc.discard(&req).await.unwrap();
        assert!(!path.exists(), "the worktree is gone");
        let e = svc.discard(&req).await.unwrap_err();
        assert!(matches!(e.code, ErrorCode::Conflict | ErrorCode::NotFound), "single use: {e:?}");
        assert!(!git_ok(&w.repo, &["show-ref", "refs/heads/gilde/throwaway"]).is_empty(), "the branch is kept");
        assert_eq!(std::fs::read(w.repo.join("README.md")).unwrap(), b"hello\n", "the main checkout is untouched");
        assert!(!git_ok(&w.repo, &["worktree", "list"]).contains("throwaway"));
    }

    #[tokio::test]
    async fn discard_is_refused_for_live_watched_and_main_checkout_sessions() {
        let w = world();
        let svc = w.service();
        let (live_path, live) = w.worktree(&svc, "live", "inGilde").await;
        let e = svc.discard_preview(&WorktreeDiscardPreviewReq { uid: live }).await.unwrap_err();
        assert_err(&e, ErrorCode::Refused, "reason", "sessionsLive");
        assert!(live_path.exists());

        let (_, watched) = w.worktree(&svc, "watched", "watching").await;
        let e = svc.discard_preview(&WorktreeDiscardPreviewReq { uid: watched }).await.unwrap_err();
        assert_err(&e, ErrorCode::Refused, "reason", "ownerAlive");

        let main = session("claude", &w.repo, None, control("notRunning"));
        let uid = main.uid.clone();
        w.places.sessions.lock().push(main);
        let e = svc.discard_preview(&WorktreeDiscardPreviewReq { uid }).await.unwrap_err();
        assert_err(&e, ErrorCode::Unsupported, "cap", "notAWorktree");
        assert!(w.repo.join("README.md").exists());
    }

    /// Merge refuses a dirty main checkout and a conflicting merge before it
    /// touches anything; a clean merge fast-forwards or makes a merge commit,
    /// and the worktree branch is kept.
    #[tokio::test]
    async fn merge_gates_and_effects() {
        let w = world();
        let svc = w.service();
        let (path, uid) = w.worktree(&svc, "feature", "notRunning").await;
        std::fs::write(path.join("feature.txt"), b"one\n").unwrap();
        git_ok(&path, &["add", "-A"]);
        git_ok(&path, &["commit", "-qm", "feature work"]);

        let preview = svc.merge_preview(&GitMergePreviewReq { uid: uid.clone() }).await.unwrap();
        assert_eq!((preview.effect.kind, preview.effect.commits, preview.effect.files_changed), (MergeKind::FastForward, 1, 1));

        // A dirty main checkout is refused (preview and execution).
        std::fs::write(w.repo.join("README.md"), b"dirty\n").unwrap();
        let e = svc.merge(&GitMergeReq { uid: uid.clone(), confirm_token: preview.confirm_token.clone() }).await.unwrap_err();
        assert_err(&e, ErrorCode::Refused, "reason", "mainDirty");
        let e = svc.merge_preview(&GitMergePreviewReq { uid: uid.clone() }).await.unwrap_err();
        assert_err(&e, ErrorCode::Refused, "reason", "mainDirty");
        git_ok(&w.repo, &["restore", "README.md"]);

        // A conflicting change on main: refused, nothing touched.
        std::fs::write(path.join("README.md"), b"from the worktree\n").unwrap();
        git_ok(&path, &["commit", "-qam", "worktree edit"]);
        std::fs::write(w.repo.join("README.md"), b"from main\n").unwrap();
        git_ok(&w.repo, &["commit", "-qam", "main edit"]);
        let head_before = git_ok(&w.repo, &["rev-parse", "HEAD"]);
        let e = svc.merge_preview(&GitMergePreviewReq { uid: uid.clone() }).await.unwrap_err();
        assert_err(&e, ErrorCode::Refused, "reason", "conflicts");
        assert_eq!(git_ok(&w.repo, &["rev-parse", "HEAD"]), head_before);
        assert!(!w.repo.join(".git/MERGE_HEAD").exists());
        assert_eq!(git_ok(&w.repo, &["status", "--porcelain"]), "");

        // Resolve it in the worktree; the divergent history merges with a commit.
        std::fs::write(path.join("README.md"), b"from main\n").unwrap();
        git_ok(&path, &["commit", "-qam", "align with main"]);
        let preview = svc.merge_preview(&GitMergePreviewReq { uid: uid.clone() }).await.unwrap();
        assert_eq!(preview.effect.kind, MergeKind::MergeCommit);
        let merged = svc.merge(&GitMergeReq { uid: uid.clone(), confirm_token: preview.confirm_token.clone() }).await.unwrap();
        assert_eq!(merged.merge_commit, git_ok(&w.repo, &["rev-parse", "HEAD"]));
        let parents = git_ok(&w.repo, &["rev-list", "--parents", "-n1", "HEAD"]);
        assert_eq!(parents.split_whitespace().count(), 3, "a merge commit: {parents}");
        assert!(w.repo.join("feature.txt").exists());
        assert!(!git_ok(&w.repo, &["show-ref", "refs/heads/gilde/feature"]).is_empty(), "branch kept");
        // The token was single use.
        let e = svc.merge(&GitMergeReq { uid: uid.clone(), confirm_token: preview.confirm_token }).await.unwrap_err();
        assert_err(&e, ErrorCode::Conflict, "changed", "changed");
        // Nothing left to merge.
        let preview = svc.merge_preview(&GitMergePreviewReq { uid }).await.unwrap();
        assert_eq!(preview.effect.kind, MergeKind::UpToDate);
    }

    /// An LP-40 worktree lives inside its main checkout: its sessions are
    /// never sessions of the main checkout (no prefix matching).
    #[tokio::test]
    async fn nested_worktree_sessions_are_not_the_main_checkouts() {
        use gilde_code_lib::contract::ipc::changes::ChangesGetReq;
        let w = world();
        let on = w.service().with_compat(Arc::new(Lp40(true)));
        let facts = on.create_worktree(&w.project, "nested", None).await.unwrap();
        let nested = PathBuf::from(facts.location_path.as_str());
        assert!(nested.starts_with(&w.repo));
        // A live session in the nested worktree known only by its cwd.
        w.places.sessions.lock().push(session("claude", &nested, None, control("inGilde")));
        let main = session("claude", &w.repo, None, control("notRunning"));
        let uid = main.uid.clone();
        w.places.sessions.lock().push(main);
        let view = on.changes(&ChangesGetReq { uid }).await.unwrap();
        assert!(view.shared_folder.is_none(), "{:?}", view.shared_folder);
        assert!(view.files.is_empty(), "the nested worktree is excluded from the main checkout: {:?}", view.files);
    }

    /// A17.4 fix (b): a worktree whose HEAD is unborn has nothing to merge —
    /// it previews as up to date, and the "merge" changes nothing.
    #[tokio::test]
    async fn an_unborn_worktree_head_previews_as_up_to_date() {
        let w = world();
        let svc = w.service();
        let (path, uid) = w.worktree(&svc, "unborn", "notRunning").await;
        git_ok(&path, &["symbolic-ref", "HEAD", "refs/heads/gilde/never-committed"]);
        let preview = svc.merge_preview(&GitMergePreviewReq { uid: uid.clone() }).await.unwrap();
        assert_eq!((preview.effect.kind, preview.effect.commits, preview.effect.files_changed), (MergeKind::UpToDate, 0, 0));
        let head = git_ok(&w.repo, &["rev-parse", "HEAD"]);
        let merged = svc.merge(&GitMergeReq { uid: uid.clone(), confirm_token: preview.confirm_token }).await.unwrap();
        assert_eq!(merged.merge_commit, head, "nothing merged");
        assert_eq!(git_ok(&w.repo, &["rev-parse", "HEAD"]), head);
        // The Changes view still answers (no base, merge hidden).
        let view = svc.changes(&gilde_code_lib::contract::ipc::changes::ChangesGetReq { uid }).await.unwrap();
        assert_eq!(view.actions.merge, gilde_code_lib::contract::ipc::changes::ActionState::Hidden);
    }

    /// A17.4 fix (c): git failing inside `create_worktree` is
    /// `REFUSED{worktreeGate}` with git's sanitized stderr as the detail.
    #[tokio::test]
    async fn a_git_failure_while_creating_a_worktree_is_a_worktree_gate() {
        let w = world();
        // The sibling root is a regular file: `git worktree add` cannot create
        // the worktree under it.
        std::fs::write(w.home.join("repo-wt"), b"not a directory
").unwrap();
        let e = w.service().create_worktree(&w.project, "blocked", None).await.unwrap_err();
        assert_err(&e, ErrorCode::Refused, "reason", "worktreeGate");
        let detail = e.detail.expect("git's stderr is the detail");
        assert!(!detail.is_empty() && !detail.contains('\u{1b}'), "{detail:?}");
        assert!(detail.to_lowercase().contains("fatal") || detail.to_lowercase().contains("could not"), "{detail}");
        assert!(git_ok(&w.repo, &["branch", "--list", "gilde/blocked"]).is_empty(), "no branch was left behind");
    }

    /// A17.7: the locations `create_worktree` makes are Gilde-created by the
    /// one helper, in both layouts; the main checkout is not.
    #[tokio::test]
    async fn created_worktrees_are_gilde_created_by_the_one_rule() {
        use gilde_code_lib::worktree::naming::is_gilde_created;
        let w = world();
        for svc in [w.service(), w.service().with_compat(Arc::new(Lp40(true)))] {
            let slug = if svc.layout() == gilde_code_lib::worktree::naming::WorktreeLayout::InRepo { "in-repo" } else { "sibling" };
            let facts = svc.create_worktree(&w.project, slug, None).await.unwrap();
            assert!(
                is_gilde_created(Path::new(facts.project_root.as_str()), Path::new(facts.location_path.as_str()), facts.branch.as_deref()),
                "{facts:?}"
            );
        }
        assert!(!is_gilde_created(&w.repo, &w.repo, Some("main")));
        // A worktree someone else made in the sibling root, on their own branch.
        let other = w.home.join("repo-wt/theirs");
        git_ok(&w.repo, &["worktree", "add", "--no-track", "-b", "feature/theirs", "--", other.to_str().unwrap(), "HEAD"]);
        let other = std::fs::canonicalize(other).unwrap();
        assert!(!is_gilde_created(&w.repo, &other, Some("feature/theirs")));
    }

    // -----------------------------------------------------------------------
    // A36 / A37: discard never loses what it did not list (G7-04…G7-10,
    // ported from the reviewer's evidence, g2/g7b 798762c and f262d13)
    // -----------------------------------------------------------------------

    use gilde_code_lib::contract::ipc::changes::WorktreeDiscardPreviewRes;

    /// `git` with a status, for steps that are expected to stop (a conflict).
    pub(super) fn git_try(cwd: &Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new("git").arg("-C").arg(cwd).args(args).env("GIT_TERMINAL_PROMPT", "0").output().expect("git runs")
    }

    fn kinds(p: &WorktreeDiscardPreviewRes) -> Vec<(LossKind, u32)> {
        p.losses.iter().map(|l| (l.kind, l.count)).collect()
    }

    async fn preview(svc: &GitService, uid: &SessionUid) -> Result<WorktreeDiscardPreviewRes, IpcError> {
        svc.discard_preview(&WorktreeDiscardPreviewReq { uid: uid.clone() }).await
    }

    pub(super) async fn discard_all(svc: &GitService, uid: &SessionUid) -> Result<(), IpcError> {
        let pv = preview(svc, uid).await?;
        svc.discard(&WorktreeDiscardReq {
            uid: uid.clone(),
            confirm_token: pv.confirm_token.clone(),
            accepted_losses: pv.losses.iter().map(|l| l.id.clone()).collect(),
        })
        .await
    }

    /// `sha` is reachable from some ref of `repo` (branches, tags, stash).
    fn reachable(repo: &Path, sha: &str) -> bool {
        !git_ok(repo, &["for-each-ref", "--contains", sha]).is_empty()
    }

    fn object_exists(repo: &Path, sha: &str) -> bool {
        git_try(repo, &["cat-file", "-e", sha]).status.success()
    }

    /// The `locked` value git reports for the worktree at `path` (`Some("")`
    /// for a lock without a reason).
    pub(super) fn lock_of(repo: &Path, path: &Path) -> Option<String> {
        let want = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        git::parse_worktree_list(&git_ok(repo, &["worktree", "list", "--porcelain", "-z"]).split('\0').map(str::to_string).collect::<Vec<_>>())
            .into_iter()
            .find(|e| std::fs::canonicalize(&e.path).unwrap_or_else(|_| e.path.clone()) == want)
            .and_then(|e| e.locked)
    }

    /// Commit one file with an explicit identity (a nested repository or a
    /// submodule has no user config under the hermetic HOME, A38.1).
    fn commit_file(dir: &Path, name: &str, body: &str, message: &str) -> String {
        std::fs::write(dir.join(name), body).unwrap();
        git_ok(dir, &["add", "-A"]);
        git_ok(dir, &["-c", "user.email=test@example.com", "-c", "user.name=Test", "commit", "-qm", message]);
        git_ok(dir, &["rev-parse", "HEAD"])
    }

    pub(super) fn assert_refused(e: &IpcError, reason: &str) {
        assert_eq!((e.code, e.params["reason"].as_str()), (ErrorCode::Refused, Some(reason)), "{e:?}");
    }

    /// A36.1 / G7-04a: commits on a detached HEAD that no other ref keeps are
    /// a listed loss with their own acknowledgement — 1 and 3 commits.
    #[tokio::test]
    async fn a36_1_commits_on_no_branch_are_a_listed_loss() {
        for n in [1u32, 3] {
            let w = world();
            let svc = w.service();
            let (path, uid) = w.worktree(&svc, "det", "notRunning").await;
            git_ok(&path, &["switch", "-q", "--detach"]);
            let mut last = String::new();
            for i in 0..n {
                last = commit_file(&path, &format!("work{i}.txt"), "precious\n", "work on a detached head");
            }
            let pv = preview(&svc, &uid).await.unwrap();
            assert_eq!(kinds(&pv), vec![(LossKind::UnmergedCommits, n)], "n = {n}");
            assert_eq!(pv.losses[0].id, "unmergedCommits");

            // Not acknowledged: refused, nothing removed, the token kept.
            let e = svc
                .discard(&WorktreeDiscardReq { uid: uid.clone(), confirm_token: pv.confirm_token.clone(), accepted_losses: vec![] })
                .await
                .unwrap_err();
            assert_refused(&e, "unacknowledgedLoss");
            assert!(path.exists() && object_exists(&w.repo, &last));

            // Acknowledged with the same token: git removes it, the branch stays.
            svc.discard(&WorktreeDiscardReq {
                uid: uid.clone(),
                confirm_token: pv.confirm_token.clone(),
                accepted_losses: vec!["unmergedCommits".into()],
            })
            .await
            .unwrap();
            assert!(!path.exists());
            assert!(!git_ok(&w.repo, &["show-ref", "refs/heads/gilde/det"]).is_empty(), "the branch is kept");
        }
    }

    /// A36.1: what another ref keeps is not a loss — a detached HEAD at the
    /// branch tip, the branch's own commits, a tagged commit, a stashed base.
    #[tokio::test]
    async fn a36_1_commits_another_ref_keeps_are_not_a_loss() {
        let w = world();
        let svc = w.service();

        // Detached at the kept branch's tip: nothing on no branch.
        let (path, uid) = w.worktree(&svc, "tip", "notRunning").await;
        commit_file(&path, "on-branch.txt", "x\n", "on the branch");
        git_ok(&path, &["switch", "-q", "--detach"]);
        assert_eq!(kinds(&preview(&svc, &uid).await.unwrap()), vec![]);
        discard_all(&svc, &uid).await.unwrap();

        // The branch's own commits (attached HEAD) are kept by the branch.
        let (path, uid) = w.worktree(&svc, "own", "notRunning").await;
        for i in 0..3 {
            commit_file(&path, &format!("c{i}.txt"), "x\n", "branch work");
        }
        assert_eq!(kinds(&preview(&svc, &uid).await.unwrap()), vec![]);

        // A detached commit that a tag keeps, and one only the stash keeps.
        let (path, uid) = w.worktree(&svc, "tagged", "notRunning").await;
        git_ok(&path, &["switch", "-q", "--detach"]);
        let tagged = commit_file(&path, "t.txt", "x\n", "tagged later");
        git_ok(&path, &["tag", "keep-this"]);
        assert!(reachable(&w.repo, &tagged));
        assert_eq!(kinds(&preview(&svc, &uid).await.unwrap()), vec![]);
        let (path, uid) = w.worktree(&svc, "stashed", "notRunning").await;
        git_ok(&path, &["switch", "-q", "--detach"]);
        commit_file(&path, "s.txt", "x\n", "stash base");
        std::fs::write(path.join("s.txt"), b"changed\n").unwrap();
        git_ok(&path, &["stash", "-q"]);
        assert_eq!(kinds(&preview(&svc, &uid).await.unwrap()), vec![], "the stash entry's parent is kept by refs/stash");
    }

    /// A36.2 / G7-04b: an operation in progress refuses — an interactive
    /// rebase stopped at `edit` (its amended commit on no branch), a merge and
    /// a cherry-pick stopped at a conflict — from the preview and from a
    /// discard holding an earlier token.
    #[tokio::test]
    async fn a36_2_an_operation_in_progress_refuses() {
        use std::os::unix::fs::PermissionsExt as _;
        let w = world();
        let svc = w.service();

        for op in ["rebase", "merge", "cherryPick"] {
            let slug = format!("op-{}", op.to_lowercase());
            let (path, uid) = w.worktree(&svc, &slug, "notRunning").await;
            let base = git_ok(&path, &["rev-parse", "HEAD"]);
            commit_file(&path, "README.md", "from the worktree\n", "worktree edit");
            // main moves on with a conflicting edit after the worktree forked.
            let main_sha = commit_file(&w.repo, "README.md", &format!("from main, {op}\n"), "main edit");
            let early = preview(&svc, &uid).await.unwrap();
            match op {
                "rebase" => {
                    let editor = w.home.join("seq-editor.sh");
                    std::fs::write(&editor, "#!/bin/sh\n/usr/bin/sed -i '' 's/^pick/edit/' \"$1\"\n").unwrap();
                    std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o755)).unwrap();
                    let out = std::process::Command::new("git")
                        .arg("-C")
                        .arg(&path)
                        .args(["rebase", "-i", &base])
                        .env("GIT_SEQUENCE_EDITOR", &editor)
                        .output()
                        .unwrap();
                    assert!(out.status.success(), "rebase stops at edit: {}", String::from_utf8_lossy(&out.stderr));
                    std::fs::write(path.join("README.md"), b"reworked for an hour\n").unwrap();
                    git_ok(&path, &["commit", "-qa", "--amend", "-m", "reworked"]);
                }
                "merge" => assert!(!git_try(&path, &["merge", "-q", "--no-edit", "main"]).status.success(), "a conflict"),
                _ => assert!(!git_try(&path, &["cherry-pick", &main_sha]).status.success(), "a conflict"),
            }
            let e = preview(&svc, &uid).await.unwrap_err();
            assert_refused(&e, "operationInProgress");
            assert_eq!(e.key, "changes.refused.operationInProgress");
            let e = svc
                .discard(&WorktreeDiscardReq {
                    uid: uid.clone(),
                    confirm_token: early.confirm_token.clone(),
                    accepted_losses: early.losses.iter().map(|l| l.id.clone()).collect(),
                })
                .await
                .unwrap_err();
            assert_refused(&e, "operationInProgress");
            assert!(path.join(".git").exists(), "{op}: the worktree is untouched");
        }
    }

    /// A36.3 / G7-05a: a worktree nested inside the target — with a live
    /// session in it (sessionsLive, also when World knows it only by its cwd)
    /// and without one (nestedRepository). The nested work survives.
    #[tokio::test]
    async fn a36_3_a_nested_worktree_refuses() {
        let w = world();
        let svc = w.service();
        let (a, uid) = w.worktree(&svc, "outer", "notRunning").await;
        let b = a.join(".claude/worktrees/b");
        git_ok(&w.repo, &["worktree", "add", "-q", "-b", "inner", b.to_str().unwrap()]);
        std::fs::write(b.join("wip.txt"), b"an hour of uncommitted work\n").unwrap();

        // Live, by location id.
        let b_loc = LocationId::derive(b.to_str().unwrap());
        w.places.sessions.lock().push(session("claude", &b, Some(&b_loc), control("inGilde")));
        let e = preview(&svc, &uid).await.unwrap_err();
        assert_refused(&e, "sessionsLive");
        assert!(discard_all(&svc, &uid).await.is_err());
        // Live, known only by its cwd (no location id).
        w.places.sessions.lock().clear();
        let me = session("claude", &a, None, control("notRunning"));
        let me_uid = me.uid.clone();
        w.places.sessions.lock().push(me);
        w.places.sessions.lock().push(session("claude", &b.join("src"), None, control("inGilde")));
        assert_refused(&preview(&svc, &me_uid).await.unwrap_err(), "sessionsLive");
        // Not live: the nested worktree itself refuses.
        w.places.sessions.lock().retain(|s| s.uid == me_uid);
        let e = preview(&svc, &me_uid).await.unwrap_err();
        assert_refused(&e, "nestedRepository");
        assert_eq!(e.key, "changes.refused.nestedRepository");
        assert!(b.join("wip.txt").exists(), "the nested worktree's work survived");
    }

    /// A36.3 / G7-05b, G7-06: a nested repository with local commits and a
    /// submodule (dirty, with an unpushed submodule commit; and clean) are
    /// refusals, not "untracked files"; nothing is deleted and Gilde's lock
    /// stays.
    #[tokio::test]
    async fn a36_3_nested_repositories_and_submodules_refuse() {
        let w = world();
        let svc = w.service();
        let (a, uid) = w.worktree(&svc, "outer2", "notRunning").await;
        let nested = a.join("vendor/tool");
        std::fs::create_dir_all(&nested).unwrap();
        git_ok(&nested, &["init", "-q", "-b", "main", "."]);
        let mut last = String::new();
        for i in 0..3 {
            last = commit_file(&nested, &format!("f{i}.txt"), "x\n", "local only");
        }
        assert_refused(&preview(&svc, &uid).await.unwrap_err(), "nestedRepository");
        assert!(object_exists(&nested, &last));

        // Submodules.
        let lib = w.home.join("libsrc");
        std::fs::create_dir_all(&lib).unwrap();
        git_ok(&lib, &["init", "-q", "-b", "main", "."]);
        commit_file(&lib, "lib.txt", "v1\n", "lib v1");
        git_ok(&w.repo, &["-c", "protocol.file.allow=always", "submodule", "add", "-q", lib.to_str().unwrap(), "lib"]);
        git_ok(&w.repo, &["commit", "-qm", "add lib"]);
        for dirty in [true, false] {
            let slug = if dirty { "sub-dirty" } else { "sub-clean" };
            let (path, uid) = w.worktree(&svc, slug, "notRunning").await;
            git_ok(&path, &["-c", "protocol.file.allow=always", "submodule", "update", "-q", "--init"]);
            let mut sub_sha = None;
            if dirty {
                let sub = path.join("lib");
                sub_sha = Some(commit_file(&sub, "lib.txt", "v2, an afternoon of work\n", "lib v2"));
                std::fs::write(path.join("junk.txt"), b"scratch\n").unwrap();
            }
            let locked = lock_of(&w.repo, &path);
            assert_eq!(locked.as_deref(), Some(git::LOCK_REASON));
            assert_refused(&preview(&svc, &uid).await.unwrap_err(), "nestedRepository");
            assert!(discard_all(&svc, &uid).await.is_err());
            assert_eq!(lock_of(&w.repo, &path), locked, "{slug}: Gilde's lock is as it was");
            if let Some(sha) = sub_sha {
                assert!(object_exists(&path.join("lib"), &sha), "the unpushed submodule commit survived");
            }
        }
    }

    /// A36.5 / G7-07: discarding one worktree never prunes another, whether
    /// that one's directory is momentarily missing (an unmounted volume) or
    /// present.
    #[tokio::test]
    async fn a36_5_discard_never_prunes_another_worktree() {
        for missing in [true, false] {
            let w = world();
            let ext = w.home.join("ext");
            std::fs::create_dir_all(&ext).unwrap();
            let user_wt = ext.join("u");
            git_ok(&w.repo, &["worktree", "add", "-q", "--detach", user_wt.to_str().unwrap()]);
            let sha = commit_file(&user_wt, "mine.txt", "user work\n", "user commit on a detached worktree");
            let away = w.home.join("ext-away");
            if missing {
                std::fs::rename(&ext, &away).unwrap();
            }
            let svc = w.service();
            let (path, uid) = w.worktree(&svc, "dirty", "notRunning").await;
            std::fs::write(path.join("scratch.txt"), b"x\n").unwrap();
            let pv = preview(&svc, &uid).await.unwrap();
            assert_eq!(kinds(&pv), vec![(LossKind::UntrackedFiles, 1)], "the other worktree's HEAD keeps its commit, so it is no loss here");
            discard_all(&svc, &uid).await.unwrap();
            if missing {
                std::fs::rename(&away, &ext).unwrap();
            }
            assert!(git_try(&user_wt, &["status", "--porcelain"]).status.success(), "missing = {missing}: the other worktree is intact");
            assert_eq!(git_ok(&user_wt, &["rev-parse", "HEAD"]), sha);
        }
    }

    /// A37.3 / G7-10: discard acts only on worktrees Gilde created, and never
    /// through a lock Gilde did not set — the user's worktree on their own
    /// branch (locked by them), one on a `gilde/` branch outside Gilde's
    /// roots, and a Gilde-created one the user locked with their own reason.
    #[tokio::test]
    async fn a37_3_discard_acts_only_on_gilde_worktrees_and_gilde_locks() {
        let w = world();
        let svc = w.service();
        let place = |path: &Path, branch: &str| {
            let id = gilde_code_lib::discovery::location::identify(path, &w.home).expect("identity");
            w.places.locations.lock().push(Location {
                id: id.location_id.clone(),
                project_id: w.project.clone(),
                path: AbsPath::parse(path.to_str().unwrap()).unwrap(),
                kind: LocationKind::Worktree,
                branch: Some(branch.into()),
                worktree_name: None,
                gilde_created: false,
                exists: true,
            });
            let s = session("claude", path, Some(&id.location_id), control("notRunning"));
            let uid = s.uid.clone();
            w.places.sessions.lock().push(s);
            uid
        };
        for (dir, branch) in [("my-own-wt", "my-feature"), ("elsewhere/gilde-branch", "gilde/elsewhere")] {
            let mine = w.home.join(dir);
            git_ok(&w.repo, &["worktree", "add", "-q", "-b", branch, mine.to_str().unwrap()]);
            git_ok(&w.repo, &["worktree", "lock", "--reason", "on the usb disk, keep", mine.to_str().unwrap()]);
            let uid = place(&std::fs::canonicalize(&mine).unwrap(), branch);
            let e = preview(&svc, &uid).await.unwrap_err();
            assert_refused(&e, "notGildeCreated");
            assert_eq!(e.key, "changes.refused.notGildeCreated");
            assert!(discard_all(&svc, &uid).await.is_err());
            assert!(mine.exists());
            assert_eq!(lock_of(&w.repo, &mine).as_deref(), Some("on the usb disk, keep"), "never unlocked");
        }
        // Gilde made it; the user locked it with their own reason.
        let (path, uid) = w.worktree(&svc, "user-locked", "notRunning").await;
        git_ok(&w.repo, &["worktree", "unlock", path.to_str().unwrap()]);
        git_ok(&w.repo, &["worktree", "lock", "--reason", "mine", path.to_str().unwrap()]);
        let e = preview(&svc, &uid).await.unwrap_err();
        assert_refused(&e, "lockedByUser");
        assert_eq!(e.key, "changes.refused.lockedByUser");
        // A lock without any reason is not Gilde's either.
        git_ok(&w.repo, &["worktree", "unlock", path.to_str().unwrap()]);
        git_ok(&w.repo, &["worktree", "lock", path.to_str().unwrap()]);
        assert_refused(&preview(&svc, &uid).await.unwrap_err(), "lockedByUser");
        assert!(path.exists());
        assert_eq!(lock_of(&w.repo, &path).as_deref(), Some(""));
    }

    /// The control: a plain dirty Gilde worktree still discards with its
    /// acknowledged losses through git; Gilde's lock goes with it, the admin
    /// entry too, and the branch is kept. An unlocked one discards as well.
    #[tokio::test]
    async fn a36_control_a_dirty_gilde_worktree_discards_through_git() {
        let w = world();
        let svc = w.service();
        for locked in [true, false] {
            let slug = if locked { "plain-locked" } else { "plain-unlocked" };
            let (path, uid) = w.worktree(&svc, slug, "notRunning").await;
            if !locked {
                git_ok(&w.repo, &["worktree", "unlock", path.to_str().unwrap()]);
            }
            std::fs::write(path.join("README.md"), b"changed\n").unwrap();
            std::fs::write(path.join("new.txt"), b"new\n").unwrap();
            std::fs::write(path.join(".env"), b"SECRET=1\n").unwrap();
            let pv = preview(&svc, &uid).await.unwrap();
            assert_eq!(kinds(&pv), vec![(LossKind::UncommittedChanges, 1), (LossKind::UntrackedFiles, 2)]);
            discard_all(&svc, &uid).await.unwrap();
            assert!(!path.exists(), "{slug}");
            assert!(lock_of(&w.repo, &path).is_none());
            assert!(!git_ok(&w.repo, &["worktree", "list"]).contains(slug), "git's admin entry went with it");
            assert!(!git_ok(&w.repo, &["show-ref", &format!("refs/heads/gilde/{slug}")]).is_empty(), "the branch is kept");
        }
        assert_eq!(std::fs::read(w.repo.join("README.md")).unwrap(), b"hello\n", "the main checkout is untouched");
    }

    /// A37.5: the discard digest covers the content of already-dirty files
    /// and HEAD's symbolic ref (branch vs detached at the same sha).
    #[tokio::test]
    async fn a37_5_the_discard_digest_sees_dirty_content_and_a_detach() {
        let w = world();
        let svc = w.service();
        let (path, uid) = w.worktree(&svc, "digest", "notRunning").await;
        std::fs::write(path.join("README.md"), b"changed\n").unwrap();
        let pv = preview(&svc, &uid).await.unwrap();
        // Same status letters, different content.
        std::fs::write(path.join("README.md"), b"changed again, much longer than before\n").unwrap();
        let req = |pv: &WorktreeDiscardPreviewRes| WorktreeDiscardReq {
            uid: uid.clone(),
            confirm_token: pv.confirm_token.clone(),
            accepted_losses: pv.losses.iter().map(|l| l.id.clone()).collect(),
        };
        let e = svc.discard(&req(&pv)).await.unwrap_err();
        assert_eq!((e.code, e.params["changed"].as_str()), (ErrorCode::Conflict, Some("changed")));
        assert!(path.exists());
        // Detached at the same sha after the preview.
        let pv = preview(&svc, &uid).await.unwrap();
        git_ok(&path, &["switch", "-q", "--detach"]);
        let e = svc.discard(&req(&pv)).await.unwrap_err();
        assert_eq!(e.code, ErrorCode::Conflict);
        assert!(path.exists());
    }

    /// A37.4: a confirm token is taken atomically — of two concurrent
    /// executions with the same token exactly one runs (discard and merge).
    #[tokio::test]
    async fn a37_4_two_concurrent_executions_with_one_token_run_once() {
        let w = world();
        let svc = w.service();
        let (path, uid) = w.worktree(&svc, "race", "notRunning").await;
        std::fs::write(path.join("new.txt"), b"x\n").unwrap();
        let pv = preview(&svc, &uid).await.unwrap();
        let req = WorktreeDiscardReq {
            uid: uid.clone(),
            confirm_token: pv.confirm_token.clone(),
            accepted_losses: pv.losses.iter().map(|l| l.id.clone()).collect(),
        };
        let (a, b) = tokio::join!(svc.discard(&req), svc.discard(&req));
        assert_eq!([a.is_ok(), b.is_ok()].iter().filter(|ok| **ok).count(), 1, "{a:?} {b:?}");
        let lost = match (a, b) {
            (Ok(()), Err(e)) | (Err(e), Ok(())) => e,
            other => panic!("exactly one runs: {other:?}"),
        };
        assert_eq!((lost.code, lost.params["changed"].as_str()), (ErrorCode::Conflict, Some("changed")));

        let (path, uid) = w.worktree(&svc, "merge-race", "notRunning").await;
        commit_file(&path, "feature.txt", "one\n", "feature");
        let pv = svc.merge_preview(&GitMergePreviewReq { uid: uid.clone() }).await.unwrap();
        let req = GitMergeReq { uid: uid.clone(), confirm_token: pv.confirm_token };
        let (a, b) = tokio::join!(svc.merge(&req), svc.merge(&req));
        assert_eq!([a.is_ok(), b.is_ok()].iter().filter(|ok| **ok).count(), 1, "{a:?} {b:?}");
    }

    /// A37.1 / G7-08: the Commit sheet is offered only what can be committed —
    /// a deletion already committed on the branch is listed by the Changes
    /// view but never passed to git; with everything committed, Commit hides.
    #[tokio::test]
    async fn a37_1_commit_offers_only_uncommitted_rows() {
        use gilde_code_lib::contract::ipc::changes::{ActionState, ChangesGetReq, GitCommitPreviewReq, GitCommitReq};
        let w = world();
        let svc = w.service();
        let (path, uid) = w.worktree(&svc, "commitrows", "notRunning").await;
        git_ok(&path, &["rm", "-q", "README.md"]);
        git_ok(&path, &["commit", "-qm", "drop readme"]);
        std::fs::write(path.join("fresh.txt"), b"new\n").unwrap();
        let view = svc.changes(&ChangesGetReq { uid: uid.clone() }).await.unwrap();
        let paths: Vec<String> = view.files.iter().map(|f| f.path_rel.clone()).collect();
        assert_eq!(paths, vec!["README.md".to_string(), "fresh.txt".to_string()], "the view still shows the branch's work");
        assert_eq!(view.actions.commit, ActionState::Enabled);
        let pv = svc.commit_preview(&GitCommitPreviewReq { uid: uid.clone(), paths: paths.clone() }).await.unwrap();
        let offered: Vec<&str> = pv.files.iter().map(|f| f.path_rel.as_str()).collect();
        assert_eq!(offered, vec!["fresh.txt"], "the committed deletion is not offered");
        // The sheet commits what it was offered.
        let done = svc
            .commit(&GitCommitReq { uid: uid.clone(), message: "commit what Gilde offered".into(), paths: vec!["fresh.txt".into()], confirm_token: pv.confirm_token.clone() })
            .await
            .unwrap();
        assert_eq!(done.commit, git_ok(&path, &["rev-parse", "HEAD"]));
        // A row the preview did not offer is refused without using the token.
        let pv2 = {
            std::fs::write(path.join("more.txt"), b"more\n").unwrap();
            svc.commit_preview(&GitCommitPreviewReq { uid: uid.clone(), paths: vec!["README.md".into(), "more.txt".into()] }).await.unwrap()
        };
        let e = svc
            .commit(&GitCommitReq { uid: uid.clone(), message: "m".into(), paths: vec!["README.md".into()], confirm_token: pv2.confirm_token.clone() })
            .await
            .unwrap_err();
        assert_eq!((e.code, e.params["rule"].as_str()), (ErrorCode::InvalidArgument, Some("path")));
        svc.commit(&GitCommitReq { uid: uid.clone(), message: "m".into(), paths: vec!["more.txt".into()], confirm_token: pv2.confirm_token })
            .await
            .unwrap();

        // Everything committed: Commit hides and nothing is offered.
        let (path2, uid2) = w.worktree(&svc, "allcommitted", "notRunning").await;
        commit_file(&path2, "done.txt", "done\n", "done");
        let view2 = svc.changes(&ChangesGetReq { uid: uid2.clone() }).await.unwrap();
        assert_eq!(view2.files.len(), 1);
        assert_eq!(view2.actions.commit, ActionState::Hidden);
        let paths2: Vec<String> = view2.files.iter().map(|f| f.path_rel.clone()).collect();
        let pv = svc.commit_preview(&GitCommitPreviewReq { uid: uid2, paths: paths2 }).await.unwrap();
        assert!(pv.files.is_empty());
    }

    /// A37.5: the commit digest covers HEAD and the branch — a commit made
    /// after the preview, or a detach, is a change.
    #[tokio::test]
    async fn a37_5_the_commit_digest_sees_head_and_branch() {
        use gilde_code_lib::contract::ipc::changes::{GitCommitPreviewReq, GitCommitReq};
        let w = world();
        let svc = w.service();
        let (path, uid) = w.worktree(&svc, "headmove", "notRunning").await;
        std::fs::write(path.join("a.txt"), b"a\n").unwrap();
        std::fs::write(path.join("b.txt"), b"b\n").unwrap();
        for step in ["commit", "detach"] {
            let pv = svc.commit_preview(&GitCommitPreviewReq { uid: uid.clone(), paths: vec!["a.txt".into()] }).await.unwrap();
            if step == "commit" {
                git_ok(&path, &["add", "b.txt"]);
                git_ok(&path, &["commit", "-qm", "the agent committed b"]);
            } else {
                git_ok(&path, &["switch", "-q", "--detach"]);
            }
            let head = git_ok(&path, &["rev-parse", "HEAD"]);
            let e = svc
                .commit(&GitCommitReq { uid: uid.clone(), message: "a".into(), paths: vec!["a.txt".into()], confirm_token: pv.confirm_token })
                .await
                .unwrap_err();
            assert_eq!(e.code, ErrorCode::Conflict, "{step}");
            assert_eq!(git_ok(&path, &["rev-parse", "HEAD"]), head, "{step}: nothing committed");
        }
    }

    /// A37.2 / G7-09: a git write that runs out of time is stopped with
    /// SIGTERM, so git removes its own `index.lock` (a slow `pre-commit`
    /// hook, and a slow `pre-merge-commit` hook in the main checkout); the
    /// repository stays usable.
    #[tokio::test]
    async fn a37_2_a_git_timeout_never_leaves_index_lock() {
        use gilde_code_lib::worktree::git::{exec, ExecOpts, GitError};
        use std::os::unix::fs::PermissionsExt as _;
        let w = world();
        for (hook, args) in [
            ("pre-commit", vec!["commit", "--only", "-F", "-", "--", "f.txt"]),
            ("pre-merge-commit", vec!["merge", "--no-ff", "--no-edit", "-m", "m", "side"]),
        ] {
            let hook_path = w.repo.join(".git/hooks").join(hook);
            std::fs::write(&hook_path, "#!/bin/sh\n/bin/sleep 30\n").unwrap();
            std::fs::set_permissions(&hook_path, std::fs::Permissions::from_mode(0o755)).unwrap();
            if hook == "pre-commit" {
                std::fs::write(w.repo.join("f.txt"), b"x\n").unwrap();
                git_ok(&w.repo, &["add", "f.txt"]);
            } else {
                // main and `side` both move on (different files): a merge commit.
                git_ok(&w.repo, &["commit", "-qm", "f"]);
                let side = w.home.join("side");
                git_ok(&w.repo, &["worktree", "add", "-q", "-b", "side", side.to_str().unwrap(), "HEAD~1"]);
                commit_file(&side, "side.txt", "side\n", "side work");
            }
            let res = exec(
                &w.repo,
                &args,
                ExecOpts { timeout: std::time::Duration::from_secs(1), stdin: Some(b"m"), ..ExecOpts::default() },
            )
            .await;
            assert!(matches!(res, Err(GitError::Timeout)), "{hook}: {res:?}");
            assert!(!w.repo.join(".git/index.lock").exists(), "{hook}: the timed-out git left .git/index.lock");
            assert!(git_try(&w.repo, &["add", "-A"]).status.success(), "{hook}: the next git works");
            std::fs::remove_file(&hook_path).unwrap();
            let _ = git_try(&w.repo, &["merge", "--abort"]);
        }
    }

    /// A37.2: a lock older than the timed-out run is not Gilde's to remove —
    /// `GIT_FAILED{lockLeft}` naming it, the lock untouched.
    #[tokio::test]
    async fn a37_2_an_older_lock_is_reported_and_left_alone() {
        use gilde_code_lib::worktree::git::{exec, ExecOpts};
        use std::os::unix::fs::PermissionsExt as _;
        let w = world();
        let hook_path = w.repo.join(".git/hooks/pre-commit");
        std::fs::write(&hook_path, "#!/bin/sh\n/bin/sleep 30\n").unwrap();
        std::fs::set_permissions(&hook_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(w.repo.join("f.txt"), b"x\n").unwrap();
        git_ok(&w.repo, &["add", "f.txt"]);
        for name in ["packed-refs.lock", "config.lock"] {
            let stale = w.repo.join(".git").join(name);
            let f = std::fs::File::create(&stale).unwrap();
            f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(3600)).unwrap();
            drop(f);
            let res = exec(
                &w.repo,
                &["commit", "--only", "-F", "-", "--", "f.txt"],
                ExecOpts { timeout: std::time::Duration::from_secs(1), stdin: Some(b"m"), ..ExecOpts::default() },
            )
            .await;
            let e = res.expect_err("timed out").to_ipc();
            assert_eq!((e.code, e.params["reason"].as_str()), (ErrorCode::GitFailed, Some("lockLeft")), "{e:?}");
            assert_eq!(e.key, "changes.gitFailed.lockLeft");
            assert!(e.detail.as_deref().unwrap_or("").ends_with(name), "{e:?}");
            assert!(stale.exists(), "{name}: someone else's lock is never removed");
            assert!(!w.repo.join(".git/index.lock").exists(), "{name}: this run's own lock is gone");
            std::fs::remove_file(&stale).unwrap();
        }
    }

    /// A37.6 / G7-11: Gilde's own git calls never run a program the
    /// repository's config names — `core.fsmonitor` on the Changes view, the
    /// `post-checkout` hook when Gilde creates a worktree — while the user's
    /// own commit keeps the repository's hooks.
    #[tokio::test]
    async fn a37_6_repository_programs_run_only_for_the_users_own_writes() {
        use gilde_code_lib::contract::ipc::changes::{ChangesGetReq, GitCommitPreviewReq, GitCommitReq};
        use std::os::unix::fs::PermissionsExt as _;
        let w = world();
        let marker = |name: &str| w.home.join(format!("{name}-ran"));
        let script = |name: &str| {
            let p = w.home.join(format!("{name}.sh"));
            std::fs::write(&p, format!("#!/bin/sh\necho ran >> '{}'\nexit 0\n", marker(name).display())).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            p
        };
        git_ok(&w.repo, &["config", "core.fsmonitor", script("fsmonitor").to_str().unwrap()]);
        let hooks = w.repo.join(".git/hooks");
        std::fs::copy(script("post-checkout"), hooks.join("post-checkout")).unwrap();
        std::fs::copy(script("pre-commit"), hooks.join("pre-commit")).unwrap();
        let svc = w.service();
        let (path, uid) = w.worktree(&svc, "fsmon", "notRunning").await;
        std::fs::write(path.join("x.txt"), b"x\n").unwrap();
        svc.changes(&ChangesGetReq { uid: uid.clone() }).await.unwrap();
        let _ = preview(&svc, &uid).await.unwrap();
        assert!(!marker("fsmonitor").exists(), "a Changes call ran the repository's fsmonitor");
        assert!(!marker("post-checkout").exists(), "creating a worktree ran the repository's post-checkout hook");
        assert!(!marker("pre-commit").exists());
        // The user's own commit runs the repository's hooks, as their git would.
        let pv = svc.commit_preview(&GitCommitPreviewReq { uid: uid.clone(), paths: vec!["x.txt".into()] }).await.unwrap();
        svc.commit(&GitCommitReq { uid, message: "x".into(), paths: vec!["x.txt".into()], confirm_token: pv.confirm_token }).await.unwrap();
        assert!(marker("pre-commit").exists(), "the user's commit keeps the pre-commit hook");
        assert!(!marker("fsmonitor").exists());
    }

    /// The v1 path (A38.7): a dirty `ArchiveThenRemove` goes through git (the
    /// one forced remove, after a verified archive), and `prune` never prunes
    /// an entry whose directory is missing.
    #[tokio::test]
    async fn v1_archive_removes_through_git_and_prune_reports_only() {
        let fx = super::fixture().await;
        let wt = fx.manager.create(super::create_req(&fx.project_id, "archive-me")).await.unwrap();
        std::fs::write(Path::new(&wt.path).join("wip.txt"), b"wip\n").unwrap();
        let out = fx.manager.remove(&wt.id, gilde_code_lib::worktree::RemoveMode::ArchiveThenRemove).await.unwrap();
        assert!(out.removed && !Path::new(&wt.path).exists());
        assert!(Path::new(out.archive_path.as_deref().unwrap()).join("wip.txt").exists());
        assert!(!git_ok(&fx.repo, &["worktree", "list"]).contains("archive-me"), "git removed its admin entry");

        // A worktree whose directory went away (an unmounted volume).
        let away = fx.manager.create(super::create_req(&fx.project_id, "away")).await.unwrap();
        let moved = PathBuf::from(format!("{}-moved", away.path));
        std::fs::rename(&away.path, &moved).unwrap();
        let report = fx.manager.prune(&fx.project_id).await.unwrap();
        assert!(report.git_only.iter().any(|p| p.contains("away")), "{report:?}");
        std::fs::rename(&moved, &away.path).unwrap();
        assert!(git_try(Path::new(&away.path), &["status", "--porcelain"]).status.success(), "its admin entry survived the prune");
    }

    use gilde_code_lib::worktree::git;
}

// ---------------------------------------------------------------------------
// 5. A36.6: the lock is restored when the removal fails (own process each:
//    the supervisor that forces the failure is bound once per process)
// ---------------------------------------------------------------------------

rusty_fork_test! {
    /// Gilde's lock is released only for the removal and put back when it
    /// fails — the removal refused before git runs (a dirty tree on the forced
    /// path) and git's own clean check refusing (a file appears just before
    /// git runs) — and an unlocked worktree stays unlocked. The worktree is
    /// untouched each time; with nothing in the way the discard goes through.
    #[test]
    fn a36_6_the_lock_is_restored_when_the_removal_fails() {
        runtime().block_on(async {
            use gilde_code_lib::contract::ErrorCode;
            let sup = FakeSupervisor::new(None);
            assert!(git::bind_supervisor(sup.clone()));
            let w = v2::world();
            let svc = w.service();

            // (a) The forced removal of a dirty tree is refused: git never runs.
            let (path, uid) = w.worktree(&svc, "refused", "notRunning").await;
            std::fs::write(path.join("wip.txt"), b"an hour of work\n").unwrap();
            *sup.on_remove.lock() = OnRemove::Refuse(gilde_code_lib::contract::IpcError::not_ready("bootSweep"));
            let e = v2::discard_all(&svc, &uid).await.unwrap_err();
            assert_eq!(e.code, ErrorCode::NotReady, "{e:?}");
            assert_eq!(v2::lock_of(&w.repo, &path).as_deref(), Some(git::LOCK_REASON), "the lock is back");
            assert!(path.join("wip.txt").exists());

            // (b) git refuses on its own: an untracked file appears just before
            // the plain removal of a clean tree.
            let (path, uid) = w.worktree(&svc, "raced", "notRunning").await;
            *sup.on_remove.lock() = OnRemove::Touch(path.join("late.txt"));
            let e = v2::discard_all(&svc, &uid).await.unwrap_err();
            assert_eq!(e.code, ErrorCode::GitFailed, "{e:?}");
            assert_eq!(v2::lock_of(&w.repo, &path).as_deref(), Some(git::LOCK_REASON), "the lock is back");
            assert!(path.join("late.txt").exists() && path.join("README.md").exists(), "git deleted nothing");

            // (c) Not locked before: not locked after.
            let (path2, uid2) = w.worktree(&svc, "unlocked", "notRunning").await;
            git_ok(&w.repo, &["worktree", "unlock", path2.to_str().unwrap()]);
            *sup.on_remove.lock() = OnRemove::Touch(path2.join("late.txt"));
            let e = v2::discard_all(&svc, &uid2).await.unwrap_err();
            assert_eq!(e.code, ErrorCode::GitFailed, "{e:?}");
            assert_eq!(v2::lock_of(&w.repo, &path2), None, "no lock was invented");
            assert!(path2.exists());

            // With nothing in the way, (b) now lists its new file and goes.
            *sup.on_remove.lock() = OnRemove::Nothing;
            v2::discard_all(&svc, &uid).await.unwrap();
            assert!(!path.exists());
            assert!(!git_ok(&w.repo, &["show-ref", "refs/heads/gilde/raced"]).is_empty(), "the branch is kept");
        });
    }
}
