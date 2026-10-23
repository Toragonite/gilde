//! The health builder on synthetic scans: every rule without a process.

use std::cell::Cell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use super::*;
use crate::sidecar::{
    CcoHostInfo, CcoMemberState, CcoOwner, CcoProcessIdentity, CcoTeamState, CcoTeamView, CcoTurn,
    CcoWorktree,
};
use crate::store::models::{ProcKind, ProcState};
use crate::supervise::procscan::PsRow;

const GILDE: i32 = 999_999;
const MIB: u64 = 1024 * 1024;
const TEAM: &str = "tmabcdefghi1";
const DATA: &str = "/Users/u/.claude-code-orchestrator";
const WT_ALPHA: &str = "/Users/u/.claude-code-orchestrator/worktrees/h1/demo-x1/alpha";
const WT_BETA: &str = "/Users/u/.claude-code-orchestrator/worktrees/h1/demo-x1/beta";
const HOST_ARGS: &str =
    "node /Users/u/.claude-code-orchestrator/mcp/server.js --host /Users/u/.claude-code-orchestrator/run/s50.spec.json";

// -- fixtures ----------------------------------------------------------------

fn ps(pid: i32, ppid: i32, pgid: i32, rss_mib: u64, cpu: f32, args: &str) -> PsRow {
    PsRow {
        pid,
        ppid,
        pgid,
        rss_kb: rss_mib * 1024,
        cpu_pct: cpu,
        elapsed_secs: 600,
        user: "u".into(),
        args: args.into(),
    }
}

fn row_of(p: &PsRow) -> ProcRow {
    ProcRow {
        pid: p.pid,
        ppid: p.ppid,
        pgid: p.pgid,
        name: p.name(),
        argv_display: p.args.clone(),
        rss_bytes: p.rss_bytes(),
        cpu_pct: p.cpu_pct,
        elapsed_secs: p.elapsed_secs,
        start_time: 1_700_000_000_000 + i64::from(p.pid),
        cwd: None,
        user_owned: true,
        group: if agent_kind(&p.args).is_some() {
            ProcGroupKind::AgentUnmanaged
        } else {
            ProcGroupKind::Other
        },
        tags: Vec::new(),
        owner: None,
        spawn_id: None,
    }
}

/// launchd, the lead session (an interactive `claude` in a terminal), its
/// dispatch host, member alpha's CLI (+ a big tool child) and beta's CLI.
fn machine() -> Vec<PsRow> {
    vec![
        ps(1, 0, 1, 10, 0.0, "/sbin/launchd"),
        ps(40, 1, 40, 50, 0.0, "/System/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal"),
        ps(45, 40, 45, 5, 0.0, "login -pf u"),
        ps(46, 45, 46, 5, 0.0, "-zsh"),
        ps(50, 46, 50, 400, 3.0, "claude"),
        ps(100, 1, 100, 120, 1.0, HOST_ARGS),
        ps(200, 100, 200, 300, 10.0, "/opt/bin/claude -p --output-format stream-json"),
        ps(201, 200, 200, 2000, 80.0, "cargo build"),
        ps(300, 100, 300, 200, 5.0, "node /opt/lib/codex/bin/codex exec"),
    ]
}

struct FakeProbe {
    cwd: HashMap<i32, PathBuf>,
    start: HashMap<i32, i64>,
    /// Readable environments; a pid not in here is unreadable (`None`).
    env: HashMap<i32, Vec<String>>,
    calls: Cell<usize>,
}

impl FakeProbe {
    fn new() -> FakeProbe {
        let mut cwd = HashMap::new();
        cwd.insert(200, PathBuf::from(WT_ALPHA));
        cwd.insert(300, PathBuf::from(format!("{WT_BETA}/src")));
        cwd.insert(50, PathBuf::from("/Users/u/repo"));
        let mut start = HashMap::new();
        start.insert(50, 1_699_999_000_000);
        let mut env = HashMap::new();
        // The engine's marker on every member turn process (host-protocol
        // TEAM_MEMBER_ENV / DISPATCHED_TASK_ENV).
        env.insert(200, marker_env("alpha", Some("turn-alpha")));
        env.insert(300, marker_env("beta", None));
        env.insert(50, vec!["HOME=/Users/u".to_string()]);
        FakeProbe {
            cwd,
            start,
            env,
            calls: Cell::new(0),
        }
    }
}

impl Probe for FakeProbe {
    fn cwd(&self, pid: i32) -> Option<PathBuf> {
        self.calls.set(self.calls.get() + 1);
        self.cwd.get(&pid).cloned()
    }

    fn start_ms(&self, pid: i32) -> Option<i64> {
        self.start.get(&pid).copied()
    }

    fn environ(&self, pid: i32) -> Option<Vec<String>> {
        self.env.get(&pid).cloned()
    }
}

/// `CCO_TEAM_MEMBER=<TEAM>:<member>:1` (+ `CCO_DISPATCHED_TASK=<turn>`).
fn marker_env(member: &str, turn: Option<&str>) -> Vec<String> {
    marker_env_for(TEAM, member, turn)
}

fn marker_env_for(team_id: &str, member: &str, turn: Option<&str>) -> Vec<String> {
    let mut env = vec![
        "HOME=/Users/u".to_string(),
        format!("CCO_TEAM_MEMBER={team_id}:{member}:1"),
    ];
    if let Some(turn) = turn {
        env.push(format!("CCO_DISPATCHED_TASK={turn}"));
    }
    env
}

fn member(name: &str, worktree: &str, turn_pid: Option<i64>) -> CcoMemberState {
    CcoMemberState {
        name: name.into(),
        state: if turn_pid.is_some() { "running" } else { "idle" }.into(),
        worktree: CcoWorktree {
            path: worktree.into(),
            ..CcoWorktree::default()
        },
        turn: turn_pid.map(|pid| CcoTurn {
            id: format!("turn-{name}"),
            n: 3,
            started_at: 1_000,
            pid: Some(pid),
            last_output_at: None,
        }),
        incarnation: 1,
        ..CcoMemberState::default()
    }
}

fn team(status: &str, live: bool, members: Vec<CcoMemberState>) -> TeamFact {
    TeamFact {
        view: CcoTeamView {
            team_id: TEAM.into(),
            rev: 1,
            source: if live { "host" } else { "disk" }.into(),
            live,
            believed: live || status != "active",
            state: CcoTeamState {
                id: TEAM.into(),
                name: "demo".into(),
                status: status.into(),
                owner: Some(CcoOwner {
                    host_pid: 100,
                    host_started_at: 1_699_999_500_000,
                    session: CcoProcessIdentity {
                        pid: 50,
                        started_at: 1_699_999_000_000,
                    },
                }),
                members,
                ..CcoTeamState::default()
            },
            ..CcoTeamView::default()
        },
        running_at_last_live: Vec::new(),
    }
}

fn live_team() -> TeamFact {
    team(
        "active",
        true,
        vec![member("alpha", WT_ALPHA, Some(200)), member("beta", WT_BETA, None)],
    )
}

fn facts(teams: Vec<TeamFact>) -> TeamFacts {
    TeamFacts {
        known: true,
        data_root: Some(DATA.into()),
        teams,
        hosts: vec![CcoHostInfo {
            key: "s50".into(),
            session_pid: 50,
            host_pid: 100,
            state: "connected".into(),
            ..CcoHostInfo::default()
        }],
    }
}

struct World {
    rows: Vec<PsRow>,
    registry: Vec<RegistryRow>,
    facts: TeamFacts,
    pressure: PressureLevel,
    ram: u64,
    probe: FakeProbe,
    /// monitor-row groups to override (registry rows are `oursLive/oursOrphan`).
    groups: HashMap<i32, (ProcGroupKind, Vec<ProcTag>)>,
}

impl World {
    fn new(rows: Vec<PsRow>, facts: TeamFacts) -> World {
        World {
            rows,
            registry: Vec::new(),
            facts,
            pressure: PressureLevel::Normal,
            ram: 64 * 1024 * MIB,
            probe: FakeProbe::new(),
            groups: HashMap::new(),
        }
    }

    fn tick(&self, builder: &mut HealthBuilder, at: i64) -> HealthSnapshot {
        let snapshot = Snapshot::from_rows(self.rows.clone());
        let rows: Vec<ProcRow> = self
            .rows
            .iter()
            .map(|p| {
                let mut r = row_of(p);
                if let Some((group, tags)) = self.groups.get(&p.pid) {
                    r.group = *group;
                    r.tags = tags.clone();
                }
                r
            })
            .collect();
        let input = HealthInput {
            at,
            snapshot: &snapshot,
            rows: &rows,
            registry: &self.registry,
            facts: &self.facts,
            pressure: self.pressure,
            physical_ram_bytes: self.ram,
            self_pid: GILDE,
        };
        builder.build(&input, &self.probe)
    }
}

