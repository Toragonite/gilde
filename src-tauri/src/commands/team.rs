//! Team commands (contract §4.2.6) over the team protocol (§7) and PaneHost (§7.4).
//!
//! Owner: T07.
//!
//! The commands run on managed state ([`TeamState`]); the composition root
//! (T09b) does `app.manage(TeamState(TeamCommands::new(panehost)))`. Until it
//! is managed every command answers `UNSUPPORTED{cap: "notBuilt"}` (the
//! wave-0 stub behaviour), after the request passed the contract checks of
//! `Req` (envelope, size, shape, value rules).
//!
//! * `team_send` — only for hosted pane members: bracketed paste + CR into the
//!   member's PTY; idempotent by `clientMsgId` (a repeat, also a concurrent
//!   double click, returns the first outcome and types nothing). Anything else
//!   → `UNSUPPORTED{viaLeadOnly | orchestratorTeam | memberGone}`.
//! * `team_member_close_preview` / `team_member_close` — the Close pane sheet
//!   as `lines` (A21.4): `team.closePane` (ux-ia #105), then
//!   `team.closePaneUnread {member}` (LP-13) while a live lead can still
//!   message the pane;
//!   a 60 s single-use confirm token bound to the pane's state; SIGHUP, then a
//!   fenced SIGTERM after 2 s; `CONFLICT{changed}` when the pane changed.
//! * `team_messages_page` — answered from World (up to its 5,000 messages per
//!   team, A33.5(c)); `team_task_title` — from World's store (the titles
//!   still in memory).
//! * `team_message_body` — answered by a registered [`MessageBodies`] source
//!   through an LRU of 200 bodies (memory only); `UNSUPPORTED{notBuilt}`
//!   until one is registered. A30.7: the key is checked against World (up to
//!   its 5,000 messages per team), the read runs on the blocking pool, and an
//!   `unavailable` answer is never cached.

use std::collections::VecDeque;
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use tauri::Manager as _;

use crate::contract::common::Notice;
use crate::contract::ids::{ClientMsgId, MessageKey};
use crate::contract::ipc::{
    MessageBodySource, TeamMemberClosePreviewReq, TeamMemberClosePreviewRes, TeamMemberCloseReq, TeamMemberCloseRes,
    TeamMessageBodyReq, TeamMessageBodyRes, TeamMessagesPageReq, TeamMessagesPageRes, TeamSendReq, TeamSendRes,
    TeamSendRoute, TeamTaskTitleReq, TeamTaskTitleRes,
};
use crate::contract::{IpcError, Req};
use crate::lifecycle::quit::{digest_of, ConfirmTokens};
use crate::panehost::{PaneHost, Purpose};
use crate::world::World;

/// The sheet text of Close pane: ux-ia #105, then the LP-13 line.
pub const CLOSE_PANE_KEY: &str = "team.closePane";
pub const CLOSE_PANE_UNREAD_KEY: &str = "team.closePaneUnread";
/// Remembered `clientMsgId`s (idempotency window).
pub const SENT_MEMORY: usize = 256;
/// Message bodies kept in memory.
pub const BODY_LRU: usize = 200;

/// Where message bodies come from (World's Claude team adapter reading the
/// recipient's turn block or the sender's `SendMessage` tool_use, §4.2.6).
/// **[content]**: bodies stay in memory only and are never logged.
pub trait MessageBodies: Send + Sync {
    fn body(&self, key: &MessageKey) -> Option<TeamMessageBodyRes>;
}

/// The managed state of the team commands.
pub struct TeamState(pub Arc<TeamCommands>);

type SendCell = Arc<tokio::sync::OnceCell<Result<TeamSendRes, IpcError>>>;

/// The team commands' logic (testable without Tauri).
pub struct TeamCommands {
    host: Arc<PaneHost>,
    tokens: ConfirmTokens,
    sent: Mutex<VecDeque<(ClientMsgId, SendCell)>>,
    bodies: RwLock<Option<Arc<dyn MessageBodies>>>,
    body_cache: Mutex<VecDeque<(MessageKey, TeamMessageBodyRes)>>,
}

impl TeamCommands {
    pub fn new(host: Arc<PaneHost>) -> Arc<TeamCommands> {
        Arc::new(TeamCommands {
            host,
            tokens: ConfirmTokens::new(),
            sent: Mutex::new(VecDeque::new()),
            bodies: RwLock::new(None),
            body_cache: Mutex::new(VecDeque::new()),
        })
    }

    /// Register the message-body source (T09b).
    pub fn set_bodies(&self, bodies: Arc<dyn MessageBodies>) {
        *self.bodies.write() = Some(bodies);
    }

    /// The one cell for `id` (inserted atomically, so a concurrent repeat
    /// waits for the first call's outcome).
    fn send_cell(&self, id: &ClientMsgId) -> SendCell {
        let mut sent = self.sent.lock();
        if let Some((_, cell)) = sent.iter().find(|(k, _)| k == id) {
            return cell.clone();
        }
        let cell: SendCell = Arc::new(tokio::sync::OnceCell::new());
        sent.push_back((id.clone(), cell.clone()));
        while sent.len() > SENT_MEMORY {
            sent.pop_front();
        }
        cell
    }

