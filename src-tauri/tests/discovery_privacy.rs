//! Discovery privacy acceptance (contract §9.7, §9.10, §14.2):
//! T-priv-neverOpen, T-priv-2 (never write a vendor root, incl. a sibling),
//! T-priv-3 (a content canary never reaches logs, diagnostics or the index
//! table), T-priv-argv (discovery's part: only the redacted argv leaves the
//! cache boundary) and the static "`claude agents --json` is never run" check.

#[path = "fixtures/vendor/support.rs"]
mod support;

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use gilde_code_lib::discovery::history::{IndexStore, SqliteIndexStore};
use gilde_code_lib::discovery::procs::{FakeProc, Scanner};
use gilde_code_lib::discovery::safe_read::{self, ReadError, Root};
use gilde_code_lib::discovery::{Engine, NoOwnProcesses};
use support::*;

/// Every path of the §9.7 list, created with mode 000 under a fixture home:
/// `safe_read` refuses each **before** `open` (an attempted open would hit
/// `EACCES` and be counted).
#[test]
fn t_priv_never_open_every_listed_path_is_refused_before_open() {
    let h = Home::new();
    let paths = [
        ".claude/sessions/4242.0a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f9.key",
        ".claude/daemon/control.key",
        ".claude/ide/51234.lock",
        ".claude/.credentials.json",
        ".claude/.claude.json",
        ".claude.json",
        ".codex/auth.json",
        ".gemini/oauth_creds.json",
        ".gemini/antigravity-cli/antigravity-oauth-token",
        ".cursor/chats/0123456789abcdef0123456789abcdef/1f2e3d4c-aaaa/store.db",
        "x/server.pem",
        "x/session_token.txt",
        "x/aws_credentials",
        "x/oauth.json",
        "Library/Keychains/login.keychain-db",
        ".claude/history.jsonl",
        ".gemini/antigravity-cli/history.jsonl",
    ];
    let root = Root::new(&h.home).unwrap();
    safe_read::reset_stats();
    for rel in paths {
        let p = h.home.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, CANARY).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o000)).unwrap();
        match safe_read::open_in(&root, &p) {
            Err(ReadError::Refused(_)) => {}
            other => panic!("{rel}: expected a refusal before open, got {other:?}"),
        }
        assert!(matches!(safe_read::read_file(&root, &p, 1 << 20), Err(ReadError::Refused(_))), "{rel}");
    }
    let s = safe_read::stats();
    assert_eq!(s.eacces, 0, "no open() was attempted on any listed path");
    assert_eq!(s.opened, 0);
    assert_eq!(s.refused, 2 * paths.len() as u64);
}

