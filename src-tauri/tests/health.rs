//! The health surface against **real processes** — every one of them started by
//! this test (the `fake_agent` fixture, reached through a symlink named
//! `claude` so the agent signature names it).
//!
//!   * a fake team member whose cwd is a member worktree under a temporary
//!     data root, whose launcher exits (so it is re-parented to launchd) and
//!     whose team has ended, becomes a leftover after exactly two ticks — and
//!     the health clean-up path ends it and nothing else: not a live sibling
//!     agent, not a process in another group, not a pid that is not a leftover;
//!   * a fake agent holding memory over a test-lowered threshold raises the
//!     memory alert, with no kill action on it;
//!   * every tick publishes `gilde://health`.
//!
//!   * a wrapper that names a CLI in its argv (`caffeinate -i claude`) is
//!     never the agent and never offered; a CLI in an ended member's worktree
//!     WITHOUT the engine's `CCO_TEAM_MEMBER` marker is never a leftover; the
//!     same CLI WITH the marker is, and its clean-up plan is exactly its pid,
//!     named;
//!
//! Team facts come from a fixed [`TeamSource`] — no sidecar, no dispatch host,
//! no real data root. Each test runs under `rusty-fork` in its own process: a
//! bug in the code under test must never reach the `cargo test` runner.

mod fixtures;

use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gilde_code_lib::config::ConfigStore;
use gilde_code_lib::events::{Events, RecordingEvents};
use gilde_code_lib::sidecar::{CcoMemberState, CcoTeamState, CcoTeamView, CcoWorktree};
use gilde_code_lib::store::models::ProcKind;
use gilde_code_lib::store::Db;
use gilde_code_lib::supervise::health::{
    HealthAttribution, HealthConfig, HealthSeverity, HealthSnapshot, TeamFact, TeamFacts, TeamSource,
};
use gilde_code_lib::supervise::kill::is_gone;
use gilde_code_lib::supervise::procscan::{ps_snapshot, start_time_of};
use gilde_code_lib::supervise::registry::Supervisor;
use gilde_code_lib::supervise::spawn::is_alive;
use gilde_code_lib::supervise::watchdog::Watchdog;

use rusty_fork::rusty_fork_test;
use serde_json::json;

const SETTLE: Duration = Duration::from_secs(20);
/// A team id of THIS test process (each test runs in its own `rusty-fork`
/// process): a marked process another test starts at the same time is that
/// test's, never this one's.
fn team_id() -> String {
    format!("tmht{:07}", std::process::id() % 10_000_000)
}

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(f)
}

struct Harness {
    sup: Arc<Supervisor>,
    sink: Arc<RecordingEvents>,
    bus: Events,
    dir: tempfile::TempDir,
}

async fn harness() -> Harness {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::open_in_memory().expect("db");
    let config =
        Arc::new(ConfigStore::open_at(&dir.path().join("config.json")).expect("config store"));
    config
        .patch(json!({ "supervision": {
            "killGraceMs": 700,
            // Whatever else this machine runs must not decide anything here.
            "globalWarnPct": 100, "globalCriticalPct": 100,
            "softRssMiB": 1000000, "hardRssMiB": 1000000,
        }}))
        .expect("patch settings");
    let sink = RecordingEvents::new();
    let bus = Events::new(sink.clone());
    let sup = Arc::new(
        Supervisor::open_instance(&db, config, "0.0.0-test")
            .await
            .expect("open_instance")
            .with_events(bus.clone()),
    );
    Harness {
        sup,
        sink,
        bus,
        dir,
    }
}

impl Harness {
    fn health_events(&self) -> Vec<serde_json::Value> {
        self.sink
            .events()
            .into_iter()
            .filter(|(n, _)| n == "gilde://health")
            .map(|(_, v)| v)
            .collect()
    }

    /// `<tmp>/.claude-code-orchestrator`.
    fn data_root(&self) -> PathBuf {
        self.dir.path().join(".claude-code-orchestrator")
    }

    fn worktree(&self, member: &str) -> PathBuf {
        let p = self
            .data_root()
            .join("worktrees")
            .join("h1")
            .join("health-x1")
            .join(member);
        std::fs::create_dir_all(&p).expect("worktree dir");
        p
    }

    /// `<tmp>/bin/claude` → the fake agent: the agent signature's name.
    fn claude(&self) -> PathBuf {
        let bin = self.dir.path().join("bin");
        std::fs::create_dir_all(&bin).expect("bin dir");
        let link = bin.join("claude");
        if !link.exists() {
            std::os::unix::fs::symlink(fixtures::fake_agent_path(), &link).expect("symlink");
        }
        link
    }
}

