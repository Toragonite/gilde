//! Team configs, inboxes and task files (contract §5.2.4) [R5 §1–§3].
//!
//! Everything here is a pure reader: World's Claude native adapter (T02)
//! assembles teams from what it returns ([`TeamDirRead`], [`LeadJoin`]).
//!
//! Privacy (§9.7): `config.json` is deserialized with an allowlist — the
//! member `prompt` (the spawn prompt, content) is **not** a field, so it is
//! skipped by the parser and never allocated; inbox `text`, `input`,
//! `summary`, `result`, `planContent` are dropped immediately after the frame
//! fields are extracted; task `description`/`activeForm` are not read; every
//! read buffer is zeroed after parsing. These files are never logged.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::Value;

use crate::contract::common::EpochMs;
use crate::contract::team::{IdleReason, LeadBinding};
use crate::discovery::is_vendor_uuid;
use crate::discovery::safe_read::{self, EntryKind, FileId, ReadError, Root};

use super::transcripts::parse_iso8601_ms;

/// `config.json` cap (§5.8).
pub const CONFIG_CAP: u64 = 1024 * 1024;
/// Inbox cap: over it the file is skipped this cycle (§5.8).
pub const INBOX_CAP: u64 = 4 * 1024 * 1024;
/// Entries kept per inbox file: the newest (last) 5,000, as a team keeps at
/// most 5,000 messages (A33.5(d)); older entries are dropped and counted.
pub const INBOX_WINDOW: usize = 5_000;
/// Task file cap (§5.8).
pub const TASK_CAP: u64 = 256 * 1024;
pub const MEMBERS_CAP: usize = 200;
pub const TASKS_CAP: usize = 1_000;
/// Non-atomic `config.json` writes: 3 retries (§5.8) [R5 §1.3].
pub const TORN_RETRIES: [Duration; 3] =
    [Duration::from_millis(50), Duration::from_millis(100), Duration::from_millis(200)];
/// Heuristic join window: `createdAt ∈ [startedAt − 2 s, startedAt]`.
pub const HEURISTIC_WINDOW_MS: i64 = 2_000;
/// Hosted join: the team dir appears within 10 s of the lead's spawn.
pub const HOSTED_WINDOW_MS: i64 = 10_000;

// ---------------------------------------------------------------------------
// config.json
// ---------------------------------------------------------------------------

/// One roster member (allowlisted fields only).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TeamMemberRow {
    pub agent_id: String,
    pub name: String,
    pub agent_type: Option<String>,
    pub model: Option<String>,
    pub color: Option<String>,
    /// `in-process` | `tmux` | `iterm2`.
    pub backend_type: Option<String>,
    /// `leader` | `in-process` | `%N` | an iTerm session UUID.
    pub tmux_pane_id: Option<String>,
    pub cwd: Option<String>,
    pub joined_at: Option<EpochMs>,
    pub is_active: Option<bool>,
    pub plan_mode_required: Option<bool>,
    pub mode: Option<String>,
}

impl TeamMemberRow {
    /// Lead = `agentType:"team-lead"` / `tmuxPaneId:"leader"` (§3.4.6).
    pub fn is_lead(&self) -> bool {
        self.agent_type.as_deref() == Some("team-lead") || self.tmux_pane_id.as_deref() == Some("leader")
    }
}

/// A parsed `config.json`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TeamConfig {
    pub name: Option<String>,
    pub created_at: Option<EpochMs>,
    pub lead_agent_id: Option<String>,
    /// The **boot** id (never used to join, §5.2.4).
    pub lead_session_id: Option<String>,
    pub members: Vec<TeamMemberRow>,
    /// More than [`MEMBERS_CAP`] members (truncated; diagnostic).
    pub members_truncated: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawConfig {
    name: Option<Value>,
    created_at: Option<Value>,
    lead_agent_id: Option<Value>,
    lead_session_id: Option<Value>,
    members: Option<Vec<RawMember>>,
}

/// The allowlist: `prompt`, `subscriptions` and anything else are not fields,
/// so the parser skips them without allocating.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMember {
    agent_id: Option<Value>,
    name: Option<Value>,
    agent_type: Option<Value>,
    model: Option<Value>,
    color: Option<Value>,
    backend_type: Option<Value>,
    tmux_pane_id: Option<Value>,
    cwd: Option<Value>,
    joined_at: Option<Value>,
    is_active: Option<Value>,
    plan_mode_required: Option<Value>,
    mode: Option<Value>,
}

