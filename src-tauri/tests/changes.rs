//! The Changes view and commit (Gilde 2 contract §4.2.5; owner T11).
//!
//! Real `git` against temporary repositories only. World is a fake
//! [`PlaceSource`]; transcripts are small JSONL files in the temp dir (never a
//! vendor directory). Covers: file rows (change kind, numstat, untracked line
//! counts, hunk-header part names), the worktree base (branch, merge-base,
//! newer on main) and its commits, made-by attribution (this session, another
//! session of the folder, nobody known, cursor/agy unknown — incrementally
//! as a transcript grows), the shared-folder count, the action states, and
//! the commit gates (owner alive, path set, confirm token, drift).

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use gilde_code_lib::contract::common::{AbsPath, Vendor};
use gilde_code_lib::contract::content::ChangeKind;
use gilde_code_lib::contract::ids::{LocationId, ProjectId, SessionUid};
use gilde_code_lib::contract::ipc::changes::{
    ActionState, ChangesGetReq, GitCommitPreviewReq, GitCommitReq, MadeBy,
};
use gilde_code_lib::contract::project::{Location, LocationKind, Project, ProjectKind};
use gilde_code_lib::contract::services::{GitApi, LocationFacts};
use gilde_code_lib::contract::session::{Session, SessionDetails};
use gilde_code_lib::contract::ErrorCode;
use gilde_code_lib::worktree::{GitService, PlaceSource, PlaceView};
use serde_json::{json, Value};

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
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}

const LIB: &str = "fn alpha() {\n    let a = 1;\n    let b = 2;\n}\n\nfn beta() {\n    let c = 3;\n}\n";

