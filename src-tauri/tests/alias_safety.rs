//! T14 — provider path alias layer safety (DESIGN §7-6).
//!
//! The alias layer plants symlinks inside the *user's own repository*. That is
//! the highest-blast-radius thing this app does to a directory it does not own,
//! so every rule below is checked against a real `git` and a real filesystem:
//!
//! * a real directory, a real file, or a foreign symlink at the convention path
//!   is never touched;
//! * a symlink we made is removed only while `read_link(alias) == target` — a
//!   link that was retargeted behind our back survives untouched;
//! * ignore hygiene goes to `.git/info/exclude` and nowhere else, so
//!   `git status --porcelain` is **empty** after we are done;
//! * `worktree_remove` tears our links down before it removes the worktree.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};

use gilde_code_lib::paths::AppPaths;
use gilde_code_lib::store::models::{AliasKind, ProjectRow, WorktreeOrigin};
use gilde_code_lib::store::{dao, now_ms, Db};
use gilde_code_lib::worktree::alias::{
    alias_ensure, exclude_block, remove_owned_aliases, scan_adoptable, AliasLayer, AliasOutcome,
    AliasProvider, RemovalSkip, SkipReason, EXCLUDE_BEGIN,
};
use gilde_code_lib::worktree::{
    naming, CreateWorktreeReq, RemoveMode, Worktree, WorktreeManager,
};

// ---------------------------------------------------------------------------
// harness
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
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}

/// `git status --porcelain` — the assertion that matters most in this file.
fn porcelain(repo: &Path) -> String {
    git_ok(repo, &["status", "--porcelain"])
}

fn init_repo(root: &Path) -> PathBuf {
    std::fs::create_dir_all(root).unwrap();
    git_ok(root, &["init", "-q", "-b", "main", "."]);
    git_ok(root, &["config", "user.email", "test@example.com"]);
    git_ok(root, &["config", "user.name", "Test"]);
    git_ok(root, &["config", "commit.gpgsign", "false"]);
    std::fs::write(root.join("README.md"), b"hello\n").unwrap();
    git_ok(root, &["add", "-A"]);
    git_ok(root, &["commit", "-qm", "init"]);
    std::fs::canonicalize(root).unwrap()
}

/// One process-wide store + one installed alias layer.
///
/// [`gilde_code_lib::worktree::alias::install`] and T13's
/// `install_alias_hook` are both `OnceLock`s — first installation wins — so the
/// database the layer sees has to be the same one every test uses. Each test
/// still gets its own repository, which is what actually needs isolating.
struct Shared {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    db: Db,
    manager: Arc<WorktreeManager>,
    layer: Arc<AliasLayer>,
}

static SHARED: OnceLock<Shared> = OnceLock::new();

fn shared() -> &'static Shared {
    SHARED.get_or_init(|| {
        let tmp = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(tmp.path()).unwrap();
        let paths = Arc::new(AppPaths::for_home(&home));
        paths.ensure_all().unwrap();

        let db = Db::open_in_memory().unwrap();
        // No event sink: these tests read the filesystem and git, not events
        // (and the legacy bus would drag in `model::graph`, A7.9).
        let manager = WorktreeManager::new(db.clone(), paths.clone(), None);
        let layer = gilde_code_lib::worktree::alias::install(db.clone(), Some(paths));

        Shared {
            _tmp: tmp,
            home,
            db,
            manager,
            layer,
        }
    })
}

/// A fresh repository plus its `project` row.
struct Repo {
    path: PathBuf,
    project_id: String,
}

async fn new_repo(name: &str) -> Repo {
    let s = shared();
    let dir = s.home.join("repos").join(name);
    let path = init_repo(&dir);

    let project = ProjectRow {
        id: uuid::Uuid::now_v7().to_string(),
        path: path.display().to_string(),
        canonical_path: path.display().to_string(),
        repo_slug: naming::repo_slug(&path),
        name: name.to_string(),
        color: None,
        default_base_ref: "main".into(),
        added_at: now_ms(),
        last_opened_at: None,
    };
    let project_id = project.id.clone();
    s.db.write(move |conn| dao::project::upsert(conn, &project))
        .await
        .unwrap();

    Repo { path, project_id }
}

async fn make_worktree(repo: &Repo, task: &str) -> Worktree {
    shared()
        .manager
        .create(CreateWorktreeReq {
            project_id: repo.project_id.clone(),
            task_name: task.to_string(),
            ..Default::default()
        })
        .await
        .expect("worktree created")
}