fn s(v: &Option<Value>, max: usize) -> Option<String> {
    v.as_ref()?.as_str().filter(|x| !x.is_empty() && x.len() <= max).map(str::to_string)
}

fn ms(v: &Option<Value>) -> Option<EpochMs> {
    match v.as_ref()? {
        Value::Number(n) => n.as_i64().filter(|x| *x > 0),
        Value::String(t) => parse_iso8601_ms(t),
        _ => None,
    }
}

fn b(v: &Option<Value>) -> Option<bool> {
    v.as_ref()?.as_bool()
}

/// Parse `config.json` bytes; `None` = not parseable (torn, or a wrong shape).
pub fn parse_config(bytes: &[u8]) -> Option<TeamConfig> {
    let raw: RawConfig = serde_json::from_slice(bytes).ok()?;
    let mut members = Vec::new();
    let mut truncated = false;
    for m in raw.members.unwrap_or_default() {
        // Members without string agentId/name are skipped (as the CLI does).
        let (Some(agent_id), Some(name)) = (s(&m.agent_id, 160), s(&m.name, 64)) else { continue };
        if members.len() >= MEMBERS_CAP {
            truncated = true;
            break;
        }
        members.push(TeamMemberRow {
            agent_id,
            name,
            agent_type: s(&m.agent_type, 64),
            model: s(&m.model, 96),
            color: s(&m.color, 32),
            backend_type: s(&m.backend_type, 32),
            tmux_pane_id: s(&m.tmux_pane_id, 64),
            cwd: s(&m.cwd, 4096),
            joined_at: ms(&m.joined_at),
            is_active: b(&m.is_active),
            plan_mode_required: b(&m.plan_mode_required),
            mode: s(&m.mode, 32),
        });
    }
    Some(TeamConfig {
        name: s(&raw.name, 128),
        created_at: ms(&raw.created_at),
        lead_agent_id: s(&raw.lead_agent_id, 160),
        lead_session_id: s(&raw.lead_session_id, 64),
        members,
        members_truncated: truncated,
    })
}

/// What reading one `config.json` produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigRead {
    Parsed(TeamConfig, FileId),
    /// Still torn after 3 retries: keep the last good roster, `roster_stale`.
    Torn,
    /// Over 1 MiB: last good parse kept; diagnostic.
    TooLarge,
    Missing,
    Failed(ReadError),
}

/// Read `config.json` with the torn-read policy (`retries` = 50/100/200 ms).
pub fn read_config(teams: &Root, path: &Path, retries: &[Duration]) -> ConfigRead {
    let mut attempt = 0usize;
    loop {
        match safe_read::read_file(teams, path, CONFIG_CAP) {
            Ok((bytes, id)) => {
                if let Some(c) = parse_config(&bytes) {
                    return ConfigRead::Parsed(c, id);
                }
            }
            Err(ReadError::NotFound) => return ConfigRead::Missing,
            Err(ReadError::TooLarge { .. }) => return ConfigRead::TooLarge,
            Err(e) if !e.is_transient() => return ConfigRead::Failed(e),
            Err(_) => {}
        }
        let Some(wait) = retries.get(attempt) else { return ConfigRead::Torn };
        std::thread::sleep(*wait);
        attempt += 1;
    }
}

// ---------------------------------------------------------------------------
// inboxes
// ---------------------------------------------------------------------------

/// The protocol-frame fields kept from an inbox `text` (§5.2.4).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InboxFrame {
    pub kind: String,
    pub request_id: Option<String>,
    pub agent_id: Option<String>,
    pub tool_name: Option<String>,
    pub idle_reason: Option<IdleReason>,
    pub completed_task_id: Option<String>,
    pub completed_status: Option<String>,
    pub timestamp: Option<EpochMs>,
}

/// One inbox entry reduced to `{from, timestamp, read, type, msg_id, color,
/// hasSummary}` (+ the frame fields). No text.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InboxEntry {
    pub from: String,
    pub timestamp: Option<EpochMs>,
    pub read: Option<bool>,
    pub kind: Option<String>,
    pub msg_id: Option<String>,
    pub color: Option<String>,
    pub has_summary: bool,
    pub frame: Option<InboxFrame>,
}

