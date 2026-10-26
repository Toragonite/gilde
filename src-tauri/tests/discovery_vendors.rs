//! Codex, cursor and agy readers (contract §5.3–§5.5, §5.7, §3.2):
//! T-disc-codex, T-disc-resumeIds, the SQLite rule against WAL databases, and
//! "cursor `store.db` is never opened".

#[path = "fixtures/vendor/support.rs"]
mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

use gilde_code_lib::contract::activity::ProcessFacts;
use gilde_code_lib::contract::common::Vendor;
use gilde_code_lib::contract::services::SourceFacts;
use gilde_code_lib::contract::session::Origin;
use gilde_code_lib::discovery::history::{HistoryService, HistorySources, MemoryUids};
use gilde_code_lib::discovery::procs::{FakeBundles, FakeProc, FakeProcs};
use gilde_code_lib::discovery::safe_read::{self, Root};
use gilde_code_lib::discovery::{agy, codex, cursor, Engine};
use support::*;

fn codex_home(h: &Home, cwd: &Path) -> (std::path::PathBuf, String) {
    let codex = h.home.join(".codex");
    let day = codex.join("sessions/2026/10/21");
    std::fs::create_dir_all(&day).unwrap();
    let id = uuid(500);
    std::fs::write(day.join(format!("rollout-2026-10-21T13-25-05-{id}.jsonl")), template("codex/rollout.jsonl", &[("SID", &id), ("CWD", cwd.to_str().unwrap())])).unwrap();
    (codex, id)
}

#[test]
fn t_disc_codex_rollouts_index_and_processes() {
    let h = Home::new();
    let cwd = h.workdir("cx");
    let (codex_dir, id) = codex_home(&h, &cwd);
    // Rollout walk + head/tail.
    let sessions = Root::new(&codex_dir.join("sessions")).unwrap();
    let (files, truncated) = codex::list_rollouts(&sessions, 100).unwrap();
    assert!(!truncated);
    assert_eq!(files.len(), 1);
    let (head, last, _) = codex::read_rollout(&sessions, &files[0].path).unwrap();
    let head = head.unwrap();
    assert_eq!(head.id, id);
    assert_eq!(head.source.as_deref(), Some("cli"));
    assert!(last > head.started_at);
    // The thread index with a live -wal (a running codex keeps its writer open).
    let db = codex_dir.join("state_5.sqlite");
    let w = rusqlite::Connection::open(&db).unwrap();
    w.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE threads (id TEXT PRIMARY KEY, cwd TEXT, title TEXT, first_user_message TEXT, created_at_ms INTEGER, updated_at_ms INTEGER, source TEXT, archived INTEGER, preview TEXT); PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
    w.execute("INSERT INTO threads VALUES (?1, ?2, ?3, ?4, 1, 2, 'vscode', 0, ?5)", rusqlite::params![id, cwd.to_str().unwrap(), format!("{CANARY} title"), CANARY, CANARY]).unwrap();
    assert!(Path::new(&format!("{}-wal", db.display())).exists());
    let before = tree_state(&codex_dir);
    let root = Root::new(&codex_dir).unwrap();
    let rows = codex::query_threads(&root, &db, &h.app_data).unwrap();
    assert_eq!(rows.len(), 1, "the WAL-only row is visible through the private copy");
    assert_eq!(tree_state(&codex_dir), before, "nothing written next to the vendor db");
    drop(w);
    // Processes: an app-server child (VsCode, thread unknown), `codex resume <id>` (id from argv).
    let p = procs();
    let mut app = claude_proc(1_792_558_000, 1, &["codex", "-c", "features.code_mode_host=true", "app-server", "--analytics-default-enabled"], &cwd);
    app.exec_path = Some("/Users/u/.vscode/extensions/openai.chatgpt-1-darwin-arm64/bin/macos-aarch64/codex".into());
    p.insert(2000, app);
    let mut resume = claude_proc(1_792_558_000, 1, &["codex", "resume", &id], &cwd);
    resume.exec_path = Some("/opt/homebrew/bin/codex".into());
    p.insert(2001, resume);
    let rec = Arc::new(Rec::default());
    let mut cfg = h.config(p, Arc::new(TeamsRec::default()), None);
    cfg.accounts = vec![gilde_code_lib::discovery::AccountSpec::new(Vendor::Codex, &codex_dir)];
    let mut e = Engine::new(cfg, rec.clone());
    e.reconcile();
    let app_ev = rec.for_pid(2000);
    assert!(app_ev.iter().all(|u| u.hint.vendor_session_id.is_none()), "thread unknown");
    assert!(app_ev.iter().any(|u| matches!(&u.facts, SourceFacts::Meta(m) if m.origin == Some(Origin::VsCode))));
    let res = rec.for_pid(2001);
    assert!(res.iter().any(|u| u.hint.vendor_session_id.as_deref() == Some(id.as_str())));
    assert!(res.iter().any(|u| matches!(u.facts, SourceFacts::Process(ProcessFacts::Alive { .. }))));
}

