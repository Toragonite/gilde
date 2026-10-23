//! T-team-send and the Close pane sheet (contract §4.2.6, §7.4.7), plus the
//! store-backed team reads. Owner: T07.

#[path = "fixtures/fake_tmux_client/mod.rs"]
mod fake_tmux_client;

use std::sync::Arc;

use fake_tmux_client::host::{parent_pid, Rig, TEAM};
use gilde_code_lib::commands::team::{MessageBodies, TeamCommands, CLOSE_PANE_KEY, CLOSE_PANE_UNREAD_KEY};
use gilde_code_lib::contract::common::EpochMs;
use gilde_code_lib::contract::error::IpcError;
use gilde_code_lib::contract::ids::{ClientMsgId, MessageKey, TaskKey, TeamKey};
use gilde_code_lib::contract::ipc::{
    MessageBodySource, TeamMemberClosePreviewReq, TeamMemberCloseReq, TeamMessageBodyReq, TeamMessageBodyRes,
    TeamMessagesPageReq, TeamSendReq, TeamSendRoute, TeamTaskTitleReq,
};
use gilde_code_lib::contract::services::SignalPlan;
use gilde_code_lib::contract::team::{
    DeliveryStatus, MemberBackend, MessageKind, Party, Presence, GoneReason, Station, Task, TaskSource, TaskStatus,
    TeamMessage, TeamSource,
};
use gilde_code_lib::panehost::PaneState;

const NEW_SESSION: [&str; 11] = ["new-session", "-d", "-s", "claude-swarm", "-n", "swarm-view", "-P", "-F", "#{pane_id}", "--", "cat"];

fn cap(e: &IpcError) -> (String, String) {
    let v = serde_json::to_value(e).unwrap();
    let param = v["params"].as_object().unwrap().values().next().map(|x| x.as_str().unwrap_or_default().to_string()).unwrap_or_default();
    (v["code"].as_str().unwrap().to_string(), param)
}

fn msg_id(n: u32) -> ClientMsgId {
    serde_json::from_value(serde_json::json!(format!("c_{n:032x}"))).unwrap()
}

/// A rig with two running teammate panes (%0 alpha, %1 beta) in a hosted team.
async fn team_rig() -> (Rig, Arc<TeamCommands>, TeamKey, Vec<gilde_code_lib::contract::ids::MemberKey>) {
    let rig = Rig::new(parent_pid()).without_listener();
    rig.call(&NEW_SESSION).await;
    rig.call(&["split-window", "-d", "-t", "%0", "-v", "-P", "-F", "#{pane_id}", "--", "cat"]).await;
    for (i, name) in ["alpha", "beta"].iter().enumerate() {
        let r = rig.call(&["respawn-pane", "-k", "-t", &format!("%{i}"), "--", &rig.teammate_command(name, "blue")]).await;
        assert_eq!(r.exit, 0);
    }
    let (team, keys) = rig.hosted_team(&[("alpha", 0), ("beta", 1)]);
    let cmds = TeamCommands::new(rig.host.clone());
    (rig, cmds, team, keys)
}

/// T-team-send: a hosted pane member gets a bracketed paste + CR into its own
/// PTY; a repeat of the same `clientMsgId` types nothing again; other members
/// are `viaLeadOnly`, orchestrator teams `orchestratorTeam`, gone members
/// `memberGone`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t_team_send_routes() {
    let (rig, cmds, team, keys) = team_rig().await;
    let send = |id: u32, member: usize, text: &str| TeamSendReq {
        team_key: team.clone(),
        member_key: keys[member].clone(),
        client_msg_id: msg_id(id),
        text: text.to_string(),
    };
    let res = cmds.send(send(1, 0, "look at the tests")).await.unwrap();
    assert_eq!(res.route, TeamSendRoute::MemberPane);
    let pastes = rig.pty.pastes.lock().unwrap().clone();
    assert_eq!(pastes, vec![(rig.pty.handle(0).pty_id, "look at the tests".to_string(), true)]);

    // Idempotent by clientMsgId, also when two arrive at once.
    let (a, b) = tokio::join!(cmds.send(send(1, 0, "look at the tests")), cmds.send(send(1, 0, "look at the tests")));
    assert!(a.is_ok() && b.is_ok());
    assert_eq!(rig.pty.pastes.lock().unwrap().len(), 1, "a repeat types nothing");
    let (c, d) = tokio::join!(cmds.send(send(2, 1, "hi beta")), cmds.send(send(2, 1, "hi beta")));
    assert!(c.is_ok() && d.is_ok());
    assert_eq!(rig.pty.pastes.lock().unwrap().len(), 2);

    // The lead's own member is in-process: via the lead only.
    let lead_key = rig.world.snapshot.lock().unwrap().teams[0].lead.clone().unwrap();
    let r = cmds
        .send(TeamSendReq { team_key: team.clone(), member_key: lead_key, client_msg_id: msg_id(3), text: "x".into() })
        .await;
    assert_eq!(cap(&r.unwrap_err()), ("UNSUPPORTED".into(), "viaLeadOnly".into()));

    rig.set_member(&keys[1], |m| m.presence = Presence::Gone { reason: GoneReason::ProcessExited, at: 1 });
    assert_eq!(cap(&cmds.send(send(4, 1, "x")).await.unwrap_err()), ("UNSUPPORTED".into(), "memberGone".into()));

    rig.world.snapshot.lock().unwrap().teams[0].source = TeamSource::Orchestrator;
    assert_eq!(cap(&cmds.send(send(5, 0, "x")).await.unwrap_err()), ("UNSUPPORTED".into(), "orchestratorTeam".into()));
    rig.world.snapshot.lock().unwrap().teams[0].source = TeamSource::ClaudeNative;

    // A pane whose process exited: memberGone.
    rig.pty.exit(&rig.pty.handle(0).pty_id, Some(1), None);
    assert_eq!(cap(&cmds.send(send(6, 0, "x")).await.unwrap_err()), ("UNSUPPORTED".into(), "memberGone".into()));
    assert_eq!(rig.pty.pastes.lock().unwrap().len(), 2);
}