#[derive(Deserialize)]
struct RawInboxEntry<'a> {
    #[serde(borrow)]
    from: Option<Cow<'a, str>>,
    #[serde(borrow)]
    text: Option<Cow<'a, str>>,
    #[serde(borrow)]
    timestamp: Option<&'a RawValue>,
    read: Option<bool>,
    #[serde(rename = "type", borrow)]
    kind: Option<Cow<'a, str>>,
    #[serde(borrow)]
    msg_id: Option<Cow<'a, str>>,
    #[serde(borrow)]
    color: Option<Cow<'a, str>>,
    #[serde(borrow)]
    summary: Option<&'a RawValue>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawFrame<'a> {
    #[serde(rename = "type", borrow)]
    kind: Option<Cow<'a, str>>,
    #[serde(borrow)]
    request_id: Option<Cow<'a, str>>,
    #[serde(rename = "request_id", borrow)]
    request_id_snake: Option<Cow<'a, str>>,
    #[serde(rename = "agent_id", borrow)]
    agent_id: Option<Cow<'a, str>>,
    #[serde(rename = "tool_name", borrow)]
    tool_name: Option<Cow<'a, str>>,
    #[serde(borrow)]
    idle_reason: Option<Cow<'a, str>>,
    #[serde(borrow)]
    completed_task_id: Option<&'a RawValue>,
    #[serde(borrow)]
    completed_status: Option<Cow<'a, str>>,
    #[serde(borrow)]
    timestamp: Option<&'a RawValue>,
}

fn raw_ts(v: Option<&RawValue>) -> Option<EpochMs> {
    let v = v?;
    let t = v.get().trim();
    if t.starts_with('"') {
        serde_json::from_str::<Cow<'_, str>>(t).ok().and_then(|s| parse_iso8601_ms(&s))
    } else {
        t.parse::<i64>().ok().filter(|x| *x > 0)
    }
}

fn cow(v: &Option<Cow<'_, str>>, max: usize) -> Option<String> {
    v.as_ref().filter(|x| !x.is_empty() && x.len() <= max).map(|x| x.to_string())
}

fn frame_of(text: &str) -> Option<InboxFrame> {
    let t = text.trim_start();
    if !t.starts_with('{') {
        return None;
    }
    let f: RawFrame<'_> = serde_json::from_str(t).ok()?;
    let kind = cow(&f.kind, 64)?;
    let idle_reason = match f.idle_reason.as_deref() {
        Some("available") => Some(IdleReason::Available),
        Some("interrupted") => Some(IdleReason::Interrupted),
        Some("failed") => Some(IdleReason::Failed),
        _ => None,
    };
    let completed_task_id = f.completed_task_id.and_then(|v| {
        let t = v.get().trim();
        let t = t.trim_matches('"');
        (!t.is_empty() && t.len() <= 32 && t.bytes().all(|c| c.is_ascii_digit())).then(|| t.to_string())
    });
    Some(InboxFrame {
        kind,
        request_id: cow(&f.request_id, 128).or_else(|| cow(&f.request_id_snake, 128)),
        agent_id: cow(&f.agent_id, 160),
        tool_name: cow(&f.tool_name, 128),
        idle_reason,
        completed_task_id,
        completed_status: cow(&f.completed_status, 32),
        timestamp: raw_ts(f.timestamp),
    })
}

/// Parse an inbox file (a JSON array). Entries that fail are skipped.
pub fn parse_inbox(bytes: &[u8]) -> Option<Vec<InboxEntry>> {
    let raw: Vec<RawInboxEntry<'_>> = serde_json::from_slice(bytes).ok()?;
    let mut out = Vec::with_capacity(raw.len());
    for e in raw {
        let Some(from) = cow(&e.from, 160) else { continue };
        let frame = e.text.as_deref().and_then(frame_of);
        out.push(InboxEntry {
            from,
            timestamp: raw_ts(e.timestamp),
            read: e.read,
            kind: cow(&e.kind, 64),
            msg_id: cow(&e.msg_id, 64),
            color: cow(&e.color, 32),
            has_summary: e.summary.is_some_and(|v| v.get() != "null"),
            frame,
        });
        // `e.text` (content) is dropped here, borrowed from the zeroized buffer.
    }
    Some(out)
}

/// Read one inbox (`ENOENT` mid-read → one retry: inboxes are replaced by rename).
pub fn read_inbox(teams: &Root, path: &Path) -> Result<Vec<InboxEntry>, ReadError> {
    read_inbox_id(teams, path).map(|(e, _)| e)
}

