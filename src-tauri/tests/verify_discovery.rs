//! G6 probe HP-1 (contract §14.5, A31.3): torn and lying vendor files through
//! the integrated core — the composition root with fakes only (real World,
//! hermetic discovery over the hermetic home, the fake process table), read
//! back through World's store and the real IPC on Tauri's mock runtime.
//! Prints one `PROBE HP-1: …` line.
//!
//! The inputs (§14.5 HP-1), each with its variants:
//!
//! 1. registry `pid` ≠ file name: the file-name pid dead (the lying `pid`
//!    names a live lead), and the file-name pid a live `claude`;
//! 2. `procStart` 1 h off, in both directions (`startedAt` honest);
//! 3. `busy` 2 days old: with a newer transcript row, with rows as old as
//!    the status, with no transcript;
//! 4. team `config.json` truncated mid-`members` array (after a good read);
//! 5. a lead inbox with 10,000 entries (under the 4 MiB cap) and a member
//!    inbox over the cap;
//! 6. a task file `status: deleted` for an id never seen.
//!
//! Expected: hidden/stale/counted; state from the next source; no crash.
//! A33.5 pins (a) a lying `pid` is stale (hidden, `hiddenStale`) whatever
//! runs under either pid, (b) a 2-day-old `busy` without a newer row is No
//! progress — also without a transcript, (d) the 10 k inbox is bounded per
//! cycle and off the discovery thread; this probe asserts all three with
//! structural counters ((c) is T34's).
//!
//! The harness seeds no process before discovery's pass 1, so pass 1 sees
//! every registry entry dead; the probe then adds the processes and rewrites
//! the registry files' mtimes (the CLI rewrites them in place on every status
//! change) and wakes discovery — the next pass re-reads every entry.
//!
//! Wall time is printed for the record only, never asserted (A10.7).
//!
//! Owner: T33.

mod fixtures;
#[path = "fixtures/vendor/support.rs"]
mod support;
#[path = "fixtures/verify/mod.rs"]
mod verify;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use gilde_code_lib::contract::activity::{ActivityState, UnknownWhy};
use gilde_code_lib::contract::ids::MessageKey;
use gilde_code_lib::contract::services::WorldApi;
use gilde_code_lib::contract::store::{SourceKind, StoreSnapshot};
use gilde_code_lib::contract::team::TaskStatus;
use gilde_code_lib::discovery::procs::FakeProc;
use gilde_code_lib::discovery::claude::teams::INBOX_WINDOW;
use gilde_code_lib::discovery::DiscoveryDiagnostics;
use gilde_code_lib::store::now_ms;
use gilde_code_lib::world::INBOX_ENTRIES_PER_CYCLE;

use verify::{with_wired, Probe};

// ---------------------------------------------------------------------------
// the fixture tree
// ---------------------------------------------------------------------------

const DAY_MS: i64 = 86_400_000;

/// The team dir (a boot-id style name).
const TEAM: &str = "session-4a1b2c3d";
const LEAD_PID: i32 = 44_000;
const MATE_PID: i32 = 44_001;
/// Inbox sizes: 10,000 entries under the 4 MiB cap; the member inbox over it.
const INBOX_ENTRIES: usize = 10_000;
const BIG_TEXT: usize = 480;

/// Every pid, session id and time the probe writes, fixed before the tree.
struct Scene {
    now: i64,
    /// 1a: `41001.json` says `pid: 44000` (the live lead); 41001 is dead.
    lie_dead_sid: String,
    /// 1b: `41002.json` says `pid: 41998`; 41002 is a live `claude`.
    lie_live_cwd: PathBuf,
    lie_live_sid: String,
    lie_live_start: i64,
    /// 2a/2b: `procStart` = kernel start ± 3600 s.
    off_plus_sid: String,
    off_minus_sid: String,
    off_start: i64,
    /// 3a/3b/3c: `busy`, `statusUpdatedAt` 2 days ago.
    busy_newer_sid: String,
    busy_old_sid: String,
    busy_none_sid: String,
    busy_start: i64,
    busy_status_at: i64,
    /// The team: a live lead and a live pane teammate.
    lead_sid: String,
    lead_start: i64,
    team_cwd: PathBuf,
}

