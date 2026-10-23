//! Open in IDE, Reveal, links and `project_pick` (Gilde 2 contract §4.2.2,
//! §4.2.5, §9.4, §13.1; owner T11).
//!
//! **No test here opens an application, a Finder window, a browser or a URL.**
//! The launcher, the Finder, the URL opener, the native confirmation and the
//! folder picker are fakes; the real launcher runs only harmless programs
//! (`/bin/sh` scripts in a temp dir) with a fake supervisor. The real editor,
//! Finder and browser paths are exercised only by the `#[ignore]` tests at the
//! end, which the lead may run by hand.
//!
//! * **T-ide-paths** — `..` escapes, absolute paths outside, symlinks out
//!   (file and directory), same-prefix siblings, case variants (on a
//!   case-insensitive volume), NFD names, `/tmp`↔`/private/tmp`, vendor roots
//!   inside an allowed root, other locations of the project, picked folders.
//! * **T-ide-editors** — discovery by bundle id, nothing chosen, chosen but
//!   gone, no bundle CLI (`open -b`; no diff).
//! * **Diff temp files** — the base side is 0400 in a 0700 dir under
//!   `tmp/diff`, a deleted file diffs against an empty file, the base bytes
//!   come from git (merge-base for a worktree).

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use gilde_code_lib::commands::project::{pick_project, FolderPicker, ProjectState};
use gilde_code_lib::contract::common::{AbsPath, Vendor};
use gilde_code_lib::contract::ids::{LocationId, ProjectId, SessionUid};
use gilde_code_lib::contract::ipc::changes::{EditorKind, IdeOpenReq, IdeRevealReq, IdeTarget, RevealTarget};
use gilde_code_lib::contract::project::{Location, LocationKind, Project, ProjectKind};
use gilde_code_lib::contract::services::{IdeApi, LocationFacts, SupervisorApi};
use gilde_code_lib::contract::session::{Session, SessionDetails};
use gilde_code_lib::contract::{ErrorCode, IpcError};
use gilde_code_lib::ide::editors::{self, BundleReader, CURSOR_BUNDLE_ID, VSCODE_BUNDLE_ID};
use gilde_code_lib::ide::links::{LinkConfirm, UrlOpener};
use gilde_code_lib::ide::opener::{LaunchPlan, Launcher, SupervisedLauncher};
use gilde_code_lib::ide::paths::PickedRoots;
use gilde_code_lib::ide::reveal::Revealer;
use gilde_code_lib::ide::{IdeService, Platform};
use gilde_code_lib::worktree::{GitService, PlaceSource, PlaceView};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// fakes
// ---------------------------------------------------------------------------

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

fn session(cwd: &Path, location: Option<&LocationId>) -> Session {
    let details = serde_json::to_value(SessionDetails::default()).unwrap();
    serde_json::from_value(json!({
        "uid": SessionUid::mint(), "vendor": "claude", "accountId": null, "vendorSessionId": null,
        "projectId": null, "locationId": location, "cwd": cwd.to_str().unwrap(),
        "control": {"kind": "notRunning", "lastOrigin": null, "endedHow": "ended"},
        "activity": {"state": {"kind": "working"}, "since": 1, "confidence": "high", "evidence": null, "episode": "e_0123456789abcdef"},
        "title": null, "team": null, "startedAt": null, "endedAt": null, "lastActivityAt": null,
        "pending": [], "queued": [], "model": null, "permissionMode": null, "binary": null,
        "claudeVersion": null, "memoryBytes": null, "usageToday": null,
        "caps": {"send": "none", "interrupt": false, "forceStop": false, "approve": "none", "setModel": false,
                 "setPermissionMode": [], "continueOptions": [], "stopSession": "none", "terminalView": false,
                 "logView": false, "changes": "full", "ownerFocus": false},
        "details": details, "mergedInto": null
    }))
    .unwrap()
}

#[derive(Default)]
struct RecordingLauncher {
    plans: parking_lot::Mutex<Vec<LaunchPlan>>,
}

#[async_trait]
impl Launcher for RecordingLauncher {
    async fn launch(&self, plan: &LaunchPlan) -> Result<(), IpcError> {
        self.plans.lock().push(plan.clone());
        Ok(())
    }
}

#[derive(Default)]
struct RecordingRevealer(parking_lot::Mutex<Vec<PathBuf>>);
impl Revealer for RecordingRevealer {
    fn reveal(&self, path: &Path) -> Result<(), IpcError> {
        self.0.lock().push(path.to_path_buf());
        Ok(())
    }
}