/// [`read_inbox`] with the identity of the file that was read.
pub fn read_inbox_id(teams: &Root, path: &Path) -> Result<(Vec<InboxEntry>, FileId), ReadError> {
    let mut tries = 0;
    loop {
        match safe_read::read_file(teams, path, INBOX_CAP) {
            Ok((bytes, id)) => return Ok((parse_inbox(&bytes).unwrap_or_default(), id)),
            Err(ReadError::NotFound) if tries == 0 => tries += 1,
            Err(e) => return Err(e),
        }
    }
}

/// Parsed inboxes by file identity (A33.5(d)): an inbox whose
/// `(size, mtime, inode)` did not change is not read again.
#[derive(Debug, Default)]
pub struct InboxCache {
    files: HashMap<PathBuf, (FileId, Vec<InboxEntry>)>,
    /// Inboxes served from the cache.
    pub hits: u64,
    /// Inboxes read and parsed.
    pub reads: u64,
    /// Entries older than the newest [`INBOX_WINDOW`] of their file.
    pub dropped: u64,
}

impl InboxCache {
    /// Bound the cache (inbox files of teams that are gone).
    const MAX_FILES: usize = 4_096;

    fn get(&mut self, teams: &Root, path: &Path) -> Result<(Vec<InboxEntry>, FileId), ReadError> {
        if let Ok(st) = safe_read::lstat(path) {
            if let Some((id, entries)) = self.files.get(path) {
                if *id == st.id {
                    self.hits += 1;
                    return Ok((entries.clone(), *id));
                }
            }
        }
        let (mut entries, id) = match read_inbox_id(teams, path) {
            Ok(v) => v,
            Err(e) => {
                self.files.remove(path);
                return Err(e);
            }
        };
        self.reads += 1;
        if entries.len() > INBOX_WINDOW {
            let extra = entries.len() - INBOX_WINDOW;
            entries.drain(..extra);
            self.dropped += extra as u64;
        }
        if self.files.len() >= Self::MAX_FILES {
            self.files.clear();
        }
        self.files.insert(path.to_path_buf(), (id, entries.clone()));
        Ok((entries, id))
    }
}

// ---------------------------------------------------------------------------
// tasks
// ---------------------------------------------------------------------------

/// A task file (`subject` is **[content]**, memory only, sanitized at the one
/// display-text boundary, ≤ 200 chars, §3.4.8; `description`/`activeForm`
/// are never read).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TaskFile {
    /// Positive integer as string.
    pub id: String,
    pub subject: Option<String>,
    pub owner: Option<String>,
    /// `pending` | `in_progress` | `completed` | `deleted`.
    pub status: String,
    pub blocks: Vec<String>,
    pub blocked_by: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawTask {
    id: String,
    subject: Option<String>,
    owner: Option<String>,
    status: String,
    #[serde(default)]
    blocks: Vec<String>,
    #[serde(default)]
    blocked_by: Vec<String>,
}

/// Parse a task file; a schema failure skips it (as the CLI does).
pub fn parse_task(bytes: &[u8]) -> Option<TaskFile> {
    let r: RawTask = serde_json::from_slice(bytes).ok()?;
    let id_ok = |x: &str| !x.is_empty() && x.len() <= 12 && x.bytes().all(|c| c.is_ascii_digit());
    if !id_ok(&r.id) || !matches!(r.status.as_str(), "pending" | "in_progress" | "completed" | "deleted") {
        return None;
    }
    Some(TaskFile {
        id: r.id,
        subject: r.subject.and_then(|s| crate::discovery::display_text(&s, crate::contract::display::TextCap::Chars(200))),
        owner: r.owner.filter(|o| !o.is_empty() && o.len() <= 160),
        status: r.status,
        blocks: r.blocks.into_iter().filter(|x| id_ok(x)).take(TASKS_CAP).collect(),
        blocked_by: r.blocked_by.into_iter().filter(|x| id_ok(x)).take(TASKS_CAP).collect(),
    })
}

/// A `tasks/<team>/` directory.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TasksRead {
    pub exists: bool,
    pub tasks: Vec<TaskFile>,
    pub skipped: u32,
    pub truncated: bool,
}