fn claude_alias(repo: &Path, wt: &Worktree) -> PathBuf {
    repo.join(".claude").join("worktrees").join(&wt.slug)
}

fn canonical_target(wt: &Worktree) -> PathBuf {
    std::fs::canonicalize(&wt.path).unwrap()
}

// ---------------------------------------------------------------------------
// 1. the source itself — DESIGN §7-6 rule 4
// ---------------------------------------------------------------------------

/// Split so this file's own text cannot trip the grep it performs.
fn tracked_ignore_name() -> String {
    format!(".{}{}", "git", "ignore")
}

fn rust_sources_under(rel: &str) -> Vec<(PathBuf, String)> {
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
    let mut out = Vec::new();
    walk(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel), &mut out);
    assert!(!out.is_empty(), "no sources found under {rel}");
    out
}

/// DESIGN §7-6 rule 4: a tracked ignore file is never named — let alone written
/// — anywhere in the worktree module. Appending to one would show up as an
/// uncommitted change in the user's repository.
#[test]
fn worktree_sources_never_name_a_tracked_ignore_file() {
    let needle = tracked_ignore_name();
    for (path, text) in rust_sources_under("src/worktree") {
        for (n, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            let is_comment = trimmed.starts_with("//") || trimmed.starts_with("*");
            assert!(
                !line.contains(&needle) || is_comment,
                "{}:{} names a tracked ignore file outside a comment: {line}",
                path.display(),
                n + 1
            );
        }
    }
}

/// The only ignore file this module writes is the untracked one.
#[test]
fn the_only_ignore_target_is_info_exclude() {
    let sources = rust_sources_under("src/worktree");
    let alias = sources
        .iter()
        .find(|(p, _)| p.ends_with("alias.rs"))
        .expect("alias.rs is in the tree");
    assert!(alias.1.contains("\"info\""));
    assert!(alias.1.contains("\"exclude\""));
    assert!(exclude_block().contains("/.claude/worktrees/"));
}

// ---------------------------------------------------------------------------
// 2. convention paths + the clean-repository invariant
// ---------------------------------------------------------------------------

#[tokio::test]
async fn conventions_point_at_the_canonical_central_path() {
    let repo = new_repo("conventions").await;
    let wt = make_worktree(&repo, "Fix Login").await;
    let target = canonical_target(&wt);

    for (provider, rel) in [
        (AliasProvider::ClaudeW, ".claude/worktrees"),
        (AliasProvider::CursorWorktree, ".cursor/worktrees"),
        (AliasProvider::Generic, ".worktrees"),
    ] {
        let outcome = alias_ensure(&wt, &repo.path, provider.clone())
            .await
            .unwrap();
        let mut expected = repo.path.clone();
        for part in rel.split('/') {
            expected.push(part);
        }
        expected.push(&wt.slug);

        match outcome {
            AliasOutcome::Created {
                path,
                target: t,
                not_ignored,
            } => {
                assert_eq!(path, expected, "{} path", provider.key());
                assert_eq!(t, target);
                assert!(
                    !not_ignored,
                    "{} alias must be covered by info/exclude",
                    provider.key()
                );
            }
            other => panic!("expected Created for {}, got {other:?}", provider.key()),
        }
        assert_eq!(std::fs::read_link(&expected).unwrap(), target);
        // Rule 2/3: the alias resolves to the central directory, never a copy.
        assert!(expected.join("README.md").exists());
    }

    // Rule 4, the part that matters: the user's repository is untouched.
    assert_eq!(porcelain(&repo.path), "", "alias creation dirtied the repo");
    assert!(!repo.path.join(tracked_ignore_name()).exists());

    let exclude = repo.path.join(".git").join("info").join("exclude");
    let text = std::fs::read_to_string(&exclude).unwrap();
    assert_eq!(text.matches(EXCLUDE_BEGIN).count(), 1);
    assert!(text.contains("/.claude/worktrees/"));
}

#[tokio::test]
async fn user_ignore_file_is_never_touched() {
    let repo = new_repo("keeps-gitignore").await;
    // A tracked ignore file with content the user cares about.
    let tracked = repo.path.join(tracked_ignore_name());
    std::fs::write(&tracked, b"node_modules/\n").unwrap();
    git_ok(&repo.path, &["add", "-A"]);
    git_ok(&repo.path, &["commit", "-qm", "ignore"]);
    let before = std::fs::read_to_string(&tracked).unwrap();

    let wt = make_worktree(&repo, "leave it alone").await;
    alias_ensure(&wt, &repo.path, AliasProvider::ClaudeW)
        .await
        .unwrap();

    assert_eq!(std::fs::read_to_string(&tracked).unwrap(), before);
    assert_eq!(porcelain(&repo.path), "");
}