impl Scene {
    fn new(home: &Path) -> Scene {
        let now = now_ms();
        let now_s = now / 1000;
        let claude = home.join(".claude");
        for d in ["sessions", "projects", "teams", "tasks"] {
            std::fs::create_dir_all(claude.join(d)).unwrap();
        }
        let work = |name: &str| {
            let p = home.join("work").join(name);
            std::fs::create_dir_all(&p).unwrap();
            std::fs::canonicalize(&p).unwrap()
        };
        let _ = work("hp1-solo");
        Scene {
            now,
            lie_dead_sid: support::uuid(0x0001_a001),
            lie_live_cwd: work("hp1-lying-pid"),
            lie_live_sid: support::uuid(0x0001_a002),
            lie_live_start: now_s - 1_800,
            off_plus_sid: support::uuid(0x0002_a001),
            off_minus_sid: support::uuid(0x0002_a002),
            off_start: now_s - 3 * 3_600,
            busy_newer_sid: support::uuid(0x0003_a001),
            busy_old_sid: support::uuid(0x0003_a002),
            busy_none_sid: support::uuid(0x0003_a003),
            busy_start: now_s - 2 * 86_400 - 600,
            busy_status_at: now - 2 * DAY_MS,
            lead_sid: support::uuid(0x0004_4000),
            lead_start: now_s - 3_600,
            team_cwd: work("hp1-team"),
        }
    }

    fn solo_cwd(home: &Path) -> PathBuf {
        std::fs::canonicalize(home.join("work/hp1-solo")).unwrap()
    }
}

/// `<cfg>/sessions/<file_pid>.json` with a `pid` field of `pid_field`.
#[allow(clippy::too_many_arguments)]
fn write_registry(home: &Path, file_pid: i32, pid_field: i64, sid: &str, cwd: &Path, start_sec: i64, proc_start_sec: i64, status: &str, status_at: i64) {
    let body = json!({
        "pid": pid_field,
        "sessionId": sid,
        "cwd": cwd.to_str().unwrap(),
        "startedAt": start_sec * 1000 + 400,
        "procStart": support::ctime_utc(proc_start_sec),
        "version": "2.1.306",
        "peerProtocol": 1,
        "peerFeatures": ["notify_idle"],
        "kind": "interactive",
        "entrypoint": "cli",
        "pidDomain": "darwin",
        "name": "hp1 derived name",
        "nameSource": "derived",
        "updatedAt": status_at,
        "status": status,
        "statusUpdatedAt": status_at,
    });
    std::fs::write(home.join(format!(".claude/sessions/{file_pid}.json")), body.to_string()).unwrap();
}

/// A transcript whose one turn opened at `prompt_at` and ended at `end_at`.
fn write_transcript(home: &Path, slug: &str, sid: &str, cwd: &Path, prompt_at: i64, end_at: i64) {
    let cwd = cwd.to_str().unwrap();
    let rows = [
        json!({"type": "permission-mode", "permissionMode": "default", "sessionId": sid}),
        json!({"type": "user", "message": {"role": "user", "content": "hp1 probe prompt"}, "uuid": "u1", "timestamp": iso(prompt_at), "sessionId": sid, "cwd": cwd, "entrypoint": "cli", "version": "2.1.306"}),
        json!({"type": "assistant", "message": {"id": "msg_1", "role": "assistant", "content": [{"type": "text", "text": "hp1 reply"}], "stop_reason": "end_turn"}, "uuid": "a1", "timestamp": iso(end_at), "sessionId": sid, "cwd": cwd}),
        json!({"type": "system", "subtype": "turn_duration", "durationMs": end_at - prompt_at, "timestamp": iso(end_at), "sessionId": sid}),
    ];
    let dir = home.join(".claude/projects").join(slug);
    std::fs::create_dir_all(&dir).unwrap();
    let body: String = rows.iter().map(|r| format!("{r}\n")).collect();
    std::fs::write(dir.join(format!("{sid}.jsonl")), body).unwrap();
}