fn agent(s: &HealthSnapshot, pid: i32) -> &HealthAgent {
    s.agents
        .iter()
        .find(|a| a.pid == pid)
        .unwrap_or_else(|| panic!("pid {pid} is not an agent: {:?}", s.agents))
}

fn alert<'a>(s: &'a HealthSnapshot, id: &str) -> Option<&'a HealthAlert> {
    s.alerts.iter().find(|a| a.id == id)
}

fn registry_row(session: &str, pid: i64, state: ProcState) -> RegistryRow {
    RegistryRow {
        spawn_id: format!("sp-{session}"),
        app_instance: "inst".into(),
        generation: 0,
        kind: ProcKind::Pty,
        vendor: None,
        session_id: Some(session.into()),
        run_id: None,
        state,
        pid: Some(pid),
        pgid: Some(pid),
        proc_start_ts: Some(1),
        argv: "[]".into(),
        argv_sig: "sig".into(),
        cwd: "/tmp".into(),
        env_marker: "m".into(),
        config_dir: None,
        worktree_id: None,
        keep_alive: false,
        planned_at: 0,
        started_at: Some(1),
        exited_at: None,
        exit_code: None,
        exit_signal: None,
        note: None,
    }
}

// -- attribution -------------------------------------------------------------

#[test]
fn attribution_by_turn_pid_marker_cwd_session_and_other() {
    let mut rows = machine();
    // A Gilde terminal session (zsh) running claude inside it.
    rows.push(ps(500, GILDE, 500, 5, 0.0, "/bin/zsh -l"));
    rows.push(ps(501, 500, 500, 100, 1.0, "claude --resume"));
    // An agent in a directory shaped like another team's member worktree.
    rows.push(ps(600, 1, 600, 50, 0.0, "/opt/bin/gemini"));
    let mut w = World::new(rows, facts(vec![live_team()]));
    w.registry.push(registry_row("sess-1", 500, ProcState::Running));
    w.probe.cwd.insert(
        600,
        PathBuf::from(format!("{DATA}/worktrees/h9/other-team-k2/gamma/deep")),
    );
    let mut b = HealthBuilder::new(HealthConfig::default());
    let s = w.tick(&mut b, 10_000);

    // By the running turn's pid — no cwd read needed.
    match &agent(&s, 200).attribution {
        HealthAttribution::TeamMember { team_id, team, member, worktree } => {
            assert_eq!(team_id.as_deref(), Some(TEAM));
            assert_eq!(team, "demo");
            assert_eq!(member, "alpha");
            assert_eq!(worktree, WT_ALPHA);
        }
        other => panic!("{other:?}"),
    }
    // By the engine's marker in its environment.
    assert!(matches!(&agent(&s, 300).attribution,
        HealthAttribution::TeamMember { member, .. } if member == "beta"));
    // By cwd — display only, and only for a member running under a live host.
    let mut w2 = World::new(machine(), facts(vec![team("active", true, vec![
        member("alpha", WT_ALPHA, Some(4444)),
        member("beta", WT_BETA, None),
    ])]));
    w2.probe.env.clear();
    let s2 = w2.tick(&mut HealthBuilder::new(HealthConfig::default()), 10_000);
    assert!(matches!(&agent(&s2, 200).attribution,
        HealthAttribution::TeamMember { member, .. } if member == "alpha"), "alpha runs: its worktree counts");
    assert_eq!(agent(&s2, 300).attribution, HealthAttribution::Other, "beta is idle: no cwd attribution");
    assert_eq!(agent(&s, 300).agent, "codex");
    // Gilde's own session.
    assert_eq!(
        agent(&s, 501).attribution,
        HealthAttribution::GildeSession { session_id: "sess-1".into() }
    );
    // The lead's interactive CLI in a repository: other.
    assert_eq!(agent(&s, 50).attribution, HealthAttribution::Other);
    // A directory shaped like a member worktree is no evidence of anything:
    // no team is ever named from a directory.
    assert_eq!(agent(&s, 600).attribution, HealthAttribution::Other);
    // The host itself is never an agent.
    assert!(s.agents.iter().all(|a| a.pid != 100));
}

#[test]
fn the_subtree_folds_tools_and_nested_agents_and_takes_the_group_only_when_it_leads() {
    let mut rows = machine();
    // alpha's CLI started another claude (a subagent) — folded into alpha.
    rows.push(ps(202, 200, 200, 100, 1.0, "claude -p sub"));
    // A CLI that shares its launcher's group (pgid 100 = the host's): its
    // "group" must not pull the host and every other member in.
    rows.push(ps(400, 100, 100, 70, 0.0, "/opt/bin/agy run"));
    let w = World::new(rows, facts(vec![live_team()]));
    let mut b = HealthBuilder::new(HealthConfig::default());
    let s = w.tick(&mut b, 10_000);
    let alpha = agent(&s, 200);
    assert_eq!(alpha.pids, vec![200, 201, 202]);
    assert_eq!(alpha.rss_bytes, (300 + 2000 + 100) * MIB);
    assert!(s.agents.iter().all(|a| a.pid != 202), "a nested CLI is not a second agent");
    let agy = agent(&s, 400);
    assert_eq!(agy.pids, vec![400], "not the leader of group 100: no group members");
    // Totals count each pid once: agents + the host's own RSS.
    let expected: u64 = s.agents.iter().map(|a| a.rss_bytes).sum::<u64>() + 120 * MIB;
    assert_eq!(s.memory.agents_rss_bytes, expected);
}

// -- leftovers -----------------------------------------------------------------

/// The host is gone and alpha's CLI was re-parented to launchd.
fn host_died() -> Vec<PsRow> {
    let mut rows: Vec<PsRow> = machine().into_iter().filter(|r| r.pid != 100).collect();
    for r in rows.iter_mut() {
        if r.pid == 200 || r.pid == 300 {
            r.ppid = 1;
        }
    }
    rows
}

#[test]
fn a_leftover_needs_two_consecutive_ticks_and_other_is_never_one() {
    let w = World::new(host_died(), facts(vec![team("ended", false, vec![
        member("alpha", WT_ALPHA, None),
        member("beta", WT_BETA, None),
    ])]));
    let mut b = HealthBuilder::new(HealthConfig::default());
    let first = w.tick(&mut b, 10_000);
    assert!(first.orphans.is_empty(), "one tick is not enough: {:?}", first.orphans);
    assert!(alert(&first, &format!("orphans:{TEAM}")).is_none());
    let second = w.tick(&mut b, 15_000);
    assert_eq!(second.orphans, vec![200, 300]);
    assert!(!agent(&second, 50).orphan, "an `other` CLI is never a leftover");
    let a = alert(&second, &format!("orphans:{TEAM}")).expect("orphans alert");
    assert_eq!(a.severity, HealthSeverity::Action);
    assert_eq!(a.params["count"], serde_json::json!(2));
    assert_eq!(a.params["members"], serde_json::json!(["alpha", "beta"]));
    assert_eq!(a.params["team"], serde_json::json!("demo"));
    assert_eq!(a.body_key, "health.alert.orphans.body");
    assert_eq!(a.actions[0].kind, HealthActionKind::CleanUp);
    assert_eq!(a.actions[0].pids, Some(vec![200, 300]), "exactly the leftover pids");
    assert_eq!(second.overall, HealthLevel::Action);
    let team_row = second.teams.iter().find(|t| t.team_id.as_deref() == Some(TEAM)).expect("team");
    assert!(team_row
        .members
        .iter()
        .all(|m| m.state == HealthMemberState::Leftover));

    // The condition breaks for one tick (the team is live again): the streak resets.
    let mut back = World::new(machine(), facts(vec![live_team()]));
    back.probe = FakeProbe::new();
    let s = back.tick(&mut b, 20_000);
    assert!(s.orphans.is_empty());
    assert!(alert(&s, &format!("orphans:{TEAM}")).is_none(), "cleared when it ends");
    let again = w.tick(&mut b, 25_000);
    assert!(again.orphans.is_empty(), "the debounce starts over");
}