#[tokio::test]
async fn ensure_is_idempotent() {
    let repo = new_repo("idempotent").await;
    let wt = make_worktree(&repo, "twice").await;

    let first = alias_ensure(&wt, &repo.path, AliasProvider::ClaudeW)
        .await
        .unwrap();
    assert!(matches!(first, AliasOutcome::Created { .. }));

    let second = alias_ensure(&wt, &repo.path, AliasProvider::ClaudeW)
        .await
        .unwrap();
    let alias = claude_alias(&repo.path, &wt);
    assert_eq!(second, AliasOutcome::AlreadyOurs { path: alias.clone() });
    assert_eq!(std::fs::read_link(&alias).unwrap(), canonical_target(&wt));

    let id = wt.id.clone();
    let rows = shared()
        .db
        .read(move |conn| dao::worktree_alias::list_by_worktree(conn, &id))
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "no duplicate rows: {rows:?}");
    assert_eq!(porcelain(&repo.path), "");

    // DESIGN §7-1: the table is mirrored to disk so a lost `state.db` does not
    // turn our symlinks into litter nobody is allowed to remove.
    let mirror = shared().home.join(".gilde").join("aliases.json");
    let text = std::fs::read_to_string(&mirror).expect("aliases.json mirror");
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(parsed.is_array(), "{text}");
    assert!(
        text.contains(alias.to_str().unwrap()),
        "mirror is missing this alias: {text}"
    );
}

/// A custom link path the managed exclude block does not cover: the alias is
/// still created, but `check-ignore` disagrees and the caller is told so rather
/// than discovering it as a mystery untracked entry later.
#[tokio::test]
async fn an_uncovered_custom_path_reports_not_ignored() {
    let repo = new_repo("not-ignored").await;
    let wt = make_worktree(&repo, "custom spot").await;

    let provider = AliasProvider::Custom {
        link_path: PathBuf::from("aliases/here"),
    };
    let outcome = alias_ensure(&wt, &repo.path, provider).await.unwrap();
    let AliasOutcome::Created {
        path, not_ignored, ..
    } = outcome
    else {
        panic!("expected Created, got {outcome:?}");
    };
    assert_eq!(path, repo.path.join("aliases").join("here"));
    assert!(
        not_ignored,
        "an alias git would show as untracked must be reported"
    );
    assert!(
        !porcelain(&repo.path).is_empty(),
        "the warning has to correspond to reality"
    );

    // And it is still ours to take back down.
    let report = remove_owned_aliases(&wt.id).await.unwrap();
    assert!(report[0].removed);
    assert_eq!(porcelain(&repo.path), "");
}

// ---------------------------------------------------------------------------
// 3. refusals — rule 1
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_existing_real_directory_is_refused() {
    let repo = new_repo("real-dir").await;
    let wt = make_worktree(&repo, "occupied").await;
    let alias = claude_alias(&repo.path, &wt);

    std::fs::create_dir_all(&alias).unwrap();
    std::fs::write(alias.join("someone-elses.txt"), b"do not touch\n").unwrap();

    let outcome = alias_ensure(&wt, &repo.path, AliasProvider::ClaudeW)
        .await
        .unwrap();
    assert_eq!(
        outcome,
        AliasOutcome::Skipped {
            path: alias.clone(),
            reason: SkipReason::ExistingDirectory
        }
    );

    // Untouched, and still a directory rather than a link.
    assert!(alias.symlink_metadata().unwrap().file_type().is_dir());
    assert_eq!(
        std::fs::read_to_string(alias.join("someone-elses.txt")).unwrap(),
        "do not touch\n"
    );

    let id = wt.id.clone();
    let rows = shared()
        .db
        .read(move |conn| dao::worktree_alias::list_by_worktree(conn, &id))
        .await
        .unwrap();
    assert!(rows.is_empty(), "a refusal must not record ownership");
}

#[tokio::test]
async fn an_existing_file_is_refused() {
    let repo = new_repo("real-file").await;
    let wt = make_worktree(&repo, "file there").await;
    let alias = claude_alias(&repo.path, &wt);

    std::fs::create_dir_all(alias.parent().unwrap()).unwrap();
    std::fs::write(&alias, b"not a worktree\n").unwrap();

    let outcome = alias_ensure(&wt, &repo.path, AliasProvider::ClaudeW)
        .await
        .unwrap();
    assert_eq!(
        outcome,
        AliasOutcome::Skipped {
            path: alias.clone(),
            reason: SkipReason::ExistingFile
        }
    );
    assert_eq!(std::fs::read_to_string(&alias).unwrap(), "not a worktree\n");
}