    /// `team_send`.
    pub async fn send(&self, req: TeamSendReq) -> Result<TeamSendRes, IpcError> {
        let cell = self.send_cell(&req.client_msg_id);
        cell.get_or_init(|| async {
            self.host
                .send_to_member_pane(&req.team_key, &req.member_key, &req.text)
                .await
                .map(|()| TeamSendRes { route: TeamSendRoute::MemberPane })
        })
        .await
        .clone()
    }

    fn close_digest(&self, mp: &crate::panehost::MemberPane, team: &str, member: &str) -> String {
        digest_of(&[
            format!("team={team}"),
            format!("member={member}"),
            format!("lead={}", mp.lead.as_str()),
            format!("pane=%{}", mp.pane),
            format!("epoch={}", mp.epoch),
        ])
    }

    /// `team_member_close_preview`.
    pub fn close_preview(&self, req: TeamMemberClosePreviewReq) -> Result<TeamMemberClosePreviewRes, IpcError> {
        let mp = self.host.resolve_member(&req.team_key, &req.member_key, Purpose::Close)?;
        let digest = self.close_digest(&mp, req.team_key.as_str(), req.member_key.as_str());
        let mut lines = vec![Notice::new(CLOSE_PANE_KEY)];
        if mp.unread_possible {
            lines.push(Notice::new(CLOSE_PANE_UNREAD_KEY).param("member", mp.member_name.clone()));
        }
        Ok(TeamMemberClosePreviewRes { lines, confirm_token: self.tokens.issue(digest) })
    }

    /// `team_member_close`.
    pub async fn close(&self, req: TeamMemberCloseReq) -> Result<TeamMemberCloseRes, IpcError> {
        let mp = match self.host.resolve_member(&req.team_key, &req.member_key, Purpose::Close) {
            Ok(mp) => mp,
            Err(e) => {
                // The token is spent either way.
                let _ = self.tokens.redeem(&req.confirm_token, "", "pane");
                return Err(e);
            }
        };
        let digest = self.close_digest(&mp, req.team_key.as_str(), req.member_key.as_str());
        self.tokens.redeem(&req.confirm_token, &digest, "pane")?;
        self.host
            .close_member(&req.team_key, &req.member_key, Some(mp.epoch))
            .await?;
        Ok(TeamMemberCloseRes {})
    }

    /// A body in the LRU (moved to its newest end).
    fn cached_body(&self, key: &MessageKey) -> Option<TeamMessageBodyRes> {
        let mut cache = self.body_cache.lock();
        let pos = cache.iter().position(|(k, _)| k == key)?;
        let entry = cache.remove(pos).expect("position is valid");
        let res = entry.1.clone();
        cache.push_back(entry);
        Some(res)
    }

    /// Keep a body in the LRU. An `unavailable` answer is never kept (A30.7):
    /// the next call reads again (the `query` rate bucket bounds it).
    fn remember_body(&self, key: MessageKey, res: &TeamMessageBodyRes) {
        if res.source == MessageBodySource::Unavailable {
            return;
        }
        let mut cache = self.body_cache.lock();
        cache.retain(|(k, _)| k != &key);
        cache.push_back((key, res.clone()));
        while cache.len() > BODY_LRU {
            cache.pop_front();
        }
    }

    /// The message is one World holds: World's own lookup (A30.7, up to its
    /// 5,000 messages per team, so every message `team_messages_page` shows
    /// can be read); without a World handle, the store's newest 500.
    fn holds_message(&self, key: &MessageKey, world: Option<&World>) -> bool {
        match world {
            Some(w) => w.message_locator(key).is_some(),
            None => self.host.world().snapshot().messages.iter().any(|m| &m.key == key),
        }
    }

    /// `team_message_body` read inline, for callers without World or a
    /// runtime (the key is checked against the store's newest 500). The
    /// command uses [`TeamCommands::message_body_on`].
    pub fn message_body(&self, req: TeamMessageBodyReq) -> Result<TeamMessageBodyRes, IpcError> {
        if let Some(res) = self.cached_body(&req.message_key) {
            return Ok(res);
        }
        let source = self.bodies.read().clone().ok_or_else(IpcError::not_built)?;
        if !self.holds_message(&req.message_key, None) {
            return Err(IpcError::not_found("message"));
        }
        let res = source.body(&req.message_key).unwrap_or_else(unavailable_body);
        self.remember_body(req.message_key, &res);
        Ok(res)
    }

    /// `team_message_body` (A30.7): the LRU first; the key must be a message
    /// `world` holds (`None`: the store's newest 500); the transcript read
    /// runs on the blocking pool, like `history_preview`; an `unavailable`
    /// answer is not cached.
    pub async fn message_body_on(
        self: Arc<Self>,
        req: TeamMessageBodyReq,
        world: Option<Arc<World>>,
    ) -> Result<TeamMessageBodyRes, IpcError> {
        if let Some(res) = self.cached_body(&req.message_key) {
            return Ok(res);
        }
        let source = self.bodies.read().clone().ok_or_else(IpcError::not_built)?;
        if !self.holds_message(&req.message_key, world.as_deref()) {
            return Err(IpcError::not_found("message"));
        }
        let key = req.message_key.clone();
        let res = tauri::async_runtime::spawn_blocking(move || source.body(&key))
            .await
            .map_err(|_| IpcError::internal(&uuid::Uuid::new_v4().to_string()))?
            .unwrap_or_else(unavailable_body);
        self.remember_body(req.message_key, &res);
        Ok(res)
    }