/// A fixed set of team facts.
struct StaticTeams(TeamFacts);

impl TeamSource for StaticTeams {
    fn facts(&self) -> TeamFacts {
        self.0.clone()
    }
}

fn ended_team(h: &Harness) -> TeamFacts {
    let member = |name: &str| CcoMemberState {
        name: name.into(),
        state: "stopped".into(),
        worktree: CcoWorktree {
            path: h.worktree(name).display().to_string(),
            ..CcoWorktree::default()
        },
        ..CcoMemberState::default()
    };
    TeamFacts {
        known: true,
        data_root: Some(h.data_root().display().to_string()),
        teams: vec![TeamFact {
            view: CcoTeamView {
                team_id: team_id(),
                source: "disk".into(),
                live: false,
                believed: true,
                state: CcoTeamState {
                    id: team_id(),
                    name: "health".into(),
                    status: "ended".into(),
                    members: vec![member("alpha"), member("beta")],
                    ..CcoTeamState::default()
                },
                ..CcoTeamView::default()
            },
            running_at_last_live: Vec::new(),
        }],
        hosts: Vec::new(),
    }
}

/// A process this test started, ended (and reaped when it is our child) even
/// when the test panics. A re-parented one is signalled only while its kernel
/// start time is still the one recorded at launch — it is ours, never a
/// recycled pid.
struct Started {
    pid: i32,
    start_secs: Option<i64>,
    child: Option<Child>,
}