/// A rollout of `originator` (rollout `source: "vscode"`) in `cwd`, written up to now.
fn app_thread(codex: &Path, n: u32, originator: &str, cwd: &Path) -> String {
    let id = uuid(n);
    let now = gilde_code_lib::discovery::now_wall_ms();
    let day = codex.join("sessions/2026/10/26");
    std::fs::create_dir_all(&day).unwrap();
    let head = serde_json::json!({"timestamp": iso(now - 60_000), "type": "session_meta", "payload": {"id": id, "timestamp": iso(now - 60_000), "cwd": cwd, "originator": originator, "cli_version": "0.153.4", "source": "vscode"}});
    let tail = serde_json::json!({"timestamp": iso(now - 5_000), "type": "event_msg", "payload": {"type": "task_started"}});
    std::fs::write(day.join(format!("rollout-2026-10-26T10-00-00-{id}.jsonl")), format!("{head}\n{tail}\n")).unwrap();
    id
}

/// A codex `app-server` under `host` (pid `pid - 1`, the app or the extension host).
fn app_server(p: &FakeProcs, pid: i32, host_exec: &str, server_exec: &str) {
    let start = gilde_code_lib::discovery::now_wall_ms() / 1000 - 600;
    p.insert(pid - 1, FakeProc { ppid: 1, uid: UID, start_sec: start, comm: "host".into(), exec_path: Some(host_exec.into()), ..FakeProc::default() });
    let mut s = claude_proc(start, pid - 1, &["codex", "-c", "features.code_mode_host=true", "app-server"], Path::new("/"));
    s.exec_path = Some(server_exec.into());
    p.insert(pid, s);
}

