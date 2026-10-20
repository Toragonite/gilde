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

use gilde_code_lib::commands::monitor;
use gilde_code_lib::config::ConfigStore;
use gilde_code_lib::events::bus::{EventBus, RecordingSink};
use gilde_code_lib::model::graph::GraphHandle;
use gilde_code_lib::sidecar::{CcoMemberState, CcoTeamState, CcoTeamView, CcoWorktree};
use gilde_code_lib::store::Db;
use gilde_code_lib::supervise::health::{
    HealthAttribution, HealthConfig, HealthSeverity, HealthSnapshot, TeamFact, TeamFacts, TeamSource,
};
use gilde_code_lib::supervise::kill::{is_gone, KillOpts, KillTarget};
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
    sink: Arc<RecordingSink>,
    bus: EventBus,
    _graph: GraphHandle,
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
    let graph = GraphHandle::new();
    let sink = Arc::new(RecordingSink::default());
    let bus = EventBus::with_sink(sink.clone(), graph.state().clone());
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
        _graph: graph,
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

            // A target that is not a leftover is refused, unsignalled.
            let refused = monitor::kill_health_orphans(
                &h.sup,
                &[KillTarget::pid(sibling.pid), KillTarget::pid(bystander.pid)],
                KillOpts::default().grace(Duration::from_millis(700)),
                &second.orphan_identities(),
            )
            .await;
            assert_eq!(refused.len(), 2);
            for outcome in &refused {
                assert_eq!(outcome.report.refused_reason.as_deref(), Some("notLeftover"));
                assert!(outcome.report.requested.is_empty());
            }
            // A recorded start time that does not match the live pid is refused too.
            let stale = monitor::kill_health_orphans(
                &h.sup,
                &[KillTarget::pid(leftover.pid)],
                KillOpts::default().dry_run(true),
                &[(leftover.pid, 1_000)],
            )
            .await;
            assert_eq!(stale[0].report.refused_reason.as_deref(), Some("notLeftover"));

            // The dry run names the leftover and sends nothing.
            let dry = monitor::kill_health_orphans(
                &h.sup,
                &[KillTarget::pid(leftover.pid)],
                KillOpts::default().dry_run(true),
                &second.orphan_identities(),
            )
            .await;
            assert!(!dry[0].report.is_refused(), "{:?}", dry[0].report);
            assert_eq!(dry[0].report.requested, vec![leftover.pid]);
            assert_eq!(dry[0].report.names.len(), 1, "every pid it would signal is named");
            assert_eq!(dry[0].report.names[0].pid, leftover.pid);
            assert_eq!(dry[0].report.names[0].name, "claude");
            assert!(is_alive(leftover.pid), "a dry run sends no signal");

            // The clean-up itself.
            let done = monitor::kill_health_orphans(
                &h.sup,
                &[KillTarget::pid(leftover.pid)],
                KillOpts::default().grace(Duration::from_millis(700)),
                &second.orphan_identities(),
            )
            .await;
            assert!(!done[0].report.is_refused(), "{:?}", done[0].report);
            assert_eq!(done[0].report.requested, vec![leftover.pid], "only its own process tree");
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

            // The clean-up plan of the leftover: itself (it has no children), named.
            let dry = monitor::kill_health_orphans(
                &h.sup,
                &[KillTarget::pid(member.pid)],
                KillOpts::default().dry_run(true),
                &health.orphan_identities(),
            )
            .await;
            assert_eq!(dry[0].report.requested, vec![member.pid]);
            assert_eq!(dry[0].report.names[0].name, "claude");
            // The wrapper and the user's own CLI are refused outright.
            let refused = monitor::kill_health_orphans(
                &h.sup,
                &[KillTarget::pid(wrapper.pid), KillTarget::pid(own.pid), KillTarget::pid(wrapped_pid)],
                KillOpts::default().dry_run(true),
                &health.orphan_identities(),
            )
            .await;
            for outcome in &refused {
                assert_eq!(outcome.report.refused_reason.as_deref(), Some("notLeftover"));
            }
            assert!(is_alive(wrapper.pid) && is_alive(wrapped_pid) && is_alive(own.pid) && is_alive(member.pid));
        });
    }
}
