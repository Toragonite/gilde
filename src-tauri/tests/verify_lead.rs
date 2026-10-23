//! The lead's own G6 probes (A31.3 paths), LV-1…LV-6: contract-violating
//! inputs beyond the §14.5 table. Each probe drives the integrated core —
//! the composition root (`lifecycle::wiring`) with fakes only, Tauri's mock
//! runtime with the shipped ACL — through the real IPC, asserts the expected
//! refusal on every variant and prints one `PROBE LV-n: …` line. A variant
//! whose observed result differs from the expectation is recorded as
//! `MISMATCH(...)` (a finding for the lead; no product code is changed here).
//!
//! | # | input | expected |
//! |---|---|---|
//! | LV-1 | `history_row` and `convo_subscribe`'s history fallback (A29.2) for a uid whose indexed transcript was swapped for a symlink out of the vendor root; a hard link to a never-open file; the project dir swapped for a symlink out | refused or unavailable; nothing outside the root is opened (no canary reaches an answer, a view or a log) — A50.1 |
//! | LV-2 | `undo_cancel` with an A29.3 group token after its window, twice, and a token of another group | `NOT_FOUND{token}`; the undo sends no signal |
//! | LV-3 | `team_message_body` with a key of a team World does not hold, and forged well-formed keys | `NOT_FOUND{message}`; no transcript is read |
//! | LV-4 | `settings_set_executable` with a binary in a world-writable dir or under a world-writable ancestor, a world-writable binary file, and symlinks into a vendor dir | refused before any confirmation — A50.2, A50.4 |
//! | LV-5 | an `agents_rescan` burst | `BUSY` by the probe class; one detection runs (A45.3) |
//! | LV-6 | `screen_prompt_key` with a stale `promptRev` after the prompt changed, and after a resize rewrap | `STALE_PROMPT`; no key reaches the PTY (A34.1) |
//!
//! Owner: T45.

mod fixtures;
#[path = "fixtures/verify/mod.rs"]
mod verify;

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tauri::test::mock_builder;
use tauri::Manager as _;

use gilde_code_lib::commands::registry;
use gilde_code_lib::commands::settings::{ConfirmState, ExecutableConfirm};
use gilde_code_lib::commands::team::MessageBodies;
use gilde_code_lib::config::ConfigStore;
use gilde_code_lib::contract::common::{AbsPath, EpochMs, Vendor};
use gilde_code_lib::contract::content::ConvoEnvelope;
use gilde_code_lib::contract::ids::{AccountId, MessageKey, ProcessKey, SessionUid, TeamKey, UndoToken, ViewId};
use gilde_code_lib::contract::ipc::TeamMessageBodyRes;
use gilde_code_lib::contract::services::{EvidenceUpdate, IdentityHint, IdentityKey, SessionMetaFacts, SourceFacts, WorldApi};
use gilde_code_lib::contract::activity::ProcessFacts;
use gilde_code_lib::contract::store::RecoveryWindow;
use gilde_code_lib::contract::team::LeadBinding;
use gilde_code_lib::discovery::claude::teams::{LeadJoin, TasksRead, TeamConfig, TeamDirRead, TeamMemberRow};
use gilde_code_lib::discovery::reconcile::{LiveMember, TeamObservation, TeamObserver as _};
use gilde_code_lib::discovery::safe_read::{self, FileId};
use gilde_code_lib::follow::hub::{ConvoChannel, MessageBodySource, SendOutcome};
use gilde_code_lib::follow::tail::RowSink as _;
use gilde_code_lib::lifecycle::boot;
use gilde_code_lib::paths::AppPaths;
use gilde_code_lib::render::transcript::row_facts;
use gilde_code_lib::store::models::ProcKind;
use gilde_code_lib::store::now_ms;
use gilde_code_lib::supervise::kill::KillOpts;
use gilde_code_lib::supervise::reaper::{self, StopGroup, StopGroupObserver};
use gilde_code_lib::supervise::registry::SpawnGuard;
use gilde_code_lib::world::World;

use verify::{invoke, short, with_wired, Ctx, Logs, Probe};

/// The text every out-of-root / never-open file carries: it must reach no
/// answer, no view, no store row and no log line.
const CANARY: &str = "LV-CANARY-9d41-never-shown";

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

/// RFC 3339 UTC with milliseconds (the CLI's `timestamp`).
fn rfc3339(ms: EpochMs) -> String {
    let secs = ms.div_euclid(1000);
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
}

fn claude_account(home: &Path) -> AccountId {
    AccountId::derive(Vendor::Claude, &home.join(".claude").to_string_lossy())
}