/// Read every `<id>.json` of a task dir (≤ 1,000; files ≤ 256 KiB).
pub fn read_tasks(tasks_root: &Root, dir: &Path) -> Result<TasksRead, ReadError> {
    let listing = match safe_read::list_dir(tasks_root, dir, safe_read::DIR_ENTRIES_CAP) {
        Ok(l) => l,
        Err(ReadError::NotFound) => return Ok(TasksRead::default()),
        Err(e) => return Err(e),
    };
    let mut out = TasksRead { exists: true, ..TasksRead::default() };
    for e in listing.entries {
        if e.kind != EntryKind::File || !e.name.ends_with(".json") || e.name.ends_with(".lock") {
            continue;
        }
        if out.tasks.len() >= TASKS_CAP {
            out.truncated = true;
            break;
        }
        match safe_read::read_file(tasks_root, &dir.join(&e.name), TASK_CAP) {
            Ok((bytes, _)) => match parse_task(&bytes) {
                Some(t) => out.tasks.push(t),
                None => out.skipped += 1,
            },
            Err(ReadError::NotFound) => {} // reset 5 s after completion
            Err(ReadError::PermissionDenied) => return Err(ReadError::PermissionDenied),
            Err(_) => out.skipped += 1,
        }
    }
    out.tasks.sort_by_key(|t| t.id.parse::<u64>().unwrap_or(0));
    Ok(out)
}

// ---------------------------------------------------------------------------
// the team dir
// ---------------------------------------------------------------------------

/// One `teams/<dir>/` read: config + inboxes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TeamDirRead {
    pub dir_name: String,
    /// The last good parse (kept when the current read is torn).
    pub config: Option<TeamConfig>,
    pub config_id: Option<FileId>,
    pub roster_stale: bool,
    /// Torn reads this pass (diagnostic).
    pub torn: u32,
    /// Inbox name (agent) → entries.
    pub inboxes: BTreeMap<String, Vec<InboxEntry>>,
    pub inboxes_skipped: u32,
}

/// Read a team dir. `previous` supplies the last good roster.
pub fn read_team_dir(teams: &Root, dir_name: &str, previous: Option<&TeamDirRead>, retries: &[Duration]) -> Result<TeamDirRead, ReadError> {
    read_team_dir_cached(teams, dir_name, previous, retries, &mut InboxCache::default()).map(|(r, _)| r)
}

/// [`read_team_dir`] through an [`InboxCache`]; also returns each inbox's
/// file identity (the engine's change digest, A33.5(d)).
pub fn read_team_dir_cached(
    teams: &Root,
    dir_name: &str,
    previous: Option<&TeamDirRead>,
    retries: &[Duration],
    cache: &mut InboxCache,
) -> Result<(TeamDirRead, Vec<(String, FileId)>), ReadError> {
    let mut ids = Vec::new();
    let dir = teams.join(dir_name);
    let mut out = TeamDirRead { dir_name: dir_name.to_string(), ..TeamDirRead::default() };
    match read_config(teams, &dir.join("config.json"), retries) {
        ConfigRead::Parsed(c, id) => {
            out.config = Some(c);
            out.config_id = Some(id);
        }
        ConfigRead::Torn | ConfigRead::TooLarge => {
            out.torn += 1;
            out.roster_stale = true;
            if let Some(p) = previous {
                out.config = p.config.clone();
                out.config_id = p.config_id;
            }
        }
        ConfigRead::Missing => {}
        ConfigRead::Failed(ReadError::PermissionDenied) => return Err(ReadError::PermissionDenied),
        ConfigRead::Failed(_) => {
            out.roster_stale = previous.is_some();
            if let Some(p) = previous {
                out.config = p.config.clone();
            }
        }
    }
    let inboxes = dir.join("inboxes");
    if let Ok(listing) = safe_read::list_dir(teams, &inboxes, safe_read::DIR_ENTRIES_CAP) {
        for e in listing.entries {
            if e.kind != EntryKind::File {
                continue;
            }
            let Some(agent) = e.name.strip_suffix(".json") else { continue };
            match cache.get(teams, &inboxes.join(&e.name)) {
                Ok((entries, id)) => {
                    out.inboxes.insert(agent.to_string(), entries);
                    ids.push((agent.to_string(), id));
                }
                Err(ReadError::PermissionDenied) => return Err(ReadError::PermissionDenied),
                Err(_) => out.inboxes_skipped += 1,
            }
        }
    }
    Ok((out, ids))
}