#[test]
fn nothing_is_a_leftover_under_a_live_host_a_terminal_or_unknown_team_facts() {
    // The team ended but the host is still alive and still the parent.
    let mut ended_live_host = World::new(machine(), facts(vec![team("ended", false, vec![
        member("alpha", WT_ALPHA, None),
        member("beta", WT_BETA, None),
    ])]));
    let mut b = HealthBuilder::new(HealthConfig::default());
    ended_live_host.tick(&mut b, 1);
    let s = ended_live_host.tick(&mut b, 2);
    assert!(s.orphans.is_empty(), "a live host above it: {:?}", s.orphans);

    // A person runs a CLI in a member worktree from a terminal (login shell above).
    let mut rows = host_died();
    rows.push(ps(700, 46, 700, 50, 0.0, "claude"));
    ended_live_host.rows = rows;
    ended_live_host.probe.cwd.insert(700, PathBuf::from(WT_ALPHA));
    ended_live_host.probe.env.insert(700, marker_env("alpha", None));
    let mut b = HealthBuilder::new(HealthConfig::default());
    ended_live_host.tick(&mut b, 1);
    let s = ended_live_host.tick(&mut b, 2);
    assert!(!agent(&s, 700).orphan, "a terminal above it is a person's");
    assert!(agent(&s, 200).orphan);

    // A launch wrapper (sh) between launchd and the CLI is still a leftover.
    let mut rows = host_died();
    rows.push(ps(800, 1, 800, 1, 0.0, "/bin/sh -c claude -p"));
    for r in rows.iter_mut().filter(|r| r.pid == 200) {
        r.ppid = 800;
    }
    let mut w = World::new(rows, facts(vec![team("ended", false, vec![member("alpha", WT_ALPHA, None)])]));
    w.probe = FakeProbe::new();
    let mut b = HealthBuilder::new(HealthConfig::default());
    w.tick(&mut b, 1);
    assert!(agent(&w.tick(&mut b, 2), 200).orphan);

    // Team facts unknown (the observer is down): nothing is decided.
    let mut unknown = facts(vec![team("ended", false, vec![member("alpha", WT_ALPHA, None)])]);
    unknown.known = false;
    let w = World::new(host_died(), unknown);
    let mut b = HealthBuilder::new(HealthConfig::default());
    w.tick(&mut b, 1);
    let s = w.tick(&mut b, 2);
    assert!(s.orphans.is_empty());
    assert!(!s.teams_known);
}

#[test]
fn a_member_with_its_own_running_turn_is_not_a_leftover_but_a_stale_one_is() {
    // Host alive per the view, but the CLI is re-parented (the host restarted).
    let mut rows = machine();
    for r in rows.iter_mut().filter(|r| r.pid == 200) {
        r.ppid = 1;
    }
    let w = World::new(rows.clone(), facts(vec![live_team()]));
    let mut b = HealthBuilder::new(HealthConfig::default());
    w.tick(&mut b, 1);
    assert!(!agent(&w.tick(&mut b, 2), 200).orphan, "its member is running this very turn");

    // The member's running turn is another process, and pid 200's marker
    // names an older turn: a stale one.
    let mut w = World::new(
        rows.clone(),
        facts(vec![team("active", true, vec![member("alpha", WT_ALPHA, Some(4444))])]),
    );
    w.probe.env.insert(200, marker_env("alpha", Some("turn-older")));
    let mut b = HealthBuilder::new(HealthConfig::default());
    w.tick(&mut b, 1);
    assert!(agent(&w.tick(&mut b, 2), 200).orphan);

    // Its marker names the turn that is running right now: never a leftover,
    // whatever pid the engine recorded.
    w.probe.env.insert(200, marker_env("alpha", Some("turn-alpha")));
    let mut b = HealthBuilder::new(HealthConfig::default());
    w.tick(&mut b, 1);
    assert!(!agent(&w.tick(&mut b, 2), 200).orphan);

    // Another incarnation of the member: stale.
    w.probe.env.insert(200, vec!["CCO_TEAM_MEMBER=tmabcdefghi1:alpha:0".into(), "CCO_DISPATCHED_TASK=turn-alpha".into()]);
    let mut b = HealthBuilder::new(HealthConfig::default());
    w.tick(&mut b, 1);
    assert!(agent(&w.tick(&mut b, 2), 200).orphan);
}

#[test]
fn a_gilde_session_leftover_is_its_registry_row_classified_ours_orphan_with_gilde_gone() {
    let mut rows = machine();
    rows.push(ps(900, 1, 900, 30, 0.0, "claude"));
    let mut w = World::new(rows, facts(vec![live_team()]));
    w.registry.push(registry_row("sess-old", 900, ProcState::Running));
    w.groups.insert(900, (ProcGroupKind::OursOrphan, vec![ProcTag::Ours, ProcTag::Orphan]));
    let mut b = HealthBuilder::new(HealthConfig::default());
    w.tick(&mut b, 1);
    let s = w.tick(&mut b, 2);
    assert!(agent(&s, 900).orphan);
    let a = alert(&s, "orphans:gilde").expect("gilde leftovers");
    assert_eq!(a.body_key, "health.alert.orphans.bodyGilde");

    // Live rows are not.
    w.groups.insert(900, (ProcGroupKind::OursLive, vec![ProcTag::Ours]));
    let mut b = HealthBuilder::new(HealthConfig::default());
    w.tick(&mut b, 1);
    assert!(!agent(&w.tick(&mut b, 2), 900).orphan);
}

// -- hosts ---------------------------------------------------------------------

#[test]
fn hosts_are_detected_with_their_lead_session_and_a_lonely_host_is_reported_after_ten_minutes() {
    let w = World::new(machine(), facts(vec![live_team()]));
    let mut b = HealthBuilder::new(HealthConfig::default());
    let s = w.tick(&mut b, 1);
    assert_eq!(s.hosts.len(), 1);
    let h = &s.hosts[0];
    assert_eq!(h.pid, 100);
    assert_eq!(h.rss_bytes, 120 * MIB, "its own process only");
    assert_eq!(h.session_pid, Some(50));
    assert_eq!(h.lead_alive, Some(true));
    assert_eq!(h.team_ids, vec![TEAM.to_string()]);
    assert!(is_dispatch_host(HOST_ARGS));
    assert!(!is_dispatch_host("node /Users/u/.claude-code-orchestrator/mcp/server.js"));
    assert!(is_dispatch_host("node /x/out/mcp/server.js --host y"), "a checkout host");
    assert!(!is_dispatch_host("server.js --host y"), "a runtime runs it");
    assert!(!is_dispatch_host("node /x/server.js --team-mcp y"));
    assert!(!is_dispatch_host("vim /x/server.js --host"), "an editor on the file is no host");
    assert!(is_dispatch_host("/Applications/X.app/Contents/MacOS/gilde-node /x/server.js --host y"));

    // The lead session ended; the host lingers.
    let rows: Vec<PsRow> = machine().into_iter().filter(|r| r.pid != 50).collect();
    let w = World::new(rows, facts(vec![live_team()]));
    let mut b = HealthBuilder::new(HealthConfig::default());
    let s = w.tick(&mut b, 0);
    assert_eq!(s.hosts[0].lead_alive, Some(false));
    assert!(alert(&s, "host-no-lead:100").is_none());
    let s = w.tick(&mut b, HOST_NO_LEAD_MS - 1);
    assert!(alert(&s, "host-no-lead:100").is_none());
    let s = w.tick(&mut b, HOST_NO_LEAD_MS);
    let a = alert(&s, "host-no-lead:100").expect("lonely host");
    assert_eq!(a.severity, HealthSeverity::Attention);
    assert_eq!(a.params["minutes"], serde_json::json!(10));
    assert_eq!(s.overall, HealthLevel::Attention);
}

#[test]
fn a_team_that_lost_its_host_while_working_says_how_to_resume_and_a_live_lead_is_informational() {
    let mut fact = team("active", false, vec![member("alpha", WT_ALPHA, None)]);
    fact.running_at_last_live = vec!["alpha".into()];
    let rows: Vec<PsRow> = machine().into_iter().filter(|r| r.pid != 100).collect();
    let w = World::new(rows.clone(), facts(vec![fact.clone()]));
    let mut b = HealthBuilder::new(HealthConfig::default());
    let s = w.tick(&mut b, 1);
    let a = alert(&s, &format!("host-lost:{TEAM}")).expect("host lost");
    assert_eq!(a.severity, HealthSeverity::Action);
    assert_eq!(a.params["command"], serde_json::json!("cco team_resume --team demo"));
    assert_eq!(a.actions[0].kind, HealthActionKind::CopyCommand);
    assert_eq!(a.actions[0].command.as_deref(), Some("cco team_resume --team demo"));
    assert!(alert(&s, &format!("host-gone:50:{TEAM}")).is_none(), "the action alert says it already");

    // Nobody was working: only the informational one, since the lead is alive.
    fact.running_at_last_live.clear();
    let w = World::new(rows, facts(vec![fact]));
    let mut b = HealthBuilder::new(HealthConfig::default());
    let s = w.tick(&mut b, 1);
    assert!(alert(&s, &format!("host-lost:{TEAM}")).is_none());
    let a = alert(&s, &format!("host-gone:50:{TEAM}")).expect("host gone, lead alive");
    assert_eq!(a.severity, HealthSeverity::Attention);
}

// -- memory --------------------------------------------------------------------