/// A full engine pass over a populated fixture home whose every file is
/// read-only (`chmod -R a-w`) and whose key/lock/credential files are mode 000,
/// including a discovered `~/.claude-*` sibling: nothing changes on disk, no
/// key file is ever opened, the sibling is reported read-only.
#[test]
fn t_priv_2_never_write_any_vendor_root_including_a_sibling() {
    let h = Home::new();
    let cwd = h.workdir("proj");
    let sid = uuid(1);
    let start = 1_792_558_000;
    h.write_registry(&h.claude, 4242, &sid, start, &cwd, "interactive", "cli", "busy", "");
    h.write_transcript(&h.claude, "-proj", &sid, "claude/transcripts/normal.jsonl", &[("CWD", cwd.to_str().unwrap())]);
    let team = "session-0000abcd";
    let tdir = h.claude.join("teams").join(team);
    std::fs::create_dir_all(tdir.join("inboxes")).unwrap();
    std::fs::write(tdir.join("config.json"), template("claude/teams/config.json", &[("TEAM", team), ("CREATED_AT", "1792558000000"), ("JOINED_AT", "1792558010000"), ("BOOT_ID", "0000abcd"), ("CWD", cwd.to_str().unwrap())])).unwrap();
    std::fs::write(tdir.join("inboxes/team-lead.json"), template("claude/teams/inbox_team_lead.json", &[])).unwrap();
    std::fs::create_dir_all(h.claude.join("tasks").join(team)).unwrap();
    std::fs::write(h.claude.join("tasks").join(team).join("1.json"), template("claude/tasks/1.json", &[])).unwrap();
    for key in ["sessions/4242.abcdef.key", "daemon/control.key", "ide/5555.lock", ".credentials.json", ".claude.json"] {
        let p = h.claude.join(key);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, CANARY).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o000)).unwrap();
    }
    // A sibling with `.claude.json` (mode 000) and its own registry.
    let sib = h.home.join(".claude-work");
    std::fs::create_dir_all(sib.join("sessions")).unwrap();
    std::fs::create_dir_all(sib.join("projects")).unwrap();
    std::fs::write(sib.join(".claude.json"), CANARY).unwrap();
    std::fs::set_permissions(sib.join(".claude.json"), std::fs::Permissions::from_mode(0o000)).unwrap();
    h.write_registry(&sib, 4343, &uuid(2), start, &cwd, "interactive", "cli", "idle", "");

    chmod_tree(&h.claude, 0o444, 0o555);
    chmod_tree(&sib, 0o444, 0o555);
    for key in ["sessions/4242.abcdef.key", "daemon/control.key", "ide/5555.lock", ".credentials.json", ".claude.json"] {
        std::fs::set_permissions(h.claude.join(key), std::fs::Permissions::from_mode(0o000)).unwrap();
    }
    std::fs::set_permissions(sib.join(".claude.json"), std::fs::Permissions::from_mode(0o000)).unwrap();
    let before_main = tree_state(&h.claude);
    let before_sib = tree_state(&sib);

    let procs = procs();
    procs.insert(4242, claude_proc(start, 1, &["claude"], &cwd));
    procs.insert(4343, claude_proc(start, 1, &["claude"], &cwd));
    let rec = Arc::new(Rec::default());
    let history = h.history();
    let mut e = Engine::new(h.config(procs, Arc::new(TeamsRec::default()), Some(history.clone())), rec.clone());
    safe_read::reset_stats();
    e.reconcile();
    history.refresh_index();
    e.tick();
    e.reconcile();

    assert_eq!(safe_read::stats().eacces, 0, "no key, lock or credential file was opened");
    assert_eq!(tree_state(&h.claude), before_main, "the main config dir is unchanged");
    assert_eq!(tree_state(&sib), before_sib, "the sibling is unchanged");
    let accounts = rec.accounts.lock().unwrap().clone();
    assert_eq!(accounts.len(), 1, "the sibling is reported once");
    assert!(!accounts[0].usable_for_new, "Discovered until the user confirms");
    assert!(rec.sids().contains(&uuid(2)), "the sibling's sessions are watched");
    chmod_tree(&h.claude, 0o644, 0o755);
    chmod_tree(&sib, 0o644, 0o755);
}

