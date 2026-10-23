//! G6 probes of the never-open filter and of a macOS privacy (TCC) denial
//! mid-run (contract §14.5, A31.3; behaviour of §5.8 and §9.7):
//!
//! * **SP-7** — a hermetic home holding every never-open kind (`*.key`,
//!   `ide/*.lock`, `.credentials.json`, cursor `store.db`, `history.jsonl`),
//!   all mode 000, in the default config dir, under `~/.cursor` *and* in a
//!   discovered `~/.claude-*` sibling, next to ordinary readable vendor files:
//!   the integrated core (composition root, real World, fake process table)
//!   discovers the readable ones and never opens a listed one — no `EACCES`
//!   in the logs or in discovery's own counters.
//! * **HP-7** — a live watched session; mid-run its transcript, its project
//!   transcripts dir, `<cfg>/sessions` and the whole config dir are made
//!   unreadable (`chmod 000`), one at a time, and restored after each. A33.4:
//!   the source is `PermissionDenied`, the session keeps its state with
//!   `Likely` + `evidence.cantRead`, exactly one `notice.cantReadFolders` and
//!   a `health.folderDenied{category}` line; restoring clears all of it.
//! * **G7-02 / A34.3** — a hard link with an innocent name to a never-open
//!   file is refused after `open`; its bytes never reach a caller, the store
//!   or a log.
//!
//! Every tree lives under the T01 `Hermetic` home; permissions are restored
//! by a drop guard (also on panic). No vendor CLI, no real vendor dir, no
//! notification (the fakes record them), no app launch.
//!
//! Owner: T33.

mod fixtures;
#[path = "fixtures/vendor/support.rs"]
mod support;
#[path = "fixtures/verify/mod.rs"]
mod verify;

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;

use gilde_code_lib::contract::account::AccountSource;
use gilde_code_lib::contract::common::{Confidence, Vendor};
use gilde_code_lib::contract::services::WorldApi as _;
use gilde_code_lib::contract::session::{Control, Session};
use gilde_code_lib::contract::store::{SourceKind, SourceState};
use gilde_code_lib::discovery::history::{HistoryService, HistorySources, MemoryUids};
use gilde_code_lib::discovery::procs::{FakeBundles, FakeProc};
use gilde_code_lib::discovery::{cursor, safe_read, AccountSpec, DiscoveryConfig, DiscoveryDiagnostics, Engine, NoOwnProcesses};
use gilde_code_lib::follow::tail::open_followed;
use gilde_code_lib::store::now_ms;

use support::{ctime_utc, template, uuid, Rec};
use verify::{with_wired, Ctx, Logs, Probe};

/// The tests of this binary run one at a time: both read the process-wide
/// log capture, and HP-7 produces `EACCES` on purpose.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// What every never-open file holds: it must reach no log line and no store row.
const SECRET: &str = "SP7-SECRET-0f9e2d";
const KEY_HEX: &str = "0a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f9";
const SLUG: &str = "-proj";
const CHAT: &str = "abcdef12";
const TEAM: &str = "session-0000abcd";
const PID_MAIN: i32 = 4242;
const PID_SIB: i32 = 4343;
const CANT_READ_NOTICE: &str = "notice.cantReadFolders";
const CANT_READ_EVIDENCE: &str = "evidence.cantRead";
const FOLDER_DENIED: &str = "health.folderDenied";

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn uid() -> u32 {
    // SAFETY: `getuid` has no failure mode and touches no memory.
    unsafe { libc::getuid() }
}

fn now_sec() -> i64 {
    now_ms() / 1000
}

fn cwd_of(home: &Path) -> PathBuf {
    home.join("work/proj")
}

fn launchd() -> FakeProc {
    FakeProc { ppid: 0, uid: 0, start_sec: 1, comm: "launchd".into(), exec_path: Some("/sbin/launchd".into()), ..FakeProc::default() }
}

