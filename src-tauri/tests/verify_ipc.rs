//! G6 probes over the real IPC (contract §14.5, A31.3): SP-3 `ide_open`
//! roots, SP-5 settings classes, HP-4 two-writer attempts. Each probe drives
//! the integrated core — the composition root with fakes only, Tauri's mock
//! runtime with the shipped ACL — and prints one `PROBE <id>: …` line.
//!
//! Owner: T33.

mod fixtures;
#[path = "fixtures/verify/mod.rs"]
mod verify;

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tauri::Manager as _;

use gilde_code_lib::commands::ide::IdeState;
use gilde_code_lib::contract::common::{AbsPath, Vendor};
use gilde_code_lib::contract::ids::{ProcessKey, SessionUid};
use gilde_code_lib::contract::ipc::changes::EditorKind;
use gilde_code_lib::contract::services::{
    EvidenceUpdate, IdeApi, IdentityHint, IdentityKey, SessionMetaFacts, SourceFacts, WorldApi,
};
use gilde_code_lib::contract::activity::ProcessFacts;
use gilde_code_lib::contract::IpcError;
use gilde_code_lib::discovery::procs::ProcSource;
use gilde_code_lib::ide::editors::{self, BundleReader, VSCODE_BUNDLE_ID};
use gilde_code_lib::ide::links::{LinkConfirm, UrlOpener};
use gilde_code_lib::ide::opener::{LaunchPlan, Launcher};
use gilde_code_lib::ide::paths::PickedRoots;
use gilde_code_lib::ide::reveal::Revealer;
use gilde_code_lib::ide::{IdeService, Platform};
use gilde_code_lib::world::{MemoryMetaStore, World, WorldConfig};
use gilde_code_lib::worktree::WorldPlaces;

use verify::{bare_app, invoke, short, with_wired, Probe};

// ---------------------------------------------------------------------------
// SP-3 — `ide_open` never leaves the session's roots
// ---------------------------------------------------------------------------

#[derive(Default)]
struct RecordingLauncher(parking_lot::Mutex<Vec<LaunchPlan>>);

#[async_trait]
impl Launcher for RecordingLauncher {
    async fn launch(&self, plan: &LaunchPlan) -> Result<(), IpcError> {
        self.0.lock().push(plan.clone());
        Ok(())
    }
}

struct NoReveal;
impl Revealer for NoReveal {
    fn reveal(&self, _path: &Path) -> Result<(), IpcError> {
        Ok(())
    }
}

struct NoUrl;
impl UrlOpener for NoUrl {
    fn open(&self, _url: &str) -> Result<(), IpcError> {
        Ok(())
    }
}

struct Decline;
#[async_trait]
impl LinkConfirm for Decline {
    async fn confirm_local(&self, _host: &str) -> bool {
        false
    }
}

/// A fixture editor bundle: the id in `Contents/id` (no plist, no AppKit).
struct FixtureBundles;
impl BundleReader for FixtureBundles {
    fn bundle_id(&self, app: &Path) -> Option<String> {
        std::fs::read_to_string(app.join("Contents/id")).ok().map(|s| s.trim().to_string())
    }
}