#[derive(Default)]
struct RecordingOpener(parking_lot::Mutex<Vec<String>>);
impl UrlOpener for RecordingOpener {
    fn open(&self, url: &str) -> Result<(), IpcError> {
        self.0.lock().push(url.to_string());
        Ok(())
    }
}

struct Answer(bool, parking_lot::Mutex<Vec<String>>);
#[async_trait]
impl LinkConfirm for Answer {
    async fn confirm_local(&self, host: &str) -> bool {
        self.1.lock().push(host.to_string());
        self.0
    }
}

/// Reads a fixture bundle's id from `Contents/id` (no plist, no AppKit).
struct FixtureBundles;
impl BundleReader for FixtureBundles {
    fn bundle_id(&self, app: &Path) -> Option<String> {
        std::fs::read_to_string(app.join("Contents/id")).ok().map(|s| s.trim().to_string())
    }
}

fn fixture_app(dir: &Path, name: &str, id: &str, with_cli: Option<EditorKind>) {
    let app = dir.join(name);
    std::fs::create_dir_all(app.join("Contents")).unwrap();
    std::fs::write(app.join("Contents/id"), id).unwrap();
    if let Some(kind) = with_cli {
        let cli = app.join(editors::cli_relative(kind));
        std::fs::create_dir_all(cli.parent().unwrap()).unwrap();
        std::fs::write(&cli, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

struct Fx {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    proj: PathBuf,
    uid: SessionUid,
    places: Arc<FakePlaces>,
    launcher: Arc<RecordingLauncher>,
    revealer: Arc<RecordingRevealer>,
    opener: Arc<RecordingOpener>,
    picked: Arc<PickedRoots>,
    chosen: Arc<parking_lot::Mutex<Option<EditorKind>>>,
    apps: PathBuf,
}

fn fx_at(cwd_of: impl Fn(&Path) -> PathBuf) -> Fx {
    let tmp = tempfile::tempdir().unwrap();
    let home = std::fs::canonicalize(tmp.path()).unwrap();
    let proj = home.join("work/proj");
    std::fs::create_dir_all(proj.join("src")).unwrap();
    std::fs::write(proj.join("src/a.rs"), b"fn a() {}\n").unwrap();
    let apps = home.join("Applications");
    std::fs::create_dir_all(&apps).unwrap();
    fixture_app(&apps, "Visual Studio Code.app", VSCODE_BUNDLE_ID, Some(EditorKind::VsCode));
    let places = Arc::new(FakePlaces::default());
    let s = session(&cwd_of(&home), None);
    let uid = s.uid.clone();
    places.sessions.lock().push(s);
    Fx {
        _tmp: tmp,
        home,
        proj,
        uid,
        places,
        launcher: Default::default(),
        revealer: Default::default(),
        opener: Default::default(),
        picked: PickedRoots::new(),
        chosen: Arc::new(parking_lot::Mutex::new(Some(EditorKind::VsCode))),
        apps,
    }
}

fn fx() -> Fx {
    fx_at(|home| home.join("work/proj"))
}

impl Fx {
    fn service_with(&self, confirm: Arc<dyn LinkConfirm>) -> IdeService {
        let chosen = self.chosen.clone();
        let platform = Platform {
            launcher: self.launcher.clone(),
            revealer: self.revealer.clone(),
            url_opener: self.opener.clone(),
            link_confirm: confirm,
            bundles: Arc::new(FixtureBundles),
            app_dirs: vec![self.apps.clone()],
        };
        IdeService::new(
            self.places.clone(),
            self.home.clone(),
            self.home.join(".gilde/tmp/diff"),
            self.picked.clone(),
            Arc::new(move || *chosen.lock()),
            platform,
        )
    }

    fn service(&self) -> IdeService {
        self.service_with(Arc::new(Answer(false, Default::default())))
    }

    async fn open_file(&self, svc: &IdeService, path: &str) -> Result<LaunchPlan, IpcError> {
        svc.open(&IdeOpenReq { target: IdeTarget::File { uid: self.uid.clone(), path: path.into(), line: Some(7) } }).await?;
        Ok(self.launcher.plans.lock().last().cloned().unwrap())
    }
}

fn refused(r: Result<LaunchPlan, IpcError>, what: &str) {
    let e = r.expect_err(what);
    assert_eq!((e.code, e.params["reason"].as_str()), (ErrorCode::Refused, Some("outsideRoots")), "{what}: {e:?}");
}

// ---------------------------------------------------------------------------
// T-ide-paths
// ---------------------------------------------------------------------------

#[tokio::test]
async fn t_ide_paths_relative_absolute_and_dot_dot() {
    let f = fx();
    let svc = f.service();
    let plan = f.open_file(&svc, "src/a.rs").await.unwrap();
    assert_eq!(plan.args, vec!["--goto".to_string(), format!("{}:7", f.proj.join("src/a.rs").display())]);
    let abs = f.proj.join("src/a.rs");
    assert!(f.open_file(&svc, abs.to_str().unwrap()).await.is_ok());
    assert!(f.open_file(&svc, "src/../src/a.rs").await.is_ok());

    std::fs::write(f.home.join("work/outside.txt"), b"x").unwrap();
    refused(f.open_file(&svc, "../outside.txt").await, "dot-dot out");
    refused(f.open_file(&svc, "src/../../outside.txt").await, "dot-dot out through a subdir");
    refused(f.open_file(&svc, "/etc/hosts").await, "absolute outside");
    // A sibling whose name extends the root's: the trailing-slash rule.
    std::fs::create_dir_all(f.home.join("work/proj-evil")).unwrap();
    std::fs::write(f.home.join("work/proj-evil/x.rs"), b"x").unwrap();
    refused(f.open_file(&svc, "../proj-evil/x.rs").await, "same-prefix sibling");
    let e = f.open_file(&svc, "src/missing.rs").await.unwrap_err();
    assert_eq!((e.code, e.params["entity"].as_str()), (ErrorCode::NotFound, Some("file")));
}

#[tokio::test]
async fn t_ide_paths_symlinks_out_are_refused_symlinks_in_are_followed() {
    let f = fx();
    let svc = f.service();
    std::fs::write(f.home.join("secret.txt"), b"x").unwrap();
    std::os::unix::fs::symlink(f.home.join("secret.txt"), f.proj.join("link-out")).unwrap();
    std::os::unix::fs::symlink("/etc", f.proj.join("etc-dir")).unwrap();
    std::os::unix::fs::symlink(f.proj.join("src/a.rs"), f.proj.join("link-in")).unwrap();
    refused(f.open_file(&svc, "link-out").await, "file symlink out");
    refused(f.open_file(&svc, "etc-dir/hosts").await, "directory symlink out");
    let plan = f.open_file(&svc, "link-in").await.unwrap();
    assert!(plan.args[1].starts_with(f.proj.join("src/a.rs").to_str().unwrap()), "resolved to the target: {plan:?}");
}

#[tokio::test]
async fn t_ide_paths_case_variants_and_unicode_forms() {
    let f = fx();
    let svc = f.service();
    if gilde_code_lib::ide::paths::case_insensitive(&f.proj) {
        assert!(f.open_file(&svc, "SRC/A.RS").await.is_ok(), "a case variant inside the root");
        std::fs::create_dir_all(f.home.join("work/PROJ2")).unwrap();
        std::fs::write(f.home.join("work/PROJ2/b.rs"), b"x").unwrap();
        refused(f.open_file(&svc, "../PROJ2/b.rs").await, "a case-folded same-prefix sibling");
        std::fs::create_dir_all(f.home.join(".claude")).unwrap();
        std::fs::write(f.home.join(".claude/settings.json"), b"{}").unwrap();
        refused(f.open_file(&svc, f.home.join(".CLAUDE/settings.json").to_str().unwrap()).await, "vendor root in another case");
    }
    use unicode_normalization::UnicodeNormalization as _;
    let nfc = "한글.txt";
    std::fs::write(f.proj.join(nfc), b"x").unwrap();
    let nfd: String = format!("{}/{}", f.proj.display(), nfc).nfd().collect();
    assert!(f.open_file(&svc, &nfd).await.is_ok(), "an NFD spelling of an NFC name");
}

#[tokio::test]
async fn t_ide_paths_vendor_roots_are_refused_even_inside_an_allowed_root() {
    // A Folder project at the home dir itself: every vendor dir is inside it.
    let f = fx_at(|home| home.to_path_buf());
    for (dir, file) in [(".claude", "settings.json"), (".claude-g", "x.json"), (".codex", "auth.json"), (".cursor", "a"), (".gemini", "b")] {
        std::fs::create_dir_all(f.home.join(dir)).unwrap();
        std::fs::write(f.home.join(dir).join(file), b"{}").unwrap();
    }
    std::fs::write(f.home.join(".claude.json"), b"{}").unwrap();
    let svc = f.service();
    assert!(f.open_file(&svc, "work/proj/src/a.rs").await.is_ok(), "the home folder project itself is open");
    for p in [".claude/settings.json", ".claude-g/x.json", ".codex/auth.json", ".cursor/a", ".gemini/b", ".claude.json"] {
        refused(f.open_file(&svc, p).await, p);
    }
    // A user-added config dir elsewhere is refused too.
    let svc = f.service().with_vendor_roots(vec![f.home.join("work/proj")]);
    refused(f.open_file(&svc, "work/proj/src/a.rs").await, "an extra vendor root");
}

#[tokio::test]
async fn t_ide_paths_tmp_spellings_other_locations_and_picked_folders() {
    // A session whose cwd is spelled `/tmp/…` while the file is `/private/tmp/…`.
    let dir = tempfile::Builder::new().prefix("gilde-ide-").tempdir_in("/tmp").unwrap();
    let spelled = dir.path().to_path_buf();
    let real = std::fs::canonicalize(&spelled).unwrap();
    assert!(real.starts_with("/private/tmp"), "{real:?}");
    std::fs::write(real.join("f.txt"), b"x").unwrap();
    let f = fx_at(|_| spelled.clone());
    let svc = f.service();
    assert!(f.open_file(&svc, real.join("f.txt").to_str().unwrap()).await.is_ok());
    assert!(f.open_file(&svc, spelled.join("f.txt").to_str().unwrap()).await.is_ok());

    // Another location of the same project is allowed; an unrelated folder is
    // allowed only once it was picked.
    let f = fx();
    let other = f.home.join("work/proj-wt/x");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("o.rs"), b"x").unwrap();
    let pid = gilde_code_lib::discovery::location::identify(&f.proj, &f.home).unwrap().project_id;
    f.places.locations.lock().push(Location {
        id: LocationId::derive(other.to_str().unwrap()),
        project_id: pid,
        path: AbsPath::parse(other.to_str().unwrap()).unwrap(),
        kind: LocationKind::Worktree,
        branch: None,
        worktree_name: None,
        gilde_created: false,
        exists: true,
    });
    let svc = f.service();
    assert!(f.open_file(&svc, other.join("o.rs").to_str().unwrap()).await.is_ok(), "another location of the project");
    let notes = f.home.join("notes");
    std::fs::create_dir_all(&notes).unwrap();
    std::fs::write(notes.join("n.md"), b"x").unwrap();
    refused(f.open_file(&svc, notes.join("n.md").to_str().unwrap()).await, "not picked yet");
    f.picked.add(&notes);
    assert!(f.open_file(&svc, notes.join("n.md").to_str().unwrap()).await.is_ok(), "picked in this instance");
}

// ---------------------------------------------------------------------------
// T-ide-editors
// ---------------------------------------------------------------------------

#[tokio::test]
async fn t_ide_editors_found_by_bundle_id_and_the_no_editor_path() {
    let f = fx();
    fixture_app(&f.apps, "Cursor Renamed.app", CURSOR_BUNDLE_ID, None);
    fixture_app(&f.apps, "Cursor.app", "com.example.other", Some(EditorKind::Cursor));
    let svc = f.service();
    let list = svc.editors();
    assert_eq!(list.found.iter().map(|o| o.editor).collect::<Vec<_>>(), vec![EditorKind::VsCode, EditorKind::Cursor]);
    assert_eq!(list.found[1].app_path, "~/Applications/Cursor Renamed.app", "by id, not by name; home as ~");
    assert_eq!(list.chosen, Some(EditorKind::VsCode));

    // Cursor without a bundle CLI: files open with `open -b`, diffs are not offered.
    *f.chosen.lock() = Some(EditorKind::Cursor);
    let plan = f.open_file(&svc, "src/a.rs").await.unwrap();
    assert_eq!(plan.argv0, PathBuf::from("/usr/bin/open"));
    assert_eq!(plan.args, vec!["-b".to_string(), CURSOR_BUNDLE_ID.to_string(), f.proj.join("src/a.rs").display().to_string()]);
    let e = svc.open(&IdeOpenReq { target: IdeTarget::Diff { uid: f.uid.clone(), path: "src/a.rs".into() } }).await.unwrap_err();
    assert_eq!(e.params["cap"], "diffNotSupportedByEditor");

    *f.chosen.lock() = None;
    let e = f.open_file(&svc, "src/a.rs").await.unwrap_err();
    assert_eq!((e.code, e.params["reason"].as_str()), (ErrorCode::NotReady, Some("editorNotChosen")));
    std::fs::remove_dir_all(f.apps.join("Visual Studio Code.app")).unwrap();
    *f.chosen.lock() = Some(EditorKind::VsCode);
    let e = f.open_file(&svc, "src/a.rs").await.unwrap_err();
    assert_eq!(e.params["reason"], "editorNotChosen", "chosen but no longer installed");
    assert_eq!(svc.editors().found.len(), 1);
}

#[tokio::test]
async fn folder_project_root_and_focus_targets() {
    let f = fx();
    let svc = f.service();
    svc.open(&IdeOpenReq { target: IdeTarget::Folder { uid: f.uid.clone() } }).await.unwrap();
    assert_eq!(f.launcher.plans.lock().last().unwrap().args, vec![f.proj.display().to_string()]);
    let pid = gilde_code_lib::discovery::location::identify(&f.proj, &f.home).unwrap().project_id;
    let e = svc.open(&IdeOpenReq { target: IdeTarget::ProjectRoot { project_id: pid.clone() } }).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound, "an unknown project");
    f.picked.add(&f.proj);
    svc.open(&IdeOpenReq { target: IdeTarget::ProjectRoot { project_id: pid } }).await.unwrap();
    assert!(svc.focus_editor_window(EditorKind::VsCode, &AbsPath::parse(f.proj.to_str().unwrap()).unwrap()).await.unwrap());
    assert_eq!(f.launcher.plans.lock().last().unwrap().args, vec![f.proj.display().to_string()]);
}

// ---------------------------------------------------------------------------
// Diff in IDE: temp files 0400, base from git
// ---------------------------------------------------------------------------

fn git_ok(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git").arg("-C").arg(cwd).args(args).env("GIT_TERMINAL_PROMPT", "0").output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}

#[tokio::test]
async fn diff_base_sides_are_read_only_temp_files() {
    let f = fx();
    git_ok(&f.proj, &["init", "-q", "-b", "main", "."]);
    git_ok(&f.proj, &["config", "user.email", "t@example.com"]);
    git_ok(&f.proj, &["config", "user.name", "T"]);
    git_ok(&f.proj, &["config", "commit.gpgsign", "false"]);
    std::fs::write(f.proj.join("gone.rs"), b"old\n").unwrap();
    git_ok(&f.proj, &["add", "-A"]);
    git_ok(&f.proj, &["commit", "-qm", "init"]);
    std::fs::write(f.proj.join("src/a.rs"), b"fn a() { changed }\n").unwrap();
    std::fs::remove_file(f.proj.join("gone.rs")).unwrap();
    std::fs::write(f.proj.join("fresh.rs"), b"new\n").unwrap();

    let git = Arc::new(GitService::with_source(f.places.clone(), f.home.clone()));
    let svc = f.service().with_diff_source(git);
    let diff = |p: &str| {
        let req = IdeOpenReq { target: IdeTarget::Diff { uid: f.uid.clone(), path: p.into() } };
        let svc = &svc;
        async move { svc.open(&req).await }
    };

    diff("src/a.rs").await.unwrap();
    let plan = f.launcher.plans.lock().last().cloned().unwrap();
    assert_eq!(plan.args[0], "--diff");
    let left = PathBuf::from(&plan.args[1]);
    assert_eq!(std::fs::read(&left).unwrap(), b"fn a() {}\n", "the base side is HEAD's bytes");
    assert_eq!(std::fs::metadata(&left).unwrap().permissions().mode() & 0o777, 0o400);
    assert_eq!(std::fs::metadata(left.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
    assert!(left.starts_with(f.home.join(".gilde/tmp/diff")));
    assert_eq!(plan.args[2], f.proj.join("src/a.rs").display().to_string(), "the right side is the working file");

    diff("gone.rs").await.unwrap();
    let plan = f.launcher.plans.lock().last().cloned().unwrap();
    assert_eq!(std::fs::read(&plan.args[1]).unwrap(), b"old\n");
    let right = PathBuf::from(&plan.args[2]);
    assert!(right.starts_with(f.home.join(".gilde/tmp/diff")), "a deletion diffs against an empty temp file");
    assert_eq!(std::fs::read(&right).unwrap(), b"");
    assert_eq!(std::fs::metadata(&right).unwrap().permissions().mode() & 0o777, 0o400);

    diff("fresh.rs").await.unwrap();
    let plan = f.launcher.plans.lock().last().cloned().unwrap();
    assert_eq!(std::fs::read(&plan.args[1]).unwrap(), b"", "a new file has an empty base");

    let e = diff("never-existed.rs").await.unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound);
    let e = diff("../outside").await.unwrap_err();
    assert!(matches!(e.code, ErrorCode::NotFound | ErrorCode::Refused), "{e:?}");
    // Quit sweeps every temp dir.
    let removed = gilde_code_lib::ide::opener::sweep_diff_dir(&f.home.join(".gilde/tmp/diff"), Duration::ZERO);
    assert!(removed >= 4, "{removed}");
}

// ---------------------------------------------------------------------------
// reveal and links (fakes only)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reveal_targets_resolve_to_existing_paths() {
    let f = fx();
    let svc = f.service();
    let t = f.home.join("t.jsonl");
    std::fs::write(&t, b"{}\n").unwrap();
    f.places.transcripts.lock().insert(f.uid.clone(), (AbsPath::parse(t.to_str().unwrap()).unwrap(), Vendor::Claude));
    svc.reveal(&IdeRevealReq { target: RevealTarget::Transcript { uid: f.uid.clone() } }).await.unwrap();
    svc.reveal(&IdeRevealReq { target: RevealTarget::Location { uid: f.uid.clone() } }).await.unwrap();
    assert_eq!(f.revealer.0.lock().as_slice(), [t.clone(), f.proj.clone()]);
    let e = svc.reveal(&IdeRevealReq { target: RevealTarget::Transcript { uid: SessionUid::mint() } }).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound);
    let e = svc.reveal(&IdeRevealReq { target: RevealTarget::ProjectRoot { project_id: ProjectId::derive("/nope") } }).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound);
}