/// A fake `claude` of this user (the wired table's uid).
fn claude_proc(start_sec: i64, cwd: &Path) -> FakeProc {
    FakeProc {
        ppid: 1,
        uid: uid(),
        start_sec,
        comm: "claude".into(),
        exec_path: Some("/opt/fake/bin/claude.exe".into()),
        argv: Some(vec!["claude".into()]),
        env_names: Some(vec!["PATH".into(), "HOME".into()]),
        cwd: Some(cwd.to_string_lossy().into_owned()),
        rss_bytes: 50_000_000,
        ..FakeProc::default()
    }
}

/// `<cfg>/sessions/<pid>.json` (the live-registry shape, R2).
fn write_registry(cfg: &Path, pid: i32, sid: &str, start_sec: i64, cwd: &Path, status: &str) {
    let body = template(
        "claude/registry/interactive.json",
        &[
            ("PID", &pid.to_string()),
            ("SID", sid),
            ("CWD", cwd.to_str().unwrap()),
            ("STARTED_AT", &(start_sec * 1000 + 400).to_string()),
            ("PROC_START", &ctime_utc(start_sec)),
            ("KIND", "interactive"),
            ("ENTRYPOINT", "cli"),
            ("STATUS", status),
            ("EXTRA", ""),
        ],
    );
    std::fs::create_dir_all(cfg.join("sessions")).unwrap();
    std::fs::write(cfg.join("sessions").join(format!("{pid}.json")), body).unwrap();
}

/// `<cfg>/projects/<slug>/<sid>.jsonl` from the `normal` fixture.
fn write_transcript(cfg: &Path, sid: &str, cwd: &Path) -> PathBuf {
    let dir = cfg.join("projects").join(SLUG);
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(format!("{sid}.jsonl"));
    std::fs::write(&p, template("claude/transcripts/normal.jsonl", &[("SID", sid), ("CWD", cwd.to_str().unwrap())])).unwrap();
    p
}

/// The vendor keeps writing: one more timestamped row (so a re-read is due).
fn append_row(path: &Path, sid: &str) {
    let mut f = std::fs::OpenOptions::new().append(true).open(path).expect("append to the transcript");
    writeln!(f, r#"{{"type":"attachment","attachment":{{"type":"queued_command"}},"timestamp":"2026-10-23T00:00:00.000Z","sessionId":"{sid}"}}"#).unwrap();
}

/// Paths made mode 000, restored (to their mode) on drop — also on panic,
/// so the hermetic temp tree can always be removed.
#[derive(Default)]
struct Denied(Vec<(PathBuf, u32)>);

impl Denied {
    fn deny(&mut self, p: &Path, back: u32) {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o000)).unwrap_or_else(|e| panic!("chmod 000 {}: {e}", p.display()));
        self.0.push((p.to_path_buf(), back));
    }

    /// Already 000 (written so by `prepare`): restore on drop only.
    fn adopt(&mut self, p: &Path, back: u32) {
        self.0.push((p.to_path_buf(), back));
    }

    fn restore(&mut self) {
        while let Some((p, m)) = self.0.pop() {
            let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(m));
        }
    }
}

impl Drop for Denied {
    fn drop(&mut self) {
        self.restore();
    }
}

/// One wake (scan + reconcile) of the wired discovery thread, and its queue
/// drained; `None` = the thread did not answer.
fn rescan(c: &Ctx) -> Option<DiscoveryDiagnostics> {
    let d = c.wired.discovery.clone();
    tokio::task::block_in_place(move || {
        let g = d.read();
        let disc = g.as_ref()?;
        disc.wake();
        disc.sync(Duration::from_secs(30))
    })
}

/// Wait (bounded, generous) until `f` yields a value.
async fn eventually<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < deadline, "{what} never happened");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// What the store shows of one session and of the denial.
struct Seen {
    session: Option<Session>,
    /// Sources in `permissionDenied`.
    denied: Vec<SourceKind>,
    cant_read_notices: usize,
    notice_keys: Vec<String>,
    /// `health.folderDenied` categories.
    categories: Vec<String>,
}