/// Every vendor file of the probe, before discovery starts.
fn prepare(home: &Path, s: &Scene) {
    let solo = Scene::solo_cwd(home);
    // 1. `pid` ≠ file name.
    write_registry(home, 41_001, i64::from(LEAD_PID), &s.lie_dead_sid, &solo, s.lead_start, s.lead_start, "busy", s.now);
    write_registry(home, 41_002, 41_998, &s.lie_live_sid, &s.lie_live_cwd, s.lie_live_start, s.lie_live_start, "busy", s.now);
    // 2. `procStart` 1 h off (kernel start = `off_start`, `startedAt` honest).
    write_registry(home, 42_001, 42_001, &s.off_plus_sid, &solo, s.off_start, s.off_start + 3_600, "busy", s.now);
    write_registry(home, 42_002, 42_002, &s.off_minus_sid, &solo, s.off_start, s.off_start - 3_600, "busy", s.now);
    // 3. `busy` 2 days old.
    for (pid, sid) in [(43_001, &s.busy_newer_sid), (43_002, &s.busy_old_sid), (43_003, &s.busy_none_sid)] {
        write_registry(home, pid, i64::from(pid), sid, &solo, s.busy_start, s.busy_start, "busy", s.busy_status_at);
    }
    // 3a: a turn that ended a minute ago (the registry is 2 days behind it).
    write_transcript(home, "-hp1-busy", &s.busy_newer_sid, &solo, s.now - 70_000, s.now - 60_000);
    // 3b: rows as old as the status (not behind it by more than 2 s).
    write_transcript(home, "-hp1-busy", &s.busy_old_sid, &solo, s.busy_status_at - 10_000, s.busy_status_at - 5_000);

    // The team: lead registry, roster, inboxes, task files.
    write_registry(home, LEAD_PID, i64::from(LEAD_PID), &s.lead_sid, &s.team_cwd, s.lead_start, s.lead_start, "idle", s.now - 30_000);
    let tdir = home.join(".claude/teams").join(TEAM);
    std::fs::create_dir_all(tdir.join("inboxes")).unwrap();
    let created = (s.lead_start - 1) * 1000;
    let joined = (s.lead_start + 10) * 1000;
    std::fs::write(
        tdir.join("config.json"),
        support::template(
            "claude/teams/config.json",
            &[("TEAM", TEAM), ("CREATED_AT", &created.to_string()), ("JOINED_AT", &joined.to_string()), ("BOOT_ID", "4a1b2c3d"), ("CWD", s.team_cwd.to_str().unwrap())],
        ),
    )
    .unwrap();
    // 5. 10,000 plain messages alpha → team-lead (each with a msg_id).
    let base = s.now - 4 * 3_600_000;
    let lead_inbox: Vec<Value> = (0..INBOX_ENTRIES)
        .map(|i| {
            json!({"from": "alpha", "text": format!("hp1 message {i}"), "timestamp": iso(base + i as i64 * 1_000), "read": true, "msgV": 1, "msg_id": format!("hp1-{i}"), "type": "message"})
        })
        .collect();
    let lead_bytes = serde_json::to_vec(&lead_inbox).unwrap();
    assert!(lead_bytes.len() < 4 * 1024 * 1024, "the 10 k inbox stays under the 4 MiB cap ({} bytes)", lead_bytes.len());
    std::fs::write(tdir.join("inboxes/team-lead.json"), lead_bytes).unwrap();
    // 5b. Over the cap: 10,000 entries team-lead → alpha with long texts.
    let filler = "y".repeat(BIG_TEXT);
    let big: Vec<Value> = (0..INBOX_ENTRIES)
        .map(|i| json!({"from": "team-lead", "text": filler, "timestamp": iso(base + i as i64 * 1_000 + 500), "read": false, "msg_id": format!("big-{i}")}))
        .collect();
    let big_bytes = serde_json::to_vec(&big).unwrap();
    assert!(big_bytes.len() > 4 * 1024 * 1024, "the member inbox is over the cap ({} bytes)", big_bytes.len());
    std::fs::write(tdir.join("inboxes/alpha.json"), big_bytes).unwrap();
    // 6. Tasks 1 (in progress, alpha) and 2 (pending), and 77 deleted, never seen.
    let tasks = home.join(".claude/tasks").join(TEAM);
    std::fs::create_dir_all(&tasks).unwrap();
    std::fs::write(tasks.join("1.json"), support::template("claude/tasks/1.json", &[])).unwrap();
    std::fs::write(tasks.join("2.json"), support::template("claude/tasks/2.json", &[])).unwrap();
    std::fs::write(tasks.join("77.json"), json!({"id": "77", "subject": "hp1 ghost task", "status": "deleted", "blocks": [], "blockedBy": []}).to_string()).unwrap();
    std::fs::write(tasks.join(".highwatermark"), "77").unwrap();
}

