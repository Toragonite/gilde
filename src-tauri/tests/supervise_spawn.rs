//! Process-group discipline and the write-before-spawn hook (DESIGN §8-1, §8-3;
//! M1-PLAN T08 acceptance 8–9).
//!
//! Two properties are checked against the real kernel and the real `git`:
//!
//!   1. a child started through [`tokio_command_grouped`] is a **process group
//!      leader**, so `killpg` on its pgid can never reach anything we did not
//!      create;
//!   2. `worktree::git::run_git` writes a durable registry row **before** it
//!      spawns — `git` is not exempt from the rule; it is planned through the
//!      supervisor's `SupervisorApi` (the legacy hook is gone, A22.3).

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use gilde_code_lib::config::ConfigStore;
use gilde_code_lib::store::models::{ProcKind, ProcState};
use gilde_code_lib::store::{dao, Db};
use gilde_code_lib::contract::services::SupervisorApi;
use gilde_code_lib::supervise::registry::Supervisor;
use gilde_code_lib::supervise::spawn::{pgid_of, tokio_command_grouped};
use rusty_fork::rusty_fork_test;

// ---------------------------------------------------------------------------
// acceptance 8 — every spawn leads its own process group
// ---------------------------------------------------------------------------

/// `ps -o pgid= -p <pid>`, as an independent second opinion on `getpgid`.
fn pgid_via_ps(pid: i32) -> i32 {
    let out = std::process::Command::new("/bin/ps")
        .args(["-o", "pgid=", "-p", &pid.to_string()])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<i32>()
        .unwrap_or_else(|e| panic!("cannot parse a pgid out of ps output: {e}"))
}

#[tokio::test]
async fn a_grouped_spawn_makes_the_child_its_own_group_leader() {
    let mut child = tokio_command_grouped(Path::new("/bin/sleep"))
        .arg("30")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn /bin/sleep");

    let pid = child.id().expect("pid") as i32;
    assert_eq!(
        pgid_via_ps(pid),
        pid,
        "the child must be the leader of a brand new process group"
    );
    assert_eq!(pgid_of(pid), Some(pid), "getpgid agrees with ps");
    assert_ne!(
        pid,
        pgid_of(std::process::id() as i32).unwrap_or(-1),
        "and that group is not the test runner's"
    );

    child.kill().await.expect("kill");
    let _ = child.wait().await;
}

// ---------------------------------------------------------------------------
// acceptance 9 — `git` goes through the registry
// ---------------------------------------------------------------------------

#[tokio::test]
async fn git_invocations_are_registered_before_they_spawn() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::open_in_memory().expect("db");
    let config =
        Arc::new(ConfigStore::open_at(&dir.path().join("config.json")).expect("config store"));
    let supervisor = Arc::new(
        Supervisor::open_instance(&db, config, "0.0.0-test")
            .await
            .expect("open_instance"),
    );
    let instance = supervisor.app_instance_id().to_string();

    assert!(
        gilde_code_lib::worktree::git::bind_supervisor(Arc::clone(&supervisor) as Arc<dyn SupervisorApi>),
        "the supervisor binds once per process"
    );
    assert!(
        !gilde_code_lib::worktree::git::bind_supervisor(Arc::clone(&supervisor) as Arc<dyn SupervisorApi>),
        "and a second binding is refused rather than swapping a live one"
    );

    let out = gilde_code_lib::worktree::git::run_git(dir.path(), &["--version"])
        .await
        .expect("git --version");
    assert!(out.text().starts_with("git version"), "{}", out.text());

    // The row exists, it is terminal, and it records what actually ran.
    let rows = db
        .read(move |conn| dao::proc::rows_of_instance(conn, &instance))
        .await
        .expect("read rows");
    let git_rows: Vec<_> = rows.iter().filter(|r| r.kind == ProcKind::Git).collect();
    assert_eq!(git_rows.len(), 1, "exactly one git spawn was registered");

    let row = git_rows[0];
    assert_eq!(row.state, ProcState::Exited);
    assert_eq!(row.exit_code, Some(0));
    assert!(row.pid.is_some(), "the pid was recorded");
    assert_eq!(row.pgid, row.pid, "git leads its own group");
    assert!(
        row.proc_start_ts.is_some(),
        "fence 1 needs the kernel start time"
    );
    assert_eq!(
        row.env_marker, row.spawn_id,
        "the marker stamped on the child is the row id"
    );
    assert_eq!(row.cwd, dir.path().to_string_lossy());
    assert!(row.planned_at > 0);
    assert!(
        row.started_at.expect("started_at") >= row.planned_at,
        "the row is written before the process starts"
    );

    let argv = row.argv_vec();
    assert!(argv[0].ends_with("git"), "argv[0] is an absolute git path: {argv:?}");
    assert!(argv.contains(&"--version".to_string()), "{argv:?}");
    assert!(argv.contains(&dir.path().to_string_lossy().into_owned()), "{argv:?}");
}