// ---------------------------------------------------------------------------
// in-process teammates
// ---------------------------------------------------------------------------

/// `projects/<slug>/<leadSid>/subagents/agent-a<name>-<hex>.meta.json` with
/// `taskKind:"in_process_teammate"` [R5 §5].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InProcessMeta {
    pub team_name: String,
    /// The `a<name>-<hex>` id (names the transcript).
    pub agent_file_id: String,
    pub color: Option<String>,
    pub plan_mode_required: Option<bool>,
    pub model: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMeta {
    task_kind: Option<String>,
    team_name: Option<String>,
    color: Option<String>,
    plan_mode_required: Option<bool>,
    model: Option<String>,
}

/// Parse a subagent `.meta.json`; `None` unless it is an in-process teammate.
pub fn parse_inprocess_meta(file_name: &str, bytes: &[u8]) -> Option<InProcessMeta> {
    let stem = file_name.strip_suffix(".meta.json")?.strip_prefix("agent-")?;
    let r: RawMeta = serde_json::from_slice(bytes).ok()?;
    if r.task_kind.as_deref() != Some("in_process_teammate") {
        return None;
    }
    Some(InProcessMeta {
        team_name: r.team_name.filter(|t| !t.is_empty() && t.len() <= 128)?,
        agent_file_id: stem.chars().take(96).collect(),
        color: r.color.filter(|c| c.len() <= 32),
        plan_mode_required: r.plan_mode_required,
        model: r.model.filter(|m| m.len() <= 96),
    })
}