#[tokio::test]
async fn link_open_confirms_local_addresses_natively() {
    let f = fx();
    let no = Arc::new(Answer(false, Default::default()));
    let svc = f.service_with(no.clone());
    assert!(!svc.link_open("http://192.168.0.10:8000/").await.unwrap());
    assert!(f.opener.0.lock().is_empty(), "declined: nothing opened");
    assert_eq!(no.1.lock().as_slice(), ["192.168.0.10:8000".to_string()]);
    assert!(svc.link_open("https://example.com/docs").await.unwrap());
    assert_eq!(f.opener.0.lock().as_slice(), ["https://example.com/docs".to_string()]);
    let yes = Arc::new(Answer(true, Default::default()));
    let svc = f.service_with(yes);
    assert!(svc.link_open("http://localhost:3000/").await.unwrap());
}

// ---------------------------------------------------------------------------
// the real launcher, against harmless programs and a fake supervisor
// ---------------------------------------------------------------------------

struct FakeSupervisor {
    refuse: bool,
    planned: parking_lot::Mutex<Vec<gilde_code_lib::contract::services::SpawnRequest>>,
    exited: parking_lot::Mutex<Vec<gilde_code_lib::contract::activity::ExitInfo>>,
}

#[async_trait]
impl SupervisorApi for FakeSupervisor {
    async fn spawn_gate(&self) -> Result<(), IpcError> {
        Ok(())
    }
    async fn plan_spawn(&self, req: gilde_code_lib::contract::services::SpawnRequest) -> Result<gilde_code_lib::contract::services::SpawnTicket, IpcError> {
        if self.refuse {
            return Err(IpcError::refused("notAllowlisted"));
        }
        self.planned.lock().push(req);
        Ok(gilde_code_lib::contract::services::SpawnTicket { spawn_id: "sp-ide".into(), env_marker: "sp-ide".into() })
    }
    async fn mark_running(&self, _t: &gilde_code_lib::contract::services::SpawnTicket, _pid: i32) -> Result<(), IpcError> {
        Ok(())
    }
    async fn mark_exited(&self, _t: &gilde_code_lib::contract::services::SpawnTicket, exit: gilde_code_lib::contract::activity::ExitInfo) {
        self.exited.lock().push(exit);
    }
    async fn signal(&self, _k: &gilde_code_lib::contract::ids::ProcessKey, _p: gilde_code_lib::contract::services::SignalPlan) -> Result<gilde_code_lib::contract::services::SignalOutcome, IpcError> {
        Ok(gilde_code_lib::contract::services::SignalOutcome { exited: true, survivor: false })
    }
    fn is_vendor_cli(&self, _e: &str) -> bool {
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

fn sup(refuse: bool) -> Arc<FakeSupervisor> {
    Arc::new(FakeSupervisor { refuse, planned: Default::default(), exited: Default::default() })
}

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::canonicalize(p).unwrap()
}

#[tokio::test]
async fn the_supervised_launcher_registers_first_and_passes_an_env_allowlist() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("env.txt");
    let cli = script(tmp.path(), "fake-cli", &format!("env > '{}'; echo \"$@\" >> '{}'", out.display(), out.display()));
    let s = sup(false);
    let launcher = SupervisedLauncher::new(s.clone());
    launcher.launch(&LaunchPlan { argv0: cli.clone(), args: vec!["--goto".into(), "/w/a.rs:3".into()] }).await.unwrap();
    let planned = s.planned.lock().clone();
    assert_eq!(planned.len(), 1);
    assert_eq!(planned[0].kind, gilde_code_lib::contract::process::ProcKind::Ide);
    assert_eq!(planned[0].argv0.as_str(), cli.to_str().unwrap());
    assert!(planned[0].env_keys.contains(&"GILDE_SPAWN_ID".to_string()));
    assert_eq!(s.exited.lock()[0].code, Some(0));
    let text = std::fs::read_to_string(&out).unwrap();
    assert!(text.contains("GILDE_SPAWN_ID=sp-ide"), "{text}");
    assert!(text.contains("--goto /w/a.rs:3"), "argv, not a shell string: {text}");
    for line in text.lines().filter(|l| l.contains('=') && !l.starts_with("--")) {
        let key = line.split('=').next().unwrap();
        assert!(
            ["PATH", "HOME", "USER", "LOGNAME", "TMPDIR", "LANG", "LC_ALL", "GILDE_SPAWN_ID", "PWD", "SHLVL", "_", "OLDPWD", "__CF_USER_TEXT_ENCODING"].contains(&key),
            "env leaked {key}"
        );
    }
}