/// The store after a flush. Discovery's evidence reaches World synchronously
/// (`EvidenceSink` → `World::mutate`), so after [`rescan`] this is final.
fn seen(c: &Ctx, sid: &str) -> Seen {
    c.wired.world.flush_now();
    let snap = c.wired.world.snapshot();
    Seen {
        session: snap.sessions.iter().find(|s| s.vendor_session_id.as_deref() == Some(sid)).cloned(),
        denied: snap.sources.iter().filter(|s| s.state == SourceState::PermissionDenied).map(|s| s.source).collect(),
        cant_read_notices: snap.notices.iter().filter(|n| n.key == CANT_READ_NOTICE).count(),
        notice_keys: snap.notices.iter().map(|n| n.key.clone()).collect(),
        categories: snap
            .notices
            .iter()
            .filter(|n| n.key == FOLDER_DENIED)
            .filter_map(|n| n.params.get("category").and_then(|v| v.as_str()).map(str::to_string))
            .collect(),
    }
}

fn control_kind(c: &Control) -> &'static str {
    match c {
        Control::InGilde { .. } => "inGilde",
        Control::Watching { .. } => "watching",
        Control::NotRunning { .. } => "notRunning",
    }
}

fn state_of(s: &Session) -> String {
    format!("{:?}/{}", s.activity.state, control_kind(&s.control))
}

/// Same activity kind (a `NoProgress{forSecs}` keeps counting) and control.
fn same_state(a: &Session, b: &Session) -> bool {
    std::mem::discriminant(&a.activity.state) == std::mem::discriminant(&b.activity.state)
        && control_kind(&a.control) == control_kind(&b.control)
}

fn confidence_of(s: &Session) -> String {
    let ev = s.activity.evidence.as_ref().map(|e| e.key.as_str()).unwrap_or("-");
    format!("{:?}+{ev}", s.activity.confidence)
}

/// Log lines that say a read hit `EACCES`/`EPERM`.
fn denial_lines(text: &str) -> Vec<String> {
    const MARKS: [&str; 6] = ["eacces", "permission denied", "permissiondenied", "permission_denied", "os error 13", "operation not permitted"];
    text.lines()
        .filter(|l| {
            let l = l.to_lowercase();
            MARKS.iter().any(|m| l.contains(m))
        })
        .map(|l| l.chars().take(240).collect())
        .collect()
}

// ---------------------------------------------------------------------------
// SP-7 — never-open files are never opened
// ---------------------------------------------------------------------------

/// Every never-open file of the SP-7 tree: (path, kind, in the sibling).
fn never_open_files(home: &Path) -> Vec<(PathBuf, &'static str, bool)> {
    let main = home.join(".claude");
    let sib = home.join(".claude-work");
    let chat = home.join(".cursor/chats").join(cursor::md5_hex(cwd_of(home).to_str().unwrap())).join(CHAT);
    vec![
        (main.join(format!("sessions/{PID_MAIN}.{KEY_HEX}.key")), "*.key", false),
        (main.join("daemon/control.key"), "*.key", false),
        (main.join("ide/51234.lock"), "ide/*.lock", false),
        (main.join(".credentials.json"), ".credentials.json", false),
        (main.join("history.jsonl"), "history.jsonl", false),
        // In a transcript dir the history index lists.
        (main.join("projects").join(SLUG).join("history.jsonl"), "history.jsonl", false),
        (chat.join("store.db"), "store.db", false),
        (chat.join("store.db-wal"), "store.db", false),
        // Where a reader DOES try to open by name (`*.json` in an inbox and a
        // task dir): the gate itself must refuse it, before `open`.
        (main.join("teams").join(TEAM).join("inboxes/.credentials.json"), ".credentials.json", false),
        (main.join("tasks").join(TEAM).join(".credentials.json"), ".credentials.json", false),
        // The sibling marker is itself never opened (lstat only).
        (sib.join(".claude.json"), ".claude.json", true),
        (sib.join(format!("sessions/{PID_SIB}.{KEY_HEX}.key")), "*.key", true),
        (sib.join("ide/5555.lock"), "ide/*.lock", true),
        (sib.join(".credentials.json"), ".credentials.json", true),
        (sib.join("history.jsonl"), "history.jsonl", true),
    ]
}

