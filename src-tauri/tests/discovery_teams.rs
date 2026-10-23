//! Team files through the engine (contract §5.2.4): the join (never by name
//! or `leadSessionId`), torn configs, inbox frames, task dirs that reset,
//! stale dirs and the S4 member facts.

#[path = "fixtures/vendor/support.rs"]
mod support;

use std::sync::Arc;

use gilde_code_lib::contract::services::SourceFacts;
use gilde_code_lib::contract::team::{IdleReason, LeadBinding};
use gilde_code_lib::discovery::Engine;
use support::*;

const START: i64 = 1_792_558_000;

struct Scene {
    h: Home,
    team: String,
    lead_sid: String,
    cwd: std::path::PathBuf,
}

/// A resumed lead (registry id ≠ the team name's boot id) with one live pane
/// teammate whose `--parent-session-id` is the lead's live id [R2 E9].
fn scene(p: &Arc<gilde_code_lib::discovery::procs::FakeProcs>) -> Scene {
    let h = Home::new();
    let cwd = h.workdir("proj");
    let team = "session-25ae1157".to_string();
    let lead_sid = "326555f6-0000-4000-8000-000000000001".to_string();
    h.write_registry(&h.claude, 1000, &lead_sid, START, &cwd, "interactive", "cli", "busy", "");
    p.insert(1000, claude_proc(START, 1, &["claude", "--teammate-mode", "tmux", "--resume", &lead_sid], &cwd));
    p.insert(1001, claude_proc(START + 10, 1, &["claude.exe", "--agent-id", &format!("alpha@{team}"), "--agent-name", "alpha", "--team-name", &team, "--agent-color", "blue", "--parent-session-id", &lead_sid, "--model", "haiku"], &cwd));
    let tdir = h.claude.join("teams").join(&team);
    std::fs::create_dir_all(tdir.join("inboxes")).unwrap();
    std::fs::write(
        tdir.join("config.json"),
        template("claude/teams/config.json", &[("TEAM", &team), ("CREATED_AT", &((START - 1) * 1000).to_string()), ("JOINED_AT", &((START + 10) * 1000).to_string()), ("BOOT_ID", "25ae1157"), ("CWD", cwd.to_str().unwrap())]),
    )
    .unwrap();
    std::fs::write(tdir.join("inboxes/team-lead.json"), template("claude/teams/inbox_team_lead.json", &[])).unwrap();
    std::fs::write(tdir.join("inboxes/alpha.json"), "[]").unwrap();
    let tasks = h.claude.join("tasks").join(&team);
    std::fs::create_dir_all(&tasks).unwrap();
    std::fs::write(tasks.join("1.json"), template("claude/tasks/1.json", &[])).unwrap();
    std::fs::write(tasks.join("2.json"), template("claude/tasks/2.json", &[])).unwrap();
    std::fs::write(tasks.join(".highwatermark"), "2").unwrap();
    // The teammate's own transcript (gives its session id).
    h.write_transcript(&h.claude, "-proj", &uuid(77), "claude/transcripts/teammate.jsonl", &[("CWD", cwd.to_str().unwrap()), ("AGENT", "alpha"), ("TEAM", &team)]);
    Scene { h, team, lead_sid, cwd }
}

#[test]
fn t_team_join_exact_by_parent_session_id_for_a_resumed_lead() {
    let p = procs();
    let s = scene(&p);
    let teams = Arc::new(TeamsRec::default());
    let rec = Arc::new(Rec::default());
    let history = s.h.history();
    history.refresh_index();
    history.fill();
    let mut e = Engine::new(s.h.config(p, teams.clone(), Some(history)), rec.clone());
    e.reconcile();
    let obs = teams.obs.lock().unwrap().last().cloned().expect("the team was observed");
    assert_eq!(obs.join.binding, LeadBinding::Exact);
    assert_eq!(obs.join.lead_session_id.as_deref(), Some(s.lead_sid.as_str()));
    assert!(obs.visible && obs.lead_alive);
    assert_eq!(obs.live_members.len(), 1);
    assert_eq!(obs.live_members[0].session_id.as_deref(), Some(uuid(77).as_str()), "member session id from its own transcript head");
    let cfg = obs.dir.config.as_ref().unwrap();
    assert_eq!(cfg.members.len(), 2);
    assert_eq!(obs.tasks.tasks.len(), 2);
    assert!(obs.tasks.exists);
    // S4 for the live member: roster isActive, the lead inbox's permission
    // request attributed to it, its last idle notification.
    let team_facts = rec.for_sid(&uuid(77)).into_iter().find_map(|f| match f {
        SourceFacts::Team(t) => Some(t),
        _ => None,
    });
    let t = team_facts.expect("S4 facts for the member");
    assert_eq!(t.is_active, Some(true));
    assert!(t.in_roster);
    assert_eq!(t.permission_request.map(|p| p.1), Some("Bash".to_string()));
    assert_eq!(t.last_idle.map(|i| i.1), Some(IdleReason::Available));
    // The teammate is a session with the team's account; never a registry entry.
    let meta = rec.for_sid(&uuid(77)).into_iter().find_map(|f| match f {
        SourceFacts::Meta(m) => Some(m),
        _ => None,
    });
    assert_eq!(meta.unwrap().teammate.unwrap().agent_name.as_deref(), Some("alpha"));
    let _ = (&s.team, &s.cwd);
}