#[test]
fn memory_thresholds_follow_ram_and_hold_with_hysteresis() {
    let cfg = HealthConfig::default();
    assert_eq!(cfg.memory_thresholds(64 * 1024 * MIB), (4 * 1024 * MIB, 8 * 1024 * MIB));
    assert_eq!(cfg.memory_thresholds(16 * 1024 * MIB), (4 * 1024 * MIB, 4 * 1024 * MIB));
    assert_eq!(cfg.memory_thresholds(8 * 1024 * MIB), (2 * 1024 * MIB, 2 * 1024 * MIB));
    assert_eq!(cfg.memory_thresholds(0).1, 8 * 1024 * MIB);

    let mut rows = machine();
    let set_tool = |rows: &mut Vec<PsRow>, mib: u64| {
        for r in rows.iter_mut().filter(|r| r.pid == 201) {
            r.rss_kb = mib * 1024;
        }
    };
    set_tool(&mut rows, 5 * 1024);
    let mut w = World::new(rows.clone(), facts(vec![live_team()]));
    let mut b = HealthBuilder::new(cfg);
    let s = w.tick(&mut b, 1);
    let id = format!("mem-agent:200-{}", 1_700_000_000_000i64 + 200);
    let a = alert(&s, &id).expect("over 4 GiB");
    assert_eq!(a.severity, HealthSeverity::Attention);
    assert_eq!(a.body_key, "health.alert.memAgent.bodyMember");
    assert_eq!(a.params["command"], serde_json::json!("cco team_member alpha --stop"));
    assert_eq!(a.params["member"], serde_json::json!("alpha"));
    assert!(a.actions.iter().all(|x| x.kind == HealthActionKind::CopyCommand), "never a kill");
    let since = a.since;

    // Escalates to action at 8 GiB, updated in place.
    set_tool(&mut rows, 9 * 1024);
    w.rows = rows.clone();
    let s = w.tick(&mut b, 2);
    let a = alert(&s, &id).expect("held");
    assert_eq!(a.severity, HealthSeverity::Action);
    assert_eq!(a.since, since, "the same alert, updated");
    assert_eq!(a.raised_at, 2, "an escalation is a re-raise");

    // Just under 4 GiB: held (hysteresis); well under: cleared.
    set_tool(&mut rows, 3 * 1024 + 900);
    w.rows = rows.clone();
    assert!(alert(&w.tick(&mut b, 3), &id).is_some());
    set_tool(&mut rows, 1024);
    w.rows = rows;
    assert!(alert(&w.tick(&mut b, 4), &id).is_none());
}

#[test]
fn low_memory_names_the_top_three_and_follows_pressure() {
    let mut w = World::new(machine(), facts(vec![live_team()]));
    let mut b = HealthBuilder::new(HealthConfig::default());
    assert!(alert(&w.tick(&mut b, 1), "mem-low").is_none());
    w.pressure = PressureLevel::Warn;
    let s = w.tick(&mut b, 2);
    let a = alert(&s, "mem-low").expect("warn");
    assert_eq!(a.severity, HealthSeverity::Attention);
    let top = a.params["top"].as_array().expect("top");
    assert_eq!(top.len(), 3);
    assert_eq!(top[0]["label"], serde_json::json!("alpha (demo)"));
    assert_eq!(top[0]["rssBytes"], serde_json::json!(2300 * MIB));
    assert_eq!(a.params["rssBytes"], serde_json::json!(s.memory.agents_rss_bytes));
    w.pressure = PressureLevel::Critical;
    let s = w.tick(&mut b, 3);
    assert_eq!(alert(&s, "mem-low").expect("critical").severity, HealthSeverity::Action);
    assert_eq!(s.overall, HealthLevel::Action);
    w.pressure = PressureLevel::Normal;
    assert!(alert(&w.tick(&mut b, 4), "mem-low").is_none());
}

// -- stuck turns -----------------------------------------------------------------

#[test]
fn a_turn_longer_than_any_team_allows_with_no_cpu_for_five_minutes_is_stuck() {
    let mut rows = machine();
    for r in rows.iter_mut().filter(|r| r.pid == 200 || r.pid == 201) {
        r.cpu_pct = 0.1;
    }
    let mut w = World::new(rows.clone(), facts(vec![live_team()]));
    let mut b = HealthBuilder::new(HealthConfig::default());
    let started = 1_000; // the fixture's turn.startedAt
    let past = started + MAX_TEAM_TURN_TIMEOUT_MINUTES * 60_000 + STUCK_GRACE_MS + 1;
    let id = format!("turn-stuck:{TEAM}:alpha:turn-alpha");
    // Five minutes of quiet samples, one every five seconds.
    let mut s = w.tick(&mut b, past);
    assert!(alert(&s, &id).is_none(), "no history yet");
    let mut at = past;
    while at < past + STUCK_IDLE_WINDOW_MS {
        at += 5_000;
        s = w.tick(&mut b, at);
    }
    let a = alert(&s, &id).expect("stuck");
    assert_eq!(a.severity, HealthSeverity::Attention);
    assert_eq!(a.params["command"], serde_json::json!("cco team_member alpha --interrupt"));
    assert!(a.params["minutes"].as_i64().unwrap_or(0) >= 242);
    assert_eq!(a.params["limitMinutes"], serde_json::json!(240));

    // Activity clears it.
    for r in rows.iter_mut().filter(|r| r.pid == 201) {
        r.cpu_pct = 50.0;
    }
    w.rows = rows;
    assert!(alert(&w.tick(&mut b, at + 5_000), &id).is_none());

    // Inside the timeout nothing is stuck, however quiet.
    let w = World::new(machine(), facts(vec![live_team()]));
    let mut b = HealthBuilder::new(HealthConfig::default());
    let mut at = started;
    for _ in 0..80 {
        at += 5_000;
        assert!(alert(&w.tick(&mut b, at), &id).is_none());
    }
}

// -- the alert lifecycle ---------------------------------------------------------

#[test]
fn raise_update_clear_dismiss_cooldown_and_escalation() {
    let mut w = World::new(machine(), facts(vec![live_team()]));
    let mut b = HealthBuilder::new(HealthConfig::default());
    w.pressure = PressureLevel::Warn;
    let s = w.tick(&mut b, 1_000);
    let a = alert(&s, "mem-low").expect("raised");
    assert_eq!((a.since, a.raised_at, a.dismissed), (1_000, 1_000, false));

    // Dismissed: stays listed, quiet, and the overall goes back to ok.
    assert!(b.dismiss("mem-low", 2_000));
    assert!(!b.dismiss("no-such-alert", 2_000));
    let s = w.tick(&mut b, 3_000);
    let a = alert(&s, "mem-low").expect("still held");
    assert!(a.dismissed);
    assert_eq!(a.raised_at, 1_000, "not re-raised");
    assert_eq!(s.overall, HealthLevel::Ok);

    // The condition ends and comes back inside the cooldown: still quiet.
    w.pressure = PressureLevel::Normal;
    assert!(alert(&w.tick(&mut b, 4_000), "mem-low").is_none(), "cleared");
    w.pressure = PressureLevel::Warn;
    let s = w.tick(&mut b, 5_000);
    assert!(alert(&s, "mem-low").expect("back").dismissed, "inside 10 min: quiet");

    // …unless it escalates.
    w.pressure = PressureLevel::Critical;
    let s = w.tick(&mut b, 6_000);
    let a = alert(&s, "mem-low").expect("escalated");
    assert!(!a.dismissed);
    assert_eq!(a.raised_at, 6_000);
    assert_eq!(s.overall, HealthLevel::Action);

    // Dismissed again; after the cooldown it is raised again if it still holds.
    assert!(b.dismiss("mem-low", 7_000));
    let s = w.tick(&mut b, 7_000 + DISMISS_COOLDOWN_MS - 1);
    assert!(alert(&s, "mem-low").expect("held").dismissed);
    let s = w.tick(&mut b, 7_000 + DISMISS_COOLDOWN_MS);
    let a = alert(&s, "mem-low").expect("re-raised");
    assert!(!a.dismissed);
    assert_eq!(a.raised_at, 7_000 + DISMISS_COOLDOWN_MS);

    // `restamp` applies a dismiss to a held snapshot at once.
    let mut held = s.clone();
    b.dismiss("mem-low", 8_000_000);
    b.restamp(&mut held);
    assert!(alert(&held, "mem-low").expect("held").dismissed);
    assert_eq!(held.overall, HealthLevel::Ok);
}

// -- a dismissal says until when (the UI's "back at") ----------------------------