/// Read the in-process teammate metas under a lead's transcript dir.
pub fn read_inprocess(projects: &Root, lead_transcript: &Path) -> Vec<InProcessMeta> {
    let (Some(dir), Some(stem)) = (lead_transcript.parent(), lead_transcript.file_stem()) else { return Vec::new() };
    let sub = dir.join(stem).join("subagents");
    let Ok(listing) = safe_read::list_dir(projects, &sub, safe_read::DIR_ENTRIES_CAP) else { return Vec::new() };
    let mut out = Vec::new();
    for e in listing.entries {
        if e.kind != EntryKind::File || !e.name.ends_with(".meta.json") {
            continue;
        }
        if let Ok((bytes, _)) = safe_read::read_file(projects, &sub.join(&e.name), 64 * 1024) {
            if let Some(m) = parse_inprocess_meta(&e.name, &bytes) {
                out.push(m);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// team ↔ lead join (§5.2.4) — never by team name, never by leadSessionId
// ---------------------------------------------------------------------------

/// A live registry session the join may bind to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveLeadCandidate {
    pub session_id: String,
    pub pid: i32,
    pub started_at: Option<EpochMs>,
    /// Realpath of the registry `cwd`.
    pub cwd: Option<String>,
}

/// A live teammate process of this account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveTeammate {
    pub team_name: String,
    pub agent_id: String,
    pub parent_session_id: Option<String>,
    pub pid: i32,
}

/// The join result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeadJoin {
    pub binding: LeadBinding,
    pub lead_session_id: Option<String>,
    pub lead_pid: Option<i32>,
}

/// Join a team dir to its lead:
/// 1. `Exact` — a live teammate's `--parent-session-id` = a live registry `sessionId`;
/// 2. `Hosted` — a Gilde-hosted lead whose spawn precedes `createdAt` by ≤ 10 s;
/// 3. `Heuristic` — no live teammate: `createdAt ∈ [startedAt − 2 s, startedAt]`
///    of a registry entry in the lead member's `cwd`;
/// 4. `None`.
pub fn join_lead(
    team_dir: &str,
    config: Option<&TeamConfig>,
    teammates: &[LiveTeammate],
    leads: &[LiveLeadCandidate],
    hosted: &[(i32, EpochMs, Option<String>)],
) -> LeadJoin {
    let mine: Vec<&LiveTeammate> = teammates.iter().filter(|t| t.team_name == team_dir).collect();
    for t in &mine {
        let Some(parent) = t.parent_session_id.as_deref().filter(|p| is_vendor_uuid(p)) else { continue };
        if let Some(l) = leads.iter().find(|l| l.session_id == parent) {
            return LeadJoin { binding: LeadBinding::Exact, lead_session_id: Some(l.session_id.clone()), lead_pid: Some(l.pid) };
        }
    }
    let created = config.and_then(|c| c.created_at);
    if let Some(created) = created {
        for (pid, spawned_at, _) in hosted {
            if created >= *spawned_at && created - *spawned_at <= HOSTED_WINDOW_MS {
                let sid = leads.iter().find(|l| l.pid == *pid).map(|l| l.session_id.clone());
                return LeadJoin { binding: LeadBinding::Hosted, lead_session_id: sid, lead_pid: Some(*pid) };
            }
        }
        if mine.is_empty() {
            let lead_cwd = config.and_then(|c| c.members.iter().find(|m| m.is_lead())).and_then(|m| m.cwd.clone());
            let lead_cwd_real = lead_cwd.as_deref().map(|c| std::fs::canonicalize(c).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| c.to_string()));
            let mut best: Option<&LiveLeadCandidate> = None;
            for l in leads {
                let Some(started) = l.started_at else { continue };
                if !(started - HEURISTIC_WINDOW_MS..=started).contains(&created) {
                    continue;
                }
                if lead_cwd_real.is_some() && l.cwd != lead_cwd_real {
                    continue;
                }
                if best.is_none_or(|b| (b.started_at.unwrap_or(0) - created).abs() > (started - created).abs()) {
                    best = Some(l);
                }
            }
            if let Some(l) = best {
                return LeadJoin { binding: LeadBinding::Heuristic, lead_session_id: Some(l.session_id.clone()), lead_pid: Some(l.pid) };
            }
        }
    }
    LeadJoin { binding: LeadBinding::None, lead_session_id: None, lead_pid: None }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CANARY: &str = "CANARY-SPAWN-PROMPT-7f3a";

    fn config_json() -> String {
        format!(
            r#"{{"name":"session-25ae1157","createdAt":1792558921340,"leadAgentId":"team-lead@session-25ae1157","leadSessionId":"25ae1157","teamAllowedPaths":[],"members":[
            {{"agentId":"team-lead@session-25ae1157","name":"team-lead","agentType":"team-lead","joinedAt":1792558921340,"tmuxPaneId":"leader","cwd":"/w","subscriptions":[],"backendType":"in-process"}},
            {{"agentId":"alpha@session-25ae1157","name":"alpha","agentType":"general-purpose","model":"haiku","color":"blue","prompt":"{CANARY}","planModeRequired":false,"joinedAt":1792558931000,"tmuxPaneId":"%1","cwd":"/w","subscriptions":[],"backendType":"tmux","isActive":true}},
            {{"agentId":7,"name":"bad"}},
            {{"name":"no-id"}}
            ]}}"#
        )
    }

    #[test]
    fn config_allowlist_never_holds_the_prompt() {
        let c = parse_config(config_json().as_bytes()).unwrap();
        assert_eq!(c.members.len(), 2, "members without string agentId/name are skipped");
        assert!(c.members[0].is_lead());
        assert_eq!(c.members[1].is_active, Some(true));
        assert_eq!(c.members[1].backend_type.as_deref(), Some("tmux"));
        assert!(!format!("{c:?}").contains(CANARY));
    }

    #[test]
    fn torn_config_retries_then_reports_torn() {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::new(dir.path()).unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(&p, &config_json()[..40]).unwrap();
        assert_eq!(read_config(&root, &p, &[Duration::ZERO; 3]), ConfigRead::Torn);
        std::fs::write(&p, config_json()).unwrap();
        assert!(matches!(read_config(&root, &p, &[Duration::ZERO; 3]), ConfigRead::Parsed(..)));
    }

    #[test]
    fn inbox_frames_keep_only_allowlisted_fields() {
        let text_idle = format!(r#"{{\"type\":\"idle_notification\",\"from\":\"alpha\",\"timestamp\":\"2026-10-22T02:00:00.000Z\",\"idleReason\":\"available\",\"summary\":\"{CANARY}\",\"result\":\"{CANARY}\",\"completedTaskId\":\"3\",\"completedStatus\":\"resolved\"}}"#);
        let text_perm = format!(r#"{{\"type\":\"permission_request\",\"request_id\":\"perm-1792-abc\",\"agent_id\":\"alpha\",\"tool_name\":\"Bash\",\"tool_use_id\":\"toolu_1\",\"description\":\"{CANARY}\",\"input\":{{\"command\":\"{CANARY}\"}}}}"#);
        let json = format!(
            r#"[{{"from":"alpha","text":"{text_idle}","timestamp":"2026-10-22T02:00:00.000Z","read":false,"msgV":1,"msg_id":"0e2c0b1a-6b7d-4c51-9a8e-1f2e3d4c5b6a","type":"message","summary":"{CANARY}"}},
               {{"from":"alpha","text":"{text_perm}","timestamp":"2026-10-22T02:00:01.000Z","read":false}},
               {{"from":"team-lead","text":"plain {CANARY}","timestamp":"2026-10-22T02:00:02.000Z"}},
               {{"text":"no from"}}]"#
        );
        let entries = parse_inbox(json.as_bytes()).unwrap();
        assert_eq!(entries.len(), 3);
        let f = entries[0].frame.as_ref().unwrap();
        assert_eq!(f.kind, "idle_notification");
        assert_eq!(f.idle_reason, Some(IdleReason::Available));
        assert_eq!(f.completed_task_id.as_deref(), Some("3"));
        assert!(entries[0].has_summary);
        let p = entries[1].frame.as_ref().unwrap();
        assert_eq!(p.request_id.as_deref(), Some("perm-1792-abc"));
        assert_eq!(p.tool_name.as_deref(), Some("Bash"));
        assert!(entries[2].frame.is_none());
        assert!(!format!("{entries:?}").contains(CANARY));
    }

    #[test]
    fn tasks_schema_and_caps() {
        assert!(parse_task(br#"{"id":"3","subject":"s","description":"d","status":"in_progress","blocks":[],"blockedBy":["1"]}"#).is_some());
        assert!(parse_task(br#"{"id":"x","subject":"s","status":"pending","blocks":[],"blockedBy":[]}"#).is_none());
        assert!(parse_task(br#"{"id":"3","subject":"s","status":"weird"}"#).is_none());
        let t = parse_task(br#"{"id":"3","subject":"s","description":"CANARY","status":"pending"}"#).unwrap();
        assert!(!format!("{t:?}").contains("CANARY"));
    }

    #[test]
    fn in_process_meta() {
        let m = parse_inprocess_meta(
            "agent-abeta-0011223344556677.meta.json",
            br#"{"taskKind":"in_process_teammate","teamName":"session-1","color":"green","planModeRequired":false,"model":"haiku","description":"x"}"#,
        )
        .unwrap();
        assert_eq!(m.team_name, "session-1");
        assert_eq!(m.agent_file_id, "abeta-0011223344556677");
        assert!(parse_inprocess_meta("agent-x.meta.json", br#"{"taskKind":"subagent"}"#).is_none());
    }

    #[test]
    fn join_never_by_name_or_lead_session_id() {
        let cfg = parse_config(config_json().as_bytes()).unwrap();
        let resumed = LiveLeadCandidate { session_id: "326555f6-0000-4000-8000-000000000001".into(), pid: 10, started_at: Some(1792558922000), cwd: Some("/w".into()) };
        let decoy = LiveLeadCandidate { session_id: "25ae1157-0000-4000-8000-000000000002".into(), pid: 11, started_at: Some(1700000000000), cwd: Some("/w".into()) };
        // Exact via the teammate's --parent-session-id (the resumed id, not the name's boot id).
        let tm = LiveTeammate { team_name: "session-25ae1157".into(), agent_id: "alpha@session-25ae1157".into(), parent_session_id: Some(resumed.session_id.clone()), pid: 20 };
        let j = join_lead("session-25ae1157", Some(&cfg), &[tm], &[resumed.clone(), decoy.clone()], &[]);
        assert_eq!((j.binding, j.lead_pid), (LeadBinding::Exact, Some(10)));
        // No live teammate: createdAt within 2 s before startedAt in the same cwd.
        let j = join_lead("session-25ae1157", Some(&cfg), &[], &[decoy.clone(), resumed.clone()], &[]);
        assert_eq!((j.binding, j.lead_pid), (LeadBinding::Heuristic, Some(10)));
        // Neither: None (the decoy's id starts with the team-name boot id: never used).
        let j = join_lead("session-25ae1157", Some(&cfg), &[], &[decoy], &[]);
        assert_eq!(j.binding, LeadBinding::None);
        // Hosted.
        let j = join_lead("session-25ae1157", Some(&cfg), &[], &[], &[(99, 1792558920000, None)]);
        assert_eq!((j.binding, j.lead_pid), (LeadBinding::Hosted, Some(99)));
    }
}