/// The live processes: 1b, 2a/2b, 3a–3c, the lead and its pane teammate.
fn procs(home: &Path, s: &Scene) -> Vec<(i32, FakeProc)> {
    let solo = Scene::solo_cwd(home);
    let lead_sid = s.lead_sid.clone();
    let mate = format!("alpha@{TEAM}");
    let mut v = vec![
        (41_002, proc(s.lie_live_start, &["claude"], &s.lie_live_cwd)),
        (42_001, proc(s.off_start, &["claude"], &solo)),
        (42_002, proc(s.off_start, &["claude"], &solo)),
        (LEAD_PID, proc(s.lead_start, &["claude", "--teammate-mode", "tmux"], &s.team_cwd)),
        (
            MATE_PID,
            proc(
                s.lead_start + 10,
                &["claude.exe", "--agent-id", &mate, "--agent-name", "alpha", "--team-name", TEAM, "--agent-color", "blue", "--parent-session-id", &lead_sid, "--model", "haiku"],
                &s.team_cwd,
            ),
        ),
    ];
    for pid in [43_001, 43_002, 43_003] {
        v.push((pid, proc(s.busy_start, &["claude"], &solo)));
    }
    v
}

fn proc(start: i64, argv: &[&str], cwd: &Path) -> FakeProc {
    let mut p = support::claude_proc(start, 1, argv, cwd);
    // SAFETY: `getuid` has no failure mode and touches no memory.
    p.uid = unsafe { libc::getuid() };
    p
}

/// The CLI rewrites its registry file in place on every status change: a new
/// mtime is what makes discovery re-read an entry.
fn rewrite_registry_mtimes(home: &Path) {
    for e in std::fs::read_dir(home.join(".claude/sessions")).unwrap().flatten() {
        let m = e.metadata().unwrap().modified().unwrap();
        let f = std::fs::File::options().write(true).open(e.path()).unwrap();
        f.set_modified(m + Duration::from_secs(1)).unwrap();
    }
}

// ---------------------------------------------------------------------------
// reading the integrated core
// ---------------------------------------------------------------------------

/// Discovery ran every queued command and the team observer applied what
/// they produced; its diagnostics. The timeout is a liveness bound, not a
/// budget.
fn settle(c: &verify::Ctx) -> DiscoveryDiagnostics {
    tokio::task::block_in_place(|| {
        let d = c.wired.discovery.read();
        d.as_ref().expect("discovery is running").sync(Duration::from_secs(300)).expect("discovery answered sync")
    })
}

/// `settle` after `cmd`, timed (the discovery thread's latency).
fn settle_timed(c: &verify::Ctx, cmd: impl FnOnce()) -> (DiscoveryDiagnostics, Duration) {
    let t = Instant::now();
    cmd();
    let d = settle(c);
    (d, t.elapsed())
}

fn reconcile_now(c: &verify::Ctx) {
    c.wired.discovery.read().as_ref().expect("discovery is running").reconcile_now();
}