#[test]
fn dismissed_until_is_the_dismissal_time_plus_the_cooldown_and_clears_on_re_raise() {
    let mut w = World::new(machine(), facts(vec![live_team()]));
    let mut b = HealthBuilder::new(HealthConfig::default());
    w.pressure = PressureLevel::Warn;
    let s = w.tick(&mut b, 1_000);
    let a = alert(&s, "mem-low").expect("raised");
    assert_eq!((a.dismissed, a.dismissed_until), (false, None), "an active alert has no end");

    // Dismissed at 2 000: until 2 000 + cooldown — on the restamped snapshot and every tick after.
    assert!(b.dismiss("mem-low", 2_000));
    let mut held = s.clone();
    b.restamp(&mut held);
    let a = alert(&held, "mem-low").expect("held");
    assert!(a.dismissed);
    assert_eq!(a.dismissed_until, Some(2_000 + DISMISS_COOLDOWN_MS), "restamp");
    let s = w.tick(&mut b, 3_000);
    assert_eq!(alert(&s, "mem-low").expect("held").dismissed_until, Some(2_000 + DISMISS_COOLDOWN_MS), "tick");
    assert_eq!(s.overall, HealthLevel::Ok, "the overall still ignores a dismissed alert");

    // The condition ends and comes back inside the cooldown: quiet until the SAME end.
    w.pressure = PressureLevel::Normal;
    assert!(alert(&w.tick(&mut b, 4_000), "mem-low").is_none(), "cleared");
    w.pressure = PressureLevel::Warn;
    let s = w.tick(&mut b, 5_000);
    let a = alert(&s, "mem-low").expect("back");
    assert!(a.dismissed);
    assert_eq!(a.dismissed_until, Some(2_000 + DISMISS_COOLDOWN_MS), "the dismissal's end, not the comeback's");

    // Escalation re-raises: no longer dismissed, no end.
    w.pressure = PressureLevel::Critical;
    let s = w.tick(&mut b, 6_000);
    let a = alert(&s, "mem-low").expect("escalated");
    assert_eq!((a.dismissed, a.dismissed_until, a.raised_at), (false, None, 6_000));

    // Dismissed again, then the cooldown runs out while it holds: re-raised, no end.
    assert!(b.dismiss("mem-low", 7_000));
    let s = w.tick(&mut b, 7_000 + DISMISS_COOLDOWN_MS - 1);
    assert_eq!(alert(&s, "mem-low").expect("held").dismissed_until, Some(7_000 + DISMISS_COOLDOWN_MS));
    let s = w.tick(&mut b, 7_000 + DISMISS_COOLDOWN_MS);
    let a = alert(&s, "mem-low").expect("re-raised");
    assert_eq!((a.dismissed, a.dismissed_until), (false, None));

    // A configured cooldown is the one used (the same that decides the re-raise).
    let mut w = World::new(machine(), facts(vec![live_team()]));
    let mut b = HealthBuilder::new(HealthConfig {
        dismiss_cooldown_ms: 60_000,
        ..HealthConfig::default()
    });
    w.pressure = PressureLevel::Warn;
    w.tick(&mut b, 1_000);
    assert!(b.dismiss("mem-low", 2_000));
    let s = w.tick(&mut b, 3_000);
    assert_eq!(alert(&s, "mem-low").expect("held").dismissed_until, Some(62_000));
}

#[test]
fn dismissed_until_crosses_ipc_as_camel_case_and_null_when_active() {
    let mut w = World::new(machine(), facts(vec![live_team()]));
    let mut b = HealthBuilder::new(HealthConfig::default());
    w.pressure = PressureLevel::Warn;
    let active = w.tick(&mut b, 1_000);
    let json = serde_json::to_value(&active).expect("serialize");
    let a = &json["alerts"][0];
    assert_eq!(a["dismissedUntil"], serde_json::Value::Null, "present, null while active: {a}");
    assert!(a.get("dismissedUntil").is_some(), "the key is sent");
    assert!(a.get("dismissed_until").is_none(), "never snake_case");

    assert!(b.dismiss("mem-low", 2_000));
    let s = w.tick(&mut b, 3_000);
    let json = serde_json::to_value(&s).expect("serialize");
    assert_eq!(json["alerts"][0]["dismissedUntil"], serde_json::json!(2_000 + DISMISS_COOLDOWN_MS));
    assert_eq!(json["alerts"][0]["dismissed"], serde_json::json!(true));
    let back: HealthSnapshot = serde_json::from_value(json.clone()).expect("round trip");
    assert_eq!(back, s, "round trip");

    // An older payload without the field reads as None.
    let mut old = json;
    old["alerts"][0].as_object_mut().expect("object").remove("dismissedUntil");
    let back: HealthSnapshot = serde_json::from_value(old).expect("old payload");
    assert_eq!(back.alerts[0].dismissed_until, None);
}

#[test]
fn overall_is_the_highest_severity_of_the_alerts_not_dismissed() {
    let w = World::new(machine(), facts(vec![live_team()]));
    let mut b = HealthBuilder::new(HealthConfig::default());
    let s = w.tick(&mut b, 1);
    assert!(s.alerts.is_empty());
    assert_eq!(s.overall, HealthLevel::Ok);
    assert!(HealthLevel::Action > HealthLevel::Attention && HealthLevel::Attention > HealthLevel::Ok);
}

// -- cost --------------------------------------------------------------------------

/// An 800-process machine with two agents every 50 rows (the §11.1 shape).
fn eight_hundred() -> Vec<PsRow> {
    let mut rows = machine();
    for i in 0..800 {
        let pid = 10_000 + i;
        let args = match i % 50 {
            0 => "/opt/bin/claude -p".to_string(),
            1 => "node /opt/lib/codex/bin/codex exec".to_string(),
            _ => format!("/usr/libexec/some-daemon --flag {i}"),
        };
        let parent = if i % 7 == 0 { 1 } else { 10_000 + i - 1 };
        rows.push(ps(pid, parent, pid, 10, 0.1, &args));
    }
    rows
}

/// The functional half (A10.7): on 800 processes the build reads a cwd only
/// for agent rows, and a second tick reads none (cached per pid + start).
#[test]
fn eight_hundred_processes_read_cwd_only_for_agents_and_cache_it() {
    let w = World::new(eight_hundred(), facts(vec![live_team()]));
    let mut b = HealthBuilder::new(HealthConfig::default());
    let first = w.tick(&mut b, 1);
    let agent_rows = first.agents.len();
    assert!(agent_rows > 0, "the fixture has agents");
    assert!(
        b.last_cwd_lookups <= agent_rows,
        "cwd only for agent rows: {} lookups for {agent_rows} agents",
        b.last_cwd_lookups
    );
    let _ = w.tick(&mut b, 2);
    assert_eq!(b.last_cwd_lookups, 0, "cached per pid + start time");
}

/// G5 benchmark (A10.7: no wall-clock budget in a functional test): a warm
/// health build over 800 processes adds < 20 ms to a tick. Run by the perf
/// harness: `cargo test --lib -- --ignored health_bench`.
#[test]
#[ignore = "G5 benchmark (A10.7): wall-clock budget, run by the perf harness"]
fn health_bench_eight_hundred_processes_cost_under_twenty_milliseconds() {
    let w = World::new(eight_hundred(), facts(vec![live_team()]));
    let mut b = HealthBuilder::new(HealthConfig::default());
    let started = Instant::now();
    let _ = w.tick(&mut b, 1);
    let cold = started.elapsed();
    let started = Instant::now();
    let _ = w.tick(&mut b, 2);
    let warm = started.elapsed();
    println!("health build over {} rows: cold {cold:?}, warm {warm:?}", w.rows.len());
    assert!(warm.as_millis() < 20, "a tick's health build must add < 20 ms (took {warm:?})");
}

// -- wire --------------------------------------------------------------------------

#[test]
fn the_snapshot_serializes_to_the_ipc_shape() {
    let w = World::new(host_died(), facts(vec![team("ended", false, vec![member("alpha", WT_ALPHA, None)])]));
    let mut b = HealthBuilder::new(HealthConfig::default());
    w.tick(&mut b, 1);
    let s = w.tick(&mut b, 2);
    let v = serde_json::to_value(&s).expect("json");
    for key in ["at", "overall", "agents", "hosts", "orphans", "teams", "memory", "alerts", "teamsKnown"] {
        assert!(v.get(key).is_some(), "missing {key}");
    }
    let a = v["agents"].as_array().expect("agents").iter().find(|a| a["pid"] == 200).expect("200");
    assert_eq!(a["attribution"]["kind"], serde_json::json!("teamMember"));
    assert_eq!(a["attribution"]["teamId"], serde_json::json!(TEAM));
    assert_eq!(a["row"]["pid"], serde_json::json!(200));
    assert!(a["startTime"].as_i64().is_some());
    let other = v["agents"].as_array().expect("agents").iter().find(|a| a["pid"] == 50).expect("50");
    assert_eq!(other["attribution"], serde_json::json!({ "kind": "other" }));
    assert_eq!(v["overall"], serde_json::json!("action"));
    let alert = &v["alerts"][0];
    for key in ["id", "kind", "severity", "titleKey", "bodyKey", "params", "actions", "since", "raisedAt", "teamId", "dismissed"] {
        assert!(alert.get(key).is_some(), "alert missing {key}");
    }
    assert_eq!(alert["titleKey"], serde_json::json!("health.alert.orphans.title"));
    assert_eq!(alert["actions"][0]["kind"], serde_json::json!("cleanUp"));
    assert_eq!(v["memory"]["pressure"], serde_json::json!("normal"));
    assert_eq!(s.orphan_identities(), vec![(200, 1_700_000_000_000 + 200), (300, 1_700_000_000_000 + 300)]
        .into_iter()
        .filter(|(p, _)| s.orphans.contains(p))
        .collect::<Vec<_>>());
    let empty = serde_json::to_value(HealthSnapshot::empty()).expect("json");
    assert_eq!(empty["at"], serde_json::json!(0));
    assert_eq!(empty["overall"], serde_json::json!("ok"));
}

