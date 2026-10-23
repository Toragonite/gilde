//! T-shim-* / T-team-* against the PaneHost service directly (contract §7.4,
//! §4.6): every auth refusal (counted, never a deviation), the rate limit,
//! the 16th pane, ids never reused, respawn input rules, unknown panes,
//! remain-on-exit, close signals (never SIGKILL), lead exit, the backgrounded
//! lead, late spawns, notes, the hosted lead env (T-team-1) and the scan hook.
//! Owner: T07.
//!
//! The "lead" is this test process's parent (a live process that is only
//! ever compared, never signalled); each direct call's kernel peer is this
//! process, so it passes the parent check like the lead's own shim would.

#[path = "fixtures/fake_tmux_client/mod.rs"]
mod fake_tmux_client;

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::time::Duration;

use fake_tmux_client::host::{parent_pid, wait_for, Rig, FAKE_PID_BASE, LEAD_SID, VERSION};
use gilde_code_lib::contract::common::Vendor;
use gilde_code_lib::contract::ids::{ProcessKey, SessionUid};
use gilde_code_lib::contract::services::{PaneHostApi, ProcInfo, ProcSnapshot, SignalPlan, SourceFacts, TeammateLabels};
use gilde_code_lib::contract::activity::ProcessFacts;
use gilde_code_lib::panehost::auth::{PeerCred, Refusal};
use gilde_code_lib::panehost::proto::ShimResponse;
use gilde_code_lib::panehost::subcommands::texts;
use gilde_code_lib::panehost::{apply_lead_env, LeadPhase, PaneKind, PaneState, Purpose, UnknownReason};

const NEW_SESSION: [&str; 11] = ["new-session", "-d", "-s", "claude-swarm", "-n", "swarm-view", "-P", "-F", "#{pane_id}", "--", "cat"];
const LIST: [&str; 5] = ["list-panes", "-t", "claude-swarm:swarm-view", "-F", "#{pane_id}"];

fn split(target: &str) -> Vec<&str> {
    vec!["split-window", "-d", "-t", target, "-v", "-P", "-F", "#{pane_id}", "--", "cat"]
}

fn ok(stdout: &str) -> ShimResponse {
    ShimResponse::ok(stdout)
}

fn fail(stderr: &str) -> ShimResponse {
    ShimResponse::fail(stderr)
}

async fn direct_rig() -> Rig {
    Rig::new(parent_pid()).without_listener()
}

async fn respawn(rig: &Rig, pane: &str, command: &str) -> ShimResponse {
    rig.call(&["respawn-pane", "-k", "-t", pane, "--", command]).await
}

/// T-shim-auth: wrong token, wrong or non-canonical pid, foreign uid, a
/// request that is neither `-V` nor a swarm call — each `not authorized`,
/// counted, and never a compat deviation or note [N-02].
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t_shim_auth_refusals_are_counted_never_deviations() {
    let rig = direct_rig().await;
    let c = rig.host.counters();

    let mut r = rig.req(&["has-session", "-t", "claude-swarm"]);
    r.token = "f".repeat(64);
    assert_eq!(rig.host.handle(r, rig.peer()).await, ShimResponse::not_authorized());
    assert_eq!(c.refusals(Refusal::Token), 1);

    for socket in [format!("claude-swarm-{}", rig.lead_pid + 1), format!("claude-swarm-0{}", rig.lead_pid)] {
        let mut r = rig.req(&["new-session"]);
        r.argv[1] = socket;
        assert_eq!(rig.host.handle(r, rig.peer()).await, ShimResponse::not_authorized());
    }
    assert_eq!(c.refusals(Refusal::LeadPid), 2);

    let foreign = PeerCred { uid: rig.peer().uid + 1, pid: rig.peer().pid };
    assert_eq!(rig.host.handle(rig.req(&["has-session", "-t", "claude-swarm"]), foreign).await, ShimResponse::not_authorized());
    assert_eq!(c.refusals(Refusal::ForeignUid), 1);

    let mut r = rig.req(&[]);
    r.argv = vec!["ls".into()];
    assert_eq!(rig.host.handle(r, rig.peer()).await.exit, 1);
    assert_eq!(c.refusals(Refusal::BadFrame), 1);

    // A mutating call whose kernel peer is not a child of the lead.
    let grandchild = PeerCred { uid: rig.peer().uid, pid: parent_pid() };
    assert_eq!(rig.host.handle(rig.req(&NEW_SESSION), grandchild).await, ShimResponse::not_authorized());
    assert_eq!(c.refusals(Refusal::NotParent), 1);

    assert!(rig.compat.deviations().is_empty() && rig.compat.notes().is_empty());
    assert_eq!(rig.call(&["has-session", "-t", "claude-swarm"]).await, fail(texts::CANT_FIND_SESSION), "the real lead still works");
}