fn prepare_sp7(home: &Path, sid_main: &str, sid_sib: &str) {
    let cwd = cwd_of(home);
    std::fs::create_dir_all(&cwd).unwrap();
    for d in [".claude/sessions", ".claude/projects", ".claude/teams", ".claude/tasks", ".claude/jobs", ".claude/daemon", ".claude/ide", ".claude-work/sessions", ".claude-work/projects", ".claude-work/ide"] {
        std::fs::create_dir_all(home.join(d)).unwrap();
    }
    // Ordinary readable files: discovery has work in every root.
    write_transcript(&home.join(".claude"), sid_main, &cwd);
    write_transcript(&home.join(".claude-work"), sid_sib, &cwd);
    let chat = home.join(".cursor/chats").join(cursor::md5_hex(cwd.to_str().unwrap())).join(CHAT);
    std::fs::create_dir_all(&chat).unwrap();
    std::fs::write(chat.join("meta.json"), template("cursor/meta.json", &[("CWD", cwd.to_str().unwrap())])).unwrap();
    let team = home.join(".claude/teams").join(TEAM);
    std::fs::create_dir_all(team.join("inboxes")).unwrap();
    let vars = [("TEAM", TEAM), ("CREATED_AT", "1792558000000"), ("JOINED_AT", "1792558010000"), ("BOOT_ID", "0000abcd"), ("CWD", cwd.to_str().unwrap())];
    std::fs::write(team.join("config.json"), template("claude/teams/config.json", &vars)).unwrap();
    std::fs::write(team.join("inboxes/team-lead.json"), template("claude/teams/inbox_team_lead.json", &[])).unwrap();
    std::fs::create_dir_all(home.join(".claude/tasks").join(TEAM)).unwrap();
    std::fs::write(home.join(".claude/tasks").join(TEAM).join("1.json"), template("claude/tasks/1.json", &[])).unwrap();
    for (p, _, _) in never_open_files(home) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, SECRET).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o000)).unwrap();
    }
}