#[test]
fn cco_commands_are_one_paste_each() {
    assert_eq!(stop_member_command("alpha"), "cco team_member alpha --stop");
    assert_eq!(interrupt_member_command("alpha"), "cco team_member alpha --interrupt");
    assert_eq!(resume_team_command("gilde-live"), "cco team_resume --team gilde-live");
    assert_eq!(resume_team_command("a b'c"), "cco team_resume --team 'a b'\\''c'");
    assert_eq!(agent_kind("/opt/homebrew/bin/cursor-agent --print"), Some("cursor-agent"));
    assert_eq!(agent_kind("node /x/bin/gemini"), Some("gemini"));
    assert_eq!(agent_kind("agy"), Some("agy"));
    assert_eq!(agent_kind("/Applications/Claude.app/Contents/MacOS/Claude"), None);
    assert_eq!(agent_kind("vim claude.md"), None);
    assert_eq!(agent_kind("/bin/sh -c claude -p"), None, "a shell wrapper is not the CLI");
    assert_eq!(agent_kind("/bin/zsh -lc claude"), None);
}

#[test]
fn agents_under_gilde_are_never_folded_into_the_agent_that_started_gilde() {
    // `npm run tauri dev` from an agent's terminal: claude → Gilde → claude.
    let mut rows = machine();
    rows.push(ps(30, 1, 30, 100, 0.0, "claude"));
    rows.push(ps(GILDE, 30, GILDE, 300, 0.0, "/Applications/Gilde Code.app/Contents/MacOS/gilde"));
    rows.push(ps(501, GILDE, 501, 80, 0.0, "/opt/bin/claude --resume"));
    let w = World::new(rows, facts(vec![live_team()]));
    let mut b = HealthBuilder::new(HealthConfig::default());
    let s = w.tick(&mut b, 1);
    assert_eq!(agent(&s, 501).pids, vec![501], "its own agent");
    assert_eq!(agent(&s, 30).pids, vec![30], "Gilde and what runs under it are not the outer agent's");
    assert!(s.agents.iter().all(|a| a.pid != GILDE));
}

#[test]
fn a_cwd_reported_with_symlinks_resolved_still_matches_the_worktree() {
    let dir = tempfile::tempdir().expect("tempdir");
    let real = dir.path().join("real");
    std::fs::create_dir_all(real.join("worktrees/h1/demo-x1/alpha")).expect("dirs");
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&real, &link).expect("symlink");
    let written = link.join("worktrees/h1/demo-x1/alpha").display().to_string();
    let resolved = std::fs::canonicalize(real.join("worktrees/h1/demo-x1/alpha")).expect("canon");

    // alpha is running (its turn's pid is not this one): its worktree counts.
    let mut f = facts(vec![team("active", true, vec![member("alpha", &written, Some(4444))])]);
    f.data_root = Some(link.display().to_string());
    let mut rows = machine();
    rows.push(ps(610, 1, 610, 10, 0.0, "/opt/bin/claude"));
    rows.push(ps(611, 1, 611, 10, 0.0, "/opt/bin/claude"));
    let mut w = World::new(rows, f);
    w.probe.cwd.insert(610, resolved.clone());
    // An unknown member directory under the same (resolved) data root.
    let other = std::fs::canonicalize(&real).expect("canon").join("worktrees/h2/gone-k1/omega");
    w.probe.cwd.insert(611, other);
    let mut b = HealthBuilder::new(HealthConfig::default());
    let s = w.tick(&mut b, 1);
    assert!(matches!(&agent(&s, 610).attribution,
        HealthAttribution::TeamMember { member, .. } if member == "alpha"));
    assert_eq!(agent(&s, 611).attribution, HealthAttribution::Other, "no team is named from a directory");
}

// ===========================================================================
// adversarial-review regressions
// ===========================================================================

/// The FE toast rule (`healthSlice.applyHealth`): one toast per
/// `(id, raisedAt)` while the alert is `action` and not dismissed.
fn toast_keys(s: &HealthSnapshot, into: &mut std::collections::BTreeSet<(String, i64)>) {
    for a in &s.alerts {
        if a.severity == HealthSeverity::Action && !a.dismissed {
            into.insert((a.id.clone(), a.raised_at));
        }
    }
}

/// An ended team whose host is gone: the ground every leftover stands on.
fn ended() -> TeamFacts {
    facts(vec![team("ended", false, vec![
        member("alpha", WT_ALPHA, None),
        member("beta", WT_BETA, None),
    ])])
}

// -- CRITICAL-1: a wrapper whose argv mentions a CLI is never the agent -----

#[test]
fn review_c1_only_argv0_names_an_agent_never_a_token_elsewhere() {
    for wrapper in [
        "screen -dmS lead claude",
        "SCREEN -dmS lead claude",
        "/usr/bin/screen -dmS lead /opt/bin/claude -p x",
        "tmux new-session -d claude",
        "caffeinate -i claude",
        "/usr/bin/caffeinate -i /opt/bin/claude",
        "script -q log claude",
        "nohup claude -p hello",
        "/usr/bin/time claude",
        "/usr/bin/env FOO=1 claude",
        "env claude",
        "ssh box claude",
        "vim claude",
        "python3 /tmp/wrap.py claude",
        "/usr/bin/python3 -c import os claude",
        "bash run.sh claude",
        "/bin/zsh -l",
        "/bin/sh /x/claude",
        "node /x/server.js claude",
        "node /opt/tools/runner.js /opt/bin/codex",
        "/usr/bin/perl -e system(@ARGV) /tmp/bin/claude hang",
    ] {
        assert_eq!(agent_kind(wrapper), None, "{wrapper:?} is a wrapper, not the agent");
    }
    for (args, kind) in [
        ("claude", "claude"),
        ("/opt/bin/claude -p --output-format stream-json", "claude"),
        ("node /usr/local/lib/node_modules/@anthropic-ai/claude-code/cli.js -p", "claude"),
        ("/opt/homebrew/bin/node --no-warnings /opt/homebrew/lib/node_modules/@anthropic-ai/claude-code/cli.js", "claude"),
        ("bun /Users/u/.bun/install/global/node_modules/@anthropic-ai/claude-code/cli.js", "claude"),
        ("node /opt/lib/codex/bin/codex exec", "codex"),
        ("node /x/bin/gemini", "gemini"),
        ("/opt/homebrew/bin/cursor-agent --print", "cursor-agent"),
        ("agy", "agy"),
        ("/Users/u/Library/Application Support/Some Host/claude-code/2.1.0/claude --resume", "claude"),
    ] {
        assert_eq!(agent_kind(args), Some(kind), "{args:?}");
    }
}

#[test]
fn review_c1_a_wrapper_in_a_member_worktree_is_never_folded_into_the_agent_nor_offered() {
    // The team ended; the user ran wrappers in alpha's worktree and closed
    // the terminal: the wrappers are re-parented to launchd.
    let mut rows = host_died();
    rows.push(ps(700, 1, 700, 5, 0.0, "SCREEN -dmS lead claude"));
    rows.push(ps(701, 700, 701, 200, 0.0, "claude"));
    rows.push(ps(710, 1, 710, 5, 0.0, "caffeinate -i claude"));
    rows.push(ps(711, 710, 710, 200, 0.0, "claude"));
    rows.push(ps(720, 1, 720, 5, 0.0, "python3 /tmp/wrap.py claude"));
    rows.push(ps(721, 720, 720, 1, 0.0, "/bin/sh -c claude"));
    rows.push(ps(722, 721, 720, 200, 0.0, "claude"));
    let mut w = World::new(rows, ended());
    for pid in [700, 701, 710, 711, 720, 721, 722] {
        w.probe.cwd.insert(pid, PathBuf::from(WT_ALPHA));
        // Even with the member's marker inherited from a take-over terminal.
        w.probe.env.insert(pid, marker_env("alpha", None));
    }
    let mut b = HealthBuilder::new(HealthConfig::default());
    w.tick(&mut b, 1);
    let s = w.tick(&mut b, 2);
    for wrapper in [700, 710, 720, 721] {
        assert!(s.agents.iter().all(|a| a.pid != wrapper), "{wrapper} is not an agent: {:?}", s.agents);
    }
    for (cli, parent) in [(701, 700), (711, 710), (722, 721)] {
        let a = agent(&s, cli);
        assert!(!a.pids.contains(&parent), "the agent's subtree never holds its wrapper");
        assert!(!a.orphan, "{cli} sits under a wrapper a person started: not a leftover");
    }
    for a in &s.alerts {
        for action in &a.actions {
            if let Some(pids) = &action.pids {
                for wrapper in [700, 710, 720, 721] {
                    assert!(!pids.contains(&wrapper), "{wrapper} offered for clean-up in {}", a.id);
                }
            }
        }
    }
}