    /// `team_messages_page`: the `count` newest messages of the team before
    /// `before`, oldest first; `hasMore` when older ones exist.
    pub fn messages_page(&self, req: TeamMessagesPageReq) -> Result<TeamMessagesPageRes, IpcError> {
        let snap = self.host.world().snapshot();
        if !snap.teams.iter().any(|t| t.key == req.team_key) {
            return Err(IpcError::not_found("team"));
        }
        let mut older: Vec<_> = snap
            .messages
            .into_iter()
            .filter(|m| m.team_key == req.team_key && m.at < req.before)
            .collect();
        older.sort_by(|a, b| a.at.cmp(&b.at).then_with(|| a.key.cmp(&b.key)));
        let count = usize::from(req.count);
        let has_more = older.len() > count;
        let messages = older.split_off(older.len().saturating_sub(count));
        Ok(TeamMessagesPageRes { messages, has_more })
    }

    /// `team_messages_page` (A33.5(c)): with a World handle, World's messages
    /// (up to its 5,000 per team), so every older page can be read, not only
    /// the store's newest 500; oldest first, as the store page. No file is
    /// read (World's memory only). Without World: [`TeamCommands::messages_page`].
    pub fn messages_page_in(&self, req: TeamMessagesPageReq, world: Option<&World>) -> Result<TeamMessagesPageRes, IpcError> {
        let Some(world) = world else { return self.messages_page(req) };
        let snap = self.host.world().snapshot();
        if !snap.teams.iter().any(|t| t.key == req.team_key) {
            return Err(IpcError::not_found("team"));
        }
        let (mut messages, has_more) = world.team_messages_page(&req.team_key, req.before, req.count);
        messages.reverse();
        Ok(TeamMessagesPageRes { messages, has_more })
    }

    /// `team_task_title`.
    pub fn task_title(&self, req: TeamTaskTitleReq) -> Result<TeamTaskTitleRes, IpcError> {
        let snap = self.host.world().snapshot();
        let task = snap
            .tasks
            .into_iter()
            .find(|t| t.key == req.task_key)
            .ok_or_else(|| IpcError::not_found("task"))?;
        Ok(TeamTaskTitleRes { title: task.title })
    }
}

fn commands<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> Result<Arc<TeamCommands>, IpcError> {
    app.try_state::<TeamState>()
        .map(|s| s.0.clone())
        .ok_or_else(IpcError::not_built)
}

/// `team_send` (contract §4.2.6).
#[tauri::command]
pub async fn team_send<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    req: Req<TeamSendReq>,
) -> Result<TeamSendRes, IpcError> {
    let c = commands(&app)?;
    c.send(req.into_inner()).await
}

/// `team_member_close_preview` (contract §4.2.6).
#[tauri::command]
pub async fn team_member_close_preview<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    req: Req<TeamMemberClosePreviewReq>,
) -> Result<TeamMemberClosePreviewRes, IpcError> {
    let c = commands(&app)?;
    c.close_preview(req.into_inner())
}

/// `team_member_close` (contract §4.2.6).
#[tauri::command]
pub async fn team_member_close<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    req: Req<TeamMemberCloseReq>,
) -> Result<TeamMemberCloseRes, IpcError> {
    let c = commands(&app)?;
    c.close(req.into_inner()).await
}

/// `team_message_body` (contract §4.2.6, A30.7): keys are checked against
/// the managed World (`Arc<World>`), the read runs on the blocking pool.
#[tauri::command]
pub async fn team_message_body<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    req: Req<TeamMessageBodyReq>,
) -> Result<TeamMessageBodyRes, IpcError> {
    let c = commands(&app)?;
    let world = app.try_state::<Arc<World>>().map(|w| w.inner().clone());
    c.message_body_on(req.into_inner(), world).await
}

/// What a body lookup answers when neither transcript has the message.
fn unavailable_body() -> TeamMessageBodyRes {
    TeamMessageBodyRes { text: None, summary: None, source: MessageBodySource::Unavailable }
}

/// `team_messages_page` (contract §4.2.6).
#[tauri::command]
pub async fn team_messages_page<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    req: Req<TeamMessagesPageReq>,
) -> Result<TeamMessagesPageRes, IpcError> {
    let c = commands(&app)?;
    let world = app.try_state::<Arc<World>>().map(|w| w.inner().clone());
    c.messages_page_in(req.into_inner(), world.as_deref())
}

/// `team_task_title` (contract §4.2.6).
#[tauri::command]
pub async fn team_task_title<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    req: Req<TeamTaskTitleReq>,
) -> Result<TeamTaskTitleRes, IpcError> {
    let c = commands(&app)?;
    c.task_title(req.into_inner())
}
