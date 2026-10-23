//! A33.5(d): a 10 k-entry inbox must not stall discovery. Discovery keeps the
//! newest 5,000 entries of an inbox file, re-reads an inbox only when its file
//! identity changes, hands a team to the observer only when it changed, and
//! (as the threaded service) runs the observer on its own thread — asserted
//! with structural counters, never wall time (A10.7).

#[path = "fixtures/vendor/support.rs"]
mod support;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gilde_code_lib::contract::common::EpochMs;
use gilde_code_lib::contract::ids::AccountId;
use gilde_code_lib::discovery::claude::teams::INBOX_WINDOW;
use gilde_code_lib::discovery::reconcile::{TeamObservation, TeamObserver};
use gilde_code_lib::discovery::{Discovery, Engine};
use support::*;

const START: i64 = 1_792_558_000;
const TEAM: &str = "session-0a33d005";

/// A live lead with a team whose lead inbox holds `n` plain messages.
fn scene(h: &Home, p: &Arc<gilde_code_lib::discovery::procs::FakeProcs>, n: usize) -> std::path::PathBuf {
    let cwd = h.workdir("proj");
    let lead = uuid(51);
    h.write_registry(&h.claude, 4500, &lead, START, &cwd, "interactive", "cli", "idle", "");
    p.insert(4500, claude_proc(START, 1, &["claude", "--teammate-mode", "tmux"], &cwd));
    p.insert(4501, claude_proc(START + 10, 1, &["claude.exe", "--agent-id", &format!("alpha@{TEAM}"), "--agent-name", "alpha", "--team-name", TEAM, "--agent-color", "blue", "--parent-session-id", &lead], &cwd));
    let tdir = h.claude.join("teams").join(TEAM);
    std::fs::create_dir_all(tdir.join("inboxes")).unwrap();
    std::fs::write(
        tdir.join("config.json"),
        template("claude/teams/config.json", &[("TEAM", TEAM), ("CREATED_AT", &((START - 1) * 1000).to_string()), ("JOINED_AT", &((START + 10) * 1000).to_string()), ("BOOT_ID", "0a33d005"), ("CWD", cwd.to_str().unwrap())]),
    )
    .unwrap();
    let inbox = tdir.join("inboxes/team-lead.json");
    write_inbox(&inbox, n);
    inbox
}

fn write_inbox(path: &Path, n: usize) {
    let base = START * 1000;
    let entries: Vec<serde_json::Value> = (0..n)
        .map(|i| serde_json::json!({"from": "alpha", "text": format!("m {i}"), "timestamp": base + i as i64 * 1_000, "read": true, "msg_id": format!("m-{i}")}))
        .collect();
    // Replaced by rename, as the CLI does (a new file identity).
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec(&entries).unwrap()).unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

fn lead_inbox(o: &TeamObservation) -> Vec<String> {
    o.dir.inboxes.get("team-lead").map(|v| v.iter().filter_map(|e| e.msg_id.clone()).collect()).unwrap_or_default()
}

#[test]
fn t_disc_inbox_window_cache_and_change_driven_observations() {
    let h = Home::new();
    let p = procs();
    let inbox = scene(&h, &p, 10_000);
    let teams = Arc::new(TeamsRec::default());
    let rec = Arc::new(Rec::default());
    let mut e = Engine::new(h.config(p, teams.clone(), None), rec);
    e.reconcile();
    let d1 = e.diagnostics();
    let obs = teams.obs.lock().unwrap().clone();
    assert_eq!(obs.len(), 1, "one observation");
    let ids = lead_inbox(&obs[0]);
    assert_eq!(ids.len(), INBOX_WINDOW, "the newest 5,000 entries");
    assert_eq!(ids.first().map(String::as_str), Some("m-5000"));
    assert_eq!(ids.last().map(String::as_str), Some("m-9999"));
    assert_eq!(d1.inbox_entries_dropped, 5_000, "{d1:?}");
    assert_eq!((d1.inbox_reads, d1.team_observations), (1, 1), "{d1:?}");

    // Nothing changed: served from the cache, nothing handed on.
    e.reconcile();
    e.reconcile();
    let d2 = e.diagnostics();
    assert_eq!(d2.inbox_reads, 1, "never re-read: {d2:?}");
    assert!(d2.inbox_cache_hits >= 2, "{d2:?}");
    assert_eq!(d2.team_observations, 1, "no re-observation: {d2:?}");
    assert_eq!(teams.obs.lock().unwrap().len(), 1);

    // One new entry: read once, one observation, the window slides.
    write_inbox(&inbox, 10_001);
    e.reconcile();
    let d3 = e.diagnostics();
    assert_eq!((d3.inbox_reads, d3.team_observations), (2, 2), "{d3:?}");
    let last = teams.obs.lock().unwrap().last().cloned().unwrap();
    let ids = lead_inbox(&last);
    assert_eq!(ids.len(), INBOX_WINDOW);
    assert_eq!(ids.last().map(String::as_str), Some("m-10000"));
}

/// Records which thread delivered each observation.
#[derive(Default)]
struct ThreadRec(Mutex<Vec<(String, usize)>>);

impl TeamObserver for ThreadRec {
    fn team(&self, obs: TeamObservation) {
        let name = std::thread::current().name().unwrap_or("").to_string();
        self.0.lock().unwrap().push((name, lead_inbox(&obs).len()));
    }
    fn team_gone(&self, _account: &AccountId, _dir_name: &str, _at: EpochMs) {}
}

/// The threaded service: the observer runs on `gilde-teams`, never on the
/// discovery thread, and `sync` returns only after it applied what the
/// queued commands produced.
#[test]
fn t_disc_team_observer_runs_off_the_discovery_thread() {
    let h = Home::new();
    let p = procs();
    let inbox = scene(&h, &p, 10_000);
    let seen = Arc::new(ThreadRec::default());
    let mut cfg = h.config(p, seen.clone(), None);
    cfg.watch = false;
    let disc = Discovery::start(cfg, Arc::new(Rec::default()));
    let d = disc.sync(Duration::from_secs(120)).expect("discovery answers sync");
    assert!(d.team_observer_queued, "{d:?}");
    {
        let got = seen.0.lock().unwrap();
        assert!(!got.is_empty(), "applied before sync returned");
        assert!(got.iter().all(|(t, _)| t == "gilde-teams"), "never on the discovery thread: {got:?}");
        assert_eq!(got[0].1, INBOX_WINDOW);
    }
    write_inbox(&inbox, 10_001);
    disc.reconcile_now();
    let d2 = disc.sync(Duration::from_secs(120)).expect("sync");
    assert_eq!(d2.inbox_reads, 2, "{d2:?}");
    assert!(seen.0.lock().unwrap().iter().all(|(t, _)| t == "gilde-teams"));
    disc.stop();
}