// -- CRITICAL-2: no engine-origin evidence, no leftover ----------------------

#[test]
fn review_c2_a_users_own_cli_in_an_ended_member_worktree_without_the_marker_is_not_a_leftover() {
    let mut rows = host_died();
    rows.push(ps(730, 1, 730, 50, 0.0, "claude -p summarize"));
    let mut w = World::new(rows, ended());
    w.probe.cwd.insert(730, PathBuf::from(format!("{WT_ALPHA}/src")));
    // Readable environment, no marker.
    w.probe.env.insert(730, vec!["HOME=/Users/u".into()]);
    // And a second one whose environment cannot be read at all.
    w.rows.push(ps(731, 1, 731, 50, 0.0, "claude -p again"));
    w.probe.cwd.insert(731, PathBuf::from(WT_BETA));
    let mut b = HealthBuilder::new(HealthConfig::default());
    w.tick(&mut b, 1);
    let s = w.tick(&mut b, 2);
    assert!(!agent(&s, 730).orphan, "no engine marker: the user's own CLI");
    assert!(!agent(&s, 731).orphan, "unreadable environment: fail closed");
    assert!(!s.orphans.contains(&730) && !s.orphans.contains(&731), "{:?}", s.orphans);
    // The members WITH the marker are still leftovers.
    assert!(agent(&s, 200).orphan && agent(&s, 300).orphan, "{:?}", s.orphans);
    let a = alert(&s, &format!("orphans:{TEAM}")).expect("leftovers");
    assert_eq!(a.actions[0].pids, Some(vec![200, 300]));
}

#[test]
fn review_c2_the_same_cli_with_the_engine_marker_is_a_leftover_and_named_from_the_marker() {
    let mut rows = host_died();
    rows.push(ps(740, 1, 740, 50, 0.0, "claude -p summarize"));
    let mut w = World::new(rows, ended());
    w.probe.cwd.insert(740, PathBuf::from(WT_ALPHA));
    w.probe.env.insert(740, marker_env("alpha", Some("turn-old")));
    let mut b = HealthBuilder::new(HealthConfig::default());
    w.tick(&mut b, 1);
    let s = w.tick(&mut b, 2);
    assert!(agent(&s, 740).orphan);

    // A team out of sight: the marker alone is the evidence, and the alert
    // names the team from the marker, never from a directory name.
    let gone = "tmgonegonego1";
    let mut rows = host_died();
    rows.push(ps(750, 1, 750, 50, 0.0, "/opt/bin/gemini"));
    let mut w = World::new(rows, ended());
    w.probe.cwd.insert(750, PathBuf::from(format!("{DATA}/worktrees/h9/pretty-name-k2/gamma")));
    w.probe.env.insert(750, marker_env_for(gone, "gamma", None));
    let mut b = HealthBuilder::new(HealthConfig::default());
    w.tick(&mut b, 1);
    let s = w.tick(&mut b, 2);
    assert!(agent(&s, 750).orphan);
    let a = alert(&s, &format!("orphans:{gone}")).expect("named by the marker's team id");
    assert_eq!(a.params["team"], serde_json::json!(gone));
    assert_eq!(a.params["members"], serde_json::json!(["gamma"]));
    assert!(s.alerts.iter().all(|a| !a.id.contains("pretty-name")), "{:?}", s.alerts);

    // A directory shaped like a member worktree, no marker: not a team at all.
    let mut rows = host_died();
    rows.push(ps(760, 1, 760, 50, 0.0, "/opt/bin/gemini"));
    let mut w = World::new(rows, ended());
    w.probe.cwd.insert(760, PathBuf::from(format!("{DATA}/worktrees/h9/pretty-name-k2/gamma")));
    w.probe.env.insert(760, vec!["HOME=/Users/u".into()]);
    let mut b = HealthBuilder::new(HealthConfig::default());
    w.tick(&mut b, 1);
    let s = w.tick(&mut b, 2);
    assert_eq!(agent(&s, 760).attribution, HealthAttribution::Other);
    assert!(!agent(&s, 760).orphan);
}

// -- HIGH: a dispatch host started from a checkout ---------------------------

#[test]
fn review_p4_a_member_mid_turn_under_a_checkout_host_is_never_a_leftover() {
    const DEV_HOST: &str =
        "node /Users/u/src/orchestrator/out/mcp/server.js --host /Users/u/.claude-code-orchestrator/run/s50.spec.json";
    assert!(is_dispatch_host(DEV_HOST), "a checkout host is a host");
    let mut rows = machine();
    for r in rows.iter_mut().filter(|r| r.pid == 100) {
        r.args = DEV_HOST.into();
    }
    // The facts lag (a disk view that is not believed yet): only the process
    // tree and the marker can save the running member.
    let mut fact = team("active", false, vec![member("alpha", WT_ALPHA, Some(200)), member("beta", WT_BETA, None)]);
    fact.view.believed = false;
    let w = World::new(rows, facts(vec![fact]));
    let mut b = HealthBuilder::new(HealthConfig::default());
    w.tick(&mut b, 1);
    let s = w.tick(&mut b, 2);
    assert_eq!(s.hosts.iter().map(|h| h.pid).collect::<Vec<_>>(), vec![100]);
    assert!(s.orphans.is_empty(), "mid-turn under its (checkout) host: {:?}", s.orphans);
    assert!(s.alerts.iter().all(|a| a.kind != HealthAlertKind::Orphans), "{:?}", s.alerts);
}

// -- MEDIUM: a login / interactive shell is a person's ------------------------

#[test]
fn review_m_a_login_or_interactive_shell_is_never_a_wrapper_link() {
    for person in ["/bin/zsh -l", "-zsh", "/bin/bash --login", "/bin/zsh -i", "/bin/zsh -il", "/bin/zsh -lc claude"] {
        assert!(!is_launch_wrapper(person), "{person:?} is a person's shell");
    }
    for wrapper in ["/bin/sh -c claude", "sh", "/usr/bin/env -i claude", "/bin/bash -c x"] {
        assert!(is_launch_wrapper(wrapper), "{wrapper:?} only launches");
    }
    // A take-over terminal (marker set) in an IDE terminal whose app went away.
    let mut rows = host_died();
    rows.push(ps(770, 1, 770, 5, 0.0, "/bin/zsh -l"));
    rows.push(ps(771, 770, 771, 100, 0.0, "claude"));
    let mut w = World::new(rows, ended());
    w.probe.env.insert(771, marker_env("alpha", Some("attach-lease1")));
    let mut b = HealthBuilder::new(HealthConfig::default());
    w.tick(&mut b, 1);
    assert!(!agent(&w.tick(&mut b, 2), 771).orphan, "a person's login shell above it");
}

// -- MEDIUM: flapping at the action boundary ------------------------------------