/// §4.6 limits: ≤ 200 requests per minute per lead, then `busy`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t_shim_rate_limit() {
    let rig = direct_rig().await;
    for _ in 0..200 {
        assert_eq!(rig.call(&["has-session", "-t", "claude-swarm"]).await, fail(texts::CANT_FIND_SESSION));
    }
    assert_eq!(rig.call(&["has-session", "-t", "claude-swarm"]).await, fail("gilde-tmux: busy\n"));
    assert_eq!(rig.host.counters().refusals(Refusal::RateLimited), 1);
    assert!(rig.compat.deviations().is_empty());
}

/// §7.4.4 row 5/6: at most 15 panes per lead; the 16th is `no space for new
/// pane` (Claude's "no room" message), new windows too; ids are never reused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t_shim_16th_pane_is_refused_and_ids_are_never_reused() {
    let rig = direct_rig().await;
    assert_eq!(rig.call(&NEW_SESSION).await, ok("%0\n"));
    for n in 1..15 {
        assert_eq!(rig.call(&split("%0")).await, ok(&format!("%{n}\n")));
    }
    assert_eq!(rig.call(&split("%0")).await, fail(texts::NO_SPACE));
    assert_eq!(
        rig.call(&["new-window", "-t", "claude-swarm", "-n", "swarm-view", "-P", "-F", "#{pane_id}", "--", "cat"]).await,
        fail(texts::NO_SPACE)
    );
    assert_eq!(rig.call(&["kill-pane", "-t", "%3"]).await, ok(""));
    assert_eq!(rig.call(&split("%0")).await, ok("%15\n"), "never %3 again");
    assert!(rig.compat.deviations().is_empty(), "a full lead is not a deviation");
}

/// T-shim-ids: the session dies with its last pane; a new session continues
/// the ids; a new lead starts its own server.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t_shim_ids_never_reused() {
    let rig = direct_rig().await;
    assert_eq!(rig.call(&NEW_SESSION).await, ok("%0\n"));
    assert_eq!(rig.call(&split("%0")).await, ok("%1\n"));
    assert_eq!(rig.call(&["kill-pane", "-t", "%1"]).await, ok(""));
    assert_eq!(rig.call(&split("%0")).await, ok("%2\n"));
    for p in ["%0", "%2"] {
        assert_eq!(rig.call(&["kill-pane", "-t", p]).await, ok(""));
    }
    assert_eq!(rig.call(&["has-session", "-t", "claude-swarm"]).await, fail(texts::CANT_FIND_SESSION));
    assert_eq!(rig.call(&LIST).await, fail(texts::CANT_FIND_SESSION));
    assert_eq!(rig.call(&NEW_SESSION).await, ok("%3\n"));
    assert_eq!(rig.call(&["list-panes", "-t", "@1", "-F", "#{pane_id}"]).await, ok("%3\n"));
    assert_eq!(rig.call(&["list-panes", "-t", "@0", "-F", "#{pane_id}"]).await, fail("can't find window: @0\n"));
    let other = direct_rig().await;
    assert_eq!(other.call(&NEW_SESSION).await, ok("%0\n"), "a new lead gets a new server");
}