fn init_repo(root: &Path) -> PathBuf {
    let repo = root.join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    git_ok(&repo, &["init", "-q", "-b", "main", "."]);
    git_ok(&repo, &["config", "user.email", "test@example.com"]);
    git_ok(&repo, &["config", "user.name", "Test Person"]);
    git_ok(&repo, &["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.join("README.md"), b"hello\n").unwrap();
    std::fs::write(repo.join("src/lib.rs"), LIB).unwrap();
    std::fs::write(repo.join("old.txt"), b"old\n").unwrap();
    git_ok(&repo, &["add", "-A"]);
    git_ok(&repo, &["commit", "-qm", "init"]);
    std::fs::canonicalize(&repo).unwrap()
}

#[derive(Default)]
struct FakePlaces {
    sessions: parking_lot::Mutex<Vec<Session>>,
    locations: parking_lot::Mutex<Vec<Location>>,
    projects: parking_lot::Mutex<Vec<Project>>,
    transcripts: parking_lot::Mutex<HashMap<SessionUid, (AbsPath, Vendor)>>,
    seen: parking_lot::Mutex<Vec<LocationFacts>>,
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

fn control(kind: &str) -> Value {
    match kind {
        "watching" => json!({"kind": "watching", "origin": "terminal", "owner": null}),
        "inGilde" => json!({"kind": "inGilde", "mode": "terminal", "pty": null, "hostedTeam": false, "transport": "l1"}),
        _ => json!({"kind": "notRunning", "lastOrigin": null, "endedHow": "ended"}),
    }
}

fn session(vendor: &str, cwd: &Path, location: Option<&LocationId>, control: Value) -> Session {
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

struct Fx {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    repo: PathBuf,
    project: ProjectId,
    places: Arc<FakePlaces>,
    svc: GitService,
}

fn fx() -> Fx {
    let tmp = tempfile::tempdir().unwrap();
    let home = std::fs::canonicalize(tmp.path()).unwrap();
    let repo = init_repo(&home);
    let id = gilde_code_lib::discovery::location::identify(&repo, &home).unwrap();
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
    let svc = GitService::with_source(places.clone(), home.clone());
    Fx { _tmp: tmp, home, repo, project: id.project_id, places, svc }
}

impl Fx {
    /// A new worktree and a session of `vendor` in it.
    async fn worktree(&self, slug: &str, vendor: &str, control_kind: &str) -> (PathBuf, SessionUid, LocationId) {
        let facts = self.svc.create_worktree(&self.project, slug, None).await.unwrap();
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
        let uid = self.add_session(vendor, &path, Some(&facts.location_id), control_kind);
        (path, uid, facts.location_id)
    }

    fn add_session(&self, vendor: &str, cwd: &Path, location: Option<&LocationId>, control_kind: &str) -> SessionUid {
        let s = session(vendor, cwd, location, control(control_kind));
        let uid = s.uid.clone();
        self.places.sessions.lock().push(s);
        uid
    }

    /// A transcript file for `uid` (`lines` are JSONL rows).
    fn transcript(&self, uid: &SessionUid, vendor: Vendor, lines: &[Value]) -> PathBuf {
        let p = self.home.join("transcripts").join(format!("{}.jsonl", uid.as_str()));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&p).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        self.places.transcripts.lock().insert(uid.clone(), (AbsPath::parse(p.to_str().unwrap()).unwrap(), vendor));
        p
    }
}

fn claude_edit(tool: &str, path: &Path) -> Value {
    json!({"type": "assistant", "uuid": "u1", "message": {"role": "assistant", "content": [
        {"type": "text", "text": "editing"},
        {"type": "tool_use", "id": "toolu_1", "name": tool, "input": {"file_path": path.to_str().unwrap(), "content": "x"}}
    ]}})
}

fn row<'a>(view: &'a gilde_code_lib::contract::ipc::changes::ChangesView, path: &str) -> &'a gilde_code_lib::contract::ipc::changes::FileRow {
    view.files.iter().find(|f| f.path_rel == path).unwrap_or_else(|| panic!("no row {path} in {:?}", view.files))
}

// ---------------------------------------------------------------------------
// the view
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_worktree_view_has_rows_parts_base_and_commits() {
    let f = fx();
    let (wt, uid, loc) = f.worktree("feature", "claude", "notRunning").await;

    // One commit on the branch, then uncommitted work on top.
    std::fs::write(wt.join("committed.txt"), b"a\nb\n").unwrap();
    git_ok(&wt, &["add", "-A"]);
    git_ok(&wt, &["commit", "-qm", "feature work"]);
    std::fs::write(
        wt.join("src/lib.rs"),
        "fn alpha() {\n    let a = 10;\n    let b = 2;\n}\n\nfn beta() {\n    let c = 30;\n    let d = 4;\n}\n",
    )
    .unwrap();
    std::fs::remove_file(wt.join("old.txt")).unwrap();
    std::fs::write(wt.join("new file.txt"), b"1\n2\n3\n").unwrap();
    // main moves on.
    std::fs::write(f.repo.join("README.md"), b"hello\nmain\n").unwrap();
    git_ok(&f.repo, &["commit", "-qam", "main moves"]);

    let view = f.svc.changes(&ChangesGetReq { uid: uid.clone() }).await.unwrap();
    assert_eq!(view.location, loc);
    let base = view.base.clone().expect("a worktree has a base");
    assert_eq!(base.branch, "main");
    assert_eq!(base.newer_on_main, 1);
    let mb = git_ok(&wt, &["merge-base", "HEAD", "main"]);
    assert!(mb.starts_with(&base.commit_short), "{mb} vs {}", base.commit_short);

    let paths: Vec<&str> = view.files.iter().map(|f| f.path_rel.as_str()).collect();
    assert_eq!(paths, vec!["committed.txt", "new file.txt", "old.txt", "src/lib.rs"], "sorted by path");
    let lib = row(&view, "src/lib.rs");
    assert_eq!((lib.change, lib.added, lib.removed), (ChangeKind::Modified, 3, 2));
    assert!(lib.parts.iter().any(|p| p.starts_with("fn alpha")), "{:?}", lib.parts);
    assert!(lib.parts.iter().any(|p| p.starts_with("fn beta")), "{:?}", lib.parts);
    assert!(lib.parts.len() <= 8);
    let new = row(&view, "new file.txt");
    assert_eq!((new.change, new.added, new.removed), (ChangeKind::Added, 3, 0), "untracked lines are counted");
    assert_eq!(row(&view, "old.txt").change, ChangeKind::Deleted);
    assert_eq!(row(&view, "committed.txt").change, ChangeKind::Added, "committed on the branch, vs the merge base");

    assert_eq!(view.commits.len(), 1);
    assert_eq!(view.commits[0].subject, "feature work");
    assert!(view.commits[0].at > 1_600_000_000_000, "epoch ms");
    assert!(view.shared_folder.is_none());
    assert_eq!(view.actions.commit, ActionState::Enabled);
    assert_eq!(view.actions.merge, ActionState::Enabled);
    assert_eq!(view.actions.discard, ActionState::Enabled);
    // No transcript: made-by is unknown, never guessed.
    assert!(view.files.iter().all(|f| f.made_by == MadeBy::Unknown));
}

#[tokio::test]
async fn made_by_attributes_this_session_other_sessions_and_nobody() {
    let f = fx();
    let (wt, me, loc) = f.worktree("shared", "claude", "inGilde").await;
    let other = f.add_session("claude", &wt, Some(&loc), "watching");
    for name in ["mine.txt", "theirs.txt", "nobody.txt", "late.txt"] {
        std::fs::write(wt.join(name), b"x\n").unwrap();
    }
    f.transcript(&me, Vendor::Claude, &[json!({"type": "user"}), claude_edit("Write", &wt.join("mine.txt"))]);
    f.transcript(&other, Vendor::Claude, &[claude_edit("Edit", &wt.join("theirs.txt"))]);

    let view = f.svc.changes(&ChangesGetReq { uid: me.clone() }).await.unwrap();
    assert_eq!(row(&view, "mine.txt").made_by, MadeBy::ThisSession);
    assert_eq!(row(&view, "theirs.txt").made_by, MadeBy::OtherSession { uid: other.clone() });
    assert_eq!(row(&view, "nobody.txt").made_by, MadeBy::NotFromKnownSession);
    assert_eq!(view.shared_folder.as_ref().map(|s| s.other_live_sessions), Some(1));
    // A live session in the worktree: Discard waits for it to stop.
    assert_eq!(view.actions.discard, ActionState::Disabled { reason: "changes.stopToDiscard".into() });

    // The transcript grows: the index picks up only the new rows.
    f.transcript(&me, Vendor::Claude, &[claude_edit("MultiEdit", &wt.join("late.txt"))]);
    let view = f.svc.changes(&ChangesGetReq { uid: me.clone() }).await.unwrap();
    assert_eq!(row(&view, "late.txt").made_by, MadeBy::ThisSession);
    assert_eq!(row(&view, "mine.txt").made_by, MadeBy::ThisSession);

    // From the other session's side the attribution flips.
    let view = f.svc.changes(&ChangesGetReq { uid: other.clone() }).await.unwrap();
    assert_eq!(row(&view, "theirs.txt").made_by, MadeBy::ThisSession);
    assert_eq!(row(&view, "mine.txt").made_by, MadeBy::OtherSession { uid: me.clone() });
    // Watched: commit, merge and discard wait for the owner (#108).
    let owner = ActionState::Disabled { reason: "changes.ownerWriting".into() };
    assert_eq!(view.actions.commit, owner);
    assert_eq!(view.actions.discard, owner);
}

#[tokio::test]
async fn codex_rollouts_and_vendors_without_tool_events() {
    let f = fx();
    let (wt, codex, loc) = f.worktree("codex", "codex", "notRunning").await;
    for name in ["patched.rs", "changed.rs", "plain.rs"] {
        std::fs::write(wt.join(name), b"x\n").unwrap();
    }
    let patch = "*** Begin Patch\n*** Update File: patched.rs\n@@\n-x\n+y\n*** End Patch\n";
    f.transcript(
        &codex,
        Vendor::Codex,
        &[
            json!({"type": "response_item", "payload": {"type": "function_call", "name": "apply_patch",
                    "arguments": serde_json::to_string(&json!({"input": patch})).unwrap()}}),
            json!({"type": "event_msg", "payload": {"type": "item_completed", "item": {"type": "FileChange",
                    "changes": {wt.join("changed.rs").to_str().unwrap(): {"type": "update"}}}}}),
        ],
    );
    let view = f.svc.changes(&ChangesGetReq { uid: codex }).await.unwrap();
    assert_eq!(row(&view, "patched.rs").made_by, MadeBy::ThisSession, "a relative patch path joins the cwd");
    assert_eq!(row(&view, "changed.rs").made_by, MadeBy::ThisSession);
    assert_eq!(row(&view, "plain.rs").made_by, MadeBy::NotFromKnownSession);

    // cursor and agy give no tool events: every row is unknown.
    for vendor in ["cursor", "agy"] {
        let uid = f.add_session(vendor, &wt, Some(&loc), "notRunning");
        let view = f.svc.changes(&ChangesGetReq { uid }).await.unwrap();
        assert!(view.files.iter().all(|r| r.made_by == MadeBy::Unknown), "{vendor}");
    }
}

#[tokio::test]
async fn the_main_checkout_diffs_against_head_and_folders_are_not_git() {
    let f = fx();
    let main = f.add_session("claude", &f.repo, None, "notRunning");
    std::fs::write(f.repo.join("README.md"), b"changed\n").unwrap();
    let view = f.svc.changes(&ChangesGetReq { uid: main }).await.unwrap();
    assert!(view.base.is_none() && view.commits.is_empty());
    assert_eq!(row(&view, "README.md").change, ChangeKind::Modified);
    assert_eq!(view.actions.merge, ActionState::Hidden);
    assert_eq!(view.actions.discard, ActionState::Hidden);

    let folder = f.home.join("plain-folder");
    std::fs::create_dir_all(&folder).unwrap();
    let uid = f.add_session("claude", &folder, None, "notRunning");
    let e = f.svc.changes(&ChangesGetReq { uid }).await.unwrap_err();
    assert_eq!((e.code, e.params["cap"].as_str()), (ErrorCode::Unsupported, Some("notGit")));

    let e = f.svc.changes(&ChangesGetReq { uid: SessionUid::mint() }).await.unwrap_err();
    assert_eq!((e.code, e.params["entity"].as_str()), (ErrorCode::NotFound, Some("session")));
}

#[tokio::test]
async fn a_repository_without_commits_lists_its_files() {
    let tmp = tempfile::tempdir().unwrap();
    let home = std::fs::canonicalize(tmp.path()).unwrap();
    let repo = home.join("fresh");
    std::fs::create_dir_all(&repo).unwrap();
    git_ok(&repo, &["init", "-q", "-b", "main", "."]);
    std::fs::write(repo.join("a.txt"), b"1\n").unwrap();
    let places = Arc::new(FakePlaces::default());
    let s = session("claude", &repo, None, control("notRunning"));
    let uid = s.uid.clone();
    places.sessions.lock().push(s);
    let svc = GitService::with_source(places, home);
    let view = svc.changes(&ChangesGetReq { uid }).await.unwrap();
    assert_eq!(view.files.len(), 1);
    assert_eq!(view.files[0].change, ChangeKind::Added);
}

// ---------------------------------------------------------------------------
// commit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn commit_takes_exactly_the_previewed_paths_behind_a_single_use_token() {
    let f = fx();
    let (wt, uid, _) = f.worktree("commit", "claude", "inGilde").await;
    std::fs::write(wt.join("a.txt"), b"a\n").unwrap();
    std::fs::write(wt.join("b.txt"), b"b\n").unwrap();
    std::fs::write(wt.join("c.txt"), b"c\n").unwrap();
    std::fs::remove_file(wt.join("old.txt")).unwrap();

    let e = f
        .svc
        .commit_preview(&GitCommitPreviewReq { uid: uid.clone(), paths: vec!["nope.txt".into()] })
        .await
        .unwrap_err();
    assert_eq!((e.params["field"].as_str(), e.params["rule"].as_str()), (Some("paths[0]"), Some("path")));

    let preview = f
        .svc
        .commit_preview(&GitCommitPreviewReq { uid: uid.clone(), paths: vec!["b.txt".into(), "a.txt".into(), "old.txt".into()] })
        .await
        .unwrap();
    assert_eq!(preview.files.iter().map(|r| r.path_rel.as_str()).collect::<Vec<_>>(), vec!["a.txt", "b.txt", "old.txt"]);
    assert_eq!((preview.author.name.as_str(), preview.author.email.as_str()), ("Test Person", "test@example.com"));

    // A path outside the previewed set is refused before anything happens.
    let e = f
        .svc
        .commit(&GitCommitReq { uid: uid.clone(), message: "x".into(), paths: vec!["c.txt".into()], confirm_token: preview.confirm_token.clone() })
        .await
        .unwrap_err();
    assert_eq!(e.params["rule"], "path");

    let message = "-starts with a dash\n\nand has a body";
    let done = f
        .svc
        .commit(&GitCommitReq {
            uid: uid.clone(),
            message: message.into(),
            paths: vec!["a.txt".into(), "old.txt".into()],
            confirm_token: preview.confirm_token.clone(),
        })
        .await
        .unwrap();
    assert_eq!(done.commit, git_ok(&wt, &["rev-parse", "HEAD"]));
    assert_eq!(git_ok(&wt, &["log", "-1", "--format=%B"]), message);
    assert_eq!(git_ok(&wt, &["log", "-1", "--format=%an"]), "Test Person", "the user is the author");
    let committed = git_ok(&wt, &["show", "--name-status", "--format=", "HEAD"]);
    assert_eq!(committed, "A\ta.txt\nD\told.txt", "exactly the chosen paths");
    let status = git_ok(&wt, &["status", "--porcelain"]);
    assert!(status.contains("?? b.txt") && status.contains("?? c.txt"), "{status}");

    // Single use.
    let e = f
        .svc
        .commit(&GitCommitReq { uid: uid.clone(), message: "again".into(), paths: vec!["b.txt".into()], confirm_token: preview.confirm_token })
        .await
        .unwrap_err();
    assert_eq!((e.code, e.params["changed"].as_str()), (ErrorCode::Conflict, Some("changed")));

    // A previewed file that changes before the commit → CONFLICT, nothing committed.
    let preview = f.svc.commit_preview(&GitCommitPreviewReq { uid: uid.clone(), paths: vec!["b.txt".into()] }).await.unwrap();
    std::fs::write(wt.join("b.txt"), b"b\nchanged by the agent\n").unwrap();
    let head = git_ok(&wt, &["rev-parse", "HEAD"]);
    let e = f
        .svc
        .commit(&GitCommitReq { uid: uid.clone(), message: "b".into(), paths: vec!["b.txt".into()], confirm_token: preview.confirm_token })
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::Conflict);
    assert_eq!(git_ok(&wt, &["rev-parse", "HEAD"]), head);
}