fn snap(c: &verify::Ctx) -> StoreSnapshot {
    let w = c.world();
    w.flush_now();
    w.snapshot()
}

/// Wait (bounded, generous; not a budget) for World to show `what`.
async fn eventually(c: &verify::Ctx, what: &str, mut f: impl FnMut(&StoreSnapshot) -> bool) -> StoreSnapshot {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let s = snap(c);
        if f(&s) {
            return s;
        }
        assert!(Instant::now() < deadline, "World never showed {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn state_of<'a>(s: &'a StoreSnapshot, sid: &str) -> Option<&'a ActivityState> {
    s.sessions.iter().find(|x| x.vendor_session_id.as_deref() == Some(sid)).map(|x| &x.activity.state)
}

fn has_sid(s: &StoreSnapshot, sid: &str) -> bool {
    s.sessions.iter().any(|x| x.vendor_session_id.as_deref() == Some(sid))
}

fn kind(st: Option<&ActivityState>) -> String {
    match st {
        None => "no row".into(),
        Some(ActivityState::NoProgress { for_secs }) => format!("noProgress{{forSecs:{for_secs}}}"),
        Some(ActivityState::StatusUnknown { why }) => format!("statusUnknown{{{why:?}}}"),
        Some(s) => s.kind_str().into(),
    }
}

fn iso(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let milli = ms.rem_euclid(1000);
    let (y, m, d) = civil(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{milli:03}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Days → (y, m, d) (H. Hinnant's civil_from_days).
fn civil(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ---------------------------------------------------------------------------
// HP-1
// ---------------------------------------------------------------------------

/// HP-1: torn and lying vendor files → hidden/stale/counted; state from the
/// next source; no crash.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_hp1_torn_and_lying_vendor_files() {
    let scene = std::sync::Arc::new(std::sync::Mutex::new(None::<Scene>));
    let slot = scene.clone();
    with_wired(
        move |home| {
            let s = Scene::new(home);
            prepare(home, &s);
            *slot.lock().unwrap() = Some(s);
        },
        |c| async move {
            let s = scene.lock().unwrap().take().expect("the scene was prepared");
            let mut p = Probe::new("HP-1");
            let home = c.home.clone();

            // Pass 1 ran with no process alive; now the processes exist and
            // the registry is rewritten (as the CLI does), discovery wakes.
            for (pid, fp) in procs(&home, &s) {
                c.procs.insert(pid, fp);
            }
            rewrite_registry_mtimes(&home);
            // The first pass that sees the team visible (its 10 k inbox).
            let (diag, first_read) = settle_timed(&c, || c.wired.discovery_handle().wake());
            let after_first = c.world().claude_team_adapter().inbox_stats();
            // Baseline: a reconcile with nothing changed.
            let (diag_base, baseline) = settle_timed(&c, || reconcile_now(&c));
            let team_of = |x: &StoreSnapshot| x.teams.iter().find(|t| t.raw_id == TEAM).cloned();
            let lead_sid = s.lead_sid.clone();
            let mate_cwd = s.team_cwd.clone();
            let snapshot = eventually(&c, "the lead, its team with alpha, and the 2-day-old sessions", |x| {
                has_sid(x, &lead_sid)
                    && team_of(x).is_some_and(|t| x.members.iter().filter(|m| m.team_key == t.key).count() >= 2)
                    && x.sessions.iter().any(|y| y.cwd.as_ref().map(|c| c.as_str()) == mate_cwd.to_str() && y.vendor_session_id.is_none())
                    && has_sid(x, &s.busy_newer_sid)
                    && has_sid(x, &s.busy_old_sid)
                    && has_sid(x, &s.busy_none_sid)
            })
            .await;
            let registry = snapshot
                .sources
                .iter()
                .find(|x| x.source == SourceKind::Registry)
                .cloned()
                .expect("a Registry source status");

            // -- 1. `pid` ≠ file name (A33.5(a)) -------------------------------
            assert!(!has_sid(&snapshot, &s.lie_dead_sid), "the lying entry's session id is never shown");
            assert!(has_sid(&snapshot, &s.lead_sid), "the live lead keeps its own session id");
            assert_eq!(diag.registry_unknown_format, 0, "a lying pid is stale, not unknownFormat: {diag:?}");
            // 2 lying pids + 2 procStart offsets (below).
            assert_eq!(diag.registry_stale, 4, "both lying entries are counted stale: {diag:?}");
            assert_eq!(registry.hidden_stale, 4, "and reported hidden: {registry:?}");
            p.saw("registry pid≠name (name-pid dead, pid names the live lead)", format!("stale, hidden (no row for its sessionId; lead keeps its own), counted (registryStale={}, Registry.hiddenStale={})", diag.registry_stale, registry.hidden_stale));
            assert!(!has_sid(&snapshot, &s.lie_live_sid), "the lying entry's session id is not attributed to the live process");
            // The live process behind it is an ordinary unregistered claude:
            // shown after the 10 s grace, without the lying session id.
            let lie_cwd = s.lie_live_cwd.clone();
            let after_grace = eventually(&c, "the unregistered claude behind the lying file", |x| {
                x.sessions.iter().any(|y| y.cwd.as_ref().map(|c| c.as_str()) == lie_cwd.to_str())
            })
            .await;
            let lying = after_grace.sessions.iter().find(|x| x.cwd.as_ref().map(|c| c.as_str()) == s.lie_live_cwd.to_str()).unwrap();
            assert!(lying.vendor_session_id.is_none(), "never the lying id: {:?}", lying.vendor_session_id);
            assert_eq!(lying.activity.state, ActivityState::StatusUnknown { why: UnknownWhy::Unregistered }, "not Ready from a lying file");
            p.saw("registry pid≠name (name-pid a live claude)", format!("stale, hidden; the process is a row without a sessionId, state {}", kind(Some(&lying.activity.state))));

            // -- 2. `procStart` 1 h off ------------------------------------------
            assert!(!has_sid(&snapshot, &s.off_plus_sid), "procStart +1 h: stale, hidden");
            assert!(!has_sid(&snapshot, &s.off_minus_sid), "procStart −1 h: stale, hidden");
            p.saw("procStart +1 h / −1 h", format!("stale, hidden, counted (with the lying pids: registryStale={}, Registry.hiddenStale={})", diag.registry_stale, registry.hidden_stale));
            assert_eq!(diag.registry_live, 4, "3 busy + the lead are live: {diag:?}");

            // -- 3. `busy` 2 days old --------------------------------------------
            let newer = state_of(&snapshot, &s.busy_newer_sid).cloned();
            assert!(
                matches!(newer, Some(ActivityState::Ready) | Some(ActivityState::NewReply)),
                "busy 2 d with a turn that ended a minute ago: the transcript decides, got {newer:?}"
            );
            p.saw("busy 2 d + a newer transcript row", format!("{} (registry stale; state from the transcript)", kind(newer.as_ref())));
            let old = state_of(&snapshot, &s.busy_old_sid).cloned();
            assert!(matches!(old, Some(ActivityState::NoProgress { .. })), "busy 2 d + rows as old: the 10 min busy rule, got {old:?}");
            p.saw("busy 2 d + rows as old", format!("{} (the 10 min busy rule)", kind(old.as_ref())));
            // A33.5(b): also without any transcript.
            let none = state_of(&snapshot, &s.busy_none_sid).cloned();
            assert!(matches!(none, Some(ActivityState::NoProgress { .. })), "busy 2 d, no transcript: not Working (PM-01), got {none:?}");
            p.saw("busy 2 d, no transcript", format!("{} (the 600 s busy rule on statusUpdatedAt)", kind(none.as_ref())));

            // -- 5. inboxes ----------------------------------------------------
            let team = team_of(&snapshot).expect("the team is visible");
            let store_msgs = snapshot.messages.iter().filter(|m| m.team_key == team.key).count();
            assert_eq!(store_msgs, 500, "the store keeps the newest 500 messages of the team");
            // World's bounded copy, page by page (World API: newest first).
            let w = c.world();
            let mut world_held = 0usize;
            let mut before = now_ms() + DAY_MS;
            for _ in 0..40 {
                let (page, more) = w.team_messages_page(&team.key, before, 500);
                world_held += page.len();
                match page.last() {
                    Some(m) if more => before = m.at,
                    _ => break,
                }
            }
            assert!(world_held <= 5_000 && world_held >= store_msgs, "World holds a bounded copy: {world_held}");
            assert!(w.message_locator(&MessageKey::derive(&team.key, &format!("hp1-{}", INBOX_ENTRIES - 1))).is_some(), "the newest inbox message is held");
            assert!(w.message_locator(&MessageKey::derive(&team.key, "hp1-0")).is_none(), "the oldest of 10 k is beyond the memory cap");
            p.saw("lead inbox with 10 k entries (1.4 MB)", format!("no crash; bounded (store {store_msgs}, World {world_held})"));
            // The same pages through the real IPC (`team_messages_page`:
            // oldest first within a page; `hasMore` pages further back).
            let mut ipc_held = 0usize;
            let mut before = now_ms() + DAY_MS;
            for _ in 0..40 {
                let r = c
                    .call("team_messages_page", json!({"teamKey": team.key.as_str(), "before": before, "count": 500}))
                    .unwrap_or_else(|e| panic!("team_messages_page answers: {e}"));
                let msgs = r["messages"].as_array().cloned().unwrap_or_default();
                ipc_held += msgs.len();
                match msgs.first().and_then(|m| m["at"].as_i64()) {
                    Some(at) if r["hasMore"] == Value::Bool(true) => before = at,
                    _ => break,
                }
            }
            // A33.5(c): the command pages World, not the store's newest 500.
            assert!(ipc_held > store_msgs, "team_messages_page pages beyond the store's {store_msgs}: {ipc_held}");
            assert_eq!(ipc_held, world_held, "team_messages_page reaches every message World holds");
            p.saw("10 k inbox paged over IPC", format!("team_messages_page reaches all {ipc_held} World holds (store {store_msgs}; A33.5(c))"));
            let big_seen = (0..INBOX_ENTRIES).step_by(997).any(|i| w.message_locator(&MessageKey::derive(&team.key, &format!("big-{i}"))).is_some());
            assert!(!big_seen, "the over-cap inbox is skipped");
            p.saw("member inbox over the 4 MiB cap", "skipped this cycle (none of its messages held)");

            // -- 6. a task file `deleted` for an unknown id -----------------------
            let tasks: Vec<_> = snapshot.tasks.iter().filter(|t| t.team_key == team.key).collect();
            let live: Vec<_> = tasks.iter().filter(|t| t.status != TaskStatus::Deleted).collect();
            assert!(!live.iter().any(|t| t.native_id.as_deref() == Some("77")), "never on the board: {tasks:?}");
            assert_eq!(team.progress.total as usize, live.len(), "the deleted id is not counted");
            let ghost = tasks.iter().find(|t| t.native_id.as_deref() == Some("77")).map(|t| format!("{:?}", t.status)).unwrap_or_else(|| "absent".into());
            p.saw("task file deleted for unknown id 77", format!("not on the board, not counted (progress.total={}; store row: {ghost})", team.progress.total));

            // -- 4. `config.json` truncated mid-array ------------------------------
            assert!(!team.roster_stale, "the good config first");
            let members_before = snapshot.members.iter().filter(|m| m.team_key == team.key).count();
            assert_eq!(members_before, 2, "lead + alpha from the good config");
            let cfg_path = home.join(".claude/teams").join(TEAM).join("config.json");
            let body = std::fs::read_to_string(&cfg_path).unwrap();
            let alpha = body.find("{\"agentId\":\"alpha@").expect("the second member");
            let cut = alpha + 20;
            assert!(body[..cut].contains("\"members\":[") && cut < body.len(), "the cut is inside the members array");
            std::fs::write(&cfg_path, &body[..cut]).unwrap();
            let before_torn = c.world().claude_team_adapter().inbox_stats();
            let (diag2, changed_read) = settle_timed(&c, || reconcile_now(&c));
            let after_torn = c.world().claude_team_adapter().inbox_stats();
            let key = team.key.clone();
            let after = eventually(&c, "roster_stale on the team", |x| x.teams.iter().any(|t| t.key == key && t.roster_stale)).await;
            let members_after = after.members.iter().filter(|m| m.team_key == team.key).count();
            assert_eq!(members_after, members_before, "the last good roster is kept");
            assert!(diag2.team_torn >= 1, "counted torn: {diag2:?}");
            p.saw("config.json truncated mid-members", format!("rosterStale, last good roster kept ({members_after} members), counted (teamTorn={})", diag2.team_torn));

            // -- A33.5(d): the 10 k inbox is bounded per cycle, off the thread -----
            // Structural counters (A10.7): the observer runs on its own
            // thread; one cycle reads ≤ 5,000 entries per inbox and applies
            // ≤ 5,000 in one World batch; an unchanged inbox is neither re-read
            // nor re-applied; a reconcile with nothing changed hands nothing on.
            assert!(diag.team_observer_queued, "the team observer has its own thread: {diag:?}");
            assert!(diag.inbox_entries_dropped >= (INBOX_ENTRIES - INBOX_WINDOW) as u64, "the lead inbox is read as its newest 5,000: {diag:?}");
            let first = &after_first;
            assert!(first.observations >= 1 && first.processed_total >= 1, "the team was applied: {first:?}");
            assert!(!after_torn.on_discovery_thread, "never on the discovery thread: {after_torn:?}");
            assert!(first.max_entries as usize <= INBOX_WINDOW, "≤ 5,000 entries in one cycle: {first:?}");
            assert!(first.max_processed as usize <= INBOX_ENTRIES_PER_CYCLE, "≤ 5,000 applied in one cycle: {first:?}");
            assert_eq!(first.max_world_batches, 1, "one World mutation per cycle's events: {first:?}");
            assert!(after_torn.observations > before_torn.observations, "the torn config was observed: {after_torn:?}");
            let torn_applied = after_torn.processed_total - before_torn.processed_total;
            let torn_events = after_torn.events_total - before_torn.events_total;
            assert_eq!(torn_applied, 0, "the unchanged inbox is not re-applied: {after_torn:?}");
            assert!(torn_events < 100, "no re-emit of the held messages: {torn_events} events");
            assert_eq!(diag2.inbox_reads, diag.inbox_reads, "the unchanged inbox is not re-read: {diag2:?}");
            assert_eq!(diag_base.team_observations, diag.team_observations, "nothing changed, nothing handed on: {diag_base:?}");
            let (first_s, changed_s, base_s) = (first_read.as_secs_f64(), changed_read.as_secs_f64(), baseline.as_secs_f64());
            p.saw(
                "10 k inbox: discovery thread",
                format!(
                    "observer on its own thread; largest cycle read {} entries, applied {} in {} World batch; torn-config cycle applied {torn_applied} ({torn_events} events); unchanged reconcile handed on nothing (info, not asserted: sync {first_s:.1} s / {changed_s:.1} s / {base_s:.1} s)",
                    first.max_entries, first.max_processed, first.max_world_batches
                ),
            );

            // -- no crash ---------------------------------------------------------
            let d3 = settle(&c);
            let ipc = c.call("team_messages_page", json!({"teamKey": team.key.as_str(), "before": now_ms() + DAY_MS, "count": 1}));
            assert!(ipc.is_ok(), "IPC still answers: {ipc:?}");
            let last = snap(&c);
            assert!(has_sid(&last, &s.lead_sid));
            p.saw("after all", format!("no crash (discovery answers sync, reconciles={}; World serves snapshot; IPC answers)", d3.reconciles));
            p.finish();
        },
    )
    .await;
}