/// SP-7: every never-open kind at mode 000 — through the wired core (pass 1,
/// two wakes, World), through an engine + history pass on this thread (its
/// own counters), and through the follow path's opener. Any `open` of a mode
/// 000 file would fail with `EACCES`, so "no `EACCES` anywhere" = never opened.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_sp7_never_open_files_are_never_opened() {
    let logs = Logs::global();
    let _serial = SERIAL.lock().await;
    let mark = logs.mark();
    let (sid_main, sid_sib) = (uuid(701), uuid(702));
    let (pm, ps) = (sid_main.clone(), sid_sib.clone());
    with_wired(
        move |home| prepare_sp7(home, &pm, &ps),
        |c| async move {
            let mut p = Probe::new("SP-7");
            let files = never_open_files(&c.home);
            let mut guard = Denied::default();
            for (f, _, _) in &files {
                guard.adopt(f, 0o600);
            }
            let cwd = cwd_of(&c.home);
            let main = c.home.join(".claude");
            let sib = c.home.join(".claude-work");

            // Two live sessions (one per config dir), registered after pass 1
            // so the registry is read with the processes present.
            let start = now_sec() - 120;
            c.procs.insert(1, launchd());
            c.procs.insert(PID_MAIN, claude_proc(start, &cwd));
            c.procs.insert(PID_SIB, claude_proc(start, &cwd));
            write_registry(&main, PID_MAIN, &sid_main, start, &cwd, "idle");
            write_registry(&sib, PID_SIB, &sid_sib, start, &cwd, "idle");
            let s_main = eventually("the default config dir's session in World", || {
                rescan(&c);
                seen(&c, &sid_main).session
            })
            .await;
            let s_sib = eventually("the sibling's session in World", || {
                rescan(&c);
                seen(&c, &sid_sib).session
            })
            .await;
            let diag = rescan(&c).expect("discovery answers");
            let snap = c.wired.world.snapshot();
            let sibling = snap
                .accounts
                .iter()
                .find(|a| a.config_dir.as_ref().is_some_and(|d| d.as_str() == sib.to_str().unwrap()))
                .map(|a| a.source);
            assert_eq!(sibling, Some(AccountSource::Discovered), "the sibling is a Discovered account");
            assert!(diag.sibling_accounts >= 1, "discovery watches the sibling");
            assert!(diag.refused_opens > 0, "the team readers reached the gate with a never-open name (the probe exercises it)");

            // (a) the wired core: logs of every thread, the discovery thread's counters, the store.
            let text = logs.since(mark);
            let denials = denial_lines(&text);
            let refusals = text.lines().filter(|l| l.contains("never-open filter refused a path")).count();
            let denied_sources: Vec<String> = snap.sources.iter().filter(|s| s.state == SourceState::PermissionDenied).map(|s| format!("{:?}/{:?}", s.vendor, s.source)).collect();
            let secret_out = text.contains(SECRET) || serde_json::to_string(&snap).unwrap().contains(SECRET);
            let variant_a = format!(
                "(a) wired core over ~/.claude + ~/.cursor + sibling ~/.claude-work ({} files at 000; sessions {} + {} in World, sibling account Discovered)",
                files.len(),
                state_of(&s_main),
                state_of(&s_sib)
            );
            let observed_a = format!(
                "discovery-thread eacces={} refusedOpens={}; EACCES/permission-denied log lines={}; sources permissionDenied={:?}; secret in logs/store={secret_out}; never-open refusals logged={refusals}",
                diag.eacces,
                diag.refused_opens,
                denials.len(),
                denied_sources
            );
            if diag.eacces == 0 && denials.is_empty() && denied_sources.is_empty() && !secret_out {
                p.saw(variant_a, observed_a);
            } else {
                p.mismatch(variant_a, format!("{observed_a}; first lines: {:?}", denials.iter().take(3).collect::<Vec<_>>()), "never opened (no EACCES in logs)");
            }

            // (b) an engine reconcile + the history stat/fill pass on THIS
            // thread over the same tree (the per-thread counters see every read).
            let app_data = c.home.join(".gilde-probe-sp7");
            std::fs::create_dir_all(&app_data).unwrap();
            let main_acct = AccountSpec::new(Vendor::Claude, &main);
            let sib_acct = AccountSpec::new(Vendor::Claude, &sib);
            let history = HistoryService::new(
                HistorySources {
                    claude: vec![(main_acct.id.clone(), main.clone()), (sib_acct.id.clone(), sib.clone())],
                    codex: vec![],
                    cursor_chats: Some(c.home.join(".cursor/chats")),
                    agy_dir: Some(c.home.join(".gemini/antigravity-cli")),
                    home: c.home.clone(),
                    app_data: app_data.clone(),
                },
                Arc::new(MemoryUids::default()),
                None,
            );
            let mut cfg = DiscoveryConfig::for_home(&c.home, &app_data, Arc::new(NoOwnProcesses));
            cfg.accounts = vec![main_acct, AccountSpec::new(Vendor::Codex, &c.home.join(".codex"))];
            cfg.source = c.procs.clone();
            cfg.bundles = Arc::new(FakeBundles::default());
            cfg.history = Some(history.clone());
            cfg.watch = false;
            cfg.registry_retry = Duration::from_millis(1);
            cfg.team_retries = vec![Duration::ZERO; 3];
            let rec = Arc::new(Rec::default());
            let sink = rec.clone();
            let (stats, rows) = tokio::task::block_in_place(move || {
                safe_read::reset_stats();
                let mut e = Engine::new(cfg, sink);
                e.reconcile();
                history.refresh_index();
                history.fill();
                e.tick();
                e.reconcile();
                (safe_read::stats(), history.coverage().0)
            });
            let sids = rec.sids();
            assert!(sids.contains(&sid_main) && sids.contains(&sid_sib), "the engine saw both sessions: {sids:?}");
            assert!(stats.opened > 0, "the engine read the readable files");
            assert!(stats.refused > 0, "the engine's team readers reached the gate with a never-open name");
            let variant_b = format!("(b) Engine reconcile ×2 + history refresh/fill on the test thread (both sessions seen, {rows} index rows)");
            let observed_b = format!("this thread: opened={} eacces={} refused={}", stats.opened, stats.eacces, stats.refused);
            if stats.eacces == 0 {
                p.saw(variant_b, observed_b);
            } else {
                p.mismatch(variant_b, observed_b, "never opened (eacces 0)");
            }

            // (c) the follow path's opener (observed transcripts) on each file.
            let mut opened_or_eacces = Vec::new();
            for (f, kind, in_sib) in &files {
                match open_followed(f) {
                    Err(e) if e.raw_os_error().is_none() => {}
                    other => opened_or_eacces.push(format!("{kind}{}:{:?}", if *in_sib { "(sibling)" } else { "" }, other.map(|_| "opened").map_err(|e| e.to_string()))),
                }
            }
            let variant_c = format!("(c) follow::tail::open_followed on all {} files", files.len());
            if opened_or_eacces.is_empty() {
                p.saw(variant_c, "every one refused before open (no os error)");
            } else {
                p.mismatch(variant_c, format!("reached open(): {opened_or_eacces:?}"), "never opened");
            }
            drop(guard);
            p.finish();
        },
    )
    .await;
}