#[tokio::test]
async fn a_watched_session_cannot_commit() {
    let f = fx();
    let (wt, uid, _) = f.worktree("watched", "claude", "watching").await;
    std::fs::write(wt.join("a.txt"), b"a\n").unwrap();
    let e = f.svc.commit_preview(&GitCommitPreviewReq { uid, paths: vec!["a.txt".into()] }).await.unwrap_err();
    assert_eq!((e.code, e.params["reason"].as_str()), (ErrorCode::Refused, Some("ownerAlive")));
}

// ---------------------------------------------------------------------------
// through the IPC layer (T-ipc-1 cases for the T11 commands)
// ---------------------------------------------------------------------------

mod ipc {
    use super::*;
    use gilde_code_lib::commands::changes::GitState;
    use gilde_code_lib::commands::registry;
    use tauri::test::{get_ipc_response, mock_builder, MockRuntime, INVOKE_KEY};
    use tauri::webview::InvokeRequest;
    use tauri::{Manager as _, WebviewWindow};

    fn invoke(w: &WebviewWindow<MockRuntime>, cmd: &str, req: Value) -> Result<Value, Value> {
        get_ipc_response(
            w,
            InvokeRequest {
                cmd: cmd.into(),
                callback: tauri::ipc::CallbackFn(0),
                error: tauri::ipc::CallbackFn(1),
                url: "tauri://localhost".parse().unwrap(),
                body: tauri::ipc::InvokeBody::Json(json!({ "req": req })),
                headers: Default::default(),
                invoke_key: INVOKE_KEY.to_string(),
            },
        )
        .map(|b| b.deserialize::<Value>().unwrap_or(Value::Null))
    }