fn script(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn has_canary(v: &impl std::fmt::Debug) -> bool {
    format!("{v:?}").contains(CANARY)
}

/// Poll `f` until it holds or `limit` passes (no wall-clock budget is
/// asserted: the limit only ends a wait that would otherwise hang).
async fn eventually(limit: Duration, mut f: impl FnMut() -> bool) -> bool {
    let until = Instant::now() + limit;
    loop {
        if f() {
            return true;
        }
        if Instant::now() > until {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// One discovery pass, waited for (the composition root's own thread).
async fn discovery_pass(c: &Ctx) {
    c.wired.discovery_handle().wake();
    let d = c.wired.discovery.clone();
    tokio::task::spawn_blocking(move || d.read().as_ref().map(|d| d.sync(Duration::from_secs(10)))).await.unwrap();
}

// ---------------------------------------------------------------------------
// LV-1 — a swapped transcript never leads a read out of the vendor root
// ---------------------------------------------------------------------------

/// A Claude transcript (one prompt, one reply) as the CLI writes it.
fn transcript(sid: &str, cwd: &Path, at: EpochMs, text: &str) -> String {
    let rows = [
        json!({"type": "user", "uuid": format!("u-{sid}"), "timestamp": rfc3339(at), "sessionId": sid,
               "cwd": cwd.to_str().unwrap(), "message": {"role": "user", "content": text}}),
        json!({"type": "assistant", "uuid": format!("a-{sid}"), "timestamp": rfc3339(at + 1_000), "sessionId": sid,
               "cwd": cwd.to_str().unwrap(),
               "message": {"id": format!("msg_{}", &sid[..8]), "role": "assistant",
                           "content": [{"type": "text", "text": format!("reply to: {text}")}],
                           "usage": {"input_tokens": 1, "output_tokens": 1}}}),
    ];
    rows.iter().map(|r| format!("{r}\n")).collect()
}

fn slug(cwd: &Path) -> String {
    cwd.to_str().unwrap().replace(['/', '.'], "-")
}

/// A view that keeps every envelope the hub sends it.
#[derive(Default)]
struct Collect(Mutex<Vec<ConvoEnvelope>>);

impl ConvoChannel for Collect {
    fn send(&self, envelope: ConvoEnvelope) -> SendOutcome {
        self.0.lock().push(envelope);
        SendOutcome::Sent
    }
}

struct Lv1Case {
    name: &'static str,
    sid: String,
    cwd: PathBuf,
}

impl Lv1Case {
    fn indexed_path(&self, home: &Path) -> PathBuf {
        home.join(".claude/projects").join(slug(&self.cwd)).join(format!("{}.jsonl", self.sid))
    }
}

fn lv1_cases(home: &Path) -> Vec<Lv1Case> {
    ["symlink-out", "hardlink-never-open", "dir-symlink-out"]
        .into_iter()
        .enumerate()
        .map(|(i, name)| Lv1Case {
            name,
            sid: format!("1e1{i:05x}-0000-4000-8000-0000000000a1"),
            cwd: home.join(format!("work/lv1-{i}")),
        })
        .collect()
}

/// An old session (30 days ago: never "ended today", so World does not hold
/// it and History's Read goes through the A29.2 fallback).
fn lv1_at() -> EpochMs {
    now_ms() - 30 * 86_400_000
}

/// LV-1: three indexed transcripts are swapped after the stat pass indexed
/// them — (a) the file for a symlink to a file outside the vendor root,
/// (b) the file for a hard link to a never-open file (`.credentials.json`)
/// inside it, (c) the project dir for a symlink to a dir outside the root.
/// Every target carries the canary in transcript-shaped rows, so a read is
/// visible. `history_row` and `convo_subscribe` (World does not hold the
/// uid: the history fallback) must answer refused/unavailable, and no canary
/// may reach an answer, the view, World's store or a log line. Then a
/// re-index after the swap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_lv1_a_swapped_transcript_never_leads_a_read_out_of_the_root() {
    let logs = Logs::global();
    with_wired(
        |home| {
            let at = lv1_at();
            for case in lv1_cases(home) {
                std::fs::create_dir_all(&case.cwd).unwrap();
                let path = case.indexed_path(home);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, transcript(&case.sid, &case.cwd, at, &format!("inside prompt {}", case.name))).unwrap();
            }
        },
        |c| async move {
            let mut p = Probe::new("LV-1");
            let home = c.home.clone();
            let cases = lv1_cases(&home);
            let world = c.world();
            let account = claude_account(&home);
            let at = lv1_at();
            // Discovery hands the claude account to history; the stat pass
            // indexes the three transcripts (nothing is loaded yet).
            let mut ready = false;
            for _ in 0..100 {
                discovery_pass(&c).await;
                if !c.wired.history.sources().claude.is_empty() {
                    ready = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert!(ready, "discovery never handed the claude account to history");
            let stats = c.wired.history.refresh_index();
            assert!(stats.total >= 3, "the three transcripts are indexed: {}", stats.total);
            let uids: Vec<SessionUid> = cases
                .iter()
                .map(|cs| {
                    world.uid_resolver().uid_for(&IdentityKey {
                        vendor: Vendor::Claude,
                        account_id: Some(account.clone()),
                        vendor_session_id: cs.sid.clone(),
                    })
                })
                .collect();

            // The swap, after the index was built (the TOCTOU case).
            let outside = home.join("outside");
            std::fs::create_dir_all(&outside).unwrap();
            // (a) the file → a symlink to a file outside the root.
            let a = &cases[0];
            let a_target = outside.join("a.jsonl");
            std::fs::write(&a_target, transcript(&a.sid, &a.cwd, at, &format!("outside {CANARY}"))).unwrap();
            std::fs::remove_file(a.indexed_path(&home)).unwrap();
            std::os::unix::fs::symlink(&a_target, a.indexed_path(&home)).unwrap();
            // (b) the file → a hard link to a never-open file in the root.
            let b = &cases[1];
            let creds = home.join(".claude/.credentials.json");
            std::fs::write(&creds, transcript(&b.sid, &b.cwd, at, &format!("credentials {CANARY}"))).unwrap();
            std::fs::remove_file(b.indexed_path(&home)).unwrap();
            std::fs::hard_link(&creds, b.indexed_path(&home)).unwrap();
            // (c) the project dir → a symlink to a dir outside the root, which
            // holds a file of the same name.
            let cc = &cases[2];
            let c_dir = cc.indexed_path(&home).parent().unwrap().to_path_buf();
            let outside_dir = home.join("outside-dir");
            std::fs::create_dir_all(&outside_dir).unwrap();
            std::fs::write(outside_dir.join(format!("{}.jsonl", cc.sid)), transcript(&cc.sid, &cc.cwd, at, &format!("outside dir {CANARY}")))
                .unwrap();
            std::fs::rename(&c_dir, home.join("moved-away")).unwrap();
            std::os::unix::fs::symlink(&outside_dir, &c_dir).unwrap();
            let mark = logs.mark();

            for (case, uid) in cases.iter().zip(&uids) {
                assert!(world.session(uid).is_none(), "{}: World does not hold the session (the A29.2 fallback)", case.name);
                // history_row over IPC.
                let r = c.call("history_row", json!({"uid": uid.as_str()}));
                let row_shown = match &r {
                    Ok(v) if v["title"].is_null() => "OK{title:null}".to_string(),
                    // The history fill read the row while it was the in-root file.
                    Ok(v) => format!("OK{{title:{}}} (loaded before the swap)", v["title"]),
                    Err(_) => short(&r),
                };
                // A row the history fill loaded before the swap (from the
                // in-root file) may show that title; nothing read after it may.
                let row_ok = match &r {
                    Err(e) => !has_canary(e),
                    Ok(v) => !has_canary(v),
                };
                // The same answer on this thread, counted by safe_read.
                safe_read::reset_stats();
                let direct = c.wired.history.row(uid);
                let opened = safe_read::stats().opened;
                assert!(
                    row_ok && opened == 0 && !has_canary(&direct),
                    "{} history_row: {row_shown}, safe_read opened {opened}, canary {}",
                    case.name,
                    has_canary(&r) || has_canary(&direct)
                );
                p.saw(format!("{} history_row", case.name), format!("{row_shown} (safe_read opened {opened})"));

                // convo_subscribe over IPC (the real command, a real channel).
                let view = format!("lv1-{}", case.name.split('-').next().unwrap());
                let r = c.call_raw(
                    "convo_subscribe",
                    json!({"req": {"uid": uid.as_str(), "viewId": view, "tailBlocks": null}, "channel": "__CHANNEL__:1"}),
                );
                // What that subscription shows: the same content, read by a
                // second view on the same hub.
                let seen = Arc::new(Collect::default());
                let direct = c.wired.hub.subscribe(uid, &ViewId::parse(format!("{view}-x")).unwrap(), None, seen.clone() as Arc<dyn ConvoChannel>);
                let shown = serde_json::to_string(&*seen.0.lock()).unwrap();
                let followed = c.wired.follower.followed().iter().any(|f| f == &case.indexed_path(&home));
                let leaked = shown.contains(CANARY);
                let answer = match &r {
                    Ok(v) => format!("OK{{source:{}}}", v["source"].as_str().unwrap_or("?")),
                    Err(_) => short(&r),
                };
                if let Ok(v) = &r {
                    if let Some(sub) = v["subId"].as_str() {
                        let _ = c.call("convo_unsubscribe", json!({"subId": sub}));
                    }
                }
                if let Ok(res) = &direct {
                    c.wired.hub.unsubscribe(&res.sub_id);
                }
                // A50.1: refused or unavailable, never its bytes.
                let unavailable = r.as_ref().err().is_some_and(|e| e["code"] == "UNSUPPORTED" && e["params"]["cap"] == "noReadableConversation");
                assert!(
                    unavailable && direct.is_err() && !leaked && !followed,
                    "{} convo_subscribe: {answer}, file followed {followed}, canary in the view {leaked}",
                    case.name
                );
                p.saw(format!("{} convo_subscribe", case.name), format!("{answer} (nothing followed, no canary)"));
            }

            // A re-index after the swap: a symlinked entry is not a file.
            let after = c.wired.history.refresh_index();
            let reindexed: Vec<String> = cases
                .iter()
                .zip(&uids)
                .map(|(case, uid)| format!("{}:{}", case.name, short(&c.call("history_row", json!({"uid": uid.as_str()})).map(|_| Value::Null))))
                .collect();
            p.saw("re-index after the swap", format!("{} rows; {}", after.total, reindexed.join(", ")));

            world.flush_now();
            let store_leak = serde_json::to_string(&world.snapshot()).unwrap().contains(CANARY);
            let log_leak = logs.since(mark).contains(CANARY);
            assert!(!store_leak, "the canary reached World's store");
            assert!(!log_leak, "the canary reached a log line");
            p.saw("store and logs", "no canary");
            p.finish();
        },
    )
    .await;
}

// ---------------------------------------------------------------------------
// LV-2 — a recovery group's undo token after its window, twice, of another group
// ---------------------------------------------------------------------------

/// A `/bin/sh` pane that logs every signal it gets (and exits on it).
struct SignalLogger {
    _guard: SpawnGuard,
    child: std::process::Child,
    log: PathBuf,
    spawn_id: String,
}

impl SignalLogger {
    async fn start(c: &Ctx, name: &str) -> SignalLogger {
        let dir = c.home.join("lv2");
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join(format!("{name}.log"));
        let ready = dir.join(format!("{name}.ready"));
        let body = format!(
            "for s in HUP INT TERM; do trap \"echo $s >> '{log}'; exit 0\" $s; done; : > '{ready}'; while :; do /bin/sleep 1 & wait $!; done",
            log = log.display(),
            ready = ready.display()
        );
        let argv = ["/bin/sh".to_string(), "-c".to_string(), body];
        gilde_code_lib::supervise::spawn::allow_executable(Path::new("/bin/sh"), &[ProcKind::Pane]);
        let req = gilde_code_lib::contract::services::SpawnRequest {
            kind: gilde_code_lib::contract::process::ProcKind::Pane,
            argv0: AbsPath::parse("/bin/sh").unwrap(),
            args: argv[1..].to_vec(),
            cwd: AbsPath::parse(dir.to_str().unwrap()).ok(),
            env_keys: Vec::new(),
            parent_uid: Some(SessionUid::mint()),
            generation: None,
            turn_id: None,
        };
        let guard = c.core.supervisor.plan_guarded(req).await.expect("plan");
        use std::os::unix::process::CommandExt as _;
        let child = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .env("GILDE_SPAWN_ID", guard.env_marker())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .expect("spawn");
        let pid = child.id() as i32;
        guard.mark_running(pid, pid).expect("running");
        assert!(eventually(Duration::from_secs(10), || ready.exists()).await, "the fixture installed its traps");
        let spawn_id = guard.spawn_id().to_string();
        SignalLogger { _guard: guard, child, log, spawn_id }
    }

    fn signals(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default().replace('\n', " ").trim().to_string()
    }

    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for SignalLogger {
    fn drop(&mut self) {
        // Only what this test started.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Hears each group's window (the boot wiring's `FeedRecovery` role).
#[derive(Default)]
struct Windows(Mutex<Vec<(String, bool)>>);

impl StopGroupObserver for Windows {
    fn started(&self, _window: &RecoveryWindow) {}
    fn ended(&self, token: &UndoToken, fired: bool) {
        self.0.lock().push((token.as_str().to_string(), fired));
    }
}

impl Windows {
    fn ended(&self, token: &str) -> Option<bool> {
        self.0.lock().iter().find(|(t, _)| t == token).map(|(_, f)| *f)
    }
}

fn arm(c: &Ctx, members: &[&SignalLogger], window: Duration, windows: &Arc<Windows>) -> String {
    let mut group = StopGroup::new();
    for m in members {
        group.push(m.spawn_id.clone(), KillOpts::default().grace(Duration::from_millis(500)));
    }
    let w = group.arm(c.core.supervisor.clone(), window, Some(windows.clone() as Arc<dyn StopGroupObserver>)).expect("armed");
    w.undo_token.as_str().to_string()
}

/// LV-2: `undo_cancel` with an A29.3 recovery group token — (a) after the
/// window expired (its stops fired), (b) twice, (c) a token of another
/// group (expired) while a third group is pending, (d) a forged
/// well-formed token — answers `NOT_FOUND{token}` and the undo itself sends
/// no signal to anything; the token of the pending group is untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_lv2_a_stale_or_foreign_undo_token_is_not_found_and_signals_nothing() {
    with_wired(
        |_| {},
        |c| async move {
            let mut p = Probe::new("LV-2");
            let windows = Arc::new(Windows::default());
            let not_found_token = |r: &Result<Value, Value>| {
                r.as_ref().err().is_some_and(|e| e["code"] == "NOT_FOUND" && e["params"]["entity"] == "token")
            };
            let undo = |token: &str| c.call("undo_cancel", json!({"undoToken": token}));

            // (a) group A's window passes: its stop fires (by design) …
            let a = SignalLogger::start(&c, "a").await;
            let ta = arm(&c, &[&a], Duration::from_millis(300), &windows);
            assert!(eventually(Duration::from_secs(10), || windows.ended(&ta) == Some(true)).await, "A's window ended");
            assert!(eventually(Duration::from_secs(10), || !a.signals().is_empty()).await, "A's stop fired");
            let fired = a.signals();
            // … and the late undo answers NOT_FOUND and signals nothing more.
            let r = undo(&ta);
            assert!(not_found_token(&r), "late undo: {r:?}");
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert_eq!(a.signals(), fired, "the late undo sent no signal");
            p.saw("A: undo after the window (its stop fired: TERM)", format!("{} ; no further signal", short(&r)));

            // (b) group B: undo in time, then the same token again.
            let mut b = SignalLogger::start(&c, "b").await;
            let mut cc = SignalLogger::start(&c, "c").await;
            let tb = arm(&c, &[&b], Duration::from_secs(4), &windows);
            let tc = arm(&c, &[&cc], Duration::from_secs(4), &windows);
            let first = undo(&tb);
            assert_eq!(first.as_ref().ok().map(|v| v["cancelled"].clone()), Some(json!(true)), "{first:?}");
            let second = undo(&tb);
            assert!(not_found_token(&second), "second undo: {second:?}");
            p.saw("B: undo in time, then again", format!("{} then {}", short(&first), short(&second)));

            // (c) A's token (another group, expired) and B's (cancelled)
            // while C is pending; (d) a forged well-formed token.
            for (variant, token) in [
                ("A's token while C is pending", ta.clone()),
                ("B's token while C is pending", tb.clone()),
                ("a forged well-formed token", UndoToken::mint().as_str().to_string()),
            ] {
                let r = undo(&token);
                assert!(not_found_token(&r), "{variant}: {r:?}");
                assert!(reaper::undo_is_pending(&tc), "{variant}: C's token is untouched");
                p.saw(variant, format!("{}, C still pending", short(&r)));
            }
            assert!(b.signals().is_empty() && cc.signals().is_empty(), "no undo signalled anything");

            // C was never cancelled: its window fires its stop; B's never does.
            assert!(eventually(Duration::from_secs(15), || windows.ended(&tc) == Some(true)).await, "C's window ended");
            assert!(eventually(Duration::from_secs(10), || !cc.signals().is_empty()).await, "C's stop fired");
            tokio::time::sleep(Duration::from_millis(500)).await;
            assert_eq!(windows.ended(&tb), Some(false), "B's window ended by the undo");
            assert!(b.signals().is_empty() && b.alive(), "B's member never got a signal: {:?}", b.signals());
            let _ = cc.alive();
            p.saw("afterwards", format!("B's member alive, no signal; C's window fired ({})", cc.signals()));
            p.finish();
        },
    )
    .await;
}

// ---------------------------------------------------------------------------
// LV-3 — a body of a team World does not hold
// ---------------------------------------------------------------------------

/// Counts every body lookup (the real reader behind it).
struct CountingBodies {
    inner: MessageBodySource,
    asked: Mutex<Vec<MessageKey>>,
}

impl MessageBodies for CountingBodies {
    fn body(&self, key: &MessageKey) -> Option<TeamMessageBodyRes> {
        self.asked.lock().push(key.clone());
        self.inner.body(key)
    }
}

struct Team {
    dir: &'static str,
    lead_sid: &'static str,
    alpha_sid: &'static str,
    pids: (u32, u32),
}

const TEAM_A: Team = Team {
    dir: "session-1a3a0001",
    lead_sid: "1a3a0001-0000-4000-8000-000000000001",
    alpha_sid: "1a3a0002-0000-4000-8000-000000000001",
    pids: (7410, 7411),
};
const TEAM_B: Team = Team {
    dir: "session-1b3b0001",
    lead_sid: "1b3b0001-0000-4000-8000-000000000001",
    alpha_sid: "1b3b0002-0000-4000-8000-000000000001",
    pids: (7420, 7421),
};

impl Team {
    fn key(&self, account: &AccountId) -> TeamKey {
        TeamKey::derive("claudeNative", Some(account), self.dir)
    }

    fn paths(&self, home: &Path) -> (PathBuf, PathBuf) {
        let base = home.join("lv3").join(self.dir);
        (base.join("lead.jsonl"), base.join("alpha.jsonl"))
    }

    fn hint(&self, account: &AccountId, sid: &str, pid: u32) -> IdentityHint {
        IdentityHint {
            vendor: Some(Vendor::Claude),
            account_id: Some(account.clone()),
            vendor_session_id: Some(sid.to_string()),
            process: Some(ProcessKey::new(pid, 1_792_700_000)),
            gilde_spawn_id: None,
        }
    }

    fn member(&self, name: &str, backend: &str, pane: &str, joined: EpochMs) -> TeamMemberRow {
        TeamMemberRow {
            agent_id: format!("{name}@{}", self.dir),
            name: name.into(),
            agent_type: Some(if name == "team-lead" { "team-lead".into() } else { "general-purpose".into() }),
            model: None,
            color: Some("blue".into()),
            backend_type: Some(backend.into()),
            tmux_pane_id: Some(pane.into()),
            cwd: None,
            joined_at: Some(joined),
            is_active: None,
            plan_mode_required: Some(false),
            mode: None,
        }
    }

    /// The team as discovery would observe it (T27's fixture shape).
    fn observation(&self, account: &AccountId, now: EpochMs) -> TeamObservation {
        let created = now - 60_000;
        TeamObservation {
            account: account.clone(),
            dir: TeamDirRead {
                dir_name: self.dir.into(),
                config: Some(TeamConfig {
                    name: Some(format!("lv3-{}", self.dir)),
                    created_at: Some(created),
                    lead_agent_id: Some(format!("team-lead@{}", self.dir)),
                    lead_session_id: Some(self.lead_sid[..8].into()),
                    members: vec![
                        self.member("team-lead", "in-process", "leader", created),
                        self.member("alpha", "tmux", "%1", created + 10_000),
                    ],
                    members_truncated: false,
                }),
                config_id: Some(FileId { size: 1, mtime_ms: created, inode: 1, dev: 1 }),
                roster_stale: false,
                torn: 0,
                inboxes: Default::default(),
                inboxes_skipped: 0,
            },
            join: LeadJoin { binding: LeadBinding::Exact, lead_session_id: Some(self.lead_sid.into()), lead_pid: Some(self.pids.0 as i32) },
            lead_alive: true,
            live_members: vec![LiveMember {
                agent_id: format!("alpha@{}", self.dir),
                agent_name: "alpha".into(),
                process: ProcessKey::new(self.pids.1, 1_792_700_000),
                pid: self.pids.1 as i32,
                session_id: Some(self.alpha_sid.into()),
            }],
            tasks: TasksRead::default(),
            in_process: vec![],
            visible: true,
            at: now,
        }
    }

    /// The lead's `SendMessage` to alpha and alpha's delivering row.
    fn rows(&self, at: EpochMs, msg_id: &str, body: &str) -> (Vec<String>, String) {
        let tool = format!("toolu_{}", msg_id.replace('-', "_"));
        let call = json!({
            "type": "assistant", "uuid": format!("a-{tool}"), "timestamp": rfc3339(at), "isSidechain": false,
            "sessionId": self.lead_sid,
            "message": {"id": format!("msg_{tool}"), "type": "message", "role": "assistant", "stop_reason": "tool_use",
                        "content": [{"type": "tool_use", "id": tool, "name": "SendMessage",
                                     "input": {"to": "alpha", "message": body, "summary": "lv3"}}],
                        "usage": {"input_tokens": 3, "output_tokens": 5}}
        });
        let result = json!({
            "type": "user", "uuid": format!("r-{tool}"), "timestamp": rfc3339(at + 40), "isSidechain": false,
            "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": tool, "content": "Message sent to alpha's inbox"}]},
            "toolUseResult": {"success": true, "message": "Message sent to alpha's inbox", "msg_id": msg_id,
                              "routing": {"sender": "team-lead", "target": "@alpha", "targetColor": "blue", "summary": "lv3", "content": body}}
        });
        let delivered = json!({
            "type": "user", "uuid": format!("o-{tool}"), "timestamp": rfc3339(at + 1_000), "isSidechain": false,
            "message": {"role": "user", "content": format!("<teammate-message teammate_id=\"team-lead\" color=\"blue\" summary=\"lv3\">\n{body}\n</teammate-message>")}
        });
        (vec![call.to_string(), result.to_string()], delivered.to_string())
    }

    /// Write the team's transcripts (both ends) for `messages`.
    fn write(&self, home: &Path, at: EpochMs, messages: &[(&str, &str)]) -> (Vec<String>, Vec<String>) {
        let (lead_path, alpha_path) = self.paths(home);
        std::fs::create_dir_all(lead_path.parent().unwrap()).unwrap();
        let mut lead = Vec::new();
        let mut alpha = Vec::new();
        for (i, (id, body)) in messages.iter().enumerate() {
            let (sent, delivered) = self.rows(at + i as i64 * 2_000, id, body);
            lead.extend(sent);
            alpha.push(delivered);
        }
        std::fs::write(&lead_path, lead.join("\n") + "\n").unwrap();
        std::fs::write(&alpha_path, alpha.join("\n") + "\n").unwrap();
        (lead, alpha)
    }

    /// Hold the team in World: both sessions, the roster, the rows.
    fn seed(&self, world: &World, account: &AccountId, home: &Path, now: EpochMs, lead: &[String], alpha: &[String]) {
        let (lead_path, alpha_path) = self.paths(home);
        let sink = world.sink();
        for (sid, pid, path) in [(self.lead_sid, self.pids.0, &lead_path), (self.alpha_sid, self.pids.1, &alpha_path)] {
            let h = self.hint(account, sid, pid);
            sink.evidence(EvidenceUpdate {
                hint: h.clone(),
                facts: SourceFacts::Process(ProcessFacts::Alive { pid: pid as i32, start_sec: 1_792_700_000, cpu_active_at: None }),
                at: now,
            });
            sink.evidence(EvidenceUpdate {
                hint: h,
                facts: SourceFacts::Meta(Box::new(SessionMetaFacts {
                    transcript_path: Some(AbsPath::parse(path.to_str().unwrap()).unwrap()),
                    ..SessionMetaFacts::default()
                })),
                at: now,
            });
        }
        let adapter = world.claude_team_adapter();
        adapter.team(self.observation(account, now));
        for (sid, path, lines) in [(self.lead_sid, &lead_path, lead), (self.alpha_sid, &alpha_path, alpha)] {
            let facts: Vec<_> = lines.iter().filter_map(|l| row_facts(l.as_bytes()).0).collect();
            let hint = IdentityHint { process: None, ..self.hint(account, sid, 0) };
            adapter.rows(path, &hint, &facts);
        }
    }
}

/// LV-3: `team_message_body` carries only a `MessageKey`, and a key names its
/// team. Team A is held by World; team B's transcripts are on disk (the
/// canary in every body) but World does not hold B. B's keys — for B's own
/// message ids and for the ids A uses — and forged well-formed keys (a
/// random `g_…`, an unknown id in A's team) answer `NOT_FOUND{message}`, and
/// no body lookup runs for any of them (a counting reader in front of the
/// real one); A's own key is the control.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_lv3_a_body_of_a_team_world_does_not_hold_is_never_read() {
    let logs = Logs::global();
    with_wired(
        |_| {},
        |c| async move {
            let mut p = Probe::new("LV-3");
            let world = c.world();
            let account = claude_account(&c.home);
            let now = now_ms();
            let mark = logs.mark();
            let (lead, alpha) = TEAM_A.write(&c.home, now, &[("m-1", "team A body one"), ("m-2", "team A body two")]);
            TEAM_A.seed(&world, &account, &c.home, now, &lead, &alpha);
            TEAM_B.write(&c.home, now, &[("m-1", &format!("team B {CANARY} one")), ("m-7", &format!("team B {CANARY} seven"))]);
            world.flush_now();
            let a_key = MessageKey::derive(&TEAM_A.key(&account), "m-1");
            assert!(world.snapshot().messages.iter().any(|m| m.key == a_key), "World holds team A's message");
            assert!(!world.snapshot().teams.iter().any(|t| t.key == TEAM_B.key(&account)), "World does not hold team B");

            let counting = Arc::new(CountingBodies { inner: MessageBodySource::new(world.clone()), asked: Mutex::new(Vec::new()) });
            c.wired.team.set_bodies(counting.clone());
            let refused: Vec<(&str, MessageKey)> = vec![
                ("team B's key for B's own m-7", MessageKey::derive(&TEAM_B.key(&account), "m-7")),
                ("team B's key for the id A uses (m-1)", MessageKey::derive(&TEAM_B.key(&account), "m-1")),
                ("forged g_<16 hex>", MessageKey::parse("g_5eed5eed5eed5eed").unwrap()),
                ("forged id in team A (m-404)", MessageKey::derive(&TEAM_A.key(&account), "m-404")),
            ];
            for (variant, key) in &refused {
                let r = c.call("team_message_body", json!({"messageKey": key.as_str()}));
                let e = r.as_ref().expect_err(variant);
                assert_eq!((e["code"].as_str(), e["params"]["entity"].as_str()), (Some("NOT_FOUND"), Some("message")), "{variant}: {e}");
                assert!(!has_canary(e), "{variant}: an error carries a body");
                p.saw(*variant, short(&r));
            }
            assert!(counting.asked.lock().is_empty(), "a body lookup ran for a refused key: {:?}", counting.asked.lock());
            p.saw("body lookups for the refused keys", "0");

            // Control: team A's own message is read (one lookup).
            let r = c.call("team_message_body", json!({"messageKey": a_key.as_str()}));
            let v = r.as_ref().expect("team A's body");
            assert_eq!(v["text"].as_str(), Some("team A body one"), "{v}");
            assert_eq!(counting.asked.lock().len(), 1);
            p.saw("team A's own m-1 (control)", format!("OK{{source:{}}}, 1 lookup", v["source"].as_str().unwrap_or("?")));

            world.flush_now();
            assert!(!serde_json::to_string(&world.snapshot()).unwrap().contains(CANARY), "B's body reached the store");
            assert!(!logs.since(mark).contains(CANARY), "B's body reached a log line");
            p.saw("store and logs", "no team B text");
            p.finish();
        },
    )
    .await;
}

// ---------------------------------------------------------------------------
// LV-4 — executables in world-writable dirs and behind symlinks into vendor dirs
// ---------------------------------------------------------------------------

#[derive(Default)]
struct RecordingConfirm(Mutex<Vec<(String, String)>>);

#[async_trait]
impl ExecutableConfirm for RecordingConfirm {
    async fn confirm(&self, key: &str, value_text: &str) -> bool {
        self.0.lock().push((key.to_string(), value_text.to_string()));
        true
    }
}

/// LV-4: `settings_set_executable` with a binary in a world-writable dir
/// (1777, the `/tmp` kind, and 0777), a clean symlink to one, a symlink into
/// a vendor dir (file, dir, a sibling `.claude-*` root, a case variant),
/// (A50.2) a binary or `pathsOverride` dir in owner-only dirs under a 0777
/// ancestor, and (A50.4) a 0777 binary file in a clean dir → refused before
/// any confirmation (a recording confirmation
/// counts every dialog). The user's own dir under a sticky 1777 dir and a
/// clean dir are each applied after one confirmation.
#[test]
fn probe_lv4_executables_from_writable_or_vendor_dirs_are_refused_before_any_dialog() {
    let _serial = verify::WIRED.blocking_lock();
    let mut p = Probe::new("LV-4");
    let dir = tempfile::tempdir().unwrap();
    let home = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::create_dir_all(home.join(".gilde")).unwrap();
    let config = Arc::new(ConfigStore::open_at(&home.join(".gilde/config.json")).unwrap());
    let shown = Arc::new(RecordingConfirm::default());
    let app = mock_builder().invoke_handler(registry::handler()).build(tauri::generate_context!(test = true)).unwrap();
    app.manage(config.clone());
    app.manage(Arc::new(AppPaths::for_home(&home)));
    app.manage(shown.clone() as ConfirmState);
    let w = tauri::WebviewWindowBuilder::new(&app, "main", Default::default()).build().unwrap();
    let fake = "#!/bin/sh\necho fake 1.0\n";
    let mode = |path: &Path, m: u32| std::fs::set_permissions(path, std::fs::Permissions::from_mode(m)).unwrap();

    let sticky = home.join("tmp-like");
    let open = home.join("open");
    for (d, m) in [(&sticky, 0o1777), (&open, 0o777)] {
        std::fs::create_dir_all(d).unwrap();
        mode(d, m);
    }
    script(&sticky.join("claude"), fake);
    script(&open.join("claude"), fake);
    script(&home.join(".claude/local/claude"), fake);
    script(&home.join(".claude-work/bin/claude"), fake);
    // A50.4: a binary anyone can rewrite, in a clean dir.
    let writable_file = home.join("wfile/claude");
    script(&writable_file, fake);
    mode(&writable_file, 0o777);
    let bin = home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(sticky.join("claude"), bin.join("to-sticky")).unwrap();
    std::os::unix::fs::symlink(home.join(".claude/local/claude"), bin.join("to-vendor")).unwrap();
    std::os::unix::fs::symlink(home.join(".claude/local"), home.join("tools")).unwrap();
    std::os::unix::fs::symlink(home.join(".claude-work/bin/claude"), bin.join("to-sibling")).unwrap();

    // A45.3: `settings_set_executable` is class `default` (10/s per
    // command) and this probe sends more than ten: on BUSY, wait
    // `retryAfterMs` and send it once more (the class is never relaxed).
    let set = |key: &str, path: &Path| {
        let once = || invoke(&w, "settings_set_executable", json!({"key": key, "value": {"kind": "path", "path": path.to_string_lossy()}}));
        let r = once();
        match r.as_ref().err().filter(|e| e["code"] == "BUSY") {
            Some(e) => {
                std::thread::sleep(Duration::from_millis(e["params"]["retryAfterMs"].as_u64().expect("retryAfterMs")));
                once()
            }
            None => r,
        }
    };
    let shown_home = |path: &Path| path.to_string_lossy().replace(home.to_str().unwrap(), "~");
    let cases: Vec<(&str, PathBuf, &str)> = vec![
        ("agents.claude.binary", sticky.join("claude"), "worldWritableDir"),
        ("agents.codex.binary", open.join("claude"), "worldWritableDir"),
        ("agents.claude.binary", bin.join("to-sticky"), "worldWritableDir"),
        ("sidecar.nodePath", sticky.join("claude"), "worldWritableDir"),
        ("agents.claude.binary", bin.join("to-vendor"), "insideVendorDir"),
        ("agents.cursor.binary", home.join("tools/claude"), "insideVendorDir"),
        ("agents.agy.binary", bin.join("to-sibling"), "insideVendorDir"),
        ("paths.override", sticky.clone(), "worldWritableDir"),
        ("paths.override", home.join("tools"), "insideVendorDir"),
        ("agents.claude.binary", writable_file.clone(), "worldWritableFile"),
    ];
    for (key, path, why) in &cases {
        let r = set(key, path);
        let e = r.as_ref().expect_err(&format!("{key} {}", shown_home(path)));
        assert_eq!((e["code"].as_str(), e["params"]["reason"].as_str()), (Some("REFUSED"), Some(*why)), "{key} {}: {e}", shown_home(path));
        p.saw(format!("{key}('{}')", shown_home(path)), short(&r));
    }
    // A case variant of the vendor dir (the same dir on a case-insensitive
    // volume; a missing path on a case-sensitive one).
    let variant = home.join(".CLAUDE/local/claude");
    let r = set("agents.claude.binary", &variant);
    let refused_or_missing = r.as_ref().err().is_some_and(|e| {
        (e["code"] == "REFUSED" && e["params"]["reason"] == "insideVendorDir") || (e["code"] == "NOT_FOUND" && e["params"]["entity"] == "file")
    });
    assert!(refused_or_missing, "~/.CLAUDE/local/claude: {r:?}");
    p.saw("agents.claude.binary('~/.CLAUDE/local/claude')", short(&r));
    assert!(shown.0.lock().is_empty(), "a dialog was shown for a refused value: {:?}", shown.0.lock());
    p.saw("dialogs for the refused values", "0");
    assert_eq!(config.get().agent_binary("claude"), None, "nothing was pinned");

    // A50.2: an owner-only dir under a 0777 dir (no sticky bit) — another
    // local user can rename the subdir away and plant one of the same name —
    // and the same as `paths.override`; under a sticky (1777) dir, the
    // user's own entries are fine (control below).
    let nested = open.join("sub");
    script(&nested.join("bin/claude"), fake);
    mode(&nested, 0o755);
    for (key, path) in [("agents.claude.binary", nested.join("bin/claude")), ("paths.override", nested.join("bin"))] {
        let r = set(key, &path);
        let e = r.as_ref().expect_err(key);
        assert_eq!((e["code"].as_str(), e["params"]["reason"].as_str()), (Some("REFUSED"), Some("worldWritableDir")), "{key}: {e}");
        p.saw(format!("{key}('{}') (0755 dirs under a 0777 dir)", shown_home(&path)), short(&r));
    }
    assert!(shown.0.lock().is_empty(), "a dialog was shown for a refused value: {:?}", shown.0.lock());
    let mine = sticky.join("mine/claude");
    script(&mine, fake);
    let before = shown.0.lock().len();
    let r = set("agents.claude.binary", &mine);
    assert_eq!(r.as_ref().map(|v| v["applied"].clone()), Ok(json!(true)), "the user's own dir under a sticky dir: {r:?}");
    assert_eq!(shown.0.lock().len(), before + 1);
    p.saw("agents.claude.binary('~/tmp-like/mine/claude') (the user's dir under a 1777 dir)", "OK{applied:true} after 1 dialog");

    // Control: a clean dir is applied after exactly one dialog.
    let clean = home.join("clean/claude");
    script(&clean, fake);
    let before = shown.0.lock().len();
    let r = set("agents.claude.binary", &clean);
    assert_eq!(r.as_ref().map(|v| v["applied"].clone()), Ok(json!(true)), "{r:?}");
    assert_eq!(shown.0.lock().len(), before + 1);
    p.saw("agents.claude.binary('~/clean/claude') (control)", "OK{applied:true} after 1 dialog");
    p.finish();
}

// ---------------------------------------------------------------------------
// LV-5 — an `agents_rescan` burst is one detection
// ---------------------------------------------------------------------------

/// A fake `claude` that logs each run's arguments and answers only once
/// `gate` exists (a detection stays in flight until the probe opens it).
fn counting_claude(path: &Path, counter: &Path, gate: &Path) {
    script(
        path,
        &format!(
            "#!/bin/sh\necho \"run $*\" >> '{}'\nwhile [ ! -e '{}' ]; do /bin/sleep 0.02; done\necho '2.1.307 (Claude Code)'\n",
            counter.display(),
            gate.display()
        ),
    );
}

/// The fake's runs so far, one line each (`run <args>`).
fn runs(counter: &Path) -> Vec<String> {
    std::fs::read_to_string(counter).map(|s| s.lines().map(str::to_string).collect()).unwrap_or_default()
}

/// Detection passes finished since `mark`: the rerun loop logs one line per
/// pass (`lifecycle::boot::spawn_rescan_as`), the coalescing counter A45.3
/// can be observed by.
fn passes(logs: &Logs, mark: usize) -> usize {
    logs.since(mark).matches("agent detection rerun finished").count()
}

/// LV-5: (a) a burst of 20 `agents_rescan` from one window: one is
/// scheduled, 19 answer `BUSY{retryAfterMs≈60 s}` (class `probe`, 1 per
/// 60 s), and one detection pass runs; (b) while that detection is held in
/// flight (a fake `claude` at the pinned path answers only when the probe
/// opens a gate), calls from two more apps over the same core — each with a
/// fresh limiter, so the limiter is not what stops them — are scheduled and
/// **join** it: still one pass. Passes are counted from the rerun loop's
/// own log line; the fake's runs are shown for context (the binary
/// resolution of the canary is cached, so they are not a pass counter).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_lv5_an_agents_rescan_burst_is_one_detection() {
    with_wired(
        |_| {},
        |c| async move {
            let mut p = Probe::new("LV-5");
            let logs = Logs::global();
            let pinned = PathBuf::from(c.core.config.get().agent_binary("claude").expect("claude is pinned (A38.2)"));
            let work = c.home.join("lv5");
            std::fs::create_dir_all(&work).unwrap();
            let (counter, gate) = (work.join("claude.runs"), work.join("gate"));
            counting_claude(&pinned, &counter, &gate);
            let lifecycle = c.core.lifecycle.clone();
            let scanned = || lifecycle.feed.boot().agents_scanned_at;

            // One detection alone, exactly as `agents_rescan` starts it (with
            // the compat service: its canary resolves the binary once more).
            std::fs::write(&gate, b"").unwrap();
            let mark = logs.mark();
            boot::spawn_rescan(lifecycle.clone(), c.wired.agents.clone(), c.core.services.compat()).await.expect("baseline");
            assert!(!runs(&counter).is_empty(), "a detection runs the pinned claude");
            assert_eq!(passes(logs, mark), 1, "the counter sees one pass");
            let _ = std::fs::remove_file(&counter);
            let _ = std::fs::remove_file(&gate);
            // (a) one window, 20 calls.
            let before = scanned();
            let mark = logs.mark();
            let (mut scheduled, mut busy) = (0, Vec::new());
            for _ in 0..20 {
                match c.call("agents_rescan", json!({})) {
                    Ok(v) if v["scheduled"] == true => scheduled += 1,
                    Err(e) if e["code"] == "BUSY" => busy.push(e["params"]["retryAfterMs"].as_u64().unwrap_or(0)),
                    other => panic!("unexpected {other:?}"),
                }
            }
            assert!(boot::rescan_running(&lifecycle), "the detection is held in flight");
            assert_eq!((scheduled, busy.len()), (1, 19), "class probe: one scheduled, the rest BUSY");
            assert!(busy.iter().all(|ms| (59_000..=60_000).contains(ms)), "{busy:?}");

            // (b) two more apps over the same core while it is in flight.
            let others: Vec<_> = (0..2).map(|_| c.another_app()).collect();
            let mut joined = Vec::new();
            for (_, w) in &others {
                let r = tokio::task::block_in_place(|| invoke(w, "agents_rescan", json!({})));
                joined.push(short(&r));
                assert_eq!(r.as_ref().map(|v| v["scheduled"].clone()), Ok(json!(true)), "a fresh limiter lets it through: {r:?}");
            }
            std::fs::write(&gate, b"").unwrap();
            assert!(eventually(Duration::from_secs(30), || !boot::rescan_running(&lifecycle)).await, "the detection finished");
            let n = runs(&counter);
            // The pass's log line is written as it finishes; let it land.
            tokio::time::sleep(Duration::from_millis(100)).await;
            let done = passes(logs, mark);
            assert_eq!(done, 1, "20 calls from one window and 2 from fresh limiters: one detection pass");
            assert_ne!(scanned(), before, "agentsScannedAt moved");
            p.saw(
                "20 agents_rescan in a burst",
                format!("1 scheduled, 19 BUSY{{retryAfterMs:{}}}", busy.iter().min().copied().unwrap_or(0)),
            );
            p.saw("2 more apps (fresh limiters) while it runs", format!("{} → joined", joined.join(", ")));
            p.saw("detection passes", format!("{done} (the fake ran: {})", n.join(", ")));
            drop(others);
            p.finish();
        },
    )
    .await;
}

// ---------------------------------------------------------------------------
// LV-6 — a stale `promptRev` never types into another prompt
// ---------------------------------------------------------------------------

/// The fake `claude` TUI at the pinned path: `--version` answers the
/// detection; a session draws a Bash permission prompt (the box shape of
/// R6's 2.1.307 prompt, as `sessions_live.rs` draws it) and logs every key
/// it reads to `<dir>/<mode>.log`. The mode is read from `<dir>/mode` at
/// start: `flip` shows prompt A, then — once `<dir>/flip` exists — prompt B
/// for another command; `wrap` shows a command too long for one row and
/// redraws it whenever the terminal size changes.
fn screen_tui(path: &Path, dir: &Path) {
    let d = dir.display();
    let body = format!(
        r#"#!/bin/sh
case "$1" in --version) echo '2.1.307 (Claude Code)'; exit 0;; esac
mode=$(/bin/cat '{d}/mode')
rule='────────────────────────────────────────────────────────────────────────────────'
draw() {{
  printf '\033[2J\033[H'
  printf ' Claude Code (fake lv6)\r\n'
  i=0
  while [ $i -lt 26 ]; do printf '\r\n'; i=$((i+1)); done
  printf '%s\r\n' "$rule"
  printf ' Bash command\r\n'
  printf '   %s\r\n' "$1"
  printf '   Clean up\r\n'
  printf '\r\n'
  printf ' Do you want to proceed?\r\n'
  printf ' \342\235\257 1. Yes\r\n'
  printf "   2. Yes, and don't ask again for rm commands in /tmp/fake-project\r\n"
  printf '   3. No\r\n'
  printf '\r\n'
  printf ' Esc to cancel \302\267 Tab to amend'
}}
key() {{ /bin/dd bs=1 count=1 2>/dev/null; }}
log() {{ printf '%s\n' "$1" >> '{d}/'"$mode"'.log'; }}
if [ "$mode" = flip ]; then
  /bin/stty raw -echo
  draw "rm -rf build/cache"
  while [ ! -e '{d}/flip' ]; do /bin/sleep 0.05; done
  draw "rm -rf ~/Documents"
  while :; do k=$(key); log "K:$k"; done
fi
/bin/stty raw -echo min 0 time 1
x=''
i=0
while [ $i -lt 100 ]; do x="${{x}}x"; i=$((i+1)); done
cmd="rm -rf /tmp/fake-project/$x"
draw "$cmd"
last=$(/bin/stty size)
while :; do
  k=$(key)
  if [ -n "$k" ]; then log "K:$k"; fi
  now=$(/bin/stty size)
  if [ "$now" != "$last" ]; then last=$now; draw "$cmd"; fi
done
"#
    );
    script(path, &body);
}