// ---------------------------------------------------------------------------
// HP-7 — TCC denial mid-run
// ---------------------------------------------------------------------------

fn prepare_hp7(home: &Path, sid: &str) {
    let cwd = cwd_of(home);
    std::fs::create_dir_all(&cwd).unwrap();
    for d in [".claude/sessions", ".claude/projects", ".claude/teams", ".claude/tasks", ".claude/jobs", ".claude/daemon"] {
        std::fs::create_dir_all(home.join(d)).unwrap();
    }
    write_transcript(&home.join(".claude"), sid, &cwd);
}

/// HP-7: a live watched session; mid-run (after the store showed it) one
/// path at a time is made unreadable, two discovery passes run, the store is
/// read, the path is restored and two more passes run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_hp7_tcc_denial_mid_run_keeps_sessions() {
    let logs = Logs::global();
    let _serial = SERIAL.lock().await;
    let mark = logs.mark();
    let sid = uuid(711);
    let ps = sid.clone();
    with_wired(
        move |home| prepare_hp7(home, &ps),
        |c| async move {
            let mut p = Probe::new("HP-7");
            let mut guard = Denied::default();
            let cwd = cwd_of(&c.home);
            let cfg = c.home.join(".claude");
            let transcript = cfg.join("projects").join(SLUG).join(format!("{sid}.jsonl"));
            let start = now_sec() - 120;
            c.procs.insert(1, launchd());
            c.procs.insert(PID_MAIN, claude_proc(start, &cwd));
            write_registry(&cfg, PID_MAIN, &sid, start, &cwd, "idle");
            let base = eventually("the live session with its transcript facts", || {
                rescan(&c);
                seen(&c, &sid).session.filter(|s| s.last_activity_at.is_some() && matches!(s.control, Control::Watching { .. }))
            })
            .await;
            assert_eq!(base.activity.confidence, Confidence::High, "readable: High before any denial");
            let pre = seen(&c, &sid);
            assert!(pre.denied.is_empty(), "no source denied before the probe: {:?}", pre.denied);

            let cases: [(&str, PathBuf, u32, &[SourceKind], &str); 4] = [
                ("(a1) its transcript file <cfg>/projects/<slug>/<sid>.jsonl (appended, then 000)", transcript.clone(), 0o644, &[SourceKind::Transcripts], "transcripts"),
                ("(a2) the project transcripts dir <cfg>/projects/<slug>", cfg.join("projects").join(SLUG), 0o755, &[SourceKind::Transcripts], "transcripts"),
                ("(b1) the config dir's registry <cfg>/sessions", cfg.join("sessions"), 0o755, &[SourceKind::Registry], "config"),
                ("(b2) the config dir itself <cfg> (~/.claude)", cfg.clone(), 0o755, &[SourceKind::Registry, SourceKind::Transcripts, SourceKind::Teams], "config"),
            ];
            for (label, path, back, sources, category) in cases {
                let before = seen(&c, &sid).session.expect("the session is in the store before the denial");
                let notified_before = c.recorded.notifications.delivered.lock().len();
                let eacces_before = rescan(&c).expect("discovery answers").eacces;
                // The vendor keeps writing up to the revocation: its new rows
                // are unread, so the next pass must re-read (an unchanged file
                // is served from discovery's cache and needs no read at all).
                append_row(&transcript, &sid);
                guard.deny(&path, back);
                let passes = [rescan(&c), rescan(&c)];
                // No crash: the discovery thread answers, the core answers IPC.
                assert!(passes.iter().all(Option::is_some), "{label}: the discovery thread stopped answering");
                let eacces = passes[1].as_ref().map_or(0, |d| d.eacces) - eacces_before;
                let ipc = c.call("app_bootstrap", json!({}));
                assert!(ipc.is_ok(), "{label}: the core stopped answering IPC: {ipc:?}");
                let s = seen(&c, &sid);
                let notified = c.recorded.notifications.delivered.lock().len() - notified_before;

                // A33.4: every expectation is asserted.
                let now = s.session.as_ref().unwrap_or_else(|| panic!("{label}: the session left the store"));
                assert!(
                    same_state(now, &before) && matches!(now.control, Control::Watching { .. }) && now.ended_at.is_none(),
                    "{label}: the session keeps its last state: {} (was {})",
                    state_of(now),
                    state_of(&before)
                );
                assert!(sources.iter().all(|k| s.denied.contains(k)), "{label}: sources {sources:?} PermissionDenied, got {:?}", s.denied);
                assert_eq!(now.activity.confidence, Confidence::Likely, "{label}: {}", confidence_of(now));
                assert_eq!(now.activity.evidence.as_ref().map(|e| e.key.as_str()), Some(CANT_READ_EVIDENCE), "{label}: {}", confidence_of(now));
                assert_eq!(s.cant_read_notices, 1, "{label}: exactly one notice: {:?}", s.notice_keys);
                assert!(s.categories.iter().any(|c| c == category), "{label}: a {FOLDER_DENIED} line for {category}: {:?}", s.categories);
                assert_eq!(notified, 0, "{label}: no notification because a read failed");
                let ok = [
                    "no crash (discovery + IPC answer)".to_string(),
                    format!("session kept {} (was {})", state_of(now), state_of(&before)),
                    format!("sources permissionDenied {:?}", s.denied),
                    format!("confidence {}", confidence_of(now)),
                    format!("one {CANT_READ_NOTICE} + {FOLDER_DENIED} {:?}", s.categories),
                    format!("{notified} notifications delivered, discovery-thread eacces +{eacces}"),
                ];

                // Restore and let discovery read again: everything clears.
                guard.restore();
                rescan(&c);
                rescan(&c);
                let r = seen(&c, &sid);
                let back_now = r.session.as_ref().unwrap_or_else(|| panic!("{label}: the session is still in the store after the restore"));
                assert!(r.denied.is_empty(), "{label}: no source denied after the restore: {:?}", r.denied);
                assert_eq!((r.cant_read_notices, r.categories.len()), (0, 0), "{label}: the notices clear: {:?}", r.notice_keys);
                assert_eq!(back_now.activity.confidence, Confidence::High, "{label}: {}", confidence_of(back_now));
                p.saw(label, format!("{}, restored → {} {}, notices cleared", ok.join(", "), state_of(back_now), confidence_of(back_now)));
            }
            let denials = denial_lines(&logs.since(mark));
            p.saw("logs", format!("{} EACCES/permission-denied lines over the run", denials.len()));
            p.finish();
        },
    )
    .await;
}