    #[test]
    fn changes_get_answers_through_the_command_with_the_contract_shapes() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let f = fx();
        let (wt, uid, _) = rt.block_on(f.worktree("ipc", "claude", "notRunning"));
        std::fs::write(wt.join("x.txt"), b"x\n").unwrap();
        let app = mock_builder()
            .invoke_handler(registry::handler())
            .build(tauri::generate_context!(test = true))
            .unwrap();
        let svc = GitService::with_source(f.places.clone(), f.home.clone());
        app.manage(GitState(Arc::new(svc)));
        let w = tauri::WebviewWindowBuilder::new(&app, "main", Default::default()).build().unwrap();

        let ok = invoke(&w, "changes_get", json!({"uid": uid.as_str()})).unwrap();
        assert_eq!(ok["files"][0]["pathRel"], "x.txt");
        assert_eq!(ok["files"][0]["madeBy"]["kind"], "unknown");
        assert_eq!(ok["actions"]["merge"]["kind"], "hidden", "nothing committed yet");
        assert_eq!(ok["base"]["branch"], "main");

        let e = invoke(&w, "changes_get", json!({"uid": SessionUid::mint().as_str()})).unwrap_err();
        assert_eq!((e["code"].as_str(), e["params"]["entity"].as_str()), (Some("NOT_FOUND"), Some("session")));
        let e = invoke(&w, "changes_get", json!({"uid": "s_nope"})).unwrap_err();
        assert_eq!(e["code"], "INVALID_ARGUMENT");
        let e = invoke(&w, "git_commit_preview", json!({"uid": uid.as_str(), "paths": []})).unwrap_err();
        assert_eq!((e["params"]["field"].as_str(), e["params"]["rule"].as_str()), (Some("paths"), Some("minItems")));
        let e = invoke(&w, "git_commit_preview", json!({"uid": uid.as_str(), "paths": ["../escape"]})).unwrap_err();
        assert_eq!(e["params"]["rule"], "path");
        // A17.5: there is no branch deletion; the old field is an unknown field.
        let e = invoke(&w, "worktree_discard", json!({"uid": uid.as_str(), "confirmToken": "c_0123456789abcdef0123456789abcdef", "acceptedLosses": [], "deleteBranch": true})).unwrap_err();
        assert_eq!((e["code"].as_str(), e["params"]["rule"].as_str()), (Some("INVALID_ARGUMENT"), Some("shape")));
        let e = invoke(&w, "worktree_discard", json!({"uid": uid.as_str(), "confirmToken": "c_0123456789abcdef0123456789abcdef", "acceptedLosses": []})).unwrap_err();
        assert_eq!((e["code"].as_str(), e["params"]["changed"].as_str()), (Some("CONFLICT"), Some("changed")), "a token never issued");
        drop(rt);
    }
}