/// T-shim-respawnInput (HP-3): control characters, NUL, > 64 KiB are refused
/// before anything runs; exactly 64 KiB is accepted; two command words are a
/// shape deviation; a pane of another lead or a never-allocated id is unknown.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t_shim_respawn_input_rules() {
    let rig = direct_rig().await;
    rig.call(&NEW_SESSION).await;
    assert_eq!(respawn(&rig, "%0", "a\u{7}b").await, fail(texts::BAD_COMMAND));
    assert_eq!(respawn(&rig, "%0", "echo\nhi").await, fail(texts::BAD_COMMAND));
    assert_eq!(respawn(&rig, "%0", "a\u{0}b").await, fail("gilde-tmux: bad request\n"), "NUL fails the frame");
    let over = "x".repeat(64 * 1024 + 1);
    assert_eq!(respawn(&rig, "%0", &over).await.exit, 1);
    assert_eq!(respawn(&rig, "%9", "echo hi").await, fail("can't find pane: %9\n"));
    assert_eq!(rig.call(&["kill-pane", "-t", "%999999999"]).await, fail("can't find pane: %999999999\n"));
    assert_eq!(rig.pty.open_count(), 0, "nothing spawned");
    assert!(rig.compat.deviations().is_empty(), "value refusals are never deviations");

    let at_cap = "x".repeat(64 * 1024);
    assert_eq!(respawn(&rig, "%0", &at_cap).await, ok(""));
    assert_eq!(rig.pty.open_count(), 1);

    let other = direct_rig().await;
    other.call(&NEW_SESSION).await;
    other.call(&split("%0")).await;
    // %1 exists only in the other lead's server.
    assert_eq!(rig.call(&["kill-pane", "-t", "%1"]).await, fail("can't find pane: %1\n"));

    assert_eq!(rig.call(&["respawn-pane", "-k", "-t", "%0", "--", "a", "b"]).await.stderr, "gilde-tmux: unsupported command: respawn-pane\n");
    assert_eq!(rig.compat.deviations().len(), 1, "a changed argv shape is a deviation");
}

/// F-07 / N-07 (T-shim-unknownPane): only the teammate launch pattern of this
/// lead's own binary is a teammate; anything else runs as an unknown pane —
/// no session row, never resolvable as a member.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t_shim_unknown_panes_are_never_members() {
    let rig = direct_rig().await;
    rig.call(&NEW_SESSION).await;
    for _ in 0..4 {
        rig.call(&split("%0")).await;
    }
    let other_bin = rig.root.join("bin").join("other-claude");
    std::fs::write(&other_bin, b"#!/bin/false\n").unwrap();
    std::fs::set_permissions(&other_bin, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let link = rig.root.join("bin").join("claude");
    std::os::unix::fs::symlink(&rig.claude, &link).unwrap();

    assert_eq!(respawn(&rig, "%0", "echo hi").await, ok(""));
    assert_eq!(respawn(&rig, "%1", &rig.teammate_command_with("bob", "red", other_bin.to_str().unwrap())).await, ok(""));
    assert_eq!(respawn(&rig, "%2", &rig.teammate_command("alice", "blue").replace(LEAD_SID, "another-session")).await, ok(""));
    assert_eq!(respawn(&rig, "%3", &rig.teammate_command_with("carol", "green", link.to_str().unwrap())).await, ok(""));
    assert_eq!(respawn(&rig, "%4", &rig.teammate_command("dave", "cyan")).await, ok(""));

    let kinds: Vec<PaneKind> = rig.table().panes.into_iter().map(|p| p.kind).collect();
    assert_eq!(kinds[0], PaneKind::Unknown { reason: UnknownReason::NotTeammateCommand });
    assert_eq!(kinds[1], PaneKind::Unknown { reason: UnknownReason::BinaryDiffers });
    assert_eq!(UnknownReason::BinaryDiffers.details_key(), Some("details.teammateBinaryDiffers"), "A21.3");
    assert_eq!(UnknownReason::NotTeammateCommand.details_key(), None);
    assert_eq!(kinds[2], PaneKind::Unknown { reason: UnknownReason::OtherParent });
    assert!(matches!(&kinds[3], PaneKind::Teammate { agent_id, .. } if agent_id == "carol@session-72cb5f46"));
    assert!(matches!(&kinds[4], PaneKind::Teammate { .. }));
    assert_eq!(rig.pty.open_count(), 5, "unknown panes still run");

    // Session rows only for the two teammates.
    let spawned: Vec<String> = rig
        .world
        .evidence()
        .into_iter()
        .filter(|u| matches!(u.facts, SourceFacts::Gilde(_)))
        .filter_map(|u| u.hint.gilde_spawn_id)
        .collect();
    assert_eq!(spawned, vec!["spawn-3".to_string(), "spawn-4".to_string()]);
    let bound: Vec<String> = rig.sessions.bound.lock().unwrap().iter().map(|b| b.0.clone()).collect();
    assert_eq!(bound, vec!["spawn-3".to_string(), "spawn-4".to_string()], "unknown panes are never sessions");

    // A member that points at an unknown pane is never typed into.
    let (team, keys) = rig.hosted_team(&[("mallory", 0), ("dave", 4)]);
    rig.set_member(&keys[0], |m| m.agent_id = None);
    let err = rig.host.resolve_member(&team, &keys[0], Purpose::Send).unwrap_err();
    assert_eq!(serde_json::to_value(&err).unwrap()["params"]["cap"], "viaLeadOnly");
    let err = rig.host.resolve_member(&team, &keys[0], Purpose::Close).unwrap_err();
    assert_eq!(serde_json::to_value(&err).unwrap()["params"]["cap"], "notAPane");
    assert!(rig.host.resolve_member(&team, &keys[1], Purpose::Send).is_ok());
}