/// T-shim-closePane / LP-13: the sheet text is `team.closePane` followed by
/// `team.closePaneUnread` for that member; the close is SIGHUP → SIGTERM
/// after 2 s; tokens are single-use; a pane that changed since the preview is
/// `CONFLICT{changed: "pane"}`; a non-pane member is `notAPane`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t_shim_close_pane_sheet_and_signals() {
    let (rig, cmds, team, keys) = team_rig().await;
    let preview = cmds
        .close_preview(TeamMemberClosePreviewReq { team_key: team.clone(), member_key: keys[0].clone() })
        .unwrap();
    // A21.4: the sheet's sentences in order; no param names another key.
    let keys_of: Vec<&str> = preview.lines.iter().map(|l| l.key.as_str()).collect();
    assert_eq!(keys_of, vec![CLOSE_PANE_KEY, CLOSE_PANE_UNREAD_KEY]);
    assert!(preview.lines[0].params.is_empty());
    assert_eq!(preview.lines[1].params.len(), 1);
    assert_eq!(preview.lines[1].params["member"], "alpha");
    for line in &preview.lines {
        for v in line.params.values() {
            assert!(!v.as_str().unwrap_or_default().contains('.'), "a param never holds a key name: {v}");
        }
    }

    let close = |token| TeamMemberCloseReq { team_key: team.clone(), member_key: keys[0].clone(), confirm_token: token };
    cmds.close(close(preview.confirm_token.clone())).await.unwrap();
    let alpha = rig.pty.handle(0).pty_id;
    assert_eq!(rig.pty.wait_close_of(&alpha), SignalPlan::HupThenTerm { grace_ms: 2_000 });
    assert!(matches!(rig.table().panes[0].state, PaneState::Killed { .. }));
    // The lead still sends kill-pane at /exit: answered 0, nothing more.
    assert_eq!(rig.call(&["kill-pane", "-t", "%0"]).await.exit, 0);
    assert_eq!(rig.pty.closes().len(), 1);

    // The pane is gone now: a repeat is memberGone (the token was spent).
    let again = cmds.close(close(preview.confirm_token)).await.unwrap_err();
    assert_eq!(cap(&again), ("UNSUPPORTED".into(), "memberGone".into()));

    // The pane changed since the preview (a respawn): CONFLICT{pane}; the
    // token is single-use, so a retry with it is CONFLICT{token}.
    let p = cmds.close_preview(TeamMemberClosePreviewReq { team_key: team.clone(), member_key: keys[1].clone() }).unwrap();
    rig.call(&["respawn-pane", "-k", "-t", "%1", "--", &rig.teammate_command("beta", "blue")]).await;
    let beta_close = || TeamMemberCloseReq { team_key: team.clone(), member_key: keys[1].clone(), confirm_token: p.confirm_token.clone() };
    let err = cmds.close(beta_close()).await.unwrap_err();
    assert_eq!(cap(&err), ("CONFLICT".into(), "pane".into()));
    let err = cmds.close(beta_close()).await.unwrap_err();
    assert_eq!(cap(&err), ("CONFLICT".into(), "token".into()));
    assert!(matches!(rig.table().panes[1].state, PaneState::Running { .. }), "nothing was closed");

    // Not a pane member.
    let lead_key = rig.world.snapshot.lock().unwrap().teams[0].lead.clone().unwrap();
    let err = cmds.close_preview(TeamMemberClosePreviewReq { team_key: team.clone(), member_key: lead_key }).unwrap_err();
    assert_eq!(cap(&err), ("UNSUPPORTED".into(), "notAPane".into()));
    rig.set_member(&keys[1], |m| m.backend = MemberBackend::InProcess);
    let err = cmds.close_preview(TeamMemberClosePreviewReq { team_key: team, member_key: keys[1].clone() }).unwrap_err();
    assert_eq!(cap(&err), ("UNSUPPORTED".into(), "notAPane".into()));
}