/// A tracing subscriber that captures everything discovery logs.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// T-priv-3 (discovery's part): the canary sits in every content field of every
/// fixture; after a full scenario it appears in no log line, no diagnostics
/// export, no `history_index` row and no team observation.
#[test]
fn t_priv_3_content_canary_never_reaches_logs_diagnostics_or_the_index() {
    let cap = Capture::default();
    let writer = cap.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .finish();
    let _g = tracing::subscriber::set_default(subscriber);

    let h = Home::new();
    let cwd = h.workdir("proj");
    let start = 1_792_558_000;
    let sid = uuid(7);
    h.write_registry(&h.claude, 777, &sid, start, &cwd, "interactive", "cli", "busy", "");
    for (n, fx) in ["normal", "open_tool", "api_error_final", "api_error_recovered", "unknown_rows"].iter().enumerate() {
        let s = if n == 0 { sid.clone() } else { uuid(100 + n as u32) };
        h.write_transcript(&h.claude, "-proj", &s, &format!("claude/transcripts/{fx}.jsonl"), &[("CWD", cwd.to_str().unwrap())]);
    }
    h.write_transcript(&h.claude, "-proj", &uuid(200), "claude/transcripts/teammate.jsonl", &[("CWD", cwd.to_str().unwrap()), ("AGENT", "alpha"), ("TEAM", "session-0000abcd")]);
    let team = "session-0000abcd";
    let tdir = h.claude.join("teams").join(team);
    std::fs::create_dir_all(tdir.join("inboxes")).unwrap();
    std::fs::write(tdir.join("config.json"), template("claude/teams/config.json", &[("TEAM", team), ("CREATED_AT", "1792558000000"), ("JOINED_AT", "1792558010000"), ("BOOT_ID", "0000abcd"), ("CWD", cwd.to_str().unwrap())])).unwrap();
    std::fs::write(tdir.join("inboxes/team-lead.json"), template("claude/teams/inbox_team_lead.json", &[])).unwrap();
    std::fs::create_dir_all(h.claude.join("tasks").join(team)).unwrap();
    for t in ["1", "2"] {
        std::fs::write(h.claude.join("tasks").join(team).join(format!("{t}.json")), template(&format!("claude/tasks/{t}.json"), &[])).unwrap();
    }
    std::fs::write(h.claude.join("jobs/state.json"), template("claude/jobs/state.json", &[("SID", &sid)])).unwrap();

    let procs = procs();
    procs.insert(777, claude_proc(start, 1, &["claude", "--model", "opus"], &cwd));
    procs.insert(888, claude_proc(start, 1, &["claude", "--agent-id", "alpha@session-0000abcd", "--agent-name", "alpha", "--team-name", team, "--parent-session-id", &sid], &cwd));
    let rec = Arc::new(Rec::default());
    let teams = Arc::new(TeamsRec::default());
    let history = h.history();
    let mut e = Engine::new(h.config(procs, teams.clone(), Some(history.clone())), rec.clone());
    e.reconcile();
    history.refresh_index();
    history.fill(); // head/tail of every row (the background pass)

    // The index table of migration v2 (§3.9), written through the store.
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE history_index (path TEXT PRIMARY KEY, vendor TEXT NOT NULL, account_id TEXT, vendor_session_id TEXT, size INTEGER NOT NULL, mtime_ms INTEGER NOT NULL, inode INTEGER NOT NULL, cwd TEXT, git_branch TEXT, entrypoint TEXT, is_teammate INTEGER NOT NULL DEFAULT 0, team_name TEXT, teammate_name TEXT, continued_in TEXT, turns INTEGER, first_row_at INTEGER, last_row_at INTEGER, scanned_at INTEGER NOT NULL);",
    )
    .unwrap();
    let store = SqliteIndexStore::new(conn);
    let rows: Vec<_> = history.rows_since(0);
    store.upsert(&rows);
    let dumped = format!("{:?}", store.load());

    let logs = String::from_utf8_lossy(&cap.0.lock().unwrap()).into_owned();
    let diag = format!("{:?}", e.diagnostics());
    // Task subjects are the one content field a team observation carries (the
    // Board's memory-only `Task.title`, sanitized); everything else — spawn
    // prompt, inbox text/summary/result, task description/activeForm — never.
    let mut obs = teams.obs.lock().unwrap().clone();
    let subjects: Vec<String> = obs.iter().flat_map(|o| o.tasks.tasks.iter().filter_map(|t| t.subject.clone())).collect();
    assert!(subjects.iter().any(|s| s.contains("subject one")), "task subjects reach the adapter: {subjects:?}");
    for o in &mut obs {
        for t in &mut o.tasks.tasks {
            t.subject = None;
        }
    }
    let team_obs = format!("{obs:?}");
    for (what, text) in [("logs", &logs), ("diagnostics", &diag), ("history_index", &dumped), ("team observations", &team_obs)] {
        assert!(!text.contains(CANARY), "the canary leaked into {what}");
    }
    assert!(!rows.is_empty(), "the index has rows to check");
    assert!(!teams.obs.lock().unwrap().is_empty(), "the team was observed");
}