async fn running_panes(rig: &Rig, n: usize) {
    rig.call(&NEW_SESSION).await;
    for _ in 1..n {
        rig.call(&split("%0")).await;
    }
    let names = ["alpha", "beta", "gamma", "delta"];
    for (i, name) in names.iter().take(n).enumerate() {
        assert_eq!(respawn(rig, &format!("%{i}"), &rig.teammate_command(name, "blue")).await, ok(""));
    }
}

/// §7.4.6: remain-on-exit `failed` keeps crashes (listed, "Crashed · exit
/// {code}"), drops clean exits; `on` keeps everything; unset keeps nothing.
/// kill-pane removes a kept pane without signalling, answers 0 for a pane
/// already gone, and `can't find pane` for one it never had.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t_shim_remain_on_exit() {
    let rig = direct_rig().await;
    running_panes(&rig, 4).await;
    for (pane, value) in [("%0", "failed"), ("%2", "failed"), ("%3", "on")] {
        assert_eq!(rig.call(&["set-option", "-p", "-t", pane, "remain-on-exit", value]).await, ok(""));
    }
    let pty = |i: usize| rig.pty.handle(i).pty_id;
    rig.pty.exit(&pty(0), Some(1), None);
    rig.pty.exit(&pty(1), Some(2), None);
    rig.pty.exit(&pty(2), Some(0), None);
    rig.pty.exit(&pty(3), Some(0), None);
    assert_eq!(rig.call(&LIST).await, ok("%0\n%3\n"));
    let panes = rig.table().panes;
    assert!(matches!(panes[0].state, PaneState::Exited { code: Some(1), kept: true, .. }));
    assert!(matches!(panes[1].state, PaneState::Exited { code: Some(2), kept: false, .. }));
    assert!(matches!(panes[2].state, PaneState::Exited { code: Some(0), kept: false, .. }));
    assert!(matches!(panes[3].state, PaneState::Exited { code: Some(0), kept: true, .. }));

    // The member's process exited: exit evidence by spawn id.
    let exited = rig
        .world
        .evidence()
        .into_iter()
        .filter(|u| matches!(u.facts, SourceFacts::Process(ProcessFacts::Exited { .. })))
        .count();
    assert_eq!(exited, 4);

    // kill-pane on a kept dead pane removes it; nothing is signalled.
    assert_eq!(rig.call(&["kill-pane", "-t", "%0"]).await, ok(""));
    assert_eq!(rig.call(&["kill-pane", "-t", "%2"]).await, ok(""), "already gone: 0, as at /exit");
    assert_eq!(rig.call(&["kill-pane", "-t", "%7"]).await, fail("can't find pane: %7\n"));
    assert_eq!(rig.call(&LIST).await, ok("%3\n"));
    assert!(rig.pty.closes().is_empty(), "dead processes are never signalled");

    // A kept dead pane can be respawned.
    assert_eq!(respawn(&rig, "%3", &rig.teammate_command("delta", "blue")).await, ok(""));
    assert!(matches!(rig.table().panes[3].state, PaneState::Running { .. }));
    assert!(rig.compat.deviations().is_empty());
}