#[tokio::test]
async fn a_refused_launch_runs_nothing_and_failures_are_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let mark = tmp.path().join("ran");
    let cli = script(tmp.path(), "mark", &format!("touch '{}'", mark.display()));
    let e = SupervisedLauncher::new(sup(true)).launch(&LaunchPlan { argv0: cli.clone(), args: vec![] }).await.unwrap_err();
    assert_eq!(e.params["reason"], "notAllowlisted");
    assert!(!mark.exists(), "refused → nothing ran");

    let failing = script(tmp.path(), "fail", "echo nope >&2; exit 3");
    let s = sup(false);
    let e = SupervisedLauncher::new(s.clone()).launch(&LaunchPlan { argv0: failing, args: vec![] }).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::SpawnFailed);
    assert_eq!(e.detail.as_deref(), Some("nope"));
    assert_eq!(s.exited.lock()[0].code, Some(3));

    let slow = script(tmp.path(), "slow", "exec sleep 30");
    let s = sup(false);
    let e = SupervisedLauncher::new(s.clone())
        .with_timeout(Duration::from_millis(300))
        .launch(&LaunchPlan { argv0: slow, args: vec![] })
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::Timeout);
    assert_eq!(s.exited.lock()[0].signal, Some(libc::SIGTERM), "stopped with SIGTERM, recorded");
}