/// T-priv-argv (discovery's part, F-03): a hostile argv and env reach the cache
/// boundary; the snapshot and every other output carry only what the injected
/// redactor returned (here the fail-closed default), labels hold only the
/// allowlisted flags, and env *values* are never read at all.
#[test]
fn t_priv_argv_only_redacted_argv_leaves_the_cache_boundary() {
    let secrets = ["AKIAIOSFODNN7EXAMPLE", "wJalrXUtnFEMI-K7MDENG-bPxRfiCYEXAMPLEKEY", "https://proxy.internal.example", "sk-ant-api03-ZZZZZZZZZZZZZZZZZZZZZZZZ", "apiKeyHelper", "PROMPT-TEXT-SECRET"];
    let argv: Vec<String> = [
        "/opt/fake/bin/claude.exe",
        "--agent-id", "alpha@session-1", "--agent-name", "alpha", "--team-name", "session-1",
        "--parent-session-id", "00000001-0000-4000-8000-000000000001",
        "--settings", r#"{"apiKeyHelper":"/bin/echo sk-ant-api03-ZZZZZZZZZZZZZZZZZZZZZZZZ"}"#,
        "--model", "haiku",
        "-p", "PROMPT-TEXT-SECRET",
        "AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI-K7MDENG-bPxRfiCYEXAMPLEKEY",
        "ANTHROPIC_BASE_URL=https://proxy.internal.example",
        "AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let p = procs();
    p.insert(900, FakeProc {
        ppid: 1,
        uid: UID,
        start_sec: 1_792_558_000,
        comm: "claude".into(),
        exec_path: Some("/opt/fake/bin/claude.exe".into()),
        argv: Some(argv),
        env_names: Some(vec!["PATH".into(), "CCO_TEAM_MEMBER".into()]),
        cwd: Some("/tmp".into()),
        ..FakeProc::default()
    });
    let mut s = Scanner::new(p, gilde_code_lib::discovery::fail_closed_redactor(), Arc::new(NoOwnProcesses), Arc::new(gilde_code_lib::discovery::procs::FakeBundles::default()));
    let out = s.scan(1, 1);
    let info = out.snapshot.procs.iter().find(|x| x.pid == 900).unwrap();
    assert_eq!(info.argv_redacted, vec!["claude.exe".to_string(), "<arguments hidden>".to_string()]);
    assert_eq!(info.labels.agent_name.as_deref(), Some("alpha"));
    assert_eq!(info.cco_team_member, Some(true));
    let everything = format!("{:?} {:?}", out.snapshot, s.procs().collect::<Vec<_>>());
    for secret in secrets {
        assert!(!everything.contains(secret), "{secret} survived the cache boundary");
    }
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// PM-35, §9.10, A11.5: discovery never runs `claude agents --json` (or any
/// vendor CLI), and its only write/delete site is `sqlite::vendorcopy` (the
/// private `<app_data>/tmp/vendorcopy/<uuid>/` copies).
#[test]
fn static_discovery_never_runs_a_vendor_cli_or_writes() {
    let mut files = Vec::new();
    walk(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src/discovery"), &mut files);
    assert!(!files.is_empty());
    let strip = |t: &str| -> String { t.lines().filter(|l| !l.trim_start().starts_with("//")).collect::<Vec<_>>().join("\n") };
    for f in files {
        let text = std::fs::read_to_string(&f).unwrap();
        let text = text.split("#[cfg(test)]").next().unwrap_or("").to_string();
        let name = f.file_name().unwrap().to_string_lossy().into_owned();
        let code = strip(&text);
        assert!(!code.contains("agents --json") && !code.contains("\"agents\", \"--json\""), "{name}");
        assert!(!code.contains("Command::new"), "{name} spawns a process");
        let outside = if name == "sqlite.rs" {
            let start = text.find("pub fn vendorcopy(").expect("the one write site exists");
            let end = text.find("// -- end of the one write site").expect("its end marker");
            assert!(start < end);
            strip(&format!("{}{}", &text[..start], &text[end..]))
        } else {
            code.clone()
        };
        for w in ["OpenOptions", "fs::write", "File::create", "remove_file", "remove_dir", "fs::rename", "create_dir", "DirBuilder", "clonefile", "fs::copy"] {
            assert!(!outside.contains(w), "{name} writes outside the one write site ({w})");
        }
    }
}