// ---------------------------------------------------------------------------
// G7-02 / A34.3 — hard links to never-open files
// ---------------------------------------------------------------------------

const HL_CANARY: &str = "sk-ant-SECRET-canary-g7-02";
const HL_TEAM: &str = "session-0000g702";
const HL_PID: i32 = 4242;

fn plant(p: &Path, body: &[u8]) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600)).unwrap();
}

/// `.credentials.json` (a canary) hard-linked to `teams/<team>/config.json`,
/// and a valid registry record (a canary session id) as `sessions/4242.key`
/// hard-linked to `sessions/4242.json`.
fn prepare_hardlinks(home: &Path, sid: &str, start: i64) {
    let cfg = home.join(".claude");
    let cred = cfg.join(".credentials.json");
    plant(&cred, format!("{{\"token\":\"{HL_CANARY}\"}}").as_bytes());
    let cfg_json = cfg.join("teams").join(HL_TEAM).join("config.json");
    std::fs::create_dir_all(cfg_json.parent().unwrap()).unwrap();
    std::fs::hard_link(&cred, &cfg_json).unwrap();
    let cwd = cwd_of(home);
    std::fs::create_dir_all(&cwd).unwrap();
    write_registry(&cfg, HL_PID, sid, start, &cwd, "idle");
    let json = cfg.join("sessions").join(format!("{HL_PID}.json"));
    let key = cfg.join("sessions").join(format!("{HL_PID}.key"));
    std::fs::rename(&json, &key).unwrap();
    std::fs::hard_link(&key, &json).unwrap();
}