// ---------------------------------------------------------------------------
// project_pick
// ---------------------------------------------------------------------------

struct Picker(Option<PathBuf>);
#[async_trait]
impl FolderPicker for Picker {
    async fn pick(&self) -> Option<PathBuf> {
        self.0.clone()
    }
}

fn pick_state(f: &Fx, choice: Option<PathBuf>) -> ProjectState {
    ProjectState {
        picker: Arc::new(Picker(choice)),
        picked: f.picked.clone(),
        places: Some(f.places.clone()),
        home: f.home.clone(),
    }
}

#[tokio::test]
async fn project_pick_identifies_folders_and_repositories_and_remembers_the_root() {
    let f = fx();
    assert!(pick_project(&pick_state(&f, None)).await.unwrap().project.is_none(), "cancelled");

    let folder = f.home.join("notes");
    std::fs::create_dir_all(&folder).unwrap();
    let p = pick_project(&pick_state(&f, Some(folder.clone()))).await.unwrap().project.unwrap();
    assert_eq!(p.kind, ProjectKind::Folder);
    assert_eq!(p.name, "notes");
    assert_eq!(p.id, ProjectId::derive(folder.to_str().unwrap()));
    assert!(f.picked.list().contains(&folder));
    assert_eq!(f.places.seen.lock().len(), 1, "World is told");

    git_ok(&f.proj, &["init", "-q", "-b", "main", "."]);
    let p = pick_project(&pick_state(&f, Some(f.proj.join("src")))).await.unwrap().project.unwrap();
    assert_eq!(p.kind, ProjectKind::Git);
    assert_eq!(p.root.as_str(), f.proj.to_str().unwrap(), "a subfolder picks its repository");
    assert_eq!(p.id, gilde_code_lib::discovery::location::identify(&f.proj, &f.home).unwrap().project_id);

    // When World already shows the project, its row is the answer.
    let mut known = p.clone();
    known.order = 7;
    known.name = "as World shows it".into();
    f.places.projects.lock().push(known);
    let p = pick_project(&pick_state(&f, Some(f.proj.clone()))).await.unwrap().project.unwrap();
    assert_eq!((p.order, p.name.as_str()), (7, "as World shows it"));

    let file = f.proj.join("src/a.rs");
    let e = pick_project(&pick_state(&f, Some(file))).await.unwrap_err();
    assert_eq!(e.params["reason"], "notADirectory");
    let e = pick_project(&pick_state(&f, Some(f.home.join("missing")))).await.unwrap_err();
    assert_eq!(e.params["reason"], "notADirectory");
    assert_eq!(gilde_code_lib::commands::project::denied_category(&f.home.join("Documents/x"), &f.home), "documents");
}