impl Drop for Started {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
            return;
        }
        if self.start_secs.is_some() && start_time_of(self.pid) == self.start_secs {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(self.pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
}

fn wait_until_visible(pid: i32) {
    let deadline = Instant::now() + SETTLE;
    loop {
        if ps_snapshot().map(|s| s.contains(pid)).unwrap_or(false) {
            return;
        }
        assert!(Instant::now() < deadline, "pid {pid} never appeared in a ps snapshot");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The engine's marker for a member of [`team_id`] (host-protocol `TEAM_MEMBER_ENV`).
fn marker(member: &str) -> (String, String) {
    ("CCO_TEAM_MEMBER".to_string(), format!("{}:{member}:1", team_id()))
}

/// Start `claude <args>` in `cwd` through a shell that exits at once, so the
/// agent is re-parented to launchd — what a member CLI looks like after its
/// dispatch host died. `envs` are added to its environment.
fn start_reparented(claude: &Path, args: &str, cwd: &Path, envs: &[(String, String)]) -> Started {
    let out = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("\"$0\" {args} </dev/null >/dev/null 2>&1 & echo $!"))
        .arg(claude)
        .envs(envs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .output()
        .expect("launcher");
    let pid: i32 = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("the launcher printed the agent's pid");
    let start_secs = start_time_of(pid);
    let started = Started {
        pid,
        start_secs,
        child: None,
    };
    wait_until_visible(pid);
    // The launcher has exited; wait for the kernel to hand the agent to launchd.
    let deadline = Instant::now() + SETTLE;
    loop {
        let ppid = ps_snapshot()
            .ok()
            .and_then(|s| s.get(pid).map(|r| r.ppid))
            .unwrap_or(0);
        if ppid == 1 {
            break;
        }
        assert!(Instant::now() < deadline, "pid {pid} was never re-parented (ppid {ppid})");
        std::thread::sleep(Duration::from_millis(20));
    }
    started
}

/// Start `program <args>` as our own child, in its own process group.
fn start_child(program: &Path, args: &[&str], cwd: &Path) -> Started {
    let child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .expect("spawn");
    let pid = child.id() as i32;
    wait_until_visible(pid);
    Started {
        pid,
        start_secs: start_time_of(pid),
        child: Some(child),
    }
}

/// The leftovers attributed to THIS test's team.
fn own_orphans(health: &HealthSnapshot) -> Vec<i32> {
    let mine = team_id();
    health
        .agents
        .iter()
        .filter(|a| a.orphan)
        .filter(|a| matches!(&a.attribution, HealthAttribution::TeamMember { team_id: Some(id), .. } if *id == mine))
        .map(|a| a.pid)
        .collect()
}

/// Any OTHER leftover on this machine (another test's marked process running
/// at the same time, a real leftover of a team out of these facts' sight)
/// stands on the engine's marker: a team member with a team id. Nothing
/// unmarked is ever a leftover.
fn assert_only_marked_orphans(health: &HealthSnapshot) {
    for a in health.agents.iter().filter(|a| a.orphan) {
        assert!(
            matches!(&a.attribution, HealthAttribution::TeamMember { team_id: Some(_), .. }),
            "pid {} is a leftover without the engine's marker: {:?}",
            a.pid,
            a.attribution
        );
    }
}

fn wait_until_gone(pid: i32) {
    let deadline = Instant::now() + SETTLE;
    while !is_gone(pid) {
        assert!(Instant::now() < deadline, "pid {pid} never exited");
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ===========================================================================
// 1. a leftover, two ticks, and the clean-up that ends it and only it
// ===========================================================================

rusty_fork_test! {
    #[test]
    fn a_reparented_member_of_an_ended_team_is_a_leftover_after_two_ticks_and_only_it_is_cleaned_up() {
        block_on(async {
            let h = harness().await;
            let claude = h.claude();
            let alpha_wt = h.worktree("alpha");
            let beta_wt = h.worktree("beta");

            // The leftover: alpha's CLI, launcher gone, team ended.
            let leftover = start_reparented(&claude, "hang", &alpha_wt, &[marker("alpha")]);
            // A sibling agent in beta's worktree that is still OURS (its parent
            // is this test, i.e. "Gilde"): never a leftover, never signalled.
            let sibling = start_child(&claude, &["hang"], &beta_wt);
            // A plain process in another group: never touched.
            let bystander = start_child(&fixtures::fake_agent_path(), &["hang"], h.dir.path());

            let watchdog = Watchdog::new(h.sup.clone(), h.bus.clone())
                .with_teams(Arc::new(StaticTeams(ended_team(&h))));

            watchdog.tick().await;
            let first = watchdog.health();
            let agent = first
                .agents
                .iter()
                .find(|a| a.pid == leftover.pid)
                .expect("the fake member is an agent");
            match &agent.attribution {
                HealthAttribution::TeamMember { team_id: id, member, .. } => {
                    assert_eq!(id.as_deref(), Some(team_id().as_str()));
                    assert_eq!(member, "alpha");
                }
                other => panic!("attributed by the engine's marker to the member: {other:?}"),
            }
            assert!(!agent.orphan, "one tick is not enough");
            assert!(first.orphans.is_empty(), "{:?}", first.orphans);

            watchdog.tick().await;
            let second = watchdog.health();
            assert_eq!(
                own_orphans(&second),
                vec![leftover.pid],
                "exactly the re-parented member of this team"
            );
            assert_only_marked_orphans(&second);
            let sib = second
                .agents
                .iter()
                .find(|a| a.pid == sibling.pid)
                .expect("the sibling is an agent");
            assert!(!sib.orphan, "an agent under a live Gilde is not left behind");
            let alert = second
                .alerts
                .iter()
                .find(|a| a.id == format!("orphans:{}", team_id()))
                .expect("the leftover alert");
            assert_eq!(alert.severity, HealthSeverity::Action);
            assert_eq!(alert.actions[0].pids.as_deref(), Some(&[leftover.pid][..]));

            // The clean-up path of Gilde 2 is `health_stop` (tested in
            // `gilde2_health_*` below); the old Process Monitor kill command
            // was dropped with `commands::monitor` (contract §2.2.1). The test
            // ends the leftover it started itself.
            // SAFETY: a process this test started.
            unsafe {
                libc::kill(leftover.pid, libc::SIGKILL);
            }
            wait_until_gone(leftover.pid);
            assert!(is_alive(sibling.pid), "the live sibling was never signalled");
            assert!(is_alive(bystander.pid), "a process in another group was never signalled");

            // The next tick clears the leftover alert.
            watchdog.tick().await;
            let after = watchdog.health();
            assert!(own_orphans(&after).is_empty());
            assert!(after.alerts.iter().all(|a| a.id != format!("orphans:{}", team_id())));

            // Every tick published `gilde://health`.
            let events = h.health_events();
            assert_eq!(events.len(), 3);
            assert!(events[1]["orphans"].as_array().is_some_and(|a| a.contains(&json!(leftover.pid))));
            assert_eq!(events[1]["overall"], json!("action"));
        });
    }
}

// ===========================================================================
// 2. an agent over a (test-lowered) memory threshold
// ===========================================================================

rusty_fork_test! {
    #[test]
    fn an_agent_over_the_memory_threshold_raises_the_memory_alert_without_a_kill_action() {
        block_on(async {
            let h = harness().await;
            let claude = h.claude();
            let hog = start_child(&claude, &["alloc", "64", "120"], h.dir.path());

            // Wait until the kernel reports the allocation as resident.
            let deadline = Instant::now() + SETTLE;
            loop {
                let rss_kb = ps_snapshot()
                    .ok()
                    .and_then(|s| s.get(hog.pid).map(|r| r.rss_kb))
                    .unwrap_or(0);
                if rss_kb >= 32 * 1024 {
                    break;
                }
                assert!(Instant::now() < deadline, "never resident (last {rss_kb} KiB)");
                std::thread::sleep(Duration::from_millis(50));
            }

            let cfg = HealthConfig {
                mem_attention_bytes: 16 * 1024 * 1024,
                mem_action_bytes: 1024 * 1024 * 1024 * 1024,
                ..HealthConfig::default()
            };
            let watchdog = Watchdog::new(h.sup.clone(), h.bus.clone())
                .with_teams(Arc::new(StaticTeams(TeamFacts::default())))
                .with_health_config(cfg);
            watchdog.tick().await;
            let health = watchdog.health();

            let agent = health
                .agents
                .iter()
                .find(|a| a.pid == hog.pid)
                .expect("the hog is an agent");
            assert_eq!(agent.agent, "claude");
            assert_eq!(agent.attribution, HealthAttribution::Other);
            let id = format!("mem-agent:{}-{}", agent.pid, agent.start_time);
            let alert = health
                .alerts
                .iter()
                .find(|a| a.id == id)
                .unwrap_or_else(|| panic!("no {id} in {:?}", health.alerts));
            assert_eq!(alert.severity, HealthSeverity::Attention);
            assert_eq!(alert.body_key, "health.alert.memAgent.bodyOther");
            assert!(
                alert.actions.iter().all(|a| a.pids.is_none() && a.command.is_none()),
                "no kill and no command for a process that is not a team member: {:?}",
                alert.actions
            );
            assert!(!agent.orphan, "`other` is never a leftover");
            assert!(health.orphans.iter().all(|p| *p != hog.pid));
            assert!(!health.teams_known, "no team facts were given");
            assert!(is_alive(hog.pid), "a memory alert never signals");
        });
    }
}

// ===========================================================================
// 3. adversarial review: wrappers, and no leftover without the engine marker
// ===========================================================================

/// The pid of `parent`'s only child, once it shows up in a scan.
fn child_of(parent: i32) -> i32 {
    let deadline = Instant::now() + SETTLE;
    loop {
        if let Some(pid) = ps_snapshot()
            .ok()
            .and_then(|s| s.rows().iter().find(|r| r.ppid == parent).map(|r| r.pid))
        {
            return pid;
        }
        assert!(Instant::now() < deadline, "{parent} never started its child");
        std::thread::sleep(Duration::from_millis(20));
    }
}

rusty_fork_test! {
    #[test]
    fn review_a_wrapper_is_never_the_agent_and_no_marker_means_no_leftover_while_the_marker_does() {
        block_on(async {
            let h = harness().await;
            let claude = h.claude();
            let alpha_wt = h.worktree("alpha");
            let beta_wt = h.worktree("beta");

            // A script wrapper whose argv ends in the CLI (the review's probe:
            // `perl -e 'system @ARGV' …/claude hang`, terminal closed): the
            // wrapper is re-parented to launchd and its child is the CLI. Even
            // with the member's marker in its environment.
            let perl = Path::new("/usr/bin/perl");
            assert!(perl.exists(), "a system binary on macOS");
            let wrapper = start_reparented(
                perl,
                &format!("-e 'system @ARGV' {} hang", claude.display()),
                &alpha_wt,
                &[marker("alpha")],
            );
            let wrapped_pid = child_of(wrapper.pid);
            let _wrapped = Started { pid: wrapped_pid, start_secs: start_time_of(wrapped_pid), child: None };
            // The user's own CLI in an ended member's worktree: no marker.
            let own = start_reparented(&claude, "hang", &beta_wt, &[]);
            // A member CLI the engine started: the marker.
            let member = start_reparented(&claude, "hang", &alpha_wt, &[marker("alpha")]);

            let watchdog = Watchdog::new(h.sup.clone(), h.bus.clone())
                .with_teams(Arc::new(StaticTeams(ended_team(&h))));
            watchdog.tick().await;
            watchdog.tick().await;
            let health = watchdog.health();

            assert!(
                health.agents.iter().all(|a| a.pid != wrapper.pid),
                "the wrapper is never the agent: {:?}",
                health.agents.iter().map(|a| (a.pid, &a.agent)).collect::<Vec<_>>()
            );
            let wrapped = health.agents.iter().find(|a| a.pid == wrapped_pid).expect("its child is the agent");
            assert!(!wrapped.pids.contains(&wrapper.pid));
            assert!(!wrapped.orphan, "a person's wrapper above it: not a leftover");
            let own_agent = health.agents.iter().find(|a| a.pid == own.pid).expect("an agent");
            assert!(!own_agent.orphan, "no engine marker: never a leftover");
            assert_eq!(own_agent.attribution, HealthAttribution::Other, "not named from its directory");
            assert_eq!(own_orphans(&health), vec![member.pid], "only the marked member CLI");
            assert_only_marked_orphans(&health);

            for alert in &health.alerts {
                for action in &alert.actions {
                    for pid in action.pids.iter().flatten() {
                        assert!(
                            *pid != wrapper.pid && *pid != wrapped_pid && *pid != own.pid,
                            "{pid} offered for clean-up in {}",
                            alert.id
                        );
                    }
                }
            }

            assert!(is_alive(wrapper.pid) && is_alive(wrapped_pid) && is_alive(own.pid) && is_alive(member.pid));
        });
    }
}

// ===========================================================================
// Gilde 2: process records, leftovers and the Clean-up flow (contract §3.4.10,
// §4.2.7) — T-health-leftover, T-health-stop
// ===========================================================================

mod gilde2 {
    use super::*;
    use gilde_code_lib::contract::common::Vendor;
    use gilde_code_lib::contract::ids::ProcessKey;
    use gilde_code_lib::contract::process::{LeftoverReason, ProcRole};
    use gilde_code_lib::contract::services::{ProcInfo, ProcSnapshot, TeammateLabels};
    use gilde_code_lib::supervise::health::{
        quantize_bytes, HealthState, LeftoverTracker, RegistryFact, RegistryView,
    };

    fn proc(pid: i32, ppid: i32, exec: &str, argv: &[&str]) -> ProcInfo {
        ProcInfo {
            key: ProcessKey::new(pid as u32, 1_000),
            pid,
            ppid,
            uid: 501,
            start_sec: 1_000,
            exec_path: Some(exec.to_string()),
            argv_redacted: argv.iter().map(|s| s.to_string()).collect(),
            labels: TeammateLabels::default(),
            cwd: None,
            rss_bytes: 123 * 1024 * 1024,
            cpu_ns: 0,
            cco_team_member: None,
            gilde_app_instance: None,
            vendor: gilde_code_lib::supervise::procscan::vendor_of_exec(exec, argv.get(1).copied())
                .and_then(Vendor::parse),
        }
    }

    fn teammate(pid: i32, ppid: i32) -> ProcInfo {
        let mut p = proc(pid, ppid, "/x/claude", &["/x/claude", "--agent-id", "a@t"]);
        p.labels = TeammateLabels {
            agent_id: Some("a@t".into()),
            agent_name: Some("a".into()),
            team_name: Some("t".into()),
            agent_color: None,
            parent_session_id: None,
        };
        p
    }

    fn snap(procs: Vec<ProcInfo>, at: i64) -> ProcSnapshot {
        ProcSnapshot { taken_at: at, procs }
    }

    #[test]
    fn t_health_leftover_reasons_hold_two_scans() {
        let mut t = LeftoverTracker::default();
        let reg = RegistryView::new();
        // A teammate whose lead is gone (re-parented to launchd).
        let s1 = snap(vec![teammate(500, 1)], 1);
        let r1 = t.observe(&s1, &reg);
        assert_eq!(r1.len(), 1);
        assert_eq!(r1[0].role, ProcRole::Teammate);
        assert!(r1[0].leftover.is_none(), "one scan is not enough");
        let r2 = t.observe(&snap(vec![teammate(500, 1)], 2), &reg);
        let l = r2[0].leftover.as_ref().expect("held two scans");
        assert_eq!(l.reason, LeftoverReason::TeammateOfGoneLead);
        assert_eq!(l.held_scans, 2);
        assert_eq!(l.since, 1);
        // The lead comes back into view: the condition stops, the hold resets.
        let lead = proc(400, 1, "/x/claude", &["/x/claude"]);
        let r3 = t.observe(&snap(vec![lead.clone(), teammate(500, 400)], 3), &reg);
        assert!(r3.iter().all(|r| r.leftover.is_none()));
        // A swarm server whose lead pid is gone.
        let tmux = proc(600, 1, "/opt/homebrew/bin/tmux", &["tmux", "-L", "claude-swarm-4242", "new-session"]);
        t.observe(&snap(vec![tmux.clone()], 4), &reg);
        let r = t.observe(&snap(vec![tmux], 5), &reg);
        assert_eq!(r[0].role, ProcRole::PaneServer);
        assert_eq!(r[0].leftover.as_ref().unwrap().reason, LeftoverReason::PaneServerOfDeadLead);
        // A user's own CLI is a record, never a leftover; RSS is quantized.
        let own = proc(700, 300, "/x/codex", &["/x/codex"]);
        let r = t.observe(&snap(vec![own.clone(), proc(300, 1, "/bin/zsh", &["-zsh"])], 6), &reg);
        let rec = r.iter().find(|x| x.pid == 700).unwrap();
        assert_eq!(rec.vendor, Some(Vendor::Codex));
        assert!(rec.leftover.is_none());
        assert_eq!(rec.rss_bytes, quantize_bytes(123 * 1024 * 1024));
        assert_eq!(rec.rss_bytes % (10 * 1024 * 1024), 0);
        // `claude daemon` is a helper, not a session.
        let daemon = proc(800, 1, "/x/claude", &["/x/claude", "daemon", "run"]);
        let r = t.observe(&snap(vec![daemon], 7), &reg);
        assert_eq!(r[0].role, ProcRole::VendorHelper);
    }

    #[test]
    fn own_registry_rows_win_and_crash_leftovers_are_named() {
        let mut t = LeftoverTracker::default();
        let mut reg = RegistryView::new();
        reg.insert(
            (900, 1_000),
            RegistryFact {
                spawn_id: "s".into(),
                kind: Some(ProcKind::Convo),
                current_instance: false,
                parent_uid: Some("s_01890a5d-ac96-774b-bcce-b302099a8057".into()),
                crash_leftover: true,
                quit_survivor: false,
                lead_gone_survivor: false,
                escapee: false,
            },
        );
        let mut p = proc(900, 1, "/x/claude", &["/x/claude"]);
        p.gilde_app_instance = Some(true);
        t.observe(&snap(vec![p.clone()], 1), &reg);
        let r = t.observe(&snap(vec![p], 2), &reg);
        assert!(r[0].gilde_owned, "own rows win over OtherGilde (N-05)");
        assert_eq!(r[0].session_uid.as_ref().map(|u| u.as_str()), Some("s_01890a5d-ac96-774b-bcce-b302099a8057"));
        assert_eq!(r[0].leftover.as_ref().unwrap().reason, LeftoverReason::GildeCrashLeftover);
    }

    /// A40.3: a hosted pane whose processes survived its lead's exit (PaneHost
    /// `lead_gone` recorded it) is Leftover `TeammateOfGoneLead`, although its
    /// row belongs to this running instance; without the record it is not.
    #[test]
    fn a_pane_that_survived_its_leads_exit_is_a_teammate_of_gone_lead_leftover() {
        let fact = |lead_gone_survivor: bool| RegistryFact {
            spawn_id: "pane".into(),
            kind: Some(ProcKind::Pane),
            current_instance: true,
            parent_uid: Some("s_01890a5d-ac96-774b-bcce-b302099a8057".into()),
            crash_leftover: false,
            quit_survivor: false,
            lead_gone_survivor,
            escapee: false,
        };
        for (recorded, want) in [(true, Some(LeftoverReason::TeammateOfGoneLead)), (false, None)] {
            let mut t = LeftoverTracker::default();
            let mut reg = RegistryView::new();
            reg.insert((910, 1_000), fact(recorded));
            let p = proc(910, 1, "/bin/sh", &["/bin/sh", "-c", "x"]);
            t.observe(&snap(vec![p.clone()], 1), &reg);
            let r = t.observe(&snap(vec![p], 2), &reg);
            assert_eq!(r[0].leftover.as_ref().map(|l| l.reason), want, "recorded={recorded}");
        }
        gilde_code_lib::supervise::reaper::note_lead_gone_survivor("pane-row");
        assert!(gilde_code_lib::supervise::reaper::lead_gone_survivor_spawn_ids().contains("pane-row"));
    }

    #[test]
    fn t_health_stop_preview_refuses_what_is_not_a_leftover() {
        let health = HealthState::new();
        let reg = RegistryView::new();
        // Our own pid stands in for a live process: it is never a leftover.
        let me = std::process::id() as i32;
        let start = start_time_of(me).expect("start");
        let mut p = proc(me, 1, "/x/claude", &["/x/claude"]);
        p.key = ProcessKey::new(me as u32, start as u64);
        p.start_sec = start;
        let s = Arc::new(snap(vec![p.clone()], 1));
        health.observe(s.clone(), &reg);
        health.observe(s, &reg);
        let res = health.stop_preview(&[p.key.clone(), ProcessKey::new(1, 1)]);
        assert!(res.targets.is_empty());
        assert_eq!(res.refused.len(), 2);
        assert!(res.refused.iter().all(|r| r.reason == "notLeftover"));
        // The detail is redacted and carries no environment.
        let d = health.detail(&p.key).expect("detail");
        assert_eq!(d.started_at, start * 1000);
        assert!(health.detail(&ProcessKey::new(1, 1)).is_err());
    }
}

rusty_fork_test! {
    #[test]
    fn t_health_stop_waits_for_the_undo_window_and_rechecks_identity() {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async {
                use gilde_code_lib::contract::ids::ProcessKey;
                use gilde_code_lib::contract::process::ProcRole;
                use gilde_code_lib::contract::services::{ProcInfo, ProcSnapshot, TeammateLabels};
                use gilde_code_lib::supervise::health::{HealthState, RegistryFact, RegistryView};

                let dir = tempfile::tempdir().expect("tempdir");
                let db = Db::open_in_memory().expect("db");
                let config = Arc::new(ConfigStore::open_at(&dir.path().join("config.json")).expect("config"));
                let sup = Arc::new(Supervisor::open_instance(&db, config, "0.0.0-test").await.expect("sup"));

                // A child this test started, presented as a crash leftover.
                let mut child = Command::new(fixtures::fake_agent_path())
                    .arg("hang")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .process_group(0)
                    .spawn()
                    .expect("spawn");
                std::thread::sleep(Duration::from_millis(400));
                let pid = child.id() as i32;
                let start = start_time_of(pid).expect("start");
                let key = ProcessKey::new(pid as u32, start as u64);
                let p = ProcInfo {
                    key: key.clone(),
                    pid,
                    ppid: 1,
                    uid: 501,
                    start_sec: start,
                    exec_path: Some(fixtures::fake_agent_path().to_string_lossy().into_owned()),
                    argv_redacted: vec!["fake_agent".into(), "hang".into()],
                    labels: TeammateLabels::default(),
                    cwd: None,
                    rss_bytes: 1,
                    cpu_ns: 0,
                    cco_team_member: None,
                    gilde_app_instance: None,
                    vendor: None,
                };
                let mut reg = RegistryView::new();
                reg.insert(
                    (pid, start),
                    RegistryFact {
                        spawn_id: "x".into(),
                        kind: Some(ProcKind::Pty),
                        current_instance: false,
                        parent_uid: None,
                        crash_leftover: true,
                        quit_survivor: false,
                        lead_gone_survivor: false,
                        escapee: false,
                    },
                );
                let health = HealthState::new();
                let s = Arc::new(ProcSnapshot { taken_at: 1, procs: vec![p] });
                health.observe(s.clone(), &reg);
                let left = health.observe(s, &reg);
                assert_eq!(left.len(), 1);
                assert_ne!(left[0].role, ProcRole::Teammate);

                // Preview → stop → undo inside the window: nothing is signalled.
                let preview = health.stop_preview(std::slice::from_ref(&key));
                assert_eq!(preview.targets.len(), 1);
                let res = health
                    .stop(&preview.confirm_token, sup.clone(), Duration::from_millis(600))
                    .expect("stop");
                let undo = res.undo_token.expect("undo token");
                assert!(health.undo(undo.as_str()), "the undo arrives in time");
                // A used confirm token is a conflict.
                assert!(health.stop(&preview.confirm_token, sup.clone(), Duration::ZERO).is_err());
                tokio::time::sleep(Duration::from_millis(1_000)).await;
                assert!(is_alive(pid), "an undone stop sends no signal");

                // Preview → stop, no undo: the window closes, the fenced TERM ends it.
                let preview = health.stop_preview(std::slice::from_ref(&key));
                let res = health
                    .stop(&preview.confirm_token, sup.clone(), Duration::from_millis(200))
                    .expect("stop");
                assert_eq!(res.stopped, 1);
                let deadline = Instant::now() + SETTLE;
                while !is_gone(pid) && Instant::now() < deadline {
                    let _ = child.try_wait();
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                assert!(is_gone(pid), "the leftover was stopped after the window");
                assert!(!health.undo(undo.as_str()), "an expired token cancels nothing");
                let _ = child.kill();
                let _ = child.wait();
            });
    }
}

// ===========================================================================
// A43.1 (G7c-07): own crash leftovers are known at once; A42.1: Clean-up of a
// non-Gilde leftover is a SIGTERM to that process only, never SIGKILL.
// ===========================================================================

mod a43 {
    use super::*;
    use gilde_code_lib::contract::common::Vendor;
    use gilde_code_lib::contract::ids::ProcessKey;
    use gilde_code_lib::contract::process::LeftoverReason;
    use gilde_code_lib::contract::services::{ProcInfo, ProcSnapshot, TeammateLabels};
    use gilde_code_lib::supervise::health::{HealthState, LeftoverTracker, RegistryFact, RegistryView};

    fn proc_info(pid: i32, start: i64, exec: &str, labels: TeammateLabels, vendor: Option<Vendor>) -> ProcInfo {
        ProcInfo {
            key: ProcessKey::new(pid as u32, start as u64),
            pid,
            ppid: 1,
            uid: 501,
            start_sec: start,
            exec_path: Some(exec.into()),
            argv_redacted: vec![exec.into()],
            labels,
            cwd: None,
            rss_bytes: 1 << 20,
            cpu_ns: 0,
            cco_team_member: None,
            gilde_app_instance: None,
            vendor,
        }
    }

    /// G7c-07a, the reviewer's test: the first scan after boot names a crash
    /// leftover the boot sweep proved.
    #[test]
    fn g7c_07a_the_first_scan_after_boot_names_a_crash_leftover() {
        let mut view = RegistryView::new();
        view.insert(
            (901, 9),
            RegistryFact { spawn_id: "s".into(), crash_leftover: true, ..RegistryFact::default() },
        );
        let mut t = LeftoverTracker::default();
        let first = t.observe(
            &ProcSnapshot { taken_at: 1, procs: vec![proc_info(901, 9, "/x/claude", TeammateLabels::default(), Some(Vendor::Claude))] },
            &view,
        );
        let l = first[0].leftover.as_ref().expect("a crash leftover from the first scan");
        assert_eq!((l.reason, l.held_scans), (LeftoverReason::GildeCrashLeftover, 1));
    }

    /// The 2-scan hold still applies to a leftover inferred from scans.
    #[test]
    fn a43_an_inferred_leftover_still_needs_two_scans() {
        let labels = TeammateLabels {
            agent_id: Some("a@t".into()),
            agent_name: Some("a".into()),
            team_name: Some("t".into()),
            ..TeammateLabels::default()
        };
        let snap = |n| ProcSnapshot { taken_at: n, procs: vec![proc_info(902, 9, "/x/claude", labels.clone(), Some(Vendor::Claude))] };
        let mut t = LeftoverTracker::default();
        let view = RegistryView::new();
        assert!(t.observe(&snap(1), &view)[0].leftover.is_none(), "inferred: held for a second scan");
        let second = t.observe(&snap(2), &view);
        assert_eq!(second[0].leftover.as_ref().map(|l| l.reason), Some(LeftoverReason::TeammateOfGoneLead));
    }

    rusty_fork_test! {
        /// A42.1: Clean-up of a non-Gilde leftover (no registry row: a
        /// teammate of a gone lead) after the undo window is a SIGTERM to that
        /// process only — one that ignores it survives, never SIGKILLed.
        #[test]
        fn a42_cleanup_of_a_non_gilde_leftover_never_sigkills() {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("runtime")
                .block_on(async {
                    let dir = tempfile::tempdir().expect("tempdir");
                    let db = Db::open_in_memory().expect("db");
                    let config = Arc::new(ConfigStore::open_at(&dir.path().join("config.json")).expect("config"));
                    let sup = Arc::new(Supervisor::open_instance(&db, config, "0.0.0-test").await.expect("sup"));
                    let mut child = Command::new(fixtures::fake_agent_path())
                        .arg("ignore-term")
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .process_group(0)
                        .spawn()
                        .expect("spawn");
                    std::thread::sleep(Duration::from_millis(400));
                    let pid = child.id() as i32;
                    let start = start_time_of(pid).expect("start");
                    let labels = TeammateLabels {
                        agent_id: Some("a@t".into()),
                        agent_name: Some("a".into()),
                        team_name: Some("t".into()),
                        ..TeammateLabels::default()
                    };
                    let p = proc_info(pid, start, &fixtures::fake_agent_path().to_string_lossy(), labels, Some(Vendor::Claude));
                    let key = p.key.clone();
                    let health = HealthState::new();
                    let reg = RegistryView::new();
                    let s = Arc::new(ProcSnapshot { taken_at: 1, procs: vec![p] });
                    health.observe(s.clone(), &reg);
                    let left = health.observe(s, &reg);
                    assert!(left[0].leftover.is_some() && !left[0].gilde_owned, "a non-Gilde leftover: {left:?}");
                    let preview = health.stop_preview(std::slice::from_ref(&key));
                    assert_eq!(preview.targets.len(), 1, "{preview:?}");
                    let res = health.stop(&preview.confirm_token, sup.clone(), Duration::from_millis(100)).expect("stop");
                    assert_eq!(res.stopped, 1);
                    // The window, the TERM (ignored) and its 2 s grace, then the
                    // kill-verify budget: still alive, nothing escalated.
                    tokio::time::sleep(Duration::from_millis(4_800)).await;
                    let alive = is_alive(pid) && !is_gone(pid);
                    let _ = child.kill();
                    let _ = child.wait();
                    assert!(alive, "Clean-up SIGKILLed a non-Gilde leftover");
                });
        }
    }
}
