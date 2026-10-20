//! Process-group discipline and the write-before-spawn hook (DESIGN §8-1, §8-3;
//! M1-PLAN T08 acceptance 8–9).
//!
//! Two properties are checked against the real kernel and the real `git`:
//!
//!   1. a child started through [`tokio_command_grouped`] is a **process group
//!      leader**, so `killpg` on its pgid can never reach anything we did not
//!      create;
//!   2. `worktree::git::run_git` writes a durable registry row **before** it
//!      spawns — `git` is not exempt from the rule.

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use gilde_code_lib::config::ConfigStore;
use gilde_code_lib::store::models::{ProcKind, ProcState};
use gilde_code_lib::store::{dao, Db};
use gilde_code_lib::supervise::registry::{install_git_spawn_hook, Supervisor};
use gilde_code_lib::supervise::spawn::{pgid_of, tokio_command_grouped};

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
        install_git_spawn_hook(Arc::clone(&supervisor)),
        "the hook installs once per process"
    );
    assert!(
        !install_git_spawn_hook(Arc::clone(&supervisor)),
        "and a second installation is refused rather than swapping a live hook"
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