// ---------------------------------------------------------------------------
// Gilde 2: binary resolution at spawn [A1], the launch allowlist (§9.3) and
// the spawn gate (§2.4 B6)
// ---------------------------------------------------------------------------

/// A stand-in "claude" whose `--version` prints `version` and counts its runs
/// (a fixture script, never a real CLI).
fn write_fake_cli(path: &Path, version: &str, counter: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    let script = format!(
        "#!/bin/sh\necho run >> '{}'\necho '{} (fixture)'\n",
        counter.display(),
        version
    );
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, script).expect("write fixture");
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    // Atomic replace: a new inode, like an installer.
    std::fs::rename(&tmp, path).expect("replace");
}

fn runs(counter: &Path) -> usize {
    std::fs::read_to_string(counter).map(|s| s.lines().count()).unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolve_binary_caches_by_fingerprint_and_fsevents_invalidate_new_spawns_only() {
    use gilde_code_lib::agents::{AgentKind, BinaryResolver};
    let dir = tempfile::tempdir().expect("tempdir");
    let bin_dir = std::fs::canonicalize(dir.path()).expect("canonical");
    let cli = bin_dir.join("claude");
    let counter = bin_dir.join("runs.log");
    write_fake_cli(&cli, "2.1.306", &counter);

    let resolver = BinaryResolver::new();
    let changed = Arc::new(parking_lot::Mutex::new(Vec::new()));
    {
        let changed = changed.clone();
        resolver.on_change(Arc::new(move |b: &gilde_code_lib::contract::session::BinaryRef| {
            changed.lock().push(b.realpath.as_str().to_string())
        }));
    }
    let first = resolver.resolve(&cli, AgentKind::Claude, None).await.expect("resolve");
    assert_eq!(first.binary.version.as_deref(), Some("2.1.306"));
    assert_eq!(first.binary.realpath.as_str(), cli.to_string_lossy());
    assert_eq!(runs(&counter), 1);
    assert!(
        gilde_code_lib::supervise::spawn::is_allowed(&cli, gilde_code_lib::contract::process::ProcKind::Convo),
        "a resolved vendor binary is allowlisted for vendor spawns (§9.3)"
    );

    // Same fingerprint: answered from the cache, no second `--version`.
    let again = resolver.resolve(&cli, AgentKind::Claude, None).await.expect("resolve");
    assert_eq!(again.binary, first.binary);
    assert_eq!(runs(&counter), 1);
    assert_eq!(resolver.version_runs(), 1);

    // The installer replaces the binary in place (a new inode).
    write_fake_cli(&cli, "2.1.307", &counter);
    // FSEvents drops the entry (latency ~1 s); the stat check would catch it
    // on the next spawn anyway.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while changed.lock().is_empty() && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(!changed.lock().is_empty(), "the change was noticed for a background canary");
    assert!(resolver.cached(&cli).is_none(), "the cached BinaryRef was invalidated");

    let next = resolver.resolve(&cli, AgentKind::Claude, None).await.expect("resolve");
    assert_eq!(next.binary.version.as_deref(), Some("2.1.307"), "the next spawn re-runs --version");
    assert_ne!(next.binary.fingerprint, first.binary.fingerprint);
    // What a running session recorded at its spawn is untouched.
    assert_eq!(first.binary.version.as_deref(), Some("2.1.306"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fingerprint_change_is_caught_without_an_event() {
    use gilde_code_lib::agents::{AgentKind, BinaryResolver};
    let dir = tempfile::tempdir().expect("tempdir");
    let bin_dir = std::fs::canonicalize(dir.path()).expect("canonical");
    let cli = bin_dir.join("claude");
    let counter = bin_dir.join("runs.log");
    write_fake_cli(&cli, "2.1.306", &counter);
    let resolver = BinaryResolver::new();
    resolver.resolve(&cli, AgentKind::Claude, None).await.expect("resolve");
    write_fake_cli(&cli, "2.1.308", &counter);
    // Straight away, before any FSEvents delivery.
    let next = resolver.resolve(&cli, AgentKind::Claude, None).await.expect("resolve");
    assert_eq!(next.binary.version.as_deref(), Some("2.1.308"));
    let missing = resolver.resolve(&bin_dir.join("nope"), AgentKind::Claude, None).await;
    assert!(missing.is_err(), "a missing binary is SPAWN_FAILED");
}

#[tokio::test]
async fn the_trait_spawn_path_enforces_the_allowlist_and_the_gate() {
    use gilde_code_lib::contract::common::AbsPath;
    use gilde_code_lib::contract::process::ProcKind as K;
    use gilde_code_lib::contract::services::{SpawnRequest, SupervisorApi};
    use gilde_code_lib::contract::ErrorCode;
    use gilde_code_lib::lifecycle::spawn_gate::SpawnGate;

    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::open_in_memory().expect("db");
    let config = Arc::new(ConfigStore::open_at(&dir.path().join("config.json")).expect("config"));
    let gate = SpawnGate::closed();
    let sup = Supervisor::open_instance(&db, config, "0.0.0-test")
        .await
        .expect("instance")
        .with_spawn_gate(gate.clone());
    let req = |argv0: &str, kind: K| SpawnRequest {
        kind,
        argv0: AbsPath::parse(argv0).unwrap(),
        args: vec!["--version".into()],
        cwd: None,
        env_keys: Vec::new(),
        parent_uid: None,
        generation: None,
        turn_id: None,
    };
    // Closed gate: NOT_READY{bootSweep} (after the 5 s wait, shortened here).
    assert!(gate.wait_for(std::time::Duration::from_millis(10)).await.is_err());
    gate.open();
    // Not allowlisted: REFUSED{notAllowlisted}; nothing is written.
    let err = SupervisorApi::plan_spawn(&sup, req("/usr/bin/true", K::Convo)).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Refused);
    assert_eq!(err.params["reason"], "notAllowlisted");
    // `/bin/sh` is allowed for panes only.
    let err = SupervisorApi::plan_spawn(&sup, req("/bin/sh", K::Convo)).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Refused);
    let ticket = SupervisorApi::plan_spawn(&sup, req("/bin/sh", K::Pane)).await.expect("pane");
    let id = ticket.spawn_id.clone();
    let row = db.read(move |c| dao::proc::get(c, &id)).await.expect("read").expect("row");
    assert_eq!(row.kind, ProcKind::Pane);
    assert_eq!(row.state, ProcState::Planned);
    SupervisorApi::mark_exited(&sup, &ticket, gilde_code_lib::contract::activity::ExitInfo { code: Some(0), signal: None }).await;
    assert!(SupervisorApi::is_vendor_cli(&sup, "/Users/x/.npm-global/lib/node_modules/@anthropic-ai/claude-code/bin/claude.exe"));
    assert!(!SupervisorApi::is_vendor_cli(&sup, "/bin/sh"));
    assert_eq!(SupervisorApi::memory_only(&sup), None);
}

/// A12.8/A22.3: the one spawn path (`SupervisorApi::plan_spawn`) refuses an
/// argv[0] outside the §9.3 allowlist with `REFUSED{notAllowlisted}` and no
/// row, per kind, and admits it once it is allowlisted for that kind. (The
/// legacy `Supervisor::plan_spawn(SpawnPlan)` it used to be compared with is
/// deleted.)
#[tokio::test]
async fn the_one_spawn_path_enforces_the_allowlist_per_kind() {
    use gilde_code_lib::contract::common::AbsPath;
    use gilde_code_lib::contract::process::ProcKind as K;
    use gilde_code_lib::contract::services::SpawnRequest;
    use gilde_code_lib::contract::ErrorCode;

    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::open_in_memory().expect("db");
    let config = Arc::new(ConfigStore::open_at(&dir.path().join("config.json")).expect("config"));
    let sup = Supervisor::open_instance(&db, config, "0.0.0-test").await.expect("instance");

    // A binary nobody allowlisted (detection and settings never saw it).
    let bin = dir.path().join("claude");
    std::fs::write(&bin, b"#!/bin/sh\nexit 0\n").expect("write");
    let req = |argv0: &str, kind: K| SpawnRequest {
        kind,
        argv0: AbsPath::parse(argv0.to_string()).unwrap(),
        args: vec!["--version".into()],
        cwd: None,
        env_keys: Vec::new(),
        parent_uid: None,
        generation: (kind == K::Turn).then_some(1),
        turn_id: (kind == K::Turn).then(|| "t-1".to_string()),
    };
    let bin_s = bin.to_string_lossy().into_owned();

    for (argv0, kind) in [(bin_s.as_str(), K::Convo), ("/bin/sh", K::Pty), ("/bin/sh", K::Turn)] {
        let err = SupervisorApi::plan_spawn(&sup, req(argv0, kind)).await.expect_err("refused");
        assert_eq!(err.code, ErrorCode::Refused, "{argv0} as {kind:?}");
        assert_eq!(err.params["reason"], "notAllowlisted", "{argv0} as {kind:?}");
    }

    // Nothing was written for any refused plan.
    let instance = sup.app_instance_id().to_string();
    let rows = db
        .read(move |c| dao::proc::rows_of_instance(c, &instance))
        .await
        .expect("read");
    assert!(rows.is_empty(), "a refused plan writes no row: {rows:?}");

    // Allowlisted as a vendor binary (what detection does), the same plan
    // goes through — as a session and as a turn run, never as a pane.
    gilde_code_lib::supervise::spawn::allow_executable(&bin, &gilde_code_lib::agents::VENDOR_KINDS);
    for kind in [K::Convo, K::Turn] {
        let t = SupervisorApi::plan_spawn(&sup, req(&bin_s, kind)).await.expect("allowlisted");
        SupervisorApi::mark_exited(&sup, &t, gilde_code_lib::contract::activity::ExitInfo { code: Some(0), signal: None }).await;
    }
    let err = SupervisorApi::plan_spawn(&sup, req(&bin_s, K::Pane)).await.expect_err("not a pane");
    assert_eq!(err.params["reason"], "notAllowlisted");
}

/// A13.1: `SupervisorApi::plan_spawn` checks the sidecar generation and the
/// turn id before anything is planned, and stores both in the row so the
/// dead-generation sweep finds a `Turn` spawn.
#[tokio::test]
async fn spawn_requests_carry_a_valid_generation_and_turn_id() {
    use gilde_code_lib::contract::common::AbsPath;
    use gilde_code_lib::contract::process::ProcKind as K;
    use gilde_code_lib::contract::services::{SpawnRequest, SupervisorApi};
    use gilde_code_lib::contract::ErrorCode;

    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::open_in_memory().expect("db");
    let config = Arc::new(ConfigStore::open_at(&dir.path().join("config.json")).expect("config"));
    let sup = Supervisor::open_instance(&db, config, "0.0.0-test").await.expect("instance");
    // A vendor binary allowlisted the way detection does it (Turn included,
    // A13.2), and a node allowlisted for the sidecar (A13.3).
    let claude = dir.path().join("claude");
    std::fs::write(&claude, b"#!/bin/sh\nexit 0\n").expect("write");
    gilde_code_lib::supervise::spawn::allow_executable(&claude, &gilde_code_lib::agents::VENDOR_KINDS);
    let node = dir.path().join("node");
    std::fs::write(&node, b"#!/bin/sh\nexit 0\n").expect("write");
    gilde_code_lib::supervise::spawn::allow_executable(&node, &[K::Sidecar, K::Probe]);

    let req = |argv0: &std::path::Path, kind: K, generation: Option<i64>, turn_id: Option<&str>| SpawnRequest {
        kind,
        argv0: AbsPath::parse(argv0.to_string_lossy().into_owned()).unwrap(),
        args: Vec::new(),
        cwd: None,
        env_keys: Vec::new(),
        parent_uid: None,
        generation,
        turn_id: turn_id.map(String::from),
    };
    let long = "t".repeat(129);
    let cases: Vec<(SpawnRequest, &str, &str)> = vec![
        (req(&node, K::Sidecar, None, None), "generation", "required"),
        (req(&node, K::Sidecar, Some(0), None), "generation", "required"),
        (req(&claude, K::Turn, None, Some("t-1")), "generation", "required"),
        (req(&claude, K::Convo, Some(3), None), "generation", "reserved"),
        (req(&claude, K::Turn, Some(3), None), "turnId", "required"),
        (req(&claude, K::Turn, Some(3), Some("")), "turnId", "required"),
        (req(&claude, K::Turn, Some(3), Some(&long)), "turnId", "maxLength"),
        (req(&claude, K::Turn, Some(3), Some("t\n1")), "turnId", "control"),
        (req(&node, K::Sidecar, Some(3), Some("t-1")), "turnId", "reserved"),
        (req(&claude, K::Convo, None, Some("t-1")), "turnId", "reserved"),
        // A15.1: a `Vendorcli` generation is optional, ≥ 1 when present; every
        // other kind but Sidecar/Turn keeps it reserved.
        (req(&claude, K::Vendorcli, Some(0), None), "generation", "range"),
        (req(&claude, K::Vendorcli, Some(-2), None), "generation", "range"),
        (req(&node, K::Probe, Some(3), None), "generation", "reserved"),
        (req(&claude, K::Vendorcli, Some(3), Some("t-1")), "turnId", "reserved"),
    ];
    for (request, field, rule) in cases {
        let what = format!("{:?} gen {:?} turn {:?}", request.kind, request.generation, request.turn_id);
        let err = SupervisorApi::plan_spawn(&sup, request).await.expect_err(&what);
        assert_eq!(err.code, ErrorCode::InvalidArgument, "{what}");
        assert_eq!(err.params["field"], field, "{what}");
        assert_eq!(err.params["rule"], rule, "{what}");
    }
    let instance = sup.app_instance_id().to_string();
    let rows = db
        .read(move |c| dao::proc::rows_of_instance(c, &instance))
        .await
        .expect("read");
    assert!(rows.is_empty(), "nothing is planned for a malformed request: {rows:?}");

    // 128 bytes is still a valid turn id; the row carries the generation and
    // the turn id, and the generation sweep finds it.
    let id = "t".repeat(128);
    let ticket = SupervisorApi::plan_spawn(&sup, req(&claude, K::Turn, Some(7), Some(&id)))
        .await
        .expect("turn");
    let sidecar = SupervisorApi::plan_spawn(&sup, req(&node, K::Sidecar, Some(7), None))
        .await
        .expect("sidecar");
    // A15.1: a sidecar-spawned probe carries the generation; one spawned by
    // the app (detection) carries none.
    let probe = SupervisorApi::plan_spawn(&sup, req(&claude, K::Vendorcli, Some(7), None))
        .await
        .expect("sidecar probe");
    let app_probe = SupervisorApi::plan_spawn(&sup, req(&claude, K::Vendorcli, None, None))
        .await
        .expect("app probe");
    let live = db
        .read(move |c| dao::proc::live_rows_of_generation(c, 7))
        .await
        .expect("read");
    assert!(
        live.iter().any(|r| r.spawn_id == probe.spawn_id && r.kind == ProcKind::Vendorcli && r.generation == 7),
        "the dead-generation sweep finds a sidecar probe (A15.1)"
    );
    assert!(!live.iter().any(|r| r.spawn_id == app_probe.spawn_id), "an app probe is not part of a generation");
    let turn_row = live.iter().find(|r| r.spawn_id == ticket.spawn_id).expect("the sweep finds the turn");
    assert_eq!(turn_row.kind, ProcKind::Turn);
    assert_eq!(turn_row.generation, 7);
    assert_eq!(turn_row.run_id.as_deref(), Some(id.as_str()));
    assert!(live.iter().any(|r| r.spawn_id == sidecar.spawn_id), "and the sidecar row");
}

rusty_fork_test! {
    /// A14.2: a worktree post-create command whose plan is refused never
    /// runs — the refusal comes back as the error and no process is
    /// launched. In a process of its own: the supervisor binds once per
    /// process, and `git_invocations_are_registered_before_they_spawn` owns
    /// that binding in this one.
    #[test]
    fn a_refused_post_create_launches_no_process() {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(refused_post_create_launches_no_process());
    }
}

async fn refused_post_create_launches_no_process() {
    use std::os::unix::fs::PermissionsExt as _;
    use gilde_code_lib::contract::{ErrorCode, IpcError};
    use gilde_code_lib::paths::AppPaths;
    use gilde_code_lib::worktree::WorktreeManager;

    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::open_in_memory().expect("db");
    let config = Arc::new(ConfigStore::open_at(&dir.path().join("config.json")).expect("config"));
    let sup = Arc::new(Supervisor::open_instance(&db, config, "0.0.0-test").await.expect("instance"));
    // The registry is wired: this forked process's one binding.
    assert!(
        gilde_code_lib::worktree::git::bind_supervisor(sup as Arc<dyn SupervisorApi>),
        "the supervisor binds once per process"
    );

    // A command nobody allowlisted, which would leave a mark if it ran.
    let ran = dir.path().join("ran");
    let script = dir.path().join("post-create.sh");
    std::fs::write(&script, format!("#!/bin/sh\ntouch '{}'\n", ran.display())).expect("write");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    let paths = Arc::new(AppPaths::new(dir.path().join("data"), dir.path().join("home")));
    let manager = WorktreeManager::new(db.clone(), paths, None);
    let err = manager
        .run_post_create(dir.path(), &script.to_string_lossy())
        .await
        .expect_err("a refused post-create is an error, not an untracked run");
    let err = IpcError::from(&err);
    assert_eq!(err.code, ErrorCode::Refused);
    assert_eq!(err.params["reason"], "notAllowlisted");
    // `run_post_create` returns only after a launched command exits, so an
    // error here with no mark means nothing ran.
    assert!(!ran.exists(), "the refused command never ran");
}

/// A39.1(a) (G7c-01): the registry records the vendor with the one shared
/// classifier over the whole argv — the native installer's realpath, cursor's
/// vendored node, a node script after its flags, and the command a hosted
/// pane runs — and `is_vendor_cli` knows the layouts. Plans only; nothing runs.
#[tokio::test]
async fn the_registry_records_the_vendor_of_every_install_layout() {
    use gilde_code_lib::contract::common::AbsPath;
    use gilde_code_lib::contract::process::ProcKind as K;
    use gilde_code_lib::contract::services::SpawnRequest;

    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::open_in_memory().expect("db");
    let config = Arc::new(ConfigStore::open_at(&dir.path().join("config.json")).expect("config"));
    let sup = Supervisor::open_instance(&db, config, "0.0.0-test").await.expect("instance");
    let home = tempfile::tempdir().expect("home");
    let file = |rel: &str| {
        let p = home.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, b"x").unwrap();
        p.to_string_lossy().into_owned()
    };
    let native = file(".local/share/claude/versions/2.1.307");
    let cursor_node = file(".local/share/cursor-agent/versions/2026.10.19-6bbf2c4/node");
    let cursor_index = file(".local/share/cursor-agent/versions/2026.10.19-6bbf2c4/index.js");
    let node = file("bin/node");
    let cli = file("lib/node_modules/@anthropic-ai/claude-code/cli.js");
    let plain = file("bin/agent-cli");
    let cases: Vec<(K, Vec<String>, Option<&str>)> = vec![
        (K::Convo, vec![native.clone(), "--resume".into()], Some("claude")),
        (K::Pty, vec![cursor_node.clone(), "--use-system-ca".into(), cursor_index], Some("cursor")),
        (K::Pty, vec![node.clone(), "--max-old-space-size=4096".into(), cli], Some("claude")),
        (K::Pty, vec![node, "/srv/server.js".into()], None),
        (K::Pane, vec!["/bin/sh".into(), "-c".into(), format!("cd /tmp && {native} --agent-id a@t")], Some("claude")),
        (K::Pane, vec!["/bin/sh".into(), "-c".into(), "cat".into()], None),
        (K::Convo, vec![plain, "-p".into()], None),
    ];
    for (kind, argv, want) in cases {
        if argv[0] != "/bin/sh" {
            gilde_code_lib::supervise::spawn::allow_executable(Path::new(&argv[0]), &[kind]);
        }
        let req = SpawnRequest {
            kind,
            argv0: AbsPath::parse(argv[0].clone()).unwrap(),
            args: argv[1..].to_vec(),
            cwd: None,
            env_keys: Vec::new(),
            parent_uid: None,
            generation: None,
            turn_id: None,
        };
        let ticket = SupervisorApi::plan_spawn(&sup, req).await.expect("plan");
        let id = ticket.spawn_id.clone();
        let row = db.read(move |c| dao::proc::get(c, &id)).await.expect("read").expect("row");
        assert_eq!(row.vendor.as_deref(), want, "{kind:?} {argv:?}");
        SupervisorApi::mark_exited(&sup, &ticket, gilde_code_lib::contract::activity::ExitInfo { code: Some(0), signal: None }).await;
    }
    assert!(SupervisorApi::is_vendor_cli(&sup, &native));
    assert!(SupervisorApi::is_vendor_cli(&sup, &cursor_node));
    assert!(!SupervisorApi::is_vendor_cli(&sup, &home.path().join("bin/node").to_string_lossy()));
}