#[test]
fn review_m_memory_flapping_at_the_action_line_toasts_at_most_once_per_ten_minutes_and_keeps_a_dismissal() {
    let set_tool = |rows: &mut Vec<PsRow>, mib: u64| {
        for r in rows.iter_mut().filter(|r| r.pid == 201) {
            r.rss_kb = mib * 1024;
        }
    };
    // alpha = 300 MiB CLI + the tool. Action line 8 GiB: 8.1 GiB vs 7.9 GiB.
    let over = 8 * 1024 + 100 - 300;
    let under = 8 * 1024 - 100 - 300;
    let mut rows = machine();
    set_tool(&mut rows, over);
    let mut w = World::new(rows.clone(), facts(vec![live_team()]));
    let mut b = HealthBuilder::new(HealthConfig::default());
    let id = format!("mem-agent:200-{}", 1_700_000_000_000i64 + 200);
    let mut toasts = std::collections::BTreeSet::new();
    let mut at = 0;
    for i in 0..100 {
        set_tool(&mut rows, if i % 2 == 0 { over } else { under });
        w.rows = rows.clone();
        at += 5_000;
        let s = w.tick(&mut b, at);
        toast_keys(&s, &mut toasts);
        assert_eq!(
            alert(&s, &id).expect("held").severity,
            HealthSeverity::Action,
            "hysteresis holds the action line (tick {i})"
        );
    }
    assert_eq!(toasts.len(), 1, "one toast in {at} ms: {toasts:?}");

    // Dismissed, then a real flap across the hysteresis band: stays dismissed.
    assert!(b.dismiss(&id, at));
    let far_under = 6 * 1024 - 300;
    let mut toasts = std::collections::BTreeSet::new();
    for i in 0..60 {
        set_tool(&mut rows, if i % 2 == 0 { far_under } else { over });
        w.rows = rows.clone();
        at += 5_000;
        let s = w.tick(&mut b, at);
        toast_keys(&s, &mut toasts);
        assert!(alert(&s, &id).expect("held").dismissed, "the dismissal holds (tick {i})");
    }
    assert!(toasts.is_empty(), "{toasts:?}");
}

#[test]
fn review_m_low_memory_flapping_between_warn_and_critical_toasts_once() {
    let mut w = World::new(machine(), facts(vec![live_team()]));
    let mut b = HealthBuilder::new(HealthConfig::default());
    let mut toasts = std::collections::BTreeSet::new();
    let mut at = 0;
    for i in 0..100 {
        w.pressure = if i % 2 == 0 { PressureLevel::Critical } else { PressureLevel::Warn };
        at += 5_000;
        toast_keys(&w.tick(&mut b, at), &mut toasts);
    }
    assert_eq!(toasts.len(), 1, "{toasts:?}");
    assert!(b.dismiss("mem-low", at));
    for i in 0..60 {
        w.pressure = if i % 2 == 0 { PressureLevel::Warn } else { PressureLevel::Critical };
        at += 5_000;
        let s = w.tick(&mut b, at);
        assert!(alert(&s, "mem-low").expect("held").dismissed, "tick {i}");
    }
}

// -- MEDIUM: a dismissed leftover alert re-raises for a NEW leftover -----------

#[test]
fn review_m_dismissing_leftovers_does_not_hide_a_new_leftover_of_the_same_team() {
    let mut w = World::new(host_died(), ended());
    let mut b = HealthBuilder::new(HealthConfig::default());
    w.tick(&mut b, 1_000);
    let s = w.tick(&mut b, 2_000);
    let id = format!("orphans:{TEAM}");
    assert!(alert(&s, &id).is_some());
    assert!(b.dismiss(&id, 3_000));
    let s = w.tick(&mut b, 4_000);
    assert!(alert(&s, &id).expect("held").dismissed);
    assert_eq!(alert(&s, &id).expect("held").dismissed_until, Some(3_000 + DISMISS_COOLDOWN_MS));

    // One of them goes: a subset stays quiet.
    let mut fewer = host_died();
    fewer.retain(|r| r.pid != 300);
    let rows_before = std::mem::replace(&mut w.rows, fewer);
    let s = w.tick(&mut b, 5_000);
    assert!(alert(&s, &id).expect("held").dismissed, "a subset of what was dismissed");

    // A NEW leftover of the same team appears: raised again.
    w.rows = rows_before;
    w.rows.push(ps(780, 1, 780, 50, 0.0, "claude -p"));
    w.probe.env.insert(780, marker_env("alpha", None));
    w.tick(&mut b, 6_000);
    let s = w.tick(&mut b, 7_000);
    let a = alert(&s, &id).expect("raised");
    assert!(!a.dismissed, "a new leftover re-raises: {a:?}");
    assert_eq!(a.dismissed_until, None, "and is no longer snoozed");
    assert_eq!(a.raised_at, 7_000);
    assert_eq!(a.actions[0].pids, Some(vec![200, 300, 780]));
}

// -- MEDIUM: a stuck turn is judged against the longest team limit ---------------

#[test]
fn review_m_a_quiet_turn_within_any_teams_limit_is_not_stuck() {
    let mut rows = machine();
    for r in rows.iter_mut().filter(|r| r.pid == 200 || r.pid == 201) {
        r.cpu_pct = 0.1;
    }
    let w = World::new(rows, facts(vec![live_team()]));
    let mut b = HealthBuilder::new(HealthConfig::default());
    let started = 1_000;
    let id = format!("turn-stuck:{TEAM}:alpha:turn-alpha");
    // 60 min in, quiet for 10 min: a team may allow up to 240 min.
    let mut at = started + 50 * 60_000;
    while at < started + 60 * 60_000 {
        at += 5_000;
        assert!(alert(&w.tick(&mut b, at), &id).is_none(), "at {} min", (at - started) / 60_000);
    }
    // 200 min in: still inside the longest limit.
    let mut b = HealthBuilder::new(HealthConfig::default());
    let mut at = started + 195 * 60_000;
    while at < started + 200 * 60_000 {
        at += 5_000;
        assert!(alert(&w.tick(&mut b, at), &id).is_none());
    }
}

#[test]
fn review_m_a_recent_last_activity_beats_a_quiet_cpu() {
    let mut rows = machine();
    for r in rows.iter_mut().filter(|r| r.pid == 200 || r.pid == 201) {
        r.cpu_pct = 0.1;
    }
    let mut fact = live_team();
    let mut w = World::new(rows, facts(vec![fact.clone()]));
    let mut b = HealthBuilder::new(HealthConfig::default());
    let id = format!("turn-stuck:{TEAM}:alpha:turn-alpha");
    let started = 1_000;
    let mut at = started + 300 * 60_000;
    for _ in 0..80 {
        at += 5_000;
        // The member keeps reporting tool activity every tick.
        fact.view.state.members[0].last_activity = Some(crate::sidecar::CcoLastActivity {
            ts: at - 1_000,
            text: "Edit".into(),
        });
        w.facts = facts(vec![fact.clone()]);
        assert!(alert(&w.tick(&mut b, at), &id).is_none(), "activity 1 s ago is activity");
    }
    // The last activity is ten minutes old: stuck (not vacuous).
    let last = at;
    let mut s = w.tick(&mut b, at);
    for _ in 0..130 {
        at += 5_000;
        s = w.tick(&mut b, at);
    }
    assert!(at - last >= 10 * 60_000);
    let a = alert(&s, &id).expect("no activity for ten minutes, beyond any team's limit");
    assert_eq!(a.severity, HealthSeverity::Attention);
}

// -- LOW ------------------------------------------------------------------------

#[test]
fn review_low_host_gone_ids_do_not_collide_for_two_teams_of_one_lead() {
    let mut a = team("active", false, vec![member("alpha", WT_ALPHA, None)]);
    let mut c = team("active", false, vec![member("gamma", "/elsewhere/gamma", None)]);
    a.view.believed = false;
    c.view.believed = false;
    c.view.team_id = "tmabcdefghi2".into();
    c.view.state.id = "tmabcdefghi2".into();
    c.view.state.name = "second".into();
    let rows: Vec<PsRow> = machine().into_iter().filter(|r| r.pid != 100).collect();
    let w = World::new(rows, facts(vec![a, c]));
    let mut b = HealthBuilder::new(HealthConfig::default());
    let s = w.tick(&mut b, 1);
    let gone: Vec<&HealthAlert> = s.alerts.iter().filter(|a| a.kind == HealthAlertKind::HostGone).collect();
    assert_eq!(gone.len(), 2, "{:?}", s.alerts);
}

#[test]
fn review_low_an_unreadable_cwd_is_retried_after_a_minute() {
    let mut rows = machine();
    rows.push(ps(790, 1, 790, 10, 0.0, "/opt/bin/claude"));
    let mut w = World::new(rows, facts(vec![live_team()]));
    let mut b = HealthBuilder::new(HealthConfig::default());
    w.tick(&mut b, 0);
    w.probe.cwd.insert(790, PathBuf::from("/Users/u/repo"));
    w.tick(&mut b, 5_000);
    assert_eq!(b.last_cwd_lookups, 0, "inside a minute: cached");
    w.tick(&mut b, 61_000);
    assert_eq!(b.last_cwd_lookups, 1, "an unreadable cwd is asked again after a minute");
}

#[test]
fn review_low_a_lead_session_pid_reused_by_another_process_is_not_alive() {
    let mut w = World::new(machine(), facts(vec![live_team()]));
    // pid 50 is alive but started an hour after the lead session did.
    w.probe.start.insert(50, 1_699_999_000_000 + 3_600_000);
    let mut b = HealthBuilder::new(HealthConfig::default());
    let s = w.tick(&mut b, 1);
    assert_eq!(s.hosts[0].lead_alive, Some(false), "{:?}", s.hosts);
}