#[tokio::test]
async fn a_foreign_symlink_is_refused() {
    let repo = new_repo("foreign-link").await;
    let wt = make_worktree(&repo, "someone else").await;
    let alias = claude_alias(&repo.path, &wt);

    let decoy = shared().home.join("decoy-foreign");
    std::fs::create_dir_all(&decoy).unwrap();
    std::fs::create_dir_all(alias.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&decoy, &alias).unwrap();

    let outcome = alias_ensure(&wt, &repo.path, AliasProvider::ClaudeW)
        .await
        .unwrap();
    assert_eq!(
        outcome,
        AliasOutcome::Skipped {
            path: alias.clone(),
            reason: SkipReason::ForeignSymlink
        }
    );
    assert_eq!(
        std::fs::read_link(&alias).unwrap(),
        decoy,
        "a foreign symlink must be left exactly as found"
    );

    let removals = remove_owned_aliases(&wt.id).await.unwrap();
    assert!(
        removals.is_empty(),
        "nothing was recorded, so nothing may be removed: {removals:?}"
    );
    assert!(alias.symlink_metadata().is_ok(), "the foreign link survives");
}

// ---------------------------------------------------------------------------
// 4. teardown — rule 5
// ---------------------------------------------------------------------------

#[tokio::test]
async fn our_own_symlink_is_removed() {
    let repo = new_repo("teardown").await;
    let wt = make_worktree(&repo, "ours").await;
    let alias = claude_alias(&repo.path, &wt);

    alias_ensure(&wt, &repo.path, AliasProvider::ClaudeW)
        .await
        .unwrap();
    assert!(alias.symlink_metadata().is_ok());

    let report = remove_owned_aliases(&wt.id).await.unwrap();
    assert_eq!(report.len(), 1);
    assert!(report[0].removed, "{report:?}");
    assert_eq!(report[0].reason, None);

    assert!(
        alias.symlink_metadata().is_err(),
        "the link should be gone"
    );
    // The worktree itself is untouched: the alias was only a pointer.
    assert!(Path::new(&wt.path).join("README.md").exists());

    let id = wt.id.clone();
    let rows = shared()
        .db
        .read(move |conn| dao::worktree_alias::list_by_worktree(conn, &id))
        .await
        .unwrap();
    assert!(rows.is_empty());
    assert_eq!(porcelain(&repo.path), "");
}

/// The case this whole check exists for: the row says the link is ours, but the
/// link now points somewhere else. Removing it would delete someone else's
/// pointer — possibly to data we know nothing about — so we do not.
#[tokio::test]
async fn a_secretly_retargeted_symlink_is_left_alone() {
    let repo = new_repo("retargeted").await;
    let wt = make_worktree(&repo, "moved under us").await;
    let alias = claude_alias(&repo.path, &wt);

    alias_ensure(&wt, &repo.path, AliasProvider::ClaudeW)
        .await
        .unwrap();

    let decoy = shared().home.join("decoy-retarget");
    std::fs::create_dir_all(&decoy).unwrap();
    std::fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink(&decoy, &alias).unwrap();

    let report = remove_owned_aliases(&wt.id).await.unwrap();
    assert_eq!(report.len(), 1);
    assert!(!report[0].removed);
    assert_eq!(report[0].reason, Some(RemovalSkip::TargetMismatch));

    assert_eq!(
        std::fs::read_link(&alias).unwrap(),
        decoy,
        "the retargeted link must survive untouched"
    );
    // Still recorded, so the UI can show it rather than silently forgetting.
    let id = wt.id.clone();
    let rows = shared()
        .db
        .read(move |conn| dao::worktree_alias::list_by_worktree(conn, &id))
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
}

#[tokio::test]
async fn a_replaced_directory_is_left_alone() {
    let repo = new_repo("replaced-dir").await;
    let wt = make_worktree(&repo, "now a dir").await;
    let alias = claude_alias(&repo.path, &wt);

    alias_ensure(&wt, &repo.path, AliasProvider::ClaudeW)
        .await
        .unwrap();
    std::fs::remove_file(&alias).unwrap();
    std::fs::create_dir_all(&alias).unwrap();
    std::fs::write(alias.join("keep.txt"), b"keep\n").unwrap();

    let report = remove_owned_aliases(&wt.id).await.unwrap();
    assert_eq!(report[0].reason, Some(RemovalSkip::NotASymlink));
    assert!(!report[0].removed);
    assert!(alias.join("keep.txt").exists());
}