/// Codex app-servers: the desktop app's is ChatGptApp, an extension's VsCode;
/// one is tied to a thread only as its app's only server with exactly one
/// thread of that app written since it started, and then takes the thread's
/// cwd unless it is the desktop app's per-chat folder.
#[test]
fn t_disc_codex_app_servers_and_their_threads() {
    let h = Home::new();
    let codex_dir = h.home.join(".codex");
    let chat = h.home.join("Documents/Codex/2026-10-26/new-chat");
    std::fs::create_dir_all(&chat).unwrap();
    let repo = h.workdir("repo");
    let desktop = app_thread(&codex_dir, 900, "codex_work_desktop", &chat);
    let editor = app_thread(&codex_dir, 901, "codex_vscode", &repo);
    let history = HistoryService::new(
        HistorySources {
            claude: vec![],
            codex: vec![(gilde_code_lib::discovery::AccountSpec::new(Vendor::Codex, &codex_dir).id, codex_dir.clone())],
            cursor_chats: None,
            agy_dir: None,
            home: h.home.clone(),
            app_data: h.app_data.clone(),
        },
        Arc::new(MemoryUids::default()),
        None,
    );
    history.refresh_index();
    history.fill();

    let p = procs();
    let vscode_ext = "/Users/u/.vscode/extensions/openai.chatgpt-1-darwin-arm64/bin/macos-aarch64/codex";
    let helper = "/Applications/Visual Studio Code.app/Contents/Frameworks/Code Helper (Plugin).app/Contents/MacOS/Code Helper (Plugin)";
    app_server(&p, 3001, "/Applications/ChatGPT.app/Contents/MacOS/ChatGPT", "/Applications/ChatGPT.app/Contents/Resources/codex");
    app_server(&p, 3101, helper, vscode_ext);
    app_server(&p, 3201, helper, vscode_ext);
    let mut bundles = FakeBundles::default();
    bundles.0.insert("/Applications/ChatGPT.app".into(), ("com.openai.codex".into(), "ChatGPT".into()));
    bundles.0.insert("/Applications/Visual Studio Code.app".into(), ("com.microsoft.VSCode".into(), "Code".into()));
    let rec = Arc::new(Rec::default());
    let mut cfg = h.config(p.clone(), Arc::new(TeamsRec::default()), Some(history));
    cfg.accounts = vec![gilde_code_lib::discovery::AccountSpec::new(Vendor::Codex, &codex_dir)];
    cfg.bundles = Arc::new(bundles);
    let mut e = Engine::new(cfg, rec.clone());
    e.reconcile();

    let meta = |pid: i32| {
        rec.for_pid(pid).into_iter().find_map(|u| match u.facts {
            SourceFacts::Meta(m) => Some((u.hint.vendor_session_id.clone(), *m)),
            _ => None,
        })
    };
    let (sid, m) = meta(3001).expect("the desktop server");
    assert_eq!(m.origin, Some(Origin::ChatGptApp));
    assert_eq!(sid.as_deref(), Some(desktop.as_str()), "the desktop app's only server, its only live thread");
    assert!(m.transcript_path.is_some(), "status comes from the thread's rollout");
    assert_eq!(m.cwd, None, "a per-chat folder is not a project");
    for pid in [3101, 3201] {
        let (sid, m) = meta(pid).expect("an extension server");
        assert_eq!(m.origin, Some(Origin::VsCode));
        assert_eq!(sid, None, "two VS Code servers: which one runs the thread is unknown");
    }

    // One VS Code window left: its server is the thread's.
    p.remove(3201);
    rec.clear();
    e.tick();
    e.reconcile();
    let (sid, m) = meta(3101).expect("the extension server");
    assert_eq!(sid.as_deref(), Some(editor.as_str()));
    assert_eq!(m.cwd.as_ref().map(|c| c.as_str()), Some(repo.to_str().unwrap()), "the thread's cwd is where the work is");
}

/// T-disc-resumeIds (§3.2): cursor ids need `meta.json` under `md5(cwd)`, agy
/// ids need `conversations/<id>.db`; otherwise the process is cwd-only.
#[test]
fn t_disc_resume_ids_checked_on_disk() {
    let h = Home::new();
    let cwd = h.workdir("cu");
    let chats = h.home.join(".cursor/chats");
    let ws = chats.join(cursor::md5_hex(cwd.to_str().unwrap()));
    std::fs::create_dir_all(ws.join("1f2e3d4c-aaaa")).unwrap();
    std::fs::write(ws.join("1f2e3d4c-aaaa/meta.json"), template("cursor/meta.json", &[("CWD", cwd.to_str().unwrap())])).unwrap();
    assert!(cursor::chat_exists(&chats, cwd.to_str().unwrap(), "1f2e3d4c-aaaa"));
    assert!(!cursor::chat_exists(&chats, cwd.to_str().unwrap(), "1f2e3d4c-bbbb"));
    assert!(!cursor::chat_exists(&chats, "/other", "1f2e3d4c-aaaa"));
    let agy_dir = h.home.join(".gemini/antigravity-cli");
    std::fs::create_dir_all(agy_dir.join("conversations")).unwrap();
    std::fs::write(agy_dir.join(format!("conversations/{}.db", uuid(600))), b"").unwrap();
    assert!(agy::conversation_exists(&agy_dir, &uuid(600)));
    assert!(!agy::conversation_exists(&agy_dir, &uuid(601)));
    assert!(!agy::conversation_exists(&agy_dir, "../../x"));

    let p = procs();
    let mut cu = claude_proc(1_792_558_000, 1, &["cursor-agent", "--resume", "1f2e3d4c-aaaa"], &cwd);
    cu.exec_path = Some("/Users/u/.local/share/cursor-agent/versions/1/cursor-agent".into());
    p.insert(3000, cu);
    let mut cu2 = claude_proc(1_792_558_000, 1, &["cursor-agent", "--resume", "1f2e3d4c-bbbb"], &cwd);
    cu2.exec_path = Some("/Users/u/.local/bin/cursor-agent".into());
    p.insert(3001, cu2);
    let mut ag = claude_proc(1_792_558_000, 1, &["agy", "--conversation", &uuid(600)], &cwd);
    ag.exec_path = Some("/opt/homebrew/bin/agy".into());
    p.insert(3002, ag);
    let rec = Arc::new(Rec::default());
    let mut e = Engine::new(h.config(p, Arc::new(TeamsRec::default()), None), rec.clone());
    e.reconcile();
    assert!(rec.for_pid(3000).iter().any(|u| u.hint.vendor_session_id.as_deref() == Some("1f2e3d4c-aaaa")));
    assert!(rec.for_pid(3001).iter().all(|u| u.hint.vendor_session_id.is_none()), "an id not on disk is never trusted");
    assert!(rec.for_pid(3002).iter().any(|u| u.hint.vendor_session_id.as_deref() == Some(uuid(600).as_str())));
}