// ---------------------------------------------------------------------------
// through the IPC layer (T-ipc-1 cases: link_open [F-39], ide_open, project_pick)
// ---------------------------------------------------------------------------

mod ipc {
    use super::*;
    use gilde_code_lib::commands::ide::IdeState;
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
    fn link_open_ide_open_and_project_pick_through_the_commands() {
        let f = fx();
        let app = mock_builder()
            .invoke_handler(registry::handler())
            .build(tauri::generate_context!(test = true))
            .unwrap();
        let svc: Arc<dyn IdeApi> = Arc::new(f.service());
        app.manage(IdeState::new(svc, f.picked.clone()));
        app.manage(pick_state(&f, Some(f.home.join("work"))));
        let w = tauri::WebviewWindowBuilder::new(&app, "main", Default::default()).build().unwrap();

        // link_open: local addresses need the (declining) native confirmation.
        assert_eq!(invoke(&w, "link_open", json!({"url": "http://127.0.0.1:9/"})).unwrap(), json!({"opened": false}));
        assert_eq!(invoke(&w, "link_open", json!({"url": "https://example.com/"})).unwrap(), json!({"opened": true}));
        for (url, rule) in [("javascript:alert(1)", "url"), ("https://u:p@example.com/", "url"), ("", "required")] {
            let e = invoke(&w, "link_open", json!({"url": url})).unwrap_err();
            assert_eq!((e["params"]["field"].as_str(), e["params"]["rule"].as_str()), (Some("url"), Some(rule)), "{url}");
        }
        let long = format!("https://example.com/{}", "a".repeat(2048));
        assert_eq!(invoke(&w, "link_open", json!({"url": long})).unwrap_err()["params"]["rule"], "url");
        assert_eq!(f.opener.0.lock().as_slice(), ["https://example.com/".to_string()]);

        // ide_open: the §9.4 refusal and the value rules.
        let e = invoke(&w, "ide_open", json!({"target": {"kind": "file", "uid": f.uid.as_str(), "path": "/etc/hosts", "line": null}})).unwrap_err();
        assert_eq!((e["code"].as_str(), e["params"]["reason"].as_str()), (Some("REFUSED"), Some("outsideRoots")));
        let e = invoke(&w, "ide_open", json!({"target": {"kind": "file", "uid": f.uid.as_str(), "path": "a", "line": 0}})).unwrap_err();
        assert_eq!(e["params"]["rule"], "range");
        assert!(invoke(&w, "ide_open", json!({"target": {"kind": "file", "uid": f.uid.as_str(), "path": "src/a.rs", "line": 1}})).is_ok());
        assert_eq!(invoke(&w, "ide_editors", json!({})).unwrap()["chosen"], "vscode");

        // project_pick through the command.
        let p = invoke(&w, "project_pick", json!({})).unwrap();
        assert_eq!(p["project"]["kind"], "folder");
        assert_eq!(p["project"]["name"], "work");
    }
}