/// T-shim-closePane / C5: kill-pane is SIGHUP then a fenced SIGTERM after
/// 2 s; respawn -k on a running pane uses 1 s; no plan can express SIGKILL.
/// The answer does not wait for the signals.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t_shim_close_signals_never_sigkill() {
    let rig = direct_rig().await;
    running_panes(&rig, 2).await;
    let first = rig.pty.handle(0).pty_id;
    assert_eq!(respawn(&rig, "%0", &rig.teammate_command("alpha", "blue")).await, ok(""));
    assert_eq!(rig.pty.wait_close_of(&first), SignalPlan::HupThenTerm { grace_ms: 1_000 });
    assert_eq!(rig.pty.open_count(), 3);
    // The replaced process's late exit is not the pane's.
    rig.pty.exit(&first, None, Some(1));
    assert!(matches!(rig.table().panes[0].state, PaneState::Running { .. }));

    let beta = rig.pty.handle(1).pty_id;
    assert_eq!(rig.call(&["kill-pane", "-t", "%1"]).await, ok(""));
    assert_eq!(rig.pty.wait_close_of(&beta), SignalPlan::HupThenTerm { grace_ms: 2_000 });
    assert!(matches!(rig.table().panes[1].state, PaneState::Killed { .. }));
    for (_, plan) in rig.pty.closes() {
        assert!(matches!(plan, SignalPlan::HupThenTerm { .. }), "SIGHUP first, never a bare TERM or KILL");
    }
    // A Gilde-sent signal is a stop, never a crash.
    rig.pty.exit(&beta, None, Some(1));
    let ev = rig.world.evidence();
    assert!(ev.iter().any(|u| u.hint.gilde_spawn_id.as_deref() == Some("spawn-1")
        && matches!(&u.facts, SourceFacts::Gilde(g) if g.stop_cause.is_some())));
}

/// §7.4.7: the lead's exit closes every pane (HUP, TERM after 2 s) and drops
/// the server; its token is revoked.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t_shim_lead_exit_closes_panes() {
    let rig = direct_rig().await;
    running_panes(&rig, 2).await;
    rig.call(&split("%0")).await; // a placeholder
    PaneHostApi::lead_exited(&*rig.host, &rig.lead).await;
    let closes = rig.pty.closes();
    assert_eq!(closes.len(), 2);
    for (_, plan) in closes {
        assert_eq!(plan, SignalPlan::HupThenTerm { grace_ms: 2_000 });
    }
    assert!(rig.host.table(&rig.lead).is_none());
    assert_eq!(rig.tables.tables.lock().unwrap().last().unwrap().phase, LeadPhase::Ended);
    assert_eq!(rig.call(&["has-session", "-t", "claude-swarm"]).await, ShimResponse::not_authorized());
    assert_eq!(rig.host.counters().refusals(Refusal::Token), 1);
}