/// cursor `store.db` (mode 000) is never opened by the chat walk or the reader.
#[test]
fn cursor_store_db_is_never_opened() {
    let h = Home::new();
    let chats = h.home.join(".cursor/chats");
    let ws = chats.join(cursor::md5_hex("/w"));
    std::fs::create_dir_all(ws.join("abcdef12")).unwrap();
    std::fs::write(ws.join("abcdef12/meta.json"), template("cursor/meta.json", &[("CWD", "/w")])).unwrap();
    std::fs::write(ws.join("abcdef12/store.db"), CANARY).unwrap();
    std::fs::set_permissions(ws.join("abcdef12/store.db"), std::fs::Permissions::from_mode(0o000)).unwrap();
    let root = Root::new(&chats).unwrap();
    safe_read::reset_stats();
    let (files, _) = cursor::list_chats(&root, 100).unwrap();
    assert_eq!(files.len(), 1);
    let meta = cursor::read_meta(&root, &files[0]).unwrap().unwrap();
    assert_eq!(meta.cwd.as_deref(), Some("/w"));
    assert_eq!(safe_read::stats().eacces, 0);
}

#[test]
fn agy_summaries_and_last_conversations() {
    let h = Home::new();
    let agy_dir = h.home.join(".gemini/antigravity-cli");
    std::fs::create_dir_all(agy_dir.join("cache")).unwrap();
    std::fs::write(agy_dir.join("cache/last_conversations.json"), template("agy/last_conversations.json", &[("CWD", "/w"), ("SID", &uuid(700)), ("SID2", &uuid(701))])).unwrap();
    std::fs::write(agy_dir.join("oauth_creds.json"), CANARY).unwrap();
    std::fs::set_permissions(agy_dir.join("oauth_creds.json"), std::fs::Permissions::from_mode(0o000)).unwrap();
    let db = agy_dir.join("conversation_summaries.db");
    {
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute_batch("CREATE TABLE conversation_summaries (id TEXT, title TEXT, workspace_uris TEXT, last_modified_time INTEGER, last_user_input_time INTEGER, status TEXT, killed INTEGER, not_fully_idle INTEGER, preview TEXT);").unwrap();
        c.execute("INSERT INTO conversation_summaries VALUES (?1, 'T', '[\"file:///w\"]', 1792558922, 1792558922000, 'CASCADE_RUN_STATUS_IDLE', 0, 0, ?2)", rusqlite::params![uuid(700), CANARY]).unwrap();
    }
    let root = Root::new(&agy_dir).unwrap();
    let rows = agy::query_summaries(&root, &db, &h.app_data).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].workspaces, vec!["/w".to_string()]);
    assert_eq!(rows[0].last_modified, Some(1_792_558_922_000));
    assert!(!format!("{rows:?}").contains(CANARY), "`preview` is never selected");
    let map = agy::read_last_conversations(&root).unwrap();
    assert_eq!(map.get("/w"), Some(&uuid(700)));
    assert_eq!(map.get("/elsewhere"), Some(&uuid(701)));
    let _ = FakeProc::default();
}