// ---------------------------------------------------------------------------
// manual only: these open real apps and windows (the lead runs them by hand)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "opens the installed VS Code with a temp file; run by hand only"]
async fn manual_open_a_temp_file_in_the_installed_vscode() {
    let found = editors::scan(&editors::default_dirs(&PathBuf::from(std::env::var("HOME").unwrap())), &editors::InfoPlistReader);
    let vscode = found.into_iter().find(|f| f.editor == EditorKind::VsCode).expect("VS Code installed");
    let tmp = tempfile::tempdir().unwrap();
    let file = std::fs::canonicalize(tmp.path()).unwrap().join("gilde-manual.txt");
    std::fs::write(&file, b"opened by Gilde's ignored test\n").unwrap();
    let plan = gilde_code_lib::ide::opener::plan(&vscode, &gilde_code_lib::ide::opener::OpenAction::File { path: file, line: Some(1) }).unwrap();
    SupervisedLauncher::new(sup(false)).launch(&plan).await.unwrap();
}

#[test]
#[ignore = "opens a Finder window; run by hand only"]
fn manual_reveal_a_temp_dir_in_finder() {
    let tmp = tempfile::tempdir().unwrap();
    gilde_code_lib::ide::reveal::FinderRevealer.reveal(tmp.path()).unwrap();
}

#[test]
#[ignore = "opens the default browser; run by hand only"]
fn manual_open_a_link_in_the_browser() {
    gilde_code_lib::ide::links::WorkspaceOpener.open("https://example.com/").unwrap();
}