/// A40.3: panes that survive the lead's exit (HUP, TERM after 2 s, never
/// KILL) are recorded, not only logged: their rows become Leftover
/// `TeammateOfGoneLead` in the health scan.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a40_panes_that_survive_the_lead_exit_are_recorded() {
    let rig = direct_rig().await;
    running_panes(&rig, 2).await;
    rig.pty.survive.store(true, Ordering::SeqCst);
    PaneHostApi::lead_exited(&*rig.host, &rig.lead).await;
    let closes = rig.pty.closes();
    assert_eq!(closes.len(), 2);
    let opens = rig.pty.opens.lock().unwrap().clone();
    let recorded = gilde_code_lib::supervise::reaper::lead_gone_survivor_spawn_ids();
    for (pty, _) in closes {
        let (_, handle) = opens.iter().find(|(_, h)| h.pty_id == pty).expect("an opened pane");
        assert!(recorded.contains(&handle.ticket.spawn_id), "{} not recorded: {recorded:?}", handle.ticket.spawn_id);
    }
}

/// F-19 / P39 (T-team-backgroundLead): a lead that moved to the background
/// keeps its panes; its later calls are refused (counted, never deviations);
/// the server ends with its last pane.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t_team_background_lead_keeps_panes() {
    let rig = direct_rig().await;
    running_panes(&rig, 2).await;
    rig.fate.backgrounded.store(true, Ordering::SeqCst);
    PaneHostApi::lead_exited(&*rig.host, &rig.lead).await;
    assert!(rig.pty.closes().is_empty(), "the teammates keep running");
    assert_eq!(rig.host.lead_phase(&rig.lead), Some(LeadPhase::Backgrounded));
    assert_eq!(rig.table().phase, LeadPhase::Backgrounded);

    // The bg job is another process: its calls name another pid.
    let mut r = rig.req(&["display-message", "-p", "x"]);
    r.argv[1] = format!("claude-swarm-{}", rig.lead_pid + 7);
    assert_eq!(rig.host.handle(r, rig.peer()).await, ShimResponse::not_authorized());
    assert_eq!(rig.call(&["display-message", "-p", "x"]).await, ShimResponse::not_authorized());
    assert!(rig.compat.deviations().is_empty(), "N-02: a backgrounded lead's refusals are never deviations");

    rig.pty.exit(&rig.pty.handle(0).pty_id, Some(0), None);
    assert_eq!(rig.host.lead_phase(&rig.lead), Some(LeadPhase::Backgrounded));
    rig.pty.exit(&rig.pty.handle(1).pty_id, Some(0), None);
    assert_eq!(rig.host.lead_phase(&rig.lead), None, "the last pane closed: the server is dropped");
}

/// A spawn that misses the deadline answers `failed to spawn`; its late
/// process is closed, never adopted (a pane is never created twice). A spawn
/// error leaves a failed placeholder ("Teammate did not start").
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t_shim_late_or_failed_spawns() {
    let rig = Rig::with_deadline(parent_pid(), Duration::from_millis(200)).without_listener();
    rig.call(&NEW_SESSION).await;
    *rig.pty.open_delay.lock().unwrap() = Some(Duration::from_millis(1_500));
    assert_eq!(respawn(&rig, "%0", &rig.teammate_command("alpha", "blue")).await, fail(texts::RESPAWN_FAILED));
    assert!(matches!(rig.table().panes[0].state, PaneState::Placeholder { spawn_failed: true, .. }));
    let late = wait_for("the late open", || (rig.pty.open_count() == 1).then(|| rig.pty.handle(0).pty_id));
    assert_eq!(rig.pty.wait_close_of(&late), SignalPlan::HupThenTerm { grace_ms: 1_000 });
    assert!(matches!(rig.table().panes[0].state, PaneState::Placeholder { .. }), "never adopted");

    *rig.pty.open_delay.lock().unwrap() = None;
    rig.pty.fail_next.store(true, Ordering::SeqCst);
    assert_eq!(respawn(&rig, "%0", &rig.teammate_command("alpha", "blue")).await, fail(texts::RESPAWN_FAILED));
    assert!(matches!(rig.table().panes[0].state, PaneState::Placeholder { spawn_failed: true, .. }));
    assert_eq!(respawn(&rig, "%0", &rig.teammate_command("alpha", "blue")).await, ok(""), "a later respawn works");
}