/// G7-02 (A34.3): a hard link with an innocent name to a never-open file is
/// refused after `open` and its bytes never returned — directly through
/// `safe_read` (both of the reviewer's variants) and through the integrated
/// core (the linked registry record's session never shows; the linked
/// credential never reaches a log line).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_g7_02_hard_links_to_never_open_files_are_refused() {
    let logs = Logs::global();
    let _serial = SERIAL.lock().await;
    let mark = logs.mark();
    let mut p = Probe::new("SP-7"); // G7-02 / A34.3: the never-open class, through a hard link

    // (1)/(2) The reviewer's variants against `safe_read` itself.
    let dir = tempfile::tempdir().unwrap();
    let root = safe_read::Root::new(dir.path()).unwrap();
    let cred = dir.path().join(".credentials.json");
    plant(&cred, HL_CANARY.as_bytes());
    assert!(matches!(safe_read::open_in(&root, &cred), Err(safe_read::ReadError::Refused(_))), "refused by its own name");
    let innocent = dir.path().join("teams/session-x/config.json");
    std::fs::create_dir_all(innocent.parent().unwrap()).unwrap();
    std::fs::hard_link(&cred, &innocent).unwrap();
    let r = safe_read::open_in(&root, &innocent).map(|_| ());
    assert!(matches!(r, Err(safe_read::ReadError::Refused(_))), "the hard link is refused: {r:?}");
    p.saw("G7-02 hard link: .credentials.json via teams/<t>/config.json", format!("refused ({:?})", r.unwrap_err()));
    let key = dir.path().join("sessions/4242.key");
    plant(&key, HL_CANARY.as_bytes());
    let json = dir.path().join("sessions/4242.json");
    std::fs::hard_link(&key, &json).unwrap();
    let r = safe_read::read_file(&root, &json, 1 << 20).map(|_| ());
    assert!(matches!(r, Err(safe_read::ReadError::Refused(_))), "the .key through a .json name is refused: {r:?}");
    p.saw("G7-02 hard link: sessions/4242.key via sessions/4242.json", format!("refused ({:?}), no bytes returned", r.unwrap_err()));

    // (3) Through the integrated core.
    let sid = uuid(742);
    let start = now_sec() - 300;
    let ps = sid.clone();
    with_wired(
        move |home| prepare_hardlinks(home, &ps, start),
        |c| async move {
            let cwd = cwd_of(&c.home);
            c.procs.insert(1, launchd());
            c.procs.insert(HL_PID, claude_proc(start, &cwd));
            let d = rescan(&c).expect("discovery answers");
            rescan(&c);
            c.wired.world.flush_now();
            let snap = c.wired.world.snapshot();
            assert!(!snap.sessions.iter().any(|s| s.vendor_session_id.as_deref() == Some(sid.as_str())), "the linked registry record is never read");
            assert!(!snap.teams.iter().any(|t| t.raw_id == HL_TEAM), "the linked credential is never read as a team config");
            let text = logs.since(mark);
            assert!(!text.contains(HL_CANARY), "the canary never reaches a log line");
            c.wired.world.flush_now();
            p.saw(
                "G7-02 hard links through the integrated core (registry record + team config)",
                format!("no session with the record's id, no config read, canary in no log line (discovery refused_opens={})", d.refused_opens),
            );
            p.finish();
        },
    )
    .await;
}