#[test]
fn t_team_torn_config_keeps_the_last_good_roster() {
    let p = procs();
    let s = scene(&p);
    let teams = Arc::new(TeamsRec::default());
    let mut e = Engine::new(s.h.config(p, teams.clone(), None), Arc::new(Rec::default()));
    e.reconcile();
    let cfg_path = s.h.claude.join("teams").join(&s.team).join("config.json");
    let body = std::fs::read_to_string(&cfg_path).unwrap();
    std::fs::write(&cfg_path, &body[..body.len() / 3]).unwrap(); // non-atomic write caught mid-way
    e.reconcile();
    let obs = teams.obs.lock().unwrap().last().cloned().unwrap();
    assert!(obs.dir.roster_stale, "roster_stale after 3 failed retries");
    assert_eq!(obs.dir.config.as_ref().unwrap().members.len(), 2, "the last good parse is kept");
    assert!(e.diagnostics().team_torn >= 1);
    std::fs::write(&cfg_path, body).unwrap();
    e.reconcile();
    assert!(!teams.obs.lock().unwrap().last().unwrap().dir.roster_stale);
}

#[test]
fn t_team_stale_dir_and_task_reset() {
    let p = procs();
    let s = scene(&p);
    let teams = Arc::new(TeamsRec::default());
    // A 20-day-stale dir: no live lead, no live member → no team row.
    let stale = s.h.claude.join("teams/session-b9ad6eb0");
    std::fs::create_dir_all(&stale).unwrap();
    std::fs::write(stale.join("config.json"), template("claude/teams/config.json", &[("TEAM", "session-b9ad6eb0"), ("CREATED_AT", "1790000000000"), ("JOINED_AT", "1790000001000"), ("BOOT_ID", "b9ad6eb0"), ("CWD", "/nowhere")])).unwrap();
    let mut e = Engine::new(s.h.config(p, teams.clone(), None), Arc::new(Rec::default()));
    e.reconcile();
    let all = teams.obs.lock().unwrap().clone();
    let st = all.iter().find(|o| o.dir.dir_name == "session-b9ad6eb0").unwrap();
    assert!(!st.visible, "diagnostics only");
    assert_eq!(st.join.binding, LeadBinding::None);
    assert_eq!(e.diagnostics().teams_hidden, 1);
    // The 5 s reset deletes every task file; the dir stays (empty).
    for t in ["1", "2"] {
        std::fs::remove_file(s.h.claude.join("tasks").join(&s.team).join(format!("{t}.json"))).unwrap();
    }
    e.reconcile();
    let o = teams.obs.lock().unwrap().iter().rev().find(|o| o.dir.dir_name == s.team).cloned().unwrap();
    assert!(o.tasks.exists && o.tasks.tasks.is_empty());
    // The team dir disappears → team_gone.
    std::fs::remove_dir_all(s.h.claude.join("teams").join(&s.team)).unwrap();
    e.reconcile();
    assert!(teams.gone.lock().unwrap().contains(&s.team));
}

#[test]
fn t_team_heuristic_join_without_live_teammates() {
    let p = procs();
    let s = scene(&p);
    p.remove(1001); // the pane teammate exited
    let teams = Arc::new(TeamsRec::default());
    let mut e = Engine::new(s.h.config(p, teams.clone(), None), Arc::new(Rec::default()));
    e.reconcile();
    let o = teams.obs.lock().unwrap().iter().rev().find(|o| o.dir.dir_name == s.team).cloned().unwrap();
    assert_eq!(o.join.binding, LeadBinding::Heuristic, "createdAt within 2 s before the lead's startedAt, same cwd");
    assert_eq!(o.join.lead_session_id.as_deref(), Some(s.lead_sid.as_str()));
}