// ---------------------------------------------------------------------------
// 5. wiring — acceptance 6
// ---------------------------------------------------------------------------

/// `worktree_remove` must tear the aliases down *before* it removes anything,
/// through T13's `AliasHook` seam.
#[tokio::test]
async fn worktree_remove_removes_owned_aliases() {
    let repo = new_repo("remove-wiring").await;
    let wt = shared()
        .manager
        .create(CreateWorktreeReq {
            project_id: repo.project_id.clone(),
            task_name: "wired".into(),
            alias_providers: vec![
                serde_json::json!({ "provider": "claude-w" }),
                serde_json::json!("generic"),
            ],
            ..Default::default()
        })
        .await
        .unwrap();

    let claude = claude_alias(&repo.path, &wt);
    let generic = repo.path.join(".worktrees").join(&wt.slug);
    assert!(claude.symlink_metadata().is_ok(), "created by worktree_create");
    assert!(generic.symlink_metadata().is_ok());
    assert_eq!(porcelain(&repo.path), "");

    let preflight = shared().manager.remove_preflight(&wt.id).await.unwrap();
    assert_eq!(preflight.alias_links.len(), 2, "{preflight:?}");

    let outcome = shared()
        .manager
        .remove(&wt.id, RemoveMode::KeepBranch)
        .await
        .unwrap();
    assert!(outcome.removed, "{outcome:?}");

    assert!(claude.symlink_metadata().is_err(), "alias survived removal");
    assert!(generic.symlink_metadata().is_err());
    assert_eq!(porcelain(&repo.path), "");
}

// ---------------------------------------------------------------------------
// 6. adoption — rule 6 / acceptance 8
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_provider_made_worktree_is_adopted_not_clobbered() {
    let repo = new_repo("adoption").await;
    let wt = make_worktree(&repo, "ours here").await;

    // Pretend `claude -w` already made its own worktree at the convention path.
    let alias = claude_alias(&repo.path, &wt);
    std::fs::create_dir_all(alias.parent().unwrap()).unwrap();
    git_ok(
        &repo.path,
        &[
            "worktree",
            "add",
            "--no-track",
            "-b",
            "provider-made",
            "--",
            alias.to_str().unwrap(),
            "HEAD",
        ],
    );
    assert!(alias.join("README.md").exists());

    let outcome = alias_ensure(&wt, &repo.path, AliasProvider::ClaudeW)
        .await
        .unwrap();
    assert_eq!(outcome, AliasOutcome::Adopted { path: alias.clone() });
    // Still a real directory — we did not replace it with a link.
    assert!(alias.symlink_metadata().unwrap().file_type().is_dir());
    assert!(alias.join("README.md").exists());

    let candidates = scan_adoptable(&repo.path).await.unwrap();
    let found = candidates
        .iter()
        .find(|c| Path::new(&c.path) == std::fs::canonicalize(&alias).unwrap())
        .unwrap_or_else(|| panic!("adoption candidate missing: {candidates:?}"));
    assert_eq!(found.provider, "claude-w");
    assert_eq!(found.branch.as_deref(), Some("provider-made"));

    let id = shared()
        .layer
        .register_adopted(&repo.project_id, found)
        .await
        .unwrap();
    let lookup = id.clone();
    let row = shared()
        .db
        .read(move |conn| dao::worktree::get(conn, &lookup))
        .await
        .unwrap()
        .expect("adopted row");
    assert_eq!(row.origin, WorktreeOrigin::Adopted);
    assert_eq!(row.path, found.path);

    let alias_rows = shared()
        .db
        .read(move |conn| dao::worktree_alias::list_by_worktree(conn, &id))
        .await
        .unwrap();
    assert_eq!(alias_rows.len(), 1);
    assert_eq!(alias_rows[0].kind, AliasKind::Adopted);

    // An adopted worktree is not a link we made: teardown must never unlink it.
    let report = remove_owned_aliases(&alias_rows[0].worktree_id)
        .await
        .unwrap();
    assert_eq!(report[0].reason, Some(RemovalSkip::Adopted));
    assert!(!report[0].removed);
    assert!(alias.join("README.md").exists());

    // A second scan no longer offers it.
    let again = scan_adoptable(&repo.path).await.unwrap();
    assert!(
        !again
            .iter()
            .any(|c| Path::new(&c.path) == std::fs::canonicalize(&alias).unwrap()),
        "an adopted worktree must not be offered twice: {again:?}"
    );
}