/// A Terminal-mode claude session started over IPC (`session_start`), and
/// its PTY once World holds it.
async fn start_terminal(c: &Ctx, project: &str, account: &AccountId, crid: &str) -> (SessionUid, gilde_code_lib::contract::ids::PtyId) {
    let r = c.call(
        "session_start",
        json!({
            "clientRequestId": crid, "projectId": project, "vendor": "claude", "accountId": account.as_str(),
            "model": null, "location": {"kind": "here", "locationId": null}, "permissionMode": "default",
            "controlMode": "terminal", "team": false, "prompt": null
        }),
    );
    let uid = SessionUid::parse(r.as_ref().expect("session_start")["uid"].as_str().unwrap()).unwrap();
    let world = c.world();
    let mut pty = None;
    assert!(
        eventually(Duration::from_secs(20), || {
            world.flush_now();
            pty = world.session(&uid).and_then(|s| match s.control {
                gilde_code_lib::contract::session::Control::InGilde { pty, .. } => pty,
                _ => None,
            });
            pty.is_some()
        })
        .await,
        "the session has a PTY"
    );
    (uid, pty.unwrap())
}

fn keys_logged(dir: &Path, mode: &str) -> String {
    std::fs::read_to_string(dir.join(format!("{mode}.log"))).unwrap_or_default().replace('\n', " ").trim().to_string()
}