fn message(team: &TeamKey, n: u32, at: EpochMs) -> TeamMessage {
    TeamMessage {
        key: MessageKey::derive(team, &format!("m{n}")),
        team_key: team.clone(),
        from: Party::User,
        to: Party::System,
        at,
        kind: MessageKind::Plain,
        status: DeliveryStatus::Delivered,
        in_reply_to: None,
        derived: false,
        idle_reason: None,
        has_summary: false,
    }
}

/// `team_messages_page` (newest `count` before `before`, oldest first) and
/// `team_task_title` from the store; unknown keys are NOT_FOUND.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t_team_store_reads() {
    let (rig, cmds, team, _keys) = team_rig().await;
    {
        let mut snap = rig.world.snapshot.lock().unwrap();
        for n in 0..10 {
            snap.messages.push(message(&team, n, 1_000 + EpochMs::from(n)));
        }
        snap.tasks.push(Task {
            key: TaskKey::derive(&team, "1"),
            team_key: team.clone(),
            native_id: Some("1".into()),
            title: Some("Write the docs".into()),
            inferred_k: None,
            status: TaskStatus::Open,
            station: Station::ToDo,
            owner: None,
            blocked_by: vec![],
            blocks: vec![],
            created_at: None,
            started_at: None,
            completed_at: None,
            first_seen_in_progress_at: None,
            source: TaskSource::Transcript,
            inferred: false,
        });
    }
    let page = cmds.messages_page(TeamMessagesPageReq { team_key: team.clone(), before: 1_008, count: 3 }).unwrap();
    let ats: Vec<EpochMs> = page.messages.iter().map(|m| m.at).collect();
    assert_eq!(ats, vec![1_005, 1_006, 1_007]);
    assert!(page.has_more);
    let page = cmds.messages_page(TeamMessagesPageReq { team_key: team.clone(), before: 1_002, count: 5 }).unwrap();
    assert_eq!(page.messages.len(), 2);
    assert!(!page.has_more);
    let other = TeamKey::derive("claudeNative", None, "nope");
    assert_eq!(cap(&cmds.messages_page(TeamMessagesPageReq { team_key: other, before: 1, count: 1 }).unwrap_err()).0, "NOT_FOUND");

    let title = cmds.task_title(TeamTaskTitleReq { task_key: TaskKey::derive(&team, "1") }).unwrap();
    assert_eq!(title.title.as_deref(), Some("Write the docs"));
    assert_eq!(cap(&cmds.task_title(TeamTaskTitleReq { task_key: TaskKey::derive(&team, "2") }).unwrap_err()).0, "NOT_FOUND");
    let _ = TEAM;
}

struct Bodies(std::sync::Mutex<u32>);

impl MessageBodies for Bodies {
    fn body(&self, _key: &MessageKey) -> Option<TeamMessageBodyRes> {
        *self.0.lock().unwrap() += 1;
        Some(TeamMessageBodyRes { text: Some("body".into()), summary: None, source: MessageBodySource::SenderToolUse })
    }
}

/// `team_message_body`: `notBuilt` until a body source is registered; then
/// served through the in-memory LRU (a repeat does not read again).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t_team_message_body() {
    let (rig, cmds, team, _keys) = team_rig().await;
    rig.world.snapshot.lock().unwrap().messages.push(message(&team, 1, 5));
    let key = MessageKey::derive(&team, "m1");
    let err = cmds.message_body(TeamMessageBodyReq { message_key: key.clone() }).unwrap_err();
    assert_eq!(cap(&err), ("UNSUPPORTED".into(), "notBuilt".into()));
    let bodies = Arc::new(Bodies(std::sync::Mutex::new(0)));
    cmds.set_bodies(bodies.clone());
    let res = cmds.message_body(TeamMessageBodyReq { message_key: key.clone() }).unwrap();
    assert_eq!(res.text.as_deref(), Some("body"));
    cmds.message_body(TeamMessageBodyReq { message_key: key }).unwrap();
    assert_eq!(*bodies.0.lock().unwrap(), 1, "served from the LRU");
    let unknown = MessageKey::derive(&team, "m9");
    assert_eq!(cap(&cmds.message_body(TeamMessageBodyReq { message_key: unknown }).unwrap_err()).0, "NOT_FOUND");
}