fn fixture_editor(apps: &Path) {
    let app = apps.join("Visual Studio Code.app");
    std::fs::create_dir_all(app.join("Contents")).unwrap();
    std::fs::write(app.join("Contents/id"), VSCODE_BUNDLE_ID).unwrap();
    let cli = app.join(editors::cli_relative(EditorKind::VsCode));
    std::fs::create_dir_all(cli.parent().unwrap()).unwrap();
    std::fs::write(&cli, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A real World with one Claude session whose cwd is `cwd`.
fn world_with_session(home: &Path, cwd: &Path) -> (World, SessionUid) {
    let world = World::new(WorldConfig::system(Some(home.to_string_lossy().into_owned()), Arc::new(MemoryMetaStore::default())));
    let account = gilde_code_lib::contract::ids::AccountId::derive(Vendor::Claude, &home.join(".claude").to_string_lossy());
    let vsid = "33a0c0de-0000-4000-8000-000000000001";
    let hint = IdentityHint {
        vendor: Some(Vendor::Claude),
        account_id: Some(account.clone()),
        vendor_session_id: Some(vsid.into()),
        process: Some(ProcessKey::new(7300, 1_792_700_000)),
        gilde_spawn_id: None,
    };
    let sink = world.sink();
    let at = gilde_code_lib::store::now_ms();
    sink.evidence(EvidenceUpdate {
        hint: hint.clone(),
        facts: SourceFacts::Process(ProcessFacts::Alive { pid: 7300, start_sec: 1_792_700_000, cpu_active_at: None }),
        at,
    });
    sink.evidence(EvidenceUpdate {
        hint,
        facts: SourceFacts::Meta(Box::new(SessionMetaFacts { cwd: Some(AbsPath::parse(cwd.to_str().unwrap()).unwrap()), ..SessionMetaFacts::default() })),
        at,
    });
    let uid = world.uid_resolver().uid_for(&IdentityKey { vendor: Vendor::Claude, account_id: Some(account), vendor_session_id: vsid.into() });
    (world, uid)
}

/// SP-3: `ide_open` (File and Diff, two operations) with `..` escapes, a
/// symlinked file and directory out of the root, `~/.claude` (literal `~`,
/// absolute, and a case variant) → `REFUSED{outsideRoots}`; the launcher
/// never ran for any of them. Also observed: what an escape to a path that
/// does not exist answers.
#[test]
fn probe_sp3_ide_open_stays_inside_the_roots() {
    let mut p = Probe::new("SP-3");
    let tmp = tempfile::tempdir().unwrap();
    let home = std::fs::canonicalize(tmp.path()).unwrap();
    let proj = home.join("work/proj");
    std::fs::create_dir_all(proj.join("src")).unwrap();
    std::fs::write(proj.join("src/a.rs"), b"fn a() {}\n").unwrap();
    std::fs::create_dir_all(home.join("outside")).unwrap();
    std::fs::write(home.join("outside/secret.txt"), b"x").unwrap();
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::write(home.join(".claude/settings.json"), b"{}").unwrap();
    std::os::unix::fs::symlink(home.join("outside/secret.txt"), proj.join("link-out.txt")).unwrap();
    std::os::unix::fs::symlink(home.join("outside"), proj.join("dir-out")).unwrap();
    let apps = home.join("Applications");
    fixture_editor(&apps);

    let (world, uid) = world_with_session(&home, &proj);
    let launcher = Arc::new(RecordingLauncher::default());
    let picked = PickedRoots::new();
    let svc: Arc<dyn IdeApi> = Arc::new(IdeService::new(
        Arc::new(WorldPlaces(Arc::new(world.clone()) as Arc<dyn WorldApi>)),
        home.clone(),
        home.join(".gilde/tmp/diff"),
        picked.clone(),
        Arc::new(|| Some(EditorKind::VsCode)),
        Platform {
            launcher: launcher.clone(),
            revealer: Arc::new(NoReveal),
            url_opener: Arc::new(NoUrl),
            link_confirm: Arc::new(Decline),
            bundles: Arc::new(FixtureBundles),
            app_dirs: vec![apps],
        },
    ));
    let (app, w) = bare_app();
    app.manage(IdeState::new(svc, picked));
    let deep = format!("{}etc/passwd", "../".repeat(proj.components().count() + 2));
    let open_once = |kind: &str, path: &str| {
        let target = if kind == "file" {
            json!({"kind": "file", "uid": uid.as_str(), "path": path, "line": null})
        } else {
            json!({"kind": "diff", "uid": uid.as_str(), "path": path})
        };
        invoke(&w, "ide_open", json!({ "target": target }))
    };
    // A45.3: `ide_open` is class `default` (10/s per command) and this probe
    // sends more than ten in a burst: on BUSY, wait `retryAfterMs` and send
    // it once more (the class is never relaxed for a test).
    let open = |kind: &str, path: &str| {
        let r = open_once(kind, path);
        match r.as_ref().err().filter(|e| e["code"] == "BUSY") {
            Some(e) => {
                let ms = e["params"]["retryAfterMs"].as_u64().expect("retryAfterMs");
                std::thread::sleep(std::time::Duration::from_millis(ms));
                open_once(kind, path)
            }
            None => r,
        }
    };
    let refused = |r: &Result<Value, Value>| {
        r.as_ref().err().is_some_and(|e| e["code"] == "REFUSED" && e["params"]["reason"] == "outsideRoots")
    };
    // Each must be REFUSED{outsideRoots} (the target exists, or its parent does).
    let must_refuse: Vec<(&str, String)> = vec![
        ("file", deep.clone()),
        ("file", "../../outside/secret.txt".into()),
        ("file", home.join("outside/secret.txt").to_string_lossy().into_owned()),
        ("file", "link-out.txt".into()),
        ("file", "dir-out/secret.txt".into()),
        ("file", home.join(".claude/settings.json").to_string_lossy().into_owned()),
        ("file", home.join(".CLAUDE/settings.json").to_string_lossy().into_owned()),
        ("file", "../PROJ/../../.claude/settings.json".into()),
        ("diff", "../../outside/secret.txt".into()),
        ("diff", "../../outside/missing.txt".into()),
        ("diff", "dir-out/secret.txt".into()),
    ];
    for (kind, path) in &must_refuse {
        let r = open(kind, path);
        let shown = path.replace(home.to_str().unwrap(), "<home>");
        assert!(refused(&r), "ide_open {kind} {shown}: {r:?}");
        p.saw(format!("{kind}('{shown}')"), short(&r));
    }
    // A33.1: the roots check is lexical first, so an escape is refused
    // whether or not its target exists (no existence oracle) …
    for (kind, path) in [
        ("file", "../../etc/passwd"),
        ("file", "../../outside/missing.txt"),
        ("file", "../../../no/such/dir/at/all"),
        ("diff", "../../etc/passwd"),
    ] {
        let r = open(kind, path);
        assert!(refused(&r), "ide_open {kind} {path}: {r:?}");
        p.saw(format!("{kind}('{path}') (target missing)"), short(&r));
    }
    let existing = open("file", "../../outside/secret.txt");
    let missing = open("file", "../../outside/missing.txt");
    assert_eq!(existing, missing, "an existing and a missing outside file answer the same");
    p.saw("outside existing vs missing", format!("both {} (no existence oracle)", short(&existing)));
    // … while an inside name that does not exist — a literal `~` is an
    // ordinary name, never expanded — is NOT_FOUND{file}.
    for path in ["~/.claude/settings.json", "src/missing.rs"] {
        let r = open("file", path);
        let e = r.as_ref().expect_err(path);
        assert_eq!((e["code"].as_str(), e["params"]["entity"].as_str()), (Some("NOT_FOUND"), Some("file")), "{path}: {e}");
        p.saw(format!("file('{path}') (inside, missing)"), short(&r));
    }
    assert!(launcher.0.lock().is_empty(), "the launcher never ran for a refused path");
    p.saw("launcher", "not called");
    // Control: a file inside the root opens.
    assert!(open("file", "src/a.rs").is_ok());
    assert_eq!(launcher.0.lock().len(), 1);
    p.saw("file('src/a.rs') control", "OK (launched once)");
    // A45.3: and the limiter is real — a burst over the class is cut with
    // BUSY{retryAfterMs ≤ 100} (refused paths, so nothing is launched).
    let mut sent = 0;
    let busy = loop {
        sent += 1;
        let r = open_once("file", "../../etc/passwd");
        if let Some(e) = r.as_ref().err().filter(|e| e["code"] == "BUSY") {
            break e["params"]["retryAfterMs"].as_u64().expect("retryAfterMs");
        }
        assert!(refused(&r), "burst {sent}: {r:?}");
        assert!(sent < 1_000, "1000 ide_open calls and none BUSY: the default class is not enforced");
    };
    assert!((1..=100).contains(&busy), "retryAfterMs {busy}");
    assert_eq!(launcher.0.lock().len(), 1, "the burst launched nothing");
    p.saw(format!("burst of {sent} ide_open"), format!("BUSY{{retryAfterMs:{busy}}}"));
    p.finish();
}

// ---------------------------------------------------------------------------
// SP-5 — executable settings never through `settings_set`; reserved env names
// ---------------------------------------------------------------------------

/// SP-5: `settings_set` with `agents.claude.binary` (flat key and nested
/// object, two positions) and `settings_set_executable` with reserved
/// `envPassthrough` names (`TMUX` first, `TMUX_PANE` second) →
/// `INVALID_ARGUMENT`; the config file is unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_sp5_settings_classes() {
    with_wired(
        |_| {},
        |c| async move {
            let mut p = Probe::new("SP-5");
            let config = c.core.paths.config_file();
            let before = std::fs::read(&config).ok();
            let cases: Vec<(&str, &str, Value, &str, &str)> = vec![
                (
                    "settings_set{patch.\"agents.claude.binary\"}",
                    "settings_set",
                    json!({"patch": {"agents.claude.binary": "/bin/sh"}}),
                    "$",
                    "shape",
                ),
                (
                    "settings_set{patch.agents.claude.binary}",
                    "settings_set",
                    json!({"patch": {"theme": null, "agents": {"claude": {"binary": "/bin/sh"}}}}),
                    "$",
                    "shape",
                ),
                (
                    "settings_set_executable{envPassthrough:[TMUX]}",
                    "settings_set_executable",
                    json!({"key": "agents.envPassthrough", "value": {"kind": "envNames", "names": ["TMUX"]}}),
                    "value.names[0]",
                    "reserved",
                ),
                (
                    "settings_set_executable{envPassthrough:[PATH_X,TMUX_PANE]}",
                    "settings_set_executable",
                    json!({"key": "agents.envPassthrough", "value": {"kind": "envNames", "names": ["PATH_X", "TMUX_PANE"]}}),
                    "value.names[1]",
                    "reserved",
                ),
            ];
            for (variant, cmd, req, field, rule) in cases {
                let r = c.call(cmd, req);
                let e = r.as_ref().expect_err(variant);
                assert_eq!(e["code"], "INVALID_ARGUMENT", "{variant}: {e}");
                assert_eq!((e["params"]["field"].as_str(), e["params"]["rule"].as_str()), (Some(field), Some(rule)), "{variant}: {e}");
                p.saw(variant, short(&r));
            }
            assert_eq!(std::fs::read(&config).ok(), before, "the config file is unchanged");
            p.saw("config file", "unchanged");
            p.finish();
        },
    )
    .await;
}

// ---------------------------------------------------------------------------
// HP-4 — a session another program runs has exactly one writer
// ---------------------------------------------------------------------------

const VSCODE_PID: i32 = 47_101;
const VSCODE_START: i64 = 1_792_700_100;
const VSCODE_SID: &str = "4400c0de-0000-4000-8000-000000000044";

/// `procStart` as the CLI writes it (UTC ctime, space-padded day).
fn ctime_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    let wd = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"][days.rem_euclid(7) as usize];
    let mon = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"][m as usize - 1];
    format!("{wd} {mon} {d:>2} {:02}:{:02}:{:02} {y}", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// A VS Code Claude session in the hermetic home: its registry file (status
/// `waiting` on a permission prompt) and a transcript whose last row is an
/// open `Bash` tool call (the watched approval card, §3.5.4).
fn vscode_session(home: &Path) -> PathBuf {
    let cfg = home.join(".claude");
    let cwd = home.join("work/vsproj");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(cfg.join("sessions")).unwrap();
    let started_ms = VSCODE_START * 1000 + 400;
    let now = gilde_code_lib::store::now_ms();
    let registry = json!({
        "pid": VSCODE_PID, "sessionId": VSCODE_SID, "cwd": cwd.to_str().unwrap(), "startedAt": started_ms,
        "procStart": ctime_utc(VSCODE_START), "version": "2.1.307", "peerProtocol": 1, "peerFeatures": ["notify_idle"],
        "kind": "interactive", "entrypoint": "claude-vscode", "pidDomain": "darwin",
        "messagingSocketPath": format!("/tmp/cc-socks/{VSCODE_PID}.sock"), "name": "probe", "nameSource": "derived",
        "nameSince": started_ms, "updatedAt": now, "status": "waiting", "waitingFor": "permission prompt", "statusUpdatedAt": now
    });
    std::fs::write(cfg.join(format!("sessions/{VSCODE_PID}.json")), registry.to_string()).unwrap();
    let slug = cwd.to_str().unwrap().replace(['/', '.'], "-");
    let dir = cfg.join("projects").join(slug);
    std::fs::create_dir_all(&dir).unwrap();
    let ts = |ms: i64| {
        let secs = ms.div_euclid(1000);
        // RFC 3339 from epoch ms via the same civil algorithm.
        let days = secs.div_euclid(86_400);
        let rem = secs.rem_euclid(86_400);
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = yoe + era * 400 + i64::from(m <= 2);
        format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z", rem / 3600, rem / 60 % 60, rem % 60, ms.rem_euclid(1000))
    };
    let rows = [
        json!({"type": "user", "uuid": "u-1", "timestamp": ts(now - 20_000), "sessionId": VSCODE_SID,
               "message": {"role": "user", "content": "run the tests"}}),
        json!({"type": "assistant", "uuid": "a-1", "timestamp": ts(now - 10_000), "sessionId": VSCODE_SID,
               "message": {"id": "msg_1", "role": "assistant", "content": [{"type": "tool_use", "id": "toolu_hp4", "name": "Bash",
                           "input": {"command": "cargo test", "description": "Run the tests"}}],
                           "usage": {"input_tokens": 1, "output_tokens": 1}}}),
    ];
    let body: String = rows.iter().map(|r| format!("{r}\n")).collect();
    std::fs::write(dir.join(format!("{VSCODE_SID}.jsonl")), body).unwrap();
    cwd
}

fn vscode_proc(cwd: &Path) -> gilde_code_lib::discovery::procs::FakeProc {
    gilde_code_lib::discovery::procs::FakeProc {
        ppid: 1,
        // SAFETY: `getuid` has no failure mode.
        uid: unsafe { libc::getuid() },
        start_sec: VSCODE_START,
        comm: "claude".into(),
        exec_path: Some("/opt/fake/bin/claude.exe".into()),
        argv: Some(vec!["claude".into()]),
        env_names: Some(vec!["PATH".into(), "HOME".into()]),
        cwd: Some(cwd.to_string_lossy().into_owned()),
        rss_bytes: 50_000_000,
        ..gilde_code_lib::discovery::procs::FakeProc::default()
    }
}

/// HP-4: a session live in VS Code (entrypoint `claude-vscode`, live in the
/// process table) has one writer — its owner. `session_continue{Resume}` →
/// `SESSION_LIVE_ELSEWHERE`; `turn_send` (two priorities) → `UNSUPPORTED`;
/// `pty_write` (it has no PTY) → `UNSUPPORTED`; `interaction_answer` on its
/// watched card (allow and deny) → `REFUSED{watching}`; nothing is spawned or
/// written.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_hp4_two_writers_are_refused() {
    with_wired(
        |home| {
            vscode_session(home);
        },
        |c| async move {
            let mut p = Probe::new("HP-4");
            let cwd = c.home.join("work/vsproj");
            c.procs.insert(VSCODE_PID, vscode_proc(&cwd));
            // The first reconcile ran before the process existed (stale); a
            // registry write is what the CLI does next anyway.
            vscode_session(&c.home);
            let mut pids_before = c.procs.list_pids();
            pids_before.sort();
            let world = c.world();
            let key = IdentityKey {
                vendor: Vendor::Claude,
                account_id: Some(gilde_code_lib::contract::ids::AccountId::derive(Vendor::Claude, &c.home.join(".claude").to_string_lossy())),
                vendor_session_id: VSCODE_SID.into(),
            };
            let uid = world.uid_resolver().uid_for(&key);
            // Let discovery see the registry, the process and the transcript.
            let mut session = None;
            for _ in 0..200 {
                c.wired.discovery_handle().wake();
                let d = c.wired.discovery.clone();
                tokio::task::spawn_blocking(move || d.read().as_ref().map(|d| d.sync(std::time::Duration::from_secs(10)))).await.unwrap();
                world.flush_now();
                session = world.session(&uid).filter(|s| matches!(s.control, gilde_code_lib::contract::session::Control::Watching { .. }));
                if session.as_ref().is_some_and(|s| !s.pending.is_empty()) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            let s = session.expect("the VS Code session is Watching");
            let control = serde_json::to_value(&s.control).unwrap();
            assert_eq!(control["kind"], "watching", "{control}");
            p.saw("session", format!("Watching{{origin:{}}}", control["origin"].as_str().unwrap_or("?")));

            let r = c.call("session_continue", json!({"clientRequestId": "0192f000-0000-4000-8000-0000000000a1", "uid": uid.as_str(), "how": {"kind": "resume"}, "controlMode": null, "model": null, "permissionMode": null}));
            if r.as_ref().err().is_some_and(|e| e["code"] == "SESSION_LIVE_ELSEWHERE") {
                p.saw("session_continue{resume}", short(&r));
            } else {
                assert!(r.is_err(), "never resumed in place: {r:?}");
                p.mismatch("session_continue{resume}", short(&r), "SESSION_LIVE_ELSEWHERE");
            }
            for (i, priority) in ["next", "now"].iter().enumerate() {
                let r = c.call("turn_send", json!({"uid": uid.as_str(), "clientMsgId": format!("0192f000-0000-4000-8000-00000000000{i}"), "text": "hello", "priority": priority}));
                let e = r.as_ref().expect_err("turn_send refused");
                assert_eq!(e["code"], "UNSUPPORTED", "{e}");
                p.saw(format!("turn_send{{priority:{priority}}}"), short(&r));
            }
            let r = c.call("pty_write", json!({"ptyId": "y_0192f000-0000-7000-8000-0000000000b1", "b64": "aGk="}));
            assert_eq!(r.as_ref().expect_err("pty_write refused")["code"], "UNSUPPORTED");
            p.saw("pty_write(the row has no pty)", short(&r));
            let request_id = s.pending.first().map(|i| i.request_id.as_str().to_string());
            let rid = request_id.clone().unwrap_or_else(|| "wa_0000000000000000".into());
            for decision in [json!({"kind": "allowOnce"}), json!({"kind": "deny"})] {
                let kind = decision["kind"].as_str().unwrap().to_string();
                let r = c.call("interaction_answer", json!({"uid": uid.as_str(), "requestId": rid, "decision": decision, "via": "button"}));
                let e = r.as_ref().expect_err("interaction_answer refused");
                assert_eq!((e["code"].as_str(), e["params"]["reason"].as_str()), (Some("REFUSED"), Some("watching")), "{e}");
                let card = if request_id.is_some() { "the watched card" } else { "a made-up card id" };
                p.saw(format!("interaction_answer{{{kind}}} on {card}"), short(&r));
            }
            world.flush_now();
            let after = world.session(&uid).expect("still there");
            assert!(matches!(after.control, gilde_code_lib::contract::session::Control::Watching { .. }), "still only watched: {:?}", after.control);
            assert_eq!(c.wired.pty.len(), 0, "no PTY was opened");
            let mut pids_after = c.procs.list_pids();
            pids_after.sort();
            assert_eq!(pids_after, pids_before, "no process was added to the table");
            p.saw("afterwards", "still Watching; no PTY opened; no process started");
            p.finish();
        },
    )
    .await;
}

// ---------------------------------------------------------------------------
// A38.9 (G7-15) — `continue_all{uids: []}` continues nothing
// ---------------------------------------------------------------------------

/// The failed uids of a `continue_all` answer, sorted.
fn failed_uids(r: &Value) -> Vec<String> {
    let mut v: Vec<String> = r["failed"].as_array().unwrap().iter().map(|f| f["uid"].as_str().unwrap().to_string()).collect();
    v.sort();
    v
}

/// A38.9 (§6.5): `continue_all{uids: []}` continues nothing; a list
/// continues only its items; only `null` means every item. The record's
/// sessions are unknown to World, so an attempted item fails with
/// `NOT_FOUND{session}` before any spawn — what was attempted is visible
/// without starting anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn continue_all_with_an_empty_list_continues_nothing() {
    with_wired(
        |_| {},
        |c| async move {
            use gilde_code_lib::store::dao::quit_record;
            use gilde_code_lib::store::models::{QuitItemRow, QuitRecordRow};
            let rid = gilde_code_lib::contract::ids::QuitRecordId::mint();
            let (a, b) = (SessionUid::mint(), SessionUid::mint());
            let record = QuitRecordRow { id: rid.as_str().into(), cause: "quit".into(), created_at: gilde_code_lib::store::now_ms(), resolved_at: None };
            let item = |u: &SessionUid| QuitItemRow {
                uid: u.as_str().into(),
                control_mode: "conversation".into(),
                permission_mode: None,
                model: None,
                was_waiting: None,
                team_lead: false,
                survivor: false,
            };
            let items = vec![item(&a), item(&b)];
            c.core.db.write(move |conn| quit_record::insert(conn, &record, &items)).await.expect("the quit record");
            let call = |uids: Value| c.call("continue_all", json!({"recordId": rid.as_str(), "uids": uids})).expect("continue_all answers");

            let r = call(json!([]));
            assert_eq!(r, json!({"started": [], "failed": []}), "an empty list continues nothing");
            let r = call(json!([a.as_str()]));
            assert_eq!(failed_uids(&r), vec![a.as_str().to_string()], "a list continues only its items: {r}");
            assert_eq!(r["failed"][0]["error"]["code"], "NOT_FOUND", "{r}");
            let r = call(Value::Null);
            let mut both = vec![a.as_str().to_string(), b.as_str().to_string()];
            both.sort();
            assert_eq!(failed_uids(&r), both, "null continues every item: {r}");
            let left = c.core.db.read({
                let id = rid.as_str().to_string();
                move |conn| quit_record::items(conn, &id)
            });
            assert_eq!(left.await.expect("items").len(), 2, "nothing was continued, so no item was removed");
        },
    )
    .await;
}