/// LV-6: `screen_prompt_key` over IPC against a real PTY behind the
/// composition root (a fake claude TUI, Terminal mode, started with
/// `session_start`): (a) prompt A is replaced by prompt B for another
/// command — a key and a deny-with-reply aimed at A's rev; (b) a resize
/// (`pty_resize` over IPC) rewraps the header — a key aimed at the old
/// layout's rev. Each is `STALE_PROMPT` and no byte reaches the PTY (the
/// TUI logs every key it reads); the current card's own rev is the control.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_lv6_a_stale_prompt_rev_types_nothing() {
    use gilde_code_lib::contract::services::{PtyApi as _, SignalPlan};
    with_wired(
        |home| {
            std::fs::create_dir_all(home.join(".claude")).unwrap();
            std::fs::create_dir_all(home.join("work/lv6")).unwrap();
        },
        |c| async move {
            let mut p = Probe::new("LV-6");
            let dir = c.home.join("lv6");
            std::fs::create_dir_all(&dir).unwrap();
            let pinned = PathBuf::from(c.core.config.get().agent_binary("claude").expect("claude is pinned (A38.2)"));
            screen_tui(&pinned, &dir);
            let detected = boot::run_detection(&c.core.lifecycle, &c.wired.agents, None).await;
            assert!(
                detected.iter().any(|b| b.installed && b.kind == gilde_code_lib::agents::AgentKind::Claude),
                "the fake claude is detected"
            );
            let id = gilde_code_lib::discovery::location::identify(&c.home.join("work/lv6"), &c.home).expect("a folder project");
            let project = c.world().register_project(id.facts().expect("facts"));
            let account = AccountId::derive(Vendor::Claude, &std::fs::canonicalize(c.home.join(".claude")).unwrap().to_string_lossy());
            let pty_api = c.wired.pty.clone();
            let rev_of = |pty: &gilde_code_lib::contract::ids::PtyId| pty_api.screen_prompt(pty).map(|f| f.prompt_rev);
            let press = |uid: &SessionUid, rev: &str, action: Value| {
                c.call("screen_prompt_key", json!({"uid": uid.as_str(), "requestId": format!("sp_{rev}"), "promptRev": rev, "action": action}))
            };
            let stale = |r: &Result<Value, Value>| r.as_ref().err().is_some_and(|e| e["code"] == "STALE_PROMPT");

            // (a) prompt A, then prompt B for another command.
            std::fs::write(dir.join("mode"), "flip").unwrap();
            let (uid, pty) = start_terminal(&c, project.as_str(), &account, "0192f000-0000-4000-8000-0000000006a1").await;
            let mut a = None;
            assert!(eventually(Duration::from_secs(20), || { a = rev_of(&pty); a.is_some() }).await, "prompt A is a card");
            let a = a.unwrap();
            std::fs::write(dir.join("flip"), b"").unwrap();
            let mut b = None;
            assert!(
                eventually(Duration::from_secs(20), || { b = rev_of(&pty).filter(|r| *r != a); b.is_some() }).await,
                "prompt B (another command) is a new card"
            );
            let b = b.unwrap();
            for (variant, action) in [
                ("key 1 with A's rev on prompt B", json!({"kind": "key", "key": "1"})),
                ("deny-with-reply with A's rev on prompt B", json!({"kind": "denyWithReply", "text": "stop"})),
            ] {
                let r = press(&uid, &a, action);
                assert!(stale(&r), "{variant}: {r:?}");
                p.saw(variant, short(&r));
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
            assert_eq!(keys_logged(&dir, "flip"), "", "a stale key reached the PTY");
            p.saw("bytes read by prompt B", "none");
            let r = press(&uid, &b, json!({"kind": "key", "key": "3"}));
            assert_eq!(r.as_ref().map(|v| v["outcome"].clone()), Ok(json!("sent")), "{r:?}");
            assert!(eventually(Duration::from_secs(10), || keys_logged(&dir, "flip") == "K:3").await, "B's own card types 3");
            p.saw("key 3 with B's rev (control)", format!("{} → the TUI read 3", short(&r)));
            let _ = pty_api.close(&pty, SignalPlan::HupThenTerm { grace_ms: 2_000 }).await;

            // (b) a resize that rewraps the header.
            std::fs::write(dir.join("mode"), "wrap").unwrap();
            let (uid, pty) = start_terminal(&c, project.as_str(), &account, "0192f000-0000-4000-8000-0000000006b1").await;
            let mut wide = None;
            assert!(eventually(Duration::from_secs(20), || { wide = rev_of(&pty); wide.is_some() }).await, "the long prompt is a card");
            let wide = wide.unwrap();
            let r = c.call("pty_resize", json!({"ptyId": pty.as_str(), "cols": 97, "rows": 40}));
            assert!(r.is_ok(), "pty_resize: {r:?}");
            let mut narrow = None;
            assert!(
                eventually(Duration::from_secs(20), || { narrow = rev_of(&pty).filter(|r| *r != wide); narrow.is_some() }).await,
                "the rewrapped prompt is a new card"
            );
            let narrow = narrow.unwrap();
            let r = press(&uid, &wide, json!({"kind": "key", "key": "1"}));
            assert!(stale(&r), "the old layout's rev: {r:?}");
            p.saw("key 1 with the rev before a resize rewrap", short(&r));
            tokio::time::sleep(Duration::from_millis(400)).await;
            assert_eq!(keys_logged(&dir, "wrap"), "", "a stale key reached the PTY");
            p.saw("bytes read after the rewrap", "none");
            let r = press(&uid, &narrow, json!({"kind": "key", "key": "1"}));
            assert_eq!(r.as_ref().map(|v| v["outcome"].clone()), Ok(json!("sent")), "{r:?}");
            assert!(eventually(Duration::from_secs(10), || keys_logged(&dir, "wrap") == "K:1").await, "the new card types 1");
            p.saw("key 1 with the rewrapped card's rev (control)", format!("{} → the TUI read 1", short(&r)));
            let _ = pty_api.close(&pty, SignalPlan::HupThenTerm { grace_ms: 2_000 }).await;
            p.finish();
        },
    )
    .await;
}