/// §7.4.4 row 7/9: unknown option names and colours answer 0 with a note
/// (not a failed version); the colour map is applied.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t_shim_option_notes() {
    let rig = direct_rig().await;
    rig.call(&NEW_SESSION).await;
    assert_eq!(rig.call(&["set-option", "-p", "-t", "%0", "synchronize-panes", "on"]).await, ok(""));
    assert_eq!(rig.call(&["set-option", "-p", "-t", "%0", "pane-border-style", "fg=colour99"]).await, ok(""));
    assert_eq!(rig.call(&["set-option", "-w", "-t", "claude-swarm:swarm-view", "mouse", "on"]).await, ok(""));
    assert_eq!(rig.call(&["set-option", "-p", "-t", "%0", "window-style", "bg=default,fg=colour208"]).await, ok(""));
    let notes: Vec<String> = rig.compat.notes().into_iter().map(|(v, n)| {
        assert_eq!(v, VERSION);
        n
    }).collect();
    assert_eq!(notes, vec![
        "unknown shim pane option: synchronize-panes".to_string(),
        "unknown shim pane colour".to_string(),
        "unknown shim window option: mouse".to_string(),
    ]);
    assert!(rig.compat.deviations().is_empty());
    assert_eq!(rig.table().panes[0].colour, Some(gilde_code_lib::contract::common::IdentityColour::Orange));
}

/// T-team-1: the env of a hosted lead through `PaneHostApi::lead_env` and
/// `panehost::apply_lead_env` — no TMUX/TMUX_PANE/ITERM_*/LC_TERMINAL*,
/// `TERM_PROGRAM=gilde-code`, the agent-teams flag, the shim dir first on
/// PATH, the real tmux (if any) found on PATH minus the shim dirs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t_team_1_hosted_lead_env() {
    let rig = direct_rig().await;
    let mut env: BTreeMap<String, String> = rig.env.base.lock().unwrap().clone();
    env.insert("LC_TERMINAL".into(), "iTerm2".into());
    env.insert("TMUX_PANE".into(), "%1".into());
    env.insert("ITERM_PROFILE".into(), "Default".into());
    let lead = SessionUid::mint();
    let le = PaneHostApi::lead_env(&*rig.host, &lead).unwrap();
    apply_lead_env(&mut env, &le);
    for absent in ["TMUX", "TMUX_PANE", "ITERM_SESSION_ID", "ITERM_PROFILE", "LC_TERMINAL"] {
        assert!(!env.contains_key(absent), "{absent}");
    }
    assert_eq!(env["TERM_PROGRAM"], "gilde-code");
    assert_eq!(env["CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS"], "1");
    let shim_dir = rig.host.config().shim_dir.to_str().unwrap().to_string();
    assert_eq!(env["PATH"].split(':').next(), Some(shim_dir.as_str()));
    assert_eq!(env["GILDE_REAL_TMUX"], "", "no tmux on the base PATH");
    assert_eq!(env["GILDE_PANE_HOST"], rig.socket().to_str().unwrap());
    assert_ne!(env["GILDE_PANE_HOST_TOKEN"], rig.token, "a token per lead");
    assert!(!format!("{le:?}").contains(&env["GILDE_PANE_HOST_TOKEN"]), "never in Debug");

    let tmux_dir = rig.root.join("realbin");
    std::fs::create_dir_all(&tmux_dir).unwrap();
    let fake = tmux_dir.join("tmux");
    std::fs::write(&fake, b"#!/bin/false\n").unwrap();
    std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", shim_dir, tmux_dir.display());
    rig.env.base.lock().unwrap().insert("PATH".into(), path);
    let le = PaneHostApi::lead_env(&*rig.host, &SessionUid::mint()).unwrap();
    let real = le.set.iter().find(|(k, _)| k == "GILDE_REAL_TMUX").unwrap().1.clone();
    assert_eq!(real, fake.to_str().unwrap(), "the real tmux on PATH minus the shim dir (never executed)");
}

fn proc_info(pid: i32, ppid: i32, start: i64, exec: &str, argv: &[&str], vendor: Option<Vendor>, parent_sid: Option<&str>) -> ProcInfo {
    ProcInfo {
        key: ProcessKey::new(pid as u32, start as u64),
        pid,
        ppid,
        uid: rig_uid(),
        start_sec: start,
        exec_path: Some(exec.to_string()),
        argv_redacted: argv.iter().map(|s| s.to_string()).collect(),
        labels: TeammateLabels { parent_session_id: parent_sid.map(str::to_string), ..Default::default() },
        cwd: None,
        rss_bytes: 0,
        cpu_ns: 0,
        cco_team_member: None,
        gilde_app_instance: None,
        vendor,
    }
}

fn rig_uid() -> u32 {
    gilde_code_lib::panehost::auth::app_uid()
}

/// §7.4.8/§7.4.9 scan hook: a real `tmux -L claude-swarm-<lead>` means the
/// shim fell back (OutsideGilde); a shim process is not one. A teammate
/// process outside every pane, seen twice, is a deviation; survivors
/// (`ppid 1`), pane descendants and processes older than the lead are not.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t_shim_scan_hook() {
    let rig = direct_rig().await;
    running_panes(&rig, 1).await;
    let start = gilde_code_lib::panehost::auth::start_of(rig.lead_pid).unwrap();
    let sock = format!("claude-swarm-{}", rig.lead_pid);
    let shim_exec = rig.shim();
    let shim_proc = proc_info(3_950_001, rig.lead_pid, start, shim_exec.to_str().unwrap(), &["tmux", "-L", &sock, "has-session"], None, None);
    let snap = |procs: Vec<ProcInfo>| ProcSnapshot { taken_at: 1, procs };

    let teammates = vec![
        // under the pane's shell: explained
        proc_info(3_950_010, FAKE_PID_BASE, start + 5, "/x/claude.exe", &["claude"], Some(Vendor::Claude), Some(LEAD_SID)),
        // a survivor of a closed pane
        proc_info(3_950_011, 1, start + 5, "/x/claude.exe", &["claude"], Some(Vendor::Claude), Some(LEAD_SID)),
        // older than this lead (a previous lead with the same session id)
        proc_info(3_950_012, 4_000_000, start - 100, "/x/claude.exe", &["claude"], Some(Vendor::Claude), Some(LEAD_SID)),
        // another lead's teammate
        proc_info(3_950_013, 4_000_000, start + 5, "/x/claude.exe", &["claude"], Some(Vendor::Claude), Some("someone-else")),
        shim_proc,
    ];
    rig.host.observe_scan(&snap(teammates.clone()));
    rig.host.observe_scan(&snap(teammates.clone()));
    assert!(!rig.table().outside_gilde, "a shim process is not a real tmux");
    assert!(rig.compat.deviations().is_empty());

    let mut with_stray = teammates.clone();
    with_stray.push(proc_info(3_950_020, rig.lead_pid, start + 5, "/x/claude.exe", &["claude"], Some(Vendor::Claude), Some(LEAD_SID)));
    rig.host.observe_scan(&snap(with_stray.clone()));
    assert!(rig.compat.deviations().is_empty(), "held for two scans");
    rig.host.observe_scan(&snap(with_stray));
    assert_eq!(rig.compat.deviations().len(), 1);
    assert_eq!(rig.compat.deviations()[0].2, "teammate process without respawn-pane");

    let other = direct_rig().await;
    let tmux = proc_info(3_950_030, 1, start, "/opt/homebrew/bin/tmux", &["tmux", "-L", &format!("claude-swarm-{}", other.lead_pid), "new-session"], None, None);
    other.host.observe_scan(&snap(vec![tmux]));
    assert!(other.table().outside_gilde, "ux-ia #104: teammate panes opened outside Gilde");
    assert!(other.tables.tables.lock().unwrap().iter().any(|t| t.outside_gilde));
}
